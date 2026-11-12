//! Indexed queries, full-text search, saved filters and the v3 -> v4 migration.
use agent_workboard::{Command, Error, IssueFilter, Store, mcp, query};
use rusqlite::Connection;
use serde_json::{Value, json};
use std::{
    path::Path,
    sync::{Arc, Barrier},
};
use tempfile::TempDir;

fn cmd(value: Value) -> Command {
    serde_json::from_value(value).unwrap()
}
fn run(store: &mut Store, actor: &str, value: Value) -> Value {
    store.execute(actor, cmd(value)).unwrap()
}
fn fail(store: &mut Store, actor: &str, value: Value) -> Error {
    store.execute(actor, cmd(value)).unwrap_err()
}
fn keys(list: &Value) -> Vec<String> {
    list["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|i| {
            i.get("issue").unwrap_or(i)["key"]
                .as_str()
                .unwrap()
                .to_owned()
        })
        .collect()
}
fn search(store: &mut Store, query: &str) -> Vec<String> {
    keys(&run(
        store,
        "ann",
        json!({"op":"issue_search","query":query,"limit":1000}),
    ))
}

fn board() -> (TempDir, Store) {
    let dir = tempfile::tempdir().unwrap();
    let mut s = Store::open(dir.path().join("s.sqlite3")).unwrap();
    for id in ["ann", "bob"] {
        run(&mut s, id, json!({"op":"agent_register","id":id,"name":id}));
    }
    run(
        &mut s,
        "ann",
        json!({"op":"project_create","key":"AG","name":"Agents"}),
    );
    run(
        &mut s,
        "ann",
        json!({"op":"project_create","key":"WEB","name":"Web"}),
    );
    (dir, s)
}
fn create(s: &mut Store, project: &str, title: &str, extra: Value) -> String {
    let mut c = json!({"op":"issue_create","project":project,"kind":"task","title":title});
    for (k, v) in extra.as_object().unwrap() {
        c[k] = v.clone();
    }
    run(s, "ann", c)["key"].as_str().unwrap().to_owned()
}

// ----------------------------------------------- equivalence with the old filter

/// The pre-index algorithm, kept here as an independent oracle: load everything,
/// filter in Rust, sort by (priority, created_at, number in key), stable on creation order.
fn oracle(docs: &[Value], f: &Value) -> Vec<String> {
    let s = |d: &Value, k: &str| d[k].as_str().map(str::to_owned);
    let mut found: Vec<&Value> = docs
        .iter()
        .filter(|d| {
            let want = |k: &str| f.get(k).and_then(Value::as_str);
            want("project").is_none_or(|v| s(d, "project").as_deref() == Some(v))
                && want("kind").is_none_or(|v| s(d, "kind").as_deref() == Some(v))
                && want("state").is_none_or(|v| s(d, "state").as_deref() == Some(v))
                && want("priority").is_none_or(|v| s(d, "priority").as_deref() == Some(v))
                && want("assignee").is_none_or(|v| s(d, "assignee").as_deref() == Some(v))
                && (f.get("unassigned") != Some(&json!(true)) || d["assignee"].is_null())
                && want("parent").is_none_or(|v| s(d, "parent").as_deref() == Some(v))
                && want("sprint").is_none_or(|v| s(d, "sprint").as_deref() == Some(v))
                && want("label")
                    .is_none_or(|v| d["labels"].as_array().unwrap().iter().any(|l| l == v))
                && want("text").is_none_or(|t| {
                    format!(
                        "{} {} {}",
                        d["key"].as_str().unwrap(),
                        d["title"].as_str().unwrap(),
                        d["description"].as_str().unwrap()
                    )
                    .to_lowercase()
                    .contains(&t.to_lowercase())
                })
        })
        .collect();
    found.sort_by_key(|d| {
        let rank = match d["priority"].as_str().unwrap() {
            "critical" => 0,
            "high" => 1,
            "medium" => 2,
            _ => 3,
        };
        let number: i64 = d["key"]
            .as_str()
            .unwrap()
            .rsplit('-')
            .next()
            .unwrap()
            .parse()
            .unwrap_or(0);
        (rank, d["created_at"].as_i64().unwrap(), number)
    });
    found
        .iter()
        .map(|d| d["key"].as_str().unwrap().to_owned())
        .collect()
}
struct Rng(u64);
impl Rng {
    fn next(&mut self, n: usize) -> usize {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        (self.0 % n as u64) as usize
    }
    fn pick<'a>(&mut self, items: &[&'a str]) -> &'a str {
        items[self.next(items.len())]
    }
}
fn raw_doc(key: &str, project: &str, rng: &mut Rng, earlier: &[String]) -> Value {
    let labels: Vec<&str> = ["rust", "ui", "db", "ops", "docs"]
        .into_iter()
        .filter(|_| rng.next(3) == 0)
        .collect();
    let words = ["lease", "board", "Scheduler", "webhook", "Dash", "retry"];
    let assignee = [json!(null), json!("ann"), json!("bob")][rng.next(3)].clone();
    let sprint = [json!(null), json!("s1"), json!("s2")][rng.next(3)].clone();
    let parent = if earlier.is_empty() || rng.next(3) != 0 {
        Value::Null
    } else {
        json!(earlier[rng.next(earlier.len())])
    };
    json!({
        "key": key, "project": project,
        "kind": rng.pick(&["epic","story","task","bug","subtask","spike"]),
        "title": format!("{} {} thing", rng.pick(&words), rng.pick(&words)),
        "description": format!("Details about {}.", rng.pick(&words)),
        "acceptance_criteria": [], "state": rng.pick(&["backlog","ready","in_progress","blocked","in_review","done","canceled"]),
        "priority": rng.pick(&["critical","high","medium","low"]),
        "assignee": assignee, "parent": parent, "sprint": sprint,
        "labels": labels, "estimate": null, "metadata": {}, "revision": 1,
        // Few distinct timestamps, so that ties and the key-number tiebreak are exercised.
        "created_at": 1000 + rng.next(6) as i64, "updated_at": 1000, "lease": null
    })
}

#[test]
fn indexed_listing_matches_the_original_algorithm_on_random_boards() {
    let (dir, mut store) = board();
    let conn = Connection::open(dir.path().join("s.sqlite3")).unwrap();
    let mut rng = Rng(0x9E3779B97F4A7C15);
    let mut docs: Vec<Value> = vec![];
    let mut keys_so_far: Vec<String> = vec![];
    // Interleave projects, and cross the 9->10 and 99->100 boundaries where numeric
    // and text ordering of the key number differ.
    for n in 1..=130 {
        for project in ["AG", "WEB"] {
            let key = format!("{project}-{n}");
            let doc = raw_doc(&key, project, &mut rng, &keys_so_far);
            conn.execute(
                "INSERT INTO issues(id,doc) VALUES(?1,?2)",
                rusqlite::params![key, doc.to_string()],
            )
            .unwrap();
            keys_so_far.push(key);
            docs.push(doc);
        }
    }
    let mut checked = 0;
    let options: Vec<(&str, Vec<Value>)> = vec![
        ("project", vec![json!("AG"), json!("WEB"), json!("NONE")]),
        ("kind", vec![json!("bug"), json!("epic"), json!("spike")]),
        (
            "state",
            vec![json!("ready"), json!("done"), json!("blocked")],
        ),
        ("priority", vec![json!("critical"), json!("low")]),
        (
            "assignee",
            vec![json!("ann"), json!("bob"), json!("nobody")],
        ),
        ("parent", vec![json!("AG-3"), json!("WEB-1")]),
        ("sprint", vec![json!("s1"), json!("s2"), json!("s3")]),
        ("label", vec![json!("rust"), json!("ui"), json!("absent")]),
        (
            "text",
            vec![
                json!("lease"),
                json!("SCHEDULER"),
                json!("webhook retry"),
                json!("ag-12"),
                json!("zzz"),
            ],
        ),
        ("unassigned", vec![json!(true)]),
    ];
    for round in 0..400 {
        let mut f = json!({});
        for (name, values) in &options {
            if rng.next(4) == 0 {
                f[name] = values[rng.next(values.len())].clone();
            }
        }
        if round % 5 == 0 {
            f = json!({});
        }
        let expected = oracle(&docs, &f);
        let mut everything = f.as_object().unwrap().clone();
        everything.insert("limit".into(), json!(1000));
        let all = run(
            &mut store,
            "ann",
            json!({"op":"issue_list","filter":everything}),
        );
        assert_eq!(
            keys(&all),
            expected.iter().take(1000).cloned().collect::<Vec<_>>(),
            "filter {f}"
        );
        assert_eq!(all["total"], expected.len(), "total for {f}");
        // And a random page of it.
        let (offset, limit) = (rng.next(40), 1 + rng.next(30));
        let mut paged = f.as_object().unwrap().clone();
        paged.insert("offset".into(), json!(offset));
        paged.insert("limit".into(), json!(limit));
        let page = run(&mut store, "ann", json!({"op":"issue_list","filter":paged}));
        let want: Vec<_> = expected.iter().skip(offset).take(limit).cloned().collect();
        assert_eq!(keys(&page), want, "page {offset}+{limit} of {f}");
        assert_eq!(page["total"], expected.len());
        let more = offset + want.len() < expected.len();
        assert_eq!(
            page["next_offset"],
            if more {
                json!(offset + want.len())
            } else {
                Value::Null
            },
            "next_offset for {f}"
        );
        checked += 1;
    }
    assert_eq!(checked, 400);
    // The board view groups the same ordered set into columns.
    let b = run(&mut store, "ann", json!({"op":"board","project":"AG"}));
    let expected = oracle(&docs, &json!({"project":"AG"}));
    assert_eq!(b["total"], expected.len());
    let ready: Vec<_> = b["columns"]["ready"]
        .as_array()
        .unwrap()
        .iter()
        .map(|i| i["key"].as_str().unwrap().to_owned())
        .collect();
    let want: Vec<_> = oracle(&docs, &json!({"project":"AG","state":"ready"}));
    assert_eq!(ready, want);
}

// ------------------------------------------------------------------- search

#[test]
fn search_finds_words_phrases_prefixes_alternatives_and_exclusions() {
    let (_d, mut s) = board();
    let a = create(
        &mut s,
        "AG",
        "Lease expiry recovery",
        json!({"description":"Recover claims when a worker crashes"}),
    );
    let b = create(
        &mut s,
        "AG",
        "Webhook retry backoff",
        json!({"description":"Exponential delay between attempts"}),
    );
    let c = create(
        &mut s,
        "AG",
        "Dashboard polling",
        json!({"description":"Replace polling with a stream"}),
    );
    let d = create(&mut s, "WEB", "Lease dashboard", json!({}));
    let both = |q: &str, s: &mut Store| {
        let mut v = search(s, q);
        v.sort();
        v
    };
    let sorted = |mut v: Vec<String>| {
        v.sort();
        v
    };
    assert_eq!(both("lease", &mut s), sorted(vec![a.clone(), d.clone()]));
    // Every word must match, across fields.
    assert_eq!(both("lease dashboard", &mut s), vec![d.clone()]);
    assert_eq!(
        both("lease recover", &mut s),
        vec![a.clone()],
        "title word plus description word"
    );
    // Phrases are exact and ordered.
    assert_eq!(both(r#""retry backoff""#, &mut s), vec![b.clone()]);
    assert!(search(&mut s, r#""backoff retry""#).is_empty());
    // Prefix, stemming and case-insensitivity.
    assert_eq!(both("recov*", &mut s), vec![a.clone()]);
    assert_eq!(both("claiming", &mut s), vec![a.clone()], "claims -> claim");
    assert_eq!(
        both("DASHBOARD", &mut s),
        sorted(vec![c.clone(), d.clone()])
    );
    // OR and exclusion.
    assert_eq!(
        both("backoff OR polling", &mut s),
        sorted(vec![b.clone(), c.clone()])
    );
    assert_eq!(both("dashboard -lease", &mut s), vec![c.clone()]);
    assert_eq!(
        both(r#"dashboard -"lease dashboard""#, &mut s),
        vec![c.clone()]
    );
    // An issue key is searchable even though '-' is an operator in raw FTS5.
    assert_eq!(both(&a, &mut s), vec![a.clone()]);
    assert!(search(&mut s, "nonexistentword").is_empty());
}

#[test]
fn comments_labels_and_criteria_are_searchable_and_stay_current() {
    let (_d, mut s) = board();
    let k = create(
        &mut s,
        "AG",
        "Plain title",
        json!({"labels":["urgent-fix"],"acceptance_criteria":["Handles clock skew"]}),
    );
    assert_eq!(search(&mut s, "skew"), [k.as_str()], "acceptance criteria");
    assert_eq!(search(&mut s, "urgent"), [k.as_str()], "labels");
    assert!(search(&mut s, "needle").is_empty());
    run(
        &mut s,
        "bob",
        json!({"op":"comment_add","key":k,"body":"Found the needle in the logs"}),
    );
    assert_eq!(
        search(&mut s, "needle"),
        [k.as_str()],
        "a comment is searchable the moment it is added"
    );
    assert_eq!(
        search(&mut s, "plain needle"),
        [k.as_str()],
        "terms may come from the title and a comment"
    );
    run(
        &mut s,
        "bob",
        json!({"op":"comment_add","key":k,"body":"second comment mentions haystack"}),
    );
    assert_eq!(
        search(&mut s, "needle haystack"),
        [k.as_str()],
        "earlier comments survive later ones"
    );

    // Editing text re-indexes: the old words stop matching.
    let rev = run(&mut s, "ann", json!({"op":"issue_get","key":k}))["issue"]["revision"]
        .as_i64()
        .unwrap();
    run(
        &mut s,
        "ann",
        json!({"op":"issue_update","key":k,"expected_revision":rev,"title":"Renamed ticket","description":"mentions zebra","labels":["other"]}),
    );
    assert!(search(&mut s, "plain").is_empty(), "old title is gone");
    assert!(search(&mut s, "urgent").is_empty(), "old label is gone");
    assert_eq!(search(&mut s, "renamed zebra other"), [k.as_str()]);
    assert_eq!(
        search(&mut s, "needle"),
        [k.as_str()],
        "comments are kept across edits"
    );

    // A change that touches no text (a transition) leaves search and labels intact.
    let rev = run(&mut s, "ann", json!({"op":"issue_get","key":k}))["issue"]["revision"]
        .as_i64()
        .unwrap();
    run(
        &mut s,
        "ann",
        json!({"op":"issue_transition","key":k,"expected_revision":rev,"state":"ready"}),
    );
    assert_eq!(search(&mut s, "renamed"), [k.as_str()]);
    let by_label = run(
        &mut s,
        "ann",
        json!({"op":"issue_list","filter":{"label":"other"}}),
    );
    assert_eq!(keys(&by_label), [k.as_str()]);
    assert!(
        keys(&run(
            &mut s,
            "ann",
            json!({"op":"issue_list","filter":{"label":"urgent-fix"}})
        ))
        .is_empty()
    );
}

#[test]
fn results_are_ranked_by_relevance_with_highlighted_snippets() {
    let (_d, mut s) = board();
    let in_comment = create(&mut s, "AG", "Unrelated heading", json!({}));
    run(
        &mut s,
        "ann",
        json!({"op":"comment_add","key":in_comment,"body":"mentions the migration once"}),
    );
    let in_description = create(
        &mut s,
        "AG",
        "Another heading",
        json!({"description":"the migration plan"}),
    );
    let in_title = create(&mut s, "AG", "Migration", json!({}));
    let found = run(
        &mut s,
        "ann",
        json!({"op":"issue_search","query":"migration"}),
    );
    assert_eq!(
        keys(&found),
        [
            in_title.as_str(),
            in_description.as_str(),
            in_comment.as_str()
        ],
        "title > description > comment"
    );
    let scores: Vec<f64> = found["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|i| i["score"].as_f64().unwrap())
        .collect();
    assert!(
        scores.windows(2).all(|w| w[0] >= w[1]),
        "scores descend: {scores:?}"
    );
    assert!(scores.iter().all(|s| *s > 0.0));
    let snippets: Vec<&str> = found["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|i| i["snippet"].as_str().unwrap())
        .collect();
    assert!(
        snippets
            .iter()
            .all(|s| s.contains("[migration]") || s.contains("[Migration]")),
        "{snippets:?}"
    );
    assert_eq!(found["query"], "migration");
}

#[test]
fn qualifiers_combine_with_text_and_actionable_respects_blockers() {
    let (_d, mut s) = board();
    let ready = create(&mut s, "AG", "Ship scheduler", json!({"priority":"high"}));
    let blocked = create(&mut s, "AG", "Ship scheduler tests", json!({}));
    let other = create(&mut s, "WEB", "Ship scheduler ui", json!({"kind":"bug"}));
    for k in [ready.as_str(), blocked.as_str(), other.as_str()] {
        let rev = run(&mut s, "ann", json!({"op":"issue_get","key":k}))["issue"]["revision"]
            .as_i64()
            .unwrap();
        run(
            &mut s,
            "ann",
            json!({"op":"issue_transition","key":k,"expected_revision":rev,"state":"ready"}),
        );
    }
    run(
        &mut s,
        "ann",
        json!({"op":"link_add","source":ready,"target":blocked,"kind":"blocks"}),
    );
    let sorted = |mut v: Vec<String>| {
        v.sort();
        v
    };
    assert_eq!(
        sorted(search(&mut s, "scheduler")),
        sorted(vec![ready.clone(), blocked.clone(), other.clone()])
    );
    assert_eq!(search(&mut s, "scheduler project:WEB"), [other.as_str()]);
    assert_eq!(search(&mut s, "scheduler kind:bug"), [other.as_str()]);
    assert_eq!(search(&mut s, "scheduler priority:high"), [ready.as_str()]);
    assert_eq!(
        search(&mut s, "scheduler is:actionable project:AG"),
        [ready.as_str()],
        "a blocked issue is not actionable"
    );
    // Actionable results are counted after the check, not before: 2 of the 3 matches qualify.
    let page = run(
        &mut s,
        "ann",
        json!({"op":"issue_search","query":"scheduler is:actionable","limit":1}),
    );
    assert_eq!(page["total"], 2);
    assert_eq!(page["next_offset"], 1);
    assert_eq!(
        sorted(search(&mut s, "state:ready")),
        sorted(vec![ready.clone(), blocked.clone(), other.clone()]),
        "filters alone need no text"
    );
    assert!(search(&mut s, "scheduler state:done").is_empty());
    run(&mut s, "ann", json!({"op":"issue_claim","project":"AG"}));
    assert_eq!(search(&mut s, "assignee:ann"), [ready.as_str()]);
    assert_eq!(
        sorted(search(&mut s, "scheduler assignee:none")),
        sorted(vec![blocked.clone(), other.clone()])
    );
    // `blocked` stays blocked while its blocker is merely in progress; only `other` can be worked.
    assert_eq!(search(&mut s, "scheduler is:actionable"), [other.as_str()]);
}

#[test]
fn paging_totals_and_argument_validation() {
    let (_d, mut s) = board();
    for n in 0..25 {
        create(&mut s, "AG", &format!("paged thing {n}"), json!({}));
    }
    let p1 = run(
        &mut s,
        "ann",
        json!({"op":"issue_search","query":"paged","limit":10}),
    );
    let p2 = run(
        &mut s,
        "ann",
        json!({"op":"issue_search","query":"paged","limit":10,"offset":10}),
    );
    let p3 = run(
        &mut s,
        "ann",
        json!({"op":"issue_search","query":"paged","limit":10,"offset":20}),
    );
    assert_eq!(
        (p1["total"].as_i64(), p1["next_offset"].as_i64()),
        (Some(25), Some(10))
    );
    assert_eq!(p2["next_offset"], 20);
    assert_eq!((keys(&p3).len(), p3["next_offset"].is_null()), (5, true));
    let mut all: Vec<_> = [&p1, &p2, &p3].iter().flat_map(|p| keys(p)).collect();
    all.sort();
    all.dedup();
    assert_eq!(all.len(), 25, "pages neither overlap nor skip");
    for bad in [
        json!({"op":"issue_search"}),
        json!({"op":"issue_search","query":"a","saved":"b"}),
        json!({"op":"issue_search","query":"a","limit":0}),
        json!({"op":"issue_search","query":"a","limit":1001}),
    ] {
        assert!(
            matches!(fail(&mut s, "ann", bad.clone()), Error::Invalid(_)),
            "{bad}"
        );
    }
    assert!(matches!(
        fail(
            &mut s,
            "ann",
            json!({"op":"issue_search","saved":"missing"})
        ),
        Error::NotFound(_)
    ));
}

#[test]
fn hostile_queries_never_reach_the_search_engine_as_syntax() {
    let (_d, mut s) = board();
    create(
        &mut s,
        "AG",
        "Quote \" and (paren) and star* and colon: title",
        json!({"description":"NEAR AND OR NOT"}),
    );
    let hostile = [
        r#"""#,
        r#""""#,
        "(",
        ")",
        "((",
        "a (",
        "*",
        "**",
        "a*b",
        "^a",
        "a^",
        "{a}",
        "title:x",
        "NEAR(a b)",
        "NEAR/3",
        "a AND b",
        "a NOT b",
        "NOT",
        "AND",
        "OR",
        "a OR",
        "OR a",
        "- -",
        "-",
        "--",
        "a -",
        ":",
        "::",
        "a::b",
        "'",
        "\\",
        "%",
        "_",
        "\u{0}",
        "a\u{0}b",
        "😀",
        "\n",
        "a\tb",
        "col:a",
        "key:AG-1",
        "rank:1",
        "title : x",
        "state:",
        "state:nope",
        "label:",
        r#"label:""#,
        &"x".repeat(1001),
        &"a ".repeat(40),
    ];
    for q in hostile {
        match s.execute("ann", cmd(json!({"op":"issue_search","query":q}))) {
            Ok(_) => {}
            // Rejected with a clear message is fine. A storage error is a bug.
            Err(Error::Invalid(message)) => assert!(!message.is_empty(), "{q:?}"),
            Err(other) => panic!("query {q:?} produced {other}"),
        }
    }
    // Text full of FTS operators is still findable as plain words.
    assert_eq!(search(&mut s, "NEAR AND OR NOT").len(), 1);
    assert_eq!(search(&mut s, r#""(paren)""#).len(), 1);
}

#[test]
fn parser_rejects_mistakes_with_actionable_messages_through_the_api() {
    let (_d, mut s) = board();
    match fail(
        &mut s,
        "ann",
        json!({"op":"issue_search","query":"statee:ready"}),
    ) {
        Error::Invalid(m) => assert!(
            m.contains("unknown filter 'statee:'") && m.contains("project, kind, state"),
            "{m}"
        ),
        other => panic!("{other}"),
    }
    let parsed = query::parse("lease state:ready").unwrap();
    assert_eq!(
        parsed.filter,
        IssueFilter {
            state: Some(agent_workboard::State::Ready),
            ..Default::default()
        }
    );
}

// -------------------------------------------------------------- saved filters

#[test]
fn saved_filters_are_shared_validated_owned_and_audited() {
    let (_d, mut s) = board();
    let k = create(
        &mut s,
        "AG",
        "Flaky scheduler",
        json!({"priority":"critical"}),
    );
    create(&mut s, "AG", "Calm scheduler", json!({}));
    let saved = run(
        &mut s,
        "ann",
        json!({"op":"filter_save","name":"critical-scheduler","query":"scheduler priority:critical"}),
    );
    assert_eq!(
        (saved["owner"].as_str(), saved["name"].as_str()),
        (Some("ann"), Some("critical-scheduler"))
    );
    // Another agent can run it, and sees the same result as the raw query.
    let by_name = run(
        &mut s,
        "bob",
        json!({"op":"issue_search","saved":"critical-scheduler"}),
    );
    assert_eq!(keys(&by_name), [k.as_str()]);
    assert_eq!(
        by_name["query"], "scheduler priority:critical",
        "the effective query is echoed back"
    );
    let listed = run(&mut s, "bob", json!({"op":"filter_list"}));
    assert_eq!(listed.as_array().unwrap().len(), 1);
    assert_eq!(listed[0]["query"], "scheduler priority:critical");

    // Bad queries are refused at save time, so a saved filter can never be unrunnable.
    for bad in ["state:nope", "statee:ready", "a OR", r#""open"#] {
        assert!(
            matches!(
                fail(
                    &mut s,
                    "ann",
                    json!({"op":"filter_save","name":"bad","query":bad})
                ),
                Error::Invalid(_)
            ),
            "{bad}"
        );
    }
    for name in ["", "has space", "a/b", &"n".repeat(65)] {
        assert!(
            matches!(
                fail(
                    &mut s,
                    "ann",
                    json!({"op":"filter_save","name":name,"query":"x"})
                ),
                Error::Invalid(_)
            ),
            "{name:?}"
        );
    }
    // Only the owner may replace or delete.
    assert!(matches!(
        fail(
            &mut s,
            "bob",
            json!({"op":"filter_save","name":"critical-scheduler","query":"other"})
        ),
        Error::Conflict(_)
    ));
    assert!(matches!(
        fail(
            &mut s,
            "bob",
            json!({"op":"filter_delete","name":"critical-scheduler"})
        ),
        Error::Conflict(_)
    ));
    run(
        &mut s,
        "ann",
        json!({"op":"filter_save","name":"critical-scheduler","query":"calm"}),
    );
    assert_eq!(
        keys(&run(
            &mut s,
            "bob",
            json!({"op":"issue_search","saved":"critical-scheduler"})
        ))
        .len(),
        1
    );
    assert_eq!(
        run(&mut s, "ann", json!({"op":"filter_list"}))
            .as_array()
            .unwrap()
            .len(),
        1,
        "replace, not duplicate"
    );
    run(
        &mut s,
        "ann",
        json!({"op":"filter_delete","name":"critical-scheduler"}),
    );
    assert!(matches!(
        fail(
            &mut s,
            "ann",
            json!({"op":"filter_delete","name":"critical-scheduler"})
        ),
        Error::NotFound(_)
    ));
    assert!(matches!(
        fail(
            &mut s,
            "ann",
            json!({"op":"issue_search","saved":"critical-scheduler"})
        ),
        Error::NotFound(_)
    ));

    let log = run(&mut s, "ann", json!({"op":"events","limit":1000}));
    let actions: Vec<_> = log
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["action"].as_str().unwrap())
        .filter(|a| a.starts_with("filter_"))
        .collect();
    assert_eq!(actions, ["filter_saved", "filter_saved", "filter_deleted"]);
}

#[test]
fn saved_filter_commands_are_retry_safe_and_search_is_read_only() {
    let (_d, mut s) = board();
    let save = cmd(json!({"op":"filter_save","name":"mine","query":"assignee:ann"}));
    let (first, r1) = s
        .execute_idempotent("ann", Some("k1"), save.clone())
        .unwrap();
    let (second, r2) = s.execute_idempotent("ann", Some("k1"), save).unwrap();
    assert_eq!((r1, r2, first == second), (false, true, true));
    let before = run(&mut s, "ann", json!({"op":"events","limit":1000}))
        .as_array()
        .unwrap()
        .len();
    for _ in 0..3 {
        run(
            &mut s,
            "ann",
            json!({"op":"issue_search","query":"anything"}),
        );
        run(&mut s, "ann", json!({"op":"filter_list"}));
    }
    assert_eq!(
        run(&mut s, "ann", json!({"op":"events","limit":1000}))
            .as_array()
            .unwrap()
            .len(),
        before,
        "reads emit no events"
    );
}

// ------------------------------------------------------------------ indexes

#[test]
fn every_filter_resolves_through_an_index_instead_of_scanning_issues() {
    let (_d, s) = board();
    let plan = |f: IssueFilter, q: Option<&str>| s.explain_issue_query(&f, q).unwrap().join(" | ");
    let none = IssueFilter::default;
    let p = plan(
        IssueFilter {
            project: Some("AG".into()),
            state: Some(agent_workboard::State::Ready),
            ..none()
        },
        None,
    );
    assert!(
        p.contains("USING INDEX issues_board") || p.contains("USING COVERING INDEX issues_board"),
        "{p}"
    );
    assert!(
        !p.contains("USE TEMP B-TREE"),
        "board order should come from the index: {p}"
    );
    let p = plan(
        IssueFilter {
            project: Some("AG".into()),
            ..none()
        },
        None,
    );
    assert!(p.contains("issues_board"), "{p}");
    let p = plan(
        IssueFilter {
            assignee: Some("ann".into()),
            ..none()
        },
        None,
    );
    assert!(p.contains("issues_assignee"), "{p}");
    let p = plan(
        IssueFilter {
            parent: Some("AG-1".into()),
            ..none()
        },
        None,
    );
    assert!(p.contains("issues_parent"), "{p}");
    let p = plan(
        IssueFilter {
            sprint: Some("s".into()),
            ..none()
        },
        None,
    );
    assert!(p.contains("issues_sprint"), "{p}");
    let p = plan(
        IssueFilter {
            label: Some("rust".into()),
            ..none()
        },
        None,
    );
    assert!(
        p.contains("issue_labels"),
        "labels use their own table: {p}"
    );
    let p = plan(none(), Some("lease"));
    assert!(
        p.contains("issue_fts") && p.contains("VIRTUAL TABLE INDEX"),
        "full-text search uses the FTS index: {p}"
    );
    // No plan may full-scan the issues table for these filters.
    for f in [
        IssueFilter {
            project: Some("AG".into()),
            ..none()
        },
        IssueFilter {
            assignee: Some("a".into()),
            ..none()
        },
        IssueFilter {
            parent: Some("a".into()),
            ..none()
        },
    ] {
        let p = plan(f, None);
        assert!(!p.contains("SCAN i") && !p.contains("SCAN issues"), "{p}");
    }
}

// ---------------------------------------------------------------- migration

/// A database exactly as the previous release (schema 3) created it.
fn legacy_database(path: &Path, issues: usize) {
    let conn = Connection::open(path).unwrap();
    conn.execute_batch(
        "CREATE TABLE agents(id TEXT PRIMARY KEY, doc TEXT NOT NULL CHECK(json_valid(doc)));
         CREATE TABLE projects(id TEXT PRIMARY KEY, doc TEXT NOT NULL CHECK(json_valid(doc)));
         CREATE TABLE issues(id TEXT PRIMARY KEY, doc TEXT NOT NULL CHECK(json_valid(doc)));
         CREATE TABLE sprints(id TEXT PRIMARY KEY, doc TEXT NOT NULL CHECK(json_valid(doc)));
         CREATE TABLE counters(project TEXT PRIMARY KEY REFERENCES projects(id), value INTEGER NOT NULL);
         CREATE TABLE links(source TEXT REFERENCES issues(id),target TEXT REFERENCES issues(id),kind TEXT NOT NULL,PRIMARY KEY(source,target,kind),CHECK(source<>target));
         CREATE TABLE comments(id INTEGER PRIMARY KEY AUTOINCREMENT,issue TEXT NOT NULL REFERENCES issues(id),actor TEXT NOT NULL REFERENCES agents(id),body TEXT NOT NULL,created_at INTEGER NOT NULL);
         CREATE TABLE events(id INTEGER PRIMARY KEY AUTOINCREMENT,issue TEXT,actor TEXT NOT NULL,action TEXT NOT NULL,data TEXT NOT NULL,created_at INTEGER NOT NULL);
         CREATE INDEX comments_issue ON comments(issue,id);
         CREATE INDEX events_issue ON events(issue,id);
         CREATE INDEX issues_project ON issues(json_extract(doc,'$.project'));
         CREATE TABLE api_keys(id TEXT PRIMARY KEY,agent TEXT NOT NULL REFERENCES agents(id),label TEXT NOT NULL,key_hash TEXT NOT NULL UNIQUE,created_at INTEGER NOT NULL,revoked_at INTEGER);
         CREATE TABLE idempotency(actor TEXT NOT NULL,key TEXT NOT NULL,request_hash TEXT NOT NULL,response TEXT NOT NULL,created_at INTEGER NOT NULL,PRIMARY KEY(actor,key));
         CREATE TABLE webhooks(id TEXT PRIMARY KEY,doc TEXT NOT NULL CHECK(json_valid(doc)),secret TEXT NOT NULL);
         PRAGMA user_version=3;",
    )
    .unwrap();
    conn.execute("INSERT INTO agents VALUES('ann','{\"id\":\"ann\",\"name\":\"Ann\",\"capabilities\":[],\"active\":true}')", []).unwrap();
    conn.execute("INSERT INTO projects VALUES('AG','{\"key\":\"AG\",\"name\":\"Agents\",\"description\":\"\"}')", []).unwrap();
    for n in 1..=issues {
        let doc = json!({"key":format!("AG-{n}"),"project":"AG","kind":"task","title":format!("Legacy issue {n}"),
            "description":"carried over","acceptance_criteria":["old criterion"],"state":"ready","priority":"medium",
            "assignee":null,"parent":null,"sprint":null,"labels":["legacy"],"estimate":null,"metadata":{},
            "revision":1,"created_at":10,"updated_at":10,"lease":null});
        conn.execute(
            "INSERT INTO issues(id,doc) VALUES(?1,?2)",
            rusqlite::params![format!("AG-{n}"), doc.to_string()],
        )
        .unwrap();
    }
    conn.execute("INSERT INTO comments(issue,actor,body,created_at) VALUES('AG-2','ann','a pre-migration remark about pelicans',5)", []).unwrap();
}

#[test]
fn a_version_three_database_migrates_with_its_history_searchable() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("old.sqlite3");
    legacy_database(&path, 12);
    let mut s = Store::open(&path).unwrap();
    assert_eq!(
        keys(&run(
            &mut s,
            "ann",
            json!({"op":"issue_search","query":"pelicans"})
        )),
        ["AG-2"],
        "comments are back-filled"
    );
    assert_eq!(
        run(
            &mut s,
            "ann",
            json!({"op":"issue_search","query":"legacy","limit":1000})
        )["total"],
        12
    );
    assert_eq!(
        keys(&run(
            &mut s,
            "ann",
            json!({"op":"issue_search","query":"AG-7"})
        )),
        ["AG-7"]
    );
    assert_eq!(
        run(
            &mut s,
            "ann",
            json!({"op":"issue_list","filter":{"label":"legacy","limit":1000}})
        )["total"],
        12,
        "labels are back-filled"
    );
    assert_eq!(
        run(
            &mut s,
            "ann",
            json!({"op":"issue_list","filter":{"state":"ready","limit":1000}})
        )["total"],
        12
    );
    assert_eq!(
        keys(&run(
            &mut s,
            "ann",
            json!({"op":"issue_list","filter":{"limit":1000}})
        ))
        .len(),
        12
    );
    // Board order across the 9 -> 10 boundary is numeric, as before.
    let order = keys(&run(
        &mut s,
        "ann",
        json!({"op":"issue_list","filter":{"limit":1000}}),
    ));
    assert!(
        order.iter().position(|k| k == "AG-9") < order.iter().position(|k| k == "AG-10"),
        "numeric, not textual, key order"
    );
    drop(s);
    let conn = Connection::open(&path).unwrap();
    assert_eq!(
        conn.pragma_query_value(None, "user_version", |r| r.get::<_, i64>(0))
            .unwrap(),
        4
    );
    let old_index: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE name='issues_project'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(old_index, 0, "the superseded expression index is dropped");
    // Reopening is a no-op and adds nothing twice.
    let mut s = Store::open(&path).unwrap();
    assert_eq!(
        run(
            &mut s,
            "ann",
            json!({"op":"issue_search","query":"legacy","limit":1000})
        )["total"],
        12
    );
    let indexed: i64 = Connection::open(&path)
        .unwrap()
        .query_row("SELECT COUNT(*) FROM issue_fts", [], |r| r.get(0))
        .unwrap();
    assert_eq!(indexed, 12);
    // And work continues normally on the migrated board.
    let k = create(&mut s, "AG", "Created after migration", json!({}));
    assert_eq!(search(&mut s, "after migration"), [k.as_str()]);
}

#[test]
fn many_processes_opening_an_old_database_at_once_migrate_it_exactly_once() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("race.sqlite3");
    legacy_database(&path, 40);
    let gate = Arc::new(Barrier::new(8));
    let handles: Vec<_> = (0..8)
        .map(|_| {
            let (path, gate) = (path.clone(), gate.clone());
            std::thread::spawn(move || {
                gate.wait();
                Store::open(&path).map(|_| ()).map_err(|e| e.to_string())
            })
        })
        .collect();
    for h in handles {
        h.join()
            .unwrap()
            .expect("a concurrent opener failed instead of finding the work done");
    }
    let conn = Connection::open(&path).unwrap();
    let rows: i64 = conn
        .query_row("SELECT COUNT(*) FROM issue_fts", [], |r| r.get(0))
        .unwrap();
    let labels: i64 = conn
        .query_row("SELECT COUNT(*) FROM issue_labels", [], |r| r.get(0))
        .unwrap();
    assert_eq!((rows, labels), (40, 40), "no row may be indexed twice");
}

#[test]
fn indexes_stay_consistent_with_the_documents() {
    let (dir, mut s) = board();
    let mut keys_made = vec![];
    for n in 0..30 {
        let labels: Vec<_> = (0..(n % 3)).map(|i| format!("l{i}")).collect();
        keys_made.push(create(
            &mut s,
            "AG",
            &format!("item {n}"),
            json!({"labels":labels}),
        ));
    }
    for (n, k) in keys_made.iter().enumerate() {
        let rev = run(&mut s, "ann", json!({"op":"issue_get","key":k}))["issue"]["revision"]
            .as_i64()
            .unwrap();
        if n % 2 == 0 {
            run(
                &mut s,
                "ann",
                json!({"op":"issue_update","key":k,"expected_revision":rev,"labels":["x","y"],"title":format!("changed {n}")}),
            );
        }
        run(
            &mut s,
            "bob",
            json!({"op":"comment_add","key":k,"body":format!("remark {n}")}),
        );
    }
    let conn = Connection::open(dir.path().join("s.sqlite3")).unwrap();
    let issues: i64 = conn
        .query_row("SELECT COUNT(*) FROM issues", [], |r| r.get(0))
        .unwrap();
    let fts: i64 = conn
        .query_row("SELECT COUNT(*) FROM issue_fts", [], |r| r.get(0))
        .unwrap();
    assert_eq!((issues, fts), (30, 30), "exactly one search row per issue");
    // The label table equals what the documents say, no more and no less.
    let from_docs: i64 = conn.query_row("SELECT COUNT(*) FROM (SELECT DISTINCT i.id, j.value FROM issues i, json_each(i.doc,'$.labels') j)", [], |r| r.get(0)).unwrap();
    let in_table: i64 = conn
        .query_row("SELECT COUNT(*) FROM issue_labels", [], |r| r.get(0))
        .unwrap();
    assert_eq!(from_docs, in_table);
    let stray: i64 = conn.query_row("SELECT COUNT(*) FROM issue_labels l WHERE NOT EXISTS (SELECT 1 FROM issues i, json_each(i.doc,'$.labels') j WHERE i.id=l.issue AND j.value=l.label)", [], |r| r.get(0)).unwrap();
    assert_eq!(stray, 0);
}

#[test]
fn the_tool_schema_advertises_search_and_filters() {
    let (_d, s) = board();
    let mut server = mcp::Server::new(s, "ann".into());
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
    let schema = ask(&mut server, 2, "tools/list", json!({}))["tools"][0]["inputSchema"].clone();
    let ops: Vec<String> = schema["oneOf"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v["properties"]["op"]["const"].as_str().unwrap().to_owned())
        .collect();
    for op in [
        "issue_search",
        "filter_save",
        "filter_list",
        "filter_delete",
    ] {
        assert!(ops.contains(&op.to_owned()), "{op} missing from {ops:?}");
    }
    assert_eq!(ops.len(), 36);
    let reply = ask(
        &mut server,
        3,
        "tools/call",
        json!({"name":"workboard_execute","arguments":{"op":"issue_search","query":"nothing"}}),
    );
    assert_eq!(reply["isError"], false);
}
