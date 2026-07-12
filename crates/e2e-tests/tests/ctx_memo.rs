//! SPIKE I1: per-request, host-registry-scoped coeffect memoization.
//!
//! Builds examples/ctxmemo, runs the real server binary, and proves:
//! 1. intra-request memoization (same key twice -> one injector run)
//! 2. cross-request isolation under genuine concurrency (N distinct values)
//! 3. leak-free teardown (registry + live-box gauge return to baseline;
//!    RSS stable across repeated bursts)
//! 4. teardown on rejection responses
//! 5. teardown when a hosted fn panics mid-handler
//!
//! Ordering matters (global counters, panic last), so everything runs in one
//! sequential test.

use std::collections::HashSet;
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

fn start_server() -> Server {
    let root = workspace_root();
    let build = Command::new("just")
        .arg("build")
        .arg("examples/ctxmemo/main.roc")
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

/// Parse "a=1;b=2" style bodies.
fn field(body: &str, name: &str) -> u64 {
    body.split(';')
        .find_map(|pair| pair.strip_prefix(&format!("{name}=")))
        .unwrap_or_else(|| panic!("field {name} missing in {body:?}"))
        .parse()
        .unwrap_or_else(|_| panic!("field {name} not a number in {body:?}"))
}

async fn get_text(client: &reqwest::Client, base: &str, path: &str) -> (u16, String) {
    let response = client
        .get(format!("{base}{path}"))
        .send()
        .await
        .unwrap_or_else(|err| panic!("GET {path} failed: {err}"));
    let status = response.status().as_u16();
    (status, response.text().await.expect("body"))
}

async fn runs(client: &reqwest::Client, base: &str) -> u64 {
    let (status, body) = get_text(client, base, "/runs").await;
    assert_eq!(status, 200);
    field(&body, "runs")
}

/// Registry size and live-box gauge, as seen by a request that is itself
/// live: a quiescent server reports registry=1, live_boxes=0.
async fn registry(client: &reqwest::Client, base: &str) -> (u64, u64) {
    let (status, body) = get_text(client, base, "/registry").await;
    assert_eq!(status, 200);
    (field(&body, "registry"), field(&body, "live_boxes"))
}

fn rss_kb(pid: u32) -> u64 {
    let out = Command::new("ps")
        .args(["-o", "rss=", "-p", &pid.to_string()])
        .output()
        .expect("ps");
    String::from_utf8_lossy(&out.stdout).trim().parse().expect("rss")
}

const BURST: usize = 64;

/// Fire one concurrent burst of /memo requests; return the distinct reserved
/// values and distinct ctx ids (both must equal the burst size).
async fn memo_burst(client: &reqwest::Client, base: &str) -> (HashSet<u64>, HashSet<u64>) {
    let calls = (0..BURST).map(|_| get_text(client, base, "/memo"));
    let responses = futures::future::join_all(calls).await;
    let mut values = HashSet::new();
    let mut ctxs = HashSet::new();
    for (status, body) in responses {
        assert_eq!(status, 200, "burst response failed: {body:?}");
        let first = field(&body, "first");
        let second = field(&body, "second");
        assert_eq!(first, second, "intra-request memoization broke: {body:?}");
        values.insert(first);
        ctxs.insert(field(&body, "ctx"));
    }
    (values, ctxs)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn spike_i1_per_request_memo_isolation() {
    let server = start_server();
    let base = server.base_url.clone();
    let pid = server.child.id();
    let client = client();

    // ---- Assert 1: intra-request memoization -----------------------------
    let runs_before = runs(&client, &base).await;
    let (status, body) = get_text(&client, &base, "/memo").await;
    assert_eq!(status, 200);
    let first = field(&body, "first");
    let second = field(&body, "second");
    assert_eq!(first, second, "same key must resolve once: {body:?}");
    let runs_after = runs(&client, &base).await;
    assert_eq!(runs_after - runs_before, 1, "injector must run exactly once");
    println!("assert 1 PASS: first={first} second={second} runs_delta=1");

    // ---- Assert 2: cross-request isolation under concurrency -------------
    let runs_before = runs(&client, &base).await;
    let (values, ctxs) = memo_burst(&client, &base).await;
    let runs_after = runs(&client, &base).await;
    assert_eq!(values.len(), BURST, "reserved values must be distinct across requests");
    assert_eq!(ctxs.len(), BURST, "ctx ids must be distinct across requests");
    assert_eq!(
        runs_after - runs_before,
        BURST as u64,
        "each request's injector must run exactly once"
    );
    println!(
        "assert 2 PASS: N={BURST} distinct_values={} distinct_ctxs={} runs_delta={}",
        values.len(),
        ctxs.len(),
        runs_after - runs_before
    );

    // ---- Assert 3: teardown + no growth across repeated bursts -----------
    let (size, live) = registry(&client, &base).await;
    assert_eq!((size, live), (1, 0), "registry must be empty after the burst");
    let rss_first = rss_kb(pid);
    for round in 0..5 {
        let (values, _ctxs) = memo_burst(&client, &base).await;
        assert_eq!(values.len(), BURST);
        let (size, live) = registry(&client, &base).await;
        assert_eq!(
            (size, live),
            (1, 0),
            "registry must drain after burst round {round}"
        );
    }
    let rss_last = rss_kb(pid);
    println!(
        "assert 3 PASS: registry=1(live probe) live_boxes=0 after each of 5x{BURST} bursts; \
         rss {rss_first} KB -> {rss_last} KB"
    );
    assert!(
        rss_last.saturating_sub(rss_first) < 32 * 1024,
        "RSS grew {} KB over 5 bursts",
        rss_last.saturating_sub(rss_first)
    );

    // ---- Assert 4: rejection teardown -------------------------------------
    let (status, body) = get_text(&client, &base, "/reject").await;
    assert_eq!(status, 403, "rejection response expected: {body:?}");
    assert!(body.starts_with("rejected after a="), "body: {body:?}");
    let (size, live) = registry(&client, &base).await;
    assert_eq!(
        (size, live),
        (1, 0),
        "request must tear down after rejection (A's thunk released)"
    );
    println!("assert 4 PASS: {body:?} then registry=1 live_boxes=0");

    // ---- Assert 5: panic teardown (last: it unwinds through Roc frames) --
    let (status, body) = get_text(&client, &base, "/panic").await;
    assert_eq!(status, 500, "panicked handler must surface as 500: {body:?}");
    let (size, live) = registry(&client, &base).await;
    assert_eq!(
        (size, live),
        (1, 0),
        "request must tear down after a host panic"
    );
    // The server must still work.
    let (status, body) = get_text(&client, &base, "/memo").await;
    assert_eq!(status, 200);
    assert_eq!(field(&body, "first"), field(&body, "second"));
    println!("assert 5 PASS: panic -> 500, registry drained, server still serving");
}
