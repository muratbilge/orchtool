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

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SendPane {
    pub pane: String,
    #[serde(default)]
    pub text: String,
    /// Press Enter (CR) after the text, after a short pause so TUIs don't treat it as paste.
    #[serde(default)]
    pub enter: bool,
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
    },
    /// The client's role changed (attach, takeover by another client).
    Role { role: Role },
    /// The pane's process exited; the stream ends.
    Exit { code: Option<u32> },
}

/// The pane's id or its name.
pub fn pane_matches(info: &PaneInfo, key: &str) -> bool {
    info.id == key || info.name.as_deref() == Some(key)
}
