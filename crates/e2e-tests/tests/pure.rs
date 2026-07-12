//! End-to-end test for the pure coeffect/effect example (`examples/pure`):
//! coeffect resolution (reserved id, JSON body, read coeffect) → pure handler →
//! host-executed effects (transactional DbWrite, err_handler on constraint
//! violation, memoized single reservation).
//!
//! Requires `just` and the `roc` binary (env `ROC`, default
//! `~/Repositories/roc/zig-out/bin/roc`).

use std::net::TcpListener;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
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

/// The pure example uses this fixed DB path (see examples/pure/main.roc).
const DB_PATH: &str = "/tmp/tower-platform-pure-e2e.db";

fn start_server() -> Server {
    let root = workspace_root();

    // Fresh database each run: the example's UNIQUE(name) + reserved-id
    // sequence assertions depend on starting empty.
    for suffix in ["", "-wal", "-shm"] {
        let _ = std::fs::remove_file(format!("{DB_PATH}{suffix}"));
    }

    let build = Command::new("just")
        .arg("build")
        .arg("examples/pure/main.roc")
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
        base_url: format!("http://127.0.0.1:{port}"),
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

/// The whole pipeline in one sequential test: the shared `./main` binary and
/// the fixed DB path mean these steps must run in order, so they live in one
/// `#[test]`.
#[tokio::test]
async fn pure_pipeline_coeffects_and_effects() {
    let server = start_server();
    let base = &server.base_url;
    let client = client();

    // --- create: reserved id in the body, row written by the effect ---
    let created = client
        .post(format!("{base}/users"))
        .body(r#"{"name":"alice"}"#)
        .send()
        .await
        .unwrap();
    assert_eq!(created.status(), 201);
    let body = created.text().await.unwrap();
    assert!(
        body.contains("\"id\":1") && body.contains("\"name\":\"alice\""),
        "expected reserved id 1 for alice, got {body:?}"
    );

    // second distinct user reserves the next id
    let bob = client
        .post(format!("{base}/users"))
        .body(r#"{"name":"bob"}"#)
        .send()
        .await
        .unwrap();
    assert_eq!(bob.status(), 201);
    assert!(bob.text().await.unwrap().contains("\"id\":2"));

    // --- read coeffect: present -> 200, missing -> 404 ---
    let (status, body) = {
        let r = client.get(format!("{base}/users/1")).send().await.unwrap();
        (r.status().as_u16(), r.text().await.unwrap())
    };
    assert_eq!(status, 200);
    assert!(body.contains("\"name\":\"alice\""), "got {body:?}");

    let missing = client.get(format!("{base}/users/999")).send().await.unwrap();
    assert_eq!(missing.status(), 404);

    // --- duplicate name: Must fails -> err_handler 409, transaction rolls back ---
    let dup = client
        .post(format!("{base}/users"))
        .body(r#"{"name":"alice"}"#)
        .send()
        .await
        .unwrap();
    assert_eq!(dup.status(), 409);
    let dup_body = dup.text().await.unwrap();
    assert!(dup_body.contains("name taken"), "got {dup_body:?}");
    // the id was reserved (3) but rolled back, so no user with id 3 exists
    assert!(
        dup_body.contains("\"rolled_back_id\":3"),
        "expected the burned reservation id in {dup_body:?}"
    );
    let ghost = client.get(format!("{base}/users/3")).send().await.unwrap();
    assert_eq!(ghost.status(), 404, "rolled-back id 3 must not exist");

    // --- single reservation: two ReservedId slots share ONE memoized id ---
    let twice = client
        .get(format!("{base}/reserve-twice"))
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    // a == b (one reservation), and it is the next id after the burned 3.
    let a = twice
        .split(';')
        .find_map(|p| p.strip_prefix("a="))
        .and_then(|s| s.parse::<u64>().ok())
        .unwrap();
    let b = twice
        .split(';')
        .find_map(|p| p.strip_prefix("b="))
        .and_then(|s| s.parse::<u64>().ok())
        .unwrap();
    assert_eq!(a, b, "two ReservedId slots must resolve to one memoized id: {twice:?}");
    assert_eq!(a, 4, "reservation should continue the sequence (1,2,3-burned,4)");
}
