use agent_workboard::{Command, Store, web};
use axum::{
    Router,
    body::{Body, to_bytes},
    http::{Request, StatusCode},
};
use serde_json::{Value, json};
use tempfile::TempDir;
use tower::ServiceExt;

fn execute(store: &mut Store, actor: &str, value: Value) -> Value {
    store
        .execute(actor, serde_json::from_value::<Command>(value).unwrap())
        .unwrap()
}
struct Fixture {
    dir: TempDir,
    app: Router,
    key: String,
    id: String,
}
impl Fixture {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let mut store = Store::open(dir.path().join("web.sqlite3")).unwrap();
        execute(
            &mut store,
            "admin",
            json!({"op":"agent_register","id":"admin","name":"Operator"}),
        );
        execute(
            &mut store,
            "admin",
            json!({"op":"agent_register","id":"worker","name":"Worker"}),
        );
        execute(
            &mut store,
            "admin",
            json!({"op":"project_create","key":"WEB","name":"Web project"}),
        );
        let issued = execute(
            &mut store,
            "admin",
            json!({"op":"api_key_create","agent":"worker","label":"Integration test"}),
        );
        Self {
            dir,
            app: web::router(store),
            key: issued["api_key"].as_str().unwrap().into(),
            id: issued["key"]["id"].as_str().unwrap().into(),
        }
    }
    fn store(&self) -> Store {
        Store::open(self.dir.path().join("web.sqlite3")).unwrap()
    }
    async fn request(
        &self,
        method: &str,
        path: &str,
        key: Option<&str>,
        body: Option<Value>,
    ) -> (StatusCode, Value) {
        let mut request = Request::builder().method(method).uri(path);
        if let Some(key) = key {
            request = request.header("Authorization", format!("Bearer {key}"));
        }
        let data = body.map(|v| v.to_string()).unwrap_or_default();
        let response = self
            .app
            .clone()
            .oneshot(
                request
                    .header("Content-Type", "application/json")
                    .body(Body::from(data))
                    .unwrap(),
            )
            .await
            .unwrap();
        let status = response.status();
        assert_eq!(response.headers()["cache-control"], "no-store");
        assert!(
            response.headers()["content-security-policy"]
                .to_str()
                .unwrap()
                .contains("script-src 'self'")
        );
        let bytes = to_bytes(response.into_body(), 16 * 1024 * 1024)
            .await
            .unwrap();
        (
            status,
            serde_json::from_slice(&bytes)
                .unwrap_or(json!({"text":String::from_utf8_lossy(&bytes)})),
        )
    }
    async fn command(&self, value: Value) -> Value {
        let (status, body) = self
            .request("POST", "/api/commands", Some(&self.key), Some(value))
            .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        body
    }
}

#[tokio::test]
async fn authentication_actor_binding_claims_and_observer_redaction() {
    let f = Fixture::new();
    for path in [
        "/api/overview",
        "/api/schema",
        "/api/session",
        "/api/activity",
        "/api/issues/WEB-1",
        "/api/issues/WEB-1/comments",
        "/api/issues/WEB-1/events",
    ] {
        assert_eq!(
            f.request("GET", path, None, None).await.0,
            StatusCode::UNAUTHORIZED
        );
        assert_eq!(
            f.request("GET", path, Some("invalid"), None).await.0,
            StatusCode::UNAUTHORIZED
        );
    }
    assert_eq!(
        f.request(
            "POST",
            "/api/commands",
            None,
            Some(json!({"op":"agent_list"}))
        )
        .await
        .0,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        f.request(
            "GET",
            &format!("/api/overview?api_key={}", f.key),
            None,
            None
        )
        .await
        .0,
        StatusCode::UNAUTHORIZED
    );
    let (_, session) = f.request("GET", "/api/session", Some(&f.key), None).await;
    assert_eq!(session["agent"]["id"], "worker");
    let issue = f
        .command(json!({"op":"issue_create","project":"WEB","kind":"task","title":"HTTP work"}))
        .await;
    f.command(
        json!({"op":"issue_transition","key":issue["key"],"expected_revision":1,"state":"ready"}),
    )
    .await;
    let claimed = f.command(json!({"op":"issue_claim","project":"WEB"})).await["claimed"].clone();
    assert_eq!(claimed["assignee"], "worker");
    let token = claimed["lease"]["token"].as_str().unwrap();
    let before = execute(&mut f.store(), "admin", json!({"op":"events"}));
    for path in [
        "/api/overview?project=WEB",
        "/api/activity?project=WEB",
        "/api/issues/WEB-1",
        "/api/issues/WEB-1/events",
    ] {
        let (status, body) = f.request("GET", path, Some(&f.key), None).await;
        assert_eq!(status, StatusCode::OK);
        assert!(
            !body.to_string().contains(token),
            "lease secret leaked at {path}"
        );
        assert!(
            !body.to_string().contains(&f.key),
            "API key leaked at {path}"
        );
    }
    assert_eq!(
        before,
        execute(&mut f.store(), "admin", json!({"op":"events"})),
        "observation emitted events"
    );
    let events = f.command(json!({"op":"events","key":"WEB-1"})).await;
    assert!(
        events
            .as_array()
            .unwrap()
            .iter()
            .all(|e| e["actor"] == "worker")
    );
    assert_eq!(f.request("POST","/api/commands",Some(&f.key),Some(json!({"op":"issue_create","actor":"admin","project":"WEB","kind":"task","title":"spoof"}))).await.0,StatusCode::UNPROCESSABLE_ENTITY);
    f.command(json!({"op":"issue_heartbeat","key":"WEB-1","lease_token":token}))
        .await;
    assert_eq!(f.request("POST","/api/commands",Some(&f.key),Some(json!({"op":"issue_transition","key":"WEB-1","expected_revision":3,"state":"done","lease_token":token}))).await.0,StatusCode::CONFLICT);
}

#[tokio::test]
async fn keys_are_hashed_individually_revocable_and_not_remotely_administrable() {
    let f = Fixture::new();
    let mut store = f.store();
    let other = execute(
        &mut store,
        "admin",
        json!({"op":"api_key_create","agent":"worker","label":"Second key"}),
    );
    let key2 = other["api_key"].as_str().unwrap();
    let list = execute(
        &mut store,
        "admin",
        json!({"op":"api_key_list","agent":"worker"}),
    );
    assert_eq!(list.as_array().unwrap().len(), 2);
    assert!(!list.to_string().contains(&f.key));
    assert!(!list.to_string().contains("key_hash"));
    let conn = rusqlite::Connection::open(f.dir.path().join("web.sqlite3")).unwrap();
    let hash: String = conn
        .query_row("SELECT key_hash FROM api_keys WHERE id=?1", [&f.id], |r| {
            r.get(0)
        })
        .unwrap();
    assert_ne!(hash, f.key);
    assert_eq!(hash.len(), 64);
    assert!(
        !execute(&mut store, "admin", json!({"op":"events"}))
            .to_string()
            .contains(&f.key)
    );
    for c in [
        json!({"op":"api_key_create","agent":"admin","label":"unauthorized"}),
        json!({"op":"api_key_list"}),
        json!({"op":"api_key_revoke","id":f.id}),
    ] {
        assert_eq!(
            f.request("POST", "/api/commands", Some(&f.key), Some(c))
                .await
                .0,
            StatusCode::FORBIDDEN
        );
    }
    execute(
        &mut store,
        "admin",
        json!({"op":"api_key_revoke","id":f.id}),
    );
    assert_eq!(
        f.request("GET", "/api/session", Some(&f.key), None).await.0,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        f.request("GET", "/api/session", Some(key2), None).await.0,
        StatusCode::OK
    );
    execute(
        &mut store,
        "admin",
        json!({"op":"agent_set_active","id":"worker","active":false}),
    );
    assert_eq!(
        f.request("GET", "/api/session", Some(key2), None).await.0,
        StatusCode::UNAUTHORIZED
    );
    execute(
        &mut store,
        "admin",
        json!({"op":"agent_set_active","id":"worker","active":true}),
    );
    assert_eq!(
        f.request("GET", "/api/session", Some(key2), None).await.0,
        StatusCode::OK
    );
}

#[tokio::test]
async fn embedded_assets_empty_states_project_isolation_and_cursor_history() {
    let f = Fixture::new();
    for path in ["/", "/app.js", "/styles.css"] {
        assert_eq!(f.request("GET", path, None, None).await.0, StatusCode::OK);
    }
    let (_, overview) = f.request("GET", "/api/overview", Some(&f.key), None).await;
    assert_eq!(overview["issues"], json!([]));
    f.command(json!({"op":"issue_create","project":"WEB","kind":"bug","title":"History"}))
        .await;
    f.command(json!({"op":"project_create","key":"OTHER","name":"Other"}))
        .await;
    f.command(json!({"op":"issue_create","project":"OTHER","kind":"task","title":"Hidden in WEB"}))
        .await;
    for n in 0..205 {
        execute(
            &mut f.store(),
            "admin",
            json!({"op":"comment_add","key":"WEB-1","body":format!("Comment {n}")}),
        );
    }
    let (_, overview) = f
        .request("GET", "/api/overview?project=WEB", Some(&f.key), None)
        .await;
    assert_eq!(overview["issues"].as_array().unwrap().len(), 1);
    assert!(
        overview["activity"]["items"]
            .as_array()
            .unwrap()
            .iter()
            .all(|e| e["issue"] != "OTHER-1")
    );
    let (_, detail) = f
        .request("GET", "/api/issues/WEB-1", Some(&f.key), None)
        .await;
    assert_eq!(detail["comments"]["items"][0]["body"], "Comment 204");
    for endpoint in ["comments", "events"] {
        let mut before = None;
        let mut ids = vec![];
        loop {
            let path = format!(
                "/api/issues/WEB-1/{endpoint}{}",
                before.map(|n| format!("?before={n}")).unwrap_or_default()
            );
            let (_, page) = f.request("GET", &path, Some(&f.key), None).await;
            ids.extend(
                page["items"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|e| e["id"].as_i64().unwrap()),
            );
            before = page["next_before"].as_i64();
            if before.is_none() {
                break;
            }
        }
        assert!(ids.len() >= 205);
        assert!(ids.windows(2).all(|w| w[0] > w[1]));
    }
    assert_eq!(
        f.request("GET", "/api/overview?project=MISSING", Some(&f.key), None)
            .await
            .0,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        f.request("GET", "/api/issues/WEB-404", Some(&f.key), None)
            .await
            .0,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        f.request("POST", "/api/overview", Some(&f.key), None)
            .await
            .0,
        StatusCode::METHOD_NOT_ALLOWED
    );
}

#[tokio::test]
async fn concurrent_authenticated_workers_claim_once() {
    let f = Fixture::new();
    let mut store = f.store();
    let second = execute(
        &mut store,
        "admin",
        json!({"op":"api_key_create","agent":"admin","label":"Second worker"}),
    );
    f.command(json!({"op":"issue_create","project":"WEB","kind":"task","title":"One claim"}))
        .await;
    f.command(json!({"op":"issue_transition","key":"WEB-1","expected_revision":1,"state":"ready"}))
        .await;
    let call = json!({"op":"issue_claim","project":"WEB"});
    let (a, b) = tokio::join!(
        f.request("POST", "/api/commands", Some(&f.key), Some(call.clone())),
        f.request(
            "POST",
            "/api/commands",
            second["api_key"].as_str(),
            Some(call)
        )
    );
    assert_eq!(a.0, StatusCode::OK);
    assert_eq!(b.0, StatusCode::OK);
    assert_eq!(
        [a.1, b.1]
            .iter()
            .filter(|v| !v["claimed"].is_null())
            .count(),
        1
    );
}

#[test]
fn version_one_database_is_migrated_without_losing_work() {
    let f = Fixture::new();
    let mut store = f.store();
    execute(
        &mut store,
        "admin",
        json!({"op":"issue_create","project":"WEB","kind":"story","title":"Preserved"}),
    );
    drop(store);
    let conn = rusqlite::Connection::open(f.dir.path().join("web.sqlite3")).unwrap();
    conn.execute_batch("DROP TABLE api_keys; PRAGMA user_version=1;")
        .unwrap();
    drop(conn);
    let mut migrated = f.store();
    assert_eq!(
        execute(
            &mut migrated,
            "admin",
            json!({"op":"issue_get","key":"WEB-1"})
        )["issue"]["title"],
        "Preserved"
    );
    let key = execute(
        &mut migrated,
        "admin",
        json!({"op":"api_key_create","agent":"worker","label":"Migrated"}),
    );
    assert_eq!(
        migrated
            .authenticate(key["api_key"].as_str().unwrap())
            .unwrap()
            .unwrap()
            .id,
        "worker"
    );
}

#[test]
fn keyed_mcp_derives_identity_and_rechecks_revocation_for_each_request() {
    let f = Fixture::new();
    let mut server = agent_workboard::mcp::Server::new(f.store(), "admin".into())
        .with_api_key(Some(f.key.clone()));
    let initialized = server.handle(json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-11-25","capabilities":{},"clientInfo":{"name":"test","version":"1"}}})).unwrap();
    assert!(initialized.get("result").is_some());
    let created = server.handle(json!({"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"workboard_execute","arguments":{"op":"issue_create","project":"WEB","kind":"task","title":"Keyed MCP"}}})).unwrap();
    assert_eq!(created["result"]["isError"], false);
    let events = execute(
        &mut f.store(),
        "admin",
        json!({"op":"events","key":"WEB-1"}),
    );
    assert_eq!(events[0]["actor"], "worker");
    let forbidden = server.handle(json!({"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"workboard_execute","arguments":{"op":"api_key_create","agent":"admin","label":"forbidden"}}})).unwrap();
    assert_eq!(forbidden["error"]["code"], -32001);
    execute(
        &mut f.store(),
        "admin",
        json!({"op":"api_key_revoke","id":f.id}),
    );
    let denied = server.handle(json!({"jsonrpc":"2.0","id":4,"method":"resources/read","params":{"uri":"workboard://projects"}})).unwrap();
    assert_eq!(denied["error"]["code"], -32001);
}

#[test]
fn keyed_cli_uses_agent_identity_and_rejects_invalid_keys() {
    let f = Fixture::new();
    let run = |key: &str, command: Value| {
        std::process::Command::new(env!("CARGO_BIN_EXE_workboard"))
            .args([
                "--db",
                f.dir.path().join("web.sqlite3").to_str().unwrap(),
                "--actor",
                "admin",
                "run",
            ])
            .env("WORKBOARD_API_KEY", key)
            .arg(command.to_string())
            .output()
            .unwrap()
    };
    let output = run(
        &f.key,
        json!({"op":"issue_create","project":"WEB","kind":"task","title":"Keyed CLI"}),
    );
    assert!(output.status.success());
    let events = execute(
        &mut f.store(),
        "admin",
        json!({"op":"events","key":"WEB-1"}),
    );
    assert_eq!(events[0]["actor"], "worker");
    let forbidden = run(&f.key, json!({"op":"api_key_list"}));
    assert!(!forbidden.status.success());
    assert_eq!(
        serde_json::from_slice::<Value>(&forbidden.stdout).unwrap()["error"]["code"],
        "forbidden"
    );
    let denied = run("invalid", json!({"op":"project_list"}));
    assert!(!denied.status.success());
    assert_eq!(
        serde_json::from_slice::<Value>(&denied.stdout).unwrap()["error"]["code"],
        "unauthorized"
    );
}
