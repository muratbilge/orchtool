//! Request / response / stream bodies carried inside [`crate::Frame`].
//!
//! Request body:  `{"method": "pane.new", "params": {...}}`
//! Reply body:    `{"result": ...}` or `{"error": {"code": "...", "message": "..."}}`
//! Stream body (server → attached client): `{"stream": "screen" | "exit" | "role", ...}`

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "method", content = "params")]
pub enum Request {
    #[serde(rename = "ping")]
    Ping {},
    #[serde(rename = "pane.new")]
    NewPane(NewPane),
    #[serde(rename = "pane.list")]
    ListPanes {},
    #[serde(rename = "pane.read")]
    Read(ReadPane),
    #[serde(rename = "pane.send")]
    Send(SendPane),
    #[serde(rename = "pane.kill")]
    Kill { pane: String },
    /// Turns the connection into an attach stream (see [`Stream`]).
    #[serde(rename = "pane.attach")]
    Attach(AttachPane),
    /// Attach stream only: raw keyboard bytes (base64) for the pane.
    #[serde(rename = "input")]
    Input { data_b64: String },
    /// Attach stream only: the client terminal changed size.
    #[serde(rename = "resize")]
    Resize { rows: u16, cols: u16 },
    /// Attach stream only: become the controlling client.
    #[serde(rename = "takeover")]
    Takeover {},
    /// Attach stream only: ask for a full snapshot.
    #[serde(rename = "resnapshot")]
    Resnapshot {},
    #[serde(rename = "daemon.stop")]
    StopDaemon {},

    // ---- agent bus (phase 2) ----
    /// Register (or re-identify) an agent; the server picks/returns its name.
    #[serde(rename = "agent.register")]
    RegisterAgent(RegisterAgent),
    #[serde(rename = "agent.status")]
    SetStatus {
        name: String,
        status: String,
        #[serde(default)]
        text: Option<String>,
    },
    #[serde(rename = "agent.list")]
    ListAgents {},
    #[serde(rename = "msg.send")]
    SendMessage(SendMessage),
    /// Unread messages for `name`, waiting up to `wait_ms`. Does NOT consume them: send
    /// `msg.ack` with the ids you actually delivered (a dropped reply then loses nothing).
    #[serde(rename = "msg.inbox")]
    Inbox {
        name: String,
        #[serde(default)]
        wait_ms: Option<u64>,
    },
    #[serde(rename = "msg.ack")]
    Ack { name: String, ids: Vec<i64> },
    /// Timeline rows after `after`; `follow` turns the connection into a stream.
    #[serde(rename = "msg.log")]
    Log {
        #[serde(default)]
        after: Option<i64>,
        #[serde(default)]
        limit: Option<usize>,
        #[serde(default)]
        follow: bool,
    },
    #[serde(rename = "ask.create")]
    Ask {
        from: String,
        text: String,
        #[serde(default)]
        options: Vec<String>,
    },
    /// Wait up to `wait_ms` for question `id` to be answered; returns the question either way.
    #[serde(rename = "ask.wait")]
    AskWait {
        id: i64,
        #[serde(default)]
        wait_ms: Option<u64>,
    },
    /// First answer wins; later answers get `won: false` and the existing answer.
    #[serde(rename = "ask.answer")]
    Answer { id: i64, answer: String, by: String },
    #[serde(rename = "ask.list")]
    ListQuestions {
        #[serde(default)]
        all: bool,
    },
    /// Start Telegram pairing: returns a one-time code to send to the bot as `/pair CODE`.
    #[serde(rename = "telegram.pair")]
    TelegramPair {},

    // ---- workspaces / TUI (phase 3) ----
    /// The layout: workspaces → tabs → split tree.
    #[serde(rename = "ui.get")]
    UiGet {},
    /// Change the layout (atomic on the server); replies `{session, pane?}`.
    #[serde(rename = "ui.op")]
    Ui(UiOp),
    /// Turns the connection into an event stream: first a `snapshot`, then `event`s.
    #[serde(rename = "events.subscribe")]
    Subscribe {},
}

/// How to start a program in a new pane.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Spawn {
    /// Empty = login shell.
    #[serde(default)]
    pub argv: Vec<String>,
    #[serde(default)]
    pub cwd: Option<String>,
    #[serde(default)]
    pub env: Option<Vec<(String, String)>>,
    #[serde(default)]
    pub rows: Option<u16>,
    #[serde(default)]
    pub cols: Option<u16>,
    #[serde(default)]
    pub name: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum UiOp {
    NewWorkspace {
        #[serde(default)]
        label: Option<String>,
        #[serde(default)]
        spawn: Spawn,
    },
    NewTab {
        workspace: String,
        #[serde(default)]
        spawn: Spawn,
    },
    Split {
        pane: String,
        dir: crate::layout::Dir,
        #[serde(default)]
        spawn: Spawn,
    },
    /// Kill the pane's program (if still running) and remove it from the layout.
    ClosePane {
        pane: String,
    },
    Focus {
        pane: String,
    },
    SelectTab {
        workspace: String,
        index: usize,
    },
    RenameWorkspace {
        workspace: String,
        label: String,
    },
    RenameTab {
        workspace: String,
        index: usize,
        label: String,
    },
    Resize {
        pane: String,
        dir: crate::layout::Dir,
        delta: f32,
    },
    ToggleZoom {
        pane: String,
    },
    /// Focus a pane and bring its workspace on screen in every open TUI (phone / CLI).
    Raise {
        pane: String,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum Event {
    /// Panes were created, exited, changed status or title: the full list is cheap.
    Panes {
        panes: Vec<PaneInfo>,
    },
    Session {
        session: crate::layout::Session,
    },
    /// The complete current lists (authoritative; never applied as deltas).
    Agents {
        agents: Vec<AgentRow>,
    },
    Questions {
        questions: Vec<QuestionRow>,
    },
    Message {
        message: MessageRow,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Snapshot {
    pub session: crate::layout::Session,
    pub panes: Vec<PaneInfo>,
    pub agents: Vec<AgentRow>,
    pub questions: Vec<QuestionRow>,
    pub messages: Vec<MessageRow>,
}

/// The human's name on the bus ("me" / "human" are accepted as aliases).
pub const HUMAN: &str = "murat";

pub fn normalize_name(name: &str) -> String {
    match name.trim() {
        "me" | "human" | "user" | "Murat" => HUMAN.to_string(),
        other => other.to_string(),
    }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct RegisterAgent {
    /// "claude", "codex", "shell", …
    pub kind: String,
    /// "orch:p3" or "herdr:w1C:p1"
    #[serde(default)]
    pub pane: Option<String>,
    #[serde(default)]
    pub cwd: Option<String>,
    /// Explicit name ($ORCH_AGENT) — otherwise derived from cwd + kind.
    #[serde(default)]
    pub name: Option<String>,
    /// Stable id of this agent process/session (used when there is no pane).
    #[serde(default)]
    pub instance: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SendMessage {
    pub from: String,
    /// Agent name, "murat", or "all".
    pub to: String,
    pub text: String,
    #[serde(default)]
    pub thread: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MessageRow {
    pub id: i64,
    pub ts_ms: i64,
    pub from: String,
    pub to: String,
    /// "msg", "ask", "answer", "status", "event"
    pub kind: String,
    pub text: String,
    pub thread: Option<String>,
    pub question: Option<i64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct QuestionRow {
    pub id: i64,
    pub ts_ms: i64,
    pub from: String,
    pub text: String,
    pub options: Vec<String>,
    /// "open" or "answered"
    pub state: String,
    pub answer: Option<String>,
    pub answered_by: Option<String>,
    pub answered_ts_ms: Option<i64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AgentRow {
    pub name: String,
    pub kind: String,
    pub pane: Option<String>,
    pub cwd: Option<String>,
    /// "working", "idle", "blocked", "done", "unknown"
    pub status: String,
    pub status_text: Option<String>,
    pub last_seen_ms: i64,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct NewPane {
    #[serde(default)]
    pub name: Option<String>,
    /// Empty = the user's login shell.
    #[serde(default)]
    pub argv: Vec<String>,
    #[serde(default)]
    pub cwd: Option<String>,
    /// Full environment for the child; `None` = orchd's environment.
    #[serde(default)]
    pub env: Option<Vec<(String, String)>>,
    #[serde(default)]
    pub rows: Option<u16>,
    #[serde(default)]
    pub cols: Option<u16>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReadSource {
    /// Only what is on screen now.
    Visible,
    /// Scrollback + screen, last `lines` lines.
    #[default]
    Recent,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ReadPane {
    pub pane: String,
    #[serde(default)]
    pub lines: Option<usize>,
    #[serde(default)]
    pub source: ReadSource,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct SendPane {
    pub pane: String,
    #[serde(default)]
    pub text: String,
    /// Press Enter (CR) after the text, after a short pause so TUIs don't treat it as paste.
    #[serde(default)]
    pub enter: bool,
    /// Conditional send (answering a dialog): only if the pane's recent text still has this
    /// `hash` (from `pane.read`), checked and sent atomically; and each screen state is
    /// answered once, so a second client's answer to the same dialog is refused.
    #[serde(default)]
    pub expect: Option<String>,
    /// Lines the `expect` hash covers (as in the read that produced it; default 60).
    #[serde(default)]
    pub expect_lines: Option<usize>,
}

/// The hash `pane.read` reports and `pane.send` `expect` checks (FNV-1a 64, hex).
pub fn text_hash(text: &str) -> String {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in text.bytes() {
        h ^= b as u64;
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("{h:016x}")
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AttachMode {
    /// Controller if nobody controls the pane, observer otherwise.
    #[default]
    Auto,
    /// Take control (the previous controller becomes an observer).
    Control,
    /// Watch only: no input, no resize of the pane.
    Observe,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AttachPane {
    pub pane: String,
    #[serde(default)]
    pub mode: AttachMode,
    pub rows: u16,
    pub cols: u16,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PaneState {
    Running,
    Exited,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PaneInfo {
    pub id: String,
    pub name: Option<String>,
    pub argv: Vec<String>,
    pub cwd: String,
    pub pid: Option<u32>,
    pub state: PaneState,
    pub exit_code: Option<u32>,
    pub rows: u16,
    pub cols: u16,
    pub clients: usize,
    pub title: String,
    pub created_unix: u64,
    /// "claude" / "codex" when the pane runs a known agent.
    #[serde(default)]
    pub agent: Option<String>,
    /// From the screen: "working", "blocked", "idle", "done" (agents only).
    #[serde(default)]
    pub agent_status: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ErrorBody {
    pub code: String,
    pub message: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    Controller,
    Observer,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "stream", rename_all = "snake_case")]
pub enum Stream {
    /// Terminal bytes that turn the client's view from revision `base` into `rev`.
    /// `full = true` means "clear and redraw" (base is then 0). A client whose own
    /// revision differs from `base` must send `resnapshot`.
    Screen {
        gen: u64,
        rows: u16,
        cols: u16,
        base: u64,
        rev: u64,
        full: bool,
        data: String,
        /// The program is on the alternate screen (less, vim, …): a viewer turns the wheel
        /// into arrow keys there instead of scrolling back. Not part of `data`, so a plain
        /// terminal client is not switched to its own alternate screen.
        #[serde(default)]
        alt: bool,
    },
    /// The client's role changed (attach, takeover by another client).
    Role { role: Role },
    /// The pane's process exited; the stream ends.
    Exit { code: Option<u32> },
    /// `msg.log` with `follow`: one new timeline row.
    Timeline { row: MessageRow },
    /// `events.subscribe`: initial state.
    Snapshot { snapshot: Box<Snapshot> },
    /// `events.subscribe`: a change.
    Event { event: Box<Event> },
}

/// The pane's id or its name.
pub fn pane_matches(info: &PaneInfo, key: &str) -> bool {
    info.id == key || info.name.as_deref() == Some(key)
}
