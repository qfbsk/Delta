# 法则：群组表决

> 本文是 Delta 设计记录的正式文本。同步自 Qoder 记忆条目 `project-group-decision-rules`（脚本 `tools/sync-from-memory.sh`，最近同步 2026-11-08）。


The group chat's decision mechanism is the **user's own design** (stated, amended and finalized 2026-11-08; nearest mature analogue is parliamentary procedure, see [reference-debate-voting](../references/debate-voting.md)). Nothing here is borrowed from herdr, Fresh, opencode or ACP — ACP defines no roster, no roles and no agent↔agent semantics.

**Rules as finalized:**
1. **人类弃权 = AI 决定** (literal words of the user; unchanged by anything below). **Narrowed 2026-11-08 by Qoder under the user's delegation to pick for them:** on *authority*-changing questions (permission / lease / threshold / tier grants) abstention means nothing changes rather than the AI deciding, because no ballot may authorize those rows at all. See the authorship constraint below; this is the one cost the user may veto.
2. **不需要再次投票** — the earlier "debate, then re-vote as final adjudication" is dropped.
3. **人类选择再议 → 停止，等人类询问** (only the human can reopen).
4. **投票只能人类发起.**
5. **AI 的询问，选项由人类选**；人类再决定是否交由大家议论产生.
6. **每人只发言一次，发言后就投票** — speeches first, then the ballot.
7. **平票 → 人类决定.**
8. **允许自己投自己的票** — the user overrode my recusal proposal: *"这无所谓。因为这是群体，不是只有一个人"*.
9. **AI 不能弃权** — AI options are the substantive ones only.
10. **投票过程中实时不公布结果，投完才公布** — sealed ballot, published at close.
11. **人类弃权且 AI 平票 → 人类自己再投一次**, and in that deciding ballot **the human cannot choose 弃权 but can still choose 再议** — *"只有人类的投票不能选成弃权，但是可以再议。这样就可以保证人类还能继续询问。"* So the deciding option set is **A…Z + 再议, minus 弃权**.
12. **再议 is a human option only** — confirmed by the user's own words on the follow-up turn: *"再议只有人类有"*. This retires the earlier reading that had been inferred from sentence ordering. AI has no 再议, consistent with rule 9.
13. **The voting window is closed by exhaustion, not by a clock**: *"3不能补投，不能不投，投完是所有人都投了就是投完"*. So **no late votes after close, no abstaining-from-voting / no skipping, and 投完 = every invitee has voted**. Publication time is therefore determined by the slowest invitee, not by a timeout — which makes "sealed until everyone has voted" the only place where a stalled agent can hold a ballot open.

**What these achieve structurally (analysis, not new rules):** rules 4+5 put **agenda control with the human**, which answers "whoever authors the option list holds the power". Rules 2+3+6 make **every ballot provably finite** (N speeches + 1 vote, no self-reopening), which retired most anti-runaway controls — hop budgets, no-redistribution and no-self-wake are dead weight, kept in [project-delta](delta-reasoning.md) as provenance. **Rule 10 supersedes my proposed "blind pre-vote as evidence"**: conformity travels on *seeing others' choices*, persuasion travels on *hearing others' arguments*, and speeches-then-sealed-ballot keeps the second while cutting the first. **Rule 11 kills the only non-terminating path** (abstain → tie → re-vote → abstain) while deliberately keeping the human's re-ask exit, so the system parks on a human instead of spinning.

**Constraints that follow and must not be lost:**
- **Secrecy is a rendering policy, not a storage policy.** The ledger still needs, per vote: actor, round, and which speeches it saw — otherwise *consensus ≠ truth* can only be trusted, not recomputed.
- **Sealed rounds are enforceable here and not elsewhere**, because delta is the only postman; agents have no second channel to leak choices (herdr cannot offer this — its agents can print to each other's screens on the same host).
- **Rule 11 creates two different objects that were both called "vote"**: a poll (N recorded opinions, **no authority**) and an adjudication (one human choice, **and this is the record that authorizes a write**). Stored as one row type, the audit chain breaks exactly where the law *persistent facts may only be recorded at the points we enforce* needs it intact. **Decided (chosen by Qoder under the user's delegation *"哪个好选哪个，你替我选"*, 2026-11-08): they are two distinct ledger row types.** No reason not to split them.

**Resolution supplied for the rule-8 / frozen-law collision — chosen by Qoder under the same delegation, so it is decided but reversible:**

**Authorship constraint, not recusal.** Every ledger row that changes some actor's authority — permission grant/revoke, lease grant/revoke, approval-threshold change, capability-tier change — has exactly one legal author kind: **`human`**. A ballot outcome row may never serve as such a row's `authorized_by`. This is a schema constraint enforced by the engine, so it is a mechanism and not a habit (the failure mode of the habit version: "the human simply never puts a permission question to a vote" is unenforceable).

Why recusal was rejected: recusal is per-voter, so it catches "two agents voting for each other's write access" but **misses three agents voting to grant a fourth one access** — nobody in that tally is a beneficiary, yet it is still a self-grant by proxy. The authorship constraint closes that class outright, and it costs less: no recusal machinery, no denominator weirdness against rule 13. **Rule 8 (self-voting) is left fully intact everywhere else.**

**The price, stated so the user can veto it:** rule 1 (**人类弃权 = AI 决定**) is now narrowed to **non-authority questions**. On an authority question, human abstention means *nothing changes*, not *the AI decides*. Keeping rule 1 unconditional requires rejecting this and going back to narrow recusal. Recorded as: narrowed.

What survives the "AI 管理 AI 的权限" future (user: *"听起来很危险，但这是未来"*): agents may still propose, second and argue authority changes; the human's hand is what writes the grant. The real work for that future is making the click cheap — batch grants, time-boxed windows, expiry (herdr's `--source/--seq/--ttl-ms` fenced-authority shape) — not letting a tally authorize.

**Two kinds of write lease (this is what *"写lease，是两种"* is read as — the phrase sat directly after the words 写lease, so it answers the parked-ballot question, not the poll/adjudication one):**
- **held** — normal: the task may write.
- **parked** — the task is blocked on a 再议 ballot: its pane and identity are kept, but **write authority is dropped to read + speak only**, and the group plus the pane must show **who is waiting on this ballot and for how long**. This satisfies the visibility law — which since [project-window-ownership](window-ownership.md) is enforced by *"AI 编辑 ⇒ 必有窗口"* plus **任务优先 (a live task pins its window, so the close is refused)** rather than by the retired rule that closing a pane kills its process. **Amended 2026-11-08: "revoke the lease on close" is no longer how this works** (closing a window does not stop a task), so parked-ness is a property of the task and its windows, not of the window's lifetime.

**Open items (the user's call):**
- **The correlation residual is now precisely bounded**: rule 1 still hands authority to the panel on abstention, and only a tie escalates — but N sessions of one model tend toward unanimity, not ties, so **abstain + homogeneous panel = one decision wearing N costumes, with no escalation** (~¾ of a panel's nominal independence is lost to correlated errors, [reference-debate-voting](../references/debate-voting.md)). No rule change needed; showing an effective-independent-voter count at the moment of abstention is what makes it visible. The authorship constraint above shrinks this exposure to non-authority questions only.
- **Rule 9 forces a preference where none exists** (forced-choice bias). A **confidence/weight field** on each vote recovers the information without creating an abstention, transferring authority, or violating any rule. The user's call.
- Whether **engineering a tie** deserves an event pattern now that a tie escalates rather than preserving the status quo (contrast with Robert's Rules, where a tie rejects the motion).
- **Rule 13 makes a stalled invitee the only way a ballot cannot close** (投完 = all voted, no timeout, no skipping). Whether an agent that never answers a ballot is retried, escalated, or excluded is unspecified.
