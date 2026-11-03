//! Restart without losing the layout: on `daemon.stop` with `restart`, orchd writes its
//! workspaces / tabs / splits and what ran in each pane (program, current folder, size) to
//! `restore.json` next to its socket; the next orchd recreates them with fresh panes. Agents
//! continue their last conversation (`claude --continue`, `codex resume --last`).

use crate::server::Daemon;
use orch_proto::api::NewPane;
use orch_proto::layout::{Node, Session};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::Path;

#[derive(Debug, Serialize, Deserialize)]
pub struct Saved {
    pub session: Session,
    pub panes: HashMap<String, SavedPane>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SavedPane {
    pub argv: Vec<String>,
    pub cwd: String,
    pub rows: u16,
    pub cols: u16,
    #[serde(default)]
    pub name: Option<String>,
    /// The agent seen in the pane (claude / codex), also when it was started inside a shell.
    #[serde(default)]
    pub agent: Option<String>,
    /// Its name on the bus: the restarted agent takes it again (unread mail, open questions).
    #[serde(default)]
    pub bus_name: Option<String>,
}

const SHELLS: [&str; 5] = ["bash", "zsh", "sh", "fish", "dash"];

fn base(p: &str) -> &str {
    Path::new(p)
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or(p)
}

/// What to start for a saved pane, plus extra environment. An agent that ran inside a shell
/// comes back resumed inside that shell (and the shell stays when it exits); an agent started
/// directly is resumed directly. Either way it gets its old bus name.
pub fn restart_spec(sp: &SavedPane) -> (Vec<String>, Vec<(String, String)>) {
    let agent = sp
        .agent
        .as_deref()
        .filter(|a| matches!(*a, "claude" | "codex"));
    let shell_pane =
        sp.argv.is_empty() || sp.argv.first().is_some_and(|a| SHELLS.contains(&base(a)));
    let name_env = |v: &mut Vec<(String, String)>| {
        if let Some(n) = &sp.bus_name {
            v.push(("ORCH_AGENT".into(), n.clone()));
        }
    };
    match agent {
        // Already a resume of one particular conversation (`codex resume <id>`,
        // `claude --resume <id>`, as moved agents run): start exactly that again.
        Some(_)
            if shell_pane
                && sp.argv.iter().skip(1).any(|a| {
                    a.contains(" resume ") || a.contains("--resume ") || a.contains(" -r ")
                }) =>
        {
            (sp.argv.clone(), Vec::new())
        }
        Some(kind) if shell_pane => {
            let shell = sp
                .argv
                .first()
                .cloned()
                .or_else(|| std::env::var("SHELL").ok())
                .unwrap_or_else(|| "/bin/bash".into());
            let resume = restart_argv(&[kind.to_string()]).join(" ");
            // Only the agent gets the name, not every program started later in the shell.
            let name = sp
                .bus_name
                .as_deref()
                .map(|n| format!("ORCH_AGENT='{}' ", n.replace('\'', "")))
                .unwrap_or_default();
            let script = format!("{name}{resume}; exec '{}'", shell.replace('\'', ""));
            (vec![shell, "-lc".into(), script], Vec::new())
        }
        _ => {
            let mut env = Vec::new();
            if agent.is_some() {
                name_env(&mut env);
            }
            (restart_argv(&sp.argv), env)
        }
    }
}

/// What to run again: the same program; agents resume their last conversation.
pub fn restart_argv(argv: &[String]) -> Vec<String> {
    let Some(first) = argv.first() else {
        return Vec::new();
    };
    let base = Path::new(first)
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or_default();
    let rest = &argv[1..];
    let only_flags = rest.iter().all(|a| a.starts_with('-'));
    let has = |flags: &[&str]| rest.iter().any(|a| flags.contains(&a.as_str()));
    match base {
        "claude" if only_flags && !has(&["-c", "--continue", "-r", "--resume"]) => {
            let mut v = argv.to_vec();
            v.push("--continue".into());
            v
        }
        "codex" if only_flags => {
            let mut v = vec![first.clone(), "resume".into(), "--last".into()];
            v.extend(rest.iter().cloned());
            v
        }
        _ => argv.to_vec(),
    }
}

fn map_node(n: &Node, ids: &HashMap<String, String>) -> Node {
    match n {
        Node::Pane { pane } => Node::Pane {
            pane: ids.get(pane).cloned().unwrap_or_default(),
        },
        Node::Split { dir, ratio, a, b } => Node::Split {
            dir: *dir,
            ratio: *ratio,
            a: Box::new(map_node(a, ids)),
            b: Box::new(map_node(b, ids)),
        },
    }
}

impl Daemon {
    /// The layout and its panes as they are now.
    pub fn snapshot_layout(&self) -> Saved {
        let session = self.session.lock().unwrap().clone();
        let mut panes = HashMap::new();
        let agents = self.bus.agents().unwrap_or_default();
        for p in self.all_panes() {
            if p.exit_code().is_some() {
                continue;
            }
            // The folder the program is in now (a shell may have cd'ed), else where it started.
            let cwd = p
                .pid
                .and_then(|pid| std::fs::read_link(format!("/proc/{pid}/cwd")).ok())
                .map(|c| c.to_string_lossy().into_owned())
                .filter(|c| !c.ends_with(" (deleted)"))
                .unwrap_or_else(|| p.cwd.clone());
            let (rows, cols) = p.size();
            let bus_name = agents
                .iter()
                .find(|a| a.pane.as_deref() == Some(format!("orch:{}", p.id).as_str()))
                .map(|a| a.name.clone());
            panes.insert(
                p.id.clone(),
                SavedPane {
                    argv: p.argv.clone(),
                    cwd,
                    rows,
                    cols,
                    name: p.name.clone(),
                    agent: p.kind(),
                    bus_name,
                },
            );
        }
        Saved { session, panes }
    }

    pub fn save_layout(&self, path: &Path) -> std::io::Result<()> {
        use std::os::unix::fs::OpenOptionsExt;
        let data = serde_json::to_vec_pretty(&self.snapshot_layout())?;
        let tmp = path.with_extension("json.new");
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&tmp)?;
        std::io::Write::write_all(&mut f, &data)?;
        f.sync_all()?;
        std::fs::rename(&tmp, path)
    }

    /// Recreates a saved layout with new panes. Panes whose program can't start are left out
    /// (their tab / workspace collapses if nothing is left). Returns how many came back.
    pub fn restore_layout(&self, saved: Saved) -> usize {
        let mut ids: HashMap<String, String> = HashMap::new();
        let mut failed: Vec<String> = Vec::new();
        for ws in &saved.session.workspaces {
            for tab in &ws.tabs {
                for old in tab.root.panes() {
                    let Some(sp) = saved.panes.get(&old) else {
                        failed.push(old);
                        continue;
                    };
                    let (argv, extra) = restart_spec(sp);
                    // orchd's own environment, plus the agent's bus name.
                    let env = (!extra.is_empty()).then(|| {
                        orch_proto::env::full_env()
                            .into_iter()
                            .chain(extra.iter().cloned())
                            .collect::<Vec<_>>()
                    });
                    let req = |cwd: Option<String>| NewPane {
                        name: sp.name.clone(),
                        argv: argv.clone(),
                        cwd,
                        env: env.clone(),
                        rows: Some(sp.rows.max(2)),
                        cols: Some(sp.cols.max(2)),
                    };
                    // A folder that is gone: start in the home folder instead.
                    let pane = self
                        .new_pane(req(Some(sp.cwd.clone())))
                        .or_else(|_| self.new_pane(req(None)));
                    match pane {
                        Ok(p) => {
                            ids.insert(old, p.id.clone());
                        }
                        Err(e) => {
                            eprintln!("orchd: restore {old}: {}", e.message);
                            failed.push(old);
                        }
                    }
                }
            }
        }
        let mut session = saved.session;
        for ws in &mut session.workspaces {
            for tab in &mut ws.tabs {
                tab.root = map_node(&tab.root, &ids);
                tab.focus = ids.get(&tab.focus).cloned().unwrap_or_default();
            }
        }
        // Gaps of panes that didn't come back: removed like closed panes.
        while session.remove_pane("") {}
        for ws in &mut session.workspaces {
            for tab in &mut ws.tabs {
                if tab.focus.is_empty() || !tab.root.panes().contains(&tab.focus) {
                    tab.focus = tab.root.panes().first().cloned().unwrap_or_default();
                }
            }
        }
        session.raised = None;
        {
            let mut s = self.session.lock().unwrap();
            session.version = s.version + 1;
            *s = session;
        }
        let _ = self.ui_events.send(crate::ui::UiEvent::Session);
        let _ = self.ui_events.send(crate::ui::UiEvent::Panes);
        ids.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(a: &[&str]) -> Vec<String> {
        a.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn agents_resume_and_other_programs_run_as_before() {
        assert_eq!(restart_argv(&v(&["claude"])), v(&["claude", "--continue"]));
        assert_eq!(
            restart_argv(&v(&["/usr/bin/claude", "--model", "x"])),
            v(&["/usr/bin/claude", "--model", "x"]),
            "an option value is not a flag: left as it was"
        );
        assert_eq!(restart_argv(&v(&["claude", "-c"])), v(&["claude", "-c"]));
        assert_eq!(
            restart_argv(&v(&["codex", "--yolo"])),
            v(&["codex", "resume", "--last", "--yolo"])
        );
        assert_eq!(
            restart_argv(&v(&["codex", "fix the bug"])),
            v(&["codex", "fix the bug"])
        );
        assert_eq!(restart_argv(&v(&["bash"])), v(&["bash"]));
        assert!(restart_argv(&[]).is_empty());
    }

    fn saved(argv: &[&str], agent: Option<&str>, bus: Option<&str>) -> SavedPane {
        SavedPane {
            argv: v(argv),
            cwd: "/tmp".into(),
            rows: 20,
            cols: 80,
            name: None,
            agent: agent.map(str::to_string),
            bus_name: bus.map(str::to_string),
        }
    }

    #[test]
    fn agents_typed_into_a_shell_come_back_resumed_with_their_name() {
        let (argv, env) =
            restart_spec(&saved(&["/bin/bash"], Some("claude"), Some("kabuk-claude")));
        assert_eq!(argv[0], "/bin/bash");
        assert_eq!(argv[1], "-lc");
        assert_eq!(
            argv[2],
            "ORCH_AGENT='kabuk-claude' claude --continue; exec '/bin/bash'"
        );
        assert!(env.is_empty(), "the shell itself gets no agent name");

        let (argv, env) = restart_spec(&saved(&["codex"], Some("codex"), Some("x-codex")));
        assert_eq!(argv, v(&["codex", "resume", "--last"]));
        assert_eq!(env, vec![("ORCH_AGENT".to_string(), "x-codex".to_string())]);

        // A moved agent that resumes one conversation by id: kept exactly.
        let script = "codex resume 01a19cb9-ca22; exec bash";
        let (argv, _) = restart_spec(&saved(
            &["bash", "-lc", script],
            Some("codex"),
            Some("o-codex"),
        ));
        assert_eq!(argv, v(&["bash", "-lc", script]));
        let script = "claude --resume 6a43; exec bash";
        let (argv, _) = restart_spec(&saved(&["bash", "-lc", script], Some("claude"), None));
        assert_eq!(argv, v(&["bash", "-lc", script]));

        // A plain shell stays a plain shell.
        let (argv, env) = restart_spec(&saved(&["bash", "--norc"], None, None));
        assert_eq!(argv, v(&["bash", "--norc"]));
        assert!(env.is_empty());
    }
}
