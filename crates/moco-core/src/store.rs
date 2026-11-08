//! SQLite store (docs/02-domain-model.md). WAL mode, migrations table, JSON columns for variable data.
use crate::compat::*;
use crate::id::new_id;
use crate::permission::{Effect, Rule};
use rusqlite::{params, Connection, OptionalExtension};
use serde_json::Value;
use std::path::Path;

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("sqlite: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("json: {0}")]
    Json(#[from] serde_json::Error),
    /// The session was deleted (for example by `moco sessions` in another process) while it was in use.
    #[error("session no longer exists")]
    SessionGone,
}
pub type Result<T> = std::result::Result<T, StoreError>;

/// Bumped whenever this process saves or removes a saved permission, so a copy of the saved rules cached for a step
/// (`engine/permit.rs`) is dropped at once instead of outliving a `/permissions remove` made while the step runs.
pub static SAVED_PERMISSIONS_GEN: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// `directory` with symlinks resolved (as far as it exists), so every spelling of a directory is one project.
fn canonical_dir(directory: &str) -> String {
    crate::tools::real_path(Path::new(directory)).to_string_lossy().into_owned()
}

const MIGRATIONS: &[(i64, &str)] = &[
    (
        1,
        "create table project(
        id text primary key, directory text not null unique, name text, time_created integer not null);
     create table session(
        id text primary key, project_id text not null references project(id), parent_id text,
        title text, directory text not null, agent text not null, model text, permission text,
        cost real not null default 0, tokens text,
        time_created integer not null, time_updated integer not null, time_archived integer,
        claim_at integer, resume_attempts integer not null default 0);
     create table message(
        id text primary key, session_id text not null references session(id) on delete cascade,
        type text not null, data text not null, time_created integer not null);
     create index message_session on message(session_id, id);
     create table session_inbox(
        id text primary key, session_id text not null references session(id) on delete cascade,
        mode text not null check(mode in ('steer','queue')), type text not null, data text not null,
        time_created integer not null);
     create index inbox_session on session_inbox(session_id, id);
     create table permission_saved(
        project_id text not null, action text not null, resource text not null,
        primary key(project_id, action, resource));
     create table todo(
        session_id text not null references session(id) on delete cascade, position integer not null,
        content text not null, status text not null, primary key(session_id, position));",
    ),
    (
        2,
        "create table instruction_state(
            session_id text not null references session(id) on delete cascade, path text not null,
            primary key(session_id, path));",
    ),
    (3, "alter table session add column revert text;"),
    (4, "alter table session add column fork_session_id text; alter table session add column fork_boundary text;"),
    (5, "alter table session add column variant text;"),
    (6, "alter table session add column permission_mode text;"),
    // The part of `cost` that went to the permission classifier (it is also counted in `cost`).
    (7, "alter table session add column classifier_cost real not null default 0;"),
    // Shell commands the permission classifier allowed without asking (the `/permissions` listing; issue #28).
    (
        8,
        "create table auto_allow(
        id integer primary key autoincrement, session_id text not null references session(id) on delete cascade,
        at integer not null, command text not null, risk text not null, reason text not null, model text not null);
     create index auto_allow_session on auto_allow(session_id);",
    ),
    // Which process holds the execution claim, so another start can tell a live run from a crash (issue #7).
    (9, "alter table session add column claim_pid integer;"),
    // Small key/value state, e.g. when snapshots were last pruned.
    (10, "create table meta(key text primary key, value text not null);"),
    // `last_snapshot` looks up the newest assistant message that carries a snapshot; a partial index keeps that from
    // scanning (and JSON-parsing) every message of a long session. Built over existing rows when it is applied.
    (11, "create index if not exists message_snapshot on message(session_id, id) where type='assistant' and json_extract(data,'$.snapshot') is not null;"),
];

#[derive(Debug, Clone, PartialEq)]
pub struct Session {
    pub id: String,
    pub project_id: String,
    pub directory: String,
    pub agent: String,
    pub title: Option<String>,
    /// `provider/model` chosen for this session, if any.
    pub model: Option<String>,
    /// Reasoning effort chosen for this session (`high`, `none`, ...), if any.
    pub variant: Option<String>,
    /// `manual` or `auto` (see `permission.rs`); unset means the configured default.
    pub permission_mode: Option<String>,
    /// Set for subagent sessions.
    pub parent_id: Option<String>,
}

/// A shell command the permission classifier let through without asking.
#[derive(Debug, Clone, PartialEq)]
pub struct AutoAllow {
    pub session_id: String,
    /// Milliseconds since the epoch.
    pub at: i64,
    pub command: String,
    pub risk: String,
    pub reason: String,
    pub model: String,
}

/// One row of the session list.
#[derive(Debug, Clone, PartialEq)]
pub struct SessionSummary {
    pub id: String,
    pub title: Option<String>,
    pub directory: String,
    pub agent: String,
    pub model: Option<String>,
    pub cost: f64,
    pub time_created: i64,
    pub time_updated: i64,
    pub archived: bool,
    /// User and assistant messages.
    pub messages: usize,
    pub forked_from: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Message {
    pub id: String,
    pub session_id: String,
    pub kind: String,
    pub data: Value,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Steer,
    Queue,
}
impl Mode {
    fn as_str(self) -> &'static str {
        match self {
            Mode::Steer => "steer",
            Mode::Queue => "queue",
        }
    }
}

/// A not yet delivered prompt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InboxItem {
    pub id: String,
    /// Steer (delivered at the next step boundary) or queue (when the session is idle).
    pub steer: bool,
    /// `user`, `synthetic`, ...
    pub kind: String,
    pub text: String,
}

pub struct Store {
    conn: Connection,
}

fn now_ms() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis() as i64).unwrap_or(0)
}

/// How long a recorded automatic allow is kept: as long as the log files (seven days).
const AUTO_ALLOW_KEEP_MS: i64 = 7 * 24 * 60 * 60 * 1000;

impl Store {
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(dir) = path.parent() {
            // The real cause (permissions, a file in the way) beats the "unable to open database file" that follows.
            std::fs::create_dir_all(dir).map_err(|e| rusqlite::Error::SqliteFailure(rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_CANTOPEN), Some(format!("could not create the data directory {}: {e}", dir.display()))))?;
        }
        let conn = Connection::open(path)?;
        conn.query_row("pragma journal_mode=wal", [], |_| Ok(()))?;
        Self::init(conn)
    }

    /// Test hook (also used by the front end's tests): run raw SQL, to make later statements fail (rename a table
    /// away, add a trigger that raises) and to undo that again.
    #[doc(hidden)]
    pub fn exec_for_test(&self, sql: &str) {
        self.conn.execute_batch(sql).expect("exec_for_test");
    }

    pub fn open_memory() -> Result<Self> {
        Self::init(Connection::open_in_memory()?)
    }

    fn init(conn: Connection) -> Result<Self> {
        conn.execute_batch("pragma foreign_keys=on; pragma busy_timeout=5000;")?;
        conn.execute_batch("create table if not exists migrations(version integer primary key, applied_at integer not null)")?;
        let current: i64 = conn.query_row("select coalesce(max(version),0) from migrations", [], |r| r.get(0))?;
        for (version, sql) in MIGRATIONS.iter().filter(|(v, _)| *v > current) {
            let tx = conn.unchecked_transaction()?;
            tx.execute_batch(sql)?;
            tx.execute("insert into migrations(version, applied_at) values (?1, ?2)", params![version, now_ms()])?;
            tx.commit()?;
        }
        let store = Store { conn };
        store.prune_auto_allows(AUTO_ALLOW_KEEP_MS);
        Ok(store)
    }

    /// Forget automatic allows older than `keep_ms`. They only feed the `/permissions` listing, and the same cutoff as
    /// the log files keeps what a command line once held from piling up. A failure is logged, never fatal.
    fn prune_auto_allows(&self, keep_ms: i64) {
        crate::log::LogErr::log_err(self.conn.execute("delete from auto_allow where at < ?1", [now_ms() - keep_ms]), "store", "pruning old automatic allows");
    }

    /// Project id is derived from the directory, so reopening the same directory finds the same project. A directory
    /// reachable under two names (a symlinked parent) is one project: the symlink-free name is the one stored, and a
    /// project recorded earlier under the name as given is still found.
    pub fn project_for(&self, directory: &str) -> Result<String> {
        if let Some(id) = self.find_project(directory)? {
            return Ok(id);
        }
        let directory = canonical_dir(directory);
        let directory = directory.as_str();
        let id = new_id("prj");
        let name = Path::new(directory).file_name().map(|n| n.to_string_lossy().into_owned());
        self.conn.execute(
            "insert into project(id, directory, name, time_created) values (?1,?2,?3,?4)",
            params![id, directory, name, now_ms()],
        )?;
        Ok(id)
    }

    /// The project of `directory` if moco has been used there, without creating one (see `project_for`).
    pub fn find_project(&self, directory: &str) -> Result<Option<String>> {
        let canonical = canonical_dir(directory);
        for d in [canonical.as_str(), directory] {
            if let Some(id) = self.conn.query_row("select id from project where directory=?1", [d], |r| r.get(0)).optional()? {
                return Ok(Some(id));
            }
        }
        Ok(None)
    }

    pub fn create_session(&self, directory: &str, agent: &str) -> Result<Session> {
        let project_id = self.project_for(directory)?;
        let directory = canonical_dir(directory);
        let directory = directory.as_str();
        let id = new_id("ses");
        let t = now_ms();
        self.conn.execute(
            "insert into session(id, project_id, directory, agent, time_created, time_updated) values (?1,?2,?3,?4,?5,?5)",
            params![id, project_id, directory, agent, t],
        )?;
        Ok(Session { id, project_id, directory: directory.into(), agent: agent.into(), title: None, model: None, variant: None, permission_mode: None, parent_id: None })
    }

    /// A subagent session: same project and directory as its parent, hidden from `latest_session`.
    pub fn create_child_session(&self, parent: &Session, agent: &str) -> Result<Session> {
        let id = new_id("ses");
        let t = now_ms();
        self.conn.execute(
            "insert into session(id, project_id, parent_id, directory, agent, time_created, time_updated) values (?1,?2,?3,?4,?5,?6,?6)",
            params![id, parent.project_id, parent.id, parent.directory, agent, t],
        )?;
        Ok(Session { id, project_id: parent.project_id.clone(), directory: parent.directory.clone(), agent: agent.into(), title: None, model: None, variant: None, permission_mode: None, parent_id: Some(parent.id.clone()) })
    }

    /// Number of ancestors (0 for a top-level session).
    pub fn depth(&self, session_id: &str) -> Result<usize> {
        let (mut depth, mut cur) = (0, session_id.to_string());
        while let Some(p) = self.conn.query_row("select parent_id from session where id=?1", [&cur], |r| r.get::<_, Option<String>>(0)).optional()?.flatten() {
            depth += 1;
            cur = p;
            if depth > 32 {
                break;
            }
        }
        Ok(depth)
    }

    pub fn set_todos(&self, session_id: &str, todos: &[crate::tools::Todo]) -> Result<()> {
        let tx = self.conn.unchecked_transaction()?;
        tx.execute("delete from todo where session_id=?1", [session_id])?;
        for (i, t) in todos.iter().enumerate() {
            tx.execute("insert into todo(session_id, position, content, status) values (?1,?2,?3,?4)", params![session_id, i as i64, t.content, t.status])?;
        }
        tx.commit()?;
        Ok(())
    }

    pub fn todos(&self, session_id: &str) -> Result<Vec<crate::tools::Todo>> {
        let mut stmt = self.conn.prepare("select content, status from todo where session_id=?1 order by position")?;
        let rows = stmt.query_map([session_id], |r| Ok(crate::tools::Todo { content: r.get(0)?, status: r.get(1)? }))?;
        Ok(rows.collect::<std::result::Result<_, _>>()?)
    }

    pub fn session(&self, id: &str) -> Result<Option<Session>> {
        Ok(self
            .conn
            .query_row("select id, project_id, directory, agent, title, model, parent_id, variant, permission_mode from session where id=?1", [id], |r| {
                Ok(Session { id: r.get(0)?, project_id: r.get(1)?, directory: r.get(2)?, agent: r.get(3)?, title: r.get(4)?, model: r.get(5)?, parent_id: r.get(6)?, variant: r.get(7)?, permission_mode: r.get(8)? })
            })
            .optional()?)
    }

    /// Removes a session the way another process could; for tests of mid-run deletion.
    #[cfg(test)]
    pub(crate) fn delete_session_for_test(&self, session_id: &str) {
        self.conn.execute("delete from session where id=?1", [session_id]).unwrap();
    }

    pub fn set_agent(&self, session_id: &str, agent: &str) -> Result<()> {
        self.conn.execute("update session set agent=?2, time_updated=?3 where id=?1", params![session_id, agent, now_ms()])?;
        Ok(())
    }

    pub fn set_permission_mode(&self, session_id: &str, mode: Option<&str>) -> Result<()> {
        self.conn.execute("update session set permission_mode=?2, time_updated=?3 where id=?1", params![session_id, mode, now_ms()])?;
        Ok(())
    }

    pub fn set_variant(&self, session_id: &str, variant: Option<&str>) -> Result<()> {
        self.conn.execute("update session set variant=?2, time_updated=?3 where id=?1", params![session_id, variant, now_ms()])?;
        Ok(())
    }

    pub fn set_model(&self, session_id: &str, model: Option<&str>) -> Result<()> {
        self.conn.execute("update session set model=?2, time_updated=?3 where id=?1", params![session_id, model, now_ms()])?;
        Ok(())
    }

    /// Add one step's usage to the session totals (`cost` in USD; `tokens` is a JSON object of running sums).
    /// Subagent spend also counts toward every ancestor session, so a parent's total covers its whole tree.
    pub fn add_usage(&self, session_id: &str, cost: f64, usage: &crate::provider::Usage) -> Result<()> {
        self.add_usage_of(session_id, cost, usage, false)
    }

    /// As `add_usage`, for a permission classifier call: also counted in `classifier_cost`, so it can be shown on
    /// its own while the session total still covers everything.
    pub fn add_classifier_usage(&self, session_id: &str, cost: f64, usage: &crate::provider::Usage) -> Result<()> {
        self.add_usage_of(session_id, cost, usage, true)
    }

    fn add_usage_of(&self, session_id: &str, cost: f64, usage: &crate::provider::Usage, classifier: bool) -> Result<()> {
        let mut cur = Some(session_id.to_string());
        let mut hops = 0;
        while let Some(id) = cur {
            let prev: Option<String> = self.conn.query_row("select tokens from session where id=?1", [&id], |r| r.get(0))?;
            let mut t: Value = prev.and_then(|p| serde_json::from_str(&p).ok()).unwrap_or_else(|| serde_json::json!({}));
            for (k, v) in [("input", usage.input), ("output", usage.output), ("cacheRead", usage.cache_read), ("cacheWrite", usage.cache_write)] {
                t[k] = serde_json::json!(t[k].as_u64().unwrap_or(0) + v);
            }
            self.conn.execute(
                "update session set cost=cost+?2, tokens=?3, time_updated=?4, classifier_cost=classifier_cost+?5 where id=?1",
                params![id, cost, t.to_string(), now_ms(), if classifier { cost } else { 0.0 }],
            )?;
            cur = self.conn.query_row("select parent_id from session where id=?1", [&id], |r| r.get::<_, Option<String>>(0)).optional()?.flatten();
            hops += 1;
            if hops > 32 {
                break;
            }
        }
        Ok(())
    }

    /// Remember a command the classifier allowed without asking. The command is stored redacted (`log::redact`): a
    /// token or password on the command line must not outlive the command in the database.
    pub fn record_auto_allow(&self, session_id: &str, command: &str, risk: &str, reason: &str, model: &str) -> Result<()> {
        let command: String = crate::log::redact(command).chars().take(2000).collect();
        self.conn.execute("insert into auto_allow(session_id, at, command, risk, reason, model) values (?1,?2,?3,?4,?5,?6)", params![session_id, now_ms(), command, risk, reason, model])?;
        Ok(())
    }

    /// What the classifier allowed in this session and in the subagent sessions below it, oldest first.
    pub fn auto_allows(&self, session_id: &str) -> Result<Vec<AutoAllow>> {
        let mut stmt = self.conn.prepare(
            "with recursive tree(id) as (select ?1 union all select s.id from session s join tree on s.parent_id = tree.id)
             select a.session_id, a.at, a.command, a.risk, a.reason, a.model from auto_allow a where a.session_id in (select id from tree) order by a.id",
        )?;
        let rows = stmt.query_map([session_id], |r| Ok(AutoAllow { session_id: r.get(0)?, at: r.get(1)?, command: r.get(2)?, risk: r.get(3)?, reason: r.get(4)?, model: r.get(5)? }))?;
        Ok(rows.collect::<std::result::Result<_, _>>()?)
    }

    /// The part of the session's cost (children included) that went to the permission classifier.
    pub fn classifier_cost(&self, session_id: &str) -> Result<f64> {
        Ok(self.conn.query_row("select classifier_cost from session where id=?1", [session_id], |r| r.get(0))?)
    }

    /// (cost in USD, token sums) for the session.
    pub fn usage_totals(&self, session_id: &str) -> Result<(f64, crate::provider::Usage)> {
        let (cost, tokens): (f64, Option<String>) = self.conn.query_row("select cost, tokens from session where id=?1", [session_id], |r| Ok((r.get(0)?, r.get(1)?)))?;
        let t: Value = tokens.and_then(|p| serde_json::from_str(&p).ok()).unwrap_or(Value::Null);
        let g = |k: &str| t[k].as_u64().unwrap_or(0);
        Ok((cost, crate::provider::Usage { input: g("input"), output: g("output"), cache_read: g("cacheRead"), cache_write: g("cacheWrite") }))
    }

    /// Most recently updated, non-archived top-level session for a directory.
    pub fn latest_session(&self, directory: &str) -> Result<Option<Session>> {
        Ok(self
            .conn
            .query_row(
                "select id, project_id, directory, agent, title, model, parent_id, variant, permission_mode from session
                 where directory in (?1, ?2) and parent_id is null and time_archived is null order by id desc limit 1",
                [canonical_dir(directory).as_str(), directory],
                |r| Ok(Session { id: r.get(0)?, project_id: r.get(1)?, directory: r.get(2)?, agent: r.get(3)?, title: r.get(4)?, model: r.get(5)?, parent_id: r.get(6)?, variant: r.get(7)?, permission_mode: r.get(8)? }),
            )
            .optional()?)
    }

    /// Top-level sessions, most recently active first. `directory` limits to one working directory;
    /// `archived` selects the archived ones instead of the active ones.
    pub fn list_sessions(&self, directory: Option<&str>, archived: bool, limit: usize) -> Result<Vec<SessionSummary>> {
        let mut stmt = self.conn.prepare(
            "select s.id, s.title, s.directory, s.agent, s.model, s.cost, s.time_created, s.time_updated, s.time_archived is not null, s.fork_session_id,
                    (select count(*) from message m where m.session_id=s.id and m.type in ('user','assistant'))
             from session s
             where s.parent_id is null and (s.time_archived is not null) = ?1 and (?2 is null or s.directory in (?2, ?4))
             order by s.time_updated desc, s.id desc limit ?3",
        )?;
        let canonical = directory.map(canonical_dir);
        let rows = stmt.query_map(params![archived, directory, limit as i64, canonical], |r| {
            Ok(SessionSummary {
                id: r.get(0)?,
                title: r.get(1)?,
                directory: r.get(2)?,
                agent: r.get(3)?,
                model: r.get(4)?,
                cost: r.get(5)?,
                time_created: r.get(6)?,
                time_updated: r.get(7)?,
                archived: r.get(8)?,
                forked_from: r.get(9)?,
                messages: r.get::<_, i64>(10)? as usize,
            })
        })?;
        Ok(rows.collect::<std::result::Result<_, _>>()?)
    }

    /// Top-level sessions whose id equals `token` or starts or ends with it (session ids are long; people type a piece).
    pub fn find_sessions(&self, token: &str) -> Result<Vec<Session>> {
        let mut stmt = self.conn.prepare(
            "select id from session where parent_id is null and length(?1) > 0
             and (id = ?1 or substr(id, 1, length(?1)) = ?1 or substr(id, -length(?1)) = ?1) order by id limit 10",
        )?;
        let ids: Vec<String> = stmt.query_map([token], |r| r.get(0))?.collect::<std::result::Result<_, _>>()?;
        ids.iter().filter_map(|id| self.session(id).transpose()).collect()
    }

    pub fn set_archived(&self, session_id: &str, archived: bool) -> Result<()> {
        self.conn.execute(
            "update session set time_archived=?2 where id=?1",
            params![session_id, if archived { Some(now_ms()) } else { None }],
        )?;
        Ok(())
    }

    /// Set the title only if the session has none (a title the user chose is never overwritten by a generated one).
    pub fn set_title_if_empty(&self, session_id: &str, title: &str) -> Result<bool> {
        Ok(self.conn.execute("update session set title=?2, time_updated=?3 where id=?1 and (title is null or title='')", params![session_id, title, now_ms()])? > 0)
    }

    /// The first user message of a session, or failing that the first prompt still waiting in its inbox;
    /// plus the latest user message when it differs.
    pub fn title_source(&self, session_id: &str) -> Result<Option<(String, Option<String>)>> {
        let texts = |sql: &str| -> Result<Vec<String>> {
            let mut stmt = self.conn.prepare(sql)?;
            let rows = stmt.query_map([session_id], |r| r.get::<_, String>(0))?;
            let mut out = vec![];
            for r in rows {
                if let Some(t) = serde_json::from_str::<Value>(&r?)?["text"].as_str().filter(|t| !t.trim().is_empty()) {
                    out.push(t.to_string());
                }
            }
            Ok(out)
        };
        let mut all = texts("select data from message where session_id=?1 and type='user' order by id")?;
        all.extend(texts("select data from session_inbox where session_id=?1 and type='user' order by id")?);
        let Some(first) = all.first().cloned() else { return Ok(None) };
        let latest = all.last().filter(|_| all.len() > 1).cloned();
        Ok(Some((first, latest)))
    }

    /// Copy a conversation into a new top-level session. With `before`, only messages ahead of that message id are
    /// copied (so a fork can branch off before a prompt); a pending undo's hidden messages are never copied.
    /// Message ids are regenerated (they must stay unique and ordered); compaction checkpoints are re-pointed.
    /// Snapshot ids carry over, so `/undo` keeps working in the fork. Usage totals start from zero.
    pub fn fork_session(&self, session_id: &str, before: Option<&str>) -> Result<Session> {
        let src = self.session(session_id)?.ok_or(rusqlite::Error::QueryReturnedNoRows)?;
        let hidden_from = self.revert(session_id)?.and_then(|r| r["messageId"].as_str().map(String::from));
        let limit = [before.map(String::from), hidden_from].into_iter().flatten().min();
        let msgs: Vec<Message> = self.messages(session_id)?.into_iter().filter(|m| limit.as_deref().is_none_or(|l| m.id.as_str() < l)).collect();

        let tx = self.conn.unchecked_transaction()?;
        let id = new_id("ses");
        let t = now_ms();
        let title = src.title.as_ref().map(|t| format!("{t} (fork)"));
        tx.execute(
            "insert into session(id, project_id, directory, agent, model, title, time_created, time_updated, fork_session_id, fork_boundary, variant, permission_mode)
             values (?1,?2,?3,?4,?5,?6,?7,?7,?8,?9,?10,?11)",
            params![id, src.project_id, src.directory, src.agent, src.model, title, t, session_id, msgs.last().map(|m| m.id.clone()), src.variant, src.permission_mode],
        )?;
        let mut map: std::collections::HashMap<String, String> = std::collections::HashMap::new();
        for m in &msgs {
            let mut data = m.data.clone();
            if m.kind == "assistant" {
                // Anthropic's thinking blocks belong to the conversation that produced them; a copy starts without.
                if let Some(parts) = data["parts"].as_array_mut() {
                    parts.retain(|p| !(p["type"] == "native" && p["provider"] == "anthropic"));
                }
            }
            if m.kind == "compaction" {
                if let Some(new_keep) = data["keepFrom"].as_str().and_then(|k| map.get(k)) {
                    data["keepFrom"] = Value::String(new_keep.clone());
                }
            }
            let new_id = Self::insert_message(&tx, &id, &m.kind, &data)?;
            map.insert(m.id.clone(), new_id);
        }
        tx.execute("insert into todo(session_id, position, content, status) select ?2, position, content, status from todo where session_id=?1", params![session_id, id])?;
        tx.execute("insert into instruction_state(session_id, path) select ?2, path from instruction_state where session_id=?1", params![session_id, id])?;
        tx.commit()?;
        Ok(Session { id, project_id: src.project_id, directory: src.directory, agent: src.agent, title, model: src.model, variant: src.variant, permission_mode: src.permission_mode, parent_id: None })
    }

    pub fn set_title(&self, session_id: &str, title: &str) -> Result<()> {
        self.conn.execute("update session set title=?2, time_updated=?3 where id=?1", params![session_id, title, now_ms()])?;
        Ok(())
    }

    pub fn add_message(&self, session_id: &str, kind: &str, data: &Value) -> Result<String> {
        Self::insert_message(&self.conn, session_id, kind, data)
    }

    fn insert_message(conn: &Connection, session_id: &str, kind: &str, data: &Value) -> Result<String> {
        let id = new_id("msg");
        conn.execute(
            "insert into message(id, session_id, type, data, time_created) values (?1,?2,?3,?4,?5)",
            params![id, session_id, kind, serde_json::to_string(data)?, now_ms()],
        )?;
        conn.execute("update session set time_updated=?2 where id=?1", params![session_id, now_ms()])?;
        Ok(id)
    }

    pub fn delete_message(&self, id: &str) -> Result<()> {
        self.conn.execute("delete from message where id=?1", [id])?;
        Ok(())
    }

    /// Replace only the `parts` of a message (keeps `snapshot`, `usage` and the other fields).
    pub fn set_message_parts(&self, id: &str, parts: &[Value]) -> Result<()> {
        self.conn.execute("update message set data=json_set(data,'$.parts',json(?2)) where id=?1", params![id, serde_json::to_string(parts)?])?;
        Ok(())
    }

    /// Delete the message `from_id` and everything after it (committing an undo). Returns how many were removed.
    pub fn delete_messages_from(&self, session_id: &str, from_id: &str) -> Result<usize> {
        Ok(self.conn.execute("delete from message where session_id=?1 and id>=?2", params![session_id, from_id])?)
    }

    /// The most recent step snapshot recorded in the session, if any.
    pub fn last_snapshot(&self, session_id: &str) -> Result<Option<String>> {
        Ok(self
            .conn
            .query_row(
                "select json_extract(data,'$.snapshot') from message
                 where session_id=?1 and type='assistant' and json_extract(data,'$.snapshot') is not null order by id desc limit 1",
                [session_id],
                |r| r.get(0),
            )
            .optional()?)
    }

    /// The pending undo of a session: `{messageId, redo}` while staged.
    pub fn revert(&self, session_id: &str) -> Result<Option<Value>> {
        let t: Option<String> = self.conn.query_row("select revert from session where id=?1", [session_id], |r| r.get(0))?;
        Ok(t.and_then(|t| serde_json::from_str(&t).ok()))
    }

    pub fn set_revert(&self, session_id: &str, revert: Option<&Value>) -> Result<()> {
        self.conn.execute("update session set revert=?2, time_updated=?3 where id=?1", params![session_id, revert.map(|v| v.to_string()), now_ms()])?;
        Ok(())
    }

    /// True (and records now) if at least `interval_ms` passed since the last time this returned true for `key`.
    /// Atomic, so two processes starting together do not both run the job.
    pub fn claim_periodic(&self, key: &str, interval_ms: i64) -> Result<bool> {
        let now = now_ms();
        let n = self.conn.execute(
            "insert into meta(key, value) values (?1, ?2)
             on conflict(key) do update set value=excluded.value where cast(meta.value as integer) <= ?3",
            params![key, now.to_string(), now - interval_ms],
        )?;
        Ok(n == 1)
    }

    pub fn project_ids(&self) -> Result<Vec<String>> {
        let mut stmt = self.conn.prepare("select id from project")?;
        let rows = stmt.query_map([], |r| r.get(0))?;
        Ok(rows.collect::<std::result::Result<_, _>>()?)
    }

    /// Snapshot ids still referenced by sessions of a project that were active since `since_ms` (for pruning).
    pub fn live_snapshots(&self, project_id: &str, since_ms: i64) -> Result<std::collections::HashSet<String>> {
        let mut out = std::collections::HashSet::new();
        let mut stmt = self.conn.prepare(
            "select json_extract(m.data,'$.snapshot') from message m join session s on s.id=m.session_id
             where s.project_id=?1 and s.time_updated>=?2 and m.type='assistant' and json_extract(m.data,'$.snapshot') is not null",
        )?;
        for r in stmt.query_map(params![project_id, since_ms], |r| r.get::<_, String>(0))? {
            out.insert(r?);
        }
        let mut stmt = self.conn.prepare("select json_extract(revert,'$.redo') from session where project_id=?1 and revert is not null")?;
        for r in stmt.query_map([project_id], |r| r.get::<_, Option<String>>(0))? {
            out.extend(r?);
        }
        Ok(out)
    }

    /// Replace a message's content (assistant messages are updated as parts stream in).
    pub fn update_message(&self, id: &str, data: &Value) -> Result<()> {
        self.conn.execute("update message set data=?2 where id=?1", params![id, serde_json::to_string(data)?])?;
        Ok(())
    }

    pub fn messages(&self, session_id: &str) -> Result<Vec<Message>> {
        let mut stmt = self.conn.prepare("select id, session_id, type, data from message where session_id=?1 order by id")?;
        let rows = stmt.query_map([session_id], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?, r.get::<_, String>(3)?)))?;
        let mut out = vec![];
        for row in rows {
            let (id, session_id, kind, data) = row?;
            out.push(Message { id, session_id, kind, data: serde_json::from_str(&data)? });
        }
        Ok(out)
    }

    /// Message rows as stored, `{id, type, data, time_created}`, oldest first (for exports).
    pub fn message_rows(&self, session_id: &str) -> Result<Vec<Value>> {
        let mut stmt = self.conn.prepare("select id, type, data, time_created from message where session_id=?1 order by id")?;
        let rows = stmt.query_map([session_id], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?, r.get::<_, i64>(3)?)))?;
        let mut out = vec![];
        for row in rows {
            let (id, kind, data, t) = row?;
            out.push(serde_json::json!({"id": id, "type": kind, "data": serde_json::from_str::<Value>(&data)?, "time_created": t}));
        }
        Ok(out)
    }

    /// Session metadata for exports: ids, directory, agent, model, times, cost, tokens and fork origin.
    pub fn session_meta(&self, session_id: &str) -> Result<Option<Value>> {
        let row = self
            .conn
            .query_row(
                "select id, parent_id, directory, agent, title, model, variant, time_created, time_updated, cost, tokens, fork_session_id from session where id=?1",
                [session_id],
                |r| {
                    let tokens: Option<String> = r.get(10)?;
                    Ok(serde_json::json!({
                        "id": r.get::<_, String>(0)?, "parent_id": r.get::<_, Option<String>>(1)?, "directory": r.get::<_, String>(2)?,
                        "agent": r.get::<_, String>(3)?, "title": r.get::<_, Option<String>>(4)?, "model": r.get::<_, Option<String>>(5)?,
                        "variant": r.get::<_, Option<String>>(6)?, "time_created": r.get::<_, i64>(7)?, "time_updated": r.get::<_, i64>(8)?,
                        "cost": r.get::<_, f64>(9)?, "tokens": tokens.and_then(|t| serde_json::from_str::<Value>(&t).ok()).unwrap_or(Value::Null),
                        "forked_from": r.get::<_, Option<String>>(11)?,
                    }))
                },
            )
            .optional()?;
        Ok(row)
    }

    /// Ids of the subagent sessions directly below a session, oldest first.
    pub fn child_session_ids(&self, session_id: &str) -> Result<Vec<String>> {
        let mut stmt = self.conn.prepare("select id from session where parent_id=?1 order by id")?;
        let rows = stmt.query_map([session_id], |r| r.get(0))?;
        Ok(rows.collect::<std::result::Result<_, _>>()?)
    }

    /// Durable admission. Re-sending the same inbox id is idempotent (first admission wins).
    pub fn enqueue(&self, session_id: &str, inbox_id: Option<&str>, mode: Mode, kind: &str, data: &Value) -> Result<String> {
        let id = inbox_id.map(String::from).unwrap_or_else(|| new_id("ibx"));
        self.conn.execute(
            "insert or ignore into session_inbox(id, session_id, mode, type, data, time_created) values (?1,?2,?3,?4,?5,?6)",
            params![id, session_id, mode.as_str(), kind, serde_json::to_string(data)?, now_ms()],
        )?;
        Ok(id)
    }

    pub fn pending_inbox(&self, session_id: &str) -> Result<usize> {
        Ok(self.conn.query_row("select count(*) from session_inbox where session_id=?1", [session_id], |r| r.get::<_, i64>(0))? as usize)
    }

    /// Everything waiting in the session's inbox, in enqueue order (what a UI shows as "queued").
    pub fn inbox_items(&self, session_id: &str) -> Result<Vec<InboxItem>> {
        let mut stmt = self.conn.prepare("select id, mode, type, data from session_inbox where session_id=?1 order by id")?;
        let rows = stmt.query_map([session_id], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?, r.get::<_, String>(3)?)))?;
        let mut out = vec![];
        for row in rows {
            let (id, mode, kind, data) = row?;
            let data: Value = serde_json::from_str(&data)?;
            out.push(InboxItem { id, steer: mode == "steer", kind, text: data["text"].as_str().unwrap_or("").to_string() });
        }
        Ok(out)
    }

    /// Switch a waiting item between steer and queue. False when it was already delivered.
    pub fn set_inbox_mode(&self, inbox_id: &str, mode: Mode) -> Result<bool> {
        Ok(self.conn.execute("update session_inbox set mode=?2 where id=?1", params![inbox_id, mode.as_str()])? > 0)
    }

    pub fn cancel_inbox(&self, inbox_id: &str) -> Result<bool> {
        Ok(self.conn.execute("delete from session_inbox where id=?1", [inbox_id])? > 0)
    }

    /// Deliver at a safe boundary: consume inbox rows and insert the visible messages in one transaction.
    /// `at_idle=false` (a step boundary) delivers all steers; `at_idle=true` delivers all steers if any,
    /// else exactly one queued item. Returns the new message ids in delivery order.
    pub fn deliver(&mut self, session_id: &str, at_idle: bool) -> Result<Vec<String>> {
        let tx = self.conn.transaction()?;
        let rows: Vec<(String, String, String, String)> = {
            let mut stmt = tx.prepare("select id, mode, type, data from session_inbox where session_id=?1 order by id")?;
            let it = stmt.query_map([session_id], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?;
            it.collect::<std::result::Result<_, _>>()?
        };
        let steers: Vec<_> = rows.iter().filter(|r| r.1 == "steer").cloned().collect();
        let chosen = if !steers.is_empty() {
            steers
        } else if at_idle {
            rows.into_iter().filter(|r| r.1 == "queue").take(1).collect()
        } else {
            vec![]
        };
        let mut ids = vec![];
        for (inbox_id, _mode, kind, data) in chosen {
            tx.execute("delete from session_inbox where id=?1", [&inbox_id])?;
            ids.push(Self::insert_message(&tx, session_id, &kind, &serde_json::from_str(&data)?)?);
        }
        tx.commit()?;
        Ok(ids)
    }

    /// Remember an instruction file that `read` discovered, so later steps keep including it.
    pub fn add_instruction(&self, session_id: &str, path: &str) -> Result<()> {
        self.conn.execute("insert or ignore into instruction_state(session_id, path) values (?1,?2)", params![session_id, path])?;
        Ok(())
    }

    pub fn instruction_paths(&self, session_id: &str) -> Result<Vec<String>> {
        let mut stmt = self.conn.prepare("select path from instruction_state where session_id=?1 order by rowid")?;
        let rows = stmt.query_map([session_id], |r| r.get(0))?;
        Ok(rows.collect::<std::result::Result<_, _>>()?)
    }

    pub fn save_permission(&self, project_id: &str, action: &str, resource: &str) -> Result<()> {
        self.conn.execute(
            "insert or ignore into permission_saved(project_id, action, resource) values (?1,?2,?3)",
            params![project_id, action, resource],
        )?;
        SAVED_PERMISSIONS_GEN.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        Ok(())
    }

    /// Forget one saved "always allow" rule. `false` if there was no such rule.
    pub fn remove_saved_permission(&self, project_id: &str, action: &str, resource: &str) -> Result<bool> {
        let n = self.conn.execute(
            "delete from permission_saved where project_id=?1 and action=?2 and resource=?3",
            params![project_id, action, resource],
        )?;
        SAVED_PERMISSIONS_GEN.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        crate::log_info!("store", "saved permission removed project={project_id} action={action} resource={resource} found={}", n > 0);
        Ok(n > 0)
    }

    /// Forget every saved "always allow" rule of a project. Returns how many there were.
    pub fn clear_saved_permissions(&self, project_id: &str) -> Result<usize> {
        let n = self.conn.execute("delete from permission_saved where project_id=?1", [project_id])?;
        SAVED_PERMISSIONS_GEN.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        crate::log_info!("store", "saved permissions cleared project={project_id} count={n}");
        Ok(n)
    }

    pub fn saved_permissions(&self, project_id: &str) -> Result<Vec<Rule>> {
        let mut stmt = self.conn.prepare("select action, resource from permission_saved where project_id=?1 order by rowid")?;
        let rows = stmt.query_map([project_id], |r| Ok(Rule { action: r.get(0)?, resource: r.get(1)?, effect: Effect::Allow, soft: false }))?;
        Ok(rows.collect::<std::result::Result<_, _>>()?)
    }

    /// Write-ahead execution claim, owned by this process. Returns false if the session is already claimed.
    pub fn claim(&self, session_id: &str) -> Result<bool> {
        let got = self.conn.execute(
            "update session set claim_at=?2, claim_pid=?3 where id=?1 and claim_at is null",
            params![session_id, now_ms(), std::process::id() as i64],
        )? > 0;
        crate::log_debug!("store", "claim session={session_id} {}", if got { "taken" } else { "refused: already claimed" });
        Ok(got)
    }

    /// Releases the claim, unless another live process has taken it over.
    pub fn release(&self, session_id: &str) -> Result<()> {
        self.conn.execute(
            "update session set claim_at=null, claim_pid=null, resume_attempts=0 where id=?1 and (claim_pid is null or claim_pid=?2)",
            params![session_id, std::process::id() as i64],
        )?;
        crate::log_debug!("store", "release session={session_id}");
        Ok(())
    }

    /// On startup: sessions left claimed by a process that is gone (a crash). Claims held by a live process are left
    /// alone. Top-level sessions get the attempt counter bumped and are re-owned by this process; those over
    /// `max_attempts` are released instead of resumed. Stale subagent claims are released. Returns the ids to resume.
    pub fn recover_claims(&self, max_attempts: i64) -> Result<Vec<String>> {
        let mut stmt = self.conn.prepare("select id, claim_pid, parent_id, resume_attempts from session where claim_at is not null")?;
        let rows: Vec<(String, Option<i64>, Option<String>, i64)> =
            stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?.collect::<std::result::Result<_, _>>()?;
        let me = std::process::id() as i64;
        let mut ids = Vec::new();
        for (id, pid, parent, attempts) in rows {
            if let Some(p) = pid.filter(|p| pid_alive(*p)) {
                crate::log_debug!("store", "recover: session={id} is claimed by live process {p}; leaving it");
                continue;
            }
            if parent.is_some() {
                self.conn.execute("update session set claim_at=null, claim_pid=null, resume_attempts=0 where id=?1", [&id])?;
                crate::log_warn!("store", "recover: subagent session={id} was left claimed by a crash; released");
            } else if attempts >= max_attempts {
                self.conn.execute("update session set claim_at=null, claim_pid=null where id=?1", [&id])?;
                crate::log_warn!("store", "recover: session={id} crashed {max_attempts} times in a row and will not be resumed again");
            } else {
                self.conn.execute("update session set resume_attempts=resume_attempts+1, claim_pid=?2 where id=?1", params![id, me])?;
                crate::log_warn!("store", "recover: session={id} was left claimed by a crash; resuming it");
                ids.push(id);
            }
        }
        Ok(ids)
    }

    /// Test helper: make the claim look like it belongs to a process that no longer exists.
    #[cfg(test)]
    pub(crate) fn orphan_claim(&self, session_id: &str) {
        self.conn.execute("update session set claim_pid=?2 where id=?1", params![session_id, i32::MAX as i64]).unwrap();
    }
}

/// Whether a process with this id exists (signal 0 only checks). EPERM means it exists but is someone else's.
fn pid_alive(pid: i64) -> bool {
    if pid <= 0 || pid > i32::MAX as i64 {
        return false;
    }
    unsafe { libc::kill(pid as i32, 0) == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM) }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn store() -> (Store, Session) {
        let s = Store::open_memory().unwrap();
        let ses = s.create_session("/work/proj", "build").unwrap();
        (s, ses)
    }

    #[test]
    fn snapshots_revert_marker_and_message_surgery() {
        let (s, ses) = store();
        assert_eq!(s.last_snapshot(&ses.id).unwrap(), None);
        let m1 = s.add_message(&ses.id, "user", &json!({"text":"one"})).unwrap();
        let a1 = s.add_message(&ses.id, "assistant", &json!({"parts":[],"snapshot":"snapA","usage":{"input":5}})).unwrap();
        let m2 = s.add_message(&ses.id, "user", &json!({"text":"two"})).unwrap();
        let a2 = s.add_message(&ses.id, "assistant", &json!({"parts":[],"snapshot":"snapB"})).unwrap();
        assert_eq!(s.last_snapshot(&ses.id).unwrap().as_deref(), Some("snapB"));
        s.set_message_parts(&a1, &[json!({"type":"text","text":"hi"})]).unwrap();
        let m = s.messages(&ses.id).unwrap();
        assert_eq!((m[1].data["parts"][0]["text"].as_str(), m[1].data["snapshot"].as_str(), m[1].data["usage"]["input"].as_u64()), (Some("hi"), Some("snapA"), Some(5)), "other fields survive");
        assert_eq!(s.revert(&ses.id).unwrap(), None);
        s.set_revert(&ses.id, Some(&json!({"messageId": m2, "redo": "snapX"}))).unwrap();
        assert_eq!(s.revert(&ses.id).unwrap().unwrap()["redo"], "snapX");
        let live = s.live_snapshots(&ses.id.replace("ses_", "prj_"), 0).unwrap();
        assert!(live.is_empty(), "other project");
        let live = s.live_snapshots(&ses.project_id, 0).unwrap();
        assert_eq!(live, ["snapA", "snapB", "snapX"].iter().map(|x| x.to_string()).collect());
        assert!(s.live_snapshots(&ses.project_id, i64::MAX).unwrap().contains("snapX"), "pending redo snapshots are always kept");
        assert_eq!(s.delete_messages_from(&ses.id, &m2).unwrap(), 2);
        assert_eq!(s.messages(&ses.id).unwrap().iter().map(|x| x.id.clone()).collect::<Vec<_>>(), [m1, a1]);
        let _ = a2;
        s.set_revert(&ses.id, None).unwrap();
        assert_eq!(s.revert(&ses.id).unwrap(), None);
    }

    #[test]
    fn listing_filters_by_directory_and_archive_state() {
        let s = Store::open_memory().unwrap();
        let a = s.create_session("/work/a", "build").unwrap();
        let b = s.create_session("/work/b", "plan").unwrap();
        let child = s.create_child_session(&a, "general").unwrap();
        s.add_message(&a.id, "user", &json!({"text":"hi"})).unwrap();
        s.add_message(&a.id, "assistant", &json!({"parts":[]})).unwrap();
        s.add_message(&a.id, "compaction", &json!({"summary":"x","keepFrom":"m"})).unwrap();
        s.set_title(&a.id, "Alpha").unwrap();
        s.add_message(&b.id, "user", &json!({"text":"yo"})).unwrap(); // b is now the most recently active
        let all = s.list_sessions(None, false, 50).unwrap();
        assert_eq!(all.iter().map(|x| x.id.as_str()).collect::<Vec<_>>(), [b.id.as_str(), a.id.as_str()], "children are never listed; newest activity first");
        let only_a = s.list_sessions(Some("/work/a"), false, 50).unwrap();
        assert_eq!((only_a.len(), only_a[0].title.as_deref(), only_a[0].messages, only_a[0].agent.as_str()), (1, Some("Alpha"), 2, "build"), "compaction messages are not counted");
        assert_eq!(s.list_sessions(None, false, 1).unwrap().len(), 1);
        s.set_archived(&a.id, true).unwrap();
        assert_eq!(s.list_sessions(None, false, 50).unwrap().len(), 1);
        let arch = s.list_sessions(None, true, 50).unwrap();
        assert_eq!((arch.len(), arch[0].archived), (1, true));
        assert_eq!(s.latest_session("/work/a").unwrap(), None, "an archived session is not the one --continue picks");
        s.set_archived(&a.id, false).unwrap();
        assert_eq!(s.list_sessions(Some("/work/a"), false, 50).unwrap().len(), 1);
        let _ = child;
    }

    #[test]
    fn activity_bumps_the_update_time() {
        let (s, ses) = store();
        let t0 = s.list_sessions(None, false, 5).unwrap()[0].time_updated;
        std::thread::sleep(std::time::Duration::from_millis(5));
        s.add_message(&ses.id, "user", &json!({"text":"x"})).unwrap();
        assert!(s.list_sessions(None, false, 5).unwrap()[0].time_updated > t0);
    }

    #[test]
    fn sessions_are_found_by_a_piece_of_their_id() {
        let (s, ses) = store();
        let other = s.create_session("/work/proj", "build").unwrap();
        assert_eq!(s.find_sessions(&ses.id).unwrap().len(), 1);
        let tail = &ses.id[ses.id.len() - 6..];
        assert_eq!(s.find_sessions(tail).unwrap().iter().map(|x| x.id.clone()).collect::<Vec<_>>(), std::slice::from_ref(&ses.id), "by suffix");
        assert!(!s.find_sessions(&ses.id[..8]).unwrap().is_empty(), "by prefix");
        assert_eq!(s.find_sessions("ses_").unwrap().len(), 2, "an ambiguous piece returns every match so the caller can refuse");
        assert!(s.find_sessions("").unwrap().is_empty() && s.find_sessions("zzzzzz").unwrap().is_empty());
        let child = s.create_child_session(&ses, "general").unwrap();
        assert!(s.find_sessions(&child.id).unwrap().is_empty(), "subagent sessions are not addressable");
        let _ = other;
    }

    #[test]
    fn title_helpers() {
        let (s, ses) = store();
        assert_eq!(s.title_source(&ses.id).unwrap(), None);
        s.enqueue(&ses.id, None, Mode::Steer, "user", &json!({"text":"first prompt"})).unwrap();
        assert_eq!(s.title_source(&ses.id).unwrap(), Some(("first prompt".into(), None)), "a prompt still in the inbox counts");
        let mut s = s;
        s.deliver(&ses.id, true).unwrap();
        s.add_message(&ses.id, "user", &json!({"text":"second prompt"})).unwrap();
        assert_eq!(s.title_source(&ses.id).unwrap(), Some(("first prompt".into(), Some("second prompt".into()))));
        assert!(s.set_title_if_empty(&ses.id, "Generated").unwrap());
        assert!(!s.set_title_if_empty(&ses.id, "Another").unwrap(), "never overwrites");
        s.set_title(&ses.id, "Chosen by me").unwrap();
        assert_eq!(s.session(&ses.id).unwrap().unwrap().title.as_deref(), Some("Chosen by me"));
    }

    #[test]
    fn forking_copies_the_conversation_and_what_hangs_off_it() {
        let (s, ses) = store();
        s.set_title(&ses.id, "Original").unwrap();
        s.set_model(&ses.id, Some("openai/gpt-x")).unwrap();
        let m1 = s.add_message(&ses.id, "user", &json!({"text":"one"})).unwrap();
        s.add_message(&ses.id, "assistant", &json!({"parts":[{"type":"text","text":"a1"}],"snapshot":"snap1"})).unwrap();
        let m3 = s.add_message(&ses.id, "user", &json!({"text":"two"})).unwrap();
        s.add_message(&ses.id, "compaction", &json!({"summary":"SUM","keepFrom":m3})).unwrap();
        let m5 = s.add_message(&ses.id, "user", &json!({"text":"three"})).unwrap();
        s.set_todos(&ses.id, &[crate::tools::Todo { content: "t".into(), status: "pending".into() }]).unwrap();
        s.add_instruction(&ses.id, "/w/AGENTS.md").unwrap();
        s.add_usage(&ses.id, 1.5, &crate::provider::Usage { input: 9, ..Default::default() }).unwrap();

        let f = s.fork_session(&ses.id, None).unwrap();
        assert_ne!(f.id, ses.id);
        assert_eq!((f.title.as_deref(), f.model.as_deref(), f.parent_id.clone(), f.agent.as_str()), (Some("Original (fork)"), Some("openai/gpt-x"), None, "build"));
        let (orig, copy) = (s.messages(&ses.id).unwrap(), s.messages(&f.id).unwrap());
        assert_eq!(orig.iter().map(|m| (&m.kind, &m.data["text"])).collect::<Vec<_>>().len(), copy.len());
        assert!(copy.iter().all(|c| orig.iter().all(|o| o.id != c.id)), "every copied message has a new id");
        assert!(copy.windows(2).all(|w| w[0].id < w[1].id), "and the new ids keep the order");
        assert_eq!(copy[1].data["snapshot"], "snap1", "snapshot ids carry over so undo works in the fork");
        let new_m3 = &copy[2].id;
        assert_eq!(copy[3].data["keepFrom"].as_str(), Some(new_m3.as_str()), "the checkpoint points into the copy, not the original");
        assert_eq!(s.todos(&f.id).unwrap().len(), 1);
        assert_eq!(s.instruction_paths(&f.id).unwrap(), ["/w/AGENTS.md"]);
        assert_eq!(s.usage_totals(&f.id).unwrap().0, 0.0, "a fork starts with no spend");
        let listed = s.list_sessions(None, false, 10).unwrap();
        assert_eq!(listed.iter().find(|x| x.id == f.id).unwrap().forked_from.as_deref(), Some(ses.id.as_str()));

        // Branch off before a message, and never copy what a pending undo hides.
        let before = s.fork_session(&ses.id, Some(&m3)).unwrap();
        assert_eq!(s.messages(&before.id).unwrap().len(), 2);
        s.set_revert(&ses.id, Some(&json!({"messageId": m5, "redo": null}))).unwrap();
        assert_eq!(s.messages(&s.fork_session(&ses.id, None).unwrap().id).unwrap().len(), 4, "the hidden last prompt stays behind");
        let _ = m1;
    }

    #[test]
    fn child_sessions_todos_and_depth() {
        use crate::tools::Todo;
        let (s, ses) = store();
        let child = s.create_child_session(&ses, "general").unwrap();
        let grandchild = s.create_child_session(&child, "general").unwrap();
        assert_eq!((child.parent_id.as_deref(), child.project_id.as_str(), child.directory.as_str()), (Some(ses.id.as_str()), ses.project_id.as_str(), "/work/proj"));
        assert_eq!((s.depth(&ses.id).unwrap(), s.depth(&child.id).unwrap(), s.depth(&grandchild.id).unwrap()), (0, 1, 2));
        assert_eq!(s.latest_session("/work/proj").unwrap().unwrap().id, ses.id, "children never show up as the latest session");
        assert_eq!(s.session(&child.id).unwrap().unwrap().parent_id.as_deref(), Some(ses.id.as_str()));
        let t = |c: &str, st: &str| Todo { content: c.into(), status: st.into() };
        s.set_todos(&ses.id, &[t("a", "pending"), t("b", "completed")]).unwrap();
        s.set_todos(&ses.id, &[t("c", "in_progress")]).unwrap();
        assert_eq!(s.todos(&ses.id).unwrap(), [t("c", "in_progress")], "replaced wholesale");
        assert!(s.todos(&child.id).unwrap().is_empty());
        // Usage rolls up: the child's spend shows in the parent's total, not the other way round.
        let u = crate::provider::Usage { input: 10, output: 5, ..Default::default() };
        s.add_usage(&child.id, 0.25, &u).unwrap();
        s.add_usage(&ses.id, 0.5, &u).unwrap();
        assert_eq!(s.usage_totals(&child.id).unwrap().0, 0.25);
        assert!((s.usage_totals(&ses.id).unwrap().0 - 0.75).abs() < 1e-9);
        assert_eq!(s.usage_totals(&ses.id).unwrap().1.input, 20);
    }

    #[test]
    fn model_and_usage_totals_accumulate() {
        let (s, ses) = store();
        assert_eq!(s.session(&ses.id).unwrap().unwrap().model, None);
        s.set_model(&ses.id, Some("openai/gpt-x")).unwrap();
        assert_eq!(s.session(&ses.id).unwrap().unwrap().model.as_deref(), Some("openai/gpt-x"));
        assert_eq!(s.latest_session("/work/proj").unwrap().unwrap().model.as_deref(), Some("openai/gpt-x"));
        let u = crate::provider::Usage { input: 10, output: 5, cache_read: 2, cache_write: 1 };
        s.add_usage(&ses.id, 0.25, &u).unwrap();
        s.add_usage(&ses.id, 0.5, &u).unwrap();
        let (cost, t) = s.usage_totals(&ses.id).unwrap();
        assert!((cost - 0.75).abs() < 1e-9);
        assert_eq!(t, crate::provider::Usage { input: 20, output: 10, cache_read: 4, cache_write: 2 });
        s.set_model(&ses.id, None).unwrap();
        assert_eq!(s.session(&ses.id).unwrap().unwrap().model, None);
    }

    #[test]
    fn project_is_stable_per_directory() {
        let (s, ses) = store();
        assert_eq!(s.project_for("/work/proj").unwrap(), ses.project_id);
        assert_ne!(s.project_for("/work/other").unwrap(), ses.project_id);
    }

    #[test]
    fn both_spellings_of_a_symlinked_directory_are_one_project() {
        let (s, _) = store();
        let base = std::env::temp_dir().join(crate::id::new_id("moco-store"));
        std::fs::create_dir_all(base.join("real/proj")).unwrap();
        std::os::unix::fs::symlink(base.join("real"), base.join("link")).unwrap();
        let (real, link) = (base.join("real/proj"), base.join("link/proj"));
        let a = s.create_session(real.to_str().unwrap(), "build").unwrap();
        let b = s.create_session(link.to_str().unwrap(), "build").unwrap();
        assert_eq!(a.project_id, b.project_id);
        assert_eq!(a.directory, b.directory);
        for dir in [&real, &link] {
            assert_eq!(s.list_sessions(Some(dir.to_str().unwrap()), false, 10).unwrap().len(), 2);
            assert_eq!(s.latest_session(dir.to_str().unwrap()).unwrap().unwrap().id, b.id);
        }
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn messages_round_trip_in_order() {
        let (s, ses) = store();
        let a = s.add_message(&ses.id, "user", &json!({"text": "hi"})).unwrap();
        let b = s.add_message(&ses.id, "assistant", &json!({"parts": []})).unwrap();
        s.update_message(&b, &json!({"parts": [{"type": "text", "text": "yo"}]})).unwrap();
        let m = s.messages(&ses.id).unwrap();
        assert_eq!(m.iter().map(|m| m.id.as_str()).collect::<Vec<_>>(), [a.as_str(), b.as_str()]);
        assert_eq!(m[1].data["parts"][0]["text"], "yo");
    }

    #[test]
    fn inbox_is_idempotent_and_cancellable() {
        let (mut s, ses) = store();
        s.enqueue(&ses.id, Some("ibx_1"), Mode::Steer, "user", &json!({"text": "first"})).unwrap();
        s.enqueue(&ses.id, Some("ibx_1"), Mode::Steer, "user", &json!({"text": "second"})).unwrap();
        assert_eq!(s.pending_inbox(&ses.id).unwrap(), 1);
        let ids = s.deliver(&ses.id, false).unwrap();
        assert_eq!(ids.len(), 1);
        assert_eq!(s.messages(&ses.id).unwrap()[0].data["text"], "first");
        assert_eq!(s.pending_inbox(&ses.id).unwrap(), 0);
        let id = s.enqueue(&ses.id, None, Mode::Queue, "user", &json!({})).unwrap();
        assert!(s.cancel_inbox(&id).unwrap());
        assert_eq!(s.pending_inbox(&ses.id).unwrap(), 0);
    }

    #[test]
    fn the_reasoning_level_is_stored_per_session_and_forks_drop_thinking_blocks() {
        let (s, ses) = store();
        assert_eq!(s.session(&ses.id).unwrap().unwrap().variant, None);
        s.set_variant(&ses.id, Some("high")).unwrap();
        assert_eq!(s.session(&ses.id).unwrap().unwrap().variant.as_deref(), Some("high"));
        s.add_message(&ses.id, "user", &json!({"text": "hi"})).unwrap();
        s.add_message(&ses.id, "assistant", &json!({"parts": [
            {"type": "native", "provider": "anthropic", "data": {"type": "thinking", "signature": "s"}},
            {"type": "native", "provider": "gemini", "data": {"call_id": "c"}},
            {"type": "text", "text": "hello"}]})).unwrap();
        let fork = s.fork_session(&ses.id, None).unwrap();
        assert_eq!(fork.variant.as_deref(), Some("high"), "the copy keeps the level");
        let parts = s.messages(&fork.id).unwrap()[1].data["parts"].clone();
        assert_eq!(parts, json!([{"type": "native", "provider": "gemini", "data": {"call_id": "c"}}, {"type": "text", "text": "hello"}]), "Anthropic thinking belongs to the conversation that produced it");
        assert_eq!(s.messages(&ses.id).unwrap()[1].data["parts"].as_array().unwrap().len(), 3, "the original is untouched");
        s.set_variant(&ses.id, None).unwrap();
        assert_eq!(s.session(&ses.id).unwrap().unwrap().variant, None);
    }

    #[test]
    fn inbox_items_can_be_listed_and_switched() {
        let (mut s, ses) = store();
        let a = s.enqueue(&ses.id, None, Mode::Steer, "user", &json!({"text": "first"})).unwrap();
        let b = s.enqueue(&ses.id, None, Mode::Queue, "synthetic", &json!({"text": "job done"})).unwrap();
        let items = s.inbox_items(&ses.id).unwrap();
        assert_eq!(items.iter().map(|i| (i.id.clone(), i.steer, i.kind.as_str(), i.text.as_str())).collect::<Vec<_>>(), vec![(a.clone(), true, "user", "first"), (b.clone(), false, "synthetic", "job done")]);
        assert!(s.set_inbox_mode(&a, Mode::Queue).unwrap());
        assert!(!s.inbox_items(&ses.id).unwrap()[0].steer);
        s.deliver(&ses.id, true).unwrap(); // idle: one queued item
        assert_eq!(s.inbox_items(&ses.id).unwrap().len(), 1);
        assert!(!s.set_inbox_mode(&a, Mode::Steer).unwrap()); // already delivered
    }

    #[test]
    fn delivery_modes() {
        let (mut s, ses) = store();
        for (m, t) in [(Mode::Queue, "q1"), (Mode::Queue, "q2"), (Mode::Steer, "s1"), (Mode::Steer, "s2")] {
            s.enqueue(&ses.id, None, m, "user", &json!({"text": t})).unwrap();
        }
        // Step boundary: only steers, in enqueue order.
        assert_eq!(s.deliver(&ses.id, false).unwrap().len(), 2);
        // Idle with no steers left: exactly one queued item.
        assert_eq!(s.deliver(&ses.id, true).unwrap().len(), 1);
        // A step boundary never delivers queued items.
        assert_eq!(s.deliver(&ses.id, false).unwrap().len(), 0);
        assert_eq!(s.deliver(&ses.id, true).unwrap().len(), 1);
        let texts: Vec<_> = s.messages(&ses.id).unwrap().iter().map(|m| m.data["text"].as_str().unwrap().to_string()).collect();
        assert_eq!(texts, ["s1", "s2", "q1", "q2"]);
    }

    #[test]
    fn saved_permissions_are_per_project_and_deduplicated() {
        let (s, ses) = store();
        s.save_permission(&ses.project_id, "shell", "git push *").unwrap();
        s.save_permission(&ses.project_id, "shell", "git push *").unwrap();
        s.save_permission("prj_other", "edit", "*").unwrap();
        let rules = s.saved_permissions(&ses.project_id).unwrap();
        assert_eq!(rules, [Rule::new("shell", "git push *", Effect::Allow)]);
    }

    #[test]
    fn automatic_allows_are_stored_without_credentials_and_forgotten_after_a_week() {
        let dir = std::env::temp_dir().join(new_id("moco-store"));
        let path = dir.join("moco.db");
        let ses = {
            let s = Store::open(&path).unwrap();
            let ses = s.create_session("/p", "build").unwrap();
            s.record_auto_allow(&ses.id, "export GITHUB_TOKEN=ghp_abcdef123456 && gh api user", "network", "", "m/a").unwrap();
            s.record_auto_allow(&ses.id, "mysql -u root -phunter2 -e 'select 1'", "read-only", "", "m/a").unwrap();
            let stored: Vec<String> = s.auto_allows(&ses.id).unwrap().into_iter().map(|a| a.command).collect();
            assert!(stored.iter().all(|c| !c.contains("ghp_abcdef123456") && !c.contains("hunter2")), "{stored:?}");
            assert_eq!(stored[0], "export GITHUB_TOKEN=[redacted] && gh api user");
            // Age one of them past the cutoff, the other to just inside it.
            let old = now_ms() - AUTO_ALLOW_KEEP_MS - 1000;
            let recent = now_ms() - AUTO_ALLOW_KEEP_MS + 60_000;
            s.conn.execute("update auto_allow set at=?1 where command like 'export%'", [old]).unwrap();
            s.conn.execute("update auto_allow set at=?1 where command like 'mysql%'", [recent]).unwrap();
            ses
        };
        let s = Store::open(&path).unwrap();
        let left: Vec<String> = s.auto_allows(&ses.id).unwrap().into_iter().map(|a| a.command).collect();
        assert_eq!(left.len(), 1, "the week-old row is pruned when the store opens: {left:?}");
        assert!(left[0].starts_with("mysql"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn snapshot_index_migrates_an_existing_database_and_keeps_last_snapshot() {
        let dir = std::env::temp_dir().join(new_id("moco-store"));
        let path = dir.join("moco.db");
        let ses = {
            let s = Store::open(&path).unwrap();
            let ses = s.create_session("/p", "build").unwrap();
            s.add_message(&ses.id, "assistant", &json!({"parts": [], "snapshot": "snap1"})).unwrap();
            s.add_message(&ses.id, "assistant", &json!({"parts": []})).unwrap();
            s.add_message(&ses.id, "assistant", &json!({"parts": [], "snapshot": "snap2"})).unwrap();
            s.add_message(&ses.id, "assistant", &json!({"parts": []})).unwrap();
            assert_eq!(s.last_snapshot(&ses.id).unwrap().as_deref(), Some("snap2"));
            // Back to a version 10 database: no index, no record of the migration.
            s.exec_for_test("drop index message_snapshot; delete from migrations where version=11;");
            assert_eq!(s.last_snapshot(&ses.id).unwrap().as_deref(), Some("snap2"));
            ses
        };
        let s = Store::open(&path).unwrap();
        let n: i64 = s.conn.query_row("select count(*) from sqlite_master where name='message_snapshot'", [], |r| r.get(0)).unwrap();
        assert_eq!(n, 1, "the index is created when the store opens");
        assert_eq!(s.last_snapshot(&ses.id).unwrap().as_deref(), Some("snap2"));
        let plan: String = s
            .conn
            .prepare("explain query plan select json_extract(data,'$.snapshot') from message where session_id=?1 and type='assistant' and json_extract(data,'$.snapshot') is not null order by id desc limit 1")
            .unwrap()
            .query_map([&ses.id], |r| r.get::<_, String>(3))
            .unwrap()
            .map(|r| r.unwrap())
            .collect::<Vec<_>>()
            .join(" | ");
        assert!(plan.contains("message_snapshot"), "{plan}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn claim_periodic_allows_one_run_per_interval() {
        let s = Store::open_memory().unwrap();
        assert!(s.claim_periodic("job", 60_000).unwrap());
        assert!(!s.claim_periodic("job", 60_000).unwrap());
        assert!(s.claim_periodic("other", 60_000).unwrap());
        assert!(s.claim_periodic("job", 0).unwrap(), "an interval that has passed claims again");
    }

    #[test]
    fn auto_allows_are_listed_oldest_first_with_the_subagents_below() {
        let s = Store::open_memory().unwrap();
        let top = s.create_session("/p", "build").unwrap();
        let other = s.create_session("/p", "build").unwrap();
        let child = s.create_child_session(&top, "general").unwrap();
        s.record_auto_allow(&top.id, "ls", "read-only", "", "m/a").unwrap();
        s.record_auto_allow(&child.id, "cargo test", "project-write", "tests", "m/a").unwrap();
        s.record_auto_allow(&other.id, "pwd", "read-only", "", "m/a").unwrap();
        let got: Vec<(String, String)> = s.auto_allows(&top.id).unwrap().into_iter().map(|a| (a.command, a.risk)).collect();
        assert_eq!(got, [("ls".to_string(), "read-only".to_string()), ("cargo test".to_string(), "project-write".to_string())]);
        assert_eq!(s.auto_allows(&child.id).unwrap().len(), 1, "a subagent sees its own");
        assert_eq!(s.auto_allows(&other.id).unwrap()[0].command, "pwd");
        s.record_auto_allow(&top.id, &"x".repeat(5000), "read-only", "", "m").unwrap();
        assert_eq!(s.auto_allows(&top.id).unwrap().last().unwrap().command.len(), 2000, "very long commands are cut");
    }

    #[test]
    fn claim_and_crash_recovery_with_attempt_cap() {
        let (s, ses) = store();
        assert!(s.claim(&ses.id).unwrap());
        assert!(!s.claim(&ses.id).unwrap(), "already claimed");
        // Simulated crash: claim remains, owner gone. Each restart resumes until the cap.
        s.orphan_claim(&ses.id);
        assert_eq!(s.recover_claims(2).unwrap(), std::slice::from_ref(&ses.id));
        s.orphan_claim(&ses.id);
        assert_eq!(s.recover_claims(2).unwrap(), std::slice::from_ref(&ses.id));
        s.orphan_claim(&ses.id);
        assert!(s.recover_claims(2).unwrap().is_empty(), "attempt cap reached, claim dropped");
        assert!(s.claim(&ses.id).unwrap());
        s.release(&ses.id).unwrap();
        assert!(s.recover_claims(2).unwrap().is_empty());
    }

    #[test]
    fn live_claim_of_another_handle_is_not_recovered() {
        let dir = std::env::temp_dir().join(new_id("moco-claim"));
        let path = dir.join("moco.db");
        let a = Store::open(&path).unwrap();
        let b = Store::open(&path).unwrap();
        let ses = a.create_session("/p", "build").unwrap();
        let child = a.create_child_session(&ses, "general").unwrap();
        assert!(a.claim(&ses.id).unwrap());
        // A second start (live owner) must neither resume the session nor count an attempt.
        for _ in 0..5 {
            assert!(b.recover_claims(3).unwrap().is_empty());
        }
        assert!(!b.claim(&ses.id).unwrap(), "still claimed by the live owner");
        // The owner dies: the top-level session is resumed, the stale subagent claim is released.
        assert!(a.claim(&child.id).unwrap());
        a.orphan_claim(&ses.id);
        a.orphan_claim(&child.id);
        assert_eq!(b.recover_claims(3).unwrap(), std::slice::from_ref(&ses.id));
        assert!(b.claim(&child.id).unwrap(), "stale subagent claim was released");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn file_store_reopens_with_data_and_wal() {
        let dir = std::env::temp_dir().join(new_id("moco-store"));
        let path = dir.join("moco.db");
        let ses_id = {
            let s = Store::open(&path).unwrap();
            let ses = s.create_session("/p", "build").unwrap();
            s.add_message(&ses.id, "user", &json!({"text": "persist"})).unwrap();
            ses.id
        };
        let s = Store::open(&path).unwrap(); // migrations are not re-run
        assert_eq!(s.messages(&ses_id).unwrap().len(), 1);
        assert_eq!(s.session(&ses_id).unwrap().unwrap().agent, "build");
        let _ = std::fs::remove_dir_all(dir);
    }
}
