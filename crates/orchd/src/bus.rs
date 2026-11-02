//! The agent bus: agents, messages (the timeline), questions for the human.
//!
//! Everything is persisted in SQLite *before* anyone is notified, so a crash or reconnect never
//! loses a question or an answer. The first answer to a question wins atomically
//! (`UPDATE … WHERE state = 'open'`); later answers get the existing one back.
//! SQLite calls are short and run under one mutex; at this scale (a handful of agents) that
//! is cheaper than a dedicated worker.

use anyhow::{Context, Result};
use orch_proto::api::{normalize_name, AgentRow, MessageRow, QuestionRow, RegisterAgent, HUMAN};
use rusqlite::{params, Connection, OptionalExtension};
use std::path::Path;
use std::sync::Mutex;
use std::time::Duration;
use tokio::sync::broadcast;

#[derive(Debug, Clone)]
pub enum BusEvent {
    Message(MessageRow),
    Question(QuestionRow),
    Agent(AgentRow),
}

pub struct Bus {
    db: Mutex<Connection>,
    tx: broadcast::Sender<BusEvent>,
}

pub fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

const SCHEMA: &str = "
PRAGMA journal_mode = WAL;
PRAGMA synchronous = NORMAL;
CREATE TABLE IF NOT EXISTS agents(
    name TEXT PRIMARY KEY, kind TEXT NOT NULL, pane TEXT, cwd TEXT,
    status TEXT NOT NULL DEFAULT 'unknown', status_text TEXT, last_seen INTEGER NOT NULL);
CREATE TABLE IF NOT EXISTS messages(
    id INTEGER PRIMARY KEY, ts INTEGER NOT NULL, sender TEXT NOT NULL, recipient TEXT NOT NULL,
    kind TEXT NOT NULL, text TEXT NOT NULL, thread TEXT, question INTEGER);
CREATE INDEX IF NOT EXISTS messages_recipient ON messages(recipient, id);
CREATE TABLE IF NOT EXISTS message_reads(
    message INTEGER NOT NULL, agent TEXT NOT NULL, PRIMARY KEY(message, agent));
CREATE TABLE IF NOT EXISTS questions(
    id INTEGER PRIMARY KEY, ts INTEGER NOT NULL, sender TEXT NOT NULL, text TEXT NOT NULL,
    options TEXT NOT NULL, state TEXT NOT NULL, answer TEXT, answered_by TEXT, answered_ts INTEGER);
CREATE TABLE IF NOT EXISTS kv(k TEXT PRIMARY KEY, v TEXT NOT NULL);
CREATE TABLE IF NOT EXISTS tg_map(
    tg_msg INTEGER PRIMARY KEY, question INTEGER, agent TEXT);
";

/// Message kinds that land in a recipient's inbox (status/event rows are timeline-only).
const INBOX_KINDS: &str = "('msg','ask','answer')";

impl Bus {
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let db = Connection::open(path).with_context(|| format!("open {}", path.display()))?;
        Self::with_connection(db)
    }

    pub fn in_memory() -> Result<Self> {
        Self::with_connection(Connection::open_in_memory()?)
    }

    fn with_connection(db: Connection) -> Result<Self> {
        db.busy_timeout(Duration::from_secs(5))?;
        db.execute_batch(SCHEMA)?;
        let (tx, _) = broadcast::channel(1024);
        Ok(Self {
            db: Mutex::new(db),
            tx,
        })
    }

    pub fn subscribe(&self) -> broadcast::Receiver<BusEvent> {
        self.tx.subscribe()
    }

    fn emit(&self, e: BusEvent) {
        let _ = self.tx.send(e);
    }

    // ---------- agents ----------

    /// Same pane → same name. Otherwise `<project>-<kind>`, numbered if a live agent in
    /// another pane already has it.
    pub fn register(&self, r: &RegisterAgent) -> Result<AgentRow> {
        let db = self.db.lock().unwrap();
        let now = now_ms();
        let name = if let Some(n) = r.name.as_deref().filter(|n| !n.trim().is_empty()) {
            normalize_name(n)
        } else if let Some(existing) = r
            .pane
            .as_ref()
            .map(|p| {
                db.query_row(
                    "SELECT name FROM agents WHERE pane = ?1 AND kind = ?2",
                    params![p, r.kind],
                    |row| row.get::<_, String>(0),
                )
                .optional()
            })
            .transpose()?
            .flatten()
        {
            existing
        } else {
            let project = r
                .cwd
                .as_deref()
                .and_then(|c| Path::new(c).file_name())
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_else(|| "agent".into());
            let base = sanitize(&format!("{project}-{}", r.kind));
            let mut candidate = base.clone();
            let mut n = 1;
            loop {
                let other: Option<(Option<String>, i64)> = db
                    .query_row(
                        "SELECT pane, last_seen FROM agents WHERE name = ?1",
                        params![candidate],
                        |row| Ok((row.get(0)?, row.get(1)?)),
                    )
                    .optional()?;
                match other {
                    None => break,
                    // Same pane, or a stale entry (not seen for 6 h): reuse the name.
                    Some((pane, seen)) if pane == r.pane || now - seen > 6 * 3600 * 1000 => break,
                    Some(_) => {
                        n += 1;
                        candidate = format!("{base}-{n}");
                    }
                }
            }
            candidate
        };
        if name == HUMAN || name == "all" {
            anyhow::bail!("{name:?} is reserved");
        }
        db.execute(
            "INSERT INTO agents(name, kind, pane, cwd, status, last_seen)
             VALUES (?1, ?2, ?3, ?4, 'unknown', ?5)
             ON CONFLICT(name) DO UPDATE SET kind = ?2, pane = COALESCE(?3, pane),
               cwd = COALESCE(?4, cwd), last_seen = ?5",
            params![name, r.kind, r.pane, r.cwd, now],
        )?;
        let row = agent_row(&db, &name)?.context("agent vanished")?;
        drop(db);
        self.emit(BusEvent::Agent(row.clone()));
        Ok(row)
    }

    pub fn set_status(&self, name: &str, status: &str, text: Option<&str>) -> Result<AgentRow> {
        let status = match status {
            "working" | "idle" | "blocked" | "done" | "unknown" => status,
            other => anyhow::bail!("unknown status {other:?} (working|idle|blocked|done)"),
        };
        let db = self.db.lock().unwrap();
        let changed = db.execute(
            "UPDATE agents SET status = ?2, status_text = ?3, last_seen = ?4 WHERE name = ?1",
            params![name, status, text, now_ms()],
        )?;
        if changed == 0 {
            anyhow::bail!("no agent named {name:?} (register first)");
        }
        let row = agent_row(&db, name)?.context("agent vanished")?;
        drop(db);
        self.emit(BusEvent::Agent(row.clone()));
        Ok(row)
    }

    pub fn touch(&self, name: &str) {
        let db = self.db.lock().unwrap();
        let _ = db.execute(
            "UPDATE agents SET last_seen = ?2 WHERE name = ?1",
            params![name, now_ms()],
        );
    }

    pub fn agent(&self, name: &str) -> Result<Option<AgentRow>> {
        agent_row(&self.db.lock().unwrap(), name)
    }

    pub fn agents(&self) -> Result<Vec<AgentRow>> {
        let db = self.db.lock().unwrap();
        let mut st = db.prepare(
            "SELECT name, kind, pane, cwd, status, status_text, last_seen FROM agents ORDER BY name",
        )?;
        let rows = st
            .query_map([], map_agent)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    // ---------- messages ----------

    pub fn send(
        &self,
        from: &str,
        to: &str,
        kind: &str,
        text: &str,
        thread: Option<&str>,
        question: Option<i64>,
    ) -> Result<MessageRow> {
        let from = normalize_name(from);
        let to = normalize_name(to);
        if text.trim().is_empty() {
            anyhow::bail!("empty message");
        }
        let db = self.db.lock().unwrap();
        if kind == "msg" && to != HUMAN && to != "all" && agent_row(&db, &to)?.is_none() {
            anyhow::bail!("no agent named {to:?} (see list_agents)");
        }
        let ts = now_ms();
        db.execute(
            "INSERT INTO messages(ts, sender, recipient, kind, text, thread, question)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![ts, from, to, kind, text, thread, question],
        )?;
        let row = MessageRow {
            id: db.last_insert_rowid(),
            ts_ms: ts,
            from,
            to,
            kind: kind.to_string(),
            text: text.to_string(),
            thread: thread.map(str::to_string),
            question,
        };
        drop(db);
        self.emit(BusEvent::Message(row.clone()));
        Ok(row)
    }

    /// Unread inbox rows for `name`; marks them read unless `peek`.
    pub fn inbox(&self, name: &str, peek: bool) -> Result<Vec<MessageRow>> {
        let name = normalize_name(name);
        let mut db = self.db.lock().unwrap();
        let tx = db.transaction()?;
        let rows = {
            let mut st = tx.prepare(&format!(
                "SELECT id, ts, sender, recipient, kind, text, thread, question FROM messages m
                 WHERE (recipient = ?1 OR (recipient = 'all' AND sender != ?1))
                   AND kind IN {INBOX_KINDS}
                   AND NOT EXISTS (SELECT 1 FROM message_reads r WHERE r.message = m.id AND r.agent = ?1)
                 ORDER BY id LIMIT 200"
            ))?;
            let rows = st
                .query_map(params![name], map_message)?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            rows
        };
        if !peek {
            for r in &rows {
                tx.execute(
                    "INSERT OR IGNORE INTO message_reads(message, agent) VALUES (?1, ?2)",
                    params![r.id, name],
                )?;
            }
            tx.execute(
                "UPDATE agents SET last_seen = ?2 WHERE name = ?1",
                params![name, now_ms()],
            )?;
        }
        tx.commit()?;
        Ok(rows)
    }

    /// Wait up to `wait` for unread inbox rows.
    pub async fn inbox_wait(
        &self,
        name: &str,
        peek: bool,
        wait: Duration,
    ) -> Result<Vec<MessageRow>> {
        let name = normalize_name(name);
        let mut rx = self.subscribe(); // before the first check: no lost wake-up
        let deadline = tokio::time::Instant::now() + wait;
        loop {
            let rows = self.inbox(&name, peek)?;
            if !rows.is_empty() || tokio::time::Instant::now() >= deadline {
                return Ok(rows);
            }
            loop {
                match tokio::time::timeout_at(deadline, rx.recv()).await {
                    Err(_) => return Ok(Vec::new()),
                    Ok(Ok(BusEvent::Message(m)))
                        if (m.to == name || (m.to == "all" && m.from != name))
                            && matches!(m.kind.as_str(), "msg" | "ask" | "answer") =>
                    {
                        break
                    }
                    Ok(Ok(_)) => continue,
                    Ok(Err(broadcast::error::RecvError::Lagged(_))) => break,
                    Ok(Err(broadcast::error::RecvError::Closed)) => return Ok(Vec::new()),
                }
            }
        }
    }

    pub fn log(&self, after: Option<i64>, limit: usize) -> Result<Vec<MessageRow>> {
        let db = self.db.lock().unwrap();
        let rows = if let Some(after) = after {
            let mut st = db.prepare(
                "SELECT id, ts, sender, recipient, kind, text, thread, question FROM messages
                 WHERE id > ?1 ORDER BY id LIMIT ?2",
            )?;
            let rows = st
                .query_map(params![after, limit as i64], map_message)?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            rows
        } else {
            let mut st = db.prepare(
                "SELECT * FROM (SELECT id, ts, sender, recipient, kind, text, thread, question
                 FROM messages ORDER BY id DESC LIMIT ?1) ORDER BY id",
            )?;
            let rows = st
                .query_map(params![limit as i64], map_message)?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            rows
        };
        Ok(rows)
    }

    // ---------- questions ----------

    pub fn ask(&self, from: &str, text: &str, options: &[String]) -> Result<QuestionRow> {
        let from = normalize_name(from);
        if text.trim().is_empty() {
            anyhow::bail!("empty question");
        }
        let options: Vec<String> = options
            .iter()
            .map(|o| o.trim().to_string())
            .filter(|o| !o.is_empty())
            .take(8)
            .collect();
        let q = {
            let db = self.db.lock().unwrap();
            let ts = now_ms();
            db.execute(
                "INSERT INTO questions(ts, sender, text, options, state) VALUES (?1, ?2, ?3, ?4, 'open')",
                params![ts, from, text, serde_json::to_string(&options)?],
            )?;
            question_row(&db, db.last_insert_rowid())?.context("question vanished")?
        };
        self.send(&from, HUMAN, "ask", text, None, Some(q.id))?;
        self.emit(BusEvent::Question(q.clone()));
        Ok(q)
    }

    pub fn question(&self, id: i64) -> Result<Option<QuestionRow>> {
        question_row(&self.db.lock().unwrap(), id)
    }

    pub fn questions(&self, all: bool) -> Result<Vec<QuestionRow>> {
        let db = self.db.lock().unwrap();
        let sql = if all {
            "SELECT id FROM questions ORDER BY id DESC LIMIT 100"
        } else {
            "SELECT id FROM questions WHERE state = 'open' ORDER BY id"
        };
        let ids: Vec<i64> = db
            .prepare(sql)?
            .query_map([], |r| r.get(0))?
            .collect::<rusqlite::Result<_>>()?;
        ids.into_iter()
            .filter_map(|id| question_row(&db, id).transpose())
            .collect()
    }

    /// Returns (won, question). Only the winner produces an "answer" message.
    pub fn answer(&self, id: i64, answer: &str, by: &str) -> Result<(bool, QuestionRow)> {
        if answer.trim().is_empty() {
            anyhow::bail!("empty answer");
        }
        let by = normalize_name(by);
        let (won, q) = {
            let db = self.db.lock().unwrap();
            let won = db.execute(
                "UPDATE questions SET state = 'answered', answer = ?2, answered_by = ?3, answered_ts = ?4
                 WHERE id = ?1 AND state = 'open'",
                params![id, answer, by, now_ms()],
            )? == 1;
            let q = question_row(&db, id)?.with_context(|| format!("no question {id}"))?;
            (won, q)
        };
        if won {
            self.send(HUMAN, &q.from, "answer", answer, None, Some(id))?;
            self.emit(BusEvent::Question(q.clone()));
        }
        Ok((won, q))
    }

    /// Wait up to `wait` for question `id` to be answered.
    pub async fn ask_wait(&self, id: i64, wait: Duration) -> Result<QuestionRow> {
        let mut rx = self.subscribe();
        let deadline = tokio::time::Instant::now() + wait;
        loop {
            let q = self
                .question(id)?
                .with_context(|| format!("no question {id}"))?;
            if q.state != "open" || tokio::time::Instant::now() >= deadline {
                return Ok(q);
            }
            loop {
                match tokio::time::timeout_at(deadline, rx.recv()).await {
                    Err(_) => break,
                    Ok(Ok(BusEvent::Question(q))) if q.id == id => break,
                    Ok(Ok(_)) => continue,
                    Ok(Err(broadcast::error::RecvError::Lagged(_))) => break,
                    Ok(Err(broadcast::error::RecvError::Closed)) => break,
                }
            }
        }
    }

    // ---------- small key/value + telegram message map ----------

    pub fn kv_get(&self, k: &str) -> Result<Option<String>> {
        Ok(self
            .db
            .lock()
            .unwrap()
            .query_row("SELECT v FROM kv WHERE k = ?1", params![k], |r| r.get(0))
            .optional()?)
    }

    pub fn kv_set(&self, k: &str, v: &str) -> Result<()> {
        self.db.lock().unwrap().execute(
            "INSERT INTO kv(k, v) VALUES (?1, ?2) ON CONFLICT(k) DO UPDATE SET v = ?2",
            params![k, v],
        )?;
        Ok(())
    }

    pub fn kv_del(&self, k: &str) -> Result<()> {
        self.db
            .lock()
            .unwrap()
            .execute("DELETE FROM kv WHERE k = ?1", params![k])?;
        Ok(())
    }

    pub fn tg_map_set(
        &self,
        tg_msg: i64,
        question: Option<i64>,
        agent: Option<&str>,
    ) -> Result<()> {
        self.db.lock().unwrap().execute(
            "INSERT OR REPLACE INTO tg_map(tg_msg, question, agent) VALUES (?1, ?2, ?3)",
            params![tg_msg, question, agent],
        )?;
        Ok(())
    }

    pub fn tg_map_get(&self, tg_msg: i64) -> Result<Option<(Option<i64>, Option<String>)>> {
        Ok(self
            .db
            .lock()
            .unwrap()
            .query_row(
                "SELECT question, agent FROM tg_map WHERE tg_msg = ?1",
                params![tg_msg],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?)
    }

    pub fn tg_msgs_for_question(&self, question: i64) -> Result<Vec<i64>> {
        let db = self.db.lock().unwrap();
        let rows = db
            .prepare("SELECT tg_msg FROM tg_map WHERE question = ?1")?
            .query_map(params![question], |r| r.get(0))?
            .collect::<rusqlite::Result<Vec<i64>>>()?;
        Ok(rows)
    }
}

fn sanitize(s: &str) -> String {
    s.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.' {
                c.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect()
}

fn map_agent(r: &rusqlite::Row) -> rusqlite::Result<AgentRow> {
    Ok(AgentRow {
        name: r.get(0)?,
        kind: r.get(1)?,
        pane: r.get(2)?,
        cwd: r.get(3)?,
        status: r.get(4)?,
        status_text: r.get(5)?,
        last_seen_ms: r.get(6)?,
    })
}

fn agent_row(db: &Connection, name: &str) -> Result<Option<AgentRow>> {
    Ok(db
        .query_row(
            "SELECT name, kind, pane, cwd, status, status_text, last_seen FROM agents WHERE name = ?1",
            params![name],
            map_agent,
        )
        .optional()?)
}

fn map_message(r: &rusqlite::Row) -> rusqlite::Result<MessageRow> {
    Ok(MessageRow {
        id: r.get(0)?,
        ts_ms: r.get(1)?,
        from: r.get(2)?,
        to: r.get(3)?,
        kind: r.get(4)?,
        text: r.get(5)?,
        thread: r.get(6)?,
        question: r.get(7)?,
    })
}

fn question_row(db: &Connection, id: i64) -> Result<Option<QuestionRow>> {
    Ok(db
        .query_row(
            "SELECT id, ts, sender, text, options, state, answer, answered_by, answered_ts
             FROM questions WHERE id = ?1",
            params![id],
            |r| {
                let options: String = r.get(4)?;
                Ok(QuestionRow {
                    id: r.get(0)?,
                    ts_ms: r.get(1)?,
                    from: r.get(2)?,
                    text: r.get(3)?,
                    options: serde_json::from_str(&options).unwrap_or_default(),
                    state: r.get(5)?,
                    answer: r.get(6)?,
                    answered_by: r.get(7)?,
                    answered_ts_ms: r.get(8)?,
                })
            },
        )
        .optional()?)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn reg(bus: &Bus, kind: &str, pane: &str, cwd: &str) -> String {
        bus.register(&RegisterAgent {
            kind: kind.into(),
            pane: Some(pane.into()),
            cwd: Some(cwd.into()),
            name: None,
        })
        .unwrap()
        .name
    }

    #[test]
    fn names_follow_panes_and_do_not_collide() {
        let bus = Bus::in_memory().unwrap();
        assert_eq!(
            reg(&bus, "claude", "orch:p1", "/projects/Paperscanner"),
            "paperscanner-claude"
        );
        // Same pane again → same name.
        assert_eq!(
            reg(&bus, "claude", "orch:p1", "/projects/paperscanner"),
            "paperscanner-claude"
        );
        // Another live pane in the same project → numbered.
        assert_eq!(
            reg(&bus, "claude", "herdr:w1:p1", "/projects/paperscanner"),
            "paperscanner-claude-2"
        );
        assert_eq!(
            reg(&bus, "codex", "orch:p2", "/projects/paperscanner"),
            "paperscanner-codex"
        );
    }

    #[test]
    fn inbox_marks_read_and_broadcast_reaches_others() {
        let bus = Bus::in_memory().unwrap();
        let a = reg(&bus, "claude", "orch:p1", "/x/proj");
        let b = reg(&bus, "codex", "orch:p2", "/x/proj");
        bus.send(&a, &b, "msg", "hello b", None, None).unwrap();
        bus.send(&b, "all", "msg", "hello everyone", None, None)
            .unwrap();
        assert!(bus.send(&a, "nobody", "msg", "x", None, None).is_err());
        let inbox_b = bus.inbox(&b, true).unwrap();
        assert_eq!(inbox_b.len(), 1, "own broadcast is not in own inbox");
        assert_eq!(bus.inbox(&b, false).unwrap().len(), 1);
        assert!(bus.inbox(&b, false).unwrap().is_empty(), "read once");
        let inbox_a = bus.inbox(&a, false).unwrap();
        assert_eq!(inbox_a.len(), 1);
        assert_eq!(inbox_a[0].text, "hello everyone");
        assert_eq!(bus.log(None, 10).unwrap().len(), 2);
    }

    #[test]
    fn first_answer_wins_and_reaches_the_asker() {
        let bus = Bus::in_memory().unwrap();
        let a = reg(&bus, "claude", "orch:p1", "/x/proj");
        let q = bus
            .ask(&a, "Deploy now?", &["yes".into(), "no".into()])
            .unwrap();
        assert_eq!(bus.questions(false).unwrap().len(), 1);
        // The human's inbox has the question.
        assert_eq!(bus.inbox(HUMAN, false).unwrap()[0].question, Some(q.id));
        let (won, _) = bus.answer(q.id, "yes", "telegram").unwrap();
        assert!(won);
        let (won2, q2) = bus.answer(q.id, "no", "cli").unwrap();
        assert!(!won2);
        assert_eq!(q2.answer.as_deref(), Some("yes"));
        assert!(bus.questions(false).unwrap().is_empty());
        let inbox = bus.inbox(&a, false).unwrap();
        assert_eq!(inbox.len(), 1, "exactly one answer message");
        assert_eq!(
            (inbox[0].kind.as_str(), inbox[0].text.as_str()),
            ("answer", "yes")
        );
    }

    #[tokio::test]
    async fn waits_wake_up_on_new_rows() {
        let bus = std::sync::Arc::new(Bus::in_memory().unwrap());
        let a = reg(&bus, "claude", "orch:p1", "/x/proj");
        let b = reg(&bus, "codex", "orch:p2", "/x/proj");
        let q = bus.ask(&a, "ok?", &[]).unwrap();
        let (b2, bus2) = (b.clone(), bus.clone());
        let waiter = tokio::spawn(async move {
            bus2.inbox_wait(&b2, false, Duration::from_secs(5))
                .await
                .unwrap()
        });
        let bus3 = bus.clone();
        let qwait =
            tokio::spawn(async move { bus3.ask_wait(q.id, Duration::from_secs(5)).await.unwrap() });
        tokio::time::sleep(Duration::from_millis(50)).await;
        bus.send(&a, &b, "msg", "ping", None, None).unwrap();
        bus.answer(q.id, "fine", "cli").unwrap();
        assert_eq!(waiter.await.unwrap()[0].text, "ping");
        assert_eq!(qwait.await.unwrap().answer.as_deref(), Some("fine"));
        // A wait on an open question times out and returns it still open.
        let q2 = bus.ask(&a, "later?", &[]).unwrap();
        let r = bus
            .ask_wait(q2.id, Duration::from_millis(50))
            .await
            .unwrap();
        assert_eq!(r.state, "open");
    }

    #[test]
    fn survives_reopen() {
        let dir = std::env::temp_dir().join(format!("orch-bus-test-{}", std::process::id()));
        let path = dir.join("bus.db");
        let _ = std::fs::remove_dir_all(&dir);
        let id = {
            let bus = Bus::open(&path).unwrap();
            let a = reg(&bus, "claude", "orch:p1", "/x/proj");
            bus.ask(&a, "persist me", &[]).unwrap().id
        };
        let bus = Bus::open(&path).unwrap();
        assert_eq!(bus.question(id).unwrap().unwrap().text, "persist me");
        assert!(bus.answer(id, "done", "cli").unwrap().0);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
