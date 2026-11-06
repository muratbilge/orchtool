//! orchd: owns the PTY panes, their terminal state and the attached clients.

pub mod bus;
pub mod conversation;
pub mod detect;
pub mod limits;
pub mod pane;
pub mod render;
pub mod restore;
mod resume_argv;
pub mod server;
pub mod term;
pub mod ui;

pub mod tasks;
mod tasks_store;
