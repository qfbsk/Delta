#!/usr/bin/env bash
# 把 Qoder 记忆里的 Delta 设计条目同步到本目录（剥 frontmatter、把 [[wikilink]] 换成本目录的相对链接）。
# 用法：bash tools/sync-from-memory.sh
# 规则：改动先写记忆条目，再跑这个脚本；README.md 与 AGENTS.md 是本目录自己写的，脚本不碰。
set -eu
P="/c/Users/QFBSK/.qoder/projects/E--Think/memory"
U="/c/Users/QFBSK/.qoder/memory"
D="$(cd "$(dirname "$0")/.." && pwd)"

# src|dst|标题
rows="
$P/project-window-ownership.md|laws/window-ownership.md|法则：窗口与所有权
$P/project-delta-status.md|laws/status-rollup.md|清单：已定 / 有据 / 未采纳 / 未定
$P/project-delta.md|laws/delta-reasoning.md|Delta：推理与证据
$P/project-group-decision-rules.md|laws/group-decision-rules.md|法则：群组表决
$P/project-agent-compliance.md|proposals/agent-compliance.md|提案（未采纳）：不靠提示词让 AI 就范
$P/reference-acp.md|references/acp.md|参考：ACP 方法面
$P/reference-debate-voting.md|references/debate-voting.md|参考：表决与投票文献池
$U/reference-tmux.md|references/tmux.md|参考：tmux（已验证）
$U/reference-ghostty.md|references/ghostty.md|参考：ghostty（已验证）
$U/reference-herdr.md|references/herdr.md|参考：herdr（已验证）
$U/reference-fresh-editor.md|references/fresh-editor.md|参考：Fresh Editor（已验证）
$U/reference-opencode.md|references/opencode.md|参考：opencode（已验证）
$P/feedback-terminal-control-bytes.md|carried-semantics/terminal-control-bytes.md|继承语义：控制字节
$P/feedback-shell-ctrl-c.md|carried-semantics/shell-ctrl-c.md|继承语义：shell 的 Ctrl+C
$P/feedback-shell-cmd-semantics.md|carried-semantics/shell-cmd-semantics.md|继承语义：裸 exe 名的解析
"

keys="project-window-ownership project-delta project project-delta-status project-group-decision-rules project-agent-compliance reference-tmux reference-ghostty reference-herdr reference-fresh-editor reference-opencode reference-acp reference-debate-voting feedback-terminal-control-bytes feedback-shell-ctrl-c feedback-shell-cmd-semantics"
vals="laws/window-ownership.md laws/delta-reasoning.md laws/delta-reasoning.md laws/status-rollup.md laws/group-decision-rules.md proposals/agent-compliance.md references/tmux.md references/ghostty.md references/herdr.md references/fresh-editor.md references/opencode.md references/acp.md references/debate-voting.md carried-semantics/terminal-control-bytes.md carried-semantics/shell-ctrl-c.md carried-semantics/shell-cmd-semantics.md"
mem="project-kite-strategy feedback-no-more-kite-precedent feedback-no-stitched-dependencies feedback-user-designs-chrome feedback-scope-discipline feedback-design-discussion-style feedback-no-superseded-wording feedback-component-checklist-not-law-chain reference-delta-docs"

echo "$rows" | while IFS='|' read -r src dst title; do
  [ -z "${src// /}" ] && continue
  {
    printf '# %s\n\n' "$title"
    printf '> 本文是 Delta 设计记录的正式文本。同步自 Qoder 记忆条目 `%s`（脚本 `tools/sync-from-memory.sh`，最近同步 2026-11-08）。\n\n' "$(basename "$src" .md)"
    awk 'BEGIN{s=0} { l=$0; gsub(/[ \t\r]/,"",l)
      if (s==0 && l=="---") { s=1; next }
      if (s==1) { if (l=="---") s=2; next }
      print }' "$src"
  } > "$D/$dst"
done

cd "$D"
for f in $(find laws proposals references carried-semantics -name '*.md'); do
  d=$(dirname "$f"); set -- $vals; i=1
  for k in $keys; do
    t=$(eval echo \$\{$i\}); i=$((i+1))
    if [ "$(dirname "$t")" = "$d" ]; then p=${t#*/}; else p="../$t"; fi
    sed -i "s|\[\[$k\]\]|[$k]($p)|g" "$f"
  done
  for k in $mem; do sed -i "s|\[\[$k\]\]|记忆条目 $k，本目录未收录，规则见 ../AGENTS.md|g" "$f"; done
done

echo "同步完成。残留 wikilink（应为空）："
grep -roh '\[\[[a-z-]*\]\]' --include='*.md' laws proposals references carried-semantics | sort -u || true
