//! The RealWorld schema, mirroring `examples/realworld/main.roc`'s init!.
//! Used to prepare queries against an in-memory database for analysis. In the
//! shipped tool this text comes from `## @schema`-marked Roc constants; during
//! the spike it is inlined so the analyzer battery has something to prepare
//! against.

pub const REALWORLD_SCHEMA: &[&str] = &[
    "CREATE TABLE users (id INTEGER PRIMARY KEY, email TEXT NOT NULL UNIQUE, username TEXT NOT NULL UNIQUE, password_hash TEXT NOT NULL, bio TEXT, image TEXT)",
    "CREATE TABLE tokens (token TEXT PRIMARY KEY, user_id INTEGER NOT NULL REFERENCES users(id))",
    "CREATE TABLE follows (follower_id INTEGER NOT NULL, followed_id INTEGER NOT NULL, PRIMARY KEY (follower_id, followed_id))",
    "CREATE TABLE articles (id INTEGER PRIMARY KEY, slug TEXT NOT NULL UNIQUE, title TEXT NOT NULL, description TEXT NOT NULL, body TEXT NOT NULL, author_id INTEGER NOT NULL REFERENCES users(id), created_at TEXT NOT NULL, updated_at TEXT NOT NULL)",
    "CREATE TABLE article_tags (article_id INTEGER NOT NULL, tag TEXT NOT NULL, PRIMARY KEY (article_id, tag))",
    "CREATE TABLE favorites (user_id INTEGER NOT NULL, article_id INTEGER NOT NULL, PRIMARY KEY (user_id, article_id))",
    "CREATE TABLE comments (id INTEGER PRIMARY KEY, article_id INTEGER NOT NULL REFERENCES articles(id), author_id INTEGER NOT NULL, body TEXT NOT NULL, created_at TEXT NOT NULL, updated_at TEXT NOT NULL)",
    "CREATE INDEX idx_articles_author ON articles(author_id)",
    "CREATE INDEX idx_article_tags_tag ON article_tags(tag)",
    "CREATE INDEX idx_favorites_article ON favorites(article_id, user_id)",
    "CREATE INDEX idx_comments_article ON comments(article_id)",
];

/// The shared article-projection fragment (`## @fragment article_select` in the
/// migrated app). The by-slug/list/feed queries all splice this.
pub const ARTICLE_SELECT: &str = "\
SELECT a.id, a.author_id, a.slug, a.title, a.description, a.body, a.created_at, a.updated_at,
       u.username, u.bio, u.image,
       (SELECT COUNT(*) FROM favorites f WHERE f.article_id = a.id) AS favorites_count,
       EXISTS(SELECT 1 FROM favorites f2 WHERE f2.article_id = a.id AND f2.user_id = :viewer) AS favorited,
       EXISTS(SELECT 1 FROM follows fo WHERE fo.follower_id = :viewer AND fo.followed_id = a.author_id) AS following,
       (SELECT GROUP_CONCAT(tag) FROM (SELECT at2.tag FROM article_tags at2 WHERE at2.article_id = a.id ORDER BY at2.tag)) AS tag_list
FROM articles a JOIN users u ON u.id = a.author_id";
