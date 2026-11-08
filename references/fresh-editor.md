# 参考：Fresh Editor（已验证）

> 本文是 Delta 设计记录的正式文本。同步自 Qoder 记忆条目 `reference-fresh-editor`（脚本 `tools/sync-from-memory.sh`，最近同步 2026-11-08）。


[Fresh Editor](https://github.com/sinelaw/fresh) (`sinelaw/fresh`, site [getfresh.dev](https://getfresh.dev/)) is the only shipping product with the same shape as [project-delta](../laws/delta-reasoning.md)'s ADE goal, so it is the best source for *semantics and hard-problem inventory*. Rust, ~9.4k stars, created 2024-12-24, v0.5.8 released 2026-11-06, repo 144MB with ~356 open issues, ships `aarch64/x86_64-pc-windows-msvc` zips plus a Windows installer and winget.

Worth reading, all verified from its README/release notes:

- **Orchestrator**: "one workspace per git worktree, terminals and long-running commands per workspace, coding agents (claude, codex, opencode, aider) with resumable sessions, dock switcher, remote/SSH workspaces" — i.e. herdr's whole product inside an editor (see [reference-herdr](herdr.md)).
- **Per-directory daemon**: `fresh -a` auto-attach, background editor survives with nothing attached, kitty keyboard protocol forwarded to attached clients, hashed socket names for paths too long for a Unix socket name, half-open client cannot stall the daemon. Its changelog reads like delta's risk register: *"A detached daemon no longer shuts itself down — it used to exit after an hour with no terminal attached, taking running agents with it"* and *"`--wait` against a daemon with nothing attached no longer waits on an invisible editor"*.
- **Terminal panes with the same Ctrl+V problem kite had**: "Ctrl+V in a terminal pane reaches the program when the clipboard has no text, so agent CLIs can paste images; turn off with `terminal.pass_ctrl_v_without_text`".
- **Agent-facing API**: `fresh --skill` prints "the guide to driving a running editor from an embedded terminal: the script verbs, the calling convention, and worked examples"; there is a **Buffers & Editing** verb group that can **apply text edits**, not just open files.
- Full LSP (definition, references, hover, code actions, rename, diagnostics, completion), tabs, split panes, integrated terminal, command palette, multi-gigabyte files, TypeScript plugins in a sandboxed QuickJS.

Limits as a reference: it is **0.x** (fails the user's "copy once, never chase their bugs" criterion), it **has a plugin system** (the machinery they want dropped), and it renders into a **terminal grid**, so delta's GPU bloom/progressive-reveal drawing can borrow semantics only. Its README never names tree-sitter (the marketing site does) — treat "Fresh uses tree-sitter" as unconfirmed.

Not installed locally; inspect via `curl` to `api.github.com` — `raw.githubusercontent.com` and `github.com` HTML fetches are blocked from this machine, but `api.github.com/repos/sinelaw/fresh/readme` works (base64 content).
