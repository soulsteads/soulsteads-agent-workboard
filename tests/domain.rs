use agent_workboard::{Command, Error, Store};
use serde_json::{Value, json};
use tempfile::TempDir;

struct Fixture {
    dir: TempDir,
    store: Store,
}
impl Fixture {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path().join("test.sqlite3")).unwrap();
        let mut f = Self { dir, store };
        f.run(json!({"op":"agent_register","id":"a","name":"Agent A","capabilities":["rust"]}));
        f.run(json!({"op":"agent_register","id":"b","name":"Agent B"}));
        f.run(json!({"op":"project_create","key":"AG","name":"Agents"}));
        f
    }
    fn run(&mut self, c: Value) -> Value {
        self.as_actor("a", c).unwrap()
    }
    fn as_actor(&mut self, a: &str, c: Value) -> Result<Value, Error> {
        self.store
            .execute(a, serde_json::from_value::<Command>(c).unwrap())
    }
    fn err(&mut self, c: Value, code: &str) {
        let e = self.as_actor("a", c).unwrap_err();
        assert_eq!(e.code(), code, "{e}");
    }
    fn create(&mut self, kind: &str, parent: Option<&str>) -> Value {
        self.run(json!({"op":"issue_create","project":"AG","kind":kind,"title":format!("{kind} work"),"parent":parent,"assignee":"a"}))
    }
    fn issue(&mut self, key: &str) -> Value {
        self.run(json!({"op":"issue_get","key":key}))["issue"].clone()
    }
    fn state(&mut self, key: &str, state: &str) -> Value {
        let rev = self.issue(key)["revision"].clone();
        self.run(json!({"op":"issue_transition","key":key,"expected_revision":rev,"state":state}))
    }
}

#[test]
fn full_epic_story_bug_subtask_workflow_and_persistence() {
    let mut f = Fixture::new();
    let epic = f.create("epic", None);
    let story = f.create("story", Some("AG-1"));
    let bug = f.create("bug", Some("AG-1"));
    let sub = f.create("subtask", Some("AG-2"));
    assert_eq!(epic["key"], "AG-1");
    assert_eq!(story["parent"], "AG-1");
    assert_eq!(bug["kind"], "bug");
    assert_eq!(sub["parent"], "AG-2");
    f.run(json!({"op":"issue_update","key":"AG-2","expected_revision":1,"acceptance_criteria":["tests pass"],"metadata":{"repo":"example"},"labels":["rust"],"estimate":3.5,"priority":"high"}));
    let comment =
        f.run(json!({"op":"comment_add","key":"AG-2","body":"Validated with cargo test"}));
    assert_eq!(comment["actor"], "a");
    assert_eq!(
        f.run(json!({"op":"comment_list","key":"AG-2"}))
            .as_array()
            .unwrap()
            .len(),
        1
    );
    for key in ["AG-1", "AG-2", "AG-3", "AG-4"] {
        f.state(key, "ready");
        f.state(key, "in_progress");
    }
    let rev = f.issue("AG-1")["revision"].clone();
    f.err(
        json!({"op":"issue_transition","key":"AG-1","expected_revision":rev,"state":"done"}),
        "conflict",
    );
    for key in ["AG-4", "AG-3", "AG-2", "AG-1"] {
        f.state(key, "in_review");
        f.state(key, "done");
    }
    let p = f.run(json!({"op":"epic_progress","key":"AG-1"}));
    assert_eq!(p["done"], 3);
    assert_eq!(p["open"], 0);
    let board = f.run(json!({"op":"board","project":"AG"}));
    assert_eq!(board["columns"]["done"].as_array().unwrap().len(), 4);
    f.store = Store::open(f.dir.path().join("test.sqlite3")).unwrap();
    assert_eq!(f.issue("AG-2")["acceptance_criteria"][0], "tests pass");
    let events = f.run(json!({"op":"events","key":"AG-2"}));
    let ev = events.as_array().unwrap();
    assert!(ev.iter().any(|e| e["action"] == "comment_added"));
    assert!(
        ev.windows(2)
            .all(|e| e[0]["id"].as_i64() < e[1]["id"].as_i64())
    );
    let rev = f.issue("AG-4")["revision"].clone();
    f.err(
        json!({"op":"issue_transition","key":"AG-4","expected_revision":rev,"state":"backlog"}),
        "conflict",
    );
    f.state("AG-1", "backlog");
    f.state("AG-2", "backlog");
    f.state("AG-4", "backlog");
}

#[test]
fn validation_rolls_back_counters_events_and_rejects_lost_updates() {
    let mut f = Fixture::new();
    let before = f.run(json!({"op":"events"})).as_array().unwrap().len();
    for c in [
        json!({"op":"issue_create","project":"AG","kind":"subtask","title":"invalid"}),
        json!({"op":"issue_create","project":"AG","kind":"task","title":" "}),
        json!({"op":"issue_create","project":"AG","kind":"task","title":"invalid","estimate":-1}),
    ] {
        f.err(c, "invalid");
    }
    assert_eq!(
        f.run(json!({"op":"events"})).as_array().unwrap().len(),
        before
    );
    assert_eq!(f.create("task", None)["key"], "AG-1");
    f.run(json!({"op":"issue_update","key":"AG-1","expected_revision":1,"title":"new title"}));
    f.err(
        json!({"op":"issue_update","key":"AG-1","expected_revision":1,"description":"stale"}),
        "conflict",
    );
    assert_eq!(f.issue("AG-1")["description"], "");
    f.err(
        json!({"op":"issue_transition","key":"AG-1","expected_revision":2,"state":"done"}),
        "conflict",
    );
    f.err(
        json!({"op":"issue_assign","key":"AG-1","expected_revision":2,"assignee":"missing"}),
        "not_found",
    );
    f.err(
        json!({"op":"project_create","key":"ag","name":"bad"}),
        "invalid",
    );
    f.err(json!({"op":"issue_list","filter":{"limit":0}}), "invalid");
    assert!(
        serde_json::from_value::<Command>(json!({"op":"issue_get","key":"AG-1","typo":true}))
            .is_err()
    );
}

#[test]
fn dependencies_cycles_links_and_claim_selection() {
    let mut f = Fixture::new();
    for _ in 0..3 {
        f.create("task", None);
    }
    f.run(json!({"op":"link_add","source":"AG-1","target":"AG-2","kind":"blocks"}));
    f.run(json!({"op":"link_add","source":"AG-2","target":"AG-3","kind":"blocks"}));
    f.err(
        json!({"op":"link_add","source":"AG-3","target":"AG-1","kind":"blocks"}),
        "conflict",
    );
    f.err(
        json!({"op":"link_add","source":"AG-1","target":"AG-1","kind":"duplicates"}),
        "invalid",
    );
    f.run(json!({"op":"link_add","source":"AG-3","target":"AG-1","kind":"relates_to"}));
    f.err(
        json!({"op":"link_add","source":"AG-1","target":"AG-3","kind":"relates_to"}),
        "conflict",
    );
    f.run(json!({"op":"link_remove","source":"AG-3","target":"AG-1","kind":"relates_to"}));
    for key in ["AG-1", "AG-2", "AG-3"] {
        f.state(key, "ready");
    }
    f.err(
        json!({"op":"issue_transition","key":"AG-2","expected_revision":2,"state":"in_progress"}),
        "conflict",
    );
    assert_eq!(
        f.run(json!({"op":"issue_list","filter":{"actionable":true}}))["total"],
        1
    );
    let claim = f.run(json!({"op":"issue_claim","project":"AG"}))["claimed"].clone();
    assert_eq!(claim["key"], "AG-1");
    assert!(f.run(json!({"op":"issue_claim","project":"AG"}))["claimed"].is_null());
    f.run(json!({"op":"issue_transition","key":"AG-1","expected_revision":claim["revision"],"state":"done","lease_token":claim["lease"]["token"]}));
    assert_eq!(
        f.run(json!({"op":"issue_claim","project":"AG"}))["claimed"]["key"],
        "AG-2"
    );
}

#[test]
fn leases_fence_stale_workers_renew_release_and_reassignment() {
    let mut f = Fixture::new();
    f.create("task", None);
    f.state("AG-1", "ready");
    let claim =
        f.run(json!({"op":"issue_claim","project":"AG","lease_seconds":5}))["claimed"].clone();
    let token = claim["lease"]["token"].clone();
    f.err(
        json!({"op":"issue_heartbeat","key":"AG-1","lease_token":"wrong"}),
        "conflict",
    );
    f.err(json!({"op":"issue_update","key":"AG-1","expected_revision":claim["revision"],"title":"no token"}),"conflict");
    assert!(
        f.as_actor(
            "b",
            json!({"op":"issue_heartbeat","key":"AG-1","lease_token":token})
        )
        .is_err()
    );
    let renewed =
        f.run(json!({"op":"issue_heartbeat","key":"AG-1","lease_token":token,"lease_seconds":600}));
    assert!(renewed["lease"]["expires_at"].as_i64() > claim["lease"]["expires_at"].as_i64());
    f.run(json!({"op":"issue_release","key":"AG-1","lease_token":token}));
    assert!(f.issue("AG-1")["assignee"].is_null());
    let claim = f
        .as_actor("b", json!({"op":"issue_claim","project":"AG"}))
        .unwrap()["claimed"]
        .clone();
    f.run(json!({"op":"issue_assign","key":"AG-1","expected_revision":claim["revision"],"assignee":"a"}));
    let rev = f.issue("AG-1")["revision"].clone();
    assert!(f.as_actor("b",json!({"op":"issue_transition","key":"AG-1","expected_revision":rev,"state":"in_progress","lease_token":claim["lease"]["token"]})).is_err());
}

#[test]
fn crashed_worker_expiry_is_recovered_with_new_token_and_audited() {
    let mut f = Fixture::new();
    f.create("bug", None);
    f.state("AG-1", "ready");
    let old = f.run(json!({"op":"issue_claim","project":"AG"}))["claimed"].clone();
    // Simulate elapsed wall time deterministically, without a sleeping/flaky test.
    let conn = rusqlite::Connection::open(f.dir.path().join("test.sqlite3")).unwrap();
    conn.execute(
        "UPDATE issues SET doc=json_set(doc,'$.lease.expires_at',0) WHERE id='AG-1'",
        [],
    )
    .unwrap();
    f.err(
        json!({"op":"issue_heartbeat","key":"AG-1","lease_token":old["lease"]["token"]}),
        "conflict",
    );
    let new = f
        .as_actor("b", json!({"op":"issue_claim","project":"AG"}))
        .unwrap()["claimed"]
        .clone();
    assert_eq!(new["assignee"], "b");
    assert_ne!(new["lease"]["token"], old["lease"]["token"]);
    assert!(
        f.run(json!({"op":"events","key":"AG-1"}))
            .as_array()
            .unwrap()
            .iter()
            .any(|e| e["action"] == "claim_expired")
    );
    f.err(json!({"op":"issue_transition","key":"AG-1","expected_revision":new["revision"],"state":"done","lease_token":old["lease"]["token"]}),"conflict");
}

#[test]
fn sprints_filters_pagination_and_agent_lifecycle() {
    let mut f = Fixture::new();
    let sprint =
        f.run(json!({"op":"sprint_create","project":"AG","name":"Sprint 1","goal":"ship"}));
    let second = f.run(json!({"op":"sprint_create","project":"AG","name":"Sprint 2"}));
    f.run(json!({"op":"sprint_transition","id":sprint["id"],"state":"active"}));
    f.err(
        json!({"op":"sprint_transition","id":second["id"],"state":"active"}),
        "conflict",
    );
    f.create("task", None);
    f.create("spike", None);
    f.run(json!({"op":"issue_update","key":"AG-1","expected_revision":1,"sprint":sprint["id"],"labels":["rust"]}));
    assert_eq!(f.run(json!({"op":"issue_list","filter":{"text":"TASK","label":"rust","sprint":sprint["id"],"assignee":"a"}}))["total"],1);
    let page = f.run(json!({"op":"issue_list","filter":{"limit":1}}));
    assert_eq!(page["next_offset"], 1);
    assert!(
        f.run(json!({"op":"issue_list","filter":{"limit":1,"offset":1}}))["next_offset"].is_null()
    );
    f.err(
        json!({"op":"sprint_transition","id":sprint["id"],"state":"completed"}),
        "conflict",
    );
    f.run(json!({"op":"issue_update","key":"AG-1","expected_revision":2,"clear_sprint":true}));
    f.run(json!({"op":"sprint_transition","id":sprint["id"],"state":"completed"}));
    f.err(
        json!({"op":"issue_update","key":"AG-1","expected_revision":3,"sprint":sprint["id"]}),
        "conflict",
    );
    f.run(json!({"op":"agent_set_active","id":"b","active":false}));
    assert!(
        f.as_actor(
            "b",
            json!({"op":"comment_add","key":"AG-1","body":"inactive"})
        )
        .is_err()
    );
    f.err(
        json!({"op":"issue_assign","key":"AG-1","expected_revision":3,"assignee":"b"}),
        "conflict",
    );
    f.run(json!({"op":"agent_set_active","id":"b","active":true}));
    assert_eq!(
        f.run(json!({"op":"agent_list"})).as_array().unwrap().len(),
        2
    );
    assert_eq!(
        f.run(json!({"op":"sprint_list","project":"AG"}))
            .as_array()
            .unwrap()
            .len(),
        2
    );
    assert_eq!(f.run(json!({"op":"project_list"}))[0]["key"], "AG");
}

#[test]
fn completion_cycles_include_hierarchy_and_reopened_blockers() {
    let mut f = Fixture::new();
    f.create("epic", None);
    f.create("story", Some("AG-1"));
    f.create("task", None);
    f.err(
        json!({"op":"link_add","source":"AG-1","target":"AG-2","kind":"blocks"}),
        "conflict",
    );
    f.run(json!({"op":"link_add","source":"AG-1","target":"AG-3","kind":"blocks"}));
    f.err(
        json!({"op":"issue_update","key":"AG-3","expected_revision":1,"parent":"AG-1"}),
        "conflict",
    );
    f.run(json!({"op":"link_remove","source":"AG-1","target":"AG-3","kind":"blocks"}));
    f.run(json!({"op":"link_add","source":"AG-2","target":"AG-3","kind":"blocks"}));
    for state in ["ready", "in_progress", "done"] {
        f.state("AG-2", state);
    }
    for state in ["ready", "in_progress", "done"] {
        f.state("AG-3", state);
    }
    let rev = f.issue("AG-2")["revision"].clone();
    f.err(
        json!({"op":"issue_transition","key":"AG-2","expected_revision":rev,"state":"backlog"}),
        "conflict",
    );
    f.state("AG-3", "backlog");
    f.state("AG-2", "backlog");
}

#[test]
fn completed_sprint_history_can_be_edited_but_not_reopened_in_place() {
    let mut f = Fixture::new();
    let sprint = f.run(json!({"op":"sprint_create","project":"AG","name":"History"}));
    f.create("task", None);
    f.run(json!({"op":"issue_update","key":"AG-1","expected_revision":1,"sprint":sprint["id"]}));
    for state in ["ready", "in_progress", "done"] {
        f.state("AG-1", state);
    }
    f.run(json!({"op":"sprint_transition","id":sprint["id"],"state":"active"}));
    f.run(json!({"op":"sprint_transition","id":sprint["id"],"state":"completed"}));
    // Historical records remain editable even when the assigned worker is retired.
    f.run(json!({"op":"issue_assign","key":"AG-1","expected_revision":5,"assignee":"b"}));
    f.run(json!({"op":"agent_set_active","id":"b","active":false}));
    f.run(json!({"op":"issue_update","key":"AG-1","expected_revision":6,"description":"retrospective"}));
    f.err(
        json!({"op":"issue_transition","key":"AG-1","expected_revision":7,"state":"backlog"}),
        "conflict",
    );
    f.run(json!({"op":"issue_update","key":"AG-1","expected_revision":7,"clear_sprint":true}));
    f.state("AG-1", "backlog");
}

#[test]
fn blocking_canceling_unassigning_and_incremental_handoffs() {
    let mut f = Fixture::new();
    f.create("epic", None);
    f.create("bug", Some("AG-1"));
    f.create("task", Some("AG-1"));
    f.run(json!({"op":"link_add","source":"AG-2","target":"AG-3","kind":"blocks"}));
    f.state("AG-3", "ready");
    assert!(
        f.run(json!({"op":"board","project":"AG"}))["actionable"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    f.state("AG-2", "canceled");
    assert_eq!(
        f.run(json!({"op":"board","project":"AG"}))["actionable"][0],
        "AG-3"
    );
    f.state("AG-3", "in_progress");
    f.state("AG-3", "blocked");
    f.state("AG-3", "in_progress");
    let revision = f.issue("AG-3")["revision"].clone();
    let unassigned = f.run(
        json!({"op":"issue_assign","key":"AG-3","expected_revision":revision,"assignee":null}),
    );
    assert_eq!(unassigned["state"], "ready");
    assert!(unassigned["assignee"].is_null());
    f.err(json!({"op":"issue_transition","key":"AG-3","expected_revision":unassigned["revision"],"state":"in_progress"}),"conflict");
    f.state("AG-3", "canceled");
    f.state("AG-1", "canceled");
    let first = f.run(json!({"op":"comment_add","key":"AG-3","body":"Handoff context"}));
    let second = f.run(json!({"op":"comment_add","key":"AG-3","body":"Validation context"}));
    let comments = f.run(json!({"op":"comment_list","key":"AG-3","after":first["id"],"limit":1}));
    assert_eq!(comments[0]["id"], second["id"]);
    assert_eq!(comments.as_array().unwrap().len(), 1);
    let page = f.run(json!({"op":"events","key":"AG-3","limit":2}));
    let cursor = page[1]["id"].clone();
    let next = f.run(json!({"op":"events","key":"AG-3","after":cursor,"limit":1000}));
    assert!(
        next.as_array()
            .unwrap()
            .iter()
            .all(|e| e["id"].as_i64() > cursor.as_i64())
    );
    assert_eq!(
        f.run(json!({"op":"epic_progress","key":"AG-1"}))["canceled"],
        2
    );
}

#[test]
fn cross_project_integrity_priorities_and_future_schema_rejection() {
    let mut f = Fixture::new();
    f.run(json!({"op":"project_create","key":"OTHER","name":"Other"}));
    f.create("epic", None);
    f.err(json!({"op":"issue_create","project":"OTHER","kind":"story","title":"bad parent","parent":"AG-1"}),"invalid");
    let sprint = f.run(json!({"op":"sprint_create","project":"OTHER","name":"Other sprint"}));
    f.err(json!({"op":"issue_create","project":"AG","kind":"task","title":"bad sprint","sprint":sprint["id"]}),"invalid");
    for priority in ["low", "critical", "high"] {
        let i=f.run(json!({"op":"issue_create","project":"AG","kind":"task","title":priority,"priority":priority}));
        f.state(i["key"].as_str().unwrap(), "ready");
    }
    let claimed = f.run(json!({"op":"issue_claim","project":"AG"}))["claimed"].clone();
    assert_eq!(claimed["priority"], "critical");
    f.err(json!({"op":"issue_heartbeat","key":claimed["key"],"lease_token":claimed["lease"]["token"],"lease_seconds":0}),"invalid");
    let conn = rusqlite::Connection::open(f.dir.path().join("test.sqlite3")).unwrap();
    conn.pragma_update(None, "user_version", 2).unwrap();
    assert!(matches!(
        Store::open(f.dir.path().join("test.sqlite3")),
        Err(Error::Invalid(_))
    ));
}
