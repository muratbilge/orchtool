//! The environment programs in panes should get. orch is often started from a bare,
//! non-login SSH command (Kabuk: `sh -c 'exec orch'`), whose PATH lacks ~/.local/bin and
//! whose profile variables (JAVA_HOME, ANDROID_HOME, …) are missing — and orchd, started by
//! that orch, inherits the same. So agents would not be found (`claude: not found`) or would
//! miss their tools. The user's login shell knows the real environment: ask it once and merge.

use std::collections::HashMap;
use std::io::Read;
use std::process::{Command, Stdio};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

/// Variables that belong to the session a command was started from (an agent's Claude Code,
/// a herdr / tmux / screen pane, an orch pane), never to a new pane or to orchd. A Claude Code
/// that inherits `CLAUDE_CODE_CHILD_SESSION` & co. takes itself for a sub-session of another
/// one (no transcript saving, wrong session id).
pub const SESSION_VARS: &[&str] = &[
    "ORCH_AGENT",
    "ORCH_PANE",
    "STY",
    "CLAUDECODE",
    "CLAUDE_CODE_ENTRYPOINT",
    "CLAUDE_CODE_CHILD_SESSION",
    "CLAUDE_CODE_SSE_PORT",
    "CLAUDE_CODE_SESSION_ID",
    "CLAUDE_CODE_SESSION_ATTENDED",
    "CLAUDE_CODE_EXECPATH",
    "CLAUDE_CODE_MESSAGING_SOCKET",
    "CLAUDE_CODE_MESSAGING_TOKEN",
    "CLAUDE_PID",
    "CLAUDE_EFFORT",
    "AI_AGENT",
];

pub fn is_session_var(k: &str) -> bool {
    k.starts_with("HERDR_") || k.starts_with("TMUX") || SESSION_VARS.contains(&k)
}

/// The login shell's environment (`$SHELL -lc 'env -0'`, 3 s at most), once per process.
fn login_vars() -> &'static Option<HashMap<String, String>> {
    static VARS: OnceLock<Option<HashMap<String, String>>> = OnceLock::new();
    VARS.get_or_init(|| {
        let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".into());
        let mut child = Command::new(&shell)
            .args(["-lc", "env -0"])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .ok()?;
        let mut out = child.stdout.take()?;
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let mut buf = Vec::new();
            let _ = out.by_ref().take(1024 * 1024).read_to_end(&mut buf);
            let _ = tx.send(buf);
        });
        let start = Instant::now();
        loop {
            match child.try_wait() {
                Ok(Some(_)) => break,
                Ok(None) if start.elapsed() < Duration::from_secs(3) => {
                    std::thread::sleep(Duration::from_millis(5))
                }
                _ => {
                    // A profile that hangs (prompts, slow mounts): do without it.
                    let _ = child.kill();
                    let _ = child.wait();
                    return None;
                }
            }
        }
        let buf = read_before_deadline(&rx, start, Duration::from_secs(3))?;
        Some(
            buf.split(|b| *b == 0)
                .filter_map(|kv| {
                    let kv = std::str::from_utf8(kv).ok()?;
                    let (k, v) = kv.split_once('=')?;
                    Some((k.to_string(), v.to_string()))
                })
                .collect(),
        )
    })
}

fn read_before_deadline(
    rx: &std::sync::mpsc::Receiver<Vec<u8>>,
    start: Instant,
    budget: Duration,
) -> Option<Vec<u8>> {
    rx.recv_timeout(budget.saturating_sub(start.elapsed())).ok()
}

/// `current` completed from the login environment: variables it lacks are added, and PATH
/// keeps its own entries first, then the login shell's that are missing. Variables that are
/// set stay as they are.
pub fn merge(
    current: Vec<(String, String)>,
    login: &HashMap<String, String>,
) -> Vec<(String, String)> {
    let mut out = current;
    let has = |out: &[(String, String)], k: &str| out.iter().any(|(ek, _)| ek == k);
    if let Some(lp) = login.get("PATH") {
        match out.iter_mut().find(|(k, _)| k == "PATH") {
            Some((_, p)) => {
                let mine: Vec<&str> = p.split(':').collect();
                let extra: Vec<&str> = lp
                    .split(':')
                    .filter(|e| !e.is_empty() && !mine.contains(e))
                    .collect();
                if !extra.is_empty() {
                    *p = format!("{p}:{}", extra.join(":"));
                }
            }
            None => out.push(("PATH".into(), lp.clone())),
        }
    }
    let mut keys: Vec<&String> = login.keys().collect();
    keys.sort();
    for k in keys {
        // Shell-internal and per-invocation values of the login shell are not ours.
        if matches!(k.as_str(), "PWD" | "OLDPWD" | "SHLVL" | "_")
            || is_session_var(k)
            || has(&out, k)
        {
            continue;
        }
        out.push((k.clone(), login[k].clone()));
    }
    out
}

/// This process's environment, completed from the login shell's (see [`merge`]).
pub fn full_env() -> Vec<(String, String)> {
    // Without the calling session's own variables (orchd may have been started from inside an
    // agent; its panes must not look like that agent's children).
    let current: Vec<(String, String)> = std::env::vars()
        .filter(|(k, _)| !is_session_var(k))
        .collect();
    match login_vars() {
        Some(login) => merge(current, login),
        None => current,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inherited_stdout_cannot_extend_the_deadline() {
        let (_writer_still_open, rx) = std::sync::mpsc::channel();
        let start = Instant::now();
        assert!(read_before_deadline(&rx, start, Duration::from_millis(20)).is_none());
        assert!(start.elapsed() < Duration::from_secs(1));
    }

    #[test]
    fn bare_path_gets_the_login_entries_and_set_values_stay() {
        let login: HashMap<String, String> = [
            ("PATH", "/home/u/.local/bin:/usr/bin"),
            ("JAVA_HOME", "/jdk"),
            ("LANG", "de_DE.UTF-8"),
            ("PWD", "/home/u"),
        ]
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
        let cur = vec![
            ("PATH".to_string(), "/usr/bin:/bin".to_string()),
            ("LANG".to_string(), "C.UTF-8".to_string()),
        ];
        let m: HashMap<_, _> = merge(cur, &login).into_iter().collect();
        assert_eq!(m["PATH"], "/usr/bin:/bin:/home/u/.local/bin");
        assert_eq!(m["JAVA_HOME"], "/jdk");
        assert_eq!(m["LANG"], "C.UTF-8", "set values are kept");
        assert!(!m.contains_key("PWD"));
    }

    #[test]
    fn agent_session_vars_never_reach_panes() {
        let login: HashMap<String, String> = [("CLAUDE_CODE_CHILD_SESSION", "1"), ("X", "y")]
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        let m: HashMap<_, _> = merge(Vec::new(), &login).into_iter().collect();
        assert!(!m.contains_key("CLAUDE_CODE_CHILD_SESSION"));
        assert_eq!(m["X"], "y");
        assert!(is_session_var("CLAUDECODE") && is_session_var("HERDR_PANE_ID"));
        assert!(!is_session_var("PATH"));
        assert!(is_session_var("ORCH_AGENT"));
        let login = HashMap::from([("ORCH_AGENT".into(), "parent".into())]);
        assert!(!merge(Vec::new(), &login)
            .iter()
            .any(|(k, _)| k == "ORCH_AGENT"));
    }
}
