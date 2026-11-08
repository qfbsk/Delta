# 参考：表决与投票文献池

> 本文是 Delta 设计记录的正式文本。同步自 Qoder 记忆条目 `reference-debate-voting`（脚本 `tools/sync-from-memory.sh`，最近同步 2026-11-08）。


Sources to read before finalizing delta's group decision rules (the rules themselves are the user's design; see [project-delta](../laws/delta-reasoning.md)).

**Parliamentary procedure — the mature analogue.** The user's mechanism maps onto it almost item-for-item: motion / debate / vote / abstain / table-and-postpone (=再议) / tie / chair's casting vote (=人类一票决定). Three directions I am confident about, exact wording to verify against Robert's Rules: a **tie rejects the motion** (status quo preserved — delta deliberately chose the opposite: tie escalates to the human, because "nothing happens" is usually the wrong default here); an **abstention is not a no** and is not counted as a negative; and **debate can be closed early by a procedural motion with a higher threshold** (the motion on the previous question; two-thirds in US practice). That last one is the piece delta's first draft was missing — the user later removed the need by dropping re-votes.

**Multi-agent debate (MAD).**
- [Improving Factuality and Reasoning through Multiagent Debate](https://www.alphaxiv.org/abs/2305.14325) (Du et al.) — the pro side: biography factuality 74% vs 66% single-model.
- [If Multi-Agent Debate is the Answer, What is the Question?](https://www.researchgate.net/publication/388963953_If_Multi-Agent_Debate_is_the_Answer_What_is_the_Question) — questions whether MAD helps at all in many settings.
- [Decomposing Stance Convergence in Multi-Agent LLM Debate](https://arxiv.org/html/2606.00820v2) (2026-10) — **answer changes and convergence alone cannot distinguish genuine belief update from conformity**; load-bearing for delta because the `consensus ≠ truth` law needs exactly that distinction. Countermeasure with a published mechanism: **MAD-M² (memory masking)**, i.e. hide what others said to keep rounds independent — the argument for a blind first round.
- [Multi-Agent Debate for LLM Judges with Adaptive Stability](https://openreview.net/forum?id=Vusd1Hw2D9).

**Correlated votes — the hard ceiling on what a panel proves.** [Correlated Errors Undermine LLM Evaluation Panels](https://machinelearning.apple.com/research/correlated-llm-evaluation-panels) (Kohli et al., 2026-05; [arXiv](https://arxiv.org/html/2605.29800v1)): **roughly three-quarters of a panel's nominal independence is lost because models make the same mistakes on the same items**, so the effective number of independent voters is far below the headcount — even across different vendors. Direct consequence for delta: its chosen architecture (N sessions of the same opencode/model) can be one voter wearing N costumes, so vote UI must show an effective-independence count, not just `5:3`. Related: [Cross-Model Error Entanglement](https://www.howardism.dev/articles/cross-model-error-entanglement) (cautions that "behavioral dependence" and "judge disagreement" are measured on different objects).

**Runaway loops (mostly mooted by the final rules, kept as provenance).** [Uncovering Infinite Agentic Loops in LLM Agents](https://arxiv.org/html/2607.01641v1) is the taxonomy; [How To Prevent Infinite Loops in Multi-Agent Systems](https://neuraltrust.ai/blog/a2a-loop) gives hard turn limits / max hop count / TTL; [Stop the Loop!](https://dev.to/alessandro_pignati/stop-the-loop-how-to-prevent-infinite-conversations-in-your-ai-agents-ekj) gives the redistribution rule; [Agent loops are burning your token budget](https://falconer.com/guides/agent-loops-token-budget/) has the quantitative claim *"a 10-turn loop sends roughly 50× the tokens of a single linear call"*.

**Measurement artifact to keep in mind:** forced-choice bias — an option set with no "no opinion" choice makes a model with no view still cast a vote, which inflates both apparent agreement and the correlation problem above.
