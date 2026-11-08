# 继承语义：控制字节

> 本文是 Delta 设计记录的正式文本。同步自 Qoder 记忆条目 `feedback-terminal-control-bytes`（脚本 `tools/sync-from-memory.sh`，最近同步 2026-11-08）。


When the integrated shell's child process owns the keyboard and screen, the ONLY control chord given special passthrough is Ctrl+Z, which must reach the child as the real byte 0x1A (not the two characters `^Z`, and only while the child runs — the prompt line keeps normal behavior). Everything else is unchanged: Ctrl+V pastes the clipboard into the child verbatim via the normal paste path — never send 0x16, and never rewrite pasted newlines into Enter keystrokes.

**Why:** The user angrily rejected two of my wrong takes: (1) forwarding Ctrl+V as the raw 0x16 byte, which broke paste entirely ("现在ctrl v直接不能粘贴了"), and (2) converting pasted line breaks into Enter ("ctrl v不要处理成回车"). The correction that landed: "只有ctrl z才^Z,其他的不变" — Ctrl+Z alone is the special control-byte case.

**How to apply:** In `handle_insert_key`, inside the `is_running()` ctrl block, match only `z` → `TerminalKey::Control(0x1a)` and `v` → normal clipboard paste (`TerminalKey::Text(text)` verbatim). No other control chord gets passthrough treatment. See also [feedback-shell-cmd-semantics](shell-cmd-semantics.md).
