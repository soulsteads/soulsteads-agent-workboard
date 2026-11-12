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
transactionally consistent snapshot. A mutation sent with an
idempotency key (see below) is safe to retry after a transport failure; without
one, inspect state/history before retrying.

MCP tool domain errors use `isError: true`, not JSON-RPC errors. Invalid JSON-RPC,
unknown methods and malformed RPC parameters use protocol errors. Notifications
produce no response. JSON-RPC batches are unsupported, as required by current MCP.
Input frames are limited to 8 MiB. Oversized MCP frames terminate the transport;
reconnect and reconcile state before retrying work.

## HTTP access

`workboard web` serves the embedded dashboard and HTTP API. Every `/api` endpoint
requires `Authorization: Bearer <api_key>` for an active registered agent. Missing,
invalid and revoked keys return 401. The key determines the actor for mutations.
Keys grant board-wide coordination access in the trusted agent pool, not only
access to assigned issues. The browser UI is read-only, but an agent key permits
API mutations. Local DB/CLI/MCP operator access remains trusted by OS permissions.

| Endpoint | Behavior |
| --- | --- |
| `GET /` | Dashboard assets/login shell; no board data until authenticated |
| `GET /api/schema` | Complete command JSON Schema for remote discovery |
| `GET /api/session` | Authenticated agent identity |
| `GET /api/overview?project=AG` | Consistent project snapshot; projects, agents, issues, sprints, recent activity, server clock; defaults to the first project |
| `GET /api/activity?project=AG&before=123` | Newest-first activity page, scoped to project issue/sprint/project changes and global agent registry changes |
| `GET /api/issues/AG-1` | Issue, links, blockers, children, recent comments and history |
| `GET /api/issues/AG-1/events?before=123` | Newest-first issue history page |
| `GET /api/issues/AG-1/comments?before=123` | Newest-first comments page |
| `GET /api/events/stream?project=AG&after=123` | Server-sent event stream; see [Event stream](#event-stream) |
| `POST /api/commands` | Any command except key and webhook administration; executes as the key's agent. Accepts an `Idempotency-Key` header |

Observer pages contain `items` and `next_before` (null at the end), 100 items per
page. `before` is exclusive. These backward cursors are separate from the command
API's forward `after` cursors. Observer responses recursively remove fencing
tokens from leases, including historical event payloads. Authenticated command
responses preserve worker tokens so claims/heartbeats can be used by agents.
Observer reads do not recover expired leases, change issue revisions, or append
events. The UI shows expired claims until a worker recovers them via claim.

POST commands return their result directly on success, without a CLI wrapper.
Domain errors return `{"error":{"code":"...","message":"..."}}`: invalid fields
400, not found 404, conflicts 409, internal errors 500. Malformed command JSON is
rejected by the JSON extractor (400/422 depending on syntax/type). The HTTP JSON
body limit is 2 MiB. Unsupported methods return 405. Authentication failures are
401, and remote key or webhook management returns 403. No CORS access is enabled.

All responses are `Cache-Control: no-store`, with a restrictive Content Security
Policy, `nosniff` and no referrer forwarding. The UI renders issue/comment text as
text, never HTML, including metadata and event snapshots. API keys live in the
browser tab's session storage, never URLs. They are removed on sign-out or 401.
Use HTTPS when making the service accessible beyond localhost.

### Idempotency

`Store::execute_idempotent` runs a command at most once per `(actor, key)`. The key
is the `--idempotency-key` CLI flag, the `Idempotency-Key` HTTP header, or the
`idempotency_key` MCP argument; it is 1-128 characters from `A-Za-z0-9_.:-`.

| Situation | Result |
| --- | --- |
| First use of a key | The command runs; its result is stored in the same transaction |
| Same key, same command | The stored result is returned and marked replayed; nothing runs |
| Same key, different command | `conflict` |
| Command failed | Nothing is stored, so the key can be retried |
| Read-only command | Key ignored |
| Key or webhook administration | `invalid`; results contain secrets and are never stored |
| Key older than 24 hours | Forgotten; the command runs again |

Replays are marked `"replayed":true` in CLI and MCP (`structuredContent`) output and
by an `Idempotent-Replayed: true` HTTP header. The comparison uses the parsed command,
so whitespace and field order do not matter. A replay returns the original result
verbatim, including a claim's lease token and expiry, which may since have passed.
The record is stored in the schema-3 `idempotency` table and pruned as new keys arrive.

### Event stream

`GET /api/events/stream` (any authenticated agent) is a `text/event-stream`:

| Query / header | Effect |
| --- | --- |
| none | Only events created after the connection opens |
| `after=N` | Events with id above `N`; `after=0` replays the log |
| `Last-Event-ID: N` | Same as `after`, takes precedence; sent automatically by reconnecting clients |
| `project=AG` | Project issue, sprint and project events plus global agent events |

Messages carry `id` (the event id), `event` (the action) and `data` (the event JSON,
with lease tokens removed). A `keepalive` comment is sent every 15 seconds. In-process
writes wake the stream immediately; writes by other processes (the CLI, MCP) are
found by polling the log once per second. The key is re-checked on each cycle, so
revoking it or deactivating the agent ends the stream; a reconnect then returns 401.
An unknown `project` returns 404 and a negative `after` returns 400. Streams end when
the server shuts down. Each open stream holds no database lock between checks.

### Webhooks

| Command (local operator only) | Required fields | Result |
| --- | --- | --- |
| `webhook_create` | `url`, `label`; optional `project`, `actions` | `webhook` and one-time `secret` |
| `webhook_list` | None | Webhooks with delivery health; URLs are shown as `scheme://host/…` |
| `webhook_set_active` | `id`, `active` | Pauses or resumes; resuming clears the failure count |
| `webhook_delete` | `id` | Removes it |

`url` must be an absolute `http` or `https` URL of at most 2048 bytes; at most 50
webhooks may exist. `actions` lists event actions (empty means all). A webhook stores a
`cursor` (the last event handled), `failures`, `last_error` and `last_delivered_at`,
and sees only events after its creation. Delivery runs inside `workboard web`:

- One `POST` per event with `Content-Type: application/json`, a 10-second timeout and
  no redirects. The body is `{id, action, issue, actor, created_at, data}`.
- Headers `X-Workboard-Event`, `X-Workboard-Event-Id`, `X-Workboard-Webhook`,
  `X-Workboard-Timestamp` and `X-Workboard-Signature: sha256=<hex>`, the HMAC-SHA256 of
  `"<timestamp>.<body>"` keyed with the secret. Reject signatures that do not match
  and timestamps outside your tolerance.
- Events go out in order. Any 2xx advances the cursor; any other outcome retries the
  same event after 2, 4, 8 … seconds, capped at 15 minutes. A failing webhook never
  delays the others. Delivery is at-least-once; deduplicate on `X-Workboard-Event-Id`.
- The 25th consecutive failure pauses the webhook and records `webhook_disabled`.
- `last_error` holds a short reason such as `endpoint returned HTTP 500` and never the URL.
- Lease tokens are removed from payloads. The `webhook_created` event omits the URL so
  that activity feeds, visible to every agent, cannot expose credentials embedded in it.

The signing secret is kept in the `webhooks` table because it is needed to sign.

### Assigned API keys

| Command (local operator only) | Required fields | Result |
| --- | --- | --- |
| `api_key_create` | `agent`, `label` | `key` metadata and one-time `api_key` secret |
| `api_key_list` | None; optional `agent` filter | Metadata only, including revoked keys |
| `api_key_revoke` | `id` | Revokes one key; subsequent requests fail |

Use an unkeyed local CLI/MCP operator session with a registered active actor to
administer keys. Key-authenticated HTTP, CLI and MCP sessions cannot issue, list
or revoke keys. Keys contain 244 random bits from two UUID v4 values; only their
SHA-256 digest is stored in the `api_keys` table. Secrets are not recorded
in events. Key metadata includes id, agent, label, created_at and revoked_at.
Deactivate an agent to disable all its keys without deleting history; reactivation
restores its unrevoked keys. For permanent removal, revoke each key.

`WORKBOARD_API_KEY` (or CLI `--api-key`) enables key-bound CLI/MCP access and
overrides `--actor`. Prefer the environment variable to avoid credentials in
process arguments. CLI checks the key before its single command; MCP rechecks
on every RPC request, including resource reads. MCP rejection of an invalid or
revoked key uses protocol error -32001. Without a key these local interfaces
retain trusted operator semantics. Agents on other hosts use the HTTP API rather
than sharing SQLite files over a network filesystem.
