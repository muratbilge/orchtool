//! `orch` — the full-screen workspace TUI (the herdr replacement).
//!
//! Layout and agent state live in orchd; this client renders them. Every pane shown here is an
//! attach stream (the same row-diff sync as `orch attach`) fed into a local vt100 screen, so
//! switching tabs is instant (views stay attached) and a slow link never builds a backlog.
//!
//! Keys: prefix (Ctrl-b, or $ORCH_PREFIX) then a command key; `prefix ?` shows them all.

use crate::{client_env, Connector};
use anyhow::{anyhow, Result};
use base64::Engine as _;
use orch_proto::api::{
    AgentRow, AttachMode, AttachPane, Event, MessageRow, PaneInfo, QuestionRow, Request, Role,
    SendMessage, Snapshot, Spawn, Stream, UiOp, HUMAN,
};
use orch_proto::client::reply_result;
use orch_proto::layout::{neighbor, Dir, Rect as LRect, Session, Toward};
use ratatui::backend::CrosstermBackend;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph, Widget, Wrap};
use ratatui::Terminal;
use serde_json::Value;
use std::collections::{HashMap, VecDeque};
use std::io::{Read, Write};
use std::time::{Duration, Instant};
use tokio::signal::unix::{signal, SignalKind};
use tokio::sync::mpsc;

const SIDEBAR_W: u16 = 28;
const MAX_VIEWS: usize = 24;

enum Msg {
    Snapshot(Box<Snapshot>),
    Event(Box<Event>),
    EventsClosed(String),
    Frame {
        pane: String,
        stream: Stream,
    },
    AttachFailed {
        pane: String,
        error: String,
    },
    Keys(Vec<u8>),
    Reply {
        what: String,
        result: Result<Value, String>,
    },
}

struct PaneView {
    parser: vt100::Parser,
    tx: mpsc::UnboundedSender<Request>,
    rev: u64,
    role: Role,
    size: (u16, u16),
    exited: Option<Option<u32>>,
    last_used: Instant,
}

#[derive(Clone)]
enum Prompt {
    NewWorkspace,
    NewAgent,
    RenameTab,
    RenameWorkspace,
    ConfirmClose(String),
    Answer(i64),
}

enum Mode {
    Normal,
    Help,
    Workspaces(usize),
    NeedsYou(usize),
    Comms(String),
    Prompt(Prompt, String),
}

struct App {
    connector: Connector,
    tx: mpsc::UnboundedSender<Msg>,
    session: Session,
    panes: HashMap<String, PaneInfo>,
    agents: Vec<AgentRow>,
    questions: Vec<QuestionRow>,
    messages: VecDeque<MessageRow>,
    ws_id: Option<String>,
    views: HashMap<String, PaneView>,
    mode: Mode,
    prefix_pending: bool,
    sidebar: bool,
    status: Option<(String, Instant)>,
    size: (u16, u16),
    prefix: u8,
    quit: bool,
    started_ws: bool,
    outer_modes: String,
}

fn prefix_byte() -> u8 {
    let p = std::env::var("ORCH_PREFIX").unwrap_or_else(|_| "C-b".into());
    match p.strip_prefix("C-").and_then(|c| c.bytes().next()) {
        Some(c) if c.is_ascii_alphabetic() => c.to_ascii_lowercase() - b'a' + 1,
        _ => 0x02,
    }
}

fn prefix_name(b: u8) -> String {
    format!("Ctrl-{}", (b + b'a' - 1) as char)
}

struct RawGuard;
impl RawGuard {
    fn enter() -> Result<Self> {
        crossterm::terminal::enable_raw_mode()?;
        let g = RawGuard;
        let mut out = std::io::stdout();
        out.write_all(b"\x1b[?1049h\x1b[H\x1b[2J")?;
        out.flush()?;
        Ok(g)
    }
}
impl Drop for RawGuard {
    fn drop(&mut self) {
        let mut out = std::io::stdout();
        let _ = out.write_all(
            b"\x1b[m\x1b[?1l\x1b>\x1b[?2004l\x1b[?9l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006l\x1b[0 q\x1b[?25h\x1b[?1049l",
        );
        let _ = out.flush();
        let _ = crossterm::terminal::disable_raw_mode();
    }
}

pub async fn run(connector: Connector) -> Result<()> {
    let (tx, mut rx) = mpsc::unbounded_channel::<Msg>();
    // Event stream first: if orchd is unreachable we fail before touching the terminal.
    let mut events = connector.connect().await?;
    let id = events.next_id();
    events
        .writer
        .send_body(&id, serde_json::to_value(Request::Subscribe {})?)
        .await?;
    let first = events
        .reader
        .next()
        .await?
        .ok_or_else(|| anyhow!("orchd closed the connection"))?;
    reply_result(first.body)?;
    {
        let tx = tx.clone();
        tokio::spawn(async move {
            let (mut r, _w) = events.into_parts();
            loop {
                match r.next().await {
                    Ok(Some(f)) => match serde_json::from_value::<Stream>(f.body) {
                        Ok(Stream::Snapshot { snapshot }) => {
                            let _ = tx.send(Msg::Snapshot(snapshot));
                        }
                        Ok(Stream::Event { event }) => {
                            let _ = tx.send(Msg::Event(event));
                        }
                        _ => {}
                    },
                    Ok(None) => {
                        let _ = tx.send(Msg::EventsClosed("orchd closed the event stream".into()));
                        return;
                    }
                    Err(e) => {
                        let _ = tx.send(Msg::EventsClosed(e.to_string()));
                        return;
                    }
                }
            }
        });
    }

    let guard = RawGuard::enter()?;
    {
        let tx = tx.clone();
        std::thread::spawn(move || {
            let mut stdin = std::io::stdin();
            let mut buf = [0u8; 4096];
            while let Ok(n) = stdin.read(&mut buf) {
                if n == 0 || tx.send(Msg::Keys(buf[..n].to_vec())).is_err() {
                    break;
                }
            }
        });
    }
    let backend = CrosstermBackend::new(std::io::stdout());
    let mut term = Terminal::new(backend)?;
    let (cols, rows) = crossterm::terminal::size().unwrap_or((120, 40));
    let mut app = App {
        connector,
        tx: tx.clone(),
        session: Session::default(),
        panes: HashMap::new(),
        agents: Vec::new(),
        questions: Vec::new(),
        messages: VecDeque::new(),
        ws_id: None,
        views: HashMap::new(),
        mode: Mode::Normal,
        prefix_pending: false,
        sidebar: cols >= 100,
        status: None,
        size: (cols, rows),
        prefix: prefix_byte(),
        quit: false,
        started_ws: false,
        outer_modes: String::new(),
    };
    let mut winch = signal(SignalKind::window_change())?;
    let mut tick = tokio::time::interval(Duration::from_secs(1));
    let mut dirty = true;
    let mut last_draw = Instant::now() - Duration::from_secs(1);
    let frame = Duration::from_millis(16);
    let result: Result<()> = loop {
        if dirty && last_draw.elapsed() >= frame {
            app.sync_views();
            if let Err(e) = term.draw(|f| app.draw(f)) {
                break Err(e.into());
            }
            app.apply_outer_modes();
            last_draw = Instant::now();
            dirty = false;
        }
        let wait = if dirty {
            frame.saturating_sub(last_draw.elapsed())
        } else {
            Duration::from_secs(3600)
        };
        tokio::select! {
            m = rx.recv() => {
                let Some(m) = m else { break Ok(()) };
                if let Err(e) = app.handle(m) { break Err(e) }
                // Drain whatever else is queued before drawing (one frame per burst).
                while let Ok(m) = rx.try_recv() {
                    if let Err(e) = app.handle(m) { return finish(guard, Err(e)); }
                }
                dirty = true;
            }
            _ = winch.recv() => {
                if let Ok((c, r)) = crossterm::terminal::size() {
                    app.size = (c, r);
                    let _ = term.autoresize();
                }
                dirty = true;
            }
            _ = tick.tick() => {
                if app.status.as_ref().is_some_and(|(_, t)| t.elapsed() > Duration::from_secs(4)) {
                    app.status = None;
                }
                dirty = true;
            }
            _ = tokio::time::sleep(wait) => {}
        }
        if app.quit {
            break Ok(());
        }
    };
    finish(guard, result)
}

fn finish(guard: RawGuard, result: Result<()>) -> Result<()> {
    drop(guard);
    if result.is_ok() {
        eprintln!("[orch detached — panes keep running; `orch` to come back]");
    }
    // The stdin thread is blocked in read(); leave without waiting for it.
    if let Err(e) = &result {
        eprintln!("orch: {e:#}");
        std::process::exit(1);
    }
    std::process::exit(0);
}

fn status_mark(status: Option<&str>) -> (&'static str, Color) {
    match status {
        Some("working") => ("●", Color::Yellow),
        Some("blocked") => ("⚠", Color::Red),
        Some("idle") => ("○", Color::Green),
        Some("done") => ("✓", Color::DarkGray),
        _ => (" ", Color::DarkGray),
    }
}

fn vt_color(c: vt100::Color) -> Color {
    match c {
        vt100::Color::Default => Color::Reset,
        vt100::Color::Idx(i) => Color::Indexed(i),
        vt100::Color::Rgb(r, g, b) => Color::Rgb(r, g, b),
    }
}

fn to_lrect(r: Rect) -> LRect {
    LRect {
        x: r.x,
        y: r.y,
        w: r.width,
        h: r.height,
    }
}

fn to_rect(r: LRect) -> Rect {
    Rect::new(r.x, r.y, r.w, r.h)
}

/// Draws a vt100 screen into a buffer area.
struct ScreenWidget<'a>(&'a vt100::Screen);

impl Widget for ScreenWidget<'_> {
    fn render(self, area: Rect, buf: &mut Buffer) {
        let screen = self.0;
        let (rows, cols) = screen.size();
        for r in 0..area.height.min(rows.get()) {
            for c in 0..area.width.min(cols.get()) {
                let Some(cell) = screen.cell(r, c) else {
                    continue;
                };
                if cell.is_wide_continuation() {
                    continue;
                }
                let mut style = Style::default()
                    .fg(vt_color(cell.fgcolor()))
                    .bg(vt_color(cell.bgcolor()));
                let mut m = Modifier::empty();
                if cell.bold() {
                    m |= Modifier::BOLD;
                }
                if cell.dim() {
                    m |= Modifier::DIM;
                }
                if cell.italic() {
                    m |= Modifier::ITALIC;
                }
                if cell.underline() {
                    m |= Modifier::UNDERLINED;
                }
                if cell.inverse() {
                    m |= Modifier::REVERSED;
                }
                style = style.add_modifier(m);
                let contents = cell.contents();
                let sym = if contents.is_empty() { " " } else { contents };
                // A wide glyph in the last column would spill out of the pane.
                if cell.is_wide() && c + 1 >= area.width {
                    buf[(area.x + c, area.y + r)]
                        .set_symbol(" ")
                        .set_style(style);
                    continue;
                }
                buf[(area.x + c, area.y + r)]
                    .set_symbol(sym)
                    .set_style(style);
            }
        }
    }
}

impl App {
    fn current_ws(&self) -> Option<&orch_proto::layout::Workspace> {
        self.ws_id
            .as_deref()
            .and_then(|id| self.session.workspace(id))
            .or_else(|| self.session.workspaces.first())
    }

    fn current_tab(&self) -> Option<&orch_proto::layout::Tab> {
        let ws = self.current_ws()?;
        ws.tabs
            .get(ws.active_tab.min(ws.tabs.len().saturating_sub(1)))
    }

    fn focused(&self) -> Option<String> {
        self.current_tab().map(|t| t.focus.clone())
    }

    fn flash(&mut self, msg: impl Into<String>) {
        self.status = Some((msg.into(), Instant::now()));
    }

    /// Main area (right of the sidebar, above the status bar).
    fn main_area(&self) -> Rect {
        let (w, h) = self.size;
        let side = if self.sidebar && w >= 60 {
            SIDEBAR_W
        } else {
            0
        };
        Rect::new(side, 0, w.saturating_sub(side), h.saturating_sub(1))
    }

    /// (pane, outer rect incl. title row) for the current tab.
    fn pane_rects(&self) -> Vec<(String, Rect)> {
        let Some(tab) = self.current_tab() else {
            return Vec::new();
        };
        let area = self.main_area();
        if tab.zoomed {
            return vec![(tab.focus.clone(), area)];
        }
        tab.root
            .layout(to_lrect(area))
            .into_iter()
            .map(|(p, r)| (p, to_rect(r)))
            .collect()
    }

    fn inner(r: Rect) -> Rect {
        Rect::new(r.x, r.y + 1, r.width, r.height.saturating_sub(1))
    }

    // ---------- requests ----------

    fn request(&self, what: &str, req: Request) {
        let (connector, tx, what) = (self.connector.clone(), self.tx.clone(), what.to_string());
        tokio::spawn(async move {
            let result = async {
                let mut c = connector.connect().await.map_err(|e| e.to_string())?;
                c.request(&req).await.map_err(|e| e.to_string())
            }
            .await;
            let _ = tx.send(Msg::Reply { what, result });
        });
    }

    fn ui(&self, what: &str, op: UiOp) {
        self.request(what, Request::Ui(op));
    }

    fn spawn_here(&self, argv: Vec<String>) -> Spawn {
        let area = Self::inner(self.main_area());
        Spawn {
            argv,
            cwd: None,
            env: client_env(&[]).ok(),
            rows: Some(area.height.max(2)),
            cols: Some(area.width.max(2)),
            name: None,
        }
    }

    // ---------- attach streams ----------

    /// Attach every visible pane (as controller, sized to its rectangle); keep recently used
    /// views attached so switching back is instant; drop the oldest beyond MAX_VIEWS.
    fn sync_views(&mut self) {
        let rects = self.pane_rects();
        for (pane, rect) in &rects {
            let inner = Self::inner(*rect);
            let size = (inner.height.max(1), inner.width.max(1));
            if !self.panes.contains_key(pane) {
                continue;
            }
            match self.views.get_mut(pane) {
                Some(v) => {
                    v.last_used = Instant::now();
                    if v.size != size && v.exited.is_none() {
                        v.size = size;
                        v.parser.set_size(
                            std::num::NonZeroU16::new(size.0).unwrap(),
                            std::num::NonZeroU16::new(size.1).unwrap(),
                        );
                        if v.role == Role::Observer {
                            let _ = v.tx.send(Request::Takeover {});
                        }
                        let _ = v.tx.send(Request::Resize {
                            rows: size.0,
                            cols: size.1,
                        });
                    }
                }
                None => self.open_view(pane, size),
            }
        }
        if self.views.len() > MAX_VIEWS {
            let visible: Vec<&String> = rects.iter().map(|(p, _)| p).collect();
            let mut old: Vec<(Instant, String)> = self
                .views
                .iter()
                .filter(|(p, _)| !visible.contains(p))
                .map(|(p, v)| (v.last_used, p.clone()))
                .collect();
            old.sort();
            for (_, p) in old.into_iter().take(self.views.len() - MAX_VIEWS) {
                self.views.remove(&p);
            }
        }
        // Views of panes that no longer exist.
        let panes = &self.panes;
        self.views.retain(|p, _| panes.contains_key(p));
    }

    fn open_view(&mut self, pane: &str, size: (u16, u16)) {
        let (req_tx, mut req_rx) = mpsc::unbounded_channel::<Request>();
        self.views.insert(
            pane.to_string(),
            PaneView {
                parser: vt100::Parser::new(
                    std::num::NonZeroU16::new(size.0.max(1)).unwrap(),
                    std::num::NonZeroU16::new(size.1.max(1)).unwrap(),
                    0,
                ),
                tx: req_tx,
                rev: 0,
                role: Role::Controller,
                size,
                exited: None,
                last_used: Instant::now(),
            },
        );
        let (connector, tx, pane) = (self.connector.clone(), self.tx.clone(), pane.to_string());
        tokio::spawn(async move {
            let fail = |e: String| {
                let _ = tx.send(Msg::AttachFailed {
                    pane: pane.clone(),
                    error: e,
                });
            };
            let conn = match connector.connect().await {
                Ok(c) => c,
                Err(e) => return fail(e.to_string()),
            };
            let (mut r, mut w) = conn.into_parts();
            let req = Request::Attach(AttachPane {
                pane: pane.clone(),
                mode: AttachMode::Control,
                rows: size.0,
                cols: size.1,
            });
            if let Err(e) = w.send_body("a", serde_json::to_value(&req).unwrap()).await {
                return fail(e.to_string());
            }
            match r.next().await {
                Ok(Some(f)) => {
                    if let Err(e) = reply_result(f.body) {
                        return fail(e.to_string());
                    }
                }
                Ok(None) => return fail("closed".into()),
                Err(e) => return fail(e.to_string()),
            }
            loop {
                tokio::select! {
                    f = r.next() => match f {
                        Ok(Some(f)) => {
                            if let Ok(stream) = serde_json::from_value::<Stream>(f.body) {
                                let exit = matches!(stream, Stream::Exit { .. });
                                if tx.send(Msg::Frame { pane: pane.clone(), stream }).is_err() || exit {
                                    return;
                                }
                            }
                        }
                        _ => return,
                    },
                    req = req_rx.recv() => match req {
                        Some(req) => {
                            if w.send_body("a", serde_json::to_value(&req).unwrap()).await.is_err() {
                                return;
                            }
                        }
                        None => return, // view dropped: detach
                    },
                }
            }
        });
    }

    fn send_input(&mut self, bytes: Vec<u8>) {
        let Some(pane) = self.focused() else { return };
        if let Some(v) = self.views.get(&pane) {
            if v.role == Role::Observer {
                let _ = v.tx.send(Request::Takeover {});
            }
            let data_b64 = base64::engine::general_purpose::STANDARD.encode(bytes);
            let _ = v.tx.send(Request::Input { data_b64 });
        }
    }

    /// Mirror the focused pane's input modes on the real terminal (arrow keys in vim/less,
    /// bracketed paste). Mouse reporting stays off: the TUI owns the screen coordinates.
    fn apply_outer_modes(&mut self) {
        let modes = self
            .focused()
            .and_then(|p| self.views.get(&p))
            .map(|v| {
                let s = v.parser.screen();
                format!(
                    "{}{}{}",
                    if s.application_cursor() {
                        "\x1b[?1h"
                    } else {
                        "\x1b[?1l"
                    },
                    if s.application_keypad() {
                        "\x1b="
                    } else {
                        "\x1b>"
                    },
                    if s.bracketed_paste() {
                        "\x1b[?2004h"
                    } else {
                        "\x1b[?2004l"
                    },
                )
            })
            .unwrap_or_default();
        if modes != self.outer_modes {
            let mut out = std::io::stdout();
            let _ = out.write_all(modes.as_bytes());
            let _ = out.flush();
            self.outer_modes = modes;
        }
    }

    // ---------- messages ----------

    fn handle(&mut self, m: Msg) -> Result<()> {
        match m {
            Msg::Snapshot(s) => {
                self.session = s.session;
                self.panes = s.panes.into_iter().map(|p| (p.id.clone(), p)).collect();
                self.agents = s.agents;
                self.questions = s.questions;
                self.messages = s.messages.into_iter().collect();
                self.pick_start_workspace();
            }
            Msg::Event(e) => match *e {
                Event::Panes { panes } => {
                    self.panes = panes.into_iter().map(|p| (p.id.clone(), p)).collect();
                }
                Event::Session { session } => {
                    if session.version >= self.session.version {
                        self.session = session;
                    }
                    if self
                        .ws_id
                        .as_deref()
                        .is_some_and(|id| self.session.workspace(id).is_none())
                    {
                        self.ws_id = self.session.workspaces.first().map(|w| w.id.clone());
                    }
                    if self.ws_id.is_none() {
                        self.ws_id = self.session.workspaces.first().map(|w| w.id.clone());
                    }
                }
                Event::Agent { agent } => {
                    match self.agents.iter_mut().find(|a| a.name == agent.name) {
                        Some(a) => *a = agent,
                        None => self.agents.push(agent),
                    }
                }
                Event::Question { question } => {
                    self.questions.retain(|q| q.id != question.id);
                    if question.state == "open" {
                        self.questions.push(question);
                        self.flash("⚑ an agent asks you something — prefix a");
                    }
                }
                Event::Message { message } => {
                    if message.to == HUMAN && message.kind == "msg" {
                        self.flash(format!("💬 {}: {}", message.from, message.text));
                    }
                    self.messages.push_back(message);
                    while self.messages.len() > 500 {
                        self.messages.pop_front();
                    }
                }
            },
            Msg::EventsClosed(e) => return Err(anyhow!("lost orchd: {e}")),
            Msg::Frame { pane, stream } => {
                let Some(v) = self.views.get_mut(&pane) else {
                    return Ok(());
                };
                match stream {
                    Stream::Screen {
                        base,
                        rev,
                        full,
                        data,
                        ..
                    } => {
                        if !full && base != v.rev {
                            let _ = v.tx.send(Request::Resnapshot {});
                        } else {
                            v.parser.process(data.as_bytes());
                            v.rev = rev;
                        }
                    }
                    Stream::Role { role } => v.role = role,
                    Stream::Exit { code } => v.exited = Some(code),
                    _ => {}
                }
            }
            Msg::AttachFailed { pane, error } => {
                self.views.remove(&pane);
                self.flash(format!("{pane}: {error}"));
            }
            Msg::Keys(bytes) => self.keys(bytes),
            Msg::Reply { what, result } => match result {
                Ok(v) => {
                    if let Ok(s) = serde_json::from_value::<Session>(v["session"].clone()) {
                        if s.version >= self.session.version {
                            self.session = s;
                        }
                    }
                    // A new pane: follow it.
                    if let Some(id) = v["pane"]["id"].as_str() {
                        if let Some((wi, _)) = self.session.locate(id) {
                            self.ws_id = Some(self.session.workspaces[wi].id.clone());
                        }
                    }
                    if what == "answer" {
                        if v["won"] == false {
                            self.flash("already answered elsewhere");
                        } else {
                            self.flash("answered ✓");
                        }
                    }
                }
                Err(e) => self.flash(format!("{what}: {e}")),
            },
        }
        Ok(())
    }

    fn pick_start_workspace(&mut self) {
        let cwd = std::env::current_dir()
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_default();
        self.ws_id = self
            .session
            .workspaces
            .iter()
            .find(|w| w.cwd == cwd)
            .or_else(|| self.session.workspaces.first())
            .map(|w| w.id.clone());
        if self.session.workspaces.is_empty() && !self.started_ws {
            // First start: a workspace with a shell in this directory.
            self.started_ws = true;
            let mut spawn = self.spawn_here(Vec::new());
            spawn.cwd = Some(cwd);
            self.ui("new workspace", UiOp::NewWorkspace { label: None, spawn });
        }
    }

    // ---------- keys ----------

    fn keys(&mut self, bytes: Vec<u8>) {
        let mut i = 0;
        let mut forward: Vec<u8> = Vec::new();
        while i < bytes.len() {
            // Escape sequences travel as one key.
            let key_len = key_len(&bytes[i..]);
            let key = &bytes[i..i + key_len];
            i += key_len;
            match self.mode {
                Mode::Normal if !self.prefix_pending => {
                    if key == [self.prefix] {
                        self.prefix_pending = true;
                    } else {
                        forward.extend_from_slice(key);
                    }
                }
                Mode::Normal => {
                    self.prefix_pending = false;
                    if !forward.is_empty() {
                        self.send_input(std::mem::take(&mut forward));
                    }
                    if key == [self.prefix] {
                        forward.push(self.prefix);
                    } else {
                        self.command(key);
                    }
                }
                _ => self.overlay_key(key),
            }
        }
        if !forward.is_empty() {
            self.send_input(forward);
        }
    }

    fn command(&mut self, key: &[u8]) {
        let focused = self.focused();
        let ws = self
            .current_ws()
            .map(|w| (w.id.clone(), w.active_tab, w.tabs.len()));
        let toward = match key {
            b"h" | b"\x1b[D" => Some(Toward::Left),
            b"l" | b"\x1b[C" => Some(Toward::Right),
            b"k" | b"\x1b[A" => Some(Toward::Up),
            b"j" | b"\x1b[B" => Some(Toward::Down),
            _ => None,
        };
        if let Some(t) = toward {
            let rects: Vec<(String, LRect)> = self
                .pane_rects()
                .into_iter()
                .map(|(p, r)| (p, to_lrect(r)))
                .collect();
            if let Some(n) = focused.as_deref().and_then(|f| neighbor(&rects, f, t)) {
                self.ui("focus", UiOp::Focus { pane: n });
            }
            return;
        }
        match key {
            b"c" => {
                if let Some((ws, _, _)) = ws {
                    let spawn = self.spawn_here(Vec::new());
                    self.ui(
                        "new tab",
                        UiOp::NewTab {
                            workspace: ws,
                            spawn,
                        },
                    );
                }
            }
            b"v" | b"%" => self.split(Dir::Right, Vec::new()),
            b"s" | b"\"" => self.split(Dir::Down, Vec::new()),
            b"x" => {
                if let Some(p) = focused {
                    self.mode = Mode::Prompt(Prompt::ConfirmClose(p), String::new());
                }
            }
            b"z" => {
                if let Some(p) = focused {
                    self.ui("zoom", UiOp::ToggleZoom { pane: p });
                }
            }
            b"H" | b"L" | b"K" | b"J" => {
                if let Some(p) = focused {
                    let (dir, delta) = match key {
                        b"H" => (Dir::Right, -0.05),
                        b"L" => (Dir::Right, 0.05),
                        b"K" => (Dir::Down, -0.05),
                        _ => (Dir::Down, 0.05),
                    };
                    self.ui(
                        "resize",
                        UiOp::Resize {
                            pane: p,
                            dir,
                            delta,
                        },
                    );
                }
            }
            b"n" | b"p" => {
                if let Some((ws, active, n)) = ws {
                    if n > 1 {
                        let index = if key == b"n" {
                            (active + 1) % n
                        } else {
                            (active + n - 1) % n
                        };
                        self.ui(
                            "tab",
                            UiOp::SelectTab {
                                workspace: ws,
                                index,
                            },
                        );
                    }
                }
            }
            [d @ b'1'..=b'9'] => {
                if let Some((ws, _, _)) = ws {
                    let index = (d - b'1') as usize;
                    self.ui(
                        "tab",
                        UiOp::SelectTab {
                            workspace: ws,
                            index,
                        },
                    );
                }
            }
            b"(" | b")" => {
                let n = self.session.workspaces.len();
                if n > 1 {
                    let cur = self
                        .session
                        .workspaces
                        .iter()
                        .position(|w| Some(&w.id) == self.ws_id.as_ref())
                        .unwrap_or(0);
                    let next = if key == b")" {
                        (cur + 1) % n
                    } else {
                        (cur + n - 1) % n
                    };
                    self.ws_id = Some(self.session.workspaces[next].id.clone());
                }
            }
            b"w" => {
                let cur = self
                    .session
                    .workspaces
                    .iter()
                    .position(|w| Some(&w.id) == self.ws_id.as_ref())
                    .unwrap_or(0);
                self.mode = Mode::Workspaces(cur);
            }
            b"N" => {
                self.mode = Mode::Prompt(
                    Prompt::NewWorkspace,
                    self.current_ws().map(|w| w.cwd.clone()).unwrap_or_default(),
                )
            }
            b"A" => self.mode = Mode::Prompt(Prompt::NewAgent, "claude".into()),
            b"," => self.mode = Mode::Prompt(Prompt::RenameTab, String::new()),
            b"$" => {
                self.mode = Mode::Prompt(
                    Prompt::RenameWorkspace,
                    self.current_ws()
                        .map(|w| w.label.clone())
                        .unwrap_or_default(),
                )
            }
            b"b" => self.sidebar = !self.sidebar,
            b"a" => self.mode = Mode::NeedsYou(0),
            b"m" => self.mode = Mode::Comms(String::new()),
            b"t" => {
                if let Some(v) = focused.as_ref().and_then(|p| self.views.get(p)) {
                    let _ = v.tx.send(Request::Takeover {});
                }
            }
            b"d" | b"q" => self.quit = true,
            b"?" => self.mode = Mode::Help,
            _ => {}
        }
    }

    fn split(&mut self, dir: Dir, argv: Vec<String>) {
        if let Some(p) = self.focused() {
            let spawn = self.spawn_here(argv);
            self.ui(
                "split",
                UiOp::Split {
                    pane: p,
                    dir,
                    spawn,
                },
            );
        }
    }

    fn needs_you(&self) -> Vec<(String, Option<i64>, Option<String>)> {
        // (label, question id, pane to jump to)
        let mut out: Vec<(String, Option<i64>, Option<String>)> = self
            .questions
            .iter()
            .map(|q| {
                let opts = if q.options.is_empty() {
                    String::new()
                } else {
                    format!(
                        "  [{}]",
                        q.options
                            .iter()
                            .enumerate()
                            .map(|(i, o)| format!("{} {o}", i + 1))
                            .collect::<Vec<_>>()
                            .join(" · ")
                    )
                };
                (
                    format!("❓ #{} {}: {}{opts}", q.id, q.from, q.text),
                    Some(q.id),
                    None,
                )
            })
            .collect();
        let mut blocked: Vec<&PaneInfo> = self
            .panes
            .values()
            .filter(|p| p.agent_status.as_deref() == Some("blocked"))
            .collect();
        blocked.sort_by(|a, b| a.id.cmp(&b.id));
        for p in blocked {
            let ws = self
                .session
                .locate(&p.id)
                .map(|(wi, _)| self.session.workspaces[wi].label.clone())
                .unwrap_or_default();
            out.push((
                format!(
                    "⚠ {} ({}, {ws}) waits for a decision on screen",
                    p.id,
                    p.agent.clone().unwrap_or_default()
                ),
                None,
                Some(p.id.clone()),
            ));
        }
        out
    }

    fn overlay_key(&mut self, key: &[u8]) {
        let esc = key == b"\x1b";
        let enter = key == b"\r" || key == b"\n";
        let up = key == b"\x1b[A" || key == b"k";
        let down = key == b"\x1b[B" || key == b"j";
        let mode = std::mem::replace(&mut self.mode, Mode::Normal);
        self.mode = match mode {
            Mode::Help => Mode::Normal,
            Mode::Workspaces(sel) => {
                let n = self.session.workspaces.len();
                if esc {
                    Mode::Normal
                } else if enter {
                    if let Some(w) = self.session.workspaces.get(sel) {
                        self.ws_id = Some(w.id.clone());
                    }
                    Mode::Normal
                } else if up {
                    Mode::Workspaces(sel.saturating_sub(1))
                } else if down {
                    Mode::Workspaces((sel + 1).min(n.saturating_sub(1)))
                } else if let [d @ b'1'..=b'9'] = key {
                    if let Some(w) = self.session.workspaces.get((d - b'1') as usize) {
                        self.ws_id = Some(w.id.clone());
                    }
                    Mode::Normal
                } else {
                    Mode::Workspaces(sel)
                }
            }
            Mode::NeedsYou(sel) => {
                let items = self.needs_you();
                if esc {
                    Mode::Normal
                } else if up {
                    Mode::NeedsYou(sel.saturating_sub(1))
                } else if down {
                    Mode::NeedsYou((sel + 1).min(items.len().saturating_sub(1)))
                } else if enter {
                    match items.get(sel) {
                        Some((_, Some(q), _)) => Mode::Prompt(Prompt::Answer(*q), String::new()),
                        Some((_, None, Some(pane))) => {
                            if let Some((wi, _)) = self.session.locate(pane) {
                                self.ws_id = Some(self.session.workspaces[wi].id.clone());
                            }
                            self.ui("focus", UiOp::Focus { pane: pane.clone() });
                            Mode::Normal
                        }
                        _ => Mode::Normal,
                    }
                } else {
                    Mode::NeedsYou(sel)
                }
            }
            Mode::Comms(mut input) => {
                if esc {
                    Mode::Normal
                } else if enter {
                    let text = input.trim().to_string();
                    if let Some(rest) = text.strip_prefix('@') {
                        if let Some((to, body)) = rest.split_once(' ') {
                            self.request(
                                "message",
                                Request::SendMessage(SendMessage {
                                    from: HUMAN.into(),
                                    to: to.into(),
                                    text: body.trim().into(),
                                    thread: None,
                                }),
                            );
                            Mode::Comms(String::new())
                        } else {
                            self.flash("write: @agent message");
                            Mode::Comms(input)
                        }
                    } else {
                        if !text.is_empty() {
                            self.flash("start with @agent-name (or @all)");
                        }
                        Mode::Comms(input)
                    }
                } else {
                    edit_line(&mut input, key);
                    Mode::Comms(input)
                }
            }
            Mode::Prompt(p, mut input) => {
                if esc {
                    Mode::Normal
                } else if let Prompt::ConfirmClose(pane) = &p {
                    if key == b"y" || key == b"Y" {
                        self.ui("close", UiOp::ClosePane { pane: pane.clone() });
                    }
                    Mode::Normal
                } else if enter {
                    self.submit_prompt(p, input.trim().to_string());
                    Mode::Normal
                } else {
                    edit_line(&mut input, key);
                    Mode::Prompt(p, input)
                }
            }
            Mode::Normal => Mode::Normal,
        };
    }

    fn submit_prompt(&mut self, p: Prompt, input: String) {
        let ws = self.current_ws().map(|w| (w.id.clone(), w.active_tab));
        match p {
            Prompt::NewWorkspace => {
                let cwd = if input.is_empty() {
                    None
                } else {
                    Some(expand_home(&input))
                };
                let mut spawn = self.spawn_here(Vec::new());
                spawn.cwd = cwd;
                self.ui("new workspace", UiOp::NewWorkspace { label: None, spawn });
            }
            Prompt::NewAgent => {
                if let Some((ws, _)) = ws {
                    let argv: Vec<String> = input.split_whitespace().map(str::to_string).collect();
                    if argv.is_empty() {
                        return;
                    }
                    let spawn = self.spawn_here(argv);
                    self.ui(
                        "new agent",
                        UiOp::NewTab {
                            workspace: ws,
                            spawn,
                        },
                    );
                }
            }
            Prompt::RenameTab => {
                if let (Some((ws, index)), false) = (ws, input.is_empty()) {
                    self.ui(
                        "rename",
                        UiOp::RenameTab {
                            workspace: ws,
                            index,
                            label: input,
                        },
                    );
                }
            }
            Prompt::RenameWorkspace => {
                if let (Some((ws, _)), false) = (ws, input.is_empty()) {
                    self.ui(
                        "rename",
                        UiOp::RenameWorkspace {
                            workspace: ws,
                            label: input,
                        },
                    );
                }
            }
            Prompt::Answer(id) => {
                if input.is_empty() {
                    return;
                }
                let mut answer = input.clone();
                if let Ok(n) = input.parse::<usize>() {
                    if let Some(o) = self
                        .questions
                        .iter()
                        .find(|q| q.id == id)
                        .and_then(|q| q.options.get(n.wrapping_sub(1)))
                    {
                        answer = o.clone();
                    }
                }
                self.request(
                    "answer",
                    Request::Answer {
                        id,
                        answer,
                        by: "tui".into(),
                    },
                );
            }
            Prompt::ConfirmClose(_) => {}
        }
    }

    // ---------- drawing ----------

    fn draw(&self, f: &mut ratatui::Frame) {
        let full = f.area();
        let main = self.main_area();
        // Gaps between panes show as a dim separator background.
        f.render_widget(
            Block::default().style(Style::default().bg(Color::Indexed(236))),
            main,
        );
        let focused = self.focused();
        let mut cursor = None;
        for (pane, rect) in self.pane_rects() {
            let is_focus = focused.as_deref() == Some(pane.as_str());
            let info = self.panes.get(&pane);
            let (mark, color) = status_mark(info.and_then(|i| i.agent_status.as_deref()));
            let name = info
                .and_then(|i| i.name.clone())
                .or_else(|| info.and_then(|i| i.agent.clone()))
                .unwrap_or_else(|| info.map(|i| i.argv.join(" ")).unwrap_or_default());
            let title_text = info.map(|i| i.title.clone()).unwrap_or_default();
            let exited = info
                .and_then(|i| i.exit_code.map(|c| format!(" exited {c}")))
                .unwrap_or_default();
            let bar_style = if is_focus {
                Style::default()
                    .bg(Color::Blue)
                    .fg(Color::White)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().bg(Color::Indexed(238)).fg(Color::Gray)
            };
            let title = Line::from(vec![
                Span::styled(format!(" {mark} "), bar_style.fg(color)),
                Span::styled(format!("{pane} {name}{exited} "), bar_style),
                Span::styled(
                    if title_text.is_empty() {
                        String::new()
                    } else {
                        format!("· {title_text}")
                    },
                    bar_style.remove_modifier(Modifier::BOLD),
                ),
            ]);
            f.render_widget(
                Paragraph::new(title).style(bar_style),
                Rect::new(rect.x, rect.y, rect.width, 1.min(rect.height)),
            );
            let inner = Self::inner(rect);
            f.render_widget(Clear, inner);
            match self.views.get(&pane) {
                Some(v) => {
                    f.render_widget(ScreenWidget(v.parser.screen()), inner);
                    let s = v.parser.screen();
                    if is_focus && !s.hide_cursor() {
                        let (r, c) = s.cursor_position();
                        if r < inner.height && c < inner.width {
                            cursor = Some((inner.x + c, inner.y + r));
                        }
                    }
                }
                None => f.render_widget(Paragraph::new(" connecting…"), inner),
            }
        }
        if self.current_tab().is_none() {
            f.render_widget(
                Paragraph::new(format!(
                    "\n  No workspace yet.  {} N = new workspace, {} ? = help",
                    prefix_name(self.prefix),
                    prefix_name(self.prefix)
                )),
                main,
            );
        }
        if self.sidebar && full.width >= 60 {
            self.draw_sidebar(
                f,
                Rect::new(
                    0,
                    0,
                    SIDEBAR_W.min(full.width),
                    full.height.saturating_sub(1),
                ),
            );
        }
        self.draw_status(
            f,
            Rect::new(0, full.height.saturating_sub(1), full.width, 1),
        );
        match &self.mode {
            Mode::Normal => {
                if let Some((x, y)) = cursor {
                    f.set_cursor_position((x, y));
                }
            }
            Mode::Help => self.draw_help(f),
            Mode::Workspaces(sel) => self.draw_workspaces(f, *sel),
            Mode::NeedsYou(sel) => self.draw_needs_you(f, *sel),
            Mode::Comms(input) => self.draw_comms(f, input),
            Mode::Prompt(p, input) => self.draw_prompt(f, p, input),
        }
    }

    fn ws_status(&self, w: &orch_proto::layout::Workspace) -> Option<&'static str> {
        let mut best: Option<&'static str> = None;
        for t in &w.tabs {
            for p in t.root.panes() {
                match self.panes.get(&p).and_then(|i| i.agent_status.as_deref()) {
                    Some("blocked") => return Some("blocked"),
                    Some("working") => best = Some("working"),
                    Some("idle") if best.is_none() => best = Some("idle"),
                    _ => {}
                }
            }
        }
        best
    }

    fn draw_sidebar(&self, f: &mut ratatui::Frame, area: Rect) {
        let mut lines: Vec<Line> = vec![Line::from(Span::styled(
            " WORKSPACES",
            Style::default()
                .fg(Color::DarkGray)
                .add_modifier(Modifier::BOLD),
        ))];
        for (i, w) in self.session.workspaces.iter().enumerate() {
            let cur = Some(&w.id) == self.ws_id.as_ref() || (self.ws_id.is_none() && i == 0);
            let (mark, color) = status_mark(self.ws_status(w));
            let style = if cur {
                Style::default()
                    .fg(Color::White)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(Color::Gray)
            };
            lines.push(Line::from(vec![
                Span::styled(format!(" {mark}"), Style::default().fg(color)),
                Span::styled(format!(" {} {}", i + 1, w.label), style),
                Span::styled(
                    format!(
                        " {}",
                        if w.tabs.len() > 1 {
                            format!("·{}", w.tabs.len())
                        } else {
                            String::new()
                        }
                    ),
                    Style::default().fg(Color::DarkGray),
                ),
            ]));
        }
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            " AGENTS",
            Style::default()
                .fg(Color::DarkGray)
                .add_modifier(Modifier::BOLD),
        )));
        // Agents on screen (pane detection) + agents on the bus.
        let mut shown: Vec<String> = Vec::new();
        let mut panes: Vec<&PaneInfo> = self.panes.values().filter(|p| p.agent.is_some()).collect();
        panes.sort_by(|a, b| a.id.cmp(&b.id));
        for p in panes {
            let bus = self
                .agents
                .iter()
                .find(|a| a.pane.as_deref() == Some(&format!("orch:{}", p.id)));
            let name = bus
                .map(|a| a.name.clone())
                .unwrap_or_else(|| format!("{} {}", p.id, p.agent.clone().unwrap_or_default()));
            if let Some(b) = bus {
                shown.push(b.name.clone());
            }
            let (mark, color) = status_mark(p.agent_status.as_deref());
            lines.push(Line::from(vec![
                Span::styled(format!(" {mark} "), Style::default().fg(color)),
                Span::raw(truncate(&name, (area.width as usize).saturating_sub(4))),
            ]));
        }
        for a in &self.agents {
            if shown.contains(&a.name) || a.pane.as_deref().is_some_and(|p| p.starts_with("orch:"))
            {
                continue;
            }
            let (mark, color) = status_mark(Some(a.status.as_str()));
            lines.push(Line::from(vec![
                Span::styled(format!(" {mark} "), Style::default().fg(color)),
                Span::styled(
                    truncate(&a.name, (area.width as usize).saturating_sub(4)),
                    Style::default().fg(Color::Gray),
                ),
            ]));
        }
        let open = self.needs_you().len();
        if open > 0 {
            lines.push(Line::from(""));
            lines.push(Line::from(Span::styled(
                format!(" ⚑ {open} need you  ({} a)", prefix_name(self.prefix)),
                Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
            )));
        }
        f.render_widget(
            Paragraph::new(lines).block(
                Block::default()
                    .borders(Borders::RIGHT)
                    .border_style(Style::default().fg(Color::DarkGray)),
            ),
            area,
        );
    }

    fn draw_status(&self, f: &mut ratatui::Frame, area: Rect) {
        let mut spans: Vec<Span> = Vec::new();
        let base = Style::default().bg(Color::Indexed(235)).fg(Color::Gray);
        if self.prefix_pending {
            spans.push(Span::styled(
                " PREFIX ",
                Style::default().bg(Color::Yellow).fg(Color::Black),
            ));
        }
        if let Some(w) = self.current_ws() {
            spans.push(Span::styled(
                format!(" {} ", w.label),
                base.fg(Color::White).add_modifier(Modifier::BOLD),
            ));
            for (i, t) in w.tabs.iter().enumerate() {
                let style = if i == w.active_tab {
                    base.bg(Color::Blue).fg(Color::White)
                } else {
                    base
                };
                let label = if t.label == (i + 1).to_string() {
                    t.label.clone()
                } else {
                    format!("{}:{}", i + 1, t.label)
                };
                let zoom = if t.zoomed { "+" } else { "" };
                spans.push(Span::styled(format!(" {label}{zoom} "), style));
            }
        }
        if let Some((msg, _)) = &self.status {
            spans.push(Span::styled(format!("  {msg}"), base.fg(Color::Yellow)));
        }
        let open = self.needs_you().len();
        let working = self
            .panes
            .values()
            .filter(|p| p.agent_status.as_deref() == Some("working"))
            .count();
        let idle = self
            .panes
            .values()
            .filter(|p| p.agent_status.as_deref() == Some("idle"))
            .count();
        let clock = {
            let t = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs() as i64)
                .unwrap_or(0);
            let mut tm: libc::tm = unsafe { std::mem::zeroed() };
            unsafe { libc::localtime_r(&(t as libc::time_t), &mut tm) };
            format!("{:02}:{:02}", tm.tm_hour, tm.tm_min)
        };
        let right = format!(
            "{}● {working}  ○ {idle}  {clock}  {} ? help ",
            if open > 0 {
                format!("⚑ {open}  ")
            } else {
                String::new()
            },
            prefix_name(self.prefix)
        );
        let left_w: usize = spans.iter().map(|s| s.width()).sum();
        let pad = (area.width as usize).saturating_sub(left_w + right.chars().count());
        spans.push(Span::styled(" ".repeat(pad), base));
        spans.push(Span::styled(
            right,
            if open > 0 { base.fg(Color::Red) } else { base },
        ));
        f.render_widget(Paragraph::new(Line::from(spans)).style(base), area);
    }

    fn popup(&self, f: &mut ratatui::Frame, title: &str, w: u16, h: u16) -> Rect {
        let full = f.area();
        let w = w.min(full.width.saturating_sub(2));
        let h = h.min(full.height.saturating_sub(2));
        let r = Rect::new((full.width - w) / 2, (full.height - h) / 2, w, h);
        f.render_widget(Clear, r);
        f.render_widget(
            Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(Color::Blue))
                .title(format!(" {title} ")),
            r,
        );
        Rect::new(
            r.x + 1,
            r.y + 1,
            r.width.saturating_sub(2),
            r.height.saturating_sub(2),
        )
    }

    fn draw_help(&self, f: &mut ratatui::Frame) {
        let p = prefix_name(self.prefix);
        let text = format!(
            "All commands: {p} then the key.\n\n\
             c      new tab (shell)          A   new agent tab (claude, codex, …)\n\
             v / %  split right              s / \"  split down\n\
             h j k l / arrows  move focus    H J K L   resize\n\
             z      zoom pane                x   close pane (kills it)\n\
             n / p  next / previous tab      1-9 go to tab\n\
             w      workspaces               ( / )  previous / next workspace\n\
             N      new workspace (cwd)      $   rename workspace   ,  rename tab\n\
             a      needs you: questions + blocked agents\n\
             m      messages (timeline, write @agent text)\n\
             b      sidebar                  t   take control of the pane\n\
             d      detach (everything keeps running)\n\
             {p} {p}  send {p} to the pane\n\n\
             Any key closes this help."
        );
        let r = self.popup(f, "orch — keys", 72, 22);
        f.render_widget(Paragraph::new(text), r);
    }

    fn draw_workspaces(&self, f: &mut ratatui::Frame, sel: usize) {
        let r = self.popup(
            f,
            "workspaces — Enter / 1-9, Esc",
            60,
            (self.session.workspaces.len() as u16 + 4).max(6),
        );
        let lines: Vec<Line> = self
            .session
            .workspaces
            .iter()
            .enumerate()
            .map(|(i, w)| {
                let (mark, color) = status_mark(self.ws_status(w));
                let style = if i == sel {
                    Style::default().bg(Color::Blue).fg(Color::White)
                } else {
                    Style::default()
                };
                Line::from(vec![
                    Span::styled(format!(" {mark} "), Style::default().fg(color)),
                    Span::styled(
                        format!(
                            "{} {:<20} {} tab(s)  {}",
                            i + 1,
                            w.label,
                            w.tabs.len(),
                            w.cwd
                        ),
                        style,
                    ),
                ])
            })
            .collect();
        f.render_widget(Paragraph::new(lines), r);
    }

    fn draw_needs_you(&self, f: &mut ratatui::Frame, sel: usize) {
        let items = self.needs_you();
        let r = self.popup(
            f,
            "needs you — Enter answers / jumps, Esc",
            90,
            (items.len() as u16 * 2 + 4).max(6),
        );
        if items.is_empty() {
            f.render_widget(Paragraph::new(" Nothing waits for you. ✓"), r);
            return;
        }
        let lines: Vec<Line> = items
            .iter()
            .enumerate()
            .map(|(i, (label, _, _))| {
                let style = if i == sel {
                    Style::default().bg(Color::Blue).fg(Color::White)
                } else {
                    Style::default()
                };
                Line::from(Span::styled(format!(" {label}"), style))
            })
            .collect();
        f.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), r);
    }

    fn draw_comms(&self, f: &mut ratatui::Frame, input: &str) {
        let full = f.area();
        let r = self.popup(
            f,
            "messages — @agent text + Enter, Esc",
            full.width.saturating_sub(8),
            full.height.saturating_sub(4),
        );
        let h = r.height.saturating_sub(2) as usize;
        let lines: Vec<Line> = self
            .messages
            .iter()
            .rev()
            .take(h)
            .rev()
            .map(|m| {
                let tag = match (m.kind.as_str(), m.question) {
                    ("ask", Some(q)) => format!(" ❓#{q}"),
                    ("answer", Some(q)) => format!(" ✅#{q}"),
                    _ => String::new(),
                };
                Line::from(vec![
                    Span::styled(
                        format!("{} ", crate::bus::clock(m.ts_ms)),
                        Style::default().fg(Color::DarkGray),
                    ),
                    Span::styled(
                        m.from.clone(),
                        Style::default().add_modifier(Modifier::BOLD),
                    ),
                    Span::raw(format!(" → {}{tag}: ", m.to)),
                    Span::raw(m.text.replace('\n', " ")),
                ])
            })
            .collect();
        f.render_widget(
            Paragraph::new(lines),
            Rect::new(r.x, r.y, r.width, r.height.saturating_sub(2)),
        );
        let agents: Vec<String> = self.agents.iter().map(|a| a.name.clone()).collect();
        f.render_widget(
            Paragraph::new(vec![
                Line::from(Span::styled(
                    format!(" agents: {}", agents.join(", ")),
                    Style::default().fg(Color::DarkGray),
                )),
                Line::from(vec![
                    Span::styled(" › ", Style::default().fg(Color::Blue)),
                    Span::raw(input.to_string()),
                ]),
            ]),
            Rect::new(r.x, r.y + r.height.saturating_sub(2), r.width, 2),
        );
        f.set_cursor_position((
            r.x + 3 + input.chars().count() as u16,
            r.y + r.height.saturating_sub(1),
        ));
    }

    fn draw_prompt(&self, f: &mut ratatui::Frame, p: &Prompt, input: &str) {
        let (title, hint) = match p {
            Prompt::NewWorkspace => ("new workspace", "directory:".to_string()),
            Prompt::NewAgent => ("new agent tab", "command (claude, codex, …):".to_string()),
            Prompt::RenameTab => ("rename tab", "name:".to_string()),
            Prompt::RenameWorkspace => ("rename workspace", "name:".to_string()),
            Prompt::ConfirmClose(pane) => {
                ("close pane", format!("Kill {pane} and close it? y / n"))
            }
            Prompt::Answer(id) => {
                let q = self.questions.iter().find(|q| q.id == *id);
                let opts = q
                    .map(|q| {
                        q.options
                            .iter()
                            .enumerate()
                            .map(|(i, o)| format!("{}) {o}", i + 1))
                            .collect::<Vec<_>>()
                            .join("   ")
                    })
                    .unwrap_or_default();
                (
                    "answer",
                    format!(
                        "{}\n{}\nanswer (number = option):",
                        q.map(|q| format!("{}: {}", q.from, q.text))
                            .unwrap_or_default(),
                        opts
                    ),
                )
            }
        };
        let r = self.popup(f, title, 76, 8);
        let mut lines: Vec<Line> = hint.lines().map(|l| Line::from(l.to_string())).collect();
        if !matches!(p, Prompt::ConfirmClose(_)) {
            lines.push(Line::from(vec![
                Span::styled("› ", Style::default().fg(Color::Blue)),
                Span::raw(input.to_string()),
            ]));
            let y = r.y + lines.len() as u16 - 1;
            f.set_cursor_position((r.x + 2 + input.chars().count() as u16, y));
        }
        f.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), r);
    }
}

fn truncate(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        s.to_string()
    } else {
        let mut t: String = s.chars().take(n.saturating_sub(1)).collect();
        t.push('…');
        t
    }
}

fn expand_home(p: &str) -> String {
    match (p.strip_prefix('~'), std::env::var("HOME")) {
        (Some(rest), Ok(home)) => format!("{home}{rest}"),
        _ => p.to_string(),
    }
}

/// Line editing for overlay inputs: printable text, Backspace, Ctrl-U.
fn edit_line(input: &mut String, key: &[u8]) {
    match key {
        [0x7f] | [0x08] => {
            input.pop();
        }
        [0x15] => input.clear(),
        k if k.first().is_some_and(|b| *b >= 0x20 && *b != 0x7f) => {
            if let Ok(s) = std::str::from_utf8(k) {
                input.push_str(s);
            }
        }
        _ => {}
    }
}

/// Length of the key starting at `b`: an escape sequence (CSI / SS3 / Alt-key), a UTF-8
/// character, or one byte.
pub fn key_len(b: &[u8]) -> usize {
    if b.is_empty() {
        return 0;
    }
    if b[0] == 0x1b && b.len() > 1 {
        match b[1] {
            b'[' => {
                // CSI: parameters/intermediates then a final byte 0x40..=0x7e.
                for (i, &c) in b.iter().enumerate().skip(2) {
                    if (0x40..=0x7e).contains(&c) {
                        return i + 1;
                    }
                }
                return b.len();
            }
            b'O' if b.len() > 2 => return 3,
            _ => return 2,
        }
    }
    let n = match b[0] {
        0xc0..=0xdf => 2,
        0xe0..=0xef => 3,
        0xf0..=0xf7 => 4,
        _ => 1,
    };
    n.min(b.len())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_split_into_sequences() {
        assert_eq!(key_len(b"\x1b[A rest"), 3);
        assert_eq!(key_len(b"\x1b[1;5C"), 6);
        assert_eq!(key_len(b"\x1bOA"), 3);
        assert_eq!(key_len(b"\x1bx"), 2);
        assert_eq!(key_len(b"\x1b"), 1);
        assert_eq!(key_len("界x".as_bytes()), 3);
        assert_eq!(key_len(b"ab"), 1);
    }

    #[test]
    fn line_editing() {
        let mut s = String::from("ab");
        edit_line(&mut s, b"c");
        edit_line(&mut s, b"\x7f");
        edit_line(&mut s, "ü".as_bytes());
        assert_eq!(s, "abü");
        edit_line(&mut s, b"\x15");
        assert!(s.is_empty());
    }
}
