#!/usr/bin/env python3
# Deterministic seed for the typed-SQL end-to-end benchmark.
# 200 users / 5000 articles / 10000 comments + tags + favorites.
# Schema is byte-identical between main and the typed-sql branch, so ONE
# seeded file drives both server binaries. password_hash is a placeholder —
# the benchmarked endpoints are all anonymous GETs, so no login occurs.
import sqlite3, sys, random

random.seed(1234)  # fully deterministic

DB = sys.argv[1]
N_USERS, N_ARTICLES, N_COMMENTS = 200, 5000, 10000
TAGS = [f"tag{i}" for i in range(20)]

con = sqlite3.connect(DB)
c = con.cursor()
c.executescript("""
CREATE TABLE IF NOT EXISTS users (id INTEGER PRIMARY KEY, email TEXT NOT NULL UNIQUE, username TEXT NOT NULL UNIQUE, password_hash TEXT NOT NULL, bio TEXT, image TEXT);
CREATE TABLE IF NOT EXISTS tokens (token TEXT PRIMARY KEY, user_id INTEGER NOT NULL REFERENCES users(id));
CREATE TABLE IF NOT EXISTS follows (follower_id INTEGER NOT NULL, followed_id INTEGER NOT NULL, PRIMARY KEY (follower_id, followed_id));
CREATE TABLE IF NOT EXISTS articles (id INTEGER PRIMARY KEY, slug TEXT NOT NULL UNIQUE, title TEXT NOT NULL, description TEXT NOT NULL, body TEXT NOT NULL, author_id INTEGER NOT NULL REFERENCES users(id), created_at TEXT NOT NULL, updated_at TEXT NOT NULL);
CREATE TABLE IF NOT EXISTS article_tags (article_id INTEGER NOT NULL, tag TEXT NOT NULL, PRIMARY KEY (article_id, tag));
CREATE TABLE IF NOT EXISTS favorites (user_id INTEGER NOT NULL, article_id INTEGER NOT NULL, PRIMARY KEY (user_id, article_id));
CREATE TABLE IF NOT EXISTS comments (id INTEGER PRIMARY KEY, article_id INTEGER NOT NULL REFERENCES articles(id), author_id INTEGER NOT NULL, body TEXT NOT NULL, created_at TEXT NOT NULL, updated_at TEXT NOT NULL);
CREATE INDEX IF NOT EXISTS idx_articles_author ON articles(author_id);
CREATE INDEX IF NOT EXISTS idx_article_tags_tag ON article_tags(tag);
CREATE INDEX IF NOT EXISTS idx_favorites_article ON favorites(article_id, user_id);
CREATE INDEX IF NOT EXISTS idx_comments_article ON comments(article_id);
""")

TS = "2026-01-01T00:00:00.000Z"
c.executemany("INSERT INTO users VALUES (?,?,?,?,?,?)",
    [(u, f"user{u}@example.com", f"user{u}", "x", None, None) for u in range(1, N_USERS+1)])

arts, atags, favs = [], [], []
for a in range(1, N_ARTICLES+1):
    author = random.randint(1, N_USERS)
    arts.append((a, f"article-{a}", f"Title {a}", f"Desc {a}", f"Body of article {a}. "*8, author, TS, TS))
    for t in random.sample(TAGS, random.randint(1, 3)):
        atags.append((a, t))
    for u in random.sample(range(1, N_USERS+1), random.randint(0, 4)):
        favs.append((u, a))
c.executemany("INSERT INTO articles VALUES (?,?,?,?,?,?,?,?)", arts)
c.executemany("INSERT OR IGNORE INTO article_tags VALUES (?,?)", atags)
c.executemany("INSERT OR IGNORE INTO favorites VALUES (?,?)", favs)

c.executemany("INSERT INTO comments VALUES (?,?,?,?,?,?)",
    [(i, random.randint(1, N_ARTICLES), random.randint(1, N_USERS), f"Comment {i} body.", TS, TS)
     for i in range(1, N_COMMENTS+1)])

# follows: each user follows ~5 others (used by profile following flag paths)
fl = set()
for u in range(1, N_USERS+1):
    for f in random.sample(range(1, N_USERS+1), 5):
        if f != u: fl.add((u, f))
c.executemany("INSERT OR IGNORE INTO follows VALUES (?,?)", list(fl))

con.commit()
for tbl in ("users","articles","comments","article_tags","favorites","follows"):
    print(f"{tbl:14} {c.execute(f'SELECT count(*) FROM {tbl}').fetchone()[0]}")
con.close()
