//! Agent CLI rewriting. Option arities follow the installed CLIs' --help; unknown syntax
//! is left alone, rather than mistaking an option value for a disposable prompt.
pub fn rewrite(argv: &[String], kind: &str, discovered: Option<&str>) -> Option<Vec<String>> {
    let exe = argv.first()?.clone();
    let mut options = Vec::new();
    let mut explicit = None;
    let mut positional = false;
    let mut resume = false;
    let mut i = 1;
    while i < argv.len() {
        let a = &argv[i];
        i += 1;
        if a == "--" {
            break;
        }
        if kind == "claude" && matches!(a.as_str(), "-c" | "--continue")
            || kind == "codex" && a == "--last"
        {
            if kind == "codex" {
                resume = false;
            }
            continue;
        }
        if kind == "claude" && matches!(a.as_str(), "-r" | "--resume") {
            if let Some(id) = argv.get(i).filter(|v| !v.starts_with('-')) {
                explicit = Some(id.clone());
                i += 1;
            }
            continue;
        }
        if kind == "claude" && a.starts_with("--resume=") {
            explicit = Some(a[9..].into());
            continue;
        }
        if !a.starts_with('-') {
            if kind == "codex" && !positional && a == "resume" {
                resume = true;
                positional = true;
                continue;
            }
            if resume {
                explicit = Some(a.clone());
                resume = false;
            }
            positional = true;
            continue; // Only prompt positionals are discarded.
        }
        let flag = a.split('=').next()?;
        let values: &[&str] = if kind == "claude" {
            &[
                "--model",
                "--agent",
                "--agents",
                "--append-system-prompt",
                "--autocompact",
                "--debug-file",
                "--effort",
                "--environment",
                "--fallback-model",
                "--input-format",
                "--json-schema",
                "--max-budget-usd",
                "-n",
                "--name",
                "--output-format",
                "--permission-mode",
                "--permission-prompts",
                "--plugin-dir",
                "--plugin-url",
                "--session-id",
                "--setting-sources",
                "--settings",
                "--system-prompt",
                "--system-prompt-snapshot",
                "--remote-control-session-name-prefix",
            ]
        } else {
            &[
                "-c",
                "--config",
                "-m",
                "--model",
                "-p",
                "--profile",
                "-s",
                "--sandbox",
                "-a",
                "--ask-for-approval",
                "-C",
                "--cd",
                "--enable",
                "--disable",
                "--local-provider",
                "--remote",
                "--remote-auth-token-env",
                "--add-dir",
            ]
        };
        let many: &[&str] = if kind == "claude" {
            &[
                "--add-dir",
                "--allowedTools",
                "--allowed-tools",
                "--disallowedTools",
                "--disallowed-tools",
                "--betas",
                "--file",
                "--mcp-config",
                "--tools",
            ]
        } else {
            &["-i", "--image"]
        };
        let switches: &[&str] = if kind == "claude" {
            &[
                "--dangerously-skip-permissions",
                "--allow-dangerously-skip-permissions",
                "--ax-screen-reader",
                "--bg",
                "--background",
                "--bare",
                "--brief",
                "--chrome",
                "--desktop",
                "--disable-slash-commands",
                "--exclude-dynamic-system-prompt-sections",
                "--fork-session",
                "--forward-subagent-text",
                "-h",
                "--help",
                "--ide",
                "--include-hook-events",
                "--include-partial-messages",
                "--no-chrome",
                "--no-session-persistence",
                "-p",
                "--print",
                "--replay-user-messages",
                "--restricted",
                "--safe-mode",
                "--strict-mcp-config",
                "--tmux",
                "--verbose",
                "-v",
                "--version",
            ]
        } else {
            &[
                "--all",
                "--include-non-interactive",
                "--strict-config",
                "--oss",
                "--approve-for-me",
                "--dangerously-bypass-approvals-and-sandbox",
                "--dangerously-bypass-hook-trust",
                "--yolo",
                "--full-auto",
                "--worktree",
                "--search",
                "--no-alt-screen",
                "--no-daemon",
                "-h",
                "--help",
                "-V",
                "--version",
            ]
        };
        if !values.contains(&flag) && !many.contains(&flag) && !switches.contains(&flag) {
            return None;
        }
        options.push(a.clone());
        if a.contains('=') {
            continue;
        }
        if values.contains(&flag) {
            options.push(argv.get(i)?.clone());
            i += 1;
        } else if many.contains(&flag) {
            while let Some(value) = argv.get(i).filter(|v| !v.starts_with('-')) {
                options.push(value.clone());
                i += 1;
            }
        }
    }
    let id = discovered.map(str::to_string).or(explicit);
    let mut out = vec![exe];
    if kind == "claude" {
        match id {
            Some(id) => out.extend(["--resume".into(), id]),
            None => out.push("--continue".into()),
        }
    } else if kind == "codex" {
        out.push("resume".into());
        out.push(id.unwrap_or_else(|| "--last".into()));
    } else {
        return None;
    }
    out.extend(options);
    Some(out)
}

/// Only the first simple shell command. No evaluation, variable expansion or shell execution.
pub fn shell_agent(argv: &[String], kind: &str) -> Option<Vec<String>> {
    let at = argv
        .iter()
        .position(|a| a.starts_with('-') && a.ends_with('c'))?;
    let script = argv.get(at + 1)?;
    if script.contains(['$', '`']) {
        return None;
    }
    let mut quote = None;
    let mut escape = false;
    let mut end = script.len();
    for (i, c) in script.char_indices() {
        if escape {
            escape = false;
            continue;
        }
        if c == '\\' && quote != Some('\'') {
            escape = true;
            continue;
        }
        if let Some(q) = quote {
            if c == q {
                quote = None;
            }
        } else if c == '\'' || c == '"' {
            quote = Some(c);
        } else if matches!(c, ';' | '&' | '|' | '\n') {
            end = i;
            break;
        }
    }
    let words = shell_words::split(&script[..end]).ok()?;
    let words: Vec<String> = words.into_iter().skip_while(|w| w.contains('=')).collect();
    (std::path::Path::new(words.first()?).file_name()? == kind).then_some(words)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn v(s: &[&str]) -> Vec<String> {
        s.iter().map(|s| s.to_string()).collect()
    }
    #[test]
    fn exact_resume_keeps_options_and_drops_only_selectors_and_prompts() {
        for (kind, before, after) in [
            (
                "claude",
                vec!["claude", "--model", "x", "old task"],
                vec!["claude", "--resume", "exact", "--model", "x"],
            ),
            (
                "claude",
                vec!["claude", "--dangerously-skip-permissions", "-c"],
                vec![
                    "claude",
                    "--resume",
                    "exact",
                    "--dangerously-skip-permissions",
                ],
            ),
            (
                "claude",
                vec!["claude", "--resume", "older", "--model=x"],
                vec!["claude", "--resume", "exact", "--model=x"],
            ),
            (
                "codex",
                vec!["codex", "-m", "y", "-c", "foo=1", "task"],
                vec!["codex", "resume", "exact", "-m", "y", "-c", "foo=1"],
            ),
            (
                "codex",
                vec!["codex", "-m", "y", "resume", "--last", "--no-alt-screen"],
                vec!["codex", "resume", "exact", "-m", "y", "--no-alt-screen"],
            ),
            (
                "codex",
                vec!["codex", "resume", "older", "task", "--yolo"],
                vec!["codex", "resume", "exact", "--yolo"],
            ),
        ] {
            assert_eq!(
                rewrite(&v(&before), kind, Some("exact")).unwrap(),
                v(&after)
            );
        }
        assert_eq!(
            rewrite(&v(&["claude", "--model", "x"]), "claude", None).unwrap(),
            v(&["claude", "--continue", "--model", "x"])
        );
        assert_eq!(
            rewrite(&v(&["codex", "resume", "known", "-m", "y"]), "codex", None).unwrap(),
            v(&["codex", "resume", "known", "-m", "y"])
        );
        assert_eq!(
            rewrite(
                &v(&["codex", "resume", "--last", "old prompt", "-m", "y"]),
                "codex",
                None
            )
            .unwrap(),
            v(&["codex", "resume", "--last", "-m", "y"])
        );
        assert!(rewrite(
            &v(&["claude", "--new-unknown-option", "value"]),
            "claude",
            Some("id")
        )
        .is_none());
        assert_eq!(
            shell_agent(
                &v(&[
                    "bash",
                    "-lc",
                    "ORCH_AGENT='name' claude --model 'x y'; exec bash"
                ]),
                "claude"
            )
            .unwrap(),
            v(&["claude", "--model", "x y"])
        );
    }
}
