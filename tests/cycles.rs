//! The completion-cycle guard looks only at the issues connected to a new edge.
//! These tests check that it makes exactly the decisions of the original check,
//! which examined the entire board: build every edge, then topologically sort.
use agent_workboard::{Command, Error, Store};
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};

fn cmd(value: Value) -> Command {
    serde_json::from_value(value).unwrap()
}

/// The original algorithm: Kahn's algorithm over every edge on the board.
/// Edges are child -> parent and blocker -> blocked.
fn has_cycle(
    parents: &BTreeMap<String, Option<String>>,
    blocks: &BTreeSet<(String, String)>,
) -> bool {
    let mut edges: Vec<(&str, &str)> = parents
        .iter()
        .filter_map(|(child, parent)| parent.as_deref().map(|p| (child.as_str(), p)))
        .collect();
    edges.extend(blocks.iter().map(|(a, b)| (a.as_str(), b.as_str())));
    let mut remaining: BTreeSet<&str> = parents.keys().map(String::as_str).collect();
    while !remaining.is_empty() {
        let roots: Vec<&str> = remaining
            .iter()
            .copied()
            .filter(|n| {
                !edges
                    .iter()
                    .any(|(from, to)| to == n && remaining.contains(from))
            })
            .collect();
        if roots.is_empty() {
            return true;
        }
        for root in roots {
            remaining.remove(root);
        }
    }
    false
}

struct Rng(u64);
impl Rng {
    fn next(&mut self, n: usize) -> usize {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        (self.0 % n as u64) as usize
    }
}

#[derive(Default)]
struct Tally {
    accepted: usize,
    cycles_rejected: usize,
    other_rejections: usize,
}

fn is_cycle_rejection(error: &Error) -> bool {
    matches!(error, Error::Conflict(m) if m.contains("completion cycle"))
}

fn play(seed: u64, steps: usize) -> Tally {
    let dir = tempfile::tempdir().unwrap();
    let mut store = Store::open(dir.path().join("cycles.sqlite3")).unwrap();
    let run = |store: &mut Store, v: Value| store.execute("a", cmd(v));
    run(
        &mut store,
        json!({"op":"agent_register","id":"a","name":"A"}),
    )
    .unwrap();
    run(
        &mut store,
        json!({"op":"project_create","key":"AG","name":"Agents"}),
    )
    .unwrap();

    // 3 epics, then stories, tasks and bugs under them, then subtasks. Parents are
    // chosen by the random walk below, so reparenting really does change structure.
    let mut kinds: BTreeMap<String, &str> = BTreeMap::new();
    let mut parents: BTreeMap<String, Option<String>> = BTreeMap::new();
    let plan: &[(&str, usize)] = &[
        ("epic", 3),
        ("story", 5),
        ("task", 4),
        ("bug", 3),
        ("subtask", 5),
    ];
    let mut rng = Rng(seed);
    for (kind, count) in plan {
        for _ in 0..*count {
            let candidates: Vec<&String> = parents
                .keys()
                .filter(|k| match *kind {
                    "epic" => false,
                    "subtask" => matches!(kinds[*k], "story" | "task" | "bug"),
                    _ => kinds[*k] == "epic",
                })
                .collect();
            let parent =
                (!candidates.is_empty()).then(|| candidates[rng.next(candidates.len())].clone());
            let mut c = json!({"op":"issue_create","project":"AG","kind":kind,"title":"n"});
            if let Some(p) = &parent {
                c["parent"] = json!(p);
            }
            let key = run(&mut store, c).unwrap()["key"]
                .as_str()
                .unwrap()
                .to_owned();
            kinds.insert(key.clone(), kind);
            parents.insert(key, parent);
        }
    }
    let all: Vec<String> = parents.keys().cloned().collect();
    let mut blocks: BTreeSet<(String, String)> = BTreeSet::new();
    let mut tally = Tally::default();
    assert!(
        !has_cycle(&parents, &blocks),
        "the starting board must be acyclic"
    );

    for _ in 0..steps {
        match rng.next(5) {
            // Add a blocking link between two random issues.
            0..=2 => {
                let (a, b) = (
                    all[rng.next(all.len())].clone(),
                    all[rng.next(all.len())].clone(),
                );
                let outcome = run(
                    &mut store,
                    json!({"op":"link_add","source":a,"target":b,"kind":"blocks"}),
                );
                let mut after = blocks.clone();
                after.insert((a.clone(), b.clone()));
                let cyclic = has_cycle(&parents, &after);
                match outcome {
                    Ok(_) => {
                        assert!(
                            !cyclic,
                            "accepted {a} blocks {b}, which the original check rejects"
                        );
                        blocks = after;
                        tally.accepted += 1;
                    }
                    Err(e) if is_cycle_rejection(&e) => {
                        assert!(
                            cyclic,
                            "rejected {a} blocks {b}, which the original check accepts"
                        );
                        tally.cycles_rejected += 1;
                    }
                    Err(_) => tally.other_rejections += 1,
                }
            }
            // Remove a link, so the board keeps changing shape.
            3 => {
                if let Some((a, b)) = blocks.iter().nth(rng.next(blocks.len().max(1))).cloned() {
                    run(
                        &mut store,
                        json!({"op":"link_remove","source":a,"target":b,"kind":"blocks"}),
                    )
                    .unwrap();
                    blocks.remove(&(a, b));
                }
            }
            // Move an issue under a different parent.
            _ => {
                let key = all[rng.next(all.len())].clone();
                let new_parent = all[rng.next(all.len())].clone();
                let revision = run(&mut store, json!({"op":"issue_get","key":key})).unwrap()["issue"]["revision"].as_i64().unwrap();
                let outcome = run(
                    &mut store,
                    json!({"op":"issue_update","key":key,"expected_revision":revision,"parent":new_parent}),
                );
                let mut after = parents.clone();
                after.insert(key.clone(), Some(new_parent.clone()));
                let cyclic = has_cycle(&after, &blocks);
                match outcome {
                    Ok(_) => {
                        assert!(
                            !cyclic,
                            "accepted {key} under {new_parent}, which the original check rejects"
                        );
                        parents = after;
                        tally.accepted += 1;
                    }
                    Err(e) if is_cycle_rejection(&e) => {
                        assert!(
                            cyclic,
                            "rejected {key} under {new_parent}, which the original check accepts"
                        );
                        tally.cycles_rejected += 1;
                    }
                    // Hierarchy rules (wrong parent kind and so on) fire before the cycle check.
                    Err(_) => tally.other_rejections += 1,
                }
            }
        }
    }
    tally
}

#[test]
fn the_local_cycle_check_agrees_with_the_whole_board_check_on_random_histories() {
    let (mut accepted, mut cycles) = (0, 0);
    for seed in 1..=5u64 {
        let tally = play(seed.wrapping_mul(0x9E3779B97F4A7C15) | 1, 200);
        accepted += tally.accepted;
        cycles += tally.cycles_rejected;
    }
    // A test that only ever accepted, or only ever rejected, would prove nothing.
    assert!(accepted > 150, "only {accepted} accepted writes");
    assert!(cycles > 60, "only {cycles} cycle rejections");
}

#[test]
fn an_epic_cannot_block_its_own_descendant_even_through_other_links() {
    let dir = tempfile::tempdir().unwrap();
    let mut s = Store::open(dir.path().join("c.sqlite3")).unwrap();
    let run = |s: &mut Store, v: Value| s.execute("a", cmd(v));
    run(&mut s, json!({"op":"agent_register","id":"a","name":"A"})).unwrap();
    run(
        &mut s,
        json!({"op":"project_create","key":"AG","name":"Agents"}),
    )
    .unwrap();
    run(
        &mut s,
        json!({"op":"issue_create","project":"AG","kind":"epic","title":"epic"}),
    )
    .unwrap();
    for n in 2..=3 {
        let c =
            json!({"op":"issue_create","project":"AG","kind":"story","title":"s","parent":"AG-1"});
        assert_eq!(run(&mut s, c).unwrap()["key"], format!("AG-{n}"));
    }
    // Direct: the epic cannot be blocked by what it contains, in either direction it would never finish.
    let e = run(
        &mut s,
        json!({"op":"link_add","source":"AG-1","target":"AG-2","kind":"blocks"}),
    )
    .unwrap_err();
    assert!(is_cycle_rejection(&e), "{e}");
    // Indirect, through a chain of unrelated-looking links.
    run(
        &mut s,
        json!({"op":"link_add","source":"AG-2","target":"AG-3","kind":"blocks"}),
    )
    .unwrap();
    assert!(is_cycle_rejection(
        &run(
            &mut s,
            json!({"op":"link_add","source":"AG-3","target":"AG-2","kind":"blocks"})
        )
        .unwrap_err()
    ));
    // A harmless link elsewhere is still fine, and relates_to never forms a completion edge.
    run(
        &mut s,
        json!({"op":"link_add","source":"AG-3","target":"AG-1","kind":"relates_to"}),
    )
    .unwrap();
}

/// A board of `n` issues: one epic and `n - 1` stories under it, inserted as raw
/// documents so that building even a large one is quick.
fn board_of(n: usize) -> (tempfile::TempDir, Store) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("big.sqlite3");
    let mut s = Store::open(&path).unwrap();
    let run = |s: &mut Store, v: Value| s.execute("a", cmd(v)).unwrap();
    run(&mut s, json!({"op":"agent_register","id":"a","name":"A"}));
    run(
        &mut s,
        json!({"op":"project_create","key":"AG","name":"Agents"}),
    );
    run(
        &mut s,
        json!({"op":"issue_create","project":"AG","kind":"epic","title":"epic"}),
    );
    let conn = rusqlite::Connection::open(&path).unwrap();
    conn.execute_batch("BEGIN").unwrap();
    for i in 2..=n {
        let key = format!("AG-{i}");
        let doc = json!({"key":key,"project":"AG","kind":"story","title":"t","description":"","acceptance_criteria":[],
            "state":"backlog","priority":"medium","assignee":null,"parent":"AG-1","sprint":null,"labels":[],"estimate":null,
            "metadata":{},"revision":1,"created_at":1,"updated_at":1,"lease":null});
        conn.execute(
            "INSERT INTO issues(id,doc) VALUES(?1,?2)",
            rusqlite::params![key, doc.to_string()],
        )
        .unwrap();
    }
    conn.execute(
        "UPDATE counters SET value=?1 WHERE project='AG'",
        [n as i64],
    )
    .unwrap();
    conn.execute_batch("COMMIT").unwrap();
    drop(conn);
    (dir, s)
}
/// The fastest of `samples` issue creations, which ignores spikes from a busy machine.
fn best_create(s: &mut Store, samples: usize) -> std::time::Duration {
    (0..samples)
        .map(|_| {
            let started = std::time::Instant::now();
            s.execute("a", cmd(json!({"op":"issue_create","project":"AG","kind":"story","title":"new","parent":"AG-1"}))).unwrap();
            started.elapsed()
        })
        .min()
        .unwrap()
}

#[test]
fn the_cost_of_a_write_does_not_grow_with_the_size_of_the_board() {
    // Wall-clock limits fail on a busy machine and prove little. Compare the same
    // operation on a small and a large board in one run instead: they slow down
    // together under load, so only growth with board size changes the ratio. The whole-board
    // check cost about 100 times more on 6,000 issues than on 100.
    let (_small_dir, mut small) = board_of(100);
    let (_large_dir, mut large) = board_of(6000);
    let (small_best, large_best) = (best_create(&mut small, 15), best_create(&mut large, 15));
    let slack = std::time::Duration::from_millis(10);
    assert!(
        large_best < small_best * 8 + slack,
        "a create took {large_best:?} on 6,000 issues but {small_best:?} on 100"
    );
    // Linking walks only what the new edge touches, too.
    let started = std::time::Instant::now();
    large
        .execute(
            "a",
            cmd(json!({"op":"link_add","source":"AG-2","target":"AG-3","kind":"blocks"})),
        )
        .unwrap();
    let link = started.elapsed();
    assert!(link < small_best * 8 + slack * 5, "a link took {link:?}");
}
