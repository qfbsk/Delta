# 提案（未采纳）：不靠提示词让 AI 就范

> 本文是 Delta 设计记录的正式文本。同步自 Qoder 记忆条目 `project-agent-compliance`（脚本 `tools/sync-from-memory.sh`，最近同步 2026-11-08）。


Raised by the user 2026-11-08 as the thing he is most afraid of: *"问题是如何让AI理解我们，我恐怕提示词来告诉AI，AI不一定按我们期望做的。比如他希望他用我们终端，他不知道语法，或者他就喜欢自己写脚本。这个管不住。我想OPENCODE也没有方案。"*

**This file is my analysis and proposal. Nothing in it is decided by the user.** It is here so the argument "prompts will handle it" cannot be made casually later.

## Splitting the problem, because "让 AI 理解我们" is three problems with three different answers

1. **Does it know the verb exists** (discoverability) — *solvable, and not by prose.* The working precedent in our own pool is machine-readable self-description: herdr's `herdr --skill` prints the agent-facing manual and `herdr api schema --json` is self-describing (verified); Fresh ships `--skill` script verbs (verified); MCP has a discovery channel by design. opencode does the same thing at the tool layer — its docs describe custom tools as *"functions you create that the LLM can call during conversations"*, working *"alongside opencode's built-in tools like read, write, and bash"*.
2. **Does it choose to use it** (preference) — *not solvable by instruction, only by cost.* Design criterion I propose, because it is testable: **the cost of routing through delta must be lower than the cost of bypassing us.** If `delta run` takes more thought than `open("x.py")`, agents will script, and no wording in a prompt changes that. Preference follows ergonomics.
3. **What if it disobeys** (compliance) — *solvable only outside language.* Either inside the agent's own runtime (opencode plugins can intercept; a third-party writeup puts the argument well: *"move enforceable rules into plugin hooks that block tool calls before they execute. The model can't 'forget' a hook"* — [nkapila](https://nkapila.me/posts/opencode-config), third-party), or below the tool layer entirely (the OS-identity boundary, still open as [project-delta-status](../laws/status-rollup.md) open item 1).

## The reframe I think actually resolves his example

**"他就喜欢自己写脚本" is not disobedience in a terminal — scripting *is* the terminal idiom.** His own scene says the AI uses the terminal like an editor to test code. So the norm should not regulate the agent's **style**, only its **location**:

> Proposed law: **every execution happens in a window.** Not "every action uses our verbs."

This is enforceable with machinery we already decided — law 10 of [project-window-ownership](../laws/window-ownership.md) already says an event binds exactly one output window — whereas "please use our command syntax" is unenforceable by construction. **Shrink the claim until it can be policed.**

## The hard edge, where "管不住" is simply correct

Rules 2's surveillance covers **file mutation**, because mutation leaves a trace. It cannot cover side effects that leave no file: **the act of executing, outbound network, package installs, system-state changes, and data read *out***. A script the AI wrote is visible; what that script *did* may not be. So the honest position is:

- **The tool layer cannot be the boundary, at any vendor.** Every tool-layer guard only sees down to the layer it recognises, and **a script is the universal currency for getting below the tool layer** — one permitted `bash` call hides an arbitrary program. This is why opencode genuinely has no complete answer either, and it is not an omission on their part.
- **Therefore the only boundary with teeth is below it.** This finally gives [project-delta-status](../laws/status-rollup.md) open item 1 (read-deny vs write-bypass being one OS-identity question) a non-aesthetic reason to exist: make the working tree **unreadable-and-unwritable-from-outside**, so that disobedience becomes **harmless** rather than **illegal**. The promise delta can actually keep is not "the agent will obey us" but **"nothing bad follows from it not obeying."**
- Shape of that, as a proposal: one OS identity/token per agent, the work tree read-only to it (writable only through our bridge), and anything it wants to run must be handed to us for input injection — which is cheap for us because **input injection is the one action where we are necessarily in the path** (see [project-window-ownership](../laws/window-ownership.md) derivations). Egress is strictly more expensive (sandbox + filtering) and is a separate tier, not part of this.

## Errors should teach, not merely refuse

Where prompts fail, a good error + a retrievable schema works, because it changes the *next* attempt rather than the current instruction's memorability. herdr's `agent explain` (it states *why* a state was inferred — verified) is the same principle: **return the machine's judgement to the actor.** A takeover that is refused for a reason the agent can act on — *"this window carries your edits, so it needs consent"* rather than *"permission denied"* — is the version of this that survives the striking of the `AI可写`/`AI可读` flags ([project-window-ownership](../laws/window-ownership.md) law 4); the principle was never about the flag vocabulary.

## Known blind spot, to be documented not solved

Even with all of the above: an agent can do its real work in its own environment and then **copy** the result into the tree. We see the copy (law 2), never the work. Every host on earth has this ceiling. Record it as a blind spot; do not let a later design claim to close it.
