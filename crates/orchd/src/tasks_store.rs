//! Task transitions and their human notifications commit together in the bus database.
use crate::bus::{insert_message, now_ms, Bus, BusEvent};
use anyhow::{bail, Result};
use orch_proto::api::{TaskRow, HUMAN};
use rusqlite::{params, Connection, OptionalExtension};

pub(crate) const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS tasks(
 id INTEGER PRIMARY KEY, target TEXT NOT NULL, text TEXT NOT NULL,
 state TEXT NOT NULL CHECK(state IN ('queued','running','done','failed','cancelled')),
 created_by TEXT NOT NULL, created_ms INTEGER NOT NULL, started_ms INTEGER, finished_ms INTEGER,
 pane TEXT, after_id INTEGER REFERENCES tasks(id), error TEXT, saw_working INTEGER NOT NULL DEFAULT 0);
CREATE UNIQUE INDEX IF NOT EXISTS task_running_target ON tasks(target) WHERE state='running';
CREATE UNIQUE INDEX IF NOT EXISTS task_running_pane ON tasks(pane) WHERE state='running';
CREATE TABLE IF NOT EXISTS task_mcp(pane TEXT PRIMARY KEY);
";
const COLS: &str = "id,target,text,state,created_by,created_ms,started_ms,finished_ms,pane,after_id,error,saw_working";
fn row(r: &rusqlite::Row<'_>) -> rusqlite::Result<TaskRow> {
    Ok(TaskRow {
        id: r.get(0)?,
        target: r.get(1)?,
        text: r.get(2)?,
        state: r.get(3)?,
        created_by: r.get(4)?,
        created_ms: r.get(5)?,
        started_ms: r.get(6)?,
        finished_ms: r.get(7)?,
        pane: r.get(8)?,
        after: r.get(9)?,
        error: r.get(10)?,
        saw_working: r.get(11)?,
    })
}
fn get(db: &Connection, id: i64) -> Result<Option<TaskRow>> {
    Ok(db
        .query_row(&format!("SELECT {COLS} FROM tasks WHERE id=?1"), [id], row)
        .optional()?)
}
impl Bus {
    pub fn task(&self, id: i64) -> Result<Option<TaskRow>> {
        get(&self.db.lock().unwrap(), id)
    }
    pub fn tasks(&self, all: bool) -> Result<Vec<TaskRow>> {
        let db = self.db.lock().unwrap();
        let mut q=db.prepare(&format!("SELECT {COLS} FROM tasks WHERE ?1 OR state IN ('queued','running') OR id IN (SELECT id FROM tasks WHERE state NOT IN ('queued','running') ORDER BY id DESC LIMIT 20) ORDER BY id"))?;
        let rows = q.query_map([all], row)?.collect::<rusqlite::Result<_>>()?;
        Ok(rows)
    }
    pub fn task_add(
        &self,
        target: &str,
        text: &str,
        creator: &str,
        after: Option<i64>,
    ) -> Result<TaskRow> {
        let db = self.db.lock().unwrap();
        if let Some(id) = after {
            if get(&db, id)?.is_none() {
                bail!("unknown dependency {id}");
            }
        }
        db.execute("INSERT INTO tasks(target,text,state,created_by,created_ms,after_id) VALUES(?1,?2,'queued',?3,?4,?5)",params![target,text,creator,now_ms(),after])?;
        let t = get(&db, db.last_insert_rowid())?.unwrap();
        drop(db);
        self.emit(BusEvent::Tasks);
        Ok(t)
    }
    /// Claim only eligible queued work, atomically with one-running-per-target AND pane.
    pub fn task_claim(&self, id: i64, pane: &str) -> Result<Option<TaskRow>> {
        self.task_transition(id, "running", Some(pane), None)
    }
    pub fn task_finish(
        &self,
        id: i64,
        state: &str,
        error: Option<&str>,
    ) -> Result<Option<TaskRow>> {
        if !matches!(state, "done" | "failed" | "cancelled") {
            bail!("invalid terminal state");
        }
        self.task_transition(id, state, None, error)
    }
    fn task_transition(
        &self,
        id: i64,
        state: &str,
        pane: Option<&str>,
        error: Option<&str>,
    ) -> Result<Option<TaskRow>> {
        let mut db = self.db.lock().unwrap();
        let tx = db.transaction()?;
        let changed = if state == "running" {
            tx.execute("UPDATE tasks SET state='running',pane=?2,started_ms=?3 WHERE id=?1 AND state='queued'
             AND (after_id IS NULL OR EXISTS(SELECT 1 FROM tasks dep WHERE dep.id=tasks.after_id AND dep.state='done'))
             AND NOT EXISTS(SELECT 1 FROM tasks active WHERE active.state='running' AND (active.target=tasks.target OR active.pane=?2))",params![id,pane,now_ms()])?
        } else {
            tx.execute("UPDATE tasks SET state=?2,finished_ms=?3,error=?4 WHERE id=?1 AND (state='running' OR (state='queued' AND ?2='cancelled'))",params![id,state,now_ms(),error])?
        };
        if changed == 0 {
            return Ok(None);
        }
        let t = get(&tx, id)?.unwrap();
        let msg = insert_message(
            &tx,
            "orchd",
            HUMAN,
            "msg",
            &format!(
                "Task #{} {} → {}{}",
                t.id,
                t.target,
                t.state,
                error.map(|e| format!(": {e}")).unwrap_or_default()
            ),
            Some("tasks"),
            None,
        )?;
        tx.commit()?;
        drop(db);
        self.emit(BusEvent::Message(msg));
        self.emit(BusEvent::Tasks);
        Ok(Some(t))
    }
    pub fn task_working(&self, id: i64) -> Result<()> {
        self.db.lock().unwrap().execute(
            "UPDATE tasks SET saw_working=1 WHERE id=?1 AND state='running'",
            [id],
        )?;
        Ok(())
    }
    pub fn task_paused(&self, id: i64) -> Result<()> {
        self.db.lock().unwrap().execute(
            "UPDATE tasks SET saw_working=0 WHERE id=?1 AND state='running' AND saw_working=1",
            [id],
        )?;
        Ok(())
    }
    pub fn task_input_uncertain(&self, pane: &str) -> Result<bool> {
        Ok(self.db.lock().unwrap().prepare("SELECT 1 FROM tasks WHERE pane=?1 AND state='failed' AND error LIKE 'input interrupted%'")?.exists([pane])?)
    }
    pub fn task_mcp(&self, pane: &str) -> Result<bool> {
        Ok(self
            .db
            .lock()
            .unwrap()
            .prepare("SELECT 1 FROM task_mcp WHERE pane=?1")?
            .exists([pane])?)
    }
    pub fn task_register_mcp(&self, pane: &str) -> Result<()> {
        self.db
            .lock()
            .unwrap()
            .execute("INSERT OR IGNORE INTO task_mcp(pane) VALUES(?1)", [pane])?;
        Ok(())
    }
    /// Restore maps panes before the dispatcher starts. Never replay running input. Require
    /// fresh work after startup (the initial prompt must not prematurely finish an old task).
    pub fn task_rebind(&self, old: &str, new: &str) -> Result<()> {
        let mut db = self.db.lock().unwrap();
        let tx = db.transaction()?;
        tx.execute(
            "UPDATE tasks SET pane=?2,saw_working=0 WHERE pane=?1 AND state='running'",
            params![old, new],
        )?;
        tx.execute(
            "INSERT OR IGNORE INTO task_mcp(pane) SELECT ?2 FROM task_mcp WHERE pane=?1",
            params![old, new],
        )?;
        tx.commit()?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn migration_reopen_and_concurrent_claims() {
        let path = std::env::temp_dir().join(format!(
            "orch-task-store-{}-{}.db",
            std::process::id(),
            now_ms()
        ));
        // A pre-task database is migrated on open, without replacing existing bus rows.
        {
            let db = Connection::open(&path).unwrap();
            db.execute_batch("CREATE TABLE kv(k TEXT PRIMARY KEY,v TEXT NOT NULL); INSERT INTO kv VALUES('keep','yes')").unwrap();
        }
        let b = Bus::open(&path).unwrap();
        let first = b.task_add("w", "first", "test", None).unwrap();
        let second = b.task_add("w", "second", "test", Some(first.id)).unwrap();
        b.task_claim(first.id, "old").unwrap();
        b.task_register_mcp("old").unwrap();
        drop(b);
        let b = std::sync::Arc::new(Bus::open(&path).unwrap());
        assert_eq!(b.task(first.id).unwrap().unwrap().state, "running");
        assert_eq!(b.task(second.id).unwrap().unwrap().after, Some(first.id));
        b.task_rebind("old", "new").unwrap();
        assert!(b.task_mcp("new").unwrap());
        b.task_finish(first.id, "done", None).unwrap();
        let third = b.task_add("w", "third", "test", None).unwrap();
        let workers: Vec<_> = [second.id, third.id]
            .into_iter()
            .map(|id| {
                let b = std::sync::Arc::clone(&b);
                std::thread::spawn(move || b.task_claim(id, &format!("p{id}")).unwrap().is_some())
            })
            .collect();
        assert_eq!(
            workers
                .into_iter()
                .filter_map(|t| t.join().ok())
                .filter(|v| *v)
                .count(),
            1
        );
        assert_eq!(
            b.db.lock()
                .unwrap()
                .query_row("SELECT v FROM kv WHERE k='keep'", [], |r| r
                    .get::<_, String>(0))
                .unwrap(),
            "yes"
        );
        drop(b);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn dependencies_claims_and_terminal_states() {
        let b = Bus::in_memory().unwrap();
        let a = b.task_add("w", "first", "murat", None).unwrap();
        let c = b.task_add("w@t2", "next", "murat", Some(a.id)).unwrap();
        assert!(b.task_claim(c.id, "p2").unwrap().is_none());
        assert!(b.task_claim(a.id, "p1").unwrap().is_some());
        let other = b.task_add("other", "other", "murat", None).unwrap();
        assert!(b.task_claim(other.id, "p1").unwrap().is_none());
        let same = b.task_add("w", "same", "murat", None).unwrap();
        assert!(b.task_claim(same.id, "p3").unwrap().is_none());
        b.task_working(a.id).unwrap();
        b.task_paused(a.id).unwrap();
        assert!(!b.task(a.id).unwrap().unwrap().saw_working);
        assert_eq!(b.task(a.id).unwrap().unwrap().state, "running");
        b.task_working(a.id).unwrap();
        b.task_rebind("p1", "p9").unwrap();
        assert!(!b.task(a.id).unwrap().unwrap().saw_working);
        assert_eq!(b.task(a.id).unwrap().unwrap().pane.as_deref(), Some("p9"));
        assert!(b.task_finish(a.id, "done", None).unwrap().is_some());
        assert!(b.task_finish(a.id, "failed", None).unwrap().is_none());
        assert!(b.task_claim(c.id, "p2").unwrap().is_some());
        assert!(b.task_finish(c.id, "cancelled", None).unwrap().is_some());
        assert!(b.task_claim(c.id, "p2").unwrap().is_none());
        let blocked = b.task_add("w", "blocked", "murat", Some(c.id)).unwrap();
        assert!(b.task_claim(blocked.id, "p4").unwrap().is_none());
        assert!(b.task_add("w", "bad", "murat", Some(999)).is_err());
        assert!(b.task_finish(other.id, "done", None).unwrap().is_none());
        assert_eq!(
            b.log(None, 100)
                .unwrap()
                .iter()
                .filter(|m| m.thread.as_deref() == Some("tasks"))
                .count(),
            4
        );
    }
}
