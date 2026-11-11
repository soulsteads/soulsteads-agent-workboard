use serde_json::{Value, json};
use std::{
    io::Write,
    process::{Command, Stdio},
    sync::{Arc, Barrier},
};
use tempfile::TempDir;

fn cli(dir: &TempDir, actor: &str, command: Value) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_workboard"))
        .arg("--db")
        .arg(dir.path().join("board.sqlite3"))
        .args(["--actor", actor, "run", &command.to_string()])
        .output()
        .unwrap()
}
fn success(dir: &TempDir, actor: &str, command: Value) -> Value {
    let o = cli(dir, actor, command);
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stdout));
    assert!(o.stderr.is_empty());
    let v: Value = serde_json::from_slice(&o.stdout).unwrap();
    assert_eq!(v["ok"], true);
    v["result"].clone()
}
fn setup(dir: &TempDir) {
    success(
        dir,
        "a",
        json!({"op":"agent_register","id":"a","name":"Worker"}),
    );
    success(
        dir,
        "a",
        json!({"op":"project_create","key":"WB","name":"Workboard"}),
    );
}

#[test]
fn cli_stdin_errors_schema_and_persistence() {
    let dir = tempfile::tempdir().unwrap();
    setup(&dir);
    let mut p = Command::new(env!("CARGO_BIN_EXE_workboard"))
        .arg("--db")
        .arg(dir.path().join("board.sqlite3"))
        .args(["--actor", "a", "run"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    p.stdin
        .take()
        .unwrap()
        .write_all(br#"{"op":"issue_create","project":"WB","kind":"bug","title":"Fix crash"}"#)
        .unwrap();
    let out = p.wait_with_output().unwrap();
    assert!(out.status.success());
    assert_eq!(
        success(&dir, "a", json!({"op":"issue_get","key":"WB-1"}))["issue"]["title"],
        "Fix crash"
    );
    let err = cli(&dir, "a", json!({"op":"issue_get","key":"WB-404"}));
    assert!(!err.status.success());
    assert_eq!(
        serde_json::from_slice::<Value>(&err.stdout).unwrap()["error"]["code"],
        "not_found"
    );
    let err = cli(
        &dir,
        "a",
        json!({"op":"issue_create","project":"WB","kind":"bogus","title":"x"}),
    );
    assert!(!err.status.success());
    assert_eq!(
        serde_json::from_slice::<Value>(&err.stdout).unwrap()["error"]["code"],
        "invalid"
    );
    let schema = Command::new(env!("CARGO_BIN_EXE_workboard"))
        .args(["schema"])
        .output()
        .unwrap();
    assert!(schema.status.success());
    let schema: Value = serde_json::from_slice(&schema.stdout).unwrap();
    assert_eq!(schema["type"], "object");
    assert!(schema.to_string().contains("issue_claim"));
}

#[test]
fn eight_competing_processes_claim_exactly_once_and_conflict_on_stale_edits() {
    let dir = Arc::new(tempfile::tempdir().unwrap());
    setup(&dir);
    let issue = success(
        &dir,
        "a",
        json!({"op":"issue_create","project":"WB","kind":"task","title":"Once"}),
    );
    success(
        &dir,
        "a",
        json!({"op":"issue_transition","key":issue["key"],"expected_revision":1,"state":"ready"}),
    );
    let barrier = Arc::new(Barrier::new(8));
    let mut handles = vec![];
    for n in 0..8 {
        let actor = format!("worker-{n}");
        success(
            &dir,
            &actor,
            json!({"op":"agent_register","id":actor,"name":actor}),
        );
        let d = dir.clone();
        let b = barrier.clone();
        handles.push(std::thread::spawn(move || {
            b.wait();
            success(&d, &actor, json!({"op":"issue_claim","project":"WB"}))
        }));
    }
    let claims: Vec<_> = handles
        .into_iter()
        .map(|h| h.join().unwrap())
        .filter(|r| !r["claimed"].is_null())
        .collect();
    assert_eq!(claims.len(), 1);
    let old = success(&dir, "a", json!({"op":"issue_get","key":"WB-1"}))["issue"].clone();
    success(
        &dir,
        "a",
        json!({"op":"issue_assign","key":"WB-1","expected_revision":old["revision"],"assignee":"a"}),
    );
    let conflict = cli(
        &dir,
        "a",
        json!({"op":"issue_update","key":"WB-1","expected_revision":old["revision"],"title":"stale"}),
    );
    assert!(!conflict.status.success());
    assert_eq!(
        serde_json::from_slice::<Value>(&conflict.stdout).unwrap()["error"]["code"],
        "conflict"
    );
}

fn rpc(id: i64, method: &str, params: Value) -> Value {
    json!({"jsonrpc":"2.0","id":id,"method":method,"params":params})
}
#[test]
fn mcp_stdio_handshake_tools_resources_prompts_and_errors() {
    let dir = tempfile::tempdir().unwrap();
    let mut child = Command::new(env!("CARGO_BIN_EXE_workboard"))
        .arg("--db")
        .arg(dir.path().join("board.sqlite3"))
        .args(["--actor", "a", "mcp"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let messages = [
        rpc(0, "tools/list", json!({})),
        rpc(
            1,
            "initialize",
            json!({"protocolVersion":"2025-11-25","capabilities":{},"clientInfo":{"name":"integration-test","version":"1"}}),
        ),
        json!({"jsonrpc":"2.0","method":"notifications/initialized"}),
        rpc(2, "tools/list", json!({})),
        rpc(
            3,
            "tools/call",
            json!({"name":"workboard_execute","arguments":{"op":"agent_register","id":"a","name":"Worker"}}),
        ),
        rpc(
            4,
            "tools/call",
            json!({"name":"workboard_execute","arguments":{"op":"project_create","key":"WB","name":"Board"}}),
        ),
        rpc(
            5,
            "tools/call",
            json!({"name":"workboard_execute","arguments":{"op":"issue_create","project":"WB","kind":"story","title":"MCP story"}}),
        ),
        rpc(6, "resources/list", json!({})),
        rpc(7, "resources/templates/list", json!({})),
        rpc(
            8,
            "resources/read",
            json!({"uri":"workboard://issues/WB-1"}),
        ),
        rpc(
            9,
            "resources/read",
            json!({"uri":"workboard://projects/WB/board"}),
        ),
        rpc(10, "prompts/list", json!({})),
        rpc(
            11,
            "prompts/get",
            json!({"name":"work-issue","arguments":{"project":"WB"}}),
        ),
        rpc(
            12,
            "tools/call",
            json!({"name":"workboard_execute","arguments":{"op":"issue_get","key":"WB-404"}}),
        ),
        rpc(
            13,
            "tools/call",
            json!({"name":"workboard_execute","arguments":{"op":"made_up"}}),
        ),
        rpc(14, "unknown/method", json!({})),
        rpc(15, "tools/call", json!({"name":"unknown","arguments":{}})),
        json!({"jsonrpc":"2.0","method":"notifications/cancelled","params":{"requestId":123,"reason":"unused"}}),
        rpc(16, "ping", json!({})),
    ];
    let mut stdin = child.stdin.take().unwrap();
    stdin.write_all(b"not json\n").unwrap();
    for msg in messages {
        writeln!(stdin, "{msg}").unwrap();
    }
    drop(stdin);
    let out = child.wait_with_output().unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(out.stderr.is_empty());
    let responses: Vec<Value> = String::from_utf8(out.stdout)
        .unwrap()
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    assert_eq!(responses.len(), 18);
    assert_eq!(responses[0]["error"]["code"], -32700);
    let response = |id: i64| responses.iter().find(|r| r["id"] == id).unwrap();
    assert_eq!(response(0)["error"]["code"], -32002);
    assert_eq!(response(1)["result"]["protocolVersion"], "2025-11-25");
    assert_eq!(
        response(2)["result"]["tools"][0]["inputSchema"]["type"],
        "object"
    );
    assert_eq!(
        response(5)["result"]["structuredContent"]["result"]["key"],
        "WB-1"
    );
    let context: Value = serde_json::from_str(
        response(8)["result"]["contents"][0]["text"]
            .as_str()
            .unwrap(),
    )
    .unwrap();
    assert_eq!(context["issue"]["title"], "MCP story");
    assert!(response(9)["result"]["contents"].is_array());
    assert!(
        response(11)["result"]["messages"][0]["content"]["text"]
            .as_str()
            .unwrap()
            .contains("WB")
    );
    assert_eq!(response(12)["result"]["isError"], true);
    assert_eq!(response(13)["result"]["isError"], true);
    assert_eq!(response(14)["error"]["code"], -32601);
    assert_eq!(response(15)["error"]["code"], -32602);
    assert_eq!(response(16)["result"], json!({}));
    assert_eq!(success(&dir, "a", json!({"op":"issue_list"}))["total"], 1);
}

#[test]
fn invalid_rpc_and_oversized_frames_do_not_corrupt_store() {
    use agent_workboard::{
        Store,
        mcp::{Server, serve},
    };
    let mut server = Server::new(Store::open(":memory:").unwrap(), "a".into());
    for value in [
        json!([]),
        json!({"jsonrpc":"1.0","id":1,"method":"ping"}),
        json!({"jsonrpc":"2.0","id":true,"method":"ping"}),
        json!({"jsonrpc":"2.0","id":1,"method":"ping","params":42}),
    ] {
        assert_eq!(server.handle(value).unwrap()["error"]["code"], -32600);
    }
    assert!(
        server
            .handle(json!({"jsonrpc":"2.0","method":"ping"}))
            .is_none()
    );
    let mut out = vec![];
    let frame = vec![b' '; 8 * 1024 * 1024 + 1];
    assert!(serve(std::io::Cursor::new(frame), &mut out, server).is_err());
    assert!(out.is_empty());
}
