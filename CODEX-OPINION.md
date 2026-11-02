# Codex opinion

1. **Verdict**

The direction fits Murat: a small Rust daemon owning PTYs, detachable SSH clients, a durable communication store, and an independent Telegram bot. Keep those boundaries and the fixed decisions. The draft front-loads terminal infrastructure while postponing its most useful addition: agents asking Murat questions he can answer anywhere.

Separate terminal state from durable messages/questions, even within one daemon. Keep the protocol versioned and the terminal implementation behind an adapter. Start with framed JSON; defer binary encoding until measurements justify it. SSH screen sync can reduce redraw traffic, but it cannot promise mosh's roaming or eliminate TCP head-of-line blocking. “Faster than herdr” needs a measured comparison on Murat's actual connection.

The <50 MB RSS target is an experiment, not an acceptance promise. Ten panes × 10,000 lines × 120 columns is 12 million cell positions before metadata. Bound scrollback bytes, client caches, queues and history retention; measure idle and populated scrollback separately, with child-agent memory reported separately. Keep the build lock and two-job limit.

2. **Biggest risks and how to cut them**

- **Terminal correctness:** validate bash, vim, Claude and Codex before committing to an emulator. Cover alternate screens, Unicode width/combining characters, cursor and input modes, bracketed paste, mouse input, terminal query replies and resize. Rendering cells alone does not implement a terminal; advertise only supported capabilities. Capture regression fixtures from failures.
- **Diff correctness and backpressure:** never discard a delta that later deltas depend on. Include pane generation, dimensions, base revision and resulting revision; resnapshot after a skipped base or resize. Coalesce unsent state, bound outgoing bytes, and reconnect/resnapshot a persistently slow client. Bytes already inside SSH/TCP cannot be replaced. Start around 20–30 fps and measure. Test skipped frames, reconnects and resize races, not just two clients receiving identical deltas.
- **Kabuk compatibility:** method names alone are insufficient. Inspect its actual watcher/client code and capture fixtures for framing, envelopes, IDs, fields, errors, events and reconnect behavior before fixing the API. The inventory establishes an SSH watcher, not a working WebSocket contract. Implement only the required compatibility surface on orchd's own endpoint; make phone acceptance a migration gate.
- **Lifetime and ownership:** detach survival is achievable; daemon restart plus agent resume does not preserve running shells, tools or approvals. Initially make restarts explicit and report interrupted work rather than silently relaunching it. Use one controlling client for input/resize, with explicit takeover; phone observers should not shrink the laptop terminal. Preserve the last size when detached. Verify user-service survival after SSH logout.
- **Question/delivery semantics:** persist questions before notifying, use stable IDs and an atomic first-answer transition, and retain answers across reconnects. A blocking MCP call needs cancellation/timeout handling and a way to retrieve or wait again on the same question. Prefer explicit inbox consumption; guessed “idle” is too weak a basis for injecting text into a pane. Screen-detected prompts remain suggestions until confirmed. File leases are advisory, not protection against conflicting edits.
- **External answers:** bind Telegram replies to question IDs and an explicitly configured user/chat allowlist. Duplicate or late replies must not resume work twice. Restrict local socket access; avoid automatically forwarding whole terminal contents.

3. **Crate choices**

These are recommendations, not benchmark results; primary crate documentation was checked for this review.

| Area | Recommendation and reason |
|---|---|
| Terminal | [vt100](https://docs.rs/vt100/latest/vt100/) first: its in-memory screen API directly fits this prototype; switch to [alacritty_terminal](https://docs.rs/alacritty_terminal/latest/alacritty_terminal/) if the compatibility spike exposes material gaps, rather than growing our own emulator. |
| PTYs | [portable-pty](https://docs.rs/portable-pty/latest/portable_pty/): reusable PTY abstraction from WezTerm; isolate blocking reads from async workers and test process cleanup. |
| TUI | [ratatui](https://docs.rs/ratatui/latest/ratatui/) with Crossterm: suitable layout/rendering tools for workspace chrome; keep terminal emulation separate. |
| MCP | [rmcp](https://docs.rs/rmcp/latest/rmcp/): official Rust MCP SDK; start with stdio and test the real clients' long-wait behavior early. |
| SQLite | [rusqlite](https://docs.rs/rusqlite/latest/rusqlite/): direct SQLite integration suits this small local store; use a dedicated database worker, transactions and migrations. |
| Async | [Tokio](https://docs.rs/tokio/latest/tokio/): sockets, channels and timers fit the daemon; keep blocking PTY/database work off its scheduler and enable only needed features. |

4. **Phase order / smallest useful version**

Revise HANDOFF before implementation; its current phase 1 already bundles several difficult subsystems.

- Phase 0: skeleton plus Kabuk contract inspection and terminal/resource feasibility checks.
- Phase 1: one-pane PTY lifecycle, bounded scrollback, local attach, snapshot/diff recovery and SSH proxy. Prove both agent CLIs, detach survival and observer/controller behavior before layouts.
- Next: durable bus, MCP inbox, resumable ask_human, CLI timeline/answer and minimal Telegram delivery/replies. Do this before the full TUI. Existing agents can consume the MCP service while herdr remains their runtime, without modifying herdr or making it a dependency of the replacement.
- Then: Kabuk compatibility and reliable agent status; laptop workspace TUI and remote performance tuning; finally task automation and native Kabuk Comms. Retire herdr only after both laptop and phone workflows pass.

The smallest useful release is a durable shared timeline and question inbox: two existing agents exchange messages, one asks Murat, he answers by SSH CLI or Telegram, and a reconnect neither loses the question nor applies its answer twice. Full pane layouts, binary transport, local echo, automatic continuation and file leases can wait.

5. **Only decisions Murat needs to make**

- Is explicit interruption/recovery on daemon restart acceptable initially, or must live processes survive daemon upgrades? I recommend accepting interruption initially; seamless survival changes the process architecture.
- May question text leave DevBox via Telegram, and should the destination be a private chat or a group? I recommend a private chat with concise questions and no automatic terminal transcript forwarding.

Everything else above can proceed as an engineering default once implementation is authorized.
