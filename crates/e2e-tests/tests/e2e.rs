//! End-to-end tests: build the fixture Roc app, run the real server binary,
//! and drive it over HTTP.
//!
//! Requires `just` and the `roc` binary (env `ROC`, default
//! `~/Repositories/roc/zig-out/bin/roc`).

use std::net::TcpListener;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

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

/// Build the fixture app once and start one server shared by every test.
fn server() -> &'static Mutex<Server> {
    static SERVER: OnceLock<Mutex<Server>> = OnceLock::new();
    SERVER.get_or_init(|| {
        let root = workspace_root();

        let build = Command::new("just")
            .arg("build")
            .arg("examples/e2e/main.roc")
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
        let db_path = std::env::temp_dir().join(format!("tower-platform-e2e-{port}.db"));
        let _ = std::fs::remove_file(&db_path);
        // Detach stdio: the server process outlives the test binary (statics
        // never drop), and an inherited pipe would keep cargo's output stream
        // open forever.
        let child = Command::new(root.join("main"))
            .env("PORT", port.to_string())
            .env("E2E_PREFIX", "from-env")
            .env("E2E_DB_PATH", &db_path)
            .current_dir(&root)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("failed to start server");

        let server = Server {
            child,
            base_url: format!("http://127.0.0.1:{port}"),
        };

        // Wait for the listener to come up.
        let deadline = Instant::now() + Duration::from_secs(15);
        loop {
            match std::net::TcpStream::connect(("127.0.0.1", port)) {
                Ok(_) => break,
                Err(_) if Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(100))
                }
                Err(err) => panic!("server did not start listening: {err}"),
            }
        }

        Mutex::new(server)
    })
}

fn base_url() -> String {
    server().lock().expect("server mutex").base_url.clone()
}

fn client() -> reqwest::Client {
    reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .build()
        .expect("client")
}

#[tokio::test]
async fn routing_dispatches_to_the_matching_handler() {
    let base = base_url();
    let body = client().get(format!("{base}/")).send().await.unwrap();
    assert_eq!(body.status(), 200);
    assert_eq!(body.text().await.unwrap(), "root ok");
}

#[tokio::test]
async fn path_params_reach_the_handler() {
    let base = base_url();
    let body = client()
        .get(format!("{base}/users/42"))
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert_eq!(body, "user 42");
}

#[tokio::test]
async fn query_string_reaches_the_handler() {
    let base = base_url();
    let body = client()
        .get(format!("{base}/query?a=1&b=2"))
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert_eq!(body, "query=a=1&b=2");
}

#[tokio::test]
async fn unknown_path_returns_404() {
    let base = base_url();
    let response = client().get(format!("{base}/nope")).send().await.unwrap();
    assert_eq!(response.status(), 404);
}

#[tokio::test]
async fn wrong_method_returns_405_with_allow_header() {
    let base = base_url();
    let response = client().get(format!("{base}/echo")).send().await.unwrap();
    assert_eq!(response.status(), 405);
    assert_eq!(response.headers()["allow"], "POST");
}

#[tokio::test]
async fn request_body_round_trips_through_roc() {
    let base = base_url();
    let payload = b"ping-pong \x00\x01\x02 binary".to_vec();
    let response = client()
        .post(format!("{base}/echo"))
        .body(payload.clone())
        .send()
        .await
        .unwrap();
    assert_eq!(response.bytes().await.unwrap().as_ref(), payload.as_slice());
}

#[tokio::test]
async fn oversized_body_is_rejected_with_413() {
    let base = base_url();
    let response = client()
        .post(format!("{base}/echo"))
        .body(vec![0u8; 4096])
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 413);
}

#[tokio::test]
async fn cors_preflight_allows_any_origin() {
    let base = base_url();
    let response = client()
        .request(reqwest::Method::OPTIONS, format!("{base}/"))
        .header("origin", "http://example.com")
        .header("access-control-request-method", "GET")
        .send()
        .await
        .unwrap();
    assert_eq!(response.headers()["access-control-allow-origin"], "*");
}

#[tokio::test]
async fn large_responses_are_compressed_when_requested() {
    let base = base_url();
    // reqwest with gzip disabled: ask for gzip manually and inspect the header.
    let response = client()
        .get(format!("{base}/big"))
        .header("accept-encoding", "gzip")
        .send()
        .await
        .unwrap();
    assert_eq!(response.headers()["content-encoding"], "gzip");
}

#[tokio::test]
async fn static_files_are_served_from_the_mount() {
    let base = base_url();
    let body = client()
        .get(format!("{base}/static/hello.txt"))
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert_eq!(body.trim(), "static fixture content");
}

#[tokio::test]
async fn init_model_is_visible_to_handlers() {
    let base = base_url();
    let body = client()
        .get(format!("{base}/model"))
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert_eq!(body, "prefix=from-env");
}

#[tokio::test]
async fn sqlite_effect_persists_across_requests() {
    let base = base_url();
    let first: u64 = client()
        .get(format!("{base}/db"))
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap()
        .strip_prefix("hits=")
        .expect("hits prefix")
        .parse()
        .expect("count");
    let second: u64 = client()
        .get(format!("{base}/db"))
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap()
        .strip_prefix("hits=")
        .expect("hits prefix")
        .parse()
        .expect("count");
    assert_eq!(second, first + 1);
}

#[tokio::test]
async fn helper_json_round_trip_and_400() {
    let base = base_url();
    let ok = client()
        .post(format!("{base}/helper/greet"))
        .body(r#"{"name":"E2E"}"#)
        .send()
        .await
        .unwrap();
    assert_eq!(ok.status(), 200);
    assert_eq!(ok.headers()["content-type"], "application/json");
    assert_eq!(
        ok.text().await.unwrap(),
        r#"{"message":"hello, E2E"}"#
    );

    let bad = client()
        .post(format!("{base}/helper/greet"))
        .body("not json")
        .send()
        .await
        .unwrap();
    assert_eq!(bad.status(), 400);
}

#[tokio::test]
async fn helper_form_post_percent_decodes() {
    let base = base_url();
    let body = client()
        .post(format!("{base}/helper/form"))
        .header("content-type", "application/x-www-form-urlencoded")
        .body("a=1&b=x+y&c=%2F")
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert_eq!(body, "a=1;b=x y;c=/");
}

#[tokio::test]
async fn helper_query_bridges_missing_to_400_via_from_try() {
    let base = base_url();
    let ok = client()
        .get(format!("{base}/helper/q?name=zed"))
        .send()
        .await
        .unwrap();
    assert_eq!(ok.text().await.unwrap(), "q=zed");

    let missing = client()
        .get(format!("{base}/helper/q"))
        .send()
        .await
        .unwrap();
    assert_eq!(missing.status(), 400);
}

#[tokio::test]
async fn helper_redirect_sets_location() {
    let base = base_url();
    let no_follow = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(Duration::from_secs(10))
        .build()
        .unwrap();
    let response = no_follow
        .get(format!("{base}/helper/redirect"))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 302);
    assert_eq!(response.headers()["location"], "/");
}

#[tokio::test]
async fn helper_header_lookup_is_case_insensitive() {
    let base = base_url();
    let body = client()
        .get(format!("{base}/helper/header"))
        .header("X-CUSTOM", "abc")
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert_eq!(body, "x-custom=abc");
}

#[tokio::test]
async fn concurrent_requests_get_correct_isolated_responses() {
    let base = base_url();
    let client = client();
    let requests = (0..100).map(|index| {
        let client = client.clone();
        let base = base.clone();
        async move {
            let body = client
                .get(format!("{base}/users/{index}"))
                .send()
                .await
                .unwrap()
                .text()
                .await
                .unwrap();
            assert_eq!(body, format!("user {index}"));
        }
    });
    futures::future::join_all(requests).await;
}
