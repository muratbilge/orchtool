//! Socket server: one task per connection; request/reply, or an attach stream.

use crate::bus::{Bus, BusEvent};
use crate::pane::Pane;
use crate::render::Renderer;
use base64::Engine as _;
use orch_proto::api::{AttachMode, AttachPane, NewPane, ReadSource, Request, Role, Stream};
use orch_proto::client::{FrameReader, FrameWriter};
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::Notify;
use tokio::time::Instant;

/// Minimum time between two frames to one client (~25 fps).
pub const FRAME_INTERVAL: Duration = Duration::from_millis(40);
/// Pause between typed text and the Enter key in `pane.send` (and nudges).
pub const ENTER_DELAY_MS: u64 = 250;

pub struct Daemon {
    pub bus: Arc<Bus>,
    panes: Mutex<BTreeMap<u64, Arc<Pane>>>,
    next_pane: AtomicU64,
    pub scrollback: usize,
    pub shutdown: Notify,
}

#[derive(Debug)]
struct ApiError {
    code: &'static str,
    message: String,
}

fn bus_err(e: anyhow::Error) -> ApiError {
    err("bus", format!("{e:#}"))
}

fn err(code: &'static str, message: impl Into<String>) -> ApiError {
    ApiError {
        code,
        message: message.into(),
    }
}

impl Daemon {
    pub fn new(scrollback: usize, bus: Arc<Bus>) -> Arc<Self> {
        Arc::new(Self {
            bus,
            panes: Mutex::new(BTreeMap::new()),
            next_pane: AtomicU64::new(1),
            scrollback,
            shutdown: Notify::new(),
        })
    }

    fn find(&self, key: &str) -> Result<Arc<Pane>, ApiError> {
        let panes = self.panes.lock().unwrap();
        panes
            .values()
            .find(|p| p.id == key)
            .or_else(|| panes.values().find(|p| p.name.as_deref() == Some(key)))
            .cloned()
            .ok_or_else(|| err("no_pane", format!("no pane {key:?}")))
    }

    fn new_pane(&self, req: NewPane) -> Result<Arc<Pane>, ApiError> {
        if let Some(name) = &req.name {
            if name.is_empty() || name.starts_with('p') && name[1..].parse::<u64>().is_ok() {
                return Err(err(
                    "bad_name",
                    "names must not look like pane ids (p<number>)",
                ));
            }
            let taken = self
                .panes
                .lock()
                .unwrap()
                .values()
                .any(|p| p.name.as_deref() == Some(name.as_str()) && p.exit_code().is_none());
            if taken {
                return Err(err(
                    "name_taken",
                    format!("a running pane is named {name:?}"),
                ));
            }
        }
        let n = self.next_pane.fetch_add(1, Ordering::SeqCst);
        let pane = Pane::spawn(format!("p{n}"), req, self.scrollback)
            .map_err(|e| err("spawn_failed", format!("{e:#}")))?;
        self.panes.lock().unwrap().insert(n, Arc::clone(&pane));
        Ok(pane)
    }

    /// SIGHUP (like closing a terminal), then SIGTERM, then SIGKILL to the process group.
    /// The pane is only forgotten once its program is really gone.
    async fn kill(&self, key: &str) -> Result<(), ApiError> {
        let pane = self.find(key)?;
        let mut exit = pane.subscribe_exit();
        for step in 0..3 {
            if pane.exit_code().is_some() {
                break;
            }
            match step {
                0 => pane.kill(),
                1 => pane.signal_group(libc::SIGTERM),
                _ => pane.signal_group(libc::SIGKILL),
            }
            let wait = exit.wait_for(|e| e.is_some());
            let _ = tokio::time::timeout(Duration::from_millis(1500), wait).await;
        }
        if pane.exit_code().is_none() {
            return Err(err(
                "kill_failed",
                format!("{} is still running after SIGKILL", pane.id),
            ));
        }
        self.panes
            .lock()
            .unwrap()
            .retain(|_, p| !Arc::ptr_eq(p, &pane));
        Ok(())
    }

    pub fn kill_all(&self) {
        for pane in self.panes.lock().unwrap().values() {
            pane.kill();
            pane.signal_group(libc::SIGTERM);
        }
    }

    async fn handle(&self, req: Request) -> Result<Value, ApiError> {
        match req {
            Request::Ping {} => Ok(json!({"pong": true, "version": orch_proto::VERSION,
                "pid": std::process::id()})),
            Request::NewPane(req) => Ok(serde_json::to_value(self.new_pane(req)?.info()).unwrap()),
            Request::ListPanes {} => {
                let panes: Vec<_> = self
                    .panes
                    .lock()
                    .unwrap()
                    .values()
                    .map(|p| p.info())
                    .collect();
                Ok(json!({ "panes": panes }))
            }
            Request::Read(r) => {
                let pane = self.find(&r.pane)?;
                let mut term = pane.term.lock().unwrap();
                let text = match r.source {
                    ReadSource::Visible => term.visible_text(),
                    ReadSource::Recent => term.recent_text(r.lines.unwrap_or(200)),
                };
                Ok(json!({ "pane": pane.id, "text": text }))
            }
            Request::Send(s) => {
                let pane = self.find(&s.pane)?;
                if pane.exit_code().is_some() {
                    return Err(err("exited", "pane has exited"));
                }
                let full = || err("input_full", "the program is not reading its input");
                if !s.text.is_empty() && !pane.write(s.text.into_bytes()) {
                    return Err(full());
                }
                if s.enter {
                    // A separate, later CR is an Enter key press, not part of a paste. Codex's
                    // paste-burst detection swallowed a CR sent 60 ms after the text.
                    tokio::time::sleep(Duration::from_millis(ENTER_DELAY_MS)).await;
                    if !pane.write(b"\r".to_vec()) {
                        return Err(full());
                    }
                }
                Ok(json!({ "pane": pane.id }))
            }
            Request::Kill { pane } => {
                self.kill(&pane).await?;
                Ok(json!({ "pane": pane }))
            }
            Request::StopDaemon {} => {
                self.shutdown.notify_one();
                Ok(json!({ "stopping": true }))
            }
            Request::RegisterAgent(r) => {
                let row = self.bus.register(&r).map_err(bus_err)?;
                Ok(serde_json::to_value(row).unwrap())
            }
            Request::SetStatus { name, status, text } => {
                let row = self
                    .bus
                    .set_status(&name, &status, text.as_deref())
                    .map_err(bus_err)?;
                Ok(serde_json::to_value(row).unwrap())
            }
            Request::ListAgents {} => {
                let rows = self.bus.agents().map_err(bus_err)?;
                Ok(json!({ "agents": rows }))
            }
            Request::SendMessage(m) => {
                let row = self
                    .bus
                    .send(&m.from, &m.to, "msg", &m.text, m.thread.as_deref(), None)
                    .map_err(bus_err)?;
                self.bus.touch(&row.from);
                Ok(serde_json::to_value(row).unwrap())
            }
            Request::Inbox {
                name,
                wait_ms,
                peek,
            } => {
                let wait = Duration::from_millis(wait_ms.unwrap_or(0).min(3_600_000));
                let rows = self
                    .bus
                    .inbox_wait(&name, peek, wait)
                    .await
                    .map_err(bus_err)?;
                Ok(json!({ "messages": rows }))
            }
            Request::Log { after, limit, .. } => {
                let rows = self
                    .bus
                    .log(after, limit.unwrap_or(50).min(1000))
                    .map_err(bus_err)?;
                Ok(json!({ "messages": rows }))
            }
            Request::Ask {
                from,
                text,
                options,
            } => {
                let q = self.bus.ask(&from, &text, &options).map_err(bus_err)?;
                let _ = self
                    .bus
                    .set_status(&q.from, "blocked", Some("waiting for Murat"));
                Ok(serde_json::to_value(q).unwrap())
            }
            Request::AskWait { id, wait_ms } => {
                let wait = Duration::from_millis(wait_ms.unwrap_or(0).min(3_600_000));
                let q = self.bus.ask_wait(id, wait).await.map_err(bus_err)?;
                Ok(serde_json::to_value(q).unwrap())
            }
            Request::Answer { id, answer, by } => {
                let (won, q) = self.bus.answer(id, &answer, &by).map_err(bus_err)?;
                if won {
                    let _ = self.bus.set_status(&q.from, "working", Some("answered"));
                }
                Ok(json!({ "won": won, "question": q }))
            }
            Request::ListQuestions { all } => {
                let rows = self.bus.questions(all).map_err(bus_err)?;
                Ok(json!({ "questions": rows }))
            }
            Request::TelegramPair {} => {
                let code = crate::telegram::new_pair_code(&self.bus).map_err(bus_err)?;
                Ok(json!({ "code": code }))
            }
            Request::Attach(_)
            | Request::Input { .. }
            | Request::Resize { .. }
            | Request::Takeover {}
            | Request::Resnapshot {} => Err(err("bad_request", "only valid on an attach stream")),
        }
    }
}

pub async fn serve(listener: UnixListener, daemon: Arc<Daemon>) {
    loop {
        match listener.accept().await {
            Ok((stream, _)) => {
                let d = Arc::clone(&daemon);
                tokio::spawn(async move {
                    if let Err(e) = connection(stream, d).await {
                        if e.kind() != std::io::ErrorKind::BrokenPipe {
                            eprintln!("orchd: connection: {e}");
                        }
                    }
                });
            }
            Err(e) => {
                eprintln!("orchd: accept: {e}");
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        }
    }
}

async fn connection(stream: UnixStream, daemon: Arc<Daemon>) -> std::io::Result<()> {
    // Same user only (the socket is 0600 too; this also covers a widened directory).
    let uid = unsafe { libc::getuid() };
    if stream.peer_cred().map(|c| c.uid()).ok() != Some(uid) {
        return Ok(());
    }
    let (r, w) = stream.into_split();
    let mut reader = FrameReader::new(Box::new(r));
    let mut writer = FrameWriter::new(Box::new(w));
    while let Some(frame) = reader.next().await? {
        let req: Request = match serde_json::from_value(frame.body) {
            Ok(r) => r,
            Err(e) => {
                reply(
                    &mut writer,
                    &frame.id,
                    Err(err("bad_request", e.to_string())),
                )
                .await?;
                continue;
            }
        };
        if let Request::Attach(a) = req {
            let pane = match daemon.find(&a.pane) {
                Ok(p) => p,
                Err(e) => {
                    reply(&mut writer, &frame.id, Err(e)).await?;
                    continue;
                }
            };
            return attach(pane, a, frame.id, reader, writer).await;
        }
        if let Request::Log {
            after,
            limit,
            follow: true,
        } = req
        {
            return follow_log(&daemon, after, limit, frame.id, reader, writer).await;
        }
        let result = daemon.handle(req).await;
        reply(&mut writer, &frame.id, result).await?;
    }
    Ok(())
}

async fn reply(
    writer: &mut FrameWriter,
    id: &str,
    result: Result<Value, ApiError>,
) -> std::io::Result<()> {
    let body = match result {
        Ok(v) => json!({ "result": v }),
        Err(e) => json!({ "error": { "code": e.code, "message": e.message } }),
    };
    writer.send_body(id, body).await
}

/// `msg.log` with follow: backlog, then every new timeline row until the client hangs up.
async fn follow_log(
    daemon: &Daemon,
    after: Option<i64>,
    limit: Option<usize>,
    id: String,
    mut reader: FrameReader,
    mut writer: FrameWriter,
) -> std::io::Result<()> {
    let mut rx = daemon.bus.subscribe(); // before the backlog query: nothing slips between
    let backlog = daemon
        .bus
        .log(after, limit.unwrap_or(30).min(1000))
        .unwrap_or_default();
    let mut last = after.unwrap_or(0);
    writer
        .send_body(&id, json!({ "result": { "following": true } }))
        .await?;
    for row in backlog {
        last = last.max(row.id);
        writer
            .send_body(&id, stream_body(&Stream::Timeline { row }))
            .await?;
    }
    loop {
        tokio::select! {
            f = reader.next() => if f?.is_none() { return Ok(()) },
            ev = rx.recv() => match ev {
                Ok(BusEvent::Message(row)) if row.id > last => {
                    last = row.id;
                    writer.send_body(&id, stream_body(&Stream::Timeline { row })).await?;
                }
                Ok(_) => {}
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                    // Catch up from the store instead of losing rows.
                    for row in daemon.bus.log(Some(last), 1000).unwrap_or_default() {
                        last = last.max(row.id);
                        writer.send_body(&id, stream_body(&Stream::Timeline { row })).await?;
                    }
                }
                Err(_) => return Ok(()),
            },
        }
    }
}

/// Wake idle agents: when a message or answer arrives for an agent whose status is `idle`
/// (reported by its hooks / post_status) and whose pane is known, type a one-line notice into
/// that pane. Never when the status is unknown, working or blocked — a notice typed into a
/// permission prompt could choose an option.
pub fn start_nudger(daemon: Arc<Daemon>) {
    let mut rx = daemon.bus.subscribe();
    tokio::spawn(async move {
        loop {
            let m = match rx.recv().await {
                Ok(BusEvent::Message(m)) => m,
                Ok(_) => continue,
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                Err(_) => return,
            };
            if !matches!(m.kind.as_str(), "msg" | "answer") || m.to == orch_proto::api::HUMAN {
                continue;
            }
            let Ok(Some(agent)) = daemon.bus.agent(&m.to) else {
                continue;
            };
            let (true, Some(pane)) = (agent.status == "idle", agent.pane.clone()) else {
                continue;
            };
            let what = if m.kind == "answer" {
                "answer"
            } else {
                "message"
            };
            let notice = format!(
                "[orch] new {what} from {} - call the orch read_inbox tool",
                m.from
            );
            // Mark first, so a burst of messages produces one notice.
            let _ = daemon
                .bus
                .set_status(&agent.name, "working", Some("nudged by orch"));
            nudge(&daemon, &pane, &notice).await;
        }
    });
}

async fn nudge(daemon: &Daemon, pane: &str, text: &str) {
    if let Some(id) = pane.strip_prefix("orch:") {
        if let Ok(p) = daemon.find(id) {
            p.write(text.as_bytes().to_vec());
            tokio::time::sleep(Duration::from_millis(ENTER_DELAY_MS)).await;
            p.write(b"\r".to_vec());
        }
    } else if let Some(id) = pane.strip_prefix("herdr:") {
        let herdr = std::env::var("ORCH_HERDR").unwrap_or_else(|_| "herdr".into());
        let _ = tokio::process::Command::new(&herdr)
            .args(["pane", "send-text", id, text])
            .output()
            .await;
        tokio::time::sleep(Duration::from_millis(ENTER_DELAY_MS)).await;
        let _ = tokio::process::Command::new(&herdr)
            .args(["pane", "send-keys", id, "Enter"])
            .output()
            .await;
    }
}

fn stream_body(s: &Stream) -> Value {
    serde_json::to_value(s).unwrap()
}

/// Attach stream: snapshot, then diffs at most every FRAME_INTERVAL, latest state wins.
async fn attach(
    pane: Arc<Pane>,
    req: AttachPane,
    id: String,
    mut reader: FrameReader,
    mut writer: FrameWriter,
) -> std::io::Result<()> {
    let take = match req.mode {
        AttachMode::Auto => None,
        AttachMode::Control => Some(true),
        AttachMode::Observe => Some(false),
    };
    // Subscribe before registering, so no controller change can slip in unnoticed.
    let mut ctrl_rx = pane.subscribe_controller();
    let (client, mut role) = pane.attach(take);
    let result = attach_loop(
        &pane,
        &req,
        &id,
        client,
        &mut role,
        &mut ctrl_rx,
        &mut reader,
        &mut writer,
    )
    .await;
    pane.detach(client);
    result
}

#[allow(clippy::too_many_arguments)]
async fn attach_loop(
    pane: &Arc<Pane>,
    req: &AttachPane,
    id: &str,
    client: u64,
    role: &mut Role,
    ctrl_rx: &mut tokio::sync::watch::Receiver<Option<u64>>,
    reader: &mut FrameReader,
    writer: &mut FrameWriter,
) -> std::io::Result<()> {
    if *role == Role::Controller {
        pane.resize_for(client, req.rows, req.cols);
    }
    writer
        .send_body(
            id,
            json!({ "result": { "pane": pane.info(), "role": role } }),
        )
        .await?;
    let mut renderer = Renderer::new(req.rows, req.cols);
    let mut rev_rx = pane.subscribe_rev();
    let mut exit_rx = pane.subscribe_exit();
    // Reconcile: another client may have taken over while we were replying.
    let current = if *ctrl_rx.borrow_and_update() == Some(client) {
        Role::Controller
    } else {
        Role::Observer
    };
    if current != *role {
        *role = current;
        writer
            .send_body(id, stream_body(&Stream::Role { role: current }))
            .await?;
    }
    let mut seq: u64 = 0;
    let mut dirty = true;
    let mut last_sent = Instant::now() - FRAME_INTERVAL;
    loop {
        if exit_rx.borrow().is_some() {
            send_frame(pane, &mut renderer, &mut seq, id, writer).await?;
            let code = exit_rx.borrow().flatten();
            writer
                .send_body(id, stream_body(&Stream::Exit { code }))
                .await?;
            return Ok(());
        }
        let next_frame = last_sent + FRAME_INTERVAL;
        tokio::select! {
            frame = reader.next() => {
                let Some(frame) = frame? else { return Ok(()) };
                match serde_json::from_value::<Request>(frame.body) {
                    Ok(Request::Input { data_b64 }) => {
                        if pane.is_controller(client) {
                            if let Ok(bytes) = base64::engine::general_purpose::STANDARD.decode(data_b64) {
                                // Over budget = the program is not reading; drop the keys.
                                let _ = pane.write(bytes);
                            }
                        }
                    }
                    Ok(Request::Resize { rows, cols }) => {
                        renderer.set_view(rows, cols);
                        pane.resize_for(client, rows, cols);
                        dirty = true;
                    }
                    Ok(Request::Takeover {}) => pane.take_control(client),
                    Ok(Request::Resnapshot {}) => {
                        renderer.force_full();
                        dirty = true;
                    }
                    _ => {}
                }
            }
            r = rev_rx.changed() => {
                if r.is_err() { return Ok(()) }
                dirty = true;
            }
            _ = exit_rx.changed() => {}
            c = ctrl_rx.changed() => {
                if c.is_err() { return Ok(()) }
                let now = if *ctrl_rx.borrow() == Some(client) { Role::Controller } else { Role::Observer };
                if now != *role {
                    *role = now;
                    writer.send_body(id, stream_body(&Stream::Role { role: now })).await?;
                    if now == Role::Controller {
                        // The new controller's terminal decides the size.
                        let (rows, cols) = renderer.view();
                        pane.resize_for(client, rows, cols);
                    }
                }
            }
            _ = tokio::time::sleep_until(next_frame), if dirty => {
                send_frame(pane, &mut renderer, &mut seq, id, writer).await?;
                last_sent = Instant::now();
                dirty = false;
            }
        }
    }
}

async fn send_frame(
    pane: &Arc<Pane>,
    renderer: &mut Renderer,
    seq: &mut u64,
    id: &str,
    writer: &mut FrameWriter,
) -> std::io::Result<()> {
    let (frame, rows, cols, gen) = {
        let term = pane.term.lock().unwrap();
        let gen = pane.gen();
        let (rows, cols) = term.size();
        (renderer.render(term.screen(), gen), rows, cols, gen)
    };
    let Some(frame) = frame else { return Ok(()) };
    let base = if frame.full { 0 } else { *seq };
    *seq += 1;
    let s = Stream::Screen {
        gen,
        rows,
        cols,
        base,
        rev: *seq,
        full: frame.full,
        data: frame.data,
    };
    writer.send_body(id, stream_body(&s)).await
}
