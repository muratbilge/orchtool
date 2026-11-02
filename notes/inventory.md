# DevBox inventory (read-only), 2026-10-27

No files, processes, sockets or services were modified, started, stopped or sent to. Only read-only commands were run.

## 1. herdr

- Binary: /home/murat/.local/bin/herdr (17 MB, 2026-07-07). `file`: ELF 64-bit x86-64, static-pie, not stripped. Single native binary, no wrapper.
- `herdr --version` reports 0.7.1 (client and server both 0.7.1, protocol 14, channel stable).
- `~/.local/share/herdr`: does not exist.
- Config: /home/murat/.config/herdr/config.toml (theme tokyo-night, sound on, onboarding off).
- Other files in /home/murat/.config/herdr: herdr.sock and herdr-client.sock (both srw-------), herdr-server.log (4.4 MB), herdr-client.log (0.6 MB), herdr.log is referenced in help but not present, session.json (7 KB, workspace layout), release-notes.json (cached notes for v0.9.5, which is newer than the binary 0.7.1; the file is only a cache).
- State: /home/murat/.local/state/herdr/agent-detection/ (status.toml and remote/*.toml agent-detection manifests for cline, codex, droid, kimi, cursor, hermes, ...).
- /run/user/1000: no herdr socket. The socket lives in ~/.config/herdr/herdr.sock.
- Architecture: client/server. `herdr server` (pid 2441, running since about 6 days) is the headless server with a Unix socket API. The `herdr` client processes attach to it. `herdr status` reports server running and compatible.
- Not inside tmux (TMUX unset). herdr has its own PTY handling: it runs the panes itself, and the agents (claude, codex) run in its panes. Terminal-level agent detection goes through its own manifests.
- `herdr session list`: one session, `default`, running, directory /home/murat/.config/herdr.

### `herdr --help` (verbatim)

```
herdr — terminal workspace manager for AI coding agents

Usage: herdr [options]
       herdr --session <name> [options]
       herdr --remote <ssh-target> [--session <name>]
       herdr session attach <name>
       herdr update [--handoff]
       herdr channel set <stable|preview>
       herdr server stop
       herdr server reload-config
       herdr config <subcommand> ...
       herdr channel <subcommand> ...
       herdr workspace <subcommand> ...
       herdr worktree <subcommand> ...
       herdr tab <subcommand> ...
       herdr notification <subcommand> ...
       herdr agent <subcommand> ...
       herdr pane <subcommand> ...
       herdr wait <subcommand> ...
       herdr session <subcommand> ...
       herdr integration <subcommand> ...

Common commands:
  herdr                            Launch or attach to the persistent session
  herdr status [server|client]     Show local client and running server status
  herdr update                     Download and install the latest version
  herdr server stop                Stop the running server via the API socket
  herdr channel set <stable|preview> Choose the stable or preview update channel
  herdr server reload-config       Reload config.toml in the running server
  herdr config reset-keys          Back up config.toml and remove custom keybindings
  herdr channel <subcommand>       Manage the stable or preview update channel
  herdr workspace <subcommand>     Workspace helpers over the socket API
  herdr worktree <subcommand>      Git worktree helpers over the socket API
  herdr tab <subcommand>           Tab helpers over the socket API
  herdr notification <subcommand>  Notification helpers over the socket API
  herdr agent <subcommand>         Agent/terminal helpers over the socket API
  herdr pane <subcommand>          Pane control helpers over the socket API
  herdr wait <subcommand>          Blocking wait helpers over the socket API
  herdr session <subcommand>       Manage named persistent sessions
  herdr integration <subcommand>   Manage built-in agent integrations

Advanced commands:
  herdr server                     Run as headless server

Options:
  --no-session        Run monolithically (no server/client, escape hatch)
  --session <name>    Use or create a named persistent session
  --remote <target>   Attach through SSH to a remote Herdr server
  --remote-keybindings <local|server>
                      Keybindings for --remote app attach (default: local)
  --handoff           Opt into live handoff for update or remote attach
  --default-config    Print default configuration and exit
  --version, -V       Print version and exit
  --help, -h          Show this help

Config: /home/murat/.config/herdr/config.toml
Logs:   /home/murat/.config/herdr/herdr.log (plus herdr-client.log, herdr-server.log)
Env:    HERDR_CONFIG_PATH overrides config file path
Home:   https://herdr.dev
```

### Subcommand help (one level, verbatim)

`herdr workspace --help`
```
  herdr workspace list
  herdr workspace create [--cwd PATH] [--label TEXT] [--env KEY=VALUE] [--focus] [--no-focus]
  herdr workspace get <workspace_id>
  herdr workspace focus <workspace_id>
  herdr workspace rename <workspace_id> <label>
  herdr workspace close <workspace_id>
```

`herdr worktree --help`
```
  herdr worktree list [--workspace ID | --cwd PATH] [--json]
  herdr worktree create [--workspace ID | --cwd PATH] [--branch NAME] [--base REF] [--path PATH] [--label TEXT] [--focus] [--no-focus] [--json]
  herdr worktree open [--workspace ID | --cwd PATH] (--path PATH | --branch NAME) [--label TEXT] [--focus] [--no-focus] [--json]
  herdr worktree remove --workspace ID [--force] [--json]
```

`herdr tab --help`
```
  herdr tab list [--workspace <workspace_id>]
  herdr tab create [--workspace <workspace_id>] [--cwd PATH] [--label TEXT] [--env KEY=VALUE] [--focus] [--no-focus]
  herdr tab get <tab_id>
  herdr tab focus <tab_id>
  herdr tab rename <tab_id> <label>
  herdr tab close <tab_id>
```

`herdr notification --help`
```
  herdr notification show <title> [--body TEXT] [--position top-left|top-right|bottom-left|bottom-right] [--sound none|done|request]
```

`herdr agent --help`
```
  herdr agent list
  herdr agent get <target>
  herdr agent read <target> [--source visible|recent|recent-unwrapped] [--lines N] [--format text|ansi] [--ansi]
  herdr agent send <target> <text>
  herdr agent rename <target> <name>|--clear
  herdr agent focus <target>
  herdr agent wait <target> --status <idle|working|blocked|unknown> [--timeout MS]
  herdr agent attach <target> [--takeover]
  herdr agent start <name> [--cwd PATH] [--workspace ID] [--tab ID] [--split right|down] [--env KEY=VALUE] [--focus|--no-focus] -- <argv...>
  herdr agent explain <target> [--json]
  herdr agent explain --file PATH --agent LABEL [--json]
  targets accept terminal ids, unique agent names, detected/reported agent labels, and legacy pane ids
  agent send writes literal text; use pane run when you want command text plus Enter
```

`herdr pane --help`
```
  herdr pane list [--workspace <workspace_id>]
  herdr pane current [--pane ID|--current]
  herdr pane get <pane_id>
  herdr pane layout [--pane ID|--current]
  herdr pane process-info [--pane ID|--current]
  herdr pane neighbor --direction left|right|up|down [--pane ID|--current]
  herdr pane edges [--pane ID|--current]
  herdr pane focus --direction left|right|up|down [--pane ID|--current]
  herdr pane resize --direction left|right|up|down [--amount FLOAT] [--pane ID|--current]
  herdr pane zoom [<pane_id>|--pane ID|--current] [--toggle|--on|--off]
  herdr pane rename <pane_id> <label>|--clear
  herdr pane read <pane_id> [--source visible|recent|recent-unwrapped] [--lines N] [--format text|ansi] [--ansi]
  herdr pane split [<pane_id>|--pane ID|--current] --direction right|down [--ratio FLOAT] [--cwd PATH] [--env KEY=VALUE] [--focus] [--no-focus]
  herdr pane swap --direction left|right|up|down [--pane ID|--current]
  herdr pane swap --source-pane ID --target-pane ID
  herdr pane move <pane_id> --tab <tab_id> --split right|down [--target-pane ID] [--ratio FLOAT] [--focus|--no-focus]
  herdr pane move <pane_id> --new-tab [--workspace ID] [--label TEXT] [--focus|--no-focus]
  herdr pane move <pane_id> --new-workspace [--label TEXT] [--tab-label TEXT] [--focus|--no-focus]
  herdr pane close <pane_id>
  herdr pane send-text <pane_id> <text>
  herdr pane send-keys <pane_id> <key> [key ...]
  herdr pane report-agent <pane_id> --source ID --agent LABEL --state idle|working|blocked|unknown [--message TEXT] [--custom-status TEXT] [--seq N] [--agent-session-id ID] [--agent-session-path PATH]
  herdr pane report-agent-session <pane_id> --source ID --agent LABEL [--seq N] [--agent-session-id ID] [--agent-session-path PATH]
  herdr pane release-agent <pane_id> --source ID --agent LABEL [--seq N]
  herdr pane report-metadata <pane_id> --source ID [--agent LABEL] [--applies-to-source ID] [--title TEXT|--clear-title] [--display-agent TEXT|--clear-display-agent] [--custom-status TEXT|--clear-custom-status] [--state-label STATUS=TEXT] [--clear-state-labels] [--seq N] [--ttl-ms N]
  herdr pane run <pane_id> <command>
```

`herdr wait --help`
```
  herdr wait output <pane_id> --match <text> [--source visible|recent|recent-unwrapped] [--lines N] [--timeout MS] [--regex] [--raw]
  herdr wait agent-status <pane_id> --status <idle|working|blocked|done|unknown> [--timeout MS]
```

`herdr session --help`
```
  herdr session list [--json]
  herdr session attach <name>
  herdr session stop <name> [--json]
  herdr session delete <name> [--json]
  use 'default' as <name> to target the default session for stop
```

`herdr integration --help` (abridged: install/uninstall for pi, omp, claude, codex, copilot, devin, droid, kimi, opencode, kilo, hermes, qodercli, cursor; plus)
```
  herdr integration status [--outdated-only]
```

`herdr server --help`
```
  herdr server                run as headless server
  herdr server stop           stop the running server via the API socket
  herdr server live-handoff   hand off live panes to a new local server
  herdr server reload-config  reload config.toml in the running server
  herdr server agent-manifests [--json]  show agent detection manifest status
  herdr server update-agent-manifests [--json]  fetch and reload agent detection manifests
  herdr server reload-agent-manifests  reload agent detection manifests in the running server
```

`herdr config --help`
```
  herdr config reset-keys  back up config.toml and remove custom keybindings
```

`herdr channel --help`
```
  herdr channel show                  print the configured update channel
  herdr channel set <stable|preview>  choose the update channel
```

`herdr status` (output)
```
client: version 0.7.1, channel stable, protocol 14
server: status running, version 0.7.1, protocol 14, compatible yes, socket /home/murat/.config/herdr/herdr.sock
update: restart_needed no
```

### Read-only list outputs (snapshot at run time)

`herdr workspace list`: 7 workspaces, all agent_status idle except `orchtool` (working, focused):
- Hardio (wT, 2 panes), homelab-watch (wV, 3), revpi_web (wX, 1), paperscanner (wZ, 2), "~" (w11, 1, codex), "Paperscanner · Codex" (w12, 1, codex, cwd /projects/paperscanner), orchtool (w14, 1, claude, working).

`herdr agent list`: 8 agents.
- wT:p2 claude, /docker/Hardio, idle
- wV:p1 claude, /projects/homelab-watch, idle
- wX:p1 claude, /projects/revpi_web, idle
- wZ:p1 claude, /projects/paperscanner, idle
- w11:p1 codex, /home/murat, idle
- w12:p1 codex, /projects/paperscanner, idle (no session id reported)
- w14:p1 claude, /projects/orchtool, working (this session, 3b94b8d4-...)

`herdr session list`: default, running, /home/murat/.config/herdr, socket /home/murat/.config/herdr/herdr.sock.

## 2. Agent-related processes (ps, at run time)

```
2441     6-09:35:20 /home/murat/.local/bin/herdr server
2534     6-09:35:20 claude --resume 90839736-ca10-4f24-a52b-b239136d655e
2538     6-09:35:20 claude --resume fca8f7d3-c76a-4b12-92bb-d28c474e3eb3
571876   5-05:02:45 claude -c
575014   5-04:54:29 /home/murat/.codex/packages/app-server-daemon/releases/0.165.0-x86_64-unknown-linux-musl/bin/codex app-server daemon pid-update-loop
1678982  3-04:49:33 tail -f HANDOFF.md AGENTS.md CLAUDE.md ... (Hardio repo file watcher)
2094487  2-08:51:39 claude
3308714    10:54:29 codex
3447603    05:32:48 codex resume 01a17353-5041-73a1-9d0b-d72338c62ac5
3459415    05:13:24 /home/murat/.local/bin/herdr            (herdr client)
3459419    05:13:24 python3 -u -c  (Kabuk's agent watcher: runs over SSH, reads herdr's event stream and agent.list / workspace.list, prints JSON on change)
3572885      35:44 codex app-server --listen unix:// ... --managed-daemon
3576393      28:13 codex-code-mode-host
3581678      17:54 flock /tmp/claude-1000/hardio-heavy.lock ./gradlew --no-daemon ... build test (Hardio build)
3586478      07:01 claude                                     (this session)
```

Note: the herdr-related cron job `*/5 * * * * /home/murat/bin/herdr-codex-continue w12:p1 paperscanner` also exists (see section 4). No tmux, mosh, relay server process (relay serve) was seen in the grep output, so relay is not running at the moment.

## 3. Terminal and tool presence

| tool | status |
|---|---|
| tmux | not installed |
| mosh | not installed |
| zellij | not installed |
| ttyd | not installed |
| gotty | not installed |

Toolchain versions:
- go: not installed
- rustc / cargo: not installed
- bun: not installed
- python3: /usr/bin/python3, Python 3.13.5
- node: v22.23.3

## 4. Helper scripts (~/bin)

Line counts: relay 308, remind 154, codex-hardio-auto.sh 93, codex-paperscanner-auto.sh 136, codex-reset-reminder.py 146, remind-session-hook 5.

Crontab entries relevant here:
```
*/5 * * * * /usr/bin/python3 /home/murat/bin/codex-reset-reminder.py
*/5 * * * * /home/murat/bin/remind tick
*/10 * * * * /home/murat/bin/codex-hardio-auto.sh >> /home/murat/codex-jobs/codex-hardio-auto.cron.log 2>&1
*/5 * * * * /home/murat/bin/herdr-codex-continue w12:p1 paperscanner >> /home/murat/codex-jobs/herdr-codex-continue.log 2>&1
```

### ~/bin/relay (308 lines)
Header (first 30 lines, abridged to the usage block):
```
#!/usr/bin/env python3
"""relay - run commands on other machines with ONE short line, and get the output back automatically.
  relay make <name> <cmdfile|->  build ~/relay/www/<name> from a command list ... shows numbered steps, asks y/N, runs them, POSTs the transcript back
  relay serve [minutes]          serve ~/relay/www on :8000 and accept POST /out/<name> into ~/relay/out/<name>-<time>.log (default 20 min)
                                 `relay make` starts it by itself (120 min, detached) when it is not running.
  relay last <name>              print the newest received output for <name>
  GET /apk/<dir>/<file>          the server also serves app releases from ~/apk-share (.apk/.json only)
  relay wait <name> [minutes] [--next]   block until <name>'s output arrives ... exit 2 on timeout (default 60 min)
  relay status | relay watch | relay done <name> | relay history [n]
Commands run under sh (dash on Debian): no bash-isms.
```
One-line purpose: a relay/command-runner that serves a generated shell script over HTTP on DevBox (:8000) for other machines, runs it there after y/N confirmation, and POSTs the output back so Claude can `relay wait` for it.

### ~/bin/remind (154 lines)
```
#!/usr/bin/env python3
"""remind - global reminders for Murat + Claude Code on DevBox, delivered by Telegram.
  remind add "YYYY-MM-DD HH:MM" "<title>" "<what to tell Claude>" [--session ID] [--check CMD]
  remind list [--all|--brief]      pending reminders (--brief: one line each, for the SessionStart hook)
  remind done <id>                 mark finished
  remind cancel <id>               drop it
  remind tick                      cron: send every due reminder by Telegram (runs every 5 min)
  remind test <id>                 print the Telegram message without sending
Store: ~/reminders/reminders.json. --check runs a read-only shell command at fire time.
Telegram credentials: /docker/repos/srv_dockerconfigs/telegram-alerts/.env (TG_TOKEN, TG_CHAT).
"""
```
One-line purpose: stores reminders in ~/reminders/reminders.json and, on each 5-minute cron tick, sends due ones to Murat via Telegram, with a resume line for the Claude session.

### ~/bin/codex-hardio-auto.sh (93 lines)
```
#!/usr/bin/env bash
# codex-hardio-auto.sh — 2026-10-24 (Murat: "Plase run it automatically every after reset").
# Cron runs this every 10 minutes. While ~/.codex-hardio-auto.env exists (SESSION=<codex session id>, TASK=<label>),
# it resumes that Codex session in /docker/Hardio after each usage-limit reset, until Codex finishes without
# hitting the limit; then it sends the summary by Telegram and deletes the state file (the job is done).
# Codex is told: no commit/push/deploy. Logs/summaries: ~/codex-jobs/<time>-hardio-auto.{log,md}.
# New task: write the task prompt to ~/.codex-hardio-auto.prompt and ~/.codex-hardio-auto.env with TASK=...
# Stop:     rm -f ~/.codex-hardio-auto.env
```
One-line purpose: headless cron job that resumes a Codex (`codex exec`) session for the Hardio repo after each usage-limit reset and Telegrams the summary when it finishes. (Per Murat's 2026-10-27 visible-agents rule this is kept for reference only.)

### ~/bin/codex-paperscanner-auto.sh (136 lines)
```
#!/usr/bin/env bash
# codex-paperscanner-auto.sh — 2026-10-25 (Murat: Paperless Android client built by Codex, restart automatically after every usage-limit reset...).
# Same idea as codex-hardio-auto.sh, but for /projects/paperscanner and with a PHASE QUEUE:
#   ~/.codex-paperscanner/queue/NN-<label>.md   prompts, run in name order
#   ~/.codex-paperscanner/state.env             SESSION=<codex session id>, TASK=<label>
#   ~/.codex-paperscanner/next                  epoch: do nothing before this
#   ~/.codex-paperscanner/hold                  exists -> queue paused
# Cron: */10 * * * * ~/bin/codex-paperscanner-auto.sh >> ~/codex-jobs/codex-paperscanner-auto.cron.log 2>&1
```
One-line purpose: phase-queue version of the Codex auto-resumer for /projects/paperscanner, gated on docs/phases/<label>.status = GREEN, with a headless Claude/DeepSeek stand-in during limit gaps. (Its cron line is not in the crontab listing above, so it is not currently scheduled.)

### ~/bin/codex-reset-reminder.py (146 lines)
```
#!/usr/bin/env python3
"""codex-reset-reminder.py — 2026-09-14: Telegram reminder whenever Codex's usage limit resets.
Cron runs this every 5 minutes. It reads the tail of the newest Codex session logs (~/.codex/sessions),
finds the latest "You've hit your usage limit ... try again at <time>" error, and sends two Telegram messages per limit.
Check without sending:  python3 ~/bin/codex-reset-reminder.py --check
"""
```
One-line purpose: cron-driven watcher of ~/.codex/sessions that sends Telegram messages when a Codex usage limit is hit and again when it resets.

### ~/bin/remind-session-hook (5 lines)
```
#!/usr/bin/env bash
# SessionStart hook: inject open reminders (from ~/bin/remind) into every Claude Code session.
out=$(/home/murat/bin/remind list --brief 2>/dev/null)
```
One-line purpose: SessionStart hook that prints open reminders from `remind list --brief` into the new Claude session's context.

### Other files in ~/bin (not requested, listed for completeness)
b3-watch-summary.sh, backup-cleanup-reminder.sh, check-prod-pbs-backup.sh (mode 700), codex-continue-hardio.sh, codex-hardio-images.sh, codex-hardio-qobuz-spotify.sh, codex-reset-reminder.py (mode 700), crowdsec-watch.sh, crowdsec-week-summary.sh, cx, herdr-codex-continue, kprobe, mailru-unblock-watch.sh, npm-baikal-wellknown.py, npm-delete-host.py, npm-harden.py, npm-set-forward.py, paperscanner-progress, relay.bak-2026-10-19, relay.bak-20261009, remind, remind-session-hook, tg-say, plus __pycache__/.

### ~/.claude/agents
- codex.md (haiku model, tools Bash/Read): thin watcher that queues a task to codex-hardio-auto.sh and watches it.
- haiku-helper.md (haiku model, tools Bash/Read/Write/Edit): cheap helper for routine tasks.

## 5. Telegram

- Env file: /docker/repos/srv_dockerconfigs/telegram-alerts/.env. The path /home/murat/srv_dockerconfigs does not exist; the real path is /docker/repos/srv_dockerconfigs. Mode 600, 74 bytes.
- Key names only: TG_TOKEN, TG_CHAT.
- Fallback env ~/.config/homelab-telegram.env key names only: TG_GROUP, TG_TOPIC_CRITICAL, TG_TOPIC_WARNINGS, TG_TOPIC_DAILY, TG_TOPIC_REMINDERS, TG_TOPIC_SECURITY, TG_TOPIC_MEDIA.
- ~/bin/remind send(): urllib POST (form-encoded: chat_id, message_thread_id, text) to `https://api.telegram.org/bot<token>/sendMessage`, timeout 20 s. It uses curl-free Python urllib. The token and chat ID come from tg_creds() (the .env above). Values were not printed.
- Also: ~/bin/tg-say sends one short text to the homelab Telegram group (Reminders topic) and is used for phase/step updates.

## 6. Languages and toolchains

- go: not installed
- rustc / cargo: not installed
- python3: 3.13.5 (/usr/bin/python3)
- node: v22.23.3 (/usr/bin/node)
- bun: not installed

## 7. Projects and hardware

/projects: blacktv, calendar, homelab-watch, instaposter, kabuk, orchtool, paperscanner, revpi_web, spectralcheck, tvapp, vpnhelper

- nproc: 4
- free -h: total 7.8 GiB, used 6.4 GiB, free 513 MiB, available 1.3 GiB, buff/cache 1.1 GiB; swap 4.0 GiB total, 2.0 GiB used

Memory is tight: 1.3 GiB available of 7.8 GiB, with a Hardio gradle build (ktlint/build/test) running at the time of the snapshot.

## Anomalies noticed
- herdr binary is 0.7.1, but the cached release-notes.json describes 0.9.5. Only the cache is newer; not verified further.
- codex-paperscanner-auto.sh has no crontab entry in the current crontab listing.
- The paths in remind and codex-reset-reminder point to /docker/repos/..., and the task brief used /home/murat/srv_dockerconfigs, which does not exist.
