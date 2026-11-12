//! Read-path scale check. Run explicitly, in release mode, to see timings:
//!
//!     cargo test --release --test scale -- --ignored --nocapture
//!
//! Issues are inserted as raw documents because creating tens of thousands through
//! the command API would measure the write-time cycle check instead.
use agent_workboard::{Command, Store};
use serde_json::{Value, json};
use std::time::Instant;

const PROJECTS: usize = 20;

/// Project keys are uppercase letters only: PA, PB, ...
fn pk(p: usize) -> String {
    format!("P{}", (b'A' + p as u8) as char)
}
const PER_PROJECT: usize = 1000;

fn cmd(value: Value) -> Command {
    serde_json::from_value(value).unwrap()
}
fn timed<T>(label: &str, f: impl FnOnce() -> T) -> T {
    let start = Instant::now();
    let out = f();
    println!(
        "{label:<44}{:>9.1} ms",
        start.elapsed().as_secs_f64() * 1000.0
    );
    out
}
fn build(path: &std::path::Path) -> Store {
    let mut store = Store::open(path).unwrap();
    store
        .execute("a", cmd(json!({"op":"agent_register","id":"a","name":"A"})))
        .unwrap();
    for p in 0..PROJECTS {
        store
            .execute(
                "a",
                cmd(json!({"op":"project_create","key":pk(p),"name":format!("Project {p}")})),
            )
            .unwrap();
    }
    let conn = rusqlite::Connection::open(path).unwrap();
    conn.execute_batch("BEGIN").unwrap();
    let states = [
        "backlog",
        "ready",
        "in_progress",
        "blocked",
        "in_review",
        "done",
    ];
    let priorities = ["critical", "high", "medium", "low"];
    let words = [
        "scheduler",
        "lease",
        "webhook",
        "dashboard",
        "retry",
        "migration",
        "index",
        "board",
    ];
    for p in 0..PROJECTS {
        for n in 1..=PER_PROJECT {
            let key = format!("{}-{n}", pk(p));
            let doc = json!({
                "key": key, "project": pk(p), "kind": "task",
                "title": format!("{} {} issue {n}", words[n % 8], words[(n / 8) % 8]),
                "description": format!("Work item {n} touching the {} subsystem in detail.", words[(n * 3) % 8]),
                "acceptance_criteria": ["It works"], "state": states[n % 6],
                "priority": priorities[n % 4], "assignee": null, "parent": null,
                "sprint": null, "labels": [words[n % 8]], "estimate": null,
                "metadata": {}, "revision": 1, "created_at": 1_700_000_000 + n as i64, "updated_at": 1_700_000_000 + n as i64,
                "lease": null
            });
            conn.execute(
                "INSERT INTO issues(id,doc) VALUES(?1,?2)",
                rusqlite::params![key, doc.to_string()],
            )
            .unwrap();
        }
        conn.execute(
            "INSERT INTO counters(project,value) VALUES(?1,?2)",
            rusqlite::params![pk(p), PER_PROJECT as i64],
        )
        .unwrap();
    }
    // A blocking link per 10th issue, so that actionable-checks have work to do.
    for p in 0..PROJECTS {
        for n in (10..=PER_PROJECT).step_by(10) {
            conn.execute(
                "INSERT INTO links(source,target,kind) VALUES(?1,?2,'\"blocks\"')",
                rusqlite::params![format!("{}-{}", pk(p), n - 1), format!("{}-{n}", pk(p))],
            )
            .unwrap();
        }
    }
    conn.execute_batch("COMMIT").unwrap();
    drop(conn);
    store
}

#[test]
#[ignore = "timing run; execute explicitly"]
fn read_paths_at_twenty_thousand_issues() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("scale.sqlite3");
    let build_started = Instant::now();
    let mut store = build(&path);
    println!(
        "built {} issues in {:.1}s",
        PROJECTS * PER_PROJECT,
        build_started.elapsed().as_secs_f64()
    );
    fn run(store: &mut Store, label: &str, value: Value) -> Value {
        timed(label, || store.execute("a", cmd(value)).unwrap())
    }
    let board = run(
        &mut store,
        "board (1 project, 1000 issues)",
        json!({"op":"board","project":"PH"}),
    );
    println!("    -> {} issues on the board", board["total"]);
    let list = run(
        &mut store,
        "issue_list project+state, limit 50",
        json!({"op":"issue_list","filter":{"project":"PH","state":"ready","limit":50}}),
    );
    println!("    -> {} total matches", list["total"]);
    let list = run(
        &mut store,
        "issue_list text 'scheduler', limit 50",
        json!({"op":"issue_list","filter":{"project":"PH","text":"scheduler","limit":50}}),
    );
    println!("    -> {} total matches", list["total"]);
    run(
        &mut store,
        "issue_get with children",
        json!({"op":"issue_get","key":"PH-500"}),
    );
    let claim = run(
        &mut store,
        "issue_claim",
        json!({"op":"issue_claim","project":"PH"}),
    );
    println!("    -> claimed {}", claim["claimed"]["key"]);
    timed("observation snapshot (dashboard poll)", || {
        store.observation(Some("PH")).unwrap()
    });
    run(
        &mut store,
        "issue_create (validates the dependency graph)",
        json!({"op":"issue_create","project":"PH","kind":"task","title":"one more"}),
    );
    if std::env::var("SCALE_SEARCH").is_ok() {
        let found = run(
            &mut store,
            "issue_search 'scheduler webhook'",
            json!({"op":"issue_search","query":"scheduler webhook","limit":50}),
        );
        println!("    -> {} total matches", found["total"]);
        run(
            &mut store,
            "issue_search project:PH state:ready lease",
            json!({"op":"issue_search","query":"project:PH state:ready lease","limit":50}),
        );
    }
}
