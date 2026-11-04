//! Socket server: one task per connection; request/reply, or an attach stream.

use crate::bus::{Bus, BusEvent};
use crate::pane::Pane;
use crate::render::Renderer;
use base64::Engine as _;
use orch_proto::api::{AttachMode, AttachPane, NewPane, ReadSource, Request, Role, Stream};
use orch_proto::client::{FrameReader, FrameWriter};
use serde_json::{json, Value};
use std::collections::{BTreeMap, HashMap};
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
/// Codex paste detection and multi-line pastes need the longer gap. Other single-line
/// input only needs Enter to arrive in a separate burst. Nudges retain their safety delay.
fn send_enter_delay(kind: Option<&str>, text: &str) -> Duration {
    Duration::from_millis(if kind == Some("codex") || text.contains(['\n', '\r']) {
        ENTER_DELAY_MS
    } else {
        20
    })
}
/// How long a conditional send's screen state counts as answered.
const ANSWERED_FOR: Duration = Duration::from_secs(30);

pub struct Daemon {
    pub bus: Arc<Bus>,
    pub(crate) session: Mutex<orch_proto::layout::Session>,
    pub(crate) ui_events: tokio::sync::broadcast::Sender<crate::ui::UiEvent>,
    panes: Mutex<BTreeMap<u64, Arc<Pane>>>,
    next_pane: AtomicU64,
    /// Conditional sends that went through: pane → (screen hash, when). First answer wins.
    answered: Mutex<HashMap<String, (String, Instant)>>,
    pub scrollback: usize,
    pub shutdown: Notify,
    /// Where `daemon.stop` with `restart` saves the layout for the next orchd (set by main).
    pub restore_path: std::sync::OnceLock<std::path::PathBuf>,
}

#[derive(Debug)]
pub struct ApiError {
    pub code: &'static str,
    pub message: String,
}

fn bus_err(e: anyhow::Error) -> ApiError {
    err("bus", format!("{e:#}"))
}

pub(crate) fn err(code: &'static str, message: impl Into<String>) -> ApiError {
    ApiError {
        code,
        message: message.into(),
    }
}

impl Daemon {
    pub fn new(scrollback: usize, bus: Arc<Bus>) -> Arc<Self> {
        Arc::new(Self {
            bus,
            session: Mutex::new(orch_proto::layout::Session::default()),
            ui_events: tokio::sync::broadcast::channel(256).0,
            panes: Mutex::new(BTreeMap::new()),
            next_pane: AtomicU64::new(1),
            answered: Mutex::new(HashMap::new()),
            scrollback,
            shutdown: Notify::new(),
            restore_path: Default::default(),
        })
    }

    pub(crate) fn all_panes(&self) -> Vec<Arc<Pane>> {
        self.panes.lock().unwrap().values().cloned().collect()
    }

    pub(crate) fn find(&self, key: &str) -> Result<Arc<Pane>, ApiError> {
        let panes = self.panes.lock().unwrap();
        panes
            .values()
            .find(|p| p.id == key)
            .or_else(|| panes.values().find(|p| p.name.as_deref() == Some(key)))
            .cloned()
            .ok_or_else(|| err("no_pane", format!("no pane {key:?}")))
    }

    pub(crate) fn new_pane(&self, req: NewPane) -> Result<Arc<Pane>, ApiError> {
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
        // Pane ids never repeat, not even across orchd restarts: agent identities and
        // nudges are keyed by pane, so a reused id would hand an old identity to a new program.
        let n = self
            .bus
            .next_counter("next_pane")
            .unwrap_or_else(|_| self.next_pane.fetch_add(1, Ordering::SeqCst) + 1_000_000);
        let pane = Pane::spawn(format!("p{n}"), req, self.scrollback)
            .map_err(|e| err("spawn_failed", format!("{e:#}")))?;
        self.panes.lock().unwrap().insert(n, Arc::clone(&pane));
        Ok(pane)
    }

    /// SIGHUP (like closing a terminal), then SIGTERM, then SIGKILL to the process group.
    /// The pane is only forgotten once its program is really gone.
    pub(crate) async fn kill(&self, key: &str) -> Result<(), ApiError> {
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
        self.expire_questions().map_err(bus_err)?;
        Ok(())
    }

    pub fn kill_all(&self) {
        for pane in self.panes.lock().unwrap().values() {
            pane.kill();
            pane.signal_group(libc::SIGTERM);
        }
    }

    pub fn expire_questions(&self) -> anyhow::Result<usize> {
        let live = self
            .all_panes()
            .iter()
            .filter(|p| p.exit_code().is_none())
            .map(|p| format!("orch:{}", p.id))
            .collect();
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as i64;
        self.bus.expire_questions(&live, now)
    }

    async fn handle(&self, req: Request) -> Result<Value, ApiError> {
        match req {
            Request::Ping {} => Ok(json!({"pong": true, "version": orch_proto::VERSION,
                "pid": std::process::id()})),
            Request::NewPane(req) => {
                let info = self.new_pane(req)?.info();
                self.place_new_pane(&info)?;
                let _ = self.ui_events.send(crate::ui::UiEvent::Panes);
                Ok(serde_json::to_value(info).unwrap())
            }
            Request::UiGet {} => Ok(serde_json::to_value(self.session()).unwrap()),
            Request::Ui(op) => self.ui_op(op).await,
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
                    ReadSource::RecentUnwrapped => {
                        term.recent_text_unwrapped(r.lines.unwrap_or(200))
                    }
                };
                let hash = orch_proto::api::text_hash(&text);
                Ok(json!({ "pane": pane.id, "text": text, "hash": hash }))
            }
            Request::Send(s) => {
                let pane = self.find(&s.pane)?;
                let enter_delay = send_enter_delay(pane.kind().as_deref(), &s.text);
                if pane.exit_code().is_some() {
                    return Err(err("exited", "pane has exited"));
                }
                let full = || err("input_full", "the program is not reading its input");
                if let Some(expect) = &s.expect {
                    // Check and send under the terminal lock: the screen can't change in between.
                    let mut term = pane.term.lock().unwrap();
                    let now =
                        orch_proto::api::text_hash(&term.recent_text(s.expect_lines.unwrap_or(60)));
                    if &now != expect {
                        return Err(err("changed", "the pane's screen changed: not sent"));
                    }
                    let mut answered = self.answered.lock().unwrap();
                    answered.retain(|_, (_, t)| t.elapsed() < ANSWERED_FOR);
                    if answered.get(&pane.id).is_some_and(|(h, _)| h == expect) {
                        return Err(err("answered", "this screen was already answered"));
                    }
                    if !s.text.is_empty() && !pane.write(s.text.clone().into_bytes()) {
                        return Err(full());
                    }
                    answered.insert(pane.id.clone(), (expect.clone(), Instant::now()));
                } else if !s.text.is_empty() && !pane.write(s.text.into_bytes()) {
                    return Err(full());
                }
                if s.enter {
                    // A separate, later CR is an Enter key press, not part of a paste. Codex's
                    // paste-burst detection swallowed a CR sent 60 ms after the text.
                    tokio::time::sleep(enter_delay).await;
                    if !pane.write(b"\r".to_vec()) {
                        return Err(full());
                    }
                }
                Ok(json!({ "pane": pane.id }))
            }
            Request::AutoContinue { pane, on } => {
                let id = self.find(&pane)?.id.clone();
                let on = crate::limits::auto_continue(&id, on);
                let _ = self.ui_events.send(crate::ui::UiEvent::Panes);
                Ok(json!({ "pane": id, "auto_continue": on }))
            }
            Request::Kill { pane } => {
                let id = self.find(&pane)?.id.clone();
                self.kill(&id).await?;
                self.unplace(&id);
                Ok(json!({ "pane": id }))
            }
            Request::StopDaemon { restart } => {
                // Saved while every pane still runs, before anything stops: if saving fails,
                // nothing is killed and the caller hears why.
                if restart {
                    let path = self
                        .restore_path
                        .get()
                        .ok_or_else(|| err("save_failed", "no place to save the layout"))?;
                    self.save_layout(path).map_err(|e| {
                        err("save_failed", format!("saving the layout failed: {e}"))
                    })?;
                }
                self.shutdown.notify_one();
                Ok(json!({ "stopping": true }))
            }
            Request::RegisterAgent(r) => {
                if let Some(id) = r.pane.as_deref().and_then(|p| p.strip_prefix("orch:")) {
                    if let Ok(p) = self.find(id) {
                        p.set_kind(&r.kind);
                    }
                }
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
            Request::Inbox { name, wait_ms } => {
                let wait = Duration::from_millis(wait_ms.unwrap_or(0).min(3_600_000));
                let rows = self.bus.inbox_wait(&name, wait).await.map_err(bus_err)?;
                Ok(json!({ "messages": rows }))
            }
            Request::Ack { name, ids } => {
                let n = self.bus.ack(&name, &ids).map_err(bus_err)?;
                Ok(json!({ "acked": n }))
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
                // Lets the asker mark the answer received once it really got it.
                let answer_message = self.bus.answer_message(id).map_err(bus_err)?;
                let mut v = serde_json::to_value(q).unwrap();
                v["answer_message"] = json!(answer_message);
                Ok(v)
            }
            Request::Answer { id, answer, by } => {
                if self.bus.question(id).map_err(bus_err)?.is_none() {
                    return Err(err("no_question", format!("no question #{id}")));
                }
                let (won, q) = self.bus.answer(id, &answer, &by).map_err(bus_err)?;
                if q.state == "expired" {
                    return Err(err("no_question", format!("question #{id} expired")));
                }

                if won {
                    let _ = self.bus.set_status(&q.from, "working", Some("answered"));
                }
                Ok(json!({ "won": won, "question": q }))
            }
            Request::ListQuestions { all } => {
                self.expire_questions().map_err(bus_err)?;
                let rows = self.bus.questions(all).map_err(bus_err)?;
                Ok(json!({ "questions": rows }))
            }
            Request::CloseQuestion { id } => match self.bus.close_question(id).map_err(bus_err)? {
                Some((true, q)) => Ok(json!({ "question": q })),
                Some((false, q)) if q.state == "answered" => {
                    Err(err("answered", format!("question #{id} already answered")))
                }
                _ => Err(err("no_question", format!("question #{id} is gone"))),
            },
            Request::Subscribe {}
            | Request::Attach(_)
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
        if let Request::Subscribe {} = req {
            return crate::ui::subscribe(&daemon, frame.id, reader, writer).await;
        }
        if let Request::Log {
            after,
            limit,
            follow: true,
        } = req
        {
            return follow_log(&daemon, after, limit, frame.id, reader, writer).await;
        }
        // A long request (inbox/ask wait) ends when the client goes away.
        let result = tokio::select! {
            r = daemon.handle(req) => r,
            next = reader.next() => match next {
                Ok(None) | Err(_) => return Ok(()),
                Ok(Some(_)) => Err(err("busy", "one request at a time per connection")),
            },
        };
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
                // Events are only hints: always send the ordered rows after the last one sent,
                // so a commit that is published late is never skipped.
                Ok(BusEvent::Message(_)) | Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                    for row in daemon.bus.log(Some(last), 1000).unwrap_or_default() {
                        last = last.max(row.id);
                        writer.send_body(&id, stream_body(&Stream::Timeline { row })).await?;
                    }
                }
                Ok(_) => {}
                Err(_) => return Ok(()),
            },
        }
    }
}

/// Wake idle agents: when a message or answer arrives for an agent, type a one-line notice
/// into its pane — but only when it is provably safe (phase-2 review):
/// 1. its bus status is idle or unknown, and is atomically switched to working (a newer
///    working/blocked status is never overwritten),
/// 2. the pane itself says idle right before typing (orch pane: screen detection; herdr pane:
///    herdr's own agent_status), so nothing is typed into a permission prompt or a shell,
/// 3. Enter is pressed only if the text got in AND the pane still is not blocked.
///
/// Claude agents also get their messages via the Stop hook; Codex has no hooks, so this is
/// its delivery path while idle.
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
            let Some(pane) = agent.pane.clone() else {
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
            let d = Arc::clone(&daemon);
            tokio::spawn(async move { nudge(&d, &agent.name, &pane, &notice).await });
        }
    });
}

/// Pane-side readiness: `Some(true)` idle, `Some(false)` busy/blocked/gone, `None` unknown.
async fn pane_idle(daemon: &Daemon, pane: &str) -> Option<bool> {
    if let Some(id) = pane.strip_prefix("orch:") {
        let p = daemon.find(id).ok()?;
        return Some(p.detect() == crate::detect::Detected::Idle);
    }
    if let Some(id) = pane.strip_prefix("herdr:") {
        let herdr = std::env::var("ORCH_HERDR").unwrap_or_else(|_| "herdr".into());
        let out = tokio::process::Command::new(&herdr)
            .args(["agent", "get", id])
            .output()
            .await
            .ok()?;
        let v: Value = serde_json::from_slice(&out.stdout).ok()?;
        let status = v["result"]["agent"]["agent_status"].as_str()?;
        return Some(status == "idle");
    }
    None
}

async fn nudge(daemon: &Daemon, agent: &str, pane: &str, text: &str) {
    if pane_idle(daemon, pane).await != Some(true) {
        return;
    }
    let claimed = daemon.bus.set_status_if(
        agent,
        Some(&["idle", "unknown"]),
        "working",
        Some("nudged by orch"),
    );
    if !matches!(claimed, Ok(Some(_))) {
        return;
    }
    if let Some(id) = pane.strip_prefix("orch:") {
        let Ok(p) = daemon.find(id) else { return };
        if !p.write(text.as_bytes().to_vec()) {
            return;
        }
        tokio::time::sleep(Duration::from_millis(ENTER_DELAY_MS)).await;
        // Fail closed: Enter only while the agent still shows its idle prompt.
        if p.detect() != crate::detect::Detected::Idle {
            return;
        }
        p.write(b"\r".to_vec());
    } else if let Some(id) = pane.strip_prefix("herdr:") {
        let herdr = std::env::var("ORCH_HERDR").unwrap_or_else(|_| "herdr".into());
        let typed = tokio::process::Command::new(&herdr)
            .args(["pane", "send-text", id, text])
            .output()
            .await
            .is_ok_and(|o| o.status.success());
        if !typed {
            return;
        }
        tokio::time::sleep(Duration::from_millis(ENTER_DELAY_MS)).await;
        // Fail closed: Enter only if herdr still reports the agent idle.
        if pane_idle(daemon, pane).await != Some(true) {
            return;
        }
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
    // (revision, alternate-screen flag last sent)
    let mut seq: (u64, Option<bool>) = (0, None);
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
    seq: &mut (u64, Option<bool>),
    id: &str,
    writer: &mut FrameWriter,
) -> std::io::Result<()> {
    let (frame, rows, cols, gen, alt) = {
        let term = pane.term.lock().unwrap();
        let gen = pane.gen();
        let (rows, cols) = term.size();
        let alt = term.screen().alternate_screen();
        (renderer.render(term.screen(), gen), rows, cols, gen, alt)
    };
    // A switch to/from the alternate screen with identical content still has to reach the
    // client (it decides what the wheel does): an empty diff carries the new flag.
    let frame = match frame {
        Some(f) => f,
        None if seq.1 != Some(alt) => crate::render::Frame {
            full: false,
            data: String::new(),
        },
        None => return Ok(()),
    };
    seq.1 = Some(alt);
    let base = if frame.full { 0 } else { seq.0 };
    seq.0 += 1;
    let s = Stream::Screen {
        gen,
        rows,
        cols,
        base,
        rev: seq.0,
        full: frame.full,
        data: frame.data,
        alt,
    };
    writer.send_body(id, stream_body(&s)).await
}

#[cfg(test)]
mod enter_delay_tests {
    use super::*;

    #[test]
    fn long_gap_is_reserved_for_codex_and_multiline_text() {
        for kind in [None, Some("claude"), Some("shell")] {
            assert_eq!(send_enter_delay(kind, "hello"), Duration::from_millis(20));
            assert_eq!(send_enter_delay(kind, ""), Duration::from_millis(20));
            assert_eq!(
                send_enter_delay(kind, "one\ntwo"),
                Duration::from_millis(250)
            );
            assert_eq!(
                send_enter_delay(kind, "one\rtwo"),
                Duration::from_millis(250)
            );
        }
        assert_eq!(
            send_enter_delay(Some("codex"), "hello"),
            Duration::from_millis(250)
        );
    }
}
