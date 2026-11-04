# orchtool

`orch` runs your coding agents (Claude Code, Codex, plain shells) in panes on a dev server, keeps them running
while you are away, and gives them a bus to talk to each other and to you: messages, status, questions you can
answer from the TUI, from a script, or from a phone. [Kabuk](https://github.com/muratbilge/kabuk), the Android
terminal, drives it from the phone: agents, their permission dialogs and questions, and the workspaces, live.

Built in Rust (stable) by Claude Code and Codex with Murat Bilge steering, for one dev server first. It is young
(0.1.0): `STATUS.md` is the running log, `PLAN.md` the plan, `CODEX-*.md` the review rounds.

## Crates

- **`orchd`**, the daemon. Owns the PTYs (`portable-pty` + `vt100`), the workspaces and tabs, and a SQLite store
  for the bus (timeline, questions, answers). Reads each agent's status off its screen: working, blocked on a
  permission dialog, idle, done, or limited (a usage-limit notice; orchd types "continue" a minute after the reset
  unless `orch auto-continue <pane> off`). Answers a dialog only if the screen still hashes to what the client
  saw (`send --expect`), and each screen once. `orch stop-daemon --restart` saves the layout and the next orchd
  recreates it, agents resumed. Starts on demand from `orch`, or as a user service (`contrib/orchd.service`).
- **`orch`**, the CLI and the TUI (`ratatui`; mouse and touch: tap a pane or a tab, `+` for a new tab, `☰` for
  the workspace list). Panes: `new`, `ls`, `attach`, `send`, `read`, `kill`, `focus`; layout: `ws` and `tab`
  (focus, new, rename, close, by id); bus: `agents`, `tail`, `msg`, `inbox`, `questions`, `question`, `answer`,
  `ask`, `status`; `watch --json` streams everything for clients; `--host H` talks to an orchd on another
  machine through `ssh H orch proxy`.
- **`orch-mcp`**, an MCP server for agents on the bus: `send_message`, `read_inbox`, `ask_human`,
  `wait_for_answer`, `post_status`, `list_agents`.
- **`orch-proto`**, the versioned JSON-lines protocol between them.

## Install

```sh
scripts/install.sh        # cargo build --release, then orchd, orch, orch-mcp → ~/.local/bin
orch                      # the TUI; starts orchd if it isn't running (socket ~/.local/state/orchtool/orchd.sock)
scripts/check.sh          # fmt, clippy, build, tests
```

Rust stable via rustup with rustfmt and clippy (`rust-toolchain.toml`). The scripts take a build lock in
`/tmp/claude-1000/` because the dev server shares its RAM with other builds; drop that line elsewhere.

## Agents on the bus

- **Claude Code:** `orch hook` as the hook entry point registers the agent and keeps its status current; the MCP
  server gives it the bus tools. A pane without hooks is still listed, from its screen.
- **Any script or shell agent:** `orch ask "<question>"` waits for the human's answer; `orch status working|idle|blocked|done`;
  `orch msg -- <agent> "<text>"`.
- **You:** `orch questions`, `orch answer <id> <text or option number>`, `orch tail -f`, `orch inbox`.

## Clients

`orch watch --json` is the stream a client reads (agents, their screens and hashes, questions, timeline,
workspaces); actions are single `orch` commands. The contract Kabuk relies on, with the stream shape, the exact
commands, the exit codes and the shared fixture `crates/orch/tests/fixtures/watch-v1.json`, is
`notes/kabuk-contract-v2.md`.

Exit codes (`orch --help`): 0 done · 1 other error · 2 usage · 10 the screen changed, nothing sent · 11 answered
already · 12 the program exited · 13 the program isn't reading input · 14 no such pane, question, workspace or
tab · 20 orchd unreachable.

## License

Apache License 2.0, see `LICENSE`.
