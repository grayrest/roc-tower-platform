//! End-to-end tests for the RealWorld (Conduit) example (`examples/realworld`):
//! the full user journey — register/login/current/update, profiles + follows,
//! article CRUD with tags, list filters + feed pagination, favorites,
//! comments, tags — plus the authorization matrix (401/403/404/422) and the
//! unique-violation → 422 err_handler path.
//!
//! Requires `just` and the `roc` binary (env `ROC`, default
//! `~/Repositories/roc/zig-out/bin/roc`).

use std::net::TcpListener;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use serde_json::Value;

struct Server {
    child: Child,
    base_url: String,
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn workspace_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(|p| p.parent())
        .expect("workspace root")
        .to_path_buf()
}

fn roc_binary() -> String {
    std::env::var("ROC").unwrap_or_else(|_| {
        let home = std::env::var("HOME").expect("HOME not set");
        format!("{home}/Repositories/roc/zig-out/bin/roc")
    })
}

fn free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .expect("bind ephemeral port")
        .local_addr()
        .expect("local addr")
        .port()
}

/// The example uses this fixed DB path (see examples/realworld/Db.roc).
const DB_PATH: &str = "/tmp/tower-platform-realworld.db";

fn start_server() -> Server {
    let root = workspace_root();

    for suffix in ["", "-wal", "-shm"] {
        let _ = std::fs::remove_file(format!("{DB_PATH}{suffix}"));
    }

    let build = Command::new("just")
        .arg("build")
        .arg("examples/realworld/main.roc")
        .env("ROC", roc_binary())
        .current_dir(&root)
        .output()
        .expect("failed to run `just build` (is just installed?)");
    assert!(
        build.status.success(),
        "`just build` failed:\n{}\n{}",
        String::from_utf8_lossy(&build.stdout),
        String::from_utf8_lossy(&build.stderr),
    );

    let port = free_port();
    let child = Command::new(root.join("main"))
        .env("PORT", port.to_string())
        .current_dir(&root)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("failed to start server");
    let server = Server {
        child,
        base_url: format!("http://127.0.0.1:{port}/api"),
    };

    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        match std::net::TcpStream::connect(("127.0.0.1", port)) {
            Ok(_) => break,
            Err(_) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(100)),
            Err(err) => panic!("server did not start listening: {err}"),
        }
    }
    server
}

fn client() -> reqwest::Client {
    reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .build()
        .expect("client")
}

async fn json_of(response: reqwest::Response) -> Value {
    let text = response.text().await.expect("body");
    serde_json::from_str(&text).unwrap_or_else(|err| panic!("invalid JSON ({err}): {text}"))
}

/// POST a JSON body, optionally authenticated, and return (status, json).
async fn post(
    client: &reqwest::Client,
    url: String,
    token: Option<&str>,
    body: &str,
) -> (u16, Value) {
    let mut request = client.post(url).header("content-type", "application/json");
    if let Some(token) = token {
        request = request.header("authorization", format!("Token {token}"));
    }
    let response = request.body(body.to_owned()).send().await.expect("send");
    (response.status().as_u16(), json_of(response).await)
}

async fn get(client: &reqwest::Client, url: String, token: Option<&str>) -> (u16, Value) {
    let mut request = client.get(url);
    if let Some(token) = token {
        request = request.header("authorization", format!("Token {token}"));
    }
    let response = request.send().await.expect("send");
    (response.status().as_u16(), json_of(response).await)
}

/// One sequential journey: the shared `./main` binary and fixed DB path mean
/// steps must run in order, so they live in one `#[test]` (same shape as the
/// pure example's suite).
#[tokio::test]
async fn realworld_full_journey() {
    let server = start_server();
    let base = &server.base_url;
    let client = client();

    // --- register: 200, user envelope with null bio/image, opaque token ---
    let (status, body) = post(
        &client,
        format!("{base}/users"),
        None,
        r#"{"user":{"username":"jake","email":"jake@x.com","password":"dragons4ever"}}"#,
    )
    .await;
    assert_eq!(status, 200, "register: {body}");
    assert_eq!(body["user"]["username"], "jake");
    assert!(body["user"]["bio"].is_null(), "bio must be JSON null: {body}");
    assert!(body["user"]["image"].is_null());
    let jake_token = body["user"]["token"].as_str().expect("token").to_owned();

    // --- duplicate username: Must fails, err_handler answers 422, and the
    //     token INSERT in the same transaction rolled back with it ---
    let (status, body) = post(
        &client,
        format!("{base}/users"),
        None,
        r#"{"user":{"username":"jake","email":"second@x.com","password":"pw"}}"#,
    )
    .await;
    assert_eq!(status, 422, "duplicate register: {body}");
    assert!(body["errors"]["body"][0].as_str().unwrap().contains("taken"));

    // --- login with wrong password: 422, no user leak ---
    let (status, _) = post(
        &client,
        format!("{base}/users/login"),
        None,
        r#"{"user":{"email":"jake@x.com","password":"wrong"}}"#,
    )
    .await;
    assert_eq!(status, 422);

    // --- login: fresh token, stored by the login effect ---
    let (status, body) = post(
        &client,
        format!("{base}/users/login"),
        None,
        r#"{"user":{"email":"jake@x.com","password":"dragons4ever"}}"#,
    )
    .await;
    assert_eq!(status, 200, "login: {body}");
    let login_token = body["user"]["token"].as_str().expect("token").to_owned();
    assert_ne!(login_token, jake_token, "login must mint a fresh token");

    // --- current user via the login token proves the token effect wrote ---
    let (status, body) = get(&client, format!("{base}/user"), Some(&login_token)).await;
    assert_eq!(status, 200, "current user: {body}");
    assert_eq!(body["user"]["email"], "jake@x.com");

    // --- current user without a token: 401 ---
    let (status, _) = get(&client, format!("{base}/user"), None).await;
    assert_eq!(status, 401);

    // --- partial user update: bio set, everything else kept ---
    let update = client
        .put(format!("{base}/user"))
        .header("authorization", format!("Token {jake_token}"))
        .header("content-type", "application/json")
        .body(r#"{"user":{"bio":"I train dragons"}}"#)
        .send()
        .await
        .unwrap();
    assert_eq!(update.status(), 200);
    let body = json_of(update).await;
    assert_eq!(body["user"]["bio"], "I train dragons");
    assert_eq!(body["user"]["username"], "jake", "unsent fields must be kept");

    // --- create article with tags (one transaction: row + tag rows) ---
    let (status, body) = post(
        &client,
        format!("{base}/articles"),
        Some(&jake_token),
        r#"{"article":{"title":"How to Train Your Dragon","description":"Ever wonder how?","body":"Very carefully.","tagList":["dragons","training"]}}"#,
    )
    .await;
    assert_eq!(status, 200, "create article: {body}");
    let slug = body["article"]["slug"].as_str().expect("slug").to_owned();
    assert!(slug.starts_with("how-to-train-your-dragon-"), "slug: {slug}");
    assert_eq!(body["article"]["favorited"], false, "must be JSON false: {body}");
    assert_eq!(body["article"]["favoritesCount"], 0);
    assert_eq!(body["article"]["tagList"], serde_json::json!(["dragons", "training"]));
    let created_at = body["article"]["createdAt"].as_str().unwrap().to_owned();
    assert!(created_at.ends_with('Z') && created_at.contains('T'), "ISO-8601: {created_at}");

    // --- anonymous create: 401 ---
    let (status, _) = post(
        &client,
        format!("{base}/articles"),
        None,
        r#"{"article":{"title":"x","description":"y","body":"z"}}"#,
    )
    .await;
    assert_eq!(status, 401);

    // --- second user: register, follow jake ---
    let (status, body) = post(
        &client,
        format!("{base}/users"),
        None,
        r#"{"user":{"username":"anne","email":"anne@x.com","password":"annespw1"}}"#,
    )
    .await;
    assert_eq!(status, 200, "register anne: {body}");
    let anne_token = body["user"]["token"].as_str().expect("token").to_owned();

    let follow = client
        .post(format!("{base}/profiles/jake/follow"))
        .header("authorization", format!("Token {anne_token}"))
        .send()
        .await
        .unwrap();
    assert_eq!(follow.status(), 200);
    let body = json_of(follow).await;
    assert_eq!(body["profile"]["following"], true, "following must be JSON true: {body}");

    // --- profile view varies by viewer (optional auth) ---
    let (_, body) = get(&client, format!("{base}/profiles/jake"), Some(&anne_token)).await;
    assert_eq!(body["profile"]["following"], true);
    let (_, body) = get(&client, format!("{base}/profiles/jake"), None).await;
    assert_eq!(body["profile"]["following"], false);
    let (status, _) = get(&client, format!("{base}/profiles/nobody"), None).await;
    assert_eq!(status, 404);

    // --- anne's feed contains jake's article, author marked followed ---
    let (status, body) = get(&client, format!("{base}/articles/feed?limit=5"), Some(&anne_token)).await;
    assert_eq!(status, 200, "feed: {body}");
    assert_eq!(body["articlesCount"], 1);
    assert_eq!(body["articles"][0]["author"]["username"], "jake");
    assert_eq!(body["articles"][0]["author"]["following"], true);

    // --- feed requires auth ---
    let (status, _) = get(&client, format!("{base}/articles/feed"), None).await;
    assert_eq!(status, 401);

    // --- anne favorites the article: flag + count in the response ---
    let favorite = client
        .post(format!("{base}/articles/{slug}/favorite"))
        .header("authorization", format!("Token {anne_token}"))
        .send()
        .await
        .unwrap();
    assert_eq!(favorite.status(), 200);
    let body = json_of(favorite).await;
    assert_eq!(body["article"]["favorited"], true);
    assert_eq!(body["article"]["favoritesCount"], 1);

    // --- list filters: tag, author, favorited-by; anon viewer ---
    let (_, body) = get(&client, format!("{base}/articles?tag=dragons"), None).await;
    assert_eq!(body["articlesCount"], 1, "tag filter: {body}");
    let (_, body) = get(&client, format!("{base}/articles?author=jake"), None).await;
    assert_eq!(body["articlesCount"], 1, "author filter: {body}");
    let (_, body) = get(&client, format!("{base}/articles?favorited=anne"), None).await;
    assert_eq!(body["articlesCount"], 1, "favorited filter: {body}");
    let (_, body) = get(&client, format!("{base}/articles?tag=nope"), None).await;
    assert_eq!(body["articlesCount"], 0, "no match: {body}");
    // pagination: offset past the end
    let (_, body) = get(&client, format!("{base}/articles?limit=1&offset=5"), None).await;
    assert_eq!(body["articles"].as_array().unwrap().len(), 0);
    assert_eq!(body["articlesCount"], 1, "count ignores pagination: {body}");

    // --- the favorited flag varies by viewer on GET article ---
    let (_, body) = get(&client, format!("{base}/articles/{slug}"), Some(&anne_token)).await;
    assert_eq!(body["article"]["favorited"], true);
    let (_, body) = get(&client, format!("{base}/articles/{slug}"), None).await;
    assert_eq!(body["article"]["favorited"], false);
    assert_eq!(body["article"]["favoritesCount"], 1);

    // --- unfavorite mirrors ---
    let unfavorite = client
        .delete(format!("{base}/articles/{slug}/favorite"))
        .header("authorization", format!("Token {anne_token}"))
        .send()
        .await
        .unwrap();
    let body = json_of(unfavorite).await;
    assert_eq!(body["article"]["favorited"], false);
    assert_eq!(body["article"]["favoritesCount"], 0);

    // --- authorization: anne cannot update or delete jake's article ---
    let put = client
        .put(format!("{base}/articles/{slug}"))
        .header("authorization", format!("Token {anne_token}"))
        .header("content-type", "application/json")
        .body(r#"{"article":{"title":"stolen"}}"#)
        .send()
        .await
        .unwrap();
    assert_eq!(put.status(), 403);
    let del = client
        .delete(format!("{base}/articles/{slug}"))
        .header("authorization", format!("Token {anne_token}"))
        .send()
        .await
        .unwrap();
    assert_eq!(del.status(), 403);

    // --- author updates: merged fields, new slug, updatedAt advances ---
    let put = client
        .put(format!("{base}/articles/{slug}"))
        .header("authorization", format!("Token {jake_token}"))
        .header("content-type", "application/json")
        .body(r#"{"article":{"title":"How to NOT Train Your Dragon"}}"#)
        .send()
        .await
        .unwrap();
    assert_eq!(put.status(), 200);
    let body = json_of(put).await;
    let new_slug = body["article"]["slug"].as_str().unwrap().to_owned();
    assert!(new_slug.starts_with("how-to-not-train-your-dragon-"), "re-slug: {new_slug}");
    assert_eq!(body["article"]["body"], "Very carefully.", "unsent fields kept");
    assert_ne!(body["article"]["updatedAt"], body["article"]["createdAt"]);
    // the old slug is gone
    let (status, _) = get(&client, format!("{base}/articles/{slug}"), None).await;
    assert_eq!(status, 404);

    // --- comments: add (auth), list varies following flag, delete own only ---
    let (status, body) = post(
        &client,
        format!("{base}/articles/{new_slug}/comments"),
        Some(&anne_token),
        r#"{"comment":{"body":"His name is Toothless."}}"#,
    )
    .await;
    assert_eq!(status, 200, "add comment: {body}");
    let comment_id = body["comment"]["id"].as_i64().expect("comment id");
    assert_eq!(body["comment"]["author"]["username"], "anne");

    let (_, body) = get(&client, format!("{base}/articles/{new_slug}/comments"), Some(&jake_token)).await;
    let comments = body["comments"].as_array().expect("comments");
    assert_eq!(comments.len(), 1);
    assert_eq!(comments[0]["author"]["following"], false, "jake does not follow anne");

    // jake cannot delete anne's comment
    let del = client
        .delete(format!("{base}/articles/{new_slug}/comments/{comment_id}"))
        .header("authorization", format!("Token {jake_token}"))
        .send()
        .await
        .unwrap();
    assert_eq!(del.status(), 403);
    // anne can
    let del = client
        .delete(format!("{base}/articles/{new_slug}/comments/{comment_id}"))
        .header("authorization", format!("Token {anne_token}"))
        .send()
        .await
        .unwrap();
    assert_eq!(del.status(), 200);
    let (_, body) = get(&client, format!("{base}/articles/{new_slug}/comments"), None).await;
    assert_eq!(body["comments"].as_array().unwrap().len(), 0);

    // --- tags reflect live articles ---
    let (_, body) = get(&client, format!("{base}/tags"), None).await;
    assert_eq!(body["tags"], serde_json::json!(["dragons", "training"]));

    // --- unfollow flips the profile flag back ---
    let unfollow = client
        .delete(format!("{base}/profiles/jake/follow"))
        .header("authorization", format!("Token {anne_token}"))
        .send()
        .await
        .unwrap();
    let body = json_of(unfollow).await;
    assert_eq!(body["profile"]["following"], false);
    let (_, body) = get(&client, format!("{base}/articles/feed"), Some(&anne_token)).await;
    assert_eq!(body["articlesCount"], 0, "feed empties after unfollow");

    // --- delete article cascades comments/favorites/tags in one tx ---
    let del = client
        .delete(format!("{base}/articles/{new_slug}"))
        .header("authorization", format!("Token {jake_token}"))
        .send()
        .await
        .unwrap();
    assert_eq!(del.status(), 200);
    let (status, _) = get(&client, format!("{base}/articles/{new_slug}"), None).await;
    assert_eq!(status, 404);
    let (_, body) = get(&client, format!("{base}/tags"), None).await;
    assert_eq!(body["tags"].as_array().unwrap().len(), 0, "tags gone with the article");
    let (_, body) = get(&client, format!("{base}/articles"), None).await;
    assert_eq!(body["articlesCount"], 0);

    // --- malformed body: 422 with the RealWorld error shape ---
    let (status, body) = post(&client, format!("{base}/users"), None, r#"{"nope":true}"#).await;
    assert_eq!(status, 422, "malformed body: {body}");
    assert!(body["errors"]["body"].is_array());
}
