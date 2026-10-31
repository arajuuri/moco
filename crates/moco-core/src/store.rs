//! SQLite store (docs/02-domain-model.md). WAL mode, migrations table, JSON columns for variable data.
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
}
pub type Result<T> = std::result::Result<T, StoreError>;

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
];

#[derive(Debug, Clone, PartialEq)]
pub struct Session {
    pub id: String,
    pub project_id: String,
    pub directory: String,
    pub agent: String,
    pub title: Option<String>,
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

pub struct Store {
    conn: Connection,
}

fn now_ms() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis() as i64).unwrap_or(0)
}

impl Store {
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let conn = Connection::open(path)?;
        conn.query_row("pragma journal_mode=wal", [], |_| Ok(()))?;
        Self::init(conn)
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
        Ok(Store { conn })
    }

    /// Project id is derived from the directory, so reopening the same directory finds the same project.
    pub fn project_for(&self, directory: &str) -> Result<String> {
        if let Some(id) = self.conn.query_row("select id from project where directory=?1", [directory], |r| r.get(0)).optional()? {
            return Ok(id);
        }
        let id = new_id("prj");
        let name = Path::new(directory).file_name().map(|n| n.to_string_lossy().into_owned());
        self.conn.execute(
            "insert into project(id, directory, name, time_created) values (?1,?2,?3,?4)",
            params![id, directory, name, now_ms()],
        )?;
        Ok(id)
    }

    pub fn create_session(&self, directory: &str, agent: &str) -> Result<Session> {
        let project_id = self.project_for(directory)?;
        let id = new_id("ses");
        let t = now_ms();
        self.conn.execute(
            "insert into session(id, project_id, directory, agent, time_created, time_updated) values (?1,?2,?3,?4,?5,?5)",
            params![id, project_id, directory, agent, t],
        )?;
        Ok(Session { id, project_id, directory: directory.into(), agent: agent.into(), title: None })
    }

    pub fn session(&self, id: &str) -> Result<Option<Session>> {
        Ok(self
            .conn
            .query_row("select id, project_id, directory, agent, title from session where id=?1", [id], |r| {
                Ok(Session { id: r.get(0)?, project_id: r.get(1)?, directory: r.get(2)?, agent: r.get(3)?, title: r.get(4)? })
            })
            .optional()?)
    }

    pub fn set_agent(&self, session_id: &str, agent: &str) -> Result<()> {
        self.conn.execute("update session set agent=?2, time_updated=?3 where id=?1", params![session_id, agent, now_ms()])?;
        Ok(())
    }

    /// Most recently updated, non-archived top-level session for a directory.
    pub fn latest_session(&self, directory: &str) -> Result<Option<Session>> {
        Ok(self
            .conn
            .query_row(
                "select id, project_id, directory, agent, title from session
                 where directory=?1 and parent_id is null and time_archived is null order by id desc limit 1",
                [directory],
                |r| Ok(Session { id: r.get(0)?, project_id: r.get(1)?, directory: r.get(2)?, agent: r.get(3)?, title: r.get(4)? }),
            )
            .optional()?)
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
        Ok(id)
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
        Ok(())
    }

    pub fn saved_permissions(&self, project_id: &str) -> Result<Vec<Rule>> {
        let mut stmt = self.conn.prepare("select action, resource from permission_saved where project_id=?1 order by rowid")?;
        let rows = stmt.query_map([project_id], |r| Ok(Rule { action: r.get(0)?, resource: r.get(1)?, effect: Effect::Allow }))?;
        Ok(rows.collect::<std::result::Result<_, _>>()?)
    }

    /// Write-ahead execution claim. Returns false if the session is already claimed.
    pub fn claim(&self, session_id: &str) -> Result<bool> {
        Ok(self.conn.execute("update session set claim_at=?2 where id=?1 and claim_at is null", params![session_id, now_ms()])? > 0)
    }

    pub fn release(&self, session_id: &str) -> Result<()> {
        self.conn.execute("update session set claim_at=null, resume_attempts=0 where id=?1", [session_id])?;
        Ok(())
    }

    /// On startup: sessions left claimed by a crash. Bumps the attempt counter; sessions over `max_attempts`
    /// are released instead of resumed. Returns the ids to resume.
    pub fn recover_claims(&self, max_attempts: i64) -> Result<Vec<String>> {
        self.conn.execute("update session set claim_at=null where claim_at is not null and resume_attempts>=?1", [max_attempts])?;
        let mut stmt = self.conn.prepare("select id from session where claim_at is not null and parent_id is null")?;
        let ids: Vec<String> = stmt.query_map([], |r| r.get(0))?.collect::<std::result::Result<_, _>>()?;
        for id in &ids {
            self.conn.execute("update session set resume_attempts=resume_attempts+1 where id=?1", [id])?;
        }
        Ok(ids)
    }
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
    fn project_is_stable_per_directory() {
        let (s, ses) = store();
        assert_eq!(s.project_for("/work/proj").unwrap(), ses.project_id);
        assert_ne!(s.project_for("/work/other").unwrap(), ses.project_id);
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
    fn claim_and_crash_recovery_with_attempt_cap() {
        let (s, ses) = store();
        assert!(s.claim(&ses.id).unwrap());
        assert!(!s.claim(&ses.id).unwrap(), "already claimed");
        // Simulated crash: claim remains. Each restart resumes until the cap.
        assert_eq!(s.recover_claims(2).unwrap(), [ses.id.clone()]);
        assert_eq!(s.recover_claims(2).unwrap(), [ses.id.clone()]);
        assert!(s.recover_claims(2).unwrap().is_empty(), "attempt cap reached, claim dropped");
        assert!(s.claim(&ses.id).unwrap());
        s.release(&ses.id).unwrap();
        assert!(s.recover_claims(2).unwrap().is_empty());
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
