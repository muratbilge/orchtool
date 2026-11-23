//! Starting a Claude/Codex agent again without "the latest conversation": a manual wake, and
//! a restart while an agent of the same kind sleeps (main's restart is `restore::restart_spec`).
//!
//! Fail-safe: the agent is started only from a command that parses completely, every option
//! recognised and carried over, with its saved program word (argv[0]) unchanged. Never a bare
//! `claude`/`codex`, never a retry with only the program, never a dropped option. When the
//! command can't be rebuilt that way, the pane comes back as a plain shell in its folder with a
//! note on its screen (why, the saved command line, the verified conversation id): nothing of
//! the saved text is run, and it is never in the pane's argv or environment (published in
//! pane lists and events), only on its screen.
use crate::restore::SavedPane;
use std::path::Path;

const SHELLS: [&str; 5] = ["bash", "zsh", "sh", "fish", "dash"];
/// Stands for "no verified conversation" while building an agent's resume command.
const PICK: &str = "\u{1}orch-pick";

/// What a pane runs: its argv and extra environment. `agent` is false when the agent is not
/// started (the pane is a shell showing the note); `refused` then says why (no command text:
/// it goes to logs and `orch ws wake`).
#[derive(Debug, Clone, PartialEq)]
pub struct Start {
    pub argv: Vec<String>,
    pub env: Vec<(String, String)>,
    pub agent: bool,
    pub refused: Option<&'static str>,
    /// Refused: the note to show on the shell's screen (`Pane::spawn` writes it before the
    /// PTY reader starts). It holds the saved command, so it is never part of the argv or
    /// environment (both are published).
    pub note: Option<String>,
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
            refused: None,
            note: None,
        };
    };
    let argv = match rebuild(sp, kind, target) {
        Ok(argv) => argv,
        Err(why) => {
            return Start {
                argv: vec![pane_shell(sp)],
                env: Vec::new(),
                agent: false,
                refused: Some(why),
                note: Some(note_lines(sp, kind, why).join("\n") + "\n"),
            }
        }
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
            refused: None,
            note: None,
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
        refused: None,
        note: None,
    }
}

/// The agent command, only when it parses completely: the discovered agent argv, else the
/// pane's own command, else (shell pane) its wrapper's first command. A shell pane's own
/// command line must set nothing (see [`wrapper_sets_nothing`]), also when the agent's argv
/// was discovered. The program word must be the agent's (`claude`/`codex`), a bare name or an
/// absolute path (a relative path would be resolved from the pane's folder, not necessarily
/// the one it ran from), kept as written; the resume parser refuses any option it does not
/// know. Latest-conversation selectors become the picker. The error is the reason, no text of
/// the command.
pub fn rebuild(sp: &SavedPane, kind: &str, target: Target) -> Result<Vec<String>, &'static str> {
    if shell_pane(sp) && !wrapper_sets_nothing(&sp.argv, kind) {
        return Err("its shell command line sets variables or can't be read");
    }
    let parsed = match &sp.agent_argv {
        Some(a) => a.clone(),
        None if shell_pane(sp) => crate::resume_argv::shell_agent(&sp.argv, kind)
            .ok_or("its agent command is not known")?,
        None => sp.argv.clone(),
    };
    let exe = parsed.first().ok_or("its agent command is not known")?;
    if base(exe) != kind {
        return Err("its program is not claude/codex by name");
    }
    if exe.contains('/') && !exe.starts_with('/') {
        return Err("its program is a relative path");
    }
    let discovered = match (&sp.session_id, target) {
        (Some(id), _) => Some(id.as_str()),
        (None, Target::Verified) => Some(PICK),
        (None, Target::Explicit) => None,
    };
    let mut argv = crate::resume_argv::rewrite(&parsed, kind, discovered)
        .ok_or("an option of its command is not recognised")?;
    argv.retain(|a| a != PICK);
    Ok(no_latest_selector(argv, kind))
}

/// A shell pane's command line sets no variable that could change how the agent runs: a plain
/// shell (no `-c` script), or a script that can be read and whose first command is the agent
/// itself with no assignment other than orch's own `ORCH_AGENT`. Anything else (`cd … &&`,
/// `export …;`, `env X=… codex`, `$…`, a `-c` without a script) counts as not known.
fn wrapper_sets_nothing(argv: &[String], kind: &str) -> bool {
    let script_flag = |a: &String| a.starts_with('-') && !a.starts_with("--") && a.contains('c');
    let Some(at) = argv.iter().skip(1).position(script_flag).map(|i| i + 1) else {
        return true;
    };
    let Some(Ok(words)) = argv.get(at + 1).map(|s| shell_words::split(s)) else {
        return false;
    };
    crate::resume_argv::shell_agent(argv, kind).is_some()
        && words
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

/// The lines of the note, shown only in the pane (the saved command can carry credentials:
/// logs and `orch ws wake` get the reason only).
pub fn note_lines(sp: &SavedPane, kind: &str, why: &str) -> Vec<String> {
    let mut lines = vec![
        format!(
            "orch: {kind} was not restarted: its start command could not be reconstructed safely ({why})."
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
                    let why = s.refused.expect("a reason");
                    // Only the shell runs; the note (with the saved command) is screen text.
                    assert_eq!(s.argv.len(), 1, "{:?}", s.argv);
                    assert!(s.env.is_empty());
                    let note = s.note.expect("a note");
                    assert!(note.contains(why));
                    assert!(
                        note.contains(&format!("Saved command: {}", shell_words::join(&pane.argv)))
                    );
                    if let Some(i) = &pane.session_id {
                        assert!(note.contains(&format!("Conversation id: {i}")));
                    }
                }
            }
        }
    }

    /// Tenth check 1 and 2: a shell command line that sets a variable (or can't be read) gives
    /// the note also when the agent's argv was discovered; a relative program path gives the
    /// note; a bare name or an absolute path starts.
    #[test]
    fn wrapper_variables_and_relative_programs_are_refused() {
        let with = |argv: &[&str], agent: &[&str]| {
            let mut p = saved(argv, Some("codex"));
            p.agent_argv = Some(v(agent));
            start(&p, Target::Verified)
        };
        for (argv, agent) in [
            (
                &["bash", "-lc", "CODEX_HOME=/x codex; exec bash"][..],
                &["/opt/x/codex"][..],
            ),
            (
                &["bash", "-lc", "export CODEX_HOME=/x; codex"][..],
                &["codex"][..],
            ),
            (
                &["bash", "-lc", "env CODEX_HOME=/x codex"][..],
                &["codex"][..],
            ),
            (&["bash", "-lc", "cd p && codex"][..], &["codex"][..]),
            (&["bash", "-lc", "X=$(cat f) codex"][..], &["codex"][..]),
            (&["bash", "-c"][..], &["codex"][..]),
            (&["bash", "-lc", "'unclosed"][..], &["codex"][..]),
            // Relative program paths.
            (&["bash", "--norc"][..], &["./bin/codex"][..]),
            (&["bash", "--norc"][..], &["bin/codex"][..]),
        ] {
            let s = with(argv, agent);
            assert!(!s.agent, "{argv:?} {agent:?}");
        }
        // Main's own restart (no sleeper) has both behaviours today: the wrapper's variable is
        // dropped, and a relative program is run again as written from the pane's folder.
        let mut env = saved(
            &["bash", "-lc", "CODEX_HOME=/x codex; exec bash"],
            Some("codex"),
        );
        env.agent_argv = Some(v(&["/opt/x/codex"]));
        assert_eq!(
            restart_spec(&env).0,
            v(&["bash", "-lc", "/opt/x/codex resume; exec bash"])
        );
        let mut rel = saved(&["bash", "--norc"], Some("codex"));
        rel.agent_argv = Some(v(&["bin/codex", "--approve-for-me"]));
        assert_eq!(
            restart_spec(&rel).0,
            v(&[
                "bash",
                "-lc",
                "bin/codex resume --approve-for-me; exec bash"
            ])
        );
        assert_eq!(
            restart_spec(&saved(&["./bin/codex"], Some("codex"))).0,
            v(&["./bin/codex", "resume"])
        );
        let s = start(&saved(&["./bin/codex"], Some("codex")), Target::Verified);
        assert_eq!(s.refused, Some("its program is a relative path"));
        // Allowed: a plain shell, orch's own ORCH_AGENT, a bare name, an absolute path.
        for (argv, agent) in [
            (
                &["bash", "--norc"][..],
                &["codex", "--sandbox", "read-only"][..],
            ),
            (
                &["bash", "-lc", "ORCH_AGENT=x codex; exec bash"][..],
                &["/opt/x/codex"][..],
            ),
        ] {
            let s = with(argv, agent);
            assert!(s.agent, "{argv:?} {agent:?}");
        }
        assert!(start(&saved(&["/opt/x/codex"], Some("codex")), Target::Verified).agent);
    }

    /// Twelfth check: the note, with any saved text, is never in the argv or environment
    /// (published in pane lists and events): the pane runs only its shell.
    #[test]
    fn the_note_is_never_in_the_argv_or_environment() {
        let nasty = "cd . && codex 'it'\\''s' \\x \"$(touch x)\" `touch y`\nMARK-1; exec fish";
        let mut p = saved(&["fish", "-c", nasty], Some("codex"));
        p.agent_argv = Some(v(&["/opt/codex", "--nope", "MARK-2"]));
        p.session_id = Some("sid-1".into());
        p.bus_name = Some("x-codex".into());
        let s = start(&p, Target::Verified);
        assert!(!s.agent);
        assert_eq!(s.argv, v(&["fish"]), "the pane's own shell, nothing else");
        assert!(s.env.is_empty());
        let note = s.note.unwrap();
        for want in ["MARK-1", "MARK-2", "Conversation id: sid-1", "$(touch x)"] {
            assert!(note.contains(want), "{want} in {note}");
        }
        // A direct agent pane continues in the login shell.
        let s = start(
            &saved(&["codex", "--nope"], Some("codex")),
            Target::Verified,
        );
        assert_eq!(s.argv.len(), 1);
        assert!(!s.argv[0].contains("--nope"));
    }
}
