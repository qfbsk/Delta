# 参考：ACP 方法面

> 本文是 Delta 设计记录的正式文本。同步自 Qoder 记忆条目 `reference-acp`（脚本 `tools/sync-from-memory.sh`，最近同步 2026-11-08）。


[Agent Client Protocol](https://agentclientprotocol.com/get-started/introduction) — open standard initiated by Zed Industries, JSON-RPC over stdio (local) with remote support still work-in-progress. It is the mechanism that lets delta host **official** agents while still owning the write path (see [project-delta](../laws/delta-reasoning.md)). Read the spec before designing anything agent-facing; do not treat it as our invention and do not depend on it — adapter layer only, per 记忆条目 feedback-no-stitched-dependencies，本目录未收录，规则见 ../AGENTS.md.

**Agent implements (we call):** `initialize` `authenticate` `session/new` `session/prompt` `session/load` `session/set_mode` `logout`.
**Client implements (the agent calls us):** `session/request_permission` (**per-request prompts only**), `fs/read_text_file`, **`fs/write_text_file`**, `terminal/create|output|release|wait_for_exit|kill`, `elicitation/create`.
**Notifications:** `session/update` carrying `agent_message_chunk`, `agent_thought_chunk`, `tool_call` / `tool_call_update` (`toolCallId`, `title`, `kind`, `status: pending|in_progress|completed|failed`, `locations` with path/line, diffs, terminals) and **`plan`**; `session/cancel`; `elicitation/complete`.

**What ACP does NOT define (all of it is ours):** persistent per-path read/write ACL, roles, approval *policy*, multi-agent roster, workspace/group object model. Its unit is one agent = one child process = one session.

**Capability gating law (quoted, [file-system](https://agentclientprotocol.com/protocol/file-system) + [terminals](https://agentclientprotocol.com/protocol/terminals)):** the client declares `fs.readTextFile`, `fs.writeTextFile` and `terminal` inside the `initialize` request. *"Before attempting to use filesystem methods, Agents MUST verify that the Client supports these capabilities… If `readTextFile` or `writeTextFile` is `false` or not present, the Agent MUST NOT attempt to call the corresponding filesystem method."* Identical shape for terminals: *"If `terminal` is `false` or not present, the Agent MUST NOT attempt to call any terminal methods."*
- **Read this as a one-way prohibition, not a guarantee.** It forbids calling the host when the host is absent; **nothing in ACP obliges an agent to use these methods when the host *is* capable.** So "we are in the write path" is true only for the writes the agent chooses to route through us — the load-bearing consequence for delta is in [project-delta](../laws/delta-reasoning.md).
- **No terminal-vs-shell preference sentence exists** in the spec — verified by asking directly, so do not claim one.
- `terminal/release` is **required to free the process**; `terminal/kill` terminates but keeps the id, so an unreleased terminal leaks.

**Agents speaking ACP (from the official agents list):** Gemini CLI, OpenCode, Qwen Code, Kimi CLI, Hermes Agent **native**; Claude Code via Zed's SDK adapter; Codex CLI via an official ACP adapter; DeepSeek/GLM/Groq via `acp-llm-adapter`. Zed's External Agents docs describe installing agents (e.g. OpenCode) from an **ACP Registry**.

Two operational facts worth copying: Zed ships a raw protocol inspector (`dev: open acp` in the command palette) — the standard escape hatch when a native CLI capability has no ACP representation; and its real gap against opencode is tracked as zed #59566 *"Opencode ACP not show all available commands"*, i.e. unified rendering currently shows *less* than the vendor CLI.
