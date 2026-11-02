//! Agent status from the screen: the fallback for agents without hooks (Codex) and the
//! last check before orchd types anything into an agent's pane.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Detected {
    /// Not an agent we know (a shell, vim, …).
    None,
    Working,
    /// Waiting for a decision: permission / approval / trust dialog, y/n prompt.
    Blocked,
    Idle,
    /// The program has exited.
    Done,
}

/// "claude" / "codex" from the pane's command line.
pub fn agent_kind(argv: &[String]) -> Option<&'static str> {
    let prog = argv.first().map(|a| {
        std::path::Path::new(a)
            .file_name()
            .map(|f| f.to_string_lossy().to_lowercase())
            .unwrap_or_default()
    })?;
    if prog.contains("claude") {
        Some("claude")
    } else if prog.contains("codex") {
        Some("codex")
    } else {
        None
    }
}

const BLOCKED: &[&str] = &[
    // Claude Code permission and trust dialogs
    "do you want to proceed?",
    "do you want to make this edit",
    "do you want to create",
    "do you want to allow",
    "enter to confirm · esc to cancel",
    "esc to cancel · tab to amend",
    // Codex approvals
    "enter to submit | esc to cancel",
    "allow the ",
    "would you like to run the following command",
    "approve this",
    // generic prompts
    "(y/n)",
    "[y/n]",
    "[y/n]?",
];

/// Classify the visible screen text of a pane running `kind`.
pub fn detect(screen: &str, kind: Option<&str>) -> Detected {
    let Some(_kind) = kind else {
        return Detected::None;
    };
    // Only the bottom of the screen matters (dialogs and status lines live there); scrolled-up
    // transcript text that merely quotes a prompt must not count.
    let lines: Vec<&str> = screen.lines().filter(|l| !l.trim().is_empty()).collect();
    let tail = lines[lines.len().saturating_sub(14)..]
        .join("\n")
        .to_lowercase();
    if BLOCKED.iter().any(|p| tail.contains(p)) {
        return Detected::Blocked;
    }
    if tail.contains("esc to interrupt") || tail.contains("ctrl+c to interrupt") {
        return Detected::Working;
    }
    Detected::Idle
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn real_screens() {
        let codex_working = "› fix it\n• Working (14s • esc to interrupt)\n\n› Ask Codex to do anything\n  GPT-6-Astra medium";
        assert_eq!(detect(codex_working, Some("codex")), Detected::Working);
        let codex_approval = "• Calling orch.read_inbox\n  Field 1/1\n  Allow the orch MCP server to run tool \"read_inbox\"?\n  › 1. Allow\n  enter to submit | esc to cancel";
        assert_eq!(detect(codex_approval, Some("codex")), Detected::Blocked);
        let codex_idle = "• OK\n  Worked for 3s • 20:55\n› Ask Codex to do anything\n  GPT-6-Astra medium · /tmp\n  ? for shortcuts";
        assert_eq!(detect(codex_idle, Some("codex")), Detected::Idle);
        let claude_trust = " Quick safety check: Is this a project you created or one you trust?\n ❯ No, exit\n   Yes, I trust this folder\n Enter to confirm · Esc to cancel";
        assert_eq!(detect(claude_trust, Some("claude")), Detected::Blocked);
        let claude_perm = "● Bash(rm -rf build)\n Do you want to proceed?\n ❯ 1. Yes\n   2. No";
        assert_eq!(detect(claude_perm, Some("claude")), Detected::Blocked);
        let claude_working =
            "✻ Thinking… (12s · esc to interrupt)\n────\n❯ \n────\n  ⏵⏵ auto mode on";
        assert_eq!(detect(claude_working, Some("claude")), Detected::Working);
        let claude_idle =
            "● Done.\n✻ Baked for 1m 27s\n────\n❯ \n────\n  ⏵⏵ auto mode on (shift+tab to cycle)";
        assert_eq!(detect(claude_idle, Some("claude")), Detected::Idle);
        assert_eq!(detect("$ ls\nfoo", None), Detected::None);
    }

    #[test]
    fn old_prompts_above_the_tail_do_not_count() {
        let mut s = String::from("Do you want to proceed?\n");
        for i in 0..30 {
            s.push_str(&format!("line {i}\n"));
        }
        s.push_str("❯ \n  ⏵⏵ auto mode on");
        assert_eq!(detect(&s, Some("claude")), Detected::Idle);
    }

    #[test]
    fn kinds_from_argv() {
        assert_eq!(agent_kind(&["/usr/bin/claude".into()]), Some("claude"));
        assert_eq!(agent_kind(&["codex".into(), "-c".into()]), Some("codex"));
        assert_eq!(agent_kind(&["bash".into()]), None);
    }
}
