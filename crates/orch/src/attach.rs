//! Full-screen attach: raw terminal, server frames → stdout, keys → pane.

use anyhow::{bail, Result};
use base64::Engine as _;
use orch_proto::api::{AttachMode, AttachPane, Request, Role, Stream};
use orch_proto::client::{reply_result, Conn};
use std::io::{Read, Write};
use std::time::{Duration, Instant};
use tokio::signal::unix::{signal, SignalKind};

/// Prefix key byte from $ORCH_PREFIX ("C-b" default, "C-a", …).
fn prefix_byte() -> u8 {
    let p = std::env::var("ORCH_PREFIX").unwrap_or_else(|_| "C-b".into());
    match p.strip_prefix("C-").and_then(|c| c.bytes().next()) {
        Some(c) if c.is_ascii_alphabetic() => c.to_ascii_lowercase() - b'a' + 1,
        _ => 0x02,
    }
}

fn b64(bytes: &[u8]) -> String {
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

struct RawGuard;

impl RawGuard {
    fn enter() -> Result<Self> {
        crossterm::terminal::enable_raw_mode()?;
        let mut out = std::io::stdout();
        out.write_all(b"\x1b[?1049h\x1b[H\x1b[2J")?;
        out.flush()?;
        Ok(RawGuard)
    }
}

impl Drop for RawGuard {
    fn drop(&mut self) {
        let mut out = std::io::stdout();
        // Undo every mode a pane may have switched on in our terminal.
        let _ = out.write_all(
            b"\x1b[m\x1b[?1l\x1b>\x1b[?2004l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1005l\x1b[?1006l\x1b[0 q\x1b[?25h\x1b[?1049l",
        );
        let _ = out.flush();
        let _ = crossterm::terminal::disable_raw_mode();
    }
}

enum Outcome {
    Detached,
    Exited(Option<u32>),
    Closed,
}

pub async fn run(conn: Conn, pane: &str, mode: AttachMode) -> Result<()> {
    let (cols, rows) = crossterm::terminal::size().unwrap_or((80, 24));
    let (mut reader, mut writer) = conn.into_parts();
    let id = "attach";
    let req = Request::Attach(AttachPane {
        pane: pane.to_string(),
        mode,
        rows,
        cols,
    });
    writer.send_body(id, serde_json::to_value(&req)?).await?;
    let first = reader
        .next()
        .await?
        .ok_or_else(|| anyhow::anyhow!("orchd closed the connection"))?;
    let result = reply_result(first.body)?;
    let pane_id = result["pane"]["id"].as_str().unwrap_or(pane).to_string();
    let mut role: Role = serde_json::from_value(result["role"].clone()).unwrap_or(Role::Observer);

    let guard = RawGuard::enter()?;
    let (key_tx, mut key_rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
    std::thread::spawn(move || {
        let mut stdin = std::io::stdin();
        let mut buf = [0u8; 4096];
        while let Ok(n) = stdin.read(&mut buf) {
            if n == 0 || key_tx.send(buf[..n].to_vec()).is_err() {
                break;
            }
        }
    });
    let mut winch = signal(SignalKind::window_change())?;
    let prefix = prefix_byte();
    let mut pending_prefix = false;
    let mut rev: u64 = 0;
    let mut stdout = std::io::stdout();

    let outcome = loop {
        tokio::select! {
            frame = reader.next() => {
                let Some(frame) = frame? else { break Outcome::Closed };
                let Ok(s) = serde_json::from_value::<Stream>(frame.body) else { continue };
                match s {
                    Stream::Screen { base, rev: r, full, data, .. } => {
                        if !full && base != rev {
                            writer.send_body(id, serde_json::to_value(Request::Resnapshot {})?).await?;
                            continue;
                        }
                        stdout.write_all(data.as_bytes())?;
                        stdout.flush()?;
                        rev = r;
                    }
                    Stream::Role { role: r } => role = r,
                    Stream::Exit { code } => break Outcome::Exited(code),
                }
            }
            keys = key_rx.recv() => {
                let Some(keys) = keys else { break Outcome::Closed };
                let mut out = Vec::with_capacity(keys.len());
                let mut detach = false;
                for b in keys {
                    if pending_prefix {
                        pending_prefix = false;
                        match b {
                            b'd' => { detach = true; break; }
                            b't' => {
                                writer.send_body(id, serde_json::to_value(Request::Takeover {})?).await?;
                            }
                            b if b == prefix => out.push(prefix),
                            other => { out.push(prefix); out.push(other); }
                        }
                    } else if b == prefix {
                        pending_prefix = true;
                    } else {
                        out.push(b);
                    }
                }
                if !out.is_empty() && role == Role::Controller {
                    writer.send_body(id, serde_json::to_value(Request::Input { data_b64: b64(&out) })?).await?;
                }
                if detach { break Outcome::Detached }
            }
            _ = winch.recv() => {
                if let Ok((cols, rows)) = crossterm::terminal::size() {
                    writer.send_body(id, serde_json::to_value(Request::Resize { rows, cols })?).await?;
                }
            }
        }
    };
    drop(guard);
    match outcome {
        Outcome::Detached => eprintln!("[detached from {pane_id}]"),
        Outcome::Exited(code) => eprintln!(
            "[{pane_id} exited{}]",
            code.map(|c| format!(" with code {c}")).unwrap_or_default()
        ),
        Outcome::Closed => eprintln!("[connection to orchd closed]"),
    }
    // The stdin thread is still blocked in read(); leave without waiting for it.
    std::process::exit(0);
}

/// Attach as controller to an echoing pane, then time keystroke → frame round trips.
pub async fn latency(conn: Conn, pane: &str, count: u32) -> Result<()> {
    let (mut reader, mut writer) = conn.into_parts();
    let id = "latency";
    let t0 = Instant::now();
    let req = Request::Attach(AttachPane {
        pane: pane.to_string(),
        mode: AttachMode::Control,
        rows: 24,
        cols: 80,
    });
    writer.send_body(id, serde_json::to_value(&req)?).await?;
    reply_result(
        reader
            .next()
            .await?
            .ok_or_else(|| anyhow::anyhow!("closed"))?
            .body,
    )?;
    // First snapshot.
    loop {
        let f = reader
            .next()
            .await?
            .ok_or_else(|| anyhow::anyhow!("closed"))?;
        if f.body["stream"] == "screen" {
            break;
        }
    }
    let attach_ms = t0.elapsed().as_secs_f64() * 1000.0;
    let mut samples = Vec::new();
    for i in 0..count {
        // Let the pacing window pass so we measure latency, not the frame limiter.
        tokio::time::sleep(Duration::from_millis(60)).await;
        let ch = [b'a' + (i % 26) as u8];
        let t = Instant::now();
        writer
            .send_body(
                id,
                serde_json::to_value(Request::Input { data_b64: b64(&ch) })?,
            )
            .await?;
        loop {
            let f = tokio::time::timeout(Duration::from_secs(3), reader.next()).await;
            let Ok(f) = f else {
                bail!("no echo within 3 s — does the pane echo input?")
            };
            let f = f?.ok_or_else(|| anyhow::anyhow!("closed"))?;
            if f.body["stream"] == "screen" {
                break;
            }
        }
        samples.push(t.elapsed().as_secs_f64() * 1000.0);
    }
    samples.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let med = samples.get(samples.len() / 2).copied().unwrap_or(0.0);
    let max = samples.last().copied().unwrap_or(0.0);
    println!("attach→first frame {attach_ms:.1} ms; keystroke→echo frame median {med:.1} ms, max {max:.1} ms ({count} samples)");
    Ok(())
}
