//! Database schema and migrations.
//!
//! Issues stay JSON documents, which remain the single source of truth. Everything
//! used to find them quickly is *derived*:
//!
//! * **Generated columns** expose document fields to SQL so they can be indexed.
//!   They are computed by SQLite from `doc`, so they cannot drift out of sync and
//!   need no backfill or maintenance code.
//! * **Triggers** keep the full-text index and the label table current, so that no
//!   code path can forget to update them. Saving an issue whose text and labels did
//!   not change (a heartbeat, a state transition) does not touch either.
use crate::store::Error;
use rusqlite::{Connection, TransactionBehavior};

pub const VERSION: i64 = 4;

/// Document fields exposed as virtual generated columns of `issues`.
const ISSUE_COLUMNS: &[(&str, &str)] = &[
    (
        "project",
        "TEXT GENERATED ALWAYS AS (json_extract(doc,'$.project')) VIRTUAL",
    ),
    (
        "kind",
        "TEXT GENERATED ALWAYS AS (json_extract(doc,'$.kind')) VIRTUAL",
    ),
    (
        "state",
        "TEXT GENERATED ALWAYS AS (json_extract(doc,'$.state')) VIRTUAL",
    ),
    (
        "priority",
        "TEXT GENERATED ALWAYS AS (json_extract(doc,'$.priority')) VIRTUAL",
    ),
    (
        "assignee",
        "TEXT GENERATED ALWAYS AS (json_extract(doc,'$.assignee')) VIRTUAL",
    ),
    (
        "parent",
        "TEXT GENERATED ALWAYS AS (json_extract(doc,'$.parent')) VIRTUAL",
    ),
    (
        "sprint",
        "TEXT GENERATED ALWAYS AS (json_extract(doc,'$.sprint')) VIRTUAL",
    ),
    // Sort keys. Together they reproduce the board order: priority, age, then the
    // number in the issue key (the digits after its last '-').
    (
        "prio_rank",
        "INTEGER GENERATED ALWAYS AS (CASE json_extract(doc,'$.priority') WHEN 'critical' THEN 0 WHEN 'high' THEN 1 WHEN 'medium' THEN 2 ELSE 3 END) VIRTUAL",
    ),
    (
        "created",
        "INTEGER GENERATED ALWAYS AS (json_extract(doc,'$.created_at')) VIRTUAL",
    ),
    (
        "seq",
        "INTEGER GENERATED ALWAYS AS (CAST(substr(id,length(rtrim(id,'0123456789'))+1) AS INTEGER)) VIRTUAL",
    ),
    (
        "lease_expires",
        "INTEGER GENERATED ALWAYS AS (json_extract(doc,'$.lease.expires_at')) VIRTUAL",
    ),
];

const BASE: &str = "
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
CREATE TABLE IF NOT EXISTS api_keys(id TEXT PRIMARY KEY,agent TEXT NOT NULL REFERENCES agents(id),label TEXT NOT NULL,key_hash TEXT NOT NULL UNIQUE,created_at INTEGER NOT NULL,revoked_at INTEGER);
CREATE TABLE IF NOT EXISTS idempotency(actor TEXT NOT NULL,key TEXT NOT NULL,request_hash TEXT NOT NULL,response TEXT NOT NULL,created_at INTEGER NOT NULL,PRIMARY KEY(actor,key));
CREATE INDEX IF NOT EXISTS idempotency_created ON idempotency(created_at);
CREATE TABLE IF NOT EXISTS webhooks(id TEXT PRIMARY KEY,doc TEXT NOT NULL CHECK(json_valid(doc)),secret TEXT NOT NULL);
CREATE TABLE IF NOT EXISTS saved_filters(name TEXT PRIMARY KEY,owner TEXT NOT NULL REFERENCES agents(id),query TEXT NOT NULL,created_at INTEGER NOT NULL,updated_at INTEGER NOT NULL);
";

const INDEXES: &str = "
DROP INDEX IF EXISTS issues_project;
CREATE INDEX IF NOT EXISTS issues_board ON issues(project,state,prio_rank,created,seq);
CREATE INDEX IF NOT EXISTS issues_assignee ON issues(assignee,state) WHERE assignee IS NOT NULL;
CREATE INDEX IF NOT EXISTS issues_parent ON issues(parent) WHERE parent IS NOT NULL;
CREATE INDEX IF NOT EXISTS issues_sprint ON issues(sprint,state) WHERE sprint IS NOT NULL;
CREATE INDEX IF NOT EXISTS issues_lease ON issues(project,lease_expires) WHERE lease_expires IS NOT NULL;
CREATE INDEX IF NOT EXISTS links_target ON links(target,kind);
CREATE INDEX IF NOT EXISTS issue_labels_issue ON issue_labels(issue);
";

/// The values of one FTS row, built from `src`: a row alias or `new`.
fn fts_values(src: &str) -> String {
    format!(
        "{src}.rowid, {src}.id,
         json_extract({src}.doc,'$.title'),
         json_extract({src}.doc,'$.description'),
         (SELECT COALESCE(group_concat(value,' '),'') FROM json_each({src}.doc,'$.labels')),
         (SELECT COALESCE(group_concat(value,char(10)),'') FROM json_each({src}.doc,'$.acceptance_criteria')),
         (SELECT COALESCE(group_concat(body,char(10)),'') FROM comments WHERE issue={src}.id)"
    )
}
const FTS_COLUMNS: &str = "rowid,key,title,description,labels,criteria,comments";
const TEXT_CHANGED: &str = "json_extract(old.doc,'$.title') IS NOT json_extract(new.doc,'$.title')
    OR json_extract(old.doc,'$.description') IS NOT json_extract(new.doc,'$.description')
    OR json_extract(old.doc,'$.labels') IS NOT json_extract(new.doc,'$.labels')
    OR json_extract(old.doc,'$.acceptance_criteria') IS NOT json_extract(new.doc,'$.acceptance_criteria')";

fn triggers() -> String {
    let new = fts_values("new");
    let joined = fts_values("i");
    format!(
        "
CREATE TRIGGER IF NOT EXISTS issues_fts_insert AFTER INSERT ON issues BEGIN
  INSERT INTO issue_fts({FTS_COLUMNS}) SELECT {new};
  INSERT OR IGNORE INTO issue_labels(issue,label) SELECT new.id, value FROM json_each(new.doc,'$.labels');
END;
CREATE TRIGGER IF NOT EXISTS issues_fts_update AFTER UPDATE OF doc ON issues WHEN {TEXT_CHANGED} BEGIN
  DELETE FROM issue_fts WHERE rowid=old.rowid;
  INSERT INTO issue_fts({FTS_COLUMNS}) SELECT {new};
END;
CREATE TRIGGER IF NOT EXISTS issues_labels_update AFTER UPDATE OF doc ON issues
  WHEN json_extract(old.doc,'$.labels') IS NOT json_extract(new.doc,'$.labels') BEGIN
  DELETE FROM issue_labels WHERE issue=old.id;
  INSERT OR IGNORE INTO issue_labels(issue,label) SELECT new.id, value FROM json_each(new.doc,'$.labels');
END;
CREATE TRIGGER IF NOT EXISTS issues_fts_delete AFTER DELETE ON issues BEGIN
  DELETE FROM issue_fts WHERE rowid=old.rowid;
  DELETE FROM issue_labels WHERE issue=old.id;
END;
CREATE TRIGGER IF NOT EXISTS comments_fts_insert AFTER INSERT ON comments BEGIN
  DELETE FROM issue_fts WHERE rowid=(SELECT rowid FROM issues WHERE id=new.issue);
  INSERT INTO issue_fts({FTS_COLUMNS}) SELECT {joined} FROM issues i WHERE i.id=new.issue;
END;
"
    )
}

fn exists(conn: &Connection, kind: &str, name: &str) -> Result<bool, Error> {
    Ok(conn
        .query_row(
            "SELECT 1 FROM sqlite_master WHERE type=?1 AND name=?2",
            [kind, name],
            |_| Ok(()),
        )
        .is_ok())
}
fn has_column(conn: &Connection, table: &str, column: &str) -> Result<bool, Error> {
    // Generated columns are hidden from table_info but listed by table_xinfo.
    let mut stmt = conn.prepare(&format!("PRAGMA table_xinfo({table})"))?;
    let names = stmt.query_map([], |r| r.get::<_, String>(1))?;
    for name in names {
        if name? == column {
            return Ok(true);
        }
    }
    Ok(false)
}

/// Brings any older database (or an empty file) up to [`VERSION`], atomically.
///
/// Every step is idempotent and the version is re-read after taking the write lock,
/// because several processes may open a database that needs migrating at once: the
/// loser must find the work already done, not fail on it.
pub fn migrate(conn: &mut Connection) -> Result<(), Error> {
    let newer = || Error::Invalid("database schema is newer than this binary".into());
    if conn.pragma_query_value(None, "user_version", |r| r.get::<_, i64>(0))? > VERSION {
        return Err(newer());
    }
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    if tx.pragma_query_value(None, "user_version", |r| r.get::<_, i64>(0))? > VERSION {
        return Err(newer());
    }
    tx.execute_batch(BASE)?;
    for (name, definition) in ISSUE_COLUMNS {
        if !has_column(&tx, "issues", name)? {
            tx.execute_batch(&format!(
                "ALTER TABLE issues ADD COLUMN {name} {definition}"
            ))?;
        }
    }
    let fresh_labels = !exists(&tx, "table", "issue_labels")?;
    tx.execute_batch(
        "CREATE TABLE IF NOT EXISTS issue_labels(
            issue TEXT NOT NULL REFERENCES issues(id) ON DELETE CASCADE,
            label TEXT NOT NULL,
            PRIMARY KEY(label,issue)) WITHOUT ROWID;",
    )?;
    let fresh_fts = !exists(&tx, "table", "issue_fts")?;
    // Porter stemming lets "claiming" find "claim"; prefix indexes keep `claim*` fast.
    tx.execute_batch(
        "CREATE VIRTUAL TABLE IF NOT EXISTS issue_fts USING fts5(
            key,title,description,labels,criteria,comments,
            tokenize='porter unicode61',prefix='2 3');",
    )?;
    tx.execute_batch(INDEXES)?;
    // Backfill before the triggers exist, so no row is indexed twice.
    if fresh_labels {
        tx.execute_batch(
            "INSERT OR IGNORE INTO issue_labels(issue,label)
               SELECT i.id, j.value FROM issues i, json_each(i.doc,'$.labels') j;",
        )?;
    }
    if fresh_fts {
        let row = fts_values("i");
        tx.execute_batch(&format!(
            "INSERT INTO issue_fts({FTS_COLUMNS}) SELECT {row} FROM issues i;"
        ))?;
    }
    tx.execute_batch(&triggers())?;
    tx.pragma_update(None, "user_version", VERSION)?;
    tx.commit()?;
    Ok(())
}
