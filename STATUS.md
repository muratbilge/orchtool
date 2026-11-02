# Status

## 2026-11-02 — Phase 2 GREEN (agent bus, ask_human, Telegram) — Claude

Built
- orchd bus (crates/orchd/src/bus.rs): SQLite at ~/.local/state/orchtool/orchtool.db (`--db`), agents /
  messages / per-agent read marks / questions. Persist first, then notify; first answer wins atomically;
  waits are resumable (ask.wait, msg.inbox with wait); timeline follow stream catches up on lag.
- Identity: server-side, keyed by pane (`orch:pN`, `herdr:wX:pY`), name `<project>-<kind>`, numbered on
  collision, `$ORCH_AGENT` overrides. orch-mcp registers at initialize, and again on every call.
- orch-mcp (hand-written MCP stdio JSON-RPC, no SDK): send_message, read_inbox, ask_human, wait_for_answer,
  post_status, list_agents; one task per call, notifications/cancelled aborts. Default waits: Claude 600 s
  (max 1800), Codex 50 s (max 55, its tool timeout is 60 s) — the answer is never lost, the agent calls
  wait_for_answer or finds it in read_inbox.
- Delivery: explicit inbox; Claude Stop hook (`orch hook`) hands unread messages over at turn end
  ({"decision":"block"}); idle-only nudges typed into the agent's pane (never when status is unknown /
  working / blocked, so nothing is typed into a permission prompt).
- Telegram (crates/orchd/src/telegram.rs): own bot, paired chat only (`orch telegram pair` → `/pair CODE`
  from a private chat, or TG_CHAT), option buttons + reply-to-answer, answered-elsewhere edits the message,
  agent→murat messages forwarded and replies routed back; token scrubbed from errors; no terminal output.
- CLI: agents, tail [-f], msg, inbox, questions, answer (number = option), ask, status, hook, telegram pair.

Checks
- `scripts/check.sh` → exit 0: proto 3, orchd unit 14 (5 bus), panes 10, bus e2e 6, spike 3. bus+panes
  re-run 3× green.
- e2e (real orchd + orch-mcp + orch binaries): two agents talk, inbox wait wakes, ask_human answered while
  blocking, second answer refused, timeline order; question survives orch-mcp death + orchd restart and
  wait_for_answer resumes, answer in inbox exactly once; MCP cancel + unknown method; Stop hook delivers
  once, silent without orchd; Telegram via mock Bot API: stranger ignored, button answers, message edited,
  reply routed to the agent; one-time pairing code.
- Live demo (test daemon): real Claude (`--mcp-config`) asked real Codex (`-c mcp_servers.orch…`)
  "17*23" → "391" → ask_human to murat → answered via `orch answer 1 1` → second answer refused → Claude
  reported "yes". Idle nudge: Codex got "[orch] new message from murat…", read inbox, replied PONG.

Found in the live demo and fixed
- `send --enter`: Codex's paste detection swallowed a CR 60 ms after the text → 250 ms.
- Agents were only reachable after their first tool call → orch-mcp registers at initialize.
- Codex starts MCP servers with a clean env → config must forward the pane:
  `env_vars = ["ORCH_PANE", "HERDR_PANE_ID"]`.

Not done / for Murat
- Telegram not tried against the real API: needs the bot token (BotFather → ~/.config/orchtool/telegram.env).
- Claude/Codex MCP + hook config is not installed globally (needs Murat's OK). Codex asks approval per
  orch tool call unless "Allow for this session"/"Always allow" is chosen.

## 2026-11-02 — Phase 1 review fixes (Codex review → CODEX-REVIEW-P1-RESULT.md) — Claude

All 8 findings confirmed and fixed, each with a regression test:
1. Mouse modes now sent as explicit set+reset state (vt100's formatter never reset them); detach also resets ?9.
2. Wide glyph cut by an observer's right edge → blank at an explicit position (no wrap/scroll). Found a second
   bug via the new clipped random test: blank runs are erase sequences that don't move the cursor.
3. Controller watch subscribed before registering + reconciled after the attach reply.
4. Resize: PTY ioctl, parser size and gen updated together under the terminal lock; `resize_for(client)`
   re-checks control under the lock.
5. Kill: SIGHUP → SIGTERM(group) → SIGKILL(group), 1.5 s each; pane kept + `kill_failed` if it survives.
6. Input budget 1 MiB per pane; `send` returns `input_full`, attach drops keys; never blocks.
7. Exit published only after the reader hit PTY EOF (max 2 s wait for descendants holding the PTY).
   Reader/waiter threads hold Weak<Pane>.
8. Raw-mode guard created right after enabling raw mode.
Checks: `scripts/check.sh` exit 0 (unit 9, integration 10, proto 3, spike 3); integration ×6 → 10/10 each.
Live: codex still renders in an orch pane; `orch kill` of a HUP+TERM-ignoring loop → gone in 3.0 s.

## 2026-11-02 — Phase 1 GREEN (PTY core) — Claude

Built: orchd (panes in PTYs, vt100 state behind `term.rs`, query replies, scrollback, per-client row-diff
renderer, controller/observer + takeover, JSON-lines socket 0600 + peer-uid check), `orch` CLI
(new/ls/attach/send/read/kill/proxy/ping/latency/stop-daemon, `--host H` via `$ORCH_SSH H orch proxy`,
autostarts orchd with setsid), `scripts/install.sh`, `contrib/orchd.service` (optional, not enabled).

Design notes
- Socket: `~/.local/state/orchtool/orchd.sock` (not $XDG_RUNTIME_DIR: Linger=no, it vanishes at logout).
- Sync: renderer keeps what each client has (formatted rows + modes + cursor); a frame rewrites changed rows
  only, ≤ 25 fps, nothing queued → a slow client jumps to the latest state; frames carry gen/base/rev,
  client asks `resnapshot` on a gap; resize bumps gen → full redraw. View clipped to the client's size.
- Size: controller's terminal decides; observers never resize; detached panes keep the last size.
- Terminal queries answered by orchd (DSR 5/6, DA1, DA2, XTVERSION, DECRQM, OSC 10/11): Codex's TUI starts
  with no client attached. Kitty keyboard query left unanswered (= unsupported).
- `orch new` passes the caller's env minus HERDR_*/TMUX*/Claude-session vars (CLAUDECODE,
  CLAUDE_CODE_CHILD_SESSION, CLAUDE_CODE_MESSAGING_TOKEN, …) — otherwise a pane started from Claude
  inherits "transcript saving off" and Claude's messaging token.
- Daemon restart kills panes (accepted for now); SIGHUP is ignored.

Checks run (2026-11-02)
- `scripts/check.sh` → exit 0 (fmt ok, clippy -D warnings ok; tests: proto 3, orchd unit 6, integration 7,
  terminal_spike 3 — all passed). Integration suite re-run 5× → 7/7 each time.
- Unit: render random-diff property test (39 seeds × 60 steps, skipped frames, alt screen, wide chars,
  insert-line) — client vt100 equals server cell by cell; clipped observer; query replies incl. fragmented;
  scrollback read order.
- Integration (real orchd): echo+exit code, send+enter, two clients mirror, observer input ignored,
  detach/reattach keeps state + frees controller slot, takeover + controller sizing, resize storm under
  output ends consistent, query reply with no client, kill + name resolution.
- Live (debug build, test socket): bash, `vim -u NONE` (insert + Esc), `claude` (trust dialog navigated with
  Down/Enter, typing), `codex` (TUI renders, typing) in orch panes. Full-screen `orch attach` from two herdr
  panes: mirror OK, observer typing ignored, prefix-t takeover resized codex to the new controller, prefix-d
  detach restores the terminal, agent keeps running. `kill -HUP orchd` → keeps running.
- Proxy: `orch --host localhost …` through a stand-in ssh (real `ssh localhost` is refused: DevBox's own key
  is not in authorized_keys — not changed). new/ls/latency/attach over the proxy OK.

Measurements (debug build, DevBox)
- orchd RSS with 5 idle panes (bash, vim, cat, claude, codex): 5.9 MB.
- Full scrollback costs ~13 MB per pane (5000 lines, 80 cols; vt100 cells are big): 59 MB with 9 panes,
  4 of them full. Default scrollback lowered to 3000 (`--scrollback` / `ORCHD_SCROLLBACK`). A compact
  scrollback store is a later optimisation if needed.
- Local: ping 0.2–0.4 ms; attach → first frame 1.8 ms; keystroke → echo frame median 0.8 ms (max 5.2).
- Via proxy pipe: attach 8.7 ms; keystroke → echo median 1.2 ms. Real network numbers need the laptop.

Open / for Murat
- SSH-logout survival not tested with a real logout: orchd is setsid'd, parent 1, KillUserProcesses is the
  Debian default (no) — same situation as herdr's server. Please confirm once: start `orch new -- bash`
  over SSH, log out, log in, `orch ls`.
- Laptop test: `ORCH_REMOTE_BIN=~/.local/bin/orch orch --host devbox attach <pane>` after `scripts/install.sh`.

## 2026-11-02 — Phase 0 GREEN (Claude finished the blocked steps)

Codex's sandbox had no network / home writes, so Claude installed rustup (stable 1.99.1-era cargo, minimal +
rustfmt + clippy, `~/.cargo`, PATH not modified — check.sh sources `~/.cargo/env`) and ran `git init -b main`.
- `cargo fmt --all` (fixed formatting in examples/terminal_spike.rs)
- `scripts/check.sh` → exit 0: fmt ok, clippy -D warnings ok, tests: orch-proto 3 passed, terminal_spike 3 passed.
- Spike A (notes/kabuk-contract.md): complete (source inspection).
- Spike B (notes/terminal-spike.md): synthetic vt100 tests pass; live bash/vim/claude/codex checks moved into
  Phase 1 acceptance (they need the real PTY driver anyway).

## 2026-11-02 — Scope update: Codex stops at Phase 0; validation still BLOCKED

Murat assigned phases 1 and 2 to Claude; Codex will review them later. Do not
start Phase 1 as part of this task. The Phase 0 skeleton, `scripts/check.sh`, and
both spike notes are present. Rechecked `scripts/check.sh` after this scope change:
exit 127 (`Rust is required: install stable rustup with rustfmt and clippy.`).
Rust remains absent and `.git` remains read-only. No build/test or live terminal
spike success can be reported, so Phase 0 is not marked GREEN. Claude's handoff
includes resolving these Phase 0 validation blockers before building on it.

## 2026-11-02 — Phase 0 BLOCKED (not GREEN); Phase 1 NOT STARTED

Read draft-3 HANDOFF/PLAN and accepted engineering defaults. No herdr commands,
socket connections, service changes or agent messages have been made.

- Rust/rustup/cargo are absent from PATH; ~/.cargo and ~/.rustup do not exist.
- Toolchain download probe: `curl --proto '=https' --tlsv1.2 -I --max-time 15 https://static.rust-lang.org/rustup/dist/x86_64-unknown-linux-gnu/rustup-init`
  failed with curl exit 6, `Could not resolve host: static.rust-lang.org`.
- `git init -b main` failed: `.git/hooks/: Read-only file system`. The session's
  permission profile explicitly makes `.git` read-only; commits cannot be made.
- Home-directory writes are also outside this session's writable roots. A normal
  user-level rustup installation needs an environment permitting those writes.

Prepared work:

- Four-crate Cargo workspace, stable/minimal toolchain declaration, explicit binary
  stubs, Tokio daemon entry point, versioned JSON-lines envelope with framing,
  version and size tests. No PTY service is implemented yet.
- `scripts/check.sh` runs fmt, clippy with warnings denied, and tests across all
  targets; holds the shared flock and sets CARGO_BUILD_JOBS=2.
- Spike A complete as source inspection: `notes/kabuk-contract.md` covers the
  watcher, CLI actions, fields, events, errors and reconnect behavior. Found that
  socket redirection alone cannot migrate the phone's hard-coded herdr CLI actions.
- Spike B prepared: vt100 replay example and synthetic mode/Unicode/resize tests.
  `notes/terminal-spike.md` explicitly lists the unrun real-application and query
  reply checks. Emulator verdict remains provisional.

Checks actually run:

- `bash -n scripts/check.sh` — exit 0, shell syntax passes.
- `scripts/check.sh` — exit 127, Rust missing; fmt/clippy/tests did NOT run.
- Python `tomllib` manifest/member inspection — passed for workspace, toolchain
  and all four package manifests (see command below).
- `git init -b main` — exit 1, read-only `.git/hooks/`; no commits possible.
- Download probe above — curl exit 6. No installation was attempted after that
  prerequisite failed. No Cargo.lock could be generated.

Static manifest check command:

```sh
python3 - <<'PY'
from pathlib import Path
import tomllib
root = Path('.')
workspace = tomllib.loads((root / 'Cargo.toml').read_text())
tomllib.loads((root / 'rust-toolchain.toml').read_text())
for member in workspace['workspace']['members']:
    package = tomllib.loads((root / member / 'Cargo.toml').read_text())
    assert package['package']['name'] == Path(member).name
    assert (root / member / 'src').is_dir()
print('PASS: workspace, toolchain, and four package manifests')
PY
```

Continuation needs an execution environment with Rust download/dependency access,
user-level toolchain installation access, and writable repository Git metadata.
Then install stable/minimal + rustfmt/clippy, initialize Git, build/fix/check this
uncompiled scaffold, complete the live terminal spike, and commit small verified
steps. No product decision is pending. Do not mark Phase 0 GREEN merely because
both note files exist. Phase 1 and its live logout/SSH measurements must follow
successful Phase 0 validation.
