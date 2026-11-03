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
            panes.insert(
                p.id.clone(),
                SavedPane {
                    argv: p.argv.clone(),
                    cwd,
                    rows,
                    cols,
                    name: p.name.clone(),
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
                    let req = |cwd: Option<String>| NewPane {
                        name: sp.name.clone(),
                        argv: restart_argv(&sp.argv),
                        cwd,
                        env: None,
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
}
