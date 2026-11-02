# Research: multi-agent CLI orchestration tools (as of 2026-10-27)

Method: about 24 web calls (WebSearch and WebFetch). Items marked (unverified) come from
secondary sources or could not be confirmed on the primary repo page.

## Tool table

| Tool | URL | Lang / license | Activity | Runs agents via | Remote / SSH | Agent-to-agent | Ask human + block | Web / mobile UI |
|---|---|---|---|---|---|---|---|---|
| herdr | https://github.com/herdrdev/herdr (fetched page; another repo name also appears in search) | Rust, Apache-2.0 | last date unverified (1,878 commits on page) | own background server that "owns the terminals" (PTY vs tmux not stated) | yes: saved SSH machines shown with local agents, independent reconnects | yes: spawn panes, prompt each other, wait until another agent is blocked | no structured question; notifies when an agent is blocked/idle | none mentioned |
| claude-squad | https://github.com/smtg-ai/claude-squad | Go (inferred from go.mod), AGPL-3.0 | last date unverified | tmux sessions + git worktree per agent | not in page | not in page | no | TUI only |
| agent-deck | https://github.com/asheshgoplani/agent-deck | Go + Bubble Tea, license unverified | unverified | tmux | unverified | fork inherits parent history (not messaging) | unverified | TUI |
| agent-manager | https://github.com/YoanWai/agent-manager | Go, Apache-2.0 (secondary) | unverified | ordinary tmux sessions on own socket (quitting leaves agents running) | unverified | unverified | unverified | TUI; status from Claude Code hook events |
| uzi | https://github.com/devflowinc/uzi | Go | unverified (about 560 stars per a directory, likely stale) | tmux + worktree per agent, dev server per agent port | no | no | `uzi auto` auto-confirms prompts | none |
| workmux / dmux / cwt / git-paw | (search only) | unverified | unverified | tmux + worktrees | no | no | no | TUI/CLI |
| Conductor | https://www.conductor.build | macOS, closed source, commercial (Melty Labs, YC) | active | git worktree + branch per task | Cloud product (unverified) | no | review and merge UI | mobile app in paid tiers (unverified) |
| Crystal | https://github.com/stravu/crystal | desktop app, license unverified | deprecated: final v0.3.5 (2026-02-26), successor Nimbalyst (secondary) | worktree per session | no | no | no | desktop only |
| opcode | https://github.com/winfunc/opcode | Tauri 2, TS + Rust, AGPL-3.0; about 22k stars (secondary) | last push about 6 months ago per one tracker (unverified) | Claude Code CLI wrapper | no | no | no | desktop GUI |
| vibe-kanban | (BloopAI/vibe-kanban) | Apache-2.0 (secondary) | possible sunsetting (one fork README, unverified) | kanban task to git worktree per workspace, agent launched | no | no | no | web kanban UI |
| MCP Agent Mail | https://github.com/Dicklesworthstone/mcp_agent_mail | Python (FastMCP), license unnamed; superseded by mcp_agent_mail_rust (Rust) | no longer actively developed | not a runner: coordination server | HTTP server on port 8765, bearer/JWT auth, Docker | yes: inbox, threads, reply, search; advisory file reservations (TTL, exclusive flag; optional pre-commit hook) | no blocking primitive; "Human Overseer" sends async messages | yes: server-rendered viewer at /mail |
| Zellij | https://zellij.dev/documentation/web-client.html | Rust, license unverified (believed MIT) | active | own PTY multiplexer | built-in web server (since 0.43.0): attach from browser or `zellij attach` over HTTPS with token | no | no | yes: web client, read-only tokens, share plugin |
| tmux control mode | https://github.com/tmux/tmux/wiki/Control-Mode | C, ISC | mature | protocol on top of tmux | n/a | n/a | n/a | n/a |
| ttyd | https://github.com/tsl0922/ttyd | C, libwebsockets + xterm.js, MIT (unverified) | active | runs one command, streams over WebSocket | web-based; port 7681 default | no | no | yes, browser. `--writable` controls read-only vs interactive (2024 log); multi-viewer via tmux |
| WezTerm mux | https://wezterm.org/multiplexing.html | Rust, MIT (unverified) | active | own mux server: unix, SSH, TLS domains | SSH domain auto-spawns remote mux (needs WezTerm on remote) | no | no | no |
| Claude Code agent teams | https://code.claude.com/docs/en/agent-teams | Claude Code (closed) | experimental, off by default | team lead + teammates, each a full Claude session | local only | yes: peer mailbox + shared task list | teammates can ask lead (per docs summary, unverified) | none (split-pane via tmux unverified) |
| Claude Code Remote Control | (Anthropic docs not fetched; third-party write-ups) | closed | new in 2026 | `/rc` or `claude --remote-control`; session stays on your machine | phone app or claude.ai/code | multiple sessions visible | n/a | yes: Claude mobile app + claude.ai/code (requires claude.ai login, not API key). Conflicting reports on starting sessions from phone |
| A2A protocol | (spec repo not fetched) | Linux Foundation project since 2025-06 | active; 150+ orgs per LF (Apr 2026) | agent-to-agent over HTTP (not terminal-level) | n/a | yes, protocol-level (tasks between agents) | task states can require input (general knowledge, unverified here) | n/a |

## Transport and terminal-level findings

- **tmux control mode (`-C` / `-CC`)**: text protocol. Commands return `%begin`/`%end` (or `%error`) blocks.
  Async notifications start with `%`: `%output`, `%extended-output`, `%pause`, `%continue`,
  `%window-*`, `%session-*`, `%exit`. Output is octal-escaped. Flow control: `refresh-client -f pause-after=N`
  pauses lagging panes. iTerm2 uses this for native tabs and windows. Mosh cannot drive `-CC` (secondary source).
- **Mosh**: UDP, syncs screen state, local echo, roaming across IP changes, best on bad links. Needs UDP. Limited scrollback. No tmux `-CC`.
- **Eternal Terminal (ET)**: TCP, reconnects the same session, real scrollback, works where UDP is blocked. Needs a root daemon on the server. Slightly worse latency than mosh on very bad links (sources disagree on the size).
- **ttyd / WebSocket + xterm.js**: simplest route to a browser or phone terminal. Put it behind auth (Zellij uses login and read-only tokens).
- **creack/pty** (Go): MIT, latest v1.1.21 (secondary, about a year old), v2 module path exists. Bubble Tea and portable-pty / ratatui were not researched (gap).

## Ideas worth stealing

1. **Drive agents through tmux `-CC` or your own PTY server, not screen scraping.** The control-mode `%output` stream with `pause-after` flow control is a ready-made, structured backend. A daemon that owns PTYs (herdr style) lets the laptop and phone detach and reattach.
2. **Hook-based agent state.** agent-manager gets working / blocked / idle from Claude Code hook events instead of guessing from pane text. Use hooks (Stop, Notification) to drive the sidebar.
3. **One git worktree and branch per agent** is the consensus pattern (Conductor, uzi, claude-squad, agent-deck, Crystal).
4. **Blocking `ask_human` as an MCP tool** is a gap. Herdr only notifies on "blocked"; Agent Mail is asynchronous. A tool that posts the question to the phone, blocks the agent until an answer arrives, and returns the reply would be a differentiator.
5. **Phone access by a web gateway**: xterm.js over WebSocket behind auth, with read-only tokens (Zellij pattern) and a per-pane "watch" view.
6. **Transport choice**: mosh-style state sync for flaky mobile links, ET as the fallback where UDP is blocked. Keep tmux `-CC` out of the mosh path.
7. **Advisory file leases plus a pre-commit check** (mcp_agent_mail) to stop agents editing the same files.
8. **Human overseer message lane**: agent inboxes that the human can write to from the web UI (mcp_agent_mail), with thread IDs.
9. **Use A2A-style task objects for agent-to-agent handoff**, so the same messages work across machines (unverified fit).

## Open gaps (not verified)

- herdr: exact repo owner (search gave two names), release date, PTY vs tmux, socket API details.
- Last-activity dates for claude-squad, agent-deck, agent-manager, uzi, workmux, Zellij, ttyd, WezTerm.
- Licenses for Zellij, ttyd, WezTerm, agent-deck, Crystal.
- Claude Code Remote Control and agent teams: only third-party write-ups were read; the official docs for Remote Control were not fetched. Agent teams settings format is disputed.
- Bubble Tea and ratatui / portable-pty research not done.
