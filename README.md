# Soulsteads Agent Workboard

A Rust issue board for autonomous agents. A JSON CLI, MCP stdio server,
authenticated HTTP API and live web dashboard
share one durable SQLite database. There are 28 command operations (key
administration is local-only). Agents can plan epics,
write stories, triage bugs, link dependencies, claim work, record evidence, and
hand work off while you follow progress in a read-only web view.

- Projects with monotonic issue keys; epics, stories, tasks, bugs, subtasks, spikes.
- Acceptance criteria, priorities, estimates, labels, arbitrary JSON metadata.
- Parent hierarchy, blocking dependencies, related issues and duplicate links.
- Registered agents, capabilities, assignment, retirement and reactivation.
- Seven guarded states; board columns, filtered search and epic progress.
- Atomic work claims, expiring leases, heartbeats and stale-worker fencing.
- Revision checks for edits, immutable audit events and comments attached to issues.
- Sprints with goals, one active sprint per project and completion checks.
- Live responsive web board, agent workload, issue details and activity feed.
- Per-agent API keys, hashed storage, revocation and authenticated HTTP access.
- Packaged [agent skill](skills/agent-workboard/SKILL.md), MCP tools, resources and prompts.

## Install and bootstrap

Install Rust's stable toolchain and a C compiler (SQLite is bundled). Then:

```bash
cargo install --locked --path .
export WORKBOARD_DB="$PWD/workboard.sqlite3"
export WORKBOARD_ACTOR="worker-1"
workboard run '{"op":"agent_register","id":"worker-1","name":"Rust worker","capabilities":["rust","tests"]}'
workboard run '{"op":"project_create","key":"AG","name":"Agent platform"}'
workboard run '{"op":"issue_create","project":"AG","kind":"epic","title":"Ship agent coordination"}'
workboard run '{"op":"issue_create","project":"AG","kind":"story","title":"Implement work loop","parent":"AG-1","acceptance_criteria":["Integration tests pass"]}'
workboard run '{"op":"issue_transition","key":"AG-2","expected_revision":1,"state":"ready"}'
workboard run '{"op":"issue_claim","project":"AG","lease_seconds":900}'
```

Retain the returned `claimed.lease.token` and `claimed.revision`. Read context with
`issue_get`, heartbeat before expiry, add validation evidence with `comment_add`,
and transition through `in_review` to `done` with the latest revision and token.
Each successful edit or heartbeat returns a new revision. `claimed: null` means
no eligible work. Epics are coordinated explicitly; workers claim their children.

`workboard run` also reads a command from stdin or `-`, so agents need not shell
escape large JSON bodies. Success is `{"ok":true,"result":...}`; failure is
`{"ok":false,"error":{"code":"...","message":"..."}}` with exit status 1.
Flags `--db` and `--actor` override the environment. Defaults are
`workboard.sqlite3` in the working directory and actor `agent`. Use absolute DB
paths when multiple agents share a board. The database's parent directory must
already exist. `workboard schema` emits the complete JSON Schema without opening
a database. Unknown fields are rejected to catch misspelled commands.

## Web dashboard and agent API keys

After registering your agents, issue each agent a key from a trusted local
operator shell (with `WORKBOARD_API_KEY` unset):

```bash
workboard --actor worker-1 run '{"op":"api_key_create","agent":"worker-1","label":"worker-1 workstation"}'
workboard --db /absolute/path/board.sqlite3 web
```

Save the returned `api_key` privately; it is shown once. Open
**http://127.0.0.1:7878** and enter an assigned key. The dashboard shows a live
Kanban board, project/agent/type/sprint filters, agent workloads, an activity feed,
and issue details with relationships, acceptance criteria, comments and history.
It refreshes every three seconds and adapts to phone screens. The UI is read-only;
agents update work through the API, CLI or MCP. Leases appear with countdowns,
not fencing tokens. Opening the dashboard does not change issue or lease state.

The key is kept in this browser tab's session storage and removed on sign-out.
Missing, invalid or revoked keys, and keys belonging to inactive agents, cannot
read any API data. A revoked session returns to the login screen on its next
request. There is no registration or key-issuance form exposed on the website.

Keys are bound to a registered agent. `api_key_list` (optional `agent`) returns
metadata, never secrets or hashes. `api_key_revoke` accepts the key metadata `id`.
Rotate by issuing a new key, switching clients, then revoking the old key. Multiple
keys per agent allow independent access and revocation. Secrets are generated
from OS-backed randomness and only SHA-256 hashes are stored.

Remote agents can submit the same command objects over HTTP. Set
`WORKBOARD_URL` to the board server and `WORKBOARD_API_KEY` to that agent's key:

```bash
curl --fail-with-body --silent --show-error "$WORKBOARD_URL/api/commands" \
  -H "Authorization: Bearer $WORKBOARD_API_KEY" \
  -H 'Content-Type: application/json' \
  --data '{"op":"issue_claim","project":"AG","lease_seconds":900}'
```

Successful HTTP commands return their result directly (without the CLI's
`ok`/`result` wrapper). Errors return `error.code` and `error.message` with an HTTP
error status. Agent identity is derived from the key; caller-supplied `actor`
fields are rejected. Key management is forbidden on the HTTP API. See
[HTTP endpoints and access rules](docs/operations.md#http-access).

`--listen 127.0.0.1:7878` is the default. For another local port, use
`workboard web --listen 127.0.0.1:8080`. To serve remote agents, bind an appropriate
interface and put the service behind an HTTPS reverse proxy. Keep credentials
out of URLs/logs and do not send real keys over unencrypted remote connections.
Assets are embedded in the Rust binary; there is no frontend build or Node.js
runtime dependency.

## MCP

Launch `workboard --db /absolute/path/board.sqlite3 --actor worker-1 mcp` from your
MCP client. For key-bound identity, set `WORKBOARD_API_KEY` in that process's
environment; it overrides `--actor`. Example client configuration (replace paths
and the key):

```json
{
  "mcpServers": {
    "agent-workboard": {
      "command": "/absolute/path/to/workboard",
      "args": ["--db", "/absolute/path/board.sqlite3", "mcp"],
      "env": {"WORKBOARD_API_KEY": "your-issued-key"}
    }
  }
}
```

The `workboard_execute` tool takes the command object directly, for example
`{"op":"board","project":"AG"}`. Its input schema describes every operation.
Success returns both JSON text and `structuredContent.result`; domain failures
return `isError: true` and `structuredContent.error`. No diagnostics appear on
protocol stdout. Register the configured actor before mutations, through either
interface. Use a separate server process and actor ID for each agent, all pointing
to the same DB. Keyed MCP sessions recheck key validity on every request, so
revocation takes effect without restarting the process. Key administration is
available only to unkeyed local operator sessions.

Resources: `workboard://workflow`, `workboard://projects`, `workboard://agents`,
`workboard://issues/{key}`, `workboard://projects/{key}/board`. The `work-issue`
prompt accepts a `project` argument and supplies the leased work loop.

The transport is newline-delimited JSON-RPC over stdio. It negotiates MCP
2024-11-05, 2025-03-26, 2025-06-18 and 2025-11-25; newer clients receive the latest
supported version. It supports tools, resources, templates, prompts and ping.
MCP itself requires no network listener; the optional `web` command serves the
dashboard and authenticated HTTP API.

## Skill

Copy the packaged directory into your agent's skill directory, for example:

```bash
mkdir -p "$HOME/.codex/skills"
cp -R skills/agent-workboard "$HOME/.codex/skills/"
```

Install `workboard` on PATH and give that agent `WORKBOARD_DB` and
`WORKBOARD_API_KEY` for key-bound identity (or `WORKBOARD_ACTOR` in a trusted
local operator session). Remote workers can instead use `WORKBOARD_URL` and
`WORKBOARD_API_KEY` with the HTTP API. The skill is self-contained and explains
discovery, planning,
claiming, renewal, revision conflicts and handoffs. It uses the JSON CLI;
MCP clients can use the same command objects.

## Operations and rules

See [the operation reference](docs/operations.md) for fields, workflow and
coordination rules. The generated schema is authoritative for JSON types.

This is a shared workspace for **trusted cooperating agents**. HTTP requests
and browser data access always require a per-agent API key. Key-authenticated
CLI/MCP sessions also derive identity from the key. Unkeyed CLI/MCP access is
retained for trusted local operators who already have direct database access;
actor IDs alone are not network credentials. Any active agent may coordinate
assignments and planning. Explicit reassignment revokes an existing claim and
returns interrupted work to ready. All participants with DB access can read board
content and claim tokens. Use OS permissions to control access. SQLite WAL must
reside on local storage; do not share its files over a network filesystem.

Every command runs in a transaction with a 10-second busy timeout. Failed
commands leave neither partial changes nor audit events. Successful mutations
are audited in the same transaction. Workers recover expired leases when the
next `issue_claim` runs for that project; no scheduler is required. The board
shows stored states until recovery. Blocking links are explicit and do not
inherit through the hierarchy. Canceled blockers count as resolved; canceling
a parent requires all descendants to be closed or canceled first.

Commands are not automatically retried or deduplicated. After a lost response,
inspect current state/events before retrying a creation, comment or claim.
Issue-list offsets are intended for browsing, not stable incremental sync; use
monotonic event IDs for synchronization. Board and epic views return complete
project/epic snapshots; filtering currently evaluates stored documents in memory.
This implementation targets coordination boards that fit on one host.

## Storage and recovery

Schema version 2 is initialized automatically; version 1 databases are migrated
in place without losing work. A newer database version is refused.
Data survives process restarts. To back up an active DB, use SQLite's online backup
API (or the `sqlite3` CLI's `.backup` command). To copy database files directly,
stop all writers first and preserve the DB and any WAL files together. Test a
restored copy before replacing the live DB. Deletion is represented by cancellation
and agent deactivation so history and issue keys remain intact.

## Validate

```bash
cargo fmt --check
cargo clippy --locked --all-targets -- -D warnings
cargo test --locked
cargo build --locked --release
uv run --locked scripts/validate_mcp.py target/release/workboard
uv run --locked scripts/validate_web.py target/release/workboard --install-browser
```

The Rust suite exercises the domain and compiled CLI/MCP processes, including an
eight-process claim race, stale revisions, expired-token recovery, rollback,
hierarchy/dependency deadlocks, sprint history and persistence. The SDK command
uses the official MCP Python SDK and validates the generated schema, full work
loop, tools, resources, prompts, errors and CLI interoperability in a temporary DB.
The browser validator starts a real server with disposable agents/keys and
checks login, revocation, filters, project switching, live updates, details,
activity, safe rendering and mobile layout. Pass `--chromium /path/to/chromium`
to use an existing browser and `--screenshots /tmp/workboard-qa` for QA images.
Python, Chromium and `uv` are development-only dependencies. See
[validation coverage](docs/validation.md). A GitHub Actions workflow is included for these checks.

Licensed under Apache-2.0.
