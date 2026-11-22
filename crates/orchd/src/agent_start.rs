//! Starting a Claude/Codex agent again without "the latest conversation": a manual wake, and
//! a restart while an agent of the same kind sleeps (main's restart is `restore::restart_spec`).
//!
//! Fail-safe: the agent is started only from a command that parses completely, every option
//! recognised and carried over, with its saved program word (argv[0]) unchanged. Never a bare
//! `claude`/`codex`, never a retry with only the program, never a dropped option. When the
//! command can't be rebuilt that way, the pane comes back as a plain shell in its folder that
//! prints a note (why, the saved command line, the verified conversation id): nothing of the
//! saved text is run.
use crate::restore::SavedPane;
use std::path::Path;

const SHELLS: [&str; 5] = ["bash", "zsh", "sh", "fish", "dash"];
/// Stands for "no verified conversation" while building an agent's resume command.
const PICK: &str = "\u{1}orch-pick";

/// What a pane runs: its argv and extra environment. `agent` is false when the agent is not
/// started (the pane is a shell showing the note).
#[derive(Debug, Clone, PartialEq)]
pub struct Start {
    pub argv: Vec<String>,
    pub env: Vec<(String, String)>,
    pub agent: bool,
}

/// Which conversation to name: `Verified` = the verified id, else the picker (a wake);
/// `Explicit` = the verified id, else a target written on the parsed command line, else the
/// picker (a restart beside a sleeper).
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Target {
    Verified,
    Explicit,
}

fn base(p: &str) -> &str {
    Path::new(p)
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or(p)
}

pub fn shell_pane(sp: &SavedPane) -> bool {
    sp.argv.is_empty() || SHELLS.contains(&base(&sp.argv[0]))
}

/// The pane's start: the rebuilt agent (inside its shell when it ran in one, so the pane stays
/// when the agent exits), or the note shell. Not a Claude/Codex pane: its saved argv.
pub fn start(sp: &SavedPane, target: Target) -> Start {
    let Some(kind) = sp
        .agent
        .as_deref()
        .filter(|a| matches!(*a, "claude" | "codex"))
    else {
        return Start {
            argv: sp.argv.clone(),
            env: Vec::new(),
            agent: true,
        };
    };
    let Some(argv) = rebuild(sp, kind, target) else {
        return Start {
            argv: note(sp, kind),
            env: Vec::new(),
            agent: false,
        };
    };
    if shell_pane(sp) {
        let shell = pane_shell(sp);
        let name = sp
            .bus_name
            .as_ref()
            .map(|n| format!("ORCH_AGENT={} ", shell_words::quote(n)))
            .unwrap_or_default();
        let script = format!(
            "{name}{}; exec {}",
            shell_words::join(&argv),
            shell_words::quote(&shell)
        );
        return Start {
            argv: vec![shell, "-lc".into(), script],
            env: Vec::new(),
            agent: true,
        };
    }
    Start {
        argv,
        env: sp
            .bus_name
            .iter()
            .map(|n| ("ORCH_AGENT".to_string(), n.clone()))
            .collect(),
        agent: true,
    }
}

/// The agent command, only when it parses completely: the discovered agent argv, else the
/// pane's own command, else (shell pane) its wrapper's first command when the wrapper sets
/// nothing but orch's own `ORCH_AGENT` before it. Its program word must be the agent's
/// (`claude`/`codex`, any folder) and is kept as written; the resume parser refuses any
/// option it does not know. Latest-conversation selectors become the picker.
pub fn rebuild(sp: &SavedPane, kind: &str, target: Target) -> Option<Vec<String>> {
    let parsed = match &sp.agent_argv {
        Some(a) => a.clone(),
        None if shell_pane(sp) => {
            if !plain_wrapper(&sp.argv) {
                return None;
            }
            crate::resume_argv::shell_agent(&sp.argv, kind)?
        }
        None => sp.argv.clone(),
    };
    if base(parsed.first()?) != kind {
        return None;
    }
    let discovered = match (&sp.session_id, target) {
        (Some(id), _) => Some(id.as_str()),
        (None, Target::Verified) => Some(PICK),
        (None, Target::Explicit) => None,
    };
    let mut argv = crate::resume_argv::rewrite(&parsed, kind, discovered)?;
    argv.retain(|a| a != PICK);
    Some(no_latest_selector(argv, kind))
}

/// A wrapper script (`<shell> -c '<script>'`) whose first command sets no variables other than
/// orch's own `ORCH_AGENT` (another assignment could change how the agent runs).
fn plain_wrapper(argv: &[String]) -> bool {
    let Some(at) = argv
        .iter()
        .position(|a| a.starts_with('-') && a.ends_with('c'))
    else {
        return false;
    };
    let Some(Ok(words)) = argv.get(at + 1).map(|s| shell_words::split(s)) else {
        return false;
    };
    words
        .iter()
        .take_while(|w| w.contains('='))
        .all(|w| w.starts_with("ORCH_AGENT="))
}

/// The latest-conversation selectors of a parsed agent command made into the picker: Claude
/// `-c`/`--continue` becomes `--resume` (no id: its picker), Codex `--last` goes (`resume`
/// without an id is its picker). The program (first word) is never touched.
pub fn no_latest_selector(argv: Vec<String>, kind: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::with_capacity(argv.len());
    for (i, a) in argv.into_iter().enumerate() {
        if i == 0 {
            out.push(a);
        } else if kind == "claude" && matches!(a.as_str(), "-c" | "--continue") {
            if !out.iter().skip(1).any(|o| o == "--resume") {
                out.push("--resume".into());
            }
        } else if !(kind == "codex" && a == "--last") {
            out.push(a);
        }
    }
    out
}

/// The shell of a shell pane; for a direct agent pane the user's login shell.
fn pane_shell(sp: &SavedPane) -> String {
    sp.argv
        .first()
        .filter(|_| shell_pane(sp))
        .cloned()
        .or_else(|| std::env::var("SHELL").ok().filter(|s| !s.is_empty()))
        .unwrap_or_else(|| "/bin/bash".into())
}

/// The lines of the note (also logged).
pub fn note_lines(sp: &SavedPane, kind: &str) -> Vec<String> {
    let mut lines = vec![
        format!(
            "orch: {kind} was not restarted: its start command could not be reconstructed safely."
        ),
        format!("Saved command: {}", shell_words::join(&sp.argv)),
    ];
    if let Some(a) = sp.agent_argv.as_ref().filter(|a| **a != sp.argv) {
        lines.push(format!("Agent command: {}", shell_words::join(a)));
    }
    if let Some(id) = &sp.session_id {
        lines.push(format!("Conversation id: {id}"));
    }
    lines
}

/// A plain shell in the pane's folder that first prints the note. Only the quoted note is
/// printed; nothing of the saved command runs.
fn note(sp: &SavedPane, kind: &str) -> Vec<String> {
    let shell = pane_shell(sp);
    let quoted: Vec<String> = note_lines(sp, kind)
        .iter()
        .map(|l| shell_words::quote(l).into_owned())
        .collect();
    let script = format!(
        "printf '%s\\n' {}; exec {}",
        quoted.join(" "),
        shell_words::quote(&shell)
    );
    vec![shell, "-lc".into(), script]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::restore::restart_spec;

    fn v(s: &[&str]) -> Vec<String> {
        s.iter().map(|s| s.to_string()).collect()
    }

    fn saved(argv: &[&str], agent: Option<&str>) -> SavedPane {
        SavedPane {
            argv: v(argv),
            cwd: "/tmp".into(),
            rows: 20,
            cols: 80,
            name: None,
            agent: agent.map(str::to_string),
            bus_name: None,
            was_working: false,
            session_id: None,
            agent_argv: None,
            sole_codex_in_cwd: false,
        }
    }

    fn id(mut p: SavedPane, s: &str) -> SavedPane {
        p.session_id = Some(s.into());
        p
    }

    /// The restart beside a sleeper. `main` is main's own argv (literals, checked against
    /// main's `restart_spec`, which the restart without sleepers still runs); `None` in the
    /// last column = not started, the note shell.
    #[test]
    fn a_restart_beside_a_sleeper_starts_only_a_complete_command() {
        let mut sole = saved(&["codex", "-c", "k=v"], Some("codex"));
        sole.sole_codex_in_cwd = true;
        let mut discovered = saved(&["bash", "--norc"], Some("codex"));
        discovered.agent_argv = Some(v(&["/opt/x/codex", "--sandbox", "read-only"]));
        type Case = (SavedPane, Vec<String>, Option<Vec<String>>);
        let cases: Vec<Case> = vec![
            (
                sole,
                v(&["codex", "resume", "--last", "-c", "k=v"]),
                Some(v(&["codex", "resume", "-c", "k=v"])),
            ),
            (
                saved(&["claude", "--model", "x"], Some("claude")),
                v(&["claude", "--continue", "--model", "x"]),
                Some(v(&["claude", "--resume", "--model", "x"])),
            ),
            (
                id(saved(&["claude"], Some("claude")), "abc"),
                v(&["claude", "--resume", "abc"]),
                Some(v(&["claude", "--resume", "abc"])),
            ),
            (
                saved(&["codex", "resume", "abc"], Some("codex")),
                v(&["codex", "resume", "abc"]),
                Some(v(&["codex", "resume", "abc"])),
            ),
            // Permission options are carried over, the program word as written.
            (
                saved(
                    &["/opt/x/codex", "--sandbox", "read-only", "--approve-for-me"],
                    Some("codex"),
                ),
                v(&[
                    "/opt/x/codex",
                    "resume",
                    "--sandbox",
                    "read-only",
                    "--approve-for-me",
                ]),
                Some(v(&[
                    "/opt/x/codex",
                    "resume",
                    "--sandbox",
                    "read-only",
                    "--approve-for-me",
                ])),
            ),
            (
                discovered,
                v(&[
                    "bash",
                    "-lc",
                    "/opt/x/codex resume --sandbox read-only; exec bash",
                ]),
                Some(v(&[
                    "bash",
                    "-lc",
                    "/opt/x/codex resume --sandbox read-only; exec bash",
                ])),
            ),
            (
                saved(&["bash", "-lc", "claude -c"], Some("claude")),
                v(&["bash", "-lc", "claude --continue; exec bash"]),
                Some(v(&["bash", "-lc", "claude --resume; exec bash"])),
            ),
            // An option the parser does not know: not started (no option is dropped).
            (
                saved(
                    &["codex", "--sandbox", "read-only", "--some-unknown-flag"],
                    Some("codex"),
                ),
                v(&["codex", "--sandbox", "read-only", "--some-unknown-flag"]),
                None,
            ),
            (
                id(saved(&["claude", "--unknown-flag"], Some("claude")), "sid"),
                v(&["claude", "--unknown-flag", "--continue"]),
                None,
            ),
            // Wrappers it can't read, or that set variables: not started.
            (
                saved(
                    &[
                        "bash",
                        "-lc",
                        "cd . && /opt/x/codex resume --last; exec bash",
                    ],
                    Some("codex"),
                ),
                v(&[
                    "bash",
                    "-lc",
                    "cd . && /opt/x/codex resume --last; exec bash",
                ]),
                None,
            ),
            (
                saved(
                    &[
                        "bash",
                        "-lc",
                        "choice=--last; codex resume \"$choice\"; exec bash",
                    ],
                    Some("codex"),
                ),
                v(&[
                    "bash",
                    "-lc",
                    "choice=--last; codex resume \"$choice\"; exec bash",
                ]),
                None,
            ),
            (
                saved(
                    &["bash", "-lc", "CODEX_HOME=/x codex; exec bash"],
                    Some("codex"),
                ),
                v(&["bash", "-lc", "codex resume; exec bash"]),
                None,
            ),
            (
                id(
                    saved(
                        &["bash", "-c", "cd . && claude --resume abc"],
                        Some("claude"),
                    ),
                    "sid",
                ),
                v(&["bash", "-lc", "claude --resume sid; exec bash"]),
                None,
            ),
            // A program that is not the agent's own word (e.g. a launcher): not started.
            (
                saved(&["node", "cli.js"], Some("claude")),
                v(&["node", "--continue"]),
                None,
            ),
            // Not an agent: as saved.
            (
                saved(&["bash", "--norc"], None),
                v(&["bash", "--norc"]),
                Some(v(&["bash", "--norc"])),
            ),
        ];
        for (pane, main, fresh) in cases {
            assert_eq!(restart_spec(&pane).0, main, "main: {:?}", pane.argv);
            let s = start(&pane, Target::Explicit);
            match fresh {
                Some(want) => {
                    assert!(s.agent, "{:?}", pane.argv);
                    assert_eq!(s.argv, want, "{:?}", pane.argv);
                }
                None => {
                    assert!(!s.agent, "{:?}", pane.argv);
                    let script = &s.argv[2];
                    assert!(script.starts_with("printf '%s\\n' "), "{script}");
                    assert!(script.contains("could not be reconstructed safely"));
                    assert!(script.contains("Saved command: "));
                    if let Some(i) = &pane.session_id {
                        assert!(script.contains(&format!("Conversation id: {i}")));
                    }
                    // Only printf and exec of the shell: the saved text is quoted, not run.
                    let words = shell_words::split(script).unwrap();
                    assert_eq!(words[0], "printf");
                    assert_eq!(words[words.len() - 2], "exec");
                }
            }
        }
    }

    /// The note shell: the pane's own shell for a shell pane, the login shell for a direct one;
    /// no ORCH_AGENT, and the saved line is shown shell-quoted.
    #[test]
    fn the_note_shows_the_saved_line_and_runs_nothing_of_it() {
        let mut p = saved(&["zsh", "-c", "cd . && codex"], Some("codex"));
        p.bus_name = Some("x-codex".into());
        let s = start(&p, Target::Verified);
        assert!(!s.agent && s.env.is_empty());
        assert_eq!(&s.argv[..2], &v(&["zsh", "-lc"])[..]);
        let mut words = shell_words::split(&s.argv[2]).unwrap();
        // The `;` after the last quoted line sticks to it when split into words.
        words[3] = words[3].trim_end_matches(';').to_string();
        assert_eq!(
            words,
            v(&[
                "printf",
                "%s\\n",
                "orch: codex was not restarted: its start command could not be reconstructed safely.",
                "Saved command: zsh -c 'cd . && codex'",
                "exec",
                "zsh",
            ])
        );
        let direct = saved(&["codex", "--nope"], Some("codex"));
        let s = start(&direct, Target::Verified);
        assert_eq!(s.argv[1], "-lc");
        assert!(SHELLS.contains(&base(&s.argv[0])) || std::env::var("SHELL").is_ok());
    }
}
