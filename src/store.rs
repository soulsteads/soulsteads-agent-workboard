use crate::model::*;
use rusqlite::{Connection, OptionalExtension, Transaction, TransactionBehavior, params};
use serde::de::DeserializeOwned;
use serde_json::{Value, json};
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
        if version > 1 {
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
            PRAGMA user_version=1;
            COMMIT;")?;
        Ok(Self { conn })
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
    if let Some(a) = &issue.assignee {
        active_agent(tx, a)?;
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
        if s.project != issue.project || s.state == SprintState::Completed {
            return Err(invalid("sprint must be in same project and not completed"));
        }
        if issue.kind == IssueKind::Epic {
            return Err(invalid(
                "epics span sprints; assign their children to sprints",
            ));
        }
    }
    Ok(())
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
            i.key.clone(),
        )
    });
    Ok(result)
}
fn read_only(c: &Command) -> bool {
    matches!(
        c,
        Command::AgentList {}
            | Command::ProjectList {}
            | Command::IssueGet { .. }
            | Command::IssueList { .. }
            | Command::CommentList { .. }
            | Command::Events { .. }
            | Command::Board { .. }
            | Command::EpicProgress { .. }
            | Command::SprintList { .. }
            | Command::Workflow {}
    )
}

fn execute(tx: &Transaction<'_>, actor: &str, c: Command) -> Result<Value> {
    use Command::*;
    if !read_only(&c) && !matches!(&c,AgentRegister{id,..} if id==actor) {
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
            if i.lease.is_some() {
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
            if !state.terminal() {
                if let Some(p) = &i.parent {
                    let p: Issue = get(tx, "issues", p)?;
                    if p.state.terminal() {
                        return Err(conflict("reopen parent first"));
                    }
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
                let mut reached = HashSet::from([target.clone()]);
                let existing = links(tx)?;
                loop {
                    let before = reached.len();
                    for l in &existing {
                        if l.kind == LinkKind::Blocks && reached.contains(&l.source) {
                            reached.insert(l.target.clone());
                        }
                    }
                    if before == reached.len() {
                        break;
                    }
                }
                if reached.contains(&source) {
                    return Err(conflict("blocking links must not form cycles"));
                }
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
