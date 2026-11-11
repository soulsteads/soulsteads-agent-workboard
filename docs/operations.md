# Command reference

All commands are JSON objects containing `op`. Send the object to `workboard run`
or as MCP `workboard_execute` arguments. `workboard schema` provides the complete
machine-readable union, defaults and enum values. Unknown fields are rejected.
Strings use UTF-8. Timestamps are Unix seconds. Issue revisions begin at 1.

## Registry and planning

| Operation | Required fields besides `op` | Optional fields / result |
| --- | --- | --- |
| `agent_register` | `id`, `name` | `capabilities` string array; returns agent |
| `agent_set_active` | `id`, `active` | Returns agent |
| `agent_list` | — | All agents including inactive |
| `project_create` | `key`, `name` | `description`; returns project |
| `project_list` | — | All projects |
| `issue_create` | `project`, `kind`, `title` | `description`, `acceptance_criteria`, `assignee`, `parent`, `sprint`, `labels`, `estimate`, `priority`, `metadata`; returns issue |
| `issue_get` | `key` | Returns `issue`, incoming/outgoing `links`, unresolved `blockers` and direct `children` |
| `issue_update` | `key`, `expected_revision` | Editable fields below, `lease_token`; returns issue |
| `issue_assign` | `key`, `expected_revision` | `assignee` string or null/omitted to unassign; returns issue |

Agent IDs use 1–64 ASCII letters, digits, `_` or `-`. IDs are stable; duplicate
registration conflicts. An agent can bootstrap its own identity without an existing
registration. All other mutations require a registered active actor. Reads do not.
Registration and project creation conflict on duplicates rather than overwriting.
Project keys use 1–16 uppercase ASCII letters. Keys such as `AG-1` are allocated
transactionally, independently in each project.

Kinds: `epic`, `story`, `task`, `bug`, `subtask`, `spike`. Hierarchy is
`epic -> story/task/bug/spike -> subtask`. Subtasks require a parent. Other non-epic
kinds can stand alone. All parents must be in the same project. Open work cannot
be added to a terminal parent. Parentage is separate from explicit links.

Priority: `critical`, `high`, `medium` (default), `low`. Estimates are finite
nonnegative numbers. Metadata is a JSON object for artifact URLs, repository refs,
external correlations or other agent context. Labels are an array of at most 100
nonempty strings, each at most 100 bytes. Title, comment body and names are nonempty
and at most 100000 bytes. Description may be empty.

`issue_update` can replace `title`, `description`, `acceptance_criteria`, `priority`,
`labels`, `estimate`, `parent`, `sprint`, `metadata`. Arrays and metadata are replaced,
not merged. Omitted fields and null optional update fields remain unchanged.
`clear_estimate`, `clear_parent`, `clear_sprint` explicitly remove nullable values;
setting and clearing a field simultaneously is invalid. Key, project, kind, state,
assignee, timestamps and revisions cannot be changed through this operation.

## States and coordination

| Operation | Required fields besides `op` | Optional fields / result |
| --- | --- | --- |
| `workflow` | — | States and permitted transitions |
| `issue_transition` | `key`, `expected_revision`, `state` | `lease_token`; returns issue |
| `issue_claim` | `project` | `key`, `lease_seconds`; returns `{ "claimed": issue or null }` |
| `issue_heartbeat` | `key`, `lease_token` | `lease_seconds`; returns issue |
| `issue_release` | `key`, `lease_token` | Returns ready, unassigned issue |

| From | Permitted targets |
| --- | --- |
| `backlog` | `ready`, `canceled` |
| `ready` | `backlog`, `in_progress`, `blocked`, `canceled` |
| `in_progress` | `ready`, `blocked`, `in_review`, `done`, `canceled` |
| `blocked` | `ready`, `in_progress`, `backlog`, `canceled` |
| `in_review` | `in_progress`, `blocked`, `done`, `canceled` |
| `done`, `canceled` | `backlog` |

New issues start in backlog. `done` and `canceled` are terminal. Starting, reviewing
or completing non-epic work requires an active assignee and no unresolved direct
blockers. Epics need no assignee. Terminal transitions require all descendants
terminal. Reopen ancestors first. Move dependent issues out of `in_progress`,
`in_review` or `done` before reopening their blocker. Move issues out of completed
sprints before reopening them. These guards prevent impossible board states.

Edits, assignments and transitions require `expected_revision` from a current
issue. A stale revision returns `conflict` and changes nothing. Read again and
reconcile; do not blindly overwrite. Heartbeats also increment revisions.

Claim selects a ready non-epic issue with no unresolved direct blockers, assigned
to nobody or to the configured actor. Order is priority, creation time, issue
sequence. An optional key restricts selection. Missing/unavailable keys yield
`claimed: null`. Claim recovery, selection, assignment, transition to in-progress,
and token issuance are atomic across processes.

A claim lasts 900 seconds by default; `lease_seconds` accepts 1–86400. Only the
owning actor with the current unexpired token can heartbeat, release, edit or
transition a leased issue. Include `lease_token` alongside revisions for edits and
transitions. Heartbeat extends expiry from the current time. Tokens survive
in-review and in-progress; other transitions clear them. Release clears assignment
and returns work to ready. Expiry is recovered by the next project claim and emits
`claim_expired`, then a new claim gets a new token. An expired worker cannot renew
or submit, even if it reads the latest revision.

Explicit `issue_assign` is a coordinator override: it revokes a lease and returns
interrupted work to ready. Changing an unleased in-progress or in-review
assignment also returns that work to ready. Retired workers cannot mutate or receive new assignments.
Their history remains readable/editable by active agents. An unleased issue can
be managed with revisions alone; use claims for worker execution.

## Links, discussion and history

| Operation | Required fields besides `op` | Optional fields / result |
| --- | --- | --- |
| `link_add`, `link_remove` | `source`, `target`, `kind` | Returns link |
| `comment_add` | `key`, `body` | Returns immutable comment |
| `comment_list` | `key` | `after`, `limit`; comments in increasing ID order |
| `events` | — | `key`, `after`, `limit`; events in increasing ID order |

`blocks`: source must be terminal before target can start, review or finish.
`relates_to`: symmetric; endpoint order is canonicalized. `duplicates`: source
is a duplicate of target; informational, does not close or block either issue.
No self-links. Duplicate adds conflict, missing removes return `not_found`.
Blocking links plus the parent completion hierarchy must remain acyclic. An
unresolved blocker cannot be added to an in-progress, in-review or done target;
move the target to ready or blocked first. Links may span projects.

Every successful mutation appends an immutable event with actor, action, timestamp
and payload. Issue events include the complete resulting issue. Link changes are
recorded on both endpoints. Registry/sprint changes have no issue key. Comments
store agent evidence or handoff context and also emit events. `after` is an
exclusive monotonic ID cursor; save the last returned ID and poll again. `limit`
defaults to 100 and must be 1–1000. Empty results mean no further data currently.

## Queries and sprints

| Operation | Required fields besides `op` | Optional fields / result |
| --- | --- | --- |
| `issue_list` | — | `filter`; returns `items`, `total`, `next_offset` |
| `board` | `project` | `sprint`; all state columns, total and actionable issue keys |
| `epic_progress` | `key` | Epic plus all descendants, total, done, canceled, open |
| `sprint_create` | `project`, `name` | `goal`; returns sprint with UUID id |
| `sprint_list` | `project` | All project sprints |
| `sprint_transition` | `id`, `state` | Returns sprint |

Filter fields: `project`, `kind`, `state`, `assignee`, `unassigned` (bool), `parent`,
`sprint`, `label`, `text`, `actionable` (bool), `offset`, `limit`. Conditions are ANDed.
Text is a case-insensitive substring of key/title/description. Parent selects direct
children. Actionable means claimable based on state/kind/dependencies, before an
actor's assignment restriction; a board may therefore show work reserved for a
different agent. Pagination uses offsets, default 0, default limit 100, max 1000.
Sorting matches claim priority/order. `next_offset: null` ends the current snapshot.
Board and epic progress intentionally return complete snapshots.

Sprints move `planned -> active -> completed`, with only one active per project.
Epics span sprints; attach their children instead. Sprint membership must match the
issue project. Finish/cancel or move out all open issues before completing a
sprint. Completed sprint membership is historical; it cannot receive new issues,
and member issues must move out before reopening. Historical issue details can
still be edited.

## Error handling

Domain codes: `invalid` (fields/ranges), `not_found` (missing entity), `conflict`
(revision, workflow, lease, duplicate or lifecycle guard), `internal` (storage/JSON
failure). CLI parse errors use `invalid`; CLI input I/O errors use `io`. Inspect the
message for context. Rejected mutations roll back entirely. Reads return a
transactionally consistent snapshot. Commands do not offer request idempotency;
inspect state/history after a transport failure before retrying mutations.

MCP tool domain errors use `isError: true`, not JSON-RPC errors. Invalid JSON-RPC,
unknown methods and malformed RPC parameters use protocol errors. Notifications
produce no response. JSON-RPC batches are unsupported, as required by current MCP.
Input frames are limited to 8 MiB. Oversized MCP frames terminate the transport;
reconnect and reconcile state before retrying work.
