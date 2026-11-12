use crate::model::*;
use rusqlite::{Connection, OptionalExtension, Transaction, TransactionBehavior, params};
use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    collections::HashSet,
    path::Path,
    time::{SystemTime, UNIX_EPOCH},
};

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("{0}")]
    Invalid(String),
    #[error("{0} not found")]
    NotFound(String),
    #[error("{0}")]
    Conflict(String),
    #[error("storage error: {0}")]
    Database(#[from] rusqlite::Error),
    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),
}
impl Error {
    pub fn code(&self) -> &'static str {
        match self {
            Self::Invalid(_) => "invalid",
            Self::NotFound(_) => "not_found",
            Self::Conflict(_) => "conflict",
            _ => "internal",
        }
    }
    pub fn json(&self) -> Value {
        json!({"code":self.code(),"message":self.to_string()})
    }
}
type Result<T> = std::result::Result<T, Error>;
fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock before epoch")
        .as_secs() as i64
}
fn invalid(s: impl Into<String>) -> Error {
    Error::Invalid(s.into())
}
fn conflict(s: impl Into<String>) -> Error {
    Error::Conflict(s.into())
}
fn nonempty(s: &str, field: &str) -> Result<()> {
    if s.trim().is_empty() || s.len() > 100_000 {
        return Err(invalid(format!(
            "{field} must be nonempty and at most 100000 bytes"
        )));
    }
    Ok(())
}
fn identifier(s: &str) -> Result<()> {
    if s.is_empty()
        || s.len() > 64
        || !s
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
    {
        return Err(invalid(
            "IDs must contain 1-64 ASCII letters, digits, '_' or '-'",
        ));
    }
    Ok(())
}
fn encode<T: serde::Serialize>(v: &T) -> Result<String> {
    Ok(serde_json::to_string(v)?)
}
fn get<T: DeserializeOwned>(tx: &Connection, table: &str, id: &str) -> Result<T> {
    let doc: Option<String> = tx
        .query_row(&format!("SELECT doc FROM {table} WHERE id=?1"), [id], |r| {
            r.get(0)
        })
        .optional()?;
    serde_json::from_str(&doc.ok_or_else(|| Error::NotFound(id.into()))?).map_err(Into::into)
}
fn all<T: DeserializeOwned>(tx: &Connection, table: &str) -> Result<Vec<T>> {
    let mut stmt = tx.prepare(&format!("SELECT doc FROM {table} ORDER BY rowid"))?;
    let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
    rows.map(|r| Ok(serde_json::from_str(&r?)?)).collect()
}
fn put<T: serde::Serialize>(tx: &Connection, table: &str, id: &str, doc: &T) -> Result<()> {
    tx.execute(&format!("INSERT INTO {table}(id,doc) VALUES(?1,?2) ON CONFLICT(id) DO UPDATE SET doc=excluded.doc"), params![id, encode(doc)?])?;
    Ok(())
}
fn event(
    tx: &Connection,
    actor: &str,
    issue: Option<&str>,
    action: &str,
    data: Value,
) -> Result<()> {
    tx.execute(
        "INSERT INTO events(issue,actor,action,data,created_at) VALUES(?1,?2,?3,?4,?5)",
        params![issue, actor, action, encode(&data)?, now()],
    )?;
    Ok(())
}
fn save(tx: &Connection, actor: &str, issue: &mut Issue, action: &str) -> Result<Value> {
    issue.revision += 1;
    issue.updated_at = now();
    put(tx, "issues", &issue.key, issue)?;
    let v = serde_json::to_value(&issue)?;
    event(tx, actor, Some(&issue.key), action, v.clone())?;
    Ok(v)
}
fn revision(issue: &Issue, expected: i64) -> Result<()> {
    if issue.revision != expected {
        return Err(conflict(format!(
            "revision mismatch: expected {expected}, current {}",
            issue.revision
        )));
    }
    Ok(())
}
fn active_agent(tx: &Connection, id: &str) -> Result<Agent> {
    let a: Agent = get(tx, "agents", id)?;
    if !a.active {
        return Err(conflict(format!("agent {id} is inactive")));
    }
    Ok(a)
}
fn lease_check(issue: &Issue, actor: &str, token: Option<&str>) -> Result<()> {
    if let Some(lease) = &issue.lease {
        if issue.assignee.as_deref() != Some(actor)
            || token != Some(&lease.token)
            || lease.expires_at <= now()
        {
            return Err(conflict(
                "valid lease token and owning actor required; expired claims must be reclaimed",
            ));
        }
    } else if token.is_some() {
        return Err(conflict("issue has no active lease"));
    }
    Ok(())
}
fn duration(seconds: Option<i64>) -> Result<i64> {
    let s = seconds.unwrap_or(900);
    if !(1..=86400).contains(&s) {
        return Err(invalid("lease_seconds must be between 1 and 86400"));
    }
    Ok(s)
}
fn limit(n: Option<usize>) -> Result<usize> {
    let n = n.unwrap_or(100);
    if !(1..=1000).contains(&n) {
        Err(invalid("limit must be between 1 and 1000"))
    } else {
        Ok(n)
    }
}

/// How long a retried mutation is recognized and answered from the stored result.
const IDEMPOTENCY_TTL: i64 = 24 * 60 * 60;
const MAX_WEBHOOKS: i64 = 50;

fn idempotency_key(key: &str) -> Result<()> {
    if key.is_empty()
        || key.len() > 128
        || !key
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'.' | b':'))
    {
        return Err(invalid(
            "idempotency key must be 1-128 ASCII letters, digits, '_', '-', '.' or ':'",
        ));
    }
    Ok(())
}
/// Shows enough of a destination to recognize it without exposing path or query
/// secrets, which services such as chat webhooks embed in their URLs.
fn mask_url(url: &str) -> String {
    let Some((scheme, rest)) = url.split_once("://") else {
        return "…".into();
    };
    let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
    let host = authority.rsplit('@').next().unwrap_or(authority);
    if rest.len() == authority.len() {
        format!("{scheme}://{host}")
    } else {
        format!("{scheme}://{host}/…")
    }
}
fn masked(webhook: &Webhook) -> Result<Value> {
    let mut value = serde_json::to_value(webhook)?;
    value["url"] = json!(mask_url(&webhook.url));
    Ok(value)
}
fn validate_webhook_url(url: &str) -> Result<()> {
    let uri: ureq::http::Uri = url
        .parse()
        .map_err(|_| invalid("webhook url must be a valid absolute http or https URL"))?;
    let scheme_ok = matches!(uri.scheme_str(), Some("http" | "https"));
    if url.len() > 2048 || !scheme_ok || uri.host().is_none_or(str::is_empty) {
        return Err(invalid(
            "webhook url must be an absolute http or https URL of at most 2048 bytes",
        ));
    }
    Ok(())
}
fn latest_event(conn: &Connection) -> Result<i64> {
    Ok(conn.query_row("SELECT COALESCE(MAX(id),0) FROM events", [], |r| r.get(0))?)
}
fn update_webhook(tx: &Connection, webhook: &Webhook) -> Result<()> {
    tx.execute(
        "UPDATE webhooks SET doc=?2 WHERE id=?1",
        params![webhook.id, encode(webhook)?],
    )?;
    Ok(())
}
fn webhook_by_id(tx: &Connection, id: &str) -> Result<Option<Webhook>> {
    match get(tx, "webhooks", id) {
        Ok(webhook) => Ok(Some(webhook)),
        Err(Error::NotFound(_)) => Ok(None),
        Err(error) => Err(error),
    }
}

/// An active webhook with the secret needed to sign its deliveries.
pub struct WebhookTarget {
    pub webhook: Webhook,
    pub secret: String,
}

pub struct Store {
    conn: Connection,
}
impl Store {
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let conn = Connection::open(path)?;
        conn.busy_timeout(std::time::Duration::from_secs(10))?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "foreign_keys", "ON")?;
        let version: i64 = conn.pragma_query_value(None, "user_version", |r| r.get(0))?;
        if version > 3 {
            return Err(invalid("database schema is newer than this binary"));
        }
        conn.execute_batch("BEGIN IMMEDIATE;
            CREATE TABLE IF NOT EXISTS agents(id TEXT PRIMARY KEY, doc TEXT NOT NULL CHECK(json_valid(doc)));
            CREATE TABLE IF NOT EXISTS projects(id TEXT PRIMARY KEY, doc TEXT NOT NULL CHECK(json_valid(doc)));
            CREATE TABLE IF NOT EXISTS issues(id TEXT PRIMARY KEY, doc TEXT NOT NULL CHECK(json_valid(doc)));
            CREATE TABLE IF NOT EXISTS sprints(id TEXT PRIMARY KEY, doc TEXT NOT NULL CHECK(json_valid(doc)));
            CREATE TABLE IF NOT EXISTS counters(project TEXT PRIMARY KEY REFERENCES projects(id), value INTEGER NOT NULL);
            CREATE TABLE IF NOT EXISTS links(source TEXT REFERENCES issues(id),target TEXT REFERENCES issues(id),kind TEXT NOT NULL,PRIMARY KEY(source,target,kind),CHECK(source<>target));
            CREATE TABLE IF NOT EXISTS comments(id INTEGER PRIMARY KEY AUTOINCREMENT,issue TEXT NOT NULL REFERENCES issues(id),actor TEXT NOT NULL REFERENCES agents(id),body TEXT NOT NULL,created_at INTEGER NOT NULL);
            CREATE TABLE IF NOT EXISTS events(id INTEGER PRIMARY KEY AUTOINCREMENT,issue TEXT,actor TEXT NOT NULL,action TEXT NOT NULL,data TEXT NOT NULL,created_at INTEGER NOT NULL);
            CREATE INDEX IF NOT EXISTS comments_issue ON comments(issue,id);
            CREATE INDEX IF NOT EXISTS events_issue ON events(issue,id);
            CREATE INDEX IF NOT EXISTS issues_project ON issues(json_extract(doc,'$.project'));
            CREATE TABLE IF NOT EXISTS api_keys(id TEXT PRIMARY KEY,agent TEXT NOT NULL REFERENCES agents(id),label TEXT NOT NULL,key_hash TEXT NOT NULL UNIQUE,created_at INTEGER NOT NULL,revoked_at INTEGER);
            CREATE TABLE IF NOT EXISTS idempotency(actor TEXT NOT NULL,key TEXT NOT NULL,request_hash TEXT NOT NULL,response TEXT NOT NULL,created_at INTEGER NOT NULL,PRIMARY KEY(actor,key));
            CREATE INDEX IF NOT EXISTS idempotency_created ON idempotency(created_at);
            CREATE TABLE IF NOT EXISTS webhooks(id TEXT PRIMARY KEY,doc TEXT NOT NULL CHECK(json_valid(doc)),secret TEXT NOT NULL);
            PRAGMA user_version=3;
            COMMIT;")?;
        Ok(Self { conn })
    }
    /// HTTP identity comes exclusively from a live key and active registry entry.
    pub fn authenticate(&self, token: &str) -> Result<Option<Agent>> {
        if token.len() != 67 || !token.starts_with("wb_") {
            return Ok(None);
        }
        let hash = format!("{:x}", Sha256::digest(token.as_bytes()));
        let id: Option<String> = self
            .conn
            .query_row(
                "SELECT agent FROM api_keys WHERE key_hash=?1 AND revoked_at IS NULL",
                [hash],
                |r| r.get(0),
            )
            .optional()?;
        match id {
            Some(id) => {
                let a: Agent = get(&self.conn, "agents", &id)?;
                Ok(a.active.then_some(a))
            }
            None => Ok(None),
        }
    }
    /// Consistent, read-only snapshot for the observer dashboard. Does not recover
    /// leases or emit events; merely viewing work must not change agent state.
    pub fn observation(&mut self, project: Option<&str>) -> Result<Value> {
        let tx = self.conn.transaction()?;
        let projects = all::<Project>(&tx, "projects")?;
        let selected = project
            .map(str::to_owned)
            .or_else(|| projects.first().map(|p| p.key.clone()));
        if let Some(key) = &selected {
            let _: Project = get(&tx, "projects", key)?;
        }
        let issues: Vec<_> = all::<Issue>(&tx, "issues")?
            .into_iter()
            .filter(|i| Some(&i.project) == selected.as_ref())
            .collect();
        let sprints: Vec<_> = all::<Sprint>(&tx, "sprints")?
            .into_iter()
            .filter(|s| Some(&s.project) == selected.as_ref())
            .collect();
        let activity = observation_events(&tx, selected.as_deref(), None, None)?;
        let value = json!({"projects":projects,"project":selected,"agents":all::<Agent>(&tx,"agents")?,"issues":issues,"sprints":sprints,"activity":activity,"server_time":now()});
        tx.commit()?;
        Ok(value)
    }
    pub fn observation_issue(&mut self, key: &str) -> Result<Value> {
        let tx = self.conn.transaction()?;
        let mut context = execute(&tx, "observer", Command::IssueGet { key: key.into() })?;
        context["comments"] = observation_comments(&tx, key, None)?;
        context["activity"] = observation_events(&tx, None, Some(key), None)?;
        tx.commit()?;
        Ok(context)
    }
    pub fn observation_activity(
        &mut self,
        project: Option<&str>,
        key: Option<&str>,
        before: Option<i64>,
    ) -> Result<Value> {
        let tx = self.conn.transaction()?;
        if let Some(p) = project {
            let _: Project = get(&tx, "projects", p)?;
        }
        if let Some(k) = key {
            let _: Issue = get(&tx, "issues", k)?;
        }
        let value = observation_events(&tx, project, key, before)?;
        tx.commit()?;
        Ok(value)
    }
    pub fn observation_comments(&mut self, key: &str, before: Option<i64>) -> Result<Value> {
        let tx = self.conn.transaction()?;
        let _: Issue = get(&tx, "issues", key)?;
        let value = observation_comments(&tx, key, before)?;
        tx.commit()?;
        Ok(value)
    }
    pub fn execute(&mut self, actor: &str, command: Command) -> Result<Value> {
        identifier(actor)?;
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let result = execute(&tx, actor, command)?;
        tx.commit()?;
        Ok(result)
    }
    /// Runs a command at most once per `(actor, key)` for 24 hours. A retry with the
    /// same key and command returns the original result and `true` without touching
    /// the board; the same key with a different command is a conflict. The record
    /// commits atomically with the mutation, so a crash can never leave one without
    /// the other. Administrative commands are excluded because their results carry
    /// secrets that must not be stored for replay.
    pub fn execute_idempotent(
        &mut self,
        actor: &str,
        key: Option<&str>,
        command: Command,
    ) -> Result<(Value, bool)> {
        let Some(key) = key else {
            return self.execute(actor, command).map(|v| (v, false));
        };
        identifier(actor)?;
        idempotency_key(key)?;
        if command.is_operator_only() {
            return Err(invalid(
                "idempotency keys are not supported for administrative commands",
            ));
        }
        if read_only(&command) {
            return self.execute(actor, command).map(|v| (v, false));
        }
        let hash = format!("{:x}", Sha256::digest(serde_json::to_vec(&command)?));
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let previous: Option<(String, String)> = tx
            .query_row(
                "SELECT request_hash,response FROM idempotency WHERE actor=?1 AND key=?2 AND created_at>?3",
                params![actor, key, now() - IDEMPOTENCY_TTL],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        if let Some((previous_hash, response)) = previous {
            if previous_hash != hash {
                return Err(conflict(
                    "idempotency key was already used with a different command",
                ));
            }
            return Ok((serde_json::from_str(&response)?, true));
        }
        let result = execute(&tx, actor, command)?;
        tx.execute(
            "DELETE FROM idempotency WHERE created_at<=?1 OR (actor=?2 AND key=?3)",
            params![now() - IDEMPOTENCY_TTL, actor, key],
        )?;
        tx.execute(
            "INSERT INTO idempotency(actor,key,request_hash,response,created_at) VALUES(?1,?2,?3,?4,?5)",
            params![actor, key, hash, encode(&result)?, now()],
        )?;
        tx.commit()?;
        Ok((result, false))
    }
    /// Validates a stream scope and picks its starting point: an explicit cursor, or
    /// the newest event so that a fresh stream carries only what happens next.
    pub fn stream_cursor(&self, project: Option<&str>, after: Option<i64>) -> Result<i64> {
        if let Some(key) = project {
            let _: Project = get(&self.conn, "projects", key)?;
        }
        match after {
            Some(cursor) if cursor < 0 => Err(invalid("after must not be negative")),
            Some(cursor) => Ok(cursor),
            None => latest_event(&self.conn),
        }
    }
    /// Events with ids above `after`, oldest first, optionally scoped to one project.
    pub fn events_after(
        &self,
        project: Option<&str>,
        after: i64,
        limit: usize,
    ) -> Result<Vec<Event>> {
        events_forward(&self.conn, project, after, limit)
    }
    /// Active webhooks with their signing secrets, oldest first.
    pub fn webhook_targets(&self) -> Result<Vec<WebhookTarget>> {
        let mut stmt = self
            .conn
            .prepare("SELECT doc,secret FROM webhooks ORDER BY rowid")?;
        let rows = stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?;
        let mut targets = vec![];
        for row in rows {
            let (doc, secret) = row?;
            let webhook: Webhook = serde_json::from_str(&doc)?;
            if webhook.active {
                targets.push(WebhookTarget { webhook, secret });
            }
        }
        Ok(targets)
    }
    /// The next events a webhook should receive, plus the id of the last event
    /// examined. Events the webhook filters out must still advance its cursor, or
    /// a run of unrelated events would stall delivery.
    pub fn webhook_batch(&self, webhook: &Webhook, limit: usize) -> Result<(Vec<Event>, i64)> {
        let scanned = events_forward(
            &self.conn,
            webhook.project.as_deref(),
            webhook.cursor,
            limit,
        )?;
        let scan_end = scanned.last().map_or(webhook.cursor, |e| e.id);
        let matching = scanned
            .into_iter()
            .filter(|e| webhook.actions.is_empty() || webhook.actions.contains(&e.action))
            .collect();
        Ok((matching, scan_end))
    }
    /// Records progress. The cursor never moves backwards and a webhook deleted in
    /// the meantime is ignored.
    pub fn webhook_ack(&mut self, id: &str, cursor: i64, delivered: bool) -> Result<()> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        if let Some(mut webhook) = webhook_by_id(&tx, id)? {
            webhook.cursor = webhook.cursor.max(cursor);
            webhook.failures = 0;
            webhook.last_error = None;
            if delivered {
                webhook.last_delivered_at = Some(now());
            }
            update_webhook(&tx, &webhook)?;
        }
        tx.commit()?;
        Ok(())
    }
    /// Counts a failed delivery. After `max_failures` in a row the webhook is paused
    /// and a `webhook_disabled` event is recorded. Returns the failure count.
    pub fn webhook_fail(&mut self, id: &str, error: &str, max_failures: u32) -> Result<u32> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let mut failures = 0;
        if let Some(mut webhook) = webhook_by_id(&tx, id)? {
            webhook.failures += 1;
            failures = webhook.failures;
            webhook.last_error = Some(error.chars().take(200).collect());
            if webhook.failures >= max_failures {
                webhook.active = false;
                event(
                    &tx,
                    "system",
                    None,
                    "webhook_disabled",
                    json!({"id":webhook.id,"label":webhook.label,"project":webhook.project,"failures":webhook.failures}),
                )?;
            }
            update_webhook(&tx, &webhook)?;
        }
        tx.commit()?;
        Ok(failures)
    }
}

fn links(tx: &Connection) -> Result<Vec<Link>> {
    let mut s = tx.prepare("SELECT source,target,kind FROM links ORDER BY source,target,kind")?;
    let rows = s.query_map([], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, String>(1)?,
            r.get::<_, String>(2)?,
        ))
    })?;
    rows.map(|r| {
        let (source, target, kind) = r?;
        Ok(Link {
            source,
            target,
            kind: serde_json::from_str(&kind)?,
        })
    })
    .collect()
}
fn blockers(tx: &Connection, key: &str) -> Result<Vec<String>> {
    let mut blocked = vec![];
    for link in links(tx)? {
        if link.kind == LinkKind::Blocks && link.target == key {
            let source: Issue = get(tx, "issues", &link.source)?;
            if !source.state.terminal() {
                blocked.push(source.key);
            }
        }
    }
    Ok(blocked)
}
fn descendants(tx: &Connection, key: &str) -> Result<Vec<Issue>> {
    let issues = all::<Issue>(tx, "issues")?;
    let mut found = HashSet::from([key.to_owned()]);
    loop {
        let before = found.len();
        for issue in &issues {
            if issue.parent.as_ref().is_some_and(|p| found.contains(p)) {
                found.insert(issue.key.clone());
            }
        }
        if before == found.len() {
            break;
        }
    }
    Ok(issues
        .into_iter()
        .filter(|i| i.key != key && found.contains(&i.key))
        .collect())
}

// Completion depends both on explicit blockers and on finishing children. Checking
// only explicit link cycles would miss an epic blocking its own descendant.
fn completion_graph(
    tx: &Connection,
    replacement: Option<&Issue>,
    added: Option<&Link>,
) -> Result<()> {
    let mut issues = all::<Issue>(tx, "issues")?;
    if let Some(issue) = replacement {
        issues.retain(|i| i.key != issue.key);
        issues.push(issue.clone());
    }
    let mut edges: Vec<(String, String)> = issues
        .iter()
        .filter_map(|i| i.parent.as_ref().map(|p| (i.key.clone(), p.clone())))
        .collect();
    for l in links(tx)?.into_iter().chain(added.cloned()) {
        if l.kind == LinkKind::Blocks {
            edges.push((l.source, l.target));
        }
    }
    let mut remaining: HashSet<_> = issues.into_iter().map(|i| i.key).collect();
    while !remaining.is_empty() {
        let roots: Vec<_> = remaining
            .iter()
            .filter(|key| {
                !edges
                    .iter()
                    .any(|(source, target)| target == *key && remaining.contains(source))
            })
            .cloned()
            .collect();
        if roots.is_empty() {
            return Err(conflict(
                "dependencies and parent hierarchy must not form a completion cycle",
            ));
        }
        for root in roots {
            remaining.remove(&root);
        }
    }
    Ok(())
}
fn validate_issue(tx: &Connection, issue: &Issue) -> Result<()> {
    nonempty(&issue.title, "title")?;
    if issue.labels.len() > 100
        || issue
            .labels
            .iter()
            .any(|l| l.trim().is_empty() || l.len() > 100)
    {
        return Err(invalid(
            "labels require 1-100 bytes each, at most 100 labels",
        ));
    }
    if issue.estimate.is_some_and(|e| !e.is_finite() || e < 0.) {
        return Err(invalid("estimate must be finite and nonnegative"));
    }
    if let Some(parent) = &issue.parent {
        if parent == &issue.key {
            return Err(invalid("issue cannot parent itself"));
        }
        let p: Issue = get(tx, "issues", parent)?;
        if p.project != issue.project {
            return Err(invalid("parent must be in the same project"));
        }
        let allowed = match issue.kind {
            IssueKind::Epic => false,
            IssueKind::Subtask => matches!(
                p.kind,
                IssueKind::Story | IssueKind::Task | IssueKind::Bug | IssueKind::Spike
            ),
            _ => p.kind == IssueKind::Epic,
        };
        if !allowed {
            return Err(invalid(
                "hierarchy must be epic -> story/task/bug/spike -> subtask",
            ));
        }
        if p.state.terminal() && !issue.state.terminal() {
            return Err(conflict(
                "cannot add open work to a terminal parent; reopen parent first",
            ));
        }
    } else if issue.kind == IssueKind::Subtask {
        return Err(invalid("subtask requires a parent"));
    }
    if let Some(id) = &issue.sprint {
        let s: Sprint = get(tx, "sprints", id)?;
        if s.project != issue.project
            || (s.state == SprintState::Completed && !issue.state.terminal())
        {
            return Err(invalid("sprint must be in same project and not completed"));
        }
        if issue.kind == IssueKind::Epic {
            return Err(invalid(
                "epics span sprints; assign their children to sprints",
            ));
        }
    }
    completion_graph(tx, Some(issue), None)
}
fn actionable(tx: &Connection, i: &Issue) -> Result<bool> {
    if i.state != State::Ready || i.kind == IssueKind::Epic || !blockers(tx, &i.key)?.is_empty() {
        return Ok(false);
    }
    if let Some(p) = &i.parent {
        let parent: Issue = get(tx, "issues", p)?;
        if parent.state.terminal() {
            return Ok(false);
        }
    }
    Ok(true)
}
fn filtered(tx: &Connection, f: &IssueFilter) -> Result<Vec<Issue>> {
    let mut result = vec![];
    for i in all::<Issue>(tx, "issues")? {
        if f.project.as_ref().is_some_and(|v| v != &i.project)
            || f.kind.is_some_and(|v| v != i.kind)
            || f.state.is_some_and(|v| v != i.state)
            || f.assignee
                .as_ref()
                .is_some_and(|v| Some(v) != i.assignee.as_ref())
            || (f.unassigned && i.assignee.is_some())
            || f.parent
                .as_ref()
                .is_some_and(|v| Some(v) != i.parent.as_ref())
            || f.sprint
                .as_ref()
                .is_some_and(|v| Some(v) != i.sprint.as_ref())
            || f.label.as_ref().is_some_and(|v| !i.labels.contains(v))
        {
            continue;
        }
        if let Some(t) = &f.text {
            let t = t.to_lowercase();
            if !format!("{} {} {}", i.key, i.title, i.description)
                .to_lowercase()
                .contains(&t)
            {
                continue;
            }
        }
        if f.actionable && !actionable(tx, &i)? {
            continue;
        }
        result.push(i);
    }
    result.sort_by_key(|i| {
        (
            match i.priority {
                Priority::Critical => 0,
                Priority::High => 1,
                Priority::Medium => 2,
                Priority::Low => 3,
            },
            i.created_at,
            i.key
                .rsplit('-')
                .next()
                .and_then(|n| n.parse::<i64>().ok())
                .unwrap_or(0),
        )
    });
    Ok(result)
}
fn read_only(c: &Command) -> bool {
    matches!(
        c,
        Command::AgentList {}
            | Command::ApiKeyList { .. }
            | Command::ProjectList {}
            | Command::IssueGet { .. }
            | Command::IssueList { .. }
            | Command::CommentList { .. }
            | Command::Events { .. }
            | Command::Board { .. }
            | Command::EpicProgress { .. }
            | Command::SprintList { .. }
            | Command::Workflow {}
            | Command::WebhookList {}
    )
}

fn execute(tx: &Transaction<'_>, actor: &str, c: Command) -> Result<Value> {
    use Command::*;
    if (!read_only(&c) || matches!(&c, ApiKeyList { .. } | WebhookList {}))
        && !matches!(&c,AgentRegister{id,..} if id==actor)
    {
        active_agent(tx, actor)?;
    }
    match c {
        AgentRegister {
            id,
            name,
            capabilities,
        } => {
            identifier(&id)?;
            nonempty(&name, "name")?;
            if tx
                .query_row("SELECT 1 FROM agents WHERE id=?1", [&id], |_| Ok(()))
                .optional()?
                .is_some()
            {
                return Err(conflict("agent already registered; IDs are stable"));
            }
            let a = Agent {
                id,
                name,
                capabilities,
                active: true,
            };
            put(tx, "agents", &a.id, &a)?;
            event(tx, actor, None, "agent_registered", json!(a))?;
            Ok(json!(a))
        }
        AgentSetActive { id, active } => {
            let mut a: Agent = get(tx, "agents", &id)?;
            a.active = active;
            put(tx, "agents", &id, &a)?;
            event(tx, actor, None, "agent_active_changed", json!(a))?;
            Ok(json!(a))
        }
        AgentList {} => Ok(json!(all::<Agent>(tx, "agents")?)),
        ApiKeyCreate { agent, label } => {
            active_agent(tx, &agent)?;
            nonempty(&label, "label")?;
            let secret = format!(
                "wb_{}{}",
                uuid::Uuid::new_v4().simple(),
                uuid::Uuid::new_v4().simple()
            );
            let hash = format!("{:x}", Sha256::digest(secret.as_bytes()));
            let key = ApiKey {
                id: uuid::Uuid::new_v4().to_string(),
                agent,
                label,
                created_at: now(),
                revoked_at: None,
            };
            tx.execute(
                "INSERT INTO api_keys(id,agent,label,key_hash,created_at) VALUES(?1,?2,?3,?4,?5)",
                params![key.id, key.agent, key.label, hash, key.created_at],
            )?;
            event(tx, actor, None, "api_key_created", json!(key))?;
            Ok(json!({"key":key,"api_key":secret}))
        }
        ApiKeyList { agent } => {
            if let Some(id) = &agent {
                let _: Agent = get(tx, "agents", id)?;
            }
            let mut stmt = tx.prepare("SELECT id,agent,label,created_at,revoked_at FROM api_keys WHERE (?1 IS NULL OR agent=?1) ORDER BY created_at,id")?;
            let rows = stmt.query_map([agent], |r| {
                Ok(ApiKey {
                    id: r.get(0)?,
                    agent: r.get(1)?,
                    label: r.get(2)?,
                    created_at: r.get(3)?,
                    revoked_at: r.get(4)?,
                })
            })?;
            Ok(json!(rows.collect::<std::result::Result<Vec<_>, _>>()?))
        }
        ApiKeyRevoke { id } => {
            let changed = tx.execute(
                "UPDATE api_keys SET revoked_at=?1 WHERE id=?2 AND revoked_at IS NULL",
                params![now(), id],
            )?;
            if changed == 0 {
                return Err(conflict("key is missing or already revoked"));
            }
            event(tx, actor, None, "api_key_revoked", json!({"id":id}))?;
            Ok(json!({"id":id,"revoked":true}))
        }
        WebhookCreate {
            url,
            label,
            project,
            actions,
        } => {
            nonempty(&label, "label")?;
            validate_webhook_url(&url)?;
            if let Some(key) = &project {
                let _: Project = get(tx, "projects", key)?;
            }
            if actions.len() > 50 || actions.iter().any(|a| a.is_empty() || a.len() > 64) {
                return Err(invalid("actions allow at most 50 names of 1-64 bytes each"));
            }
            let existing: i64 = tx.query_row("SELECT COUNT(*) FROM webhooks", [], |r| r.get(0))?;
            if existing >= MAX_WEBHOOKS {
                return Err(conflict(format!(
                    "at most {MAX_WEBHOOKS} webhooks are allowed"
                )));
            }
            let secret = format!(
                "whsec_{}{}",
                uuid::Uuid::new_v4().simple(),
                uuid::Uuid::new_v4().simple()
            );
            let mut webhook = Webhook {
                id: uuid::Uuid::new_v4().to_string(),
                label,
                url,
                project,
                actions,
                active: true,
                cursor: 0,
                failures: 0,
                last_error: None,
                last_delivered_at: None,
                created_at: now(),
            };
            tx.execute(
                "INSERT INTO webhooks(id,doc,secret) VALUES(?1,?2,?3)",
                params![webhook.id, encode(&webhook)?, secret],
            )?;
            // The URL is deliberately absent: activity feeds are visible to every agent.
            event(
                tx,
                actor,
                None,
                "webhook_created",
                json!({"id":webhook.id,"label":webhook.label,"project":webhook.project,"actions":webhook.actions}),
            )?;
            // Only future events are delivered, not the history that predates the webhook.
            webhook.cursor = latest_event(tx)?;
            update_webhook(tx, &webhook)?;
            Ok(json!({"webhook":masked(&webhook)?,"secret":secret}))
        }
        WebhookList {} => Ok(json!(
            all::<Webhook>(tx, "webhooks")?
                .iter()
                .map(masked)
                .collect::<Result<Vec<_>>>()?
        )),
        WebhookSetActive { id, active } => {
            let mut webhook: Webhook = get(tx, "webhooks", &id)?;
            webhook.active = active;
            if active {
                webhook.failures = 0;
                webhook.last_error = None;
            }
            update_webhook(tx, &webhook)?;
            event(
                tx,
                actor,
                None,
                if active {
                    "webhook_resumed"
                } else {
                    "webhook_paused"
                },
                json!({"id":webhook.id,"label":webhook.label,"project":webhook.project}),
            )?;
            masked(&webhook)
        }
        WebhookDelete { id } => {
            let webhook: Webhook = get(tx, "webhooks", &id)?;
            tx.execute("DELETE FROM webhooks WHERE id=?1", [&id])?;
            event(
                tx,
                actor,
                None,
                "webhook_deleted",
                json!({"id":webhook.id,"label":webhook.label,"project":webhook.project}),
            )?;
            Ok(json!({"id":id,"deleted":true}))
        }
        ProjectCreate {
            key,
            name,
            description,
        } => {
            if key.is_empty() || key.len() > 16 || !key.bytes().all(|b| b.is_ascii_uppercase()) {
                return Err(invalid("project key must be 1-16 uppercase ASCII letters"));
            }
            nonempty(&name, "name")?;
            if tx
                .query_row("SELECT 1 FROM projects WHERE id=?1", [&key], |_| Ok(()))
                .optional()?
                .is_some()
            {
                return Err(conflict("project already exists"));
            }
            let p = Project {
                key,
                name,
                description,
            };
            put(tx, "projects", &p.key, &p)?;
            event(tx, actor, None, "project_created", json!(p))?;
            Ok(json!(p))
        }
        ProjectList {} => Ok(json!(all::<Project>(tx, "projects")?)),
        IssueCreate {
            project,
            kind,
            title,
            description,
            acceptance_criteria,
            assignee,
            parent,
            sprint,
            labels,
            estimate,
            priority,
            metadata,
        } => {
            let _: Project = get(tx, "projects", &project)?;
            let n:i64=tx.query_row("INSERT INTO counters(project,value) VALUES(?1,1) ON CONFLICT(project) DO UPDATE SET value=value+1 RETURNING value",[&project],|r|r.get(0))?;
            let mut i = Issue {
                key: format!("{project}-{n}"),
                project,
                kind,
                title,
                description,
                acceptance_criteria,
                state: State::Backlog,
                priority: priority.unwrap_or(Priority::Medium),
                assignee,
                parent,
                sprint,
                labels,
                estimate,
                metadata,
                revision: 0,
                created_at: now(),
                updated_at: now(),
                lease: None,
            };
            if let Some(a) = &i.assignee {
                active_agent(tx, a)?;
            }
            validate_issue(tx, &i)?;
            save(tx, actor, &mut i, "issue_created")
        }
        IssueGet { key } => {
            let i: Issue = get(tx, "issues", &key)?;
            let l: Vec<_> = links(tx)?
                .into_iter()
                .filter(|l| l.source == key || l.target == key)
                .collect();
            Ok(
                json!({"issue":i,"links":l,"blockers":blockers(tx,&key)?,"children":all::<Issue>(tx,"issues")?.into_iter().filter(|i|i.parent.as_deref()==Some(&key)).collect::<Vec<_>>()}),
            )
        }
        IssueList { filter } => {
            let n = limit(filter.limit)?;
            let items = filtered(tx, &filter)?;
            let total = items.len();
            let items: Vec<_> = items.into_iter().skip(filter.offset).take(n).collect();
            let next = if filter.offset.saturating_add(items.len()) < total {
                Some(filter.offset + items.len())
            } else {
                None
            };
            Ok(json!({"items":items,"total":total,"next_offset":next}))
        }
        IssueUpdate {
            key,
            expected_revision,
            title,
            description,
            acceptance_criteria,
            priority,
            labels,
            estimate,
            clear_estimate,
            parent,
            clear_parent,
            sprint,
            clear_sprint,
            metadata,
            lease_token,
        } => {
            let mut i: Issue = get(tx, "issues", &key)?;
            revision(&i, expected_revision)?;
            lease_check(&i, actor, lease_token.as_deref())?;
            if (clear_estimate && estimate.is_some())
                || (clear_parent && parent.is_some())
                || (clear_sprint && sprint.is_some())
            {
                return Err(invalid("cannot set and clear the same field"));
            }
            if let Some(v) = title {
                i.title = v;
            }
            if let Some(v) = description {
                i.description = v;
            }
            if let Some(v) = acceptance_criteria {
                i.acceptance_criteria = v;
            }
            if let Some(v) = priority {
                i.priority = v;
            }
            if let Some(v) = labels {
                i.labels = v;
            }
            if let Some(v) = metadata {
                i.metadata = v;
            }
            if clear_estimate {
                i.estimate = None;
            } else if estimate.is_some() {
                i.estimate = estimate;
            }
            if clear_parent {
                i.parent = None;
            } else if parent.is_some() {
                i.parent = parent;
            }
            if clear_sprint {
                i.sprint = None;
            } else if sprint.is_some() {
                let s: Sprint = get(tx, "sprints", sprint.as_deref().expect("checked"))?;
                if s.state == SprintState::Completed {
                    return Err(conflict("cannot attach work to a completed sprint"));
                }
                i.sprint = sprint;
            }
            validate_issue(tx, &i)?;
            save(tx, actor, &mut i, "issue_updated")
        }
        IssueAssign {
            key,
            expected_revision,
            assignee,
        } => {
            let mut i: Issue = get(tx, "issues", &key)?;
            revision(&i, expected_revision)?;
            if let Some(a) = &assignee {
                active_agent(tx, a)?;
            }
            // Explicit reassignment revokes a claim and returns interrupted work to ready.
            if i.lease.is_some()
                || (i.assignee != assignee
                    && matches!(i.state, State::InProgress | State::InReview))
            {
                i.state = State::Ready;
                i.lease = None;
            }
            i.assignee = assignee;
            save(tx, actor, &mut i, "issue_assigned")
        }
        IssueTransition {
            key,
            expected_revision,
            state,
            lease_token,
        } => {
            let mut i: Issue = get(tx, "issues", &key)?;
            revision(&i, expected_revision)?;
            lease_check(&i, actor, lease_token.as_deref())?;
            if !i.state.permits(state) {
                return Err(conflict(format!(
                    "transition {:?} -> {:?} is not allowed",
                    i.state, state
                )));
            }
            if i.state.terminal() && !state.terminal() {
                for link in links(tx)? {
                    if link.kind == LinkKind::Blocks && link.source == key {
                        let target: Issue = get(tx, "issues", &link.target)?;
                        if matches!(
                            target.state,
                            State::InProgress | State::InReview | State::Done
                        ) {
                            return Err(conflict(
                                "move dependent issues to backlog, ready or blocked before reopening their blocker",
                            ));
                        }
                    }
                }
            }
            if matches!(state, State::InProgress | State::InReview | State::Done) {
                if !blockers(tx, &key)?.is_empty() {
                    return Err(conflict("unresolved blocking issues"));
                }
                if i.kind != IssueKind::Epic && i.assignee.is_none() {
                    return Err(conflict(
                        "assign an agent before starting or completing work",
                    ));
                }
                if let Some(a) = &i.assignee {
                    active_agent(tx, a)?;
                }
            }
            if state.terminal() && descendants(tx, &key)?.iter().any(|c| !c.state.terminal()) {
                return Err(conflict("close or cancel all descendants first"));
            }
            if !state.terminal()
                && let Some(p) = &i.parent
            {
                let p: Issue = get(tx, "issues", p)?;
                if p.state.terminal() {
                    return Err(conflict("reopen parent first"));
                }
            }
            if !state.terminal()
                && let Some(id) = &i.sprint
            {
                let s: Sprint = get(tx, "sprints", id)?;
                if s.state == SprintState::Completed {
                    return Err(conflict(
                        "move issue out of its completed sprint before reopening",
                    ));
                }
            }
            i.state = state;
            if !matches!(state, State::InProgress | State::InReview) {
                i.lease = None;
            }
            save(tx, actor, &mut i, "issue_transitioned")
        }
        IssueClaim {
            project,
            key,
            lease_seconds,
        } => {
            let _: Project = get(tx, "projects", &project)?;
            let seconds = duration(lease_seconds)?;
            // Recovery, selection, assignment and lease issuance are a single write transaction.
            for mut i in all::<Issue>(tx, "issues")? {
                if i.project == project && i.lease.as_ref().is_some_and(|l| l.expires_at <= now()) {
                    i.lease = None;
                    i.assignee = None;
                    i.state = State::Ready;
                    save(tx, actor, &mut i, "claim_expired")?;
                }
            }
            let mut selected = None;
            for i in filtered(
                tx,
                &IssueFilter {
                    project: Some(project),
                    ..Default::default()
                },
            )? {
                if key.as_ref().is_some_and(|k| k != &i.key)
                    || i.assignee.as_ref().is_some_and(|a| a != actor)
                    || !actionable(tx, &i)?
                {
                    continue;
                }
                selected = Some(i);
                break;
            }
            let Some(mut i) = selected else {
                return Ok(json!({"claimed":null}));
            };
            i.assignee = Some(actor.into());
            i.state = State::InProgress;
            i.lease = Some(Lease {
                token: uuid::Uuid::new_v4().to_string(),
                expires_at: now() + seconds,
            });
            Ok(json!({"claimed":save(tx,actor,&mut i,"issue_claimed")?}))
        }
        IssueHeartbeat {
            key,
            lease_token,
            lease_seconds,
        } => {
            let seconds = duration(lease_seconds)?;
            let mut i: Issue = get(tx, "issues", &key)?;
            if i.lease.is_none() {
                return Err(conflict("issue is not leased"));
            }
            lease_check(&i, actor, Some(&lease_token))?;
            i.lease.as_mut().expect("checked").expires_at = now() + seconds;
            save(tx, actor, &mut i, "claim_renewed")
        }
        IssueRelease { key, lease_token } => {
            let mut i: Issue = get(tx, "issues", &key)?;
            if i.lease.is_none() {
                return Err(conflict("issue is not leased"));
            }
            lease_check(&i, actor, Some(&lease_token))?;
            i.lease = None;
            i.assignee = None;
            i.state = State::Ready;
            save(tx, actor, &mut i, "claim_released")
        }
        LinkAdd {
            source,
            target,
            kind,
        } => {
            let _: Issue = get(tx, "issues", &source)?;
            let t: Issue = get(tx, "issues", &target)?;
            if source == target {
                return Err(invalid("cannot link an issue to itself"));
            }
            if kind == LinkKind::Blocks {
                completion_graph(
                    tx,
                    None,
                    Some(&Link {
                        source: source.clone(),
                        target: target.clone(),
                        kind,
                    }),
                )?;
                let s: Issue = get(tx, "issues", &source)?;
                if !s.state.terminal()
                    && matches!(t.state, State::InProgress | State::InReview | State::Done)
                {
                    return Err(conflict(
                        "move target to blocked or ready before introducing an unresolved dependency",
                    ));
                }
            }
            let (source, target) = if kind == LinkKind::RelatesTo && source > target {
                (target, source)
            } else {
                (source, target)
            };
            if tx.execute(
                "INSERT OR IGNORE INTO links(source,target,kind) VALUES(?1,?2,?3)",
                params![source, target, encode(&kind)?],
            )? == 0
            {
                return Err(conflict("link already exists"));
            }
            let l = Link {
                source,
                target,
                kind,
            };
            event(tx, actor, Some(&l.source), "link_added", json!(l))?;
            event(tx, actor, Some(&l.target), "link_added", json!(l))?;
            Ok(json!(l))
        }
        LinkRemove {
            source,
            target,
            kind,
        } => {
            let (source, target) = if kind == LinkKind::RelatesTo && source > target {
                (target, source)
            } else {
                (source, target)
            };
            if tx.execute(
                "DELETE FROM links WHERE source=?1 AND target=?2 AND kind=?3",
                params![source, target, encode(&kind)?],
            )? == 0
            {
                return Err(Error::NotFound("link".into()));
            }
            let l = Link {
                source,
                target,
                kind,
            };
            event(tx, actor, Some(&l.source), "link_removed", json!(l))?;
            event(tx, actor, Some(&l.target), "link_removed", json!(l))?;
            Ok(json!(l))
        }
        CommentAdd { key, body } => {
            let _: Issue = get(tx, "issues", &key)?;
            nonempty(&body, "body")?;
            let ts = now();
            tx.execute(
                "INSERT INTO comments(issue,actor,body,created_at) VALUES(?1,?2,?3,?4)",
                params![key, actor, body, ts],
            )?;
            let c = Comment {
                id: tx.last_insert_rowid(),
                issue: key,
                actor: actor.into(),
                body,
                created_at: ts,
            };
            event(tx, actor, Some(&c.issue), "comment_added", json!(c))?;
            Ok(json!(c))
        }
        CommentList {
            key,
            after,
            limit: n,
        } => {
            let _: Issue = get(tx, "issues", &key)?;
            let n = limit(n)?;
            let mut s=tx.prepare("SELECT id,issue,actor,body,created_at FROM comments WHERE issue=?1 AND id>?2 ORDER BY id LIMIT ?3")?;
            let rows = s.query_map(params![key, after.unwrap_or(0), n as i64], |r| {
                Ok(Comment {
                    id: r.get(0)?,
                    issue: r.get(1)?,
                    actor: r.get(2)?,
                    body: r.get(3)?,
                    created_at: r.get(4)?,
                })
            })?;
            Ok(json!(rows.collect::<std::result::Result<Vec<_>, _>>()?))
        }
        Events {
            key,
            after,
            limit: n,
        } => {
            if let Some(k) = &key {
                let _: Issue = get(tx, "issues", k)?;
            }
            let n = limit(n)?;
            let mut s=tx.prepare("SELECT id,issue,actor,action,data,created_at FROM events WHERE (?1 IS NULL OR issue=?1) AND id>?2 ORDER BY id LIMIT ?3")?;
            let rows = s.query_map(params![key, after.unwrap_or(0), n as i64], |r| {
                Ok((
                    r.get::<_, i64>(0)?,
                    r.get::<_, Option<String>>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, String>(3)?,
                    r.get::<_, String>(4)?,
                    r.get::<_, i64>(5)?,
                ))
            })?;
            let mut events = vec![];
            for r in rows {
                let (id, issue, actor, action, data, created_at) = r?;
                events.push(Event {
                    id,
                    issue,
                    actor,
                    action,
                    data: serde_json::from_str(&data)?,
                    created_at,
                });
            }
            Ok(json!(events))
        }
        Board { project, sprint } => {
            let _: Project = get(tx, "projects", &project)?;
            let items = filtered(
                tx,
                &IssueFilter {
                    project: Some(project.clone()),
                    sprint: sprint.clone(),
                    ..Default::default()
                },
            )?;
            let mut columns = serde_json::Map::new();
            for state in [
                State::Backlog,
                State::Ready,
                State::InProgress,
                State::Blocked,
                State::InReview,
                State::Done,
                State::Canceled,
            ] {
                let group: Vec<_> = items.iter().filter(|i| i.state == state).collect();
                columns.insert(
                    serde_json::to_value(state)?
                        .as_str()
                        .expect("enum string")
                        .into(),
                    json!(group),
                );
            }
            let ready: Vec<_> = items
                .iter()
                .filter_map(|i| match actionable(tx, i) {
                    Ok(true) => Some(Ok(i.key.clone())),
                    Ok(false) => None,
                    Err(e) => Some(Err(e)),
                })
                .collect::<Result<_>>()?;
            Ok(
                json!({"project":project,"sprint":sprint,"total":items.len(),"columns":columns,"actionable":ready}),
            )
        }
        EpicProgress { key } => {
            let i: Issue = get(tx, "issues", &key)?;
            if i.kind != IssueKind::Epic {
                return Err(invalid("expected an epic"));
            }
            let children = descendants(tx, &key)?;
            let done = children.iter().filter(|i| i.state == State::Done).count();
            let canceled = children
                .iter()
                .filter(|i| i.state == State::Canceled)
                .count();
            Ok(
                json!({"epic":i,"total":children.len(),"done":done,"canceled":canceled,"open":children.len()-done-canceled,"descendants":children}),
            )
        }
        SprintCreate {
            project,
            name,
            goal,
        } => {
            let _: Project = get(tx, "projects", &project)?;
            nonempty(&name, "name")?;
            let s = Sprint {
                id: uuid::Uuid::new_v4().to_string(),
                project,
                name,
                goal,
                state: SprintState::Planned,
            };
            put(tx, "sprints", &s.id, &s)?;
            event(tx, actor, None, "sprint_created", json!(s))?;
            Ok(json!(s))
        }
        SprintList { project } => {
            let _: Project = get(tx, "projects", &project)?;
            Ok(json!(
                all::<Sprint>(tx, "sprints")?
                    .into_iter()
                    .filter(|s| s.project == project)
                    .collect::<Vec<_>>()
            ))
        }
        SprintTransition { id, state } => {
            let mut s: Sprint = get(tx, "sprints", &id)?;
            if !matches!(
                (s.state, state),
                (SprintState::Planned, SprintState::Active)
                    | (SprintState::Active, SprintState::Completed)
            ) {
                return Err(conflict("sprints move planned -> active -> completed"));
            }
            if state == SprintState::Active
                && all::<Sprint>(tx, "sprints")?
                    .iter()
                    .any(|other| other.project == s.project && other.state == SprintState::Active)
            {
                return Err(conflict("project already has an active sprint"));
            }
            if state == SprintState::Completed
                && all::<Issue>(tx, "issues")?
                    .iter()
                    .any(|i| i.sprint.as_deref() == Some(&id) && !i.state.terminal())
            {
                return Err(conflict(
                    "finish work or move open issues out of sprint before completing",
                ));
            }
            s.state = state;
            put(tx, "sprints", &id, &s)?;
            event(tx, actor, None, "sprint_transitioned", json!(s))?;
            Ok(json!(s))
        }
        Workflow {} => {
            let states = [
                State::Backlog,
                State::Ready,
                State::InProgress,
                State::Blocked,
                State::InReview,
                State::Done,
                State::Canceled,
            ];
            let transitions:Vec<_>=states.iter().map(|s|json!({"from":s,"to":states.iter().filter(|t|s.permits(**t)).collect::<Vec<_>>()})).collect();
            Ok(
                json!({"states":states,"transitions":transitions,"hierarchy":"epic -> story/task/bug/spike -> subtask"}),
            )
        }
    }
}

// Events that belong to a project: its issues, plus project-level and agent events.
// Parameter ?1 is the project key, or NULL for every event.
const EVENT_SCOPE: &str = "(?1 IS NULL OR e.issue IN (SELECT id FROM issues WHERE json_extract(doc,'$.project')=?1)
    OR (e.issue IS NULL AND (e.action LIKE 'agent_%' OR json_extract(e.data,'$.project')=?1 OR (e.action='project_created' AND json_extract(e.data,'$.key')=?1))))";

// Oldest-first events after a cursor, for push streams and webhook delivery.
fn events_forward(
    tx: &Connection,
    project: Option<&str>,
    after: i64,
    limit: usize,
) -> Result<Vec<Event>> {
    let mut stmt = tx.prepare(&format!(
        "SELECT id,issue,actor,action,data,created_at FROM events e
        WHERE e.id>?2 AND {EVENT_SCOPE} ORDER BY e.id LIMIT ?3"
    ))?;
    let rows = stmt.query_map(params![project, after, limit as i64], |r| {
        Ok((
            r.get::<_, i64>(0)?,
            r.get::<_, Option<String>>(1)?,
            r.get::<_, String>(2)?,
            r.get::<_, String>(3)?,
            r.get::<_, String>(4)?,
            r.get::<_, i64>(5)?,
        ))
    })?;
    let mut events = vec![];
    for row in rows {
        let (id, issue, actor, action, data, created_at) = row?;
        events.push(Event {
            id,
            issue,
            actor,
            action,
            data: serde_json::from_str(&data)?,
            created_at,
        });
    }
    Ok(events)
}

// Newest-first cursor pages for a monitoring feed; independent of the agents'
// forward event cursors used for synchronization.
fn observation_events(
    tx: &Connection,
    project: Option<&str>,
    key: Option<&str>,
    before: Option<i64>,
) -> Result<Value> {
    let mut stmt = tx.prepare(&format!(
        "SELECT id,issue,actor,action,data,created_at FROM events e
        WHERE {EVENT_SCOPE}
        AND (?2 IS NULL OR e.issue=?2) AND (?3 IS NULL OR e.id<?3)
        ORDER BY id DESC LIMIT 101"
    ))?;
    let rows = stmt.query_map(params![project, key, before], |r| {
        Ok((
            r.get::<_, i64>(0)?,
            r.get::<_, Option<String>>(1)?,
            r.get::<_, String>(2)?,
            r.get::<_, String>(3)?,
            r.get::<_, String>(4)?,
            r.get::<_, i64>(5)?,
        ))
    })?;
    let mut items = Vec::new();
    for row in rows {
        let (id, issue, actor, action, data, created_at) = row?;
        items.push(Event {
            id,
            issue,
            actor,
            action,
            data: serde_json::from_str(&data)?,
            created_at,
        });
    }
    let next = if items.len() > 100 {
        items.truncate(100);
        items.last().map(|e| e.id)
    } else {
        None
    };
    Ok(json!({"items":items,"next_before":next}))
}
fn observation_comments(tx: &Connection, key: &str, before: Option<i64>) -> Result<Value> {
    let mut stmt = tx.prepare("SELECT id,issue,actor,body,created_at FROM comments WHERE issue=?1 AND (?2 IS NULL OR id<?2) ORDER BY id DESC LIMIT 101")?;
    let rows = stmt.query_map(params![key, before], |r| {
        Ok(Comment {
            id: r.get(0)?,
            issue: r.get(1)?,
            actor: r.get(2)?,
            body: r.get(3)?,
            created_at: r.get(4)?,
        })
    })?;
    let mut items = rows.collect::<std::result::Result<Vec<_>, _>>()?;
    let next = if items.len() > 100 {
        items.truncate(100);
        items.last().map(|c| c.id)
    } else {
        None
    };
    Ok(json!({"items":items,"next_before":next}))
}
