# Validation and requirement coverage

The implementation exposes one Rust command engine through the compiled JSON CLI
and MCP stdio server. The following checks are reproducible from the repository:

```bash
cargo fmt --check
cargo clippy --locked --all-targets -- -D warnings
cargo test --locked
cargo build --locked --release
uv run --locked scripts/validate_mcp.py target/release/workboard
```

Validated on Linux with Rust/Cargo 1.99.1. The Rust suite has 14 integration tests
(10 domain tests and 4 interface/process tests). Development dependencies for the
SDK validator are pinned in `scripts/validate_mcp.py.lock`; Rust dependencies are
pinned in `Cargo.lock`. Runtime code and both interfaces are Rust; Python is used
only for independent SDK interoperability validation.

| Requested capability | Authoritative implementation | Observable validation |
| --- | --- | --- |
| Rust end-to-end implementation | `src/model.rs`, `src/store.rs`, `src/main.rs`, `src/mcp.rs` | Locked release build, compiled CLI/MCP process tests, SDK validator against release binary |
| Epics, user stories, bugs and related types | `IssueKind`, issue creation and parent validation | `full_epic_story_bug_subtask_workflow_and_persistence` creates epic/story/bug/subtask; `sprints_filters_pagination_and_agent_lifecycle` exercises task/spike |
| Issue linking | Link operations and combined completion graph | `dependencies_cycles_links_and_claim_selection` checks blockers, related links, self-links, cycles and selection; `completion_cycles_include_hierarchy_and_reopened_blockers` checks hierarchy deadlocks and reopen guards |
| Assignment to agents | Registry, active-agent checks, assignment and claims | `leases_fence_stale_workers_renew_release_and_reassignment`, `sprints_filters_pagination_and_agent_lifecycle`, `blocking_canceling_unassigning_and_incremental_handoffs` |
| Issue states | `State::permits` and transition guards | Full epic workflow; blocked/canceled workflow and unassigned-start rejection; invalid shortcuts and descendant/reopen guards |
| Skill access | `skills/agent-workboard/SKILL.md` | Skill-creator frontmatter validation, documented CLI syntax, compiled CLI tests and SDK/CLI shared-database work loop |
| MCP access | JSON-RPC stdio server, tool, resources and prompt | `mcp_stdio_handshake_tools_resources_prompts_and_errors`, malformed/oversized frame test, independent official Python MCP SDK validator |
| Agent concurrency and recovery | SQLite immediate transactions, revisions and fenced expiring leases | Eight competing **processes** claim one issue exactly once; stale edits rejected; expiry gets a new token and audit event; old tokens rejected |
| Durable board and handoff state | SQLite WAL, audit events, comments, query operations | Close/reopen database and separate CLI/MCP processes read persisted work; exclusive comment/event cursors checked |
| Sprints and planning detail | Sprint state/membership guards, metadata, acceptance criteria, priorities, estimates, labels and filters | Sprint lifecycle, completed history, priority selection, pagination, rollback and cross-project tests |
| Tested and validated delivery | Tests, locked SDK validator, build/lint checks and packaged skill | Commands above; installation smoke test; Git commits and remote branch equality at delivery |

The official SDK validator negotiates the protocol, validates the advertised JSON
Schema with an independent validator, creates an epic/story/bug dependency chain,
rejects a blocked claim, completes the dependency, claims/renews/edits/reviews/
completes the story, closes the epic, reads all resource forms and the prompt,
checks domain/invalid-command errors, pings and confirms persisted results via CLI.
It uses a temporary database, so validation does not alter an operator's board.

Expiry tests set the stored expiry to a past timestamp to simulate elapsed time
without sleeps. Process concurrency tests start eight workers behind a barrier.
The suite checks rollback of both state and audit events, and refuses unknown
future database versions.

Operating boundaries are explicit in the README: trusted agents on one host,
stdio MCP, local SQLite storage, direct blocking semantics and no automatic
mutation deduplication. A GitHub Actions workflow supplies the same build, lint,
test and SDK checks; remote runner execution depends on the hosting service.
