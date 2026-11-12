//! Idempotent retries, live event streams, and webhook delivery.
use agent_workboard::{Command, Error, Store, mcp, web, web::Hub, webhooks};
use axum::{
    Router,
    body::{Body, to_bytes},
    extract::State,
    http::{HeaderMap, Request, StatusCode},
    routing::post,
};
use serde_json::{Value, json};
use std::{
    net::SocketAddr,
    process::Command as Process,
    sync::{
        Arc, Mutex,
        atomic::{AtomicU16, Ordering},
    },
    time::{Duration, Instant},
};
use tempfile::TempDir;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
};
use tower::ServiceExt;

fn cmd(value: Value) -> Command {
    serde_json::from_value(value).unwrap()
}
fn run(store: &mut Store, actor: &str, value: Value) -> Value {
    store.execute(actor, cmd(value)).unwrap()
}
fn run_idem(store: &mut Store, actor: &str, key: &str, value: Value) -> (Value, bool) {
    store
        .execute_idempotent(actor, Some(key), cmd(value))
        .unwrap()
}
fn actions(store: &mut Store, name: &str) -> usize {
    run(store, "admin", json!({"op":"events","limit":1000}))
        .as_array()
        .unwrap()
        .iter()
        .filter(|e| e["action"] == name)
        .count()
}
fn latest_event(store: &mut Store) -> i64 {
    run(store, "admin", json!({"op":"events","limit":1000}))
        .as_array()
        .unwrap()
        .last()
        .unwrap()["id"]
        .as_i64()
        .unwrap()
}
async fn until(what: &str, seconds: u64, mut ready: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(seconds);
    while !ready() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        tokio::time::sleep(Duration::from_millis(40)).await;
    }
}

struct Board {
    dir: TempDir,
    key: String,
}
impl Board {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let mut store = Store::open(dir.path().join("push.sqlite3")).unwrap();
        for id in ["admin", "worker"] {
            run(
                &mut store,
                "admin",
                json!({"op":"agent_register","id":id,"name":id}),
            );
        }
        run(
            &mut store,
            "admin",
            json!({"op":"project_create","key":"PUSH","name":"Push"}),
        );
        let key = run(
            &mut store,
            "admin",
            json!({"op":"api_key_create","agent":"worker","label":"t"}),
        )["api_key"]
            .as_str()
            .unwrap()
            .to_owned();
        Self { dir, key }
    }
    fn open(&self) -> Store {
        Store::open(self.dir.path().join("push.sqlite3")).unwrap()
    }
    fn sql(&self) -> rusqlite::Connection {
        rusqlite::Connection::open(self.dir.path().join("push.sqlite3")).unwrap()
    }
}

// ---------------------------------------------------------------- idempotency

#[test]
fn a_retried_command_returns_the_original_result_and_applies_once() {
    let b = Board::new();
    let mut s = b.open();
    let create = json!({"op":"issue_create","project":"PUSH","kind":"task","title":"once"});
    let (first, replayed) = run_idem(&mut s, "worker", "req-1", create.clone());
    assert!(!replayed);
    let (second, replayed) = run_idem(&mut s, "worker", "req-1", create.clone());
    assert!(replayed, "second call must be a replay");
    assert_eq!(first, second, "replay returns the stored result verbatim");
    assert_eq!(actions(&mut s, "issue_created"), 1);

    // The key is scoped to the actor: another agent's identical key is independent.
    let (_, replayed) = run_idem(&mut s, "admin", "req-1", create.clone());
    assert!(!replayed);
    assert_eq!(actions(&mut s, "issue_created"), 2);

    // Reusing a key for a different request is rejected rather than ignored.
    let other = json!({"op":"issue_create","project":"PUSH","kind":"task","title":"different"});
    let error = s
        .execute_idempotent("worker", Some("req-1"), cmd(other))
        .unwrap_err();
    assert!(matches!(error, Error::Conflict(_)), "{error}");
    assert_eq!(actions(&mut s, "issue_created"), 2);
}

#[test]
fn idempotency_keys_are_validated_and_exclude_secrets_failures_and_reads() {
    let b = Board::new();
    let mut s = b.open();
    for bad in ["", "has space", "emoji-😀", &"x".repeat(129)] {
        let e = s
            .execute_idempotent("worker", Some(bad), cmd(json!({"op":"agent_list"})))
            .unwrap_err();
        assert!(matches!(e, Error::Invalid(_)), "{bad:?} accepted");
    }
    // Replaying an API key or webhook secret would store it in plaintext.
    for admin in [
        json!({"op":"api_key_create","agent":"worker","label":"x"}),
        json!({"op":"webhook_list"}),
    ] {
        let e = s
            .execute_idempotent("admin", Some("k"), cmd(admin))
            .unwrap_err();
        assert!(matches!(e, Error::Invalid(_)));
    }
    // A failed command rolls back entirely, so its key stays usable for the retry.
    let late = json!({"op":"issue_create","project":"LATE","kind":"task","title":"x"});
    assert!(matches!(
        s.execute_idempotent("worker", Some("retry-me"), cmd(late.clone())),
        Err(Error::NotFound(_))
    ));
    run(
        &mut s,
        "admin",
        json!({"op":"project_create","key":"LATE","name":"Late"}),
    );
    let (_, replayed) = run_idem(&mut s, "worker", "retry-me", late);
    assert!(!replayed);
    // Reads are never recorded.
    run_idem(&mut s, "worker", "read-1", json!({"op":"agent_list"}));
    let (_, replayed) = run_idem(&mut s, "worker", "read-1", json!({"op":"agent_list"}));
    assert!(!replayed);
    let stored: i64 = b
        .sql()
        .query_row(
            "SELECT COUNT(*) FROM idempotency WHERE key='read-1'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(stored, 0);
}

#[test]
fn idempotency_records_expire_after_a_day() {
    let b = Board::new();
    let mut s = b.open();
    let create = json!({"op":"issue_create","project":"PUSH","kind":"task","title":"daily"});
    run_idem(&mut s, "worker", "old", create.clone());
    b.sql()
        .execute("UPDATE idempotency SET created_at=created_at-90000", [])
        .unwrap();
    let (_, replayed) = run_idem(&mut s, "worker", "old", create);
    assert!(!replayed, "an expired key must run again");
    assert_eq!(actions(&mut s, "issue_created"), 2);
    let rows: i64 = b
        .sql()
        .query_row(
            "SELECT COUNT(*) FROM idempotency WHERE key='old'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(rows, 1, "the stale record is replaced, not duplicated");
}

#[test]
fn racing_retries_with_one_key_apply_exactly_once() {
    let b = Board::new();
    let barrier = Arc::new(std::sync::Barrier::new(8));
    let handles: Vec<_> = (0..8)
        .map(|_| {
            let (store, barrier) = (b.open(), barrier.clone());
            std::thread::spawn(move || {
                let mut store = store;
                barrier.wait();
                store
                    .execute_idempotent(
                        "worker",
                        Some("race"),
                        cmd(json!({"op":"issue_create","project":"PUSH","kind":"task","title":"race"})),
                    )
                    .unwrap()
            })
        })
        .collect();
    let results: Vec<_> = handles.into_iter().map(|h| h.join().unwrap()).collect();
    assert_eq!(results.iter().filter(|(_, replayed)| !replayed).count(), 1);
    assert!(results.iter().all(|(value, _)| value == &results[0].0));
    assert_eq!(actions(&mut b.open(), "issue_created"), 1);
}

async fn http(
    app: &Router,
    method: &str,
    path: &str,
    key: &str,
    extra: &[(&str, &str)],
    body: Value,
) -> (StatusCode, HeaderMap, Value) {
    let mut request = Request::builder()
        .method(method)
        .uri(path)
        .header("Authorization", format!("Bearer {key}"))
        .header("Content-Type", "application/json");
    for (name, value) in extra {
        request = request.header(*name, *value);
    }
    let response = app
        .clone()
        .oneshot(request.body(Body::from(body.to_string())).unwrap())
        .await
        .unwrap();
    let (status, headers) = (response.status(), response.headers().clone());
    let bytes = to_bytes(response.into_body(), 1 << 20).await.unwrap();
    (
        status,
        headers,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

#[tokio::test]
async fn http_idempotency_key_header_replays_and_flags_the_response() {
    let b = Board::new();
    let app = web::router(b.open());
    let create = json!({"op":"issue_create","project":"PUSH","kind":"task","title":"http"});
    let header = [("Idempotency-Key", "http-1")];
    let (status, headers, first) = http(
        &app,
        "POST",
        "/api/commands",
        &b.key,
        &header,
        create.clone(),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(headers.get("idempotent-replayed").is_none());
    let (status, headers, second) =
        http(&app, "POST", "/api/commands", &b.key, &header, create).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(headers["idempotent-replayed"], "true");
    assert_eq!(first, second);
    assert_eq!(actions(&mut b.open(), "issue_created"), 1);

    let different = json!({"op":"issue_create","project":"PUSH","kind":"task","title":"other"});
    let (status, _, body) = http(
        &app,
        "POST",
        "/api/commands",
        &b.key,
        &header,
        different.clone(),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    let (status, _, _) = http(
        &app,
        "POST",
        "/api/commands",
        &b.key,
        &[("Idempotency-Key", "no spaces")],
        different,
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[test]
fn cli_and_mcp_accept_idempotency_keys() {
    let b = Board::new();
    let db = b.dir.path().join("push.sqlite3");
    let create =
        json!({"op":"issue_create","project":"PUSH","kind":"task","title":"cli"}).to_string();
    let call = || {
        let out = Process::new(env!("CARGO_BIN_EXE_workboard"))
            .arg("--db")
            .arg(&db)
            .args([
                "--actor",
                "worker",
                "run",
                &create,
                "--idempotency-key",
                "cli-1",
            ])
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stdout)
        );
        serde_json::from_slice::<Value>(&out.stdout).unwrap()
    };
    let (first, second) = (call(), call());
    assert!(first.get("replayed").is_none());
    assert_eq!(second["replayed"], true);
    assert_eq!(first["result"], second["result"]);
    assert_eq!(actions(&mut b.open(), "issue_created"), 1);

    let mut server = mcp::Server::new(b.open(), "worker".into());
    let ask = |server: &mut mcp::Server, id: i64, method: &str, params: Value| {
        server
            .handle(json!({"jsonrpc":"2.0","id":id,"method":method,"params":params}))
            .unwrap()["result"]
            .clone()
    };
    ask(
        &mut server,
        1,
        "initialize",
        json!({"protocolVersion":"2025-11-25","capabilities":{},"clientInfo":{"name":"t","version":"1"}}),
    );
    let schema = &ask(&mut server, 2, "tools/list", json!({}))["tools"][0]["inputSchema"];
    assert!(
        schema["oneOf"]
            .as_array()
            .unwrap()
            .iter()
            .all(|v| v["properties"].get("idempotency_key").is_some()),
        "every command variant must document idempotency_key"
    );
    let args = json!({"op":"issue_create","project":"PUSH","kind":"task","title":"mcp","idempotency_key":"mcp-1"});
    let first = ask(
        &mut server,
        3,
        "tools/call",
        json!({"name":"workboard_execute","arguments":args}),
    );
    let second = ask(
        &mut server,
        4,
        "tools/call",
        json!({"name":"workboard_execute","arguments":args}),
    );
    assert_eq!(first["isError"], false);
    assert!(first["structuredContent"].get("replayed").is_none());
    assert_eq!(second["structuredContent"]["replayed"], true);
    assert_eq!(
        first["structuredContent"]["result"],
        second["structuredContent"]["result"]
    );
    let bad = ask(
        &mut server,
        5,
        "tools/call",
        json!({"name":"workboard_execute","arguments":{"op":"agent_list","idempotency_key":7}}),
    );
    assert_eq!(bad["isError"], true);
}

// ------------------------------------------------------------- event streams

#[derive(Debug)]
struct Ev {
    id: i64,
    action: String,
    data: Value,
}
struct Stream {
    socket: TcpStream,
    status: u16,
    pending: String,
    seen: String,
}
impl Stream {
    async fn open(
        addr: SocketAddr,
        path: &str,
        key: Option<&str>,
        headers: &[(&str, &str)],
    ) -> Self {
        let mut socket = TcpStream::connect(addr).await.unwrap();
        let mut request = format!("GET {path} HTTP/1.0\r\nHost: localhost\r\n");
        if let Some(key) = key {
            request += &format!("Authorization: Bearer {key}\r\n");
        }
        for (name, value) in headers {
            request += &format!("{name}: {value}\r\n");
        }
        socket
            .write_all(format!("{request}\r\n").as_bytes())
            .await
            .unwrap();
        let mut head = Vec::new();
        let mut byte = [0u8; 1];
        while !head.ends_with(b"\r\n\r\n") {
            assert_eq!(
                socket.read(&mut byte).await.unwrap(),
                1,
                "closed during headers"
            );
            head.push(byte[0]);
        }
        let head = String::from_utf8(head).unwrap();
        let status = head.split_whitespace().nth(1).unwrap().parse().unwrap();
        Self {
            socket,
            status,
            pending: String::new(),
            seen: String::new(),
        }
    }
    /// The next event, or why none arrived: "timeout" or "closed".
    async fn next(&mut self, wait: Duration) -> Result<Ev, &'static str> {
        let deadline = Instant::now() + wait;
        loop {
            while let Some(end) = self.pending.find("\n\n") {
                let block: String = self.pending.drain(..end + 2).collect();
                let field = |name: &str| {
                    block
                        .lines()
                        .find_map(|l| l.strip_prefix(name).map(|v| v.trim_start().to_owned()))
                };
                if let (Some(id), Some(action), Some(data)) =
                    (field("id:"), field("event:"), field("data:"))
                {
                    return Ok(Ev {
                        id: id.parse().unwrap(),
                        action,
                        data: serde_json::from_str(&data).unwrap(),
                    });
                }
            }
            let left = deadline.saturating_duration_since(Instant::now());
            let mut buffer = [0u8; 8192];
            match tokio::time::timeout(left, self.socket.read(&mut buffer)).await {
                Err(_) => return Err("timeout"),
                Ok(Ok(0) | Err(_)) => return Err("closed"),
                Ok(Ok(n)) => {
                    let text = String::from_utf8_lossy(&buffer[..n]);
                    self.pending += &text;
                    self.seen += &text;
                }
            }
        }
    }
}
async fn serve(b: &Board) -> (SocketAddr, Arc<Hub>) {
    let hub = Arc::new(Hub::new());
    let app = web::shared_router(Arc::new(Mutex::new(b.open())), hub.clone());
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (addr, hub)
}
const SECOND: Duration = Duration::from_secs(1);
const PATIENT: Duration = Duration::from_secs(5);

#[tokio::test]
async fn a_stream_replays_resumes_and_then_delivers_new_events_live() {
    let b = Board::new();
    let (addr, _) = serve(&b).await;
    let newest = latest_event(&mut b.open());

    let mut replay = Stream::open(addr, "/api/events/stream?after=0", Some(&b.key), &[]).await;
    assert_eq!(replay.status, 200);
    for expected in 1..=newest {
        assert_eq!(replay.next(SECOND).await.unwrap().id, expected);
    }
    assert_eq!(
        replay.next(Duration::from_millis(300)).await.unwrap_err(),
        "timeout"
    );

    // A reconnecting client sends Last-Event-ID and receives only what it missed.
    let header = (newest - 1).to_string();
    let mut resumed = Stream::open(
        addr,
        "/api/events/stream",
        Some(&b.key),
        &[("Last-Event-ID", &header)],
    )
    .await;
    assert_eq!(resumed.next(SECOND).await.unwrap().id, newest);
    assert_eq!(
        resumed.next(Duration::from_millis(300)).await.unwrap_err(),
        "timeout"
    );

    // A fresh stream starts at "now". Writes by another process are noticed by polling.
    let mut live = Stream::open(addr, "/api/events/stream", Some(&b.key), &[]).await;
    assert_eq!(
        live.next(Duration::from_millis(300)).await.unwrap_err(),
        "timeout"
    );
    let mut other_process = b.open();
    run(
        &mut other_process,
        "admin",
        json!({"op":"issue_create","project":"PUSH","kind":"task","title":"live"}),
    );
    let event = live.next(PATIENT).await.unwrap();
    assert_eq!(
        (event.action.as_str(), event.data["issue"].as_str()),
        ("issue_created", Some("PUSH-1"))
    );
    assert_eq!(event.id, newest + 1);

    // Writes through the HTTP API wake the stream through the hub.
    let app = web::shared_router(Arc::new(Mutex::new(b.open())), Arc::new(Hub::new()));
    http(
        &app,
        "POST",
        "/api/commands",
        &b.key,
        &[],
        json!({"op":"comment_add","key":"PUSH-1","body":"hi"}),
    )
    .await;
    assert_eq!(live.next(PATIENT).await.unwrap().action, "comment_added");
}

#[tokio::test]
async fn streams_require_a_key_validate_scope_and_filter_by_project() {
    let b = Board::new();
    let mut s = b.open();
    run(
        &mut s,
        "admin",
        json!({"op":"project_create","key":"OTHER","name":"Other"}),
    );
    run(
        &mut s,
        "admin",
        json!({"op":"issue_create","project":"PUSH","kind":"task","title":"mine"}),
    );
    run(
        &mut s,
        "admin",
        json!({"op":"issue_create","project":"OTHER","kind":"task","title":"theirs"}),
    );
    let (addr, _) = serve(&b).await;
    assert_eq!(
        Stream::open(addr, "/api/events/stream", None, &[])
            .await
            .status,
        401
    );
    assert_eq!(
        Stream::open(addr, "/api/events/stream", Some("wb_nope"), &[])
            .await
            .status,
        401
    );
    assert_eq!(
        Stream::open(
            addr,
            "/api/events/stream?project=MISSING",
            Some(&b.key),
            &[]
        )
        .await
        .status,
        404
    );
    assert_eq!(
        Stream::open(addr, "/api/events/stream?after=-4", Some(&b.key), &[])
            .await
            .status,
        400
    );

    let mut scoped = Stream::open(
        addr,
        "/api/events/stream?project=PUSH&after=0",
        Some(&b.key),
        &[],
    )
    .await;
    assert_eq!(scoped.status, 200);
    let mut issues = vec![];
    while let Ok(event) = scoped.next(Duration::from_millis(400)).await {
        if let Some(issue) = event.data["issue"].as_str() {
            issues.push(issue.to_owned());
        }
    }
    assert_eq!(
        issues,
        ["PUSH-1"],
        "OTHER-1 must not leak into a PUSH stream"
    );
}

#[tokio::test]
async fn streams_never_carry_lease_tokens() {
    let b = Board::new();
    let mut s = b.open();
    run(
        &mut s,
        "admin",
        json!({"op":"issue_create","project":"PUSH","kind":"task","title":"leased"}),
    );
    run(
        &mut s,
        "admin",
        json!({"op":"issue_transition","key":"PUSH-1","expected_revision":1,"state":"ready"}),
    );
    let claimed = run(
        &mut s,
        "worker",
        json!({"op":"issue_claim","project":"PUSH"}),
    );
    let token = claimed["claimed"]["lease"]["token"]
        .as_str()
        .unwrap()
        .to_owned();
    assert!(
        run(&mut s, "admin", json!({"op":"events","limit":1000}))
            .to_string()
            .contains(&token),
        "precondition: the raw log does hold the token"
    );
    let (addr, _) = serve(&b).await;
    let mut stream = Stream::open(addr, "/api/events/stream?after=0", Some(&b.key), &[]).await;
    let mut claim_seen = false;
    while let Ok(event) = stream.next(Duration::from_millis(400)).await {
        claim_seen |= event.action == "issue_claimed";
    }
    assert!(claim_seen);
    assert!(
        !stream.seen.contains(&token),
        "lease token leaked into the stream"
    );
    assert!(!stream.seen.contains(&b.key));
}

#[tokio::test]
async fn revoking_a_key_ends_its_open_stream() {
    let b = Board::new();
    let (addr, _) = serve(&b).await;
    let mut stream = Stream::open(addr, "/api/events/stream", Some(&b.key), &[]).await;
    assert_eq!(
        stream.next(Duration::from_millis(300)).await.unwrap_err(),
        "timeout"
    );
    let mut operator = b.open();
    let keys = run(&mut operator, "admin", json!({"op":"api_key_list"}));
    let id = keys[0]["id"].as_str().unwrap();
    run(
        &mut operator,
        "admin",
        json!({"op":"api_key_revoke","id":id}),
    );
    assert_eq!(stream.next(PATIENT).await.unwrap_err(), "closed");
    assert_eq!(
        Stream::open(addr, "/api/events/stream", Some(&b.key), &[])
            .await
            .status,
        401
    );
}

#[tokio::test]
async fn shutdown_closes_open_streams_so_the_server_can_stop() {
    let b = Board::new();
    let (addr, hub) = serve(&b).await;
    let mut stream = Stream::open(addr, "/api/events/stream", Some(&b.key), &[]).await;
    assert_eq!(
        stream.next(Duration::from_millis(300)).await.unwrap_err(),
        "timeout"
    );
    hub.close();
    assert_eq!(stream.next(PATIENT).await.unwrap_err(), "closed");
}

// ------------------------------------------------------------------ webhooks

#[derive(Clone)]
struct Sink {
    received: Arc<Mutex<Vec<(HeaderMap, String)>>>,
    status: Arc<AtomicU16>,
}
impl Sink {
    fn count(&self) -> usize {
        self.received.lock().unwrap().len()
    }
    fn bodies(&self) -> Vec<Value> {
        self.received
            .lock()
            .unwrap()
            .iter()
            .map(|(_, b)| serde_json::from_str(b).unwrap())
            .collect()
    }
}
async fn receiver(status: u16) -> (String, Sink) {
    let sink = Sink {
        received: Default::default(),
        status: Arc::new(AtomicU16::new(status)),
    };
    async fn hook(State(sink): State<Sink>, headers: HeaderMap, body: String) -> StatusCode {
        sink.received.lock().unwrap().push((headers, body));
        StatusCode::from_u16(sink.status.load(Ordering::SeqCst)).unwrap()
    }
    let app = Router::new()
        .route("/services/SECRETPATH", post(hook))
        .with_state(sink.clone());
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!(
        "http://{}/services/SECRETPATH",
        listener.local_addr().unwrap()
    );
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (url, sink)
}
fn delivery(b: &Board) -> (Arc<Mutex<Store>>, Arc<Hub>) {
    let db = Arc::new(Mutex::new(b.open()));
    let hub = Arc::new(Hub::new());
    tokio::spawn(webhooks::run(db.clone(), hub.subscribe()));
    (db, hub)
}
fn webhook_state(b: &Board, id: &str) -> Value {
    let list = run(&mut b.open(), "admin", json!({"op":"webhook_list"}));
    list.as_array()
        .unwrap()
        .iter()
        .find(|w| w["id"] == id)
        .unwrap()
        .clone()
}

struct GatedReceiver {
    url: String,
    requests: tokio::sync::mpsc::UnboundedReceiver<Value>,
    permits: Arc<tokio::sync::Semaphore>,
    status: Arc<AtomicU16>,
    shutdown: tokio::sync::oneshot::Sender<()>,
    server: tokio::task::JoinHandle<()>,
}
impl GatedReceiver {
    async fn new() -> Self {
        let permits = Arc::new(tokio::sync::Semaphore::new(0));
        let (sender, requests) = tokio::sync::mpsc::unbounded_channel();
        let status = Arc::new(AtomicU16::new(200));
        let state = (sender, permits.clone(), status.clone());
        async fn hook(
            State((sender, permits, status)): State<(
                tokio::sync::mpsc::UnboundedSender<Value>,
                Arc<tokio::sync::Semaphore>,
                Arc<AtomicU16>,
            )>,
            body: String,
        ) -> StatusCode {
            let _ = sender.send(serde_json::from_str(&body).unwrap());
            if let Ok(permit) = permits.acquire().await {
                permit.forget();
            }
            StatusCode::from_u16(status.load(Ordering::SeqCst)).unwrap()
        }
        let app = Router::new().route("/hook", post(hook)).with_state(state);
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/hook", listener.local_addr().unwrap());
        let (shutdown, stopped) = tokio::sync::oneshot::channel();
        let server = tokio::spawn(async move {
            axum::serve(listener, app)
                .with_graceful_shutdown(async {
                    let _ = stopped.await;
                })
                .await
                .unwrap();
        });
        Self {
            url,
            requests,
            permits,
            status,
            shutdown,
            server,
        }
    }
    async fn next(&mut self) -> Option<Value> {
        tokio::time::timeout(Duration::from_secs(3), self.requests.recv())
            .await
            .ok()
            .flatten()
    }
    async fn stop(self) {
        self.permits.close();
        let _ = self.shutdown.send(());
        self.server.await.unwrap();
    }
}

fn gated_subscription(db: &webhooks::Database, receiver: &GatedReceiver) -> String {
    run(
        &mut db.lock().unwrap(),
        "admin",
        json!({"op":"webhook_create","url":receiver.url,"label":"gated","project":"PUSH","actions":["issue_created"]}),
    )["webhook"]["id"].as_str().unwrap().to_owned()
}

fn queued_issues(db: &webhooks::Database, count: usize) -> Vec<i64> {
    let mut store = db.lock().unwrap();
    (0..count).map(|index| {
        run(&mut store, "admin", json!({"op":"issue_create","project":"PUSH","kind":"task","title":format!("queued-{index}")}));
        latest_event(&mut store)
    }).collect()
}

async fn cursor_reaches(b: &Board, id: &str, cursor: i64) -> bool {
    tokio::time::timeout(Duration::from_secs(3), async {
        while webhook_state(b, id)["cursor"].as_i64().unwrap() < cursor {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .is_ok()
}

#[tokio::test]
async fn dispatcher_regression_slow_receiver_does_not_block_healthy_next_event() {
    let board = Board::new();
    let db = Arc::new(Mutex::new(board.open()));
    let hub = Hub::new();
    let mut slow = GatedReceiver::new().await;
    let mut healthy = GatedReceiver::new().await;
    gated_subscription(&db, &slow);
    let healthy_id = gated_subscription(&db, &healthy);
    let first_id = queued_issues(&db, 1)[0];
    let worker = tokio::spawn(webhooks::run(db.clone(), hub.subscribe()));
    let slow_first = slow.next().await;
    let healthy_first = healthy.next().await;
    healthy.permits.add_permits(1);
    let first_acked = cursor_reaches(&board, &healthy_id, first_id).await;
    let second_id = queued_issues(&db, 1)[0];
    hub.notify();
    let healthy_second = healthy.next().await;
    worker.abort();
    let _ = worker.await;
    slow.stop().await;
    healthy.stop().await;
    assert_eq!(
        slow_first.map(|event| event["id"].clone()),
        Some(json!(first_id))
    );
    assert_eq!(
        healthy_first.map(|event| event["id"].clone()),
        Some(json!(first_id))
    );
    assert!(first_acked, "healthy first event must be acknowledged");
    assert_eq!(
        healthy_second.map(|event| event["id"].clone()),
        Some(json!(second_id)),
        "healthy second event must arrive before releasing the slow response"
    );
}

#[tokio::test]
async fn dispatcher_regression_delete_stops_subsequent_batch_requests() {
    let board = Board::new();
    let db = Arc::new(Mutex::new(board.open()));
    let hub = Hub::new();
    let mut receiver = GatedReceiver::new().await;
    let id = gated_subscription(&db, &receiver);
    let events = queued_issues(&db, 3);
    let worker = tokio::spawn(webhooks::run(db.clone(), hub.subscribe()));
    let first = receiver.next().await;
    run(
        &mut db.lock().unwrap(),
        "admin",
        json!({"op":"webhook_delete","id":id}),
    );
    hub.notify();
    receiver.permits.add_permits(1);
    let subsequent = receiver.next().await;
    worker.abort();
    let _ = worker.await;
    receiver.stop().await;
    assert_eq!(
        first.map(|event| event["id"].clone()),
        Some(json!(events[0]))
    );
    assert!(
        subsequent.is_none(),
        "deleted webhook must not initiate requests 2/3: {subsequent:?}"
    );
}

#[tokio::test]
async fn dispatcher_regression_pause_stops_batch_and_resume_preserves_remaining_order() {
    let board = Board::new();
    let db = Arc::new(Mutex::new(board.open()));
    let hub = Hub::new();
    let mut receiver = GatedReceiver::new().await;
    let id = gated_subscription(&db, &receiver);
    let events = queued_issues(&db, 3);
    let worker = tokio::spawn(webhooks::run(db.clone(), hub.subscribe()));
    let first = receiver.next().await;
    run(
        &mut db.lock().unwrap(),
        "admin",
        json!({"op":"webhook_set_active","id":id,"active":false}),
    );
    hub.notify();
    receiver.permits.add_permits(1);
    let first_acked = cursor_reaches(&board, &id, events[0]).await;
    let while_paused = receiver.next().await;
    let paused_cursor = webhook_state(&board, &id)["cursor"].clone();
    run(
        &mut db.lock().unwrap(),
        "admin",
        json!({"op":"webhook_set_active","id":id,"active":true}),
    );
    hub.notify();
    receiver.permits.add_permits(3);
    let second = receiver.next().await;
    let third = receiver.next().await;
    let all_acked = cursor_reaches(&board, &id, events[2]).await;
    let duplicate = receiver.next().await;
    worker.abort();
    let _ = worker.await;
    receiver.stop().await;
    assert_eq!(
        first.map(|event| event["id"].clone()),
        Some(json!(events[0]))
    );
    assert!(
        first_acked,
        "already-in-flight success must advance the paused cursor"
    );
    assert!(
        while_paused.is_none(),
        "paused webhook must not initiate requests 2/3: {while_paused:?}"
    );
    assert_eq!(paused_cursor, events[0]);
    assert_eq!(
        second.map(|event| event["id"].clone()),
        Some(json!(events[1]))
    );
    assert_eq!(
        third.map(|event| event["id"].clone()),
        Some(json!(events[2]))
    );
    assert!(all_acked, "resume must acknowledge the remaining events");
    assert!(
        duplicate.is_none(),
        "resume must not duplicate acknowledged events"
    );
}

#[tokio::test]
async fn dispatcher_regression_wakes_keep_one_batch_in_flight_and_drain_promptly() {
    let board = Board::new();
    let db = Arc::new(Mutex::new(board.open()));
    let hub = Hub::new();
    let mut receiver = GatedReceiver::new().await;
    let id = gated_subscription(&db, &receiver);
    let mut events = queued_issues(&db, 1);
    let worker = tokio::spawn(webhooks::run(db.clone(), hub.subscribe()));
    let first = receiver.next().await;
    events.extend(queued_issues(&db, 30));
    for _ in 0..10 {
        hub.notify();
        tokio::task::yield_now().await;
    }
    let overlapping =
        tokio::time::timeout(Duration::from_millis(200), receiver.requests.recv()).await;
    receiver.permits.add_permits(events.len());
    let remaining = tokio::time::timeout(Duration::from_millis(700), async {
        let mut received = Vec::new();
        for _ in 1..events.len() {
            received.push(
                receiver.requests.recv().await.unwrap()["id"]
                    .as_i64()
                    .unwrap(),
            );
        }
        received
    })
    .await;
    let all_acked = cursor_reaches(&board, &id, *events.last().unwrap()).await;
    worker.abort();
    let _ = worker.await;
    receiver.stop().await;
    assert_eq!(
        first.map(|event| event["id"].clone()),
        Some(json!(events[0]))
    );
    assert!(
        overlapping.is_err(),
        "wakes must not start overlapping batches"
    );
    assert_eq!(
        remaining.ok(),
        Some(events[1..].to_vec()),
        "pending batches must drain without waiting for the one-second idle poll"
    );
    assert!(all_acked);
}

#[tokio::test]
async fn dispatcher_regression_wakes_preserve_independent_retry_backoff() {
    let board = Board::new();
    let db = Arc::new(Mutex::new(board.open()));
    let hub = Hub::new();
    let mut failing = GatedReceiver::new().await;
    let mut healthy = GatedReceiver::new().await;
    let failing_id = gated_subscription(&db, &failing);
    let healthy_id = gated_subscription(&db, &healthy);
    let first_id = queued_issues(&db, 1)[0];
    failing.status.store(500, Ordering::SeqCst);
    let worker = tokio::spawn(webhooks::run(db.clone(), hub.subscribe()));
    let failed_first = failing.next().await;
    let healthy_first = healthy.next().await;
    failing.permits.add_permits(1);
    healthy.permits.add_permits(1);
    let recorded_failure = tokio::time::timeout(Duration::from_secs(3), async {
        while webhook_state(&board, &failing_id)["failures"] != 1 {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .is_ok();
    let healthy_acked = cursor_reaches(&board, &healthy_id, first_id).await;
    let second_id = queued_issues(&db, 1)[0];
    hub.notify();
    let healthy_second =
        tokio::time::timeout(Duration::from_millis(700), healthy.requests.recv()).await;
    let early_retry =
        tokio::time::timeout(Duration::from_millis(200), failing.requests.recv()).await;
    let retry = failing.next().await;
    failing.status.store(200, Ordering::SeqCst);
    failing.permits.add_permits(2);
    healthy.permits.add_permits(1);
    let failing_second = failing.next().await;
    let recovered = cursor_reaches(&board, &failing_id, second_id).await;
    worker.abort();
    let _ = worker.await;
    failing.stop().await;
    healthy.stop().await;
    assert_eq!(
        failed_first.map(|event| event["id"].clone()),
        Some(json!(first_id))
    );
    assert_eq!(
        healthy_first.map(|event| event["id"].clone()),
        Some(json!(first_id))
    );
    assert!(recorded_failure && healthy_acked);
    assert_eq!(
        healthy_second
            .ok()
            .flatten()
            .map(|event| event["id"].clone()),
        Some(json!(second_id)),
        "healthy wake must not wait for another subscription's backoff"
    );
    assert!(early_retry.is_err(), "wakes must not bypass retry backoff");
    assert_eq!(
        retry.map(|event| event["id"].clone()),
        Some(json!(first_id))
    );
    assert_eq!(
        failing_second.map(|event| event["id"].clone()),
        Some(json!(second_id))
    );
    assert!(recovered, "retry must recover in order and clear failures");
    assert_eq!(webhook_state(&board, &failing_id)["failures"], 0);
}

#[tokio::test]
async fn dispatcher_regression_failure_recording_error_preserves_retry_backoff() {
    let board = Board::new();
    let db = Arc::new(Mutex::new(board.open()));
    let hub = Hub::new();
    let mut receiver = GatedReceiver::new().await;
    let id = gated_subscription(&db, &receiver);
    let event = queued_issues(&db, 1)[0];
    // Reads still work, but recording a delivery failure must roll back.
    board
        .sql()
        .execute_batch(
            "CREATE TRIGGER reject_failure_recording BEFORE UPDATE ON webhooks
         WHEN json_extract(NEW.doc,'$.failures') > json_extract(OLD.doc,'$.failures')
         BEGIN SELECT RAISE(ABORT,'injected failure recording error'); END;",
        )
        .unwrap();
    receiver.status.store(500, Ordering::SeqCst);
    let worker = tokio::spawn(webhooks::run(db.clone(), hub.subscribe()));
    let first = receiver.next().await;
    receiver.permits.add_permits(1);
    let early_retry =
        tokio::time::timeout(Duration::from_millis(400), receiver.requests.recv()).await;
    let delayed_retry = if early_retry.is_err() {
        receiver.next().await
    } else {
        None
    };
    let stored_failures = webhook_state(&board, &id)["failures"].clone();
    worker.abort();
    let _ = worker.await;
    receiver.stop().await;
    assert_eq!(first.map(|event| event["id"].clone()), Some(json!(event)));
    assert_eq!(
        stored_failures, 0,
        "injected failure must not persist a counter"
    );
    assert!(
        early_retry.is_err(),
        "failed persistence must not bypass retry backoff"
    );
    assert_eq!(
        delayed_retry.map(|event| event["id"].clone()),
        Some(json!(event)),
        "failure to persist must still allow the delayed retry"
    );
}

#[test]
fn webhook_administration_validates_input_and_never_reveals_secrets() {
    let b = Board::new();
    let mut s = b.open();
    for (bad, expect_invalid) in [
        (json!({"url":"ftp://example.com/x","label":"l"}), true),
        (json!({"url":"not a url","label":"l"}), true),
        (json!({"url":"http://","label":"l"}), true),
        (json!({"url":"https://example.com/x","label":" "}), true),
        (
            json!({"url":"https://example.com/x","label":"l","actions":[""]}),
            true,
        ),
        (
            json!({"url":"https://example.com/x","label":"l","project":"MISSING"}),
            false,
        ),
    ] {
        let mut command = bad.clone();
        command["op"] = json!("webhook_create");
        let error = s.execute("admin", cmd(command)).unwrap_err();
        assert_eq!(
            matches!(error, Error::Invalid(_)),
            expect_invalid,
            "{bad}: {error}"
        );
    }
    let made = run(
        &mut s,
        "admin",
        json!({"op":"webhook_create","url":"https://hooks.example.com/services/SECRETPATH?token=abc","label":"chat"}),
    );
    let secret = made["secret"].as_str().unwrap();
    assert!(secret.starts_with("whsec_") && secret.len() > 40);
    assert_eq!(made["webhook"]["url"], "https://hooks.example.com/…");
    let listed = run(&mut s, "admin", json!({"op":"webhook_list"})).to_string();
    let log = run(&mut s, "admin", json!({"op":"events","limit":1000})).to_string();
    for text in [&listed, &log] {
        assert!(!text.contains(secret), "secret leaked");
        assert!(
            !text.contains("SECRETPATH") && !text.contains("token=abc"),
            "URL leaked"
        );
    }
    assert_eq!(actions(&mut s, "webhook_created"), 1);
    // The secret is the only thing returned once; the database keeps it to sign with.
    let stored: String = b
        .sql()
        .query_row("SELECT secret FROM webhooks", [], |r| r.get(0))
        .unwrap();
    assert_eq!(stored, secret);

    let id = made["webhook"]["id"].as_str().unwrap();
    let paused = run(
        &mut s,
        "admin",
        json!({"op":"webhook_set_active","id":id,"active":false}),
    );
    assert_eq!(paused["active"], false);
    run(&mut s, "admin", json!({"op":"webhook_delete","id":id}));
    assert_eq!(
        run(&mut s, "admin", json!({"op":"webhook_list"})),
        json!([])
    );
    assert!(matches!(
        s.execute("admin", cmd(json!({"op":"webhook_delete","id":id}))),
        Err(Error::NotFound(_))
    ));
}

#[tokio::test]
async fn webhooks_are_operator_only_over_http_and_keyed_mcp() {
    let b = Board::new();
    let app = web::router(b.open());
    for body in [
        json!({"op":"webhook_list"}),
        json!({"op":"webhook_create","url":"https://example.com/x","label":"l"}),
        json!({"op":"webhook_delete","id":"x"}),
        json!({"op":"webhook_set_active","id":"x","active":true}),
    ] {
        assert_eq!(
            http(&app, "POST", "/api/commands", &b.key, &[], body)
                .await
                .0,
            StatusCode::FORBIDDEN
        );
    }
    let mut server = mcp::Server::new(b.open(), "worker".into()).with_api_key(Some(b.key.clone()));
    server.handle(json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-11-25","capabilities":{},"clientInfo":{"name":"t","version":"1"}}}));
    let reply = server
        .handle(json!({"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"workboard_execute","arguments":{"op":"webhook_list"}}}))
        .unwrap();
    assert_eq!(reply["error"]["code"], -32001);
}

#[tokio::test]
async fn webhooks_deliver_new_events_in_order_signed_and_without_lease_tokens() {
    let b = Board::new();
    let (url, sink) = receiver(200).await;
    let (db, hub) = delivery(&b);
    let made = db
        .lock()
        .unwrap()
        .execute(
            "admin",
            cmd(json!({"op":"webhook_create","url":url,"label":"ci","project":"PUSH"})),
        )
        .unwrap();
    let (id, secret) = (
        made["webhook"]["id"].as_str().unwrap().to_owned(),
        made["secret"].as_str().unwrap().to_owned(),
    );
    {
        let mut s = db.lock().unwrap();
        run(
            &mut s,
            "admin",
            json!({"op":"issue_create","project":"PUSH","kind":"task","title":"ship"}),
        );
        run(
            &mut s,
            "admin",
            json!({"op":"issue_transition","key":"PUSH-1","expected_revision":1,"state":"ready"}),
        );
        run(
            &mut s,
            "worker",
            json!({"op":"issue_claim","project":"PUSH"}),
        );
    }
    hub.notify();
    until("three deliveries", 5, || sink.count() >= 3).await;
    tokio::time::sleep(Duration::from_millis(1500)).await;

    let bodies = sink.bodies();
    let names: Vec<_> = bodies
        .iter()
        .map(|e| e["action"].as_str().unwrap())
        .collect();
    assert_eq!(
        names,
        ["issue_created", "issue_transitioned", "issue_claimed"],
        "history before creation must not replay"
    );
    let ids: Vec<i64> = bodies.iter().map(|e| e["id"].as_i64().unwrap()).collect();
    assert!(ids.windows(2).all(|w| w[0] < w[1]), "out of order: {ids:?}");

    let token = {
        let list = run(
            &mut db.lock().unwrap(),
            "admin",
            json!({"op":"events","limit":1000}),
        );
        list.as_array()
            .unwrap()
            .iter()
            .find_map(|e| e["data"]["lease"]["token"].as_str().map(str::to_owned))
            .unwrap()
    };
    for (headers, body) in sink.received.lock().unwrap().iter() {
        assert!(
            !body.contains(&token),
            "lease token reached an external endpoint"
        );
        assert_eq!(headers["x-workboard-webhook"], id.as_str());
        let timestamp: i64 = headers["x-workboard-timestamp"]
            .to_str()
            .unwrap()
            .parse()
            .unwrap();
        assert_eq!(
            headers["x-workboard-signature"].to_str().unwrap(),
            webhooks::sign(&secret, timestamp, body)
        );
        let event: Value = serde_json::from_str(body).unwrap();
        assert_eq!(
            headers["x-workboard-event-id"].to_str().unwrap(),
            event["id"].to_string()
        );
        assert_eq!(
            headers["x-workboard-event"],
            event["action"].as_str().unwrap()
        );
    }
    let state = webhook_state(&b, &id);
    assert_eq!(state["cursor"], *ids.last().unwrap());
    assert_eq!(
        (state["failures"].as_i64(), state["last_error"].is_null()),
        (Some(0), true)
    );
    assert!(state["last_delivered_at"].is_number());
}

#[tokio::test]
async fn action_filters_skip_unwanted_events_without_stalling_the_cursor() {
    let b = Board::new();
    let (url, sink) = receiver(200).await;
    let (db, hub) = delivery(&b);
    let made = db.lock().unwrap().execute("admin", cmd(json!({"op":"webhook_create","url":url,"label":"only-creates","actions":["issue_created"]}))).unwrap();
    let id = made["webhook"]["id"].as_str().unwrap().to_owned();
    let newest = {
        let mut s = db.lock().unwrap();
        run(
            &mut s,
            "admin",
            json!({"op":"issue_create","project":"PUSH","kind":"task","title":"a"}),
        );
        run(
            &mut s,
            "admin",
            json!({"op":"comment_add","key":"PUSH-1","body":"noise"}),
        );
        run(
            &mut s,
            "admin",
            json!({"op":"issue_transition","key":"PUSH-1","expected_revision":1,"state":"ready"}),
        );
        latest_event(&mut s)
    };
    hub.notify();
    until("filtered delivery", 5, || {
        sink.count() >= 1 && webhook_state(&b, &id)["cursor"] == newest
    })
    .await;
    assert_eq!(
        sink.bodies()
            .iter()
            .map(|e| e["action"].as_str().unwrap().to_owned())
            .collect::<Vec<_>>(),
        ["issue_created"]
    );
}

#[tokio::test]
async fn a_failing_endpoint_is_retried_with_the_same_event_and_recovers() {
    let b = Board::new();
    let (url, sink) = receiver(500).await;
    let (db, hub) = delivery(&b);
    let made = db
        .lock()
        .unwrap()
        .execute(
            "admin",
            cmd(json!({"op":"webhook_create","url":url,"label":"flaky"})),
        )
        .unwrap();
    let id = made["webhook"]["id"].as_str().unwrap().to_owned();
    run(
        &mut db.lock().unwrap(),
        "admin",
        json!({"op":"issue_create","project":"PUSH","kind":"task","title":"retry"}),
    );
    hub.notify();
    until("a recorded failure", 5, || {
        webhook_state(&b, &id)["failures"].as_i64() >= Some(1)
    })
    .await;
    let failed = webhook_state(&b, &id);
    let reason = failed["last_error"].as_str().unwrap();
    assert_eq!(reason, "endpoint returned HTTP 500");
    assert!(!reason.contains("127.0.0.1") && !reason.contains("SECRETPATH"));
    assert_eq!(failed["active"], true);

    sink.status.store(200, Ordering::SeqCst);
    until("redelivery after backoff", 10, || {
        webhook_state(&b, &id)["failures"] == 0 && sink.count() >= 2
    })
    .await;
    let bodies = sink.bodies();
    let (first, last) = (&bodies[0], bodies.last().unwrap());
    assert_eq!(
        first["id"], last["id"],
        "the same event is retried, not skipped"
    );
    assert!(webhook_state(&b, &id)["last_error"].is_null());
}

#[test]
fn repeated_failures_pause_a_webhook_until_an_operator_resumes_it() {
    let b = Board::new();
    let mut s = b.open();
    let made = run(
        &mut s,
        "admin",
        json!({"op":"webhook_create","url":"http://127.0.0.1:9/x","label":"dead"}),
    );
    let id = made["webhook"]["id"].as_str().unwrap();
    for attempt in 1..webhooks::MAX_FAILURES {
        s.webhook_fail(
            id,
            "connection error: ConnectionRefused",
            webhooks::MAX_FAILURES,
        )
        .unwrap();
        assert_eq!(
            webhook_state(&b, id)["active"],
            true,
            "paused too early at {attempt}"
        );
    }
    assert_eq!(actions(&mut s, "webhook_disabled"), 0);
    s.webhook_fail(
        id,
        "connection error: ConnectionRefused",
        webhooks::MAX_FAILURES,
    )
    .unwrap();
    let paused = webhook_state(&b, id);
    assert_eq!(
        (paused["active"].as_bool(), paused["failures"].as_u64()),
        (Some(false), Some(webhooks::MAX_FAILURES as u64))
    );
    assert_eq!(actions(&mut s, "webhook_disabled"), 1);
    assert!(
        s.webhook_targets().unwrap().is_empty(),
        "paused webhooks must not be delivered"
    );

    let resumed = run(
        &mut s,
        "admin",
        json!({"op":"webhook_set_active","id":id,"active":true}),
    );
    assert_eq!(
        (resumed["active"].as_bool(), resumed["failures"].as_u64()),
        (Some(true), Some(0))
    );
    assert!(resumed["last_error"].is_null());
    assert_eq!(s.webhook_targets().unwrap().len(), 1);
}
