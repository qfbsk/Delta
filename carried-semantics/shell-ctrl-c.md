# 继承语义：shell 的 Ctrl+C

> 本文是 Delta 设计记录的正式文本。同步自 Qoder 记忆条目 `feedback-shell-ctrl-c`（脚本 `tools/sync-from-memory.sh`，最近同步 2026-11-08）。


Ctrl+C in the integrated shell routes by selection first, not by whether a child is running:

- **A selection exists → copy**, always. This holds even while a child owns the screen. This deliberately REVERSES the earlier design (old app.rs comment: "a leftover selection must never be able to swallow the one way out of a running program") — do not "restore" that guard.
- **No selection → interrupt**, and it must always echo a lone `^C` line, in all three cases: a child that answers the byte (exit timing unchanged), a stuck child that never reads input, and an idle shell with nothing running (idle has no other effect but still prints `^C`).
- A child that ignores the 0x03 byte (e.g. a hung `gcc`/`ping` that reads no keyboard) must still be closable with the same Ctrl+C: after a grace period (`INTERRUPT_GRACE`, 1000ms in shell.rs) `poll` force-kills it via `kill_job`. A child that answers the byte exits on its own well inside the grace, so its timing is untouched.

**Why:** ConPTY neither echoes the 0x03 byte nor turns it (or `GenerateConsoleCtrlEvent`) into a real signal, so without kite drawing `^C` the interrupt is silent, and a non-input-reading child would sit out the byte forever. The user asked for copy-on-selection to win, a visible `^C` on every interrupt, and stuck children to die by the same key.

**How to apply:** app.rs `"c" | "C"` chord checks `selection_text(ui)` first → `clipboard_copy`; else `shell.interrupt(system)`. shell.rs `interrupt` calls `echo_interrupt` (running: write `^C` straight to the grid on its own row; idle: `push_line("^C", Echo)`) then, if running, sends 0x03 and arms `interrupt_at`. `poll` enforces the grace and clears `interrupt_at` whenever the child is gone; `spawn_external` clears it too. **Scope:** this describes **kite**, which is frozen as of 2026-11-07. For **delta** the user chose herdr's model instead of ours — `copy_on_select = true` so a mouse selection copies immediately and Ctrl+C is never taken from the child, with all app shortcuts behind a prefix or limited to ctrl+letter / function keys / explicit multi-modifier chords. Do not carry kite's selection-first rule into delta design. See [project-delta](../laws/delta-reasoning.md), [reference-herdr](../references/herdr.md).

See also [feedback-terminal-control-bytes](terminal-control-bytes.md).
