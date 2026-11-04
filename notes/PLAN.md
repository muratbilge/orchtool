# Orchtool — plan (draft 2, 2026-10-27)

Goal: **our own agent workspace manager that replaces herdr** — keeps everything herdr does (panes, workspaces,
agents that survive disconnects, agent status) and adds **agent orchestration, one visible timeline of all
agent↔agent and agent↔Murat communication, and a blocking "ask Murat" that reaches him on SSH, phone and
Telegram**. Must feel faster than herdr over SSH.

Decisions (Murat, 2026-10-27): **Rust** · **own stack, no herdr** · **own Telegram bot** · name **orchtool** (CLI `orch`).

Background research (DevBox inventory, other tools, transport options) is in git history: notes/inventory.md, notes/research.md.

## Architecture

```
 laptop:  orch attach ──ssh stdio── orch proxy ┐
 phone:   Kabuk ───────ssh / websocket──────────┤
 Telegram ◄──► bot task ────────────────────────┼─► orchd (DevBox, systemd --user)
 agents:  orch-mcp (stdio) ─────────────────────┘     ├ PTYs (portable-pty) + terminal state (vt100 / alacritty_terminal)
                                                       ├ sessions / workspaces / tabs / panes
                                                       ├ agent status (hooks + screen detection)
                                                       ├ message bus + questions + tasks (SQLite)
                                                       └ Unix socket API + event stream
```

### Why it can be faster than herdr over SSH
The terminal state of every pane lives in orchd. `orch attach` runs **on the client machine** and talks to orchd
through `ssh host orch proxy` (one SSH stdio channel, ControlMaster-friendly). orchd sends **screen diffs**
(mosh-style state sync: only changed cells, coalesced to the frame rate, latest state wins), not the raw byte
stream, so a slow link never builds up a backlog; local rendering, instant pane switching from cached state,
optional local echo later. Works anywhere SSH works (no UDP needed); `orch attach` can also run directly on
DevBox.

### Components (cargo workspace)
- `orch-proto` — message types (serde; JSON-lines on the socket for easy scripting, binary frames for diffs).
- `orchd` — the server: PTY panes, terminal emulation, scrollback, layout tree, multi-client attach,
  persistence of layout + agent session ids (after a daemon restart agents are relaunched with `--resume`),
  event stream, SQLite store.
- `orch` — CLI + TUI client (ratatui/crossterm): `orch new|ls|attach|send|read|kill|tail|ask|answer|proxy`.
- `orch-mcp` — MCP server for agents (rmcp): `send_message`, `read_inbox`, `ask_human` (blocks until answered),
  `post_status`, `claim_files` (advisory leases).
- Telegram bot (inside orchd, own bot): questions with inline buttons / reply-to-answer, finished/failed digest.
- herdr-compat layer: same JSON methods Kabuk uses (`agent.list`, `workspace.list`, `pane.list`, `pane.read`,
  `pane.send-text`, `events.subscribe`) so **Kabuk and our scripts keep working** during the switch.

### Agent awareness
Status (idle / working / blocked / done) from Claude Code hooks (Notification, Stop, UserPromptSubmit,
PreToolUse) and Codex hooks where available, screen-pattern detection as fallback. Blocked prompts are read
from the pane and land in the **needs-you inbox** together with `ask_human` questions; first answer from any
surface (TUI, Kabuk, Telegram) wins.

### Communication timeline
Every message (agent→agent, agent→Murat, Murat→agent, orchestration events) is a row: from, to, thread,
kind, text, time. Delivery into an agent = typed into its pane when it's idle (queued while working) or read via
`read_inbox`. The TUI has a Comms view (all threads, live), per-agent filter, and you can write into any thread.

### Orchestration
Task queue per project (generalises paperscanner's phase queue + GREEN gate), role templates (Codex builds,
Codex reviews, Claude checks), limit-reset "continue" for Codex panes, worktree per agent optional.
Replaces `herdr-codex-continue`, `codex-*-auto.sh`, `codex-reset-reminder.py` over time.

### Constraints
DevBox: 4 cores, 7.8 GB RAM (often ~1.3 GB free), 89 GB disk. Rust builds: `CARGO_BUILD_JOBS=2`, take
`flock /tmp/claude-1000/hardio-heavy.lock` around cargo builds (shared with the gradle builds). Keep orchd lean
(target < 50 MB RSS with 10 panes).

## Phases (draft 4, 2026-11-02 — TUI before Kabuk; Kabuk gets a native orchtool backend)
Murat's goal (2026-11-02): manage ALL agents in a Rust tool — like herdr but better and faster — let them talk to
each other, report to him and ask him, with the best interaction and efficiency.
Decisions (Murat OK'd Codex defaults 2026-11-02): daemon restart may interrupt running agents at first (orchd
reports what was interrupted, no silent relaunch); Telegram = private chat, short questions, no automatic
terminal-output forwarding. Engineering defaults from Codex's opinion (in git history, CODEX-OPINION.md) apply (versioned JSON protocol first,
vt100 behind an adapter, one controlling client + observers, bounded scrollback/queues, rusqlite + tokio).

Codex builds each phase in the visible herdr workspace "Orchtool · Codex", Claude checks key claims; each phase
ends GREEN (tests + short demo note in notes/STATUS.md).

| # | Phase | Done when |
|---|---|---|
| 0 | rustup, git, cargo workspace, `scripts/check.sh`; **spikes**: inspect Kabuk's real herdr client (framing, fields, events) → notes/kabuk-contract.md; vt100 check with bash/vim/claude/codex | check.sh green, both notes written |
| 1 | orchd PTY core (slim): spawn/kill panes, vt100 state, bounded scrollback, attach/detach, snapshot + revisioned diffs with resnapshot, controller/observer, `orch new/ls/attach/send/read/kill/proxy` | bash, vim, claude, codex run in orch panes, survive detach + SSH logout, two clients mirror |
| 2 | **Agent bus + ask_human**: SQLite timeline, orch-mcp (`send_message`, `read_inbox`, `ask_human` resumable, `post_status`), `orch tail/ask/answer`, minimal own Telegram bot (private chat, inline answers). Works for agents in herdr panes **and** orch panes | two agents talk visibly; one asks Murat, he answers via CLI or Telegram, reconnect loses nothing, answer applied once |
| 3 | **Full TUI + agent status** (replaces herdr on DevBox + laptop): workspaces/tabs/splits, keybindings, status bar, agent list with working/idle/blocked/done (hooks + screen fallback), needs-you inbox, Comms view; `orch attach host` over ssh proxy, measured vs herdr | Murat uses `orch` instead of herdr for a day, measurably faster |
| 4 | **Kabuk: native orchtool backend** (Murat 2026-11-02: "add orchtool option to Kabuk, we have the code"): backend switch herdr/orchtool, agents/panes via `orch … --json` over SSH, answers questions natively, timeline (Comms) tab; released through Kabuk's own in-app updater. No herdr-compat layer needed | phone shows agents, timeline and questions from orchd |
| 5 | Orchestration: task queues, role flows (build → review → check), limit-reset continue, worktrees; move paperscanner + Hardio | herdr not needed anymore |

## For Murat
- Create the bot: Telegram → @BotFather → `/newbot` (e.g. "Orchtool"), then put the token into
  `~/.config/orchtool/telegram.env` as `TG_TOKEN=…` (chmod 600) and send the bot one message (so it can find
  your chat). Needed from phase 5.
