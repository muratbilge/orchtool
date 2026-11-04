//! CLI side of the agent bus: timeline, messages, questions, Claude Code hook.

use anyhow::{bail, Result};
use orch_proto::api::{AgentRow, MessageRow, QuestionRow, Request, SendMessage, Stream};
use orch_proto::client::{register_request, Conn};
use std::io::Read;
use std::path::Path;
use std::time::Duration;

/// Local "HH:MM:SS" for a unix-ms timestamp.
pub(crate) fn clock(ts_ms: i64) -> String {
    let t = (ts_ms / 1000) as libc::time_t;
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    unsafe {
        libc::localtime_r(&t, &mut tm);
    }
    format!("{:02}:{:02}:{:02}", tm.tm_hour, tm.tm_min, tm.tm_sec)
}

fn color() -> bool {
    std::io::IsTerminal::is_terminal(&std::io::stdout()) && std::env::var_os("NO_COLOR").is_none()
}

fn row_line(m: &MessageRow) -> String {
    let c = color();
    let (dim, bold, reset) = if c {
        ("\x1b[2m", "\x1b[1m", "\x1b[m")
    } else {
        ("", "", "")
    };
    let tag = match (m.kind.as_str(), m.question) {
        ("ask", Some(q)) => format!(" {bold}❓#{q}{reset}"),
        ("answer", Some(q)) => format!(" {bold}✅#{q}{reset}"),
        ("msg", _) => String::new(),
        (k, _) => format!(" [{k}]"),
    };
    let thread = m
        .thread
        .as_deref()
        .map(|t| format!(" {dim}#{t}{reset}"))
        .unwrap_or_default();
    format!(
        "{dim}{}{reset} {bold}{}{reset} → {}{tag}{thread}: {}",
        clock(m.ts_ms),
        m.from,
        m.to,
        m.text.replace('\n', "\n    ")
    )
}

pub async fn agents(conn: &mut Conn, json: bool) -> Result<()> {
    let v = conn.request(&Request::ListAgents {}).await?;
    if json {
        println!("{}", serde_json::to_string_pretty(&v)?);
        return Ok(());
    }
    let rows: Vec<AgentRow> = serde_json::from_value(v["agents"].clone())?;
    println!("NAME                       KIND     STATUS    SEEN      PANE              NOTE");
    for a in rows {
        println!(
            "{:<26} {:<8} {:<9} {:<9} {:<17} {}",
            a.name,
            a.kind,
            a.status,
            clock(a.last_seen_ms),
            a.pane.unwrap_or_default(),
            a.status_text.unwrap_or_default()
        );
    }
    Ok(())
}

pub async fn tail(mut conn: Conn, follow: bool, lines: usize) -> Result<()> {
    if !follow {
        let v = conn
            .request(&Request::Log {
                after: None,
                limit: Some(lines),
                follow: false,
            })
            .await?;
        let rows: Vec<MessageRow> = serde_json::from_value(v["messages"].clone())?;
        for r in rows {
            println!("{}", row_line(&r));
        }
        return Ok(());
    }
    let (mut reader, mut writer) = conn.into_parts();
    let req = Request::Log {
        after: None,
        limit: Some(lines),
        follow: true,
    };
    writer
        .send_body("tail", serde_json::to_value(&req)?)
        .await?;
    while let Some(f) = reader.next().await? {
        if let Ok(Stream::Timeline { row }) = serde_json::from_value::<Stream>(f.body) {
            println!("{}", row_line(&row));
        }
    }
    Ok(())
}

pub async fn msg(conn: &mut Conn, from: &str, to: &str, text: &str) -> Result<()> {
    let v = conn
        .request(&Request::SendMessage(SendMessage {
            from: from.into(),
            to: to.into(),
            text: text.into(),
            thread: None,
        }))
        .await?;
    println!("sent #{}", v["id"]);
    Ok(())
}

pub async fn inbox(conn: &mut Conn, name: &str, wait: u64, peek: bool) -> Result<()> {
    let v = conn
        .request(&Request::Inbox {
            delivered: Vec::new(),
            name: name.into(),
            wait_ms: Some(wait * 1000),
        })
        .await?;
    let rows: Vec<MessageRow> = serde_json::from_value(v["messages"].clone())?;
    if rows.is_empty() {
        println!("no new messages");
    }
    for r in &rows {
        println!("{}", row_line(r));
    }
    if !peek && !rows.is_empty() {
        conn.request(&Request::Ack {
            name: name.into(),
            ids: rows.iter().map(|r| r.id).collect(),
        })
        .await?;
    }
    Ok(())
}

fn print_question(q: &QuestionRow) {
    println!(
        "#{} {} [{}] {}: {}",
        q.id,
        clock(q.ts_ms),
        q.state,
        q.from,
        q.text
    );
    for (i, o) in q.options.iter().enumerate() {
        println!("    {}) {o}", i + 1);
    }
    if let Some(a) = &q.answer {
        println!("    → {a} (by {})", q.answered_by.as_deref().unwrap_or("?"));
    }
}

pub async fn questions(conn: &mut Conn, all: bool) -> Result<()> {
    let v = conn.request(&Request::ListQuestions { all }).await?;
    let rows: Vec<QuestionRow> = serde_json::from_value(v["questions"].clone())?;
    if rows.is_empty() {
        println!("no open questions");
    }
    for q in rows {
        print_question(&q);
    }
    Ok(())
}

pub async fn answer(conn: &mut Conn, id: i64, text: &str, by: &str, literal: bool) -> Result<()> {
    // A bare number picks that option (unless --literal).
    let mut text = text.to_string();
    if let (false, Ok(n)) = (literal, text.trim().parse::<usize>()) {
        let v = conn.request(&Request::ListQuestions { all: true }).await?;
        let rows: Vec<QuestionRow> = serde_json::from_value(v["questions"].clone())?;
        if let Some(opt) = rows
            .iter()
            .find(|q| q.id == id)
            .and_then(|q| q.options.get(n.wrapping_sub(1)))
        {
            text = opt.clone();
        }
    }
    let v = conn
        .request(&Request::Answer {
            id,
            answer: text,
            by: by.into(),
        })
        .await?;
    let q: QuestionRow = serde_json::from_value(v["question"].clone())?;
    if v["won"].as_bool() == Some(true) {
        println!("answered #{id} → {}", q.from);
    } else {
        // Exit 11: first answer wins; the existing answer goes with the message.
        return Err(crate::exit::CliError::Answered(format!(
            "#{id} was already answered by {}: {}",
            q.answered_by.unwrap_or_default(),
            q.answer.unwrap_or_default()
        ))
        .into());
    }
    Ok(())
}

pub async fn ask(
    conn: &mut Conn,
    from: Option<String>,
    text: &str,
    options: Vec<String>,
    wait: u64,
) -> Result<()> {
    let from = match from {
        Some(f) => f,
        None => {
            let v = conn
                .request(&Request::RegisterAgent(register_request("shell")))
                .await?;
            v["name"].as_str().unwrap_or("shell").to_string()
        }
    };
    let q = conn
        .request(&Request::Ask {
            from,
            text: text.into(),
            options,
        })
        .await?;
    let id = q["id"].as_i64().unwrap_or(0);
    if wait == 0 {
        println!("#{id}");
        return Ok(());
    }
    let v = conn
        .request(&Request::AskWait {
            id,
            wait_ms: Some(wait * 1000),
        })
        .await?;
    let q: QuestionRow = serde_json::from_value(v)?;
    match q.answer {
        Some(a) => println!("{a}"),
        None => bail!("question #{id} not answered within {wait} s"),
    }
    Ok(())
}

/// Claude Code hook: keep the agent's status current and hand it unread bus messages when
/// it stops. Must never break Claude: any problem → exit quietly, print nothing.
/// Wired as `orch hook` for SessionStart, UserPromptSubmit, PreToolUse, Notification, Stop.
pub async fn hook(socket: &Path, kind: &str) {
    let mut input = String::new();
    let _ = std::io::stdin().take(1 << 20).read_to_string(&mut input);
    let ev: serde_json::Value = serde_json::from_str(&input).unwrap_or_default();
    let event = ev["hook_event_name"].as_str().unwrap_or("").to_string();
    let run = async {
        // Do not start orchd from a hook: if it is not running there is nothing to report.
        let mut conn = Conn::unix(socket).await.ok()?;
        let mut reg = register_request(kind);
        // Claude's session id: a stable identity when there is no pane.
        reg.instance = ev["session_id"]
            .as_str()
            .map(|s| format!("{kind}-session:{s}"));
        let me = conn.request(&Request::RegisterAgent(reg)).await.ok()?;
        let me = me["name"].as_str()?.to_string();
        let status = |s: &str, t: Option<String>| Request::SetStatus {
            name: me.clone(),
            status: s.into(),
            text: t,
        };
        match event.as_str() {
            "UserPromptSubmit" | "PreToolUse" | "PostToolUse" => {
                conn.request(&status("working", None)).await.ok()?;
            }
            "SessionStart" => {
                conn.request(&status("idle", Some("session started".into())))
                    .await
                    .ok()?;
            }
            "Notification" => {
                let text = ev["message"].as_str().unwrap_or("").to_string();
                let s = if text.to_lowercase().contains("permission") {
                    "blocked"
                } else {
                    "idle"
                };
                conn.request(&status(s, Some(text))).await.ok()?;
            }
            "Stop" => {
                let v = conn
                    .request(&Request::Inbox {
                        delivered: Vec::new(),
                        name: me.clone(),
                        wait_ms: Some(0),
                    })
                    .await
                    .ok()?;
                let rows: Vec<MessageRow> = serde_json::from_value(v["messages"].clone()).ok()?;
                if rows.is_empty() {
                    conn.request(&status("idle", None)).await.ok()?;
                    return Some(());
                }
                let mut reason = String::from("New messages for you on the orch bus:");
                for m in &rows {
                    reason.push_str(&format!("\n- from {} ({}): {}", m.from, m.kind, m.text));
                }
                reason.push_str("\nHandle them (reply with the orch send_message tool if needed).");
                // Hand over first, consume after: if anything below fails, the messages
                // stay unread and come again at the next Stop (at-least-once).
                {
                    use std::io::Write as _;
                    let mut out = std::io::stdout();
                    let line = serde_json::json!({ "decision": "block", "reason": reason });
                    writeln!(out, "{line}").ok()?;
                    out.flush().ok()?;
                }
                conn.request(&Request::Ack {
                    name: me.clone(),
                    ids: rows.iter().map(|r| r.id).collect(),
                })
                .await
                .ok()?;
                let _ = conn
                    .request(&status("working", Some("handling bus messages".into())))
                    .await;
            }
            _ => {}
        }
        Some(())
    };
    let _ = tokio::time::timeout(Duration::from_secs(3), run).await;
}
