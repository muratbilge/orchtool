//! Read-only process-owned conversation discovery. Never choose a cwd's newest transcript.
use std::collections::{HashSet, VecDeque};
use std::path::{Path, PathBuf};

#[derive(Debug)]
pub struct AgentProcess {
    pub argv: Vec<String>,
    pub session_id: Option<String>,
}

pub fn uuid(s: &str) -> bool {
    s.len() == 36
        && s.bytes().enumerate().all(|(i, b)| {
            if [8, 13, 18, 23].contains(&i) {
                b == b'-'
            } else {
                b.is_ascii_hexdigit()
            }
        })
}

fn transcript_id(path: &Path, home: &Path, kind: &str) -> Option<String> {
    let root = home.join(match kind {
        "claude" => ".claude/projects",
        "codex" => ".codex/sessions",
        _ => return None,
    });
    let relative = path.strip_prefix(root).ok()?;
    if relative
        .components()
        .any(|c| matches!(c, std::path::Component::ParentDir))
    {
        return None;
    }
    let stem = path.file_name()?.to_str()?.strip_suffix(".jsonl")?;
    let id = if kind == "codex" {
        stem.strip_prefix("rollout-")?;
        stem.get(stem.len().checked_sub(36)?..)?
    } else {
        stem
    };
    uuid(id).then(|| id.to_string())
}

/// The nearest actual agent executable in a pane's process tree, including agents started
/// interactively inside a shell. Multiple candidates at that depth are ambiguous: fail closed.
pub fn discover(proc_root: &Path, root: u32, home: &Path, kind: &str) -> Option<AgentProcess> {
    let mut queue = VecDeque::from([(root, 0usize)]);
    let mut seen = HashSet::new();
    let mut matches = Vec::new();
    let mut found_depth = None;
    while let Some((pid, depth)) = queue.pop_front() {
        if found_depth.is_some_and(|d| depth > d) {
            break;
        }
        if !seen.insert(pid) || seen.len() > 1024 {
            continue;
        }
        let dir = proc_root.join(pid.to_string());
        let argv: Vec<String> = std::fs::read(dir.join("cmdline"))
            .ok()
            .unwrap_or_default()
            .split(|b| *b == 0)
            .filter(|b| !b.is_empty())
            .map(|b| String::from_utf8_lossy(b).into_owned())
            .collect();
        let named = |s: &str| Path::new(s).file_name().is_some_and(|n| n == kind);
        if argv.first().is_some_and(|s| named(s))
            || std::fs::read_link(dir.join("exe"))
                .ok()
                .is_some_and(|p| p.file_name().is_some_and(|n| n == kind))
        {
            found_depth = Some(depth);
            let mut ids = HashSet::new();
            if let Ok(fds) = std::fs::read_dir(dir.join("fd")) {
                for fd in fds.flatten() {
                    if let Some(id) = std::fs::read_link(fd.path())
                        .ok()
                        .and_then(|p| transcript_id(&p, home, kind))
                    {
                        ids.insert(id);
                    }
                }
            }
            matches.push(AgentProcess {
                argv,
                session_id: if ids.len() == 1 {
                    ids.into_iter().next()
                } else {
                    None
                },
            });
        } else if let Ok(children) =
            std::fs::read_to_string(dir.join(format!("task/{pid}/children")))
        {
            queue.extend(
                children
                    .split_whitespace()
                    .filter_map(|s| s.parse().ok())
                    .map(|p| (p, depth + 1)),
            );
        }
    }
    (matches.len() == 1).then(|| matches.remove(0))
}

/// Prove a hook belongs to this pane, not to the shared app-server's inherited environment.
/// Fail closed on inaccessible/exited processes, cycles, or a different process namespace.
pub fn descendant(proc_root: &Path, root: u32, mut child: u32) -> bool {
    let mut seen = HashSet::new();
    while child > 1 && seen.len() < 1024 && seen.insert(child) {
        if child == root {
            return true;
        }
        let Ok(stat) = std::fs::read_to_string(proc_root.join(child.to_string()).join("stat"))
        else {
            return false;
        };
        let Some((_, rest)) = stat.rsplit_once(')') else {
            return false;
        };
        let Some(parent) = rest.split_whitespace().nth(1).and_then(|p| p.parse().ok()) else {
            return false;
        };
        child = parent;
    }
    false
}

pub fn host_home() -> Option<PathBuf> {
    std::env::var_os("HOME").map(PathBuf::from)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;
    #[test]
    fn ancestry_rejects_shared_server_and_handles_process_names() {
        let root = std::env::temp_dir().join(format!("orch-ancestry-{}", std::process::id()));
        for (pid, parent) in [(100, 1), (101, 100), (102, 101), (200, 1), (201, 200)] {
            std::fs::create_dir_all(root.join(pid.to_string())).unwrap();
            std::fs::write(
                root.join(format!("{pid}/stat")),
                format!("{pid} (name with ) spaces) S {parent} 0"),
            )
            .unwrap();
        }
        assert!(descendant(&root, 100, 102));
        assert!(!descendant(&root, 100, 201));
        assert!(!descendant(&root, 100, 999));
        std::fs::write(root.join("101/stat"), "101 (cycle) S 102").unwrap();
        assert!(!descendant(&root, 100, 102));
        std::fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn two_agents_in_one_cwd_keep_their_own_open_transcripts() {
        let root = std::env::temp_dir().join(format!(
            "orch-discover-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let home = root.join("home");
        let proc = root.join("proc");
        let ids = [
            "11111111-1111-4111-8111-111111111111",
            "22222222-2222-4222-8222-222222222222",
        ];
        for (pid, id) in [(10, ids[0]), (20, ids[1])] {
            let d = proc.join(pid.to_string());
            std::fs::create_dir_all(d.join("fd")).unwrap();
            std::fs::write(d.join("cmdline"), b"claude\0--model\0sonnet\0").unwrap();
            let file = home.join(format!(".claude/projects/-same-cwd/{id}.jsonl"));
            std::fs::create_dir_all(file.parent().unwrap()).unwrap();
            std::fs::write(&file, "").unwrap();
            symlink(file, d.join("fd/8")).unwrap();
            assert_eq!(
                discover(&proc, pid, &home, "claude")
                    .unwrap()
                    .session_id
                    .as_deref(),
                Some(id)
            );
        }
        let shell = proc.join("1/task/1");
        std::fs::create_dir_all(&shell).unwrap();
        std::fs::write(shell.join("children"), "10").unwrap();
        assert_eq!(
            discover(&proc, 1, &home, "claude").unwrap().argv,
            vec!["claude", "--model", "sonnet"]
        );
        std::fs::write(shell.join("children"), "10 20").unwrap();
        assert!(discover(&proc, 1, &home, "claude").is_none());
        std::fs::remove_file(proc.join("10/fd/8")).unwrap();
        assert!(
            discover(&proc, 10, &home, "claude")
                .unwrap()
                .session_id
                .is_none(),
            "no newest-file fallback"
        );
        let path = home.join(format!(
            ".codex/sessions/2026/11/04/rollout-2026-11-04T12-00-00-{}.jsonl",
            ids[0]
        ));
        assert_eq!(
            transcript_id(&path, &home, "codex").as_deref(),
            Some(ids[0])
        );
        assert!(transcript_id(&path, &home, "claude").is_none());
        std::fs::remove_dir_all(root).unwrap();
    }
}
