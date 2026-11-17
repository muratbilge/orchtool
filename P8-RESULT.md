# P8 result: orchd deadlock fix + hang watchdog

Branch `claude/p8-deadlock` (from main 4a07ec7), worktree `.worktrees/opus`. Done by Claude
(Opus) standing in for Codex. Local commits only; nothing merged, pushed, installed or restarted.

## What changed and why

1. **Deadlock fix** (the proven one)
   - `server.rs` `Request::ListPanes`: now `self.pane_infos()` (snapshot via `all_panes()`, then
     `info()` per pane with no lock held). Before, `info()` → `limits::info` / `auto_continue`
     took the limits `state` while `Daemon.panes` was held.
   - `limits.rs` `check()`: `daemon.all_panes()` is taken *before* `state().lock()`. Before, it
     took `panes` while holding `state` → opposite order to ListPanes → both runtime workers
     parked on futexes forever.
   - `server.rs` `kill_all`: iterates `all_panes()` instead of holding `panes` while locking each
     pane's `killer` (not an inversion, but keeps `panes` a true leaf).
   - Lock order documented in one comment on `struct Daemon`.
2. **Watchdog** (`crates/orchd/src/watchdog.rs`, started from `main.rs` right after the signal
   streams): a tokio interval bumps an `AtomicU64` heartbeat every 1 s (stall/5 when a test sets a short threshold); a plain std thread
   (`orchd-watchdog`) checks every 250 ms. Heartbeat older than 15 s (`ORCHD_WATCHDOG_STALL_MS`
   overrides) → one stderr line `orchd: runtime not responding for N s (possible deadlock)`,
   repeated at most once a minute; `orchd: runtime responding again` once it recovers.
   - **Stuck runtime + SIGTERM/SIGINT**: an extra action registered through
     `signal-hook-registry` (already in Cargo.lock as tokio's dependency; now a direct dep of
     orchd) only sets an `AtomicBool`; tokio's own handler still runs. If that flag has been
     pending for 5 s **and** the runtime is stuck, the thread logs
     `orchd: stop signal not handled for N s, runtime stuck: exiting without the final save` and
     calls `_exit(1)`. A healthy runtime never meets the "stuck" condition, so the normal stop
     path (final autosave ≤ 5 s, kill_all, exit 0) is unchanged — covered by a test.
   - Test hook: `ORCHD_TEST_STALL_RUNTIME_MS` blocks both workers for that long one second after
     start; compiled only with `debug_assertions` (absent from release builds).
3. **Tests** `crates/orchd/tests/hang.rs`:
   - `list_panes_racing_the_limit_check_never_hangs`: private orchd with
     `ORCHD_LIMIT_CHECK_MS=1`, 6 `sleep 600` panes, 3 concurrent connections × 700 ListPanes
     (2100 requests), 5 s timeout each; then ping, SIGTERM, must exit within 10 s and log
     `orchd: stopping`.
   - `a_stuck_runtime_is_logged_and_its_recovery_too` (debug only): stall 4 s, threshold 1.5 s →
     exactly one "not responding" line, then "responding again", daemon answers again.
   - `sigterm_ends_orchd_even_with_a_stuck_runtime` (debug only): stall 60 s, threshold 1.5 s, SIGTERM → exit
     code 1 within 15 s with the "stop signal not handled" line.
   - `a_healthy_sigterm_stops_normally`: threshold 3 s, SIGTERM on a healthy daemon → exit 0, "orchd: stopping",
     no watchdog line.

## Lock-pair audit (crates/orchd/src, std mutexes + watch borrows)

| Held → taken | Where | Verdict |
|---|---|---|
| `panes` → limits `state` (via `Pane::info`) | server.rs ListPanes | **Inversion with the next row — the deadlock. Fixed** (snapshot first). |
| limits `state` → `panes` (`all_panes()`) | limits.rs `check` phase 1 | **Fixed**: panes taken before `state`. |
| limits `state` → `Pane.term` → `Pane.kind` (`pane.agent`) | limits.rs `check` (phase 1 via `visible_text`/`agent`; phases 2/3 explicit) | OK: nobody takes `state` under `term`/`kind`. Documented order. |
| limits `state` → `term` → exit watch read, `pane.write` (mpsc, no lock) | limits.rs skip_updates / keep_models | OK. |
| `Pane.term` → `Pane.kind` | pane.rs `detect()`/`info` are sequential; tasks.rs dispatch (`term` held, `p.agent()`); limits phases 2/3 | OK, one direction only. |
| `Pane.term` → `Pane.master`, controller watch read | pane.rs `resize_inner` | OK: `master` only locked there. |
| `Pane.term` → `answered` | server.rs `Send` with `expect` | OK: `answered` only locked there. |
| `panes` → exit watch read | server.rs `new_pane` name check | OK (watch read is a leaf). |
| `panes` → `Pane.killer` | server.rs `kill_all` | Not an inversion; changed to `all_panes()` anyway. |
| `session` → (edit closures) | ui.rs `edit` | OK: closures only touch the `Session` value. CloseTab/CloseWorkspace drop `session` before `close_panes`. |
| `session` (clone) then `panes`/`term`/`kind`/`session_hint`/bus `db`, each alone | restore.rs `snapshot` (used by autosave) | OK: every lock is taken and released on its own. |
| `autosave.state` → file IO only | autosave.rs `write`, `last_written`, `forget` | OK: leaf. `autosave_now` calls `snapshot` before `write`, `last_written` before `write` — never nested. Runs on `spawn_blocking`, not a worker. |
| bus `db` → broadcast `send` (`emit`) | bus.rs | OK: tokio broadcast's internal lock is never held across anything. |
| tasks_store `db` | tasks_store.rs | OK: leaf. |
| `task_gate` (tokio) → std locks | tasks.rs, server.rs TaskCancel/Done, main.rs stop | OK: std locks never held across `.await` (clippy `await_holding_lock` is denied under `-D warnings`). |
| watch `borrow()` across calls/awaits | server.rs attach loop, pane.rs | OK: only temporaries; no borrow held across an await or a lock. |

No other inversion found.

## Test evidence

- Fails before the fix (fix reverted in `server.rs` + `limits.rs`, watchdog kept; checked
  twice, with 3×1500 and with the final 3×700 requests):
  `cargo test -p orchd --offline --test hang list_panes` →
  `ListPanes request 0 got no answer in 5 s: orchd hangs` /
  `test result: FAILED. 0 passed; 1 failed`.
- Real deadlock + watchdog, manual, private daemon only (unfixed binary copy in the scratchpad,
  `ORCHD_LIMIT_CHECK_MS=1 ORCHD_WATCHDOG_STALL_MS=3000`, 6 panes, `orch --socket $D/s.sock ls`
  hung on the first call, then `kill -TERM <pid>`): exit status 1, log:
  ```
  orchd: runtime not responding for 3 s (possible deadlock)
  orchd: stop signal not handled for 5 s, runtime stuck: exiting without the final save
  ```
  No `sleep 600` pane processes left afterwards (PTY close → SIGHUP).
- With the fix: `cargo test -p orchd --offline --test hang` → `4 passed`.
- Repeat runs of `list_panes_racing_the_limit_check_never_hangs`: see "Final runs" below.

### Final runs

- First version of the watchdog tests had a 1 s threshold with a fixed 1 s heartbeat:
  `a_healthy_sigterm_stops_normally` failed 2 of 10 runs (a spurious "not responding" line
  under load). Fixed by beating at `min(1 s, stall/5)` and using 1.5 s / 3 s test thresholds.
- Final code, `for i in $(seq 1 10); do cargo test --offline -p orchd --test hang; done` →
  10 × `test result: ok. 4 passed; 0 failed` (11.4–13.6 s each).
- The deadlock test alone ran 12 more times green earlier (3×1500 and 3×700 variants).
- `scripts/check.sh` (fmt --check, clippy --workspace --all-targets -D warnings, build, test
  --workspace --all-targets) → exit 0, 13 test binaries `test result: ok`, none failed.

## Not done / choices

- No timeout on the runtime's shutdown: if a hung `spawn_blocking` job (e.g. a stuck final
  snapshot) keeps the runtime's drop waiting, the watchdog now ends the process after the
  heartbeat goes stale (15 s + 5 s grace) instead of hanging forever. Not changed further.
- The watchdog exit skips the final autosave and does not kill panes explicitly; the panes get
  SIGHUP when the PTY masters close, and the last periodic autosave (≤ 60 s old) is what the
  next orchd restores.
- The watchdog cannot tell *which* locks are stuck; for that, `gdb -p <pid> -batch -ex 'thread
  apply all bt'` on the hung process is still the tool.

## Risks for the planned restart

- The running orchd has the old code: it can still deadlock until it is replaced. Its stop path
  may then ignore SIGTERM (old binary, no watchdog) — `stop-daemon --restart` needs a responsive
  runtime; if it hangs, only `kill -9` works and the layout comes back from `autosave.json`.
- New dependency edge: `signal-hook-registry` is now a direct dependency (same version 1.4.8 tokio
  already uses; no new crate downloaded).
- An orchd whose runtime is legitimately busy > 15 s will log a "not responding" line (harmless);
  a SIGTERM during such a > 20 s stall exits with status 1 without the final save.
