//! orchd: owns the PTY panes, their terminal state and the attached clients.

pub mod bus;
pub mod detect;
pub mod limits;
pub mod pane;
pub mod render;
pub mod restore;
pub mod server;
pub mod term;
pub mod ui;
