# Codex — step 1: your opinion on the plan (no code yet)

Murat wants your honest opinion before anything is built. Read PLAN.md, HANDOFF.md, notes/inventory.md and
notes/research.md (all in /projects/orchtool). Fixed decisions: **Rust**, own stack replacing herdr, own Telegram
bot, name orchtool / CLI `orch`. Everything else is open to challenge.

Write your answer into **/projects/orchtool/CODEX-OPINION.md**, short and concrete:

1. **Verdict** — is the architecture right for Murat (DevBox: 4 cores, 7.8 GB RAM; uses it over SSH from a laptop
   and from the phone app Kabuk; runs several Claude/Codex agents at once)? What would you change?
2. **Biggest risks** and how to cut them (e.g. terminal emulation correctness, diff sync, keeping Kabuk working).
3. **Crate choices** — vt100 vs alacritty_terminal, portable-pty, ratatui, rmcp, SQLite crate, async runtime.
   Recommend one each, with a one-line reason.
4. **Phase order / scope** — should anything move earlier (e.g. agent bus/ask_human before the full TUI, since
   herdr already works as a runtime)? What is the smallest first version that is actually useful to Murat?
5. **Open questions for Murat** — only the ones he really must decide.

Don't install anything, don't write code, don't touch herdr. When CODEX-OPINION.md is written, stop and say
"OPINION DONE". Claude will go through it with Murat, then you get the go for phases 0+1 (HANDOFF.md).
