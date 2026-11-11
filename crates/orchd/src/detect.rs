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
    /// The agent's own input prompt is on screen (positive match), nothing else going on.
    Idle,
    /// An agent, but the screen matches neither working, blocked nor its idle prompt.
    Unknown,
    /// The program has exited.
    Done,
    /// Stopped on a usage limit ("hit your usage limit … try again at 5:03 AM"): waiting for
    /// the reset (orchd types "continue" then, see limits.rs).
    Limited,
}

/// "claude" / "codex" from the pane's command line — also when a shell runs it
/// (`bash -lc 'claude --continue; exec bash'`, as restored and phone-started agents do).
pub fn agent_kind(argv: &[String]) -> Option<&'static str> {
    let base = |a: &str| {
        std::path::Path::new(a)
            .file_name()
            .map(|f| f.to_string_lossy().to_lowercase())
            .unwrap_or_default()
    };
    let named = |prog: &str| {
        if prog.contains("claude") {
            Some("claude")
        } else if prog.contains("codex") {
            Some("codex")
        } else {
            None
        }
    };
    let prog = base(argv.first()?);
    if let Some(k) = named(&prog) {
        return Some(k);
    }
    // A shell with a script: the first command of the script (after VAR=value words).
    if matches!(prog.as_str(), "bash" | "sh" | "zsh" | "dash" | "fish") {
        let i = argv
            .iter()
            .position(|a| a.starts_with('-') && a.ends_with('c'))?;
        let script = argv.get(i + 1)?;
        let first = script
            .split_whitespace()
            .find(|w| !w.contains('=') || w.starts_with('-'))?;
        return named(&base(first.trim_matches(|c| c == '\'' || c == '"')));
    }
    None
}

/// The agent from its screen, for a pane whose command doesn't say (an agent typed into a
/// shell): the input-prompt footers are specific enough.
pub fn screen_kind(screen: &str) -> Option<&'static str> {
    let tail = tail_lines(screen, 8).to_lowercase();
    if tail.contains("ask codex") || tail.contains("gpt-") && tail.contains("for shortcuts") {
        Some("codex")
    } else if tail.contains("⏵⏵") || tail.contains("shift+tab to cycle") {
        Some("claude")
    } else {
        None
    }
}

fn tail_lines(screen: &str, n: usize) -> String {
    let lines: Vec<&str> = screen.lines().filter(|l| !l.trim().is_empty()).collect();
    lines[lines.len().saturating_sub(n)..].join("\n")
}

/// A usage-limit notice in these words (Codex, Claude Code).
const LIMIT: &[&str] = &[
    "hit your usage limit",
    "usage limit reached",
    "hit your limit",
    "limit reached",
    "limit will reset",
];

/// The screen ends with a usage-limit notice (nothing has run since): the agent waits for its
/// reset. Returns the reset time as written ("5:03 AM", "Oct 28th, 2026 3:17 AM", "3am"), or
/// an empty string when the notice gives none.
pub fn limit_notice(screen: &str) -> Option<String> {
    let lines: Vec<&str> = screen.lines().filter(|l| !l.trim().is_empty()).collect();
    let tail = &lines[lines.len().saturating_sub(14)..];
    let at = tail
        .iter()
        .rposition(|l| LIMIT.iter().any(|p| l.to_lowercase().contains(p)))?;
    // Something ran after the notice: not waiting any more.
    let after = tail[at + 1..].join("\n").to_lowercase();
    if after.contains("esc to interrupt") || after.contains("working (") || after.contains("• ran ")
    {
        return None;
    }
    // The notice may wrap: its line plus the next two.
    let text = tail[at..(at + 3).min(tail.len())].join(" ");
    Some(reset_words(&text).unwrap_or_default())
}

/// "try again at X." / "resets X" / "reset at X" → "X".
fn reset_words(text: &str) -> Option<String> {
    let lower = text.to_lowercase();
    let (i, n) = ["try again at ", "resets at ", "reset at ", "resets "]
        .iter()
        .find_map(|k| lower.find(k).map(|i| (i, k.len())))?;
    let rest = &text[i + n..];
    // Up to the sentence end or the bracketed timezone.
    let end = rest
        .char_indices()
        .find(|(j, c)| {
            *c == '('
                || *c == '∙'
                || *c == '·'
                || *c == '\n'
                || *c == '.' && !rest[j + 1..].starts_with(char::is_numeric)
        })
        .map_or(rest.len(), |(j, _)| j);
    // Only the words of a time (month, day, year, hour, am/pm): the next screen line is joined on.
    let timeish = |t: &str| {
        let t = t.trim_end_matches(',').to_lowercase();
        let digits = t.trim_end_matches(|c: char| c.is_ascii_alphabetic());
        let suffix = &t[digits.len()..];
        (!digits.is_empty()
            && digits.chars().all(|c| c.is_ascii_digit() || c == ':')
            && matches!(suffix, "" | "am" | "pm" | "st" | "nd" | "rd" | "th"))
            || matches!(t.as_str(), "am" | "pm")
            || t.len() >= 3
                && [
                    "jan", "feb", "mar", "apr", "may", "jun", "jul", "aug", "sep", "oct", "nov",
                    "dec",
                ]
                .iter()
                .any(|m| t.starts_with(m) && t.chars().all(|c| c.is_ascii_alphabetic()))
    };
    let w = rest[..end]
        .split_whitespace()
        .take_while(|t| timeish(t))
        .collect::<Vec<_>>()
        .join(" ");
    (!w.is_empty()).then_some(w)
}

/// The next moment the reset time `when` (local time, as an agent writes it) names, after
/// `now` minus 12 h (a time of day already past by more than that is tomorrow's). Unix secs.
pub fn reset_unix(when: &str, now: i64) -> Option<i64> {
    let w = when.to_lowercase().replace(',', " ");
    let mut month: Option<i32> = None;
    let mut day: Option<i32> = None;
    let mut year: Option<i32> = None;
    let (mut hour, mut minute): (Option<i32>, i32) = (None, 0);
    let mut pm: Option<bool> = None;
    const MONTHS: [&str; 12] = [
        "jan", "feb", "mar", "apr", "may", "jun", "jul", "aug", "sep", "oct", "nov", "dec",
    ];
    for tok in w.split_whitespace() {
        if let Some(m) = MONTHS.iter().position(|m| tok.starts_with(m)) {
            month = Some(m as i32);
            continue;
        }
        if tok == "am" || tok == "pm" {
            pm = Some(tok == "pm");
            continue;
        }
        let (core, suffix) = tok.split_at(
            tok.find(|c: char| !c.is_ascii_digit() && c != ':')
                .unwrap_or(tok.len()),
        );
        if core.is_empty() {
            continue;
        }
        match suffix {
            "am" | "pm" => pm = Some(suffix == "pm"),
            "st" | "nd" | "rd" | "th" => {
                day = core.parse().ok();
                continue;
            }
            _ => {}
        }
        if let Some((h, m)) = core.split_once(':') {
            hour = h.parse().ok();
            minute = m.parse().ok()?;
        } else if suffix == "am" || suffix == "pm" {
            hour = core.parse().ok();
        } else if let Ok(n) = core.parse::<i32>() {
            if n > 1900 {
                year = Some(n);
            } else if month.is_some() && day.is_none() {
                day = Some(n);
            }
        }
    }
    let mut hour = hour?;
    match pm {
        Some(true) if hour < 12 => hour += 12,
        Some(false) if hour == 12 => hour = 0,
        _ => {}
    }
    if !(0..24).contains(&hour) || !(0..60).contains(&minute) {
        return None;
    }
    // SAFETY: plain libc time conversion on stack values.
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    let t = now as libc::time_t;
    unsafe { libc::localtime_r(&t, &mut tm) };
    if let Some(m) = month {
        tm.tm_mon = m;
    }
    if let Some(d) = day {
        tm.tm_mday = d;
    }
    if let Some(y) = year {
        tm.tm_year = y - 1900;
    }
    tm.tm_hour = hour;
    tm.tm_min = minute;
    tm.tm_sec = 0;
    tm.tm_isdst = -1;
    let mut at = unsafe { libc::mktime(&mut tm) } as i64;
    if at < 0 {
        return None;
    }
    if month.is_none() && day.is_none() && at < now - 12 * 3600 {
        at += 24 * 3600;
    }
    Some(at)
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

/// Codex's active rate-limit model menu, not a quoted menu above an ordinary prompt.
/// Require option 2 to mean exactly "keep current model" before automatically sending 2.
pub fn model_switch_question(screen: &str, kind: Option<&str>) -> bool {
    if kind != Some("codex") {
        return false;
    }
    let tail = tail_lines(screen, 10).to_lowercase();
    let footer = tail.lines().last().unwrap_or_default();
    tail.contains("approaching rate limits")
        && tail.contains("switch to ")
        && tail
            .lines()
            .any(|l| l.trim_start_matches([' ', '›', '❯']).trim_end() == "2. keep current model")
        && footer.contains("enter")
        && footer.contains("select")
        && footer.contains("esc")
        && footer.contains("back")
}

/// Codex's start-up "Update available" menu (it comes before the session, so also before a
/// restored `codex resume`). Must be the active bottom menu with "2. Skip" and the
/// "esc skip" footer before orchd presses Escape. The pane's agent may not be known yet
/// (Codex typed into a shell has drawn nothing else), so only another known agent rules it out.
pub fn codex_update_question(screen: &str, kind: Option<&str>) -> bool {
    if kind.is_some_and(|k| k != "codex") {
        return false;
    }
    let tail = tail_lines(screen, 10).to_lowercase();
    let footer = tail.lines().last().unwrap_or_default();
    let option = |text: &str| {
        tail.lines()
            .any(|l| l.trim_start_matches([' ', '›', '❯']).trim_end() == text)
    };
    tail.contains("update available")
        // The command after "Update now" names Codex, possibly on its wrapped next line.
        && tail.lines().any(|l| l.contains("1. update now"))
        && tail.contains("codex")
        && option("2. skip")
        && option("3. skip until next version")
        && footer.contains("enter")
        && footer.contains("continue")
        && footer.contains("esc")
        && footer.contains("skip")
}

/// Classify the visible screen text of a pane running `kind`.
pub fn detect(screen: &str, kind: Option<&str>) -> Detected {
    let Some(_kind) = kind else {
        return Detected::None;
    };
    if model_switch_question(screen, kind) || codex_update_question(screen, kind) {
        return Detected::Blocked;
    }
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
    if limit_notice(screen).is_some() {
        return Detected::Limited;
    }
    // Idle only on a positive sign of the agent's own prompt footer — never "by elimination"
    // (an unrecognised dialog or a shell after the agent exited must not count as idle).
    if IDLE.iter().any(|p| tail.contains(p)) {
        return Detected::Idle;
    }
    Detected::Unknown
}

/// Footers both agents show only while waiting at their input prompt.
const IDLE: &[&str] = &[
    "for shortcuts",      // Claude "? for shortcuts", Codex "? for shortcuts"
    "shift+tab to cycle", // Claude mode line
    "⏵⏵",                 // Claude mode indicator
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn update_menu_must_be_codex_and_the_active_bottom_menu() {
        // As Codex 0.166.0 drew it in a 70-column pane (the command wraps).
        let menu = "  Update available · 0.166.0 → 0.167.1\n  Release notes: https://github.com/openai/codex/releases/latest\n\n› 1. Update now (runs `sh -c 's=$(curl -fsSL https://chatgpt.com/\n     codex/install.sh) && CODEX_NON_INTERACTIVE=1 eval \"$s\"'`)\n  2. Skip\n  3. Skip until next version\n\n  enter continue · esc skip\n";
        assert!(codex_update_question(menu, Some("codex")));
        assert!(codex_update_question(menu, None), "agent not known yet");
        assert_eq!(detect(menu, Some("codex")), Detected::Blocked);
        assert!(!codex_update_question(menu, Some("claude")));
        // The npm variant names the package on the same line.
        let npm = menu.replace(
            "› 1. Update now (runs `sh -c 's=$(curl -fsSL https://chatgpt.com/\n     codex/install.sh) && CODEX_NON_INTERACTIVE=1 eval \"$s\"'`)",
            "› 1. Update now (runs `npm install -g @openai/codex`)",
        );
        assert!(codex_update_question(&npm, Some("codex")));
        // Answered or quoted further up: not the active menu.
        assert!(!codex_update_question(
            &format!("{menu}› Ask Codex\n? for shortcuts"),
            Some("codex")
        ));
        assert!(!codex_update_question(
            &format!("{menu}{}$ ", "old output\n".repeat(20)),
            None
        ));
        // Another program's update menu, or a changed meaning of Escape / option 2.
        assert!(!codex_update_question(
            &menu.replace("codex", "other").replace("CODEX", "OTHER"),
            None
        ));
        assert!(!codex_update_question(
            &menu.replace("2. Skip\n", "2. Update later\n"),
            Some("codex")
        ));
        assert!(!codex_update_question(
            &menu.replace("esc skip", "esc update"),
            Some("codex")
        ));
    }

    #[test]
    fn model_menu_is_codex_only_and_must_be_the_active_bottom_menu() {
        let menu = "Approaching rate limits\nSwitch to a-future-model for lower credit usage?\n› 1. Switch to a-future-model\n  2. Keep current model\n  3. Keep current model (never show again)\nenter select · esc back";
        assert!(model_switch_question(menu, Some("codex")));
        assert_eq!(detect(menu, Some("codex")), Detected::Blocked);
        assert!(!model_switch_question(menu, Some("claude")));
        assert!(!model_switch_question(menu, None));
        assert!(!model_switch_question(
            &format!("{menu}\n› Ask Codex\n? for shortcuts"),
            Some("codex")
        ));
        assert!(!model_switch_question(
            &format!("{menu}\n{}› Ask Codex", "old transcript\n".repeat(20)),
            Some("codex")
        ));
        assert!(!model_switch_question(
            &menu.replace("2. Keep current model", "2. Switch anyway"),
            Some("codex")
        ));
    }

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
        // An agent pane showing something unrecognised is NOT idle.
        assert_eq!(detect("murat@devbox:~$ ", Some("codex")), Detected::Unknown);
        assert_eq!(
            detect(
                "Some new kind of dialog\n[ OK ]  [ Cancel ]",
                Some("claude")
            ),
            Detected::Unknown
        );
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

    fn v(a: &[&str]) -> Vec<String> {
        a.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn agents_run_by_a_shell_and_agents_seen_on_screen() {
        assert_eq!(
            agent_kind(&v(&["/bin/bash", "-lc", "claude --continue; exec bash"])),
            Some("claude")
        );
        assert_eq!(
            agent_kind(&v(&[
                "bash",
                "-lc",
                "ORCH_AGENT='x-codex' codex resume --last; exec '/bin/bash'"
            ])),
            Some("codex")
        );
        assert_eq!(
            agent_kind(&v(&["bash", "-lc", "ls; claude"])),
            None,
            "only the first command"
        );
        assert_eq!(agent_kind(&v(&["bash", "--norc"])), None);
        assert_eq!(
            screen_kind("x\n› Ask Codex to do anything\n  GPT-6-Astra medium · /p\n"),
            Some("codex")
        );
        assert_eq!(
            screen_kind("❯ \n  ⏵⏵ auto mode on (shift+tab to cycle)\n"),
            Some("claude")
        );
        assert_eq!(screen_kind("$ ls\nfoo\n$ "), None);
    }

    #[test]
    fn usage_limits_and_their_reset_times() {
        let codex = "• Ran cargo test\n\n■ You’ve hit your usage limit. Upgrade to Pro (https://chatgpt.com/explore/pro), visit https://chatgpt.com/settings/\nusage to purchase more credits or try again at 5:03 AM.\n\n› Ask Codex to do anything\n\n  GPT-6-Astra medium · /projects/x\n";
        assert_eq!(limit_notice(codex).as_deref(), Some("5:03 AM"));
        assert_eq!(detect(codex, Some("codex")), Detected::Limited);
        let dated =
            "■ You've hit your usage limit. Try again at Oct 28th, 2026 3:17 AM.\n› Ask Codex\n";
        assert_eq!(
            limit_notice(dated).as_deref(),
            Some("Oct 28th, 2026 3:17 AM")
        );
        let claude = "  ⎿  Claude usage limit reached. Your limit will reset at 5pm (Europe/Berlin).\n\n❯ \n  ⏵⏵ auto mode on\n";
        assert_eq!(limit_notice(claude).as_deref(), Some("5pm"));
        assert_eq!(detect(claude, Some("claude")), Detected::Limited);
        let claude2 = "  ⎿  5-hour limit reached ∙ resets 3am\n❯ \n";
        assert_eq!(limit_notice(claude2).as_deref(), Some("3am"));
        // Work happened after the notice: no longer limited.
        let resumed = "■ You’ve hit your usage limit. try again at 5:03 AM.\n› continue\n• Ran ls\n• Working (3s • esc to interrupt)\n";
        assert_eq!(limit_notice(resumed), None);
        assert_eq!(detect(resumed, Some("codex")), Detected::Working);
        // A limit notice scrolled far above the bottom does not count.
        let old = format!(
            "■ You’ve hit your usage limit. try again at 5:03 AM.\n{}› Ask Codex\n",
            "line\n".repeat(20)
        );
        assert_eq!(limit_notice(&old), None);
    }

    #[test]
    fn reset_times_are_the_next_such_moment() {
        // 2026-11-03 23:12 local time.
        let mut tm: libc::tm = unsafe { std::mem::zeroed() };
        tm.tm_year = 126;
        tm.tm_mon = 10;
        tm.tm_mday = 3;
        tm.tm_hour = 23;
        tm.tm_min = 12;
        tm.tm_isdst = -1;
        let now = unsafe { libc::mktime(&mut tm) } as i64;
        let at = |w: &str| reset_unix(w, now).map(|t| (t - now) / 60);
        assert_eq!(at("5:03 AM"), Some(5 * 60 + 51), "tomorrow morning");
        assert_eq!(at("11:50 PM"), Some(38));
        assert_eq!(at("3am"), Some(3 * 60 + 48));
        assert_eq!(
            at("5pm"),
            Some(-(6 * 60 + 12)),
            "earlier today (within 12 h): already reset"
        );
        assert_eq!(at("Nov 4th 2026 3:17 AM"), Some(4 * 60 + 5));
        assert_eq!(at("17:30"), Some(-(5 * 60 + 42)));
        assert_eq!(at("soon"), None);
    }

    #[test]
    fn kinds_from_argv() {
        assert_eq!(agent_kind(&["/usr/bin/claude".into()]), Some("claude"));
        assert_eq!(agent_kind(&["codex".into(), "-c".into()]), Some("codex"));
        assert_eq!(agent_kind(&["bash".into()]), None);
    }
}
