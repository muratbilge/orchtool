//! Exit codes of `orch`: the only outcome an SSH exec channel carries to Kabuk, so every outcome
//! the phone acts on has its own code (see notes/kabuk-contract-v2.md). This is the one table;
//! the help text below and the contract note copy it. Changing a code here bumps the watch
//! line's `v` (watch.rs); adding one does not.
//!
//! | code | meaning |
//! |------|---------|
//! | 0    | done |
//! | 1    | any other error (message on stderr) |
//! | 2    | usage error (clap: unknown flag or subcommand — the CLI and the caller disagree) |
//! | 10   | `send --expect`: the pane's screen changed, nothing was sent |
//! | 11   | already answered: `send --expect` on a screen answered within 30 s, or `answer` on a question somebody answered first (stderr carries that answer) |
//! | 12   | the pane's program has exited, nothing was sent |
//! | 13   | the program is not reading its input, nothing was sent |
//! | 14   | no such pane, question, workspace, tab, task or task target, or the pane is not in a workspace |
//! | 20   | orchd unreachable: not running, did not start, or did not answer in time |
//!
//! 3 and 4 are never used: Kabuk's own `sh` wrapper takes them (no `orch` binary, no such folder).

use orch_proto::client::ClientError;
use std::fmt;
use std::io::ErrorKind;

pub const FAILURE: i32 = 1;
pub const CHANGED: i32 = 10;
pub const ANSWERED: i32 = 11;
pub const EXITED: i32 = 12;
pub const INPUT_FULL: i32 = 13;
pub const NOT_FOUND: i32 = 14;
pub const UNREACHABLE: i32 = 20;

/// Shown by `orch --help`.
pub const HELP: &str = "Exit codes:
  0   done
  1   other error (message on stderr)
  2   usage error (unknown flag or subcommand)
  10  send --expect: the pane's screen changed, nothing sent
  11  already answered: the screen (send --expect, 30 s) or the question (answer; stderr has the answer)
  12  the pane's program has exited, nothing sent
  13  the program is not reading its input, nothing sent
  14  no such pane, question, workspace, tab, task or task target, or the pane is not in a workspace
  20  orchd unreachable (not running, did not start, or did not answer in time)";

/// Outcomes decided on the client side that have their own exit code (orchd's own come back as
/// [`ClientError::Remote`] with a code).
#[derive(Debug)]
pub enum CliError {
    /// The best-effort herdr `send --expect`: the screen changed between the read and the send.
    Changed(String),
    /// `answer` lost first-answer-wins; the message names who answered and what.
    Answered(String),
    /// orchd could not be reached or did not answer.
    Unreachable(String),
    /// A workspace or tab named on the command line does not exist (any more).
    NotFound(String),
}

impl fmt::Display for CliError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CliError::Changed(m)
            | CliError::Answered(m)
            | CliError::Unreachable(m)
            | CliError::NotFound(m) => f.write_str(m),
        }
    }
}

impl std::error::Error for CliError {}

/// The exit code for a failed command. Looks through anyhow contexts for the typed cause.
pub fn code(e: &anyhow::Error) -> i32 {
    if let Some(c) = e.downcast_ref::<CliError>() {
        return match c {
            CliError::Changed(_) => CHANGED,
            CliError::Answered(_) => ANSWERED,
            CliError::Unreachable(_) => UNREACHABLE,
            CliError::NotFound(_) => NOT_FOUND,
        };
    }
    match e.downcast_ref::<ClientError>() {
        Some(ClientError::Remote(body)) => remote(&body.code),
        // The connection to orchd failed or broke: unreachable. Other I/O errors (a protocol
        // mismatch is InvalidData) are not.
        Some(ClientError::Io(io)) if is_unreachable(io.kind()) => UNREACHABLE,
        _ => FAILURE,
    }
}

/// orchd's error codes (orchd/src/server.rs, ui.rs) → exit codes.
fn remote(code: &str) -> i32 {
    match code {
        "changed" => CHANGED,
        "answered" => ANSWERED,
        "exited" => EXITED,
        "input_full" => INPUT_FULL,
        "no_task" | "no_target" | "no_pane" | "no_question" | "not_placed" | "no_workspace"
        | "no_tab" => NOT_FOUND,
        _ => FAILURE,
    }
}

fn is_unreachable(kind: ErrorKind) -> bool {
    matches!(
        kind,
        ErrorKind::NotFound
            | ErrorKind::ConnectionRefused
            | ErrorKind::ConnectionReset
            | ErrorKind::ConnectionAborted
            | ErrorKind::NotConnected
            | ErrorKind::BrokenPipe
            | ErrorKind::UnexpectedEof
            | ErrorKind::TimedOut
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use orch_proto::api::ErrorBody;

    fn remote_err(code: &str) -> anyhow::Error {
        // Wrapped in contexts the way `run()` adds them: the typed cause is still found.
        anyhow::Error::new(ClientError::Remote(ErrorBody {
            code: code.into(),
            message: "m".into(),
        }))
        .context("while sending")
        .context("outer")
    }

    fn io_err(kind: ErrorKind) -> anyhow::Error {
        anyhow::Error::new(ClientError::Io(std::io::Error::new(kind, "x"))).context("request")
    }

    #[test]
    fn every_outcome_has_its_code() {
        assert_eq!(code(&remote_err("changed")), 10);
        assert_eq!(code(&remote_err("answered")), 11);
        assert_eq!(code(&remote_err("exited")), 12);
        assert_eq!(code(&remote_err("input_full")), 13);
        assert_eq!(code(&remote_err("no_pane")), 14);
        assert_eq!(code(&remote_err("no_question")), 14);
        assert_eq!(code(&remote_err("not_placed")), 14);
        assert_eq!(code(&remote_err("bus")), 1);
        assert_eq!(code(&remote_err("bad_request")), 1);
        assert_eq!(code(&io_err(ErrorKind::ConnectionRefused)), 20);
        assert_eq!(code(&io_err(ErrorKind::NotFound)), 20);
        assert_eq!(code(&io_err(ErrorKind::UnexpectedEof)), 20);
        assert_eq!(code(&io_err(ErrorKind::TimedOut)), 20);
        assert_eq!(
            code(&io_err(ErrorKind::InvalidData)),
            1,
            "protocol mismatch is not 'down'"
        );
        let cli = |e: CliError| code(&anyhow::Error::new(e).context("ctx"));
        assert_eq!(cli(CliError::Changed("c".into())), 10);
        assert_eq!(cli(CliError::Answered("a".into())), 11);
        assert_eq!(cli(CliError::Unreachable("u".into())), 20);
        assert_eq!(code(&anyhow::anyhow!("plain")), 1);
        assert_eq!(code(&anyhow::anyhow!("plain").context("more")), 1);
        // 2 is clap's, 3 and 4 are Kabuk's: never produced here.
        for c in [
            CHANGED,
            ANSWERED,
            EXITED,
            INPUT_FULL,
            NOT_FOUND,
            UNREACHABLE,
        ] {
            assert!(c >= 10, "{c}");
        }
        assert!(HELP.contains("20  orchd unreachable"));
    }

    #[test]
    fn answered_message_keeps_the_existing_answer() {
        let e = anyhow::Error::new(CliError::Answered(
            "#7 was already answered by tui: ship it".into(),
        ));
        assert_eq!(format!("{e:#}"), "#7 was already answered by tui: ship it");
    }
}
