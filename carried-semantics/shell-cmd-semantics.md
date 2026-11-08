# 继承语义：裸 exe 名的解析

> 本文是 Delta 设计记录的正式文本。同步自 Qoder 记忆条目 `feedback-shell-cmd-semantics`（脚本 `tools/sync-from-memory.sh`，最近同步 2026-11-08）。


A bare program name typed or prefilled in the shell must run a file of that name in the shell's current directory before falling back to PATH.

**Why:** After building `main.exe` in `X:\c`, typing `main.exe` failed with "系统找不到指定的文件" while `.\main.exe` would work. The user called this out as behavior wrongly learned from cmd: cmd does not require `./`, and neither should this shell.

**How to apply:** When resolving external commands, check `<cwd>/<token>` first, then PATH (`gcc`, `cmd` must still resolve). Keep prefilled run commands as bare `{stem}.exe`. See also [feedback-terminal-control-bytes](terminal-control-bytes.md).
