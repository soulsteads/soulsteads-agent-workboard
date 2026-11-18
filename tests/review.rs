use agent_workboard::{Command, Store};
use serde_json::{Value, json};

#[test]
fn submission_nesting_refusal_preserves_issue_events_and_retries_after_restart() {
    for idempotent in [false, true] {
        let tmp = tempfile::tempdir().unwrap();
        let db = tmp.path().join("depth.sqlite");
        let mut store = Store::open(&db).unwrap();
        execute(
            &mut store,
            "worker",
            json!({"op":"agent_register","id":"worker","name":"Worker"}),
        )
        .unwrap();
        execute(
            &mut store,
            "worker",
            json!({"op":"project_create","key":"RV","name":"Review"}),
        )
        .unwrap();
        let mut metadata = json!({});
        for _ in 0..124 {
            metadata = json!({"n": metadata});
        }
        let mut issue = execute(&mut store, "worker", json!({"op":"issue_create","project":"RV","kind":"task","title":"Deep metadata","metadata":metadata,"assignee":"worker"})).unwrap();
        for state in ["ready", "in_progress"] {
            issue = execute(&mut store, "worker", json!({"op":"issue_transition","key":"RV-1","expected_revision":issue["revision"],"state":state})).unwrap();
        }
        let before = get(&mut store);
        let events = execute(&mut store, "worker", json!({"op":"events","key":"RV-1"})).unwrap();
        let command: Command = serde_json::from_value(json!({"op":"issue_submit","key":"RV-1","expected_revision":issue["revision"],"summary":"Done","evidence":[{"reference":"test://depth","summary":"Inspected"}]})).unwrap();
        let key = idempotent.then_some("deep-submit");
        for _ in 0..2 {
            let error = store
                .execute_idempotent("worker", key, command.clone())
                .unwrap_err();
            assert_eq!(error.code(), "invalid");
            assert_eq!(get(&mut store), before);
            assert_eq!(
                execute(&mut store, "worker", json!({"op":"events","key":"RV-1"})).unwrap(),
                events
            );
            execute(&mut store, "worker", json!({"op":"board","project":"RV"})).unwrap();
            execute(
                &mut store,
                "worker",
                json!({"op":"issue_list","filter":{"project":"RV"}}),
            )
            .unwrap();
            drop(store);
            store = Store::open(&db).unwrap();
        }
        // A failed admission does not poison an idempotency key. A smaller,
        // valid snapshot can use that same key after correcting the metadata.
        let updated = execute(&mut store, "worker", json!({"op":"issue_update","key":"RV-1","expected_revision":before["revision"],"metadata":{"bounded":true}})).unwrap();
        let corrected: Command = serde_json::from_value(json!({"op":"issue_submit","key":"RV-1","expected_revision":updated["revision"],"summary":"Done","evidence":[{"reference":"test://depth","summary":"Inspected"}]})).unwrap();
        let (submitted, replayed) = store
            .execute_idempotent("worker", key, corrected.clone())
            .unwrap();
        assert!(!replayed);
        assert_eq!(submitted["state"], "in_review");
        drop(store);
        let mut store = Store::open(&db).unwrap();
        assert_eq!(get(&mut store), submitted);
        if idempotent {
            let (repeated, replayed) = store.execute_idempotent("worker", key, corrected).unwrap();
            assert!(replayed);
            assert_eq!(repeated, submitted);
        }
    }
}

fn execute(
    store: &mut Store,
    actor: &str,
    command: Value,
) -> Result<Value, agent_workboard::Error> {
    store.execute(actor, serde_json::from_value::<Command>(command).unwrap())
}
fn get(store: &mut Store) -> Value {
    execute(store, "worker", json!({"op":"issue_get","key":"RV-1"})).unwrap()["issue"].clone()
}

#[test]
fn rejected_or_changed_evidence_cannot_authorize_closure_and_retry_is_stable() {
    let tmp = tempfile::tempdir().unwrap();
    let mut s = Store::open(tmp.path().join("review.sqlite")).unwrap();
    for actor in ["worker", "reviewer"] {
        execute(
            &mut s,
            actor,
            json!({"op":"agent_register","id":actor,"name":actor}),
        )
        .unwrap();
    }
    execute(
        &mut s,
        "worker",
        json!({"op":"project_create","key":"RV","name":"Review"}),
    )
    .unwrap();
    execute(&mut s, "worker", json!({"op":"issue_create","project":"RV","kind":"task","title":"Original","assignee":"worker"})).unwrap();
    for state in ["ready", "in_progress"] {
        let i = get(&mut s);
        execute(&mut s, "worker", json!({"op":"issue_transition","key":"RV-1","expected_revision":i["revision"],"state":state})).unwrap();
    }
    for approve in [false, true] {
        let i = get(&mut s);
        let command: Command = serde_json::from_value(json!({"op":"issue_submit","key":"RV-1","expected_revision":i["revision"],"summary":"Receipt","evidence":[{"reference":"test://receipt","summary":"Actual result"}]})).unwrap();
        let key = format!("submit-{approve}");
        let (submitted, replayed) = s
            .execute_idempotent("worker", Some(&key), command.clone())
            .unwrap();
        assert!(!replayed);
        let (repeated, replayed) = s.execute_idempotent("worker", Some(&key), command).unwrap();
        assert!(replayed);
        assert_eq!(submitted, repeated);
        assert!(execute(&mut s, "reviewer", json!({"op":"issue_review","key":"RV-1","expected_revision":submitted["revision"],"submission_id":"outdated","approved":true,"note":"Wrong submission"})).is_err());
        let reviewed = execute(&mut s, "reviewer", json!({"op":"issue_review","key":"RV-1","expected_revision":submitted["revision"],"submission_id":submitted["submission"]["id"],"approved":approve,"note":"Inspected receipt"})).unwrap();
        if !approve {
            assert!(execute(&mut s, "worker", json!({"op":"issue_transition","key":"RV-1","expected_revision":reviewed["revision"],"state":"done"})).is_err());
        }
        let changed = execute(&mut s, "worker", json!({"op":"issue_update","key":"RV-1","expected_revision":reviewed["revision"],"title":"Changed acceptance context"})).unwrap();
        assert_eq!(changed["state"], "in_progress");
        assert!(changed["submission"].is_null() && changed["review"].is_null());
        assert!(execute(&mut s, "worker", json!({"op":"issue_transition","key":"RV-1","expected_revision":changed["revision"],"state":"done"})).is_err());
    }
}

#[test]
fn evidence_review_closure_preserve_history_and_reject_stale_workers() {
    let tmp = tempfile::tempdir().unwrap();
    let db = tmp.path().join("review.sqlite");
    let mut s = Store::open(&db).unwrap();
    for actor in ["worker", "reviewer"] {
        execute(
            &mut s,
            actor,
            json!({"op":"agent_register","id":actor,"name":actor}),
        )
        .unwrap();
    }
    execute(
        &mut s,
        "worker",
        json!({"op":"project_create","key":"RV","name":"Review"}),
    )
    .unwrap();
    execute(&mut s,"worker",json!({"op":"issue_create","project":"RV","kind":"task","title":"Bounded work","acceptance_criteria":["Inspect result"]})).unwrap();
    execute(
        &mut s,
        "worker",
        json!({"op":"issue_transition","key":"RV-1","expected_revision":1,"state":"ready"}),
    )
    .unwrap();
    let i =
        execute(&mut s, "worker", json!({"op":"issue_claim","project":"RV"})).unwrap()["claimed"]
            .clone();
    let token = i["lease"]["token"].clone();
    assert!(execute(&mut s,"worker",json!({"op":"issue_transition","key":"RV-1","expected_revision":i["revision"],"lease_token":token,"state":"done"})).is_err());
    assert!(execute(&mut s,"worker",json!({"op":"issue_submit","key":"RV-1","expected_revision":i["revision"],"lease_token":"old","summary":"Done","evidence":[{"reference":"test://result","summary":"Inspected"}]})).is_err());
    assert!(execute(&mut s,"worker",json!({"op":"issue_submit","key":"RV-1","expected_revision":i["revision"],"lease_token":token,"summary":"Done","evidence":[]})).is_err());
    let submitted = execute(&mut s,"worker",json!({"op":"issue_submit","key":"RV-1","expected_revision":i["revision"],"lease_token":token,"summary":"Done","evidence":[{"reference":"test://result","summary":"Result receipt"}]})).unwrap();
    assert_eq!(submitted["state"], "in_review");
    assert!(submitted["lease"].is_null());
    let review = json!({"op":"issue_review","key":"RV-1","expected_revision":submitted["revision"],"submission_id":submitted["submission"]["id"],"approved":true,"note":"Independently inspected result receipt"});
    assert!(execute(&mut s, "worker", review.clone()).is_err());
    let accepted = execute(&mut s, "reviewer", review).unwrap();
    assert_eq!(accepted["review"]["external_outcome_verified"], false);
    assert!(execute(&mut s,"worker",json!({"op":"issue_transition","key":"RV-1","expected_revision":accepted["revision"],"lease_token":token,"state":"done"})).is_err());
    execute(&mut s,"reviewer",json!({"op":"issue_transition","key":"RV-1","expected_revision":accepted["revision"],"state":"done"})).unwrap();
    drop(s);
    let mut s = Store::open(&db).unwrap();
    let closed = get(&mut s);
    assert_eq!(closed["state"], "done");
    assert_eq!(closed["submission"]["actor"], "worker");
    assert_eq!(closed["review"]["actor"], "reviewer");
    assert_eq!(
        closed["submission"]["issue_snapshot"]["acceptance_criteria"][0],
        "Inspect result"
    );
    let reopened = execute(&mut s,"worker",json!({"op":"issue_transition","key":"RV-1","expected_revision":closed["revision"],"state":"backlog"})).unwrap();
    assert!(reopened["submission"].is_null());
    assert!(reopened["review"].is_null());
    let events = execute(&mut s, "worker", json!({"op":"events","key":"RV-1"})).unwrap();
    assert!(
        events
            .as_array()
            .unwrap()
            .iter()
            .any(|event| event["action"] == "review_recorded")
    );
}
