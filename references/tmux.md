# 参考：tmux（已验证）

> 本文是 Delta 设计记录的正式文本。同步自 Qoder 记忆条目 `reference-tmux`（脚本 `tools/sync-from-memory.sh`，最近同步 2026-11-08）。


tmux is delta's named teacher for window management (*"我说了学习tmux的窗口"*, and again on the full-screen-program question: *"我突然发现你说的哪个tmux更好…就原地开就行"*). This file holds only what was read off the manual or an authoritative quote this session, plus an explicit list of what was **not** verified — so "tmux does it that way" is never said without a source.

## Switching — default key table (verified against [`tmux(1)`](https://man7.org/linux/man-pages/man1/tmux.1.html), 2026-11-08)

All of these are **after the prefix** (`C-b` by default):

- Windows: `0-9` select window, `l` last window, `n` next, `p` previous, `w` choose-tree (interactive picker).
- Panes: `o` next pane ("swap pane" style cycle), `C-o`/`M-o` rotate panes, `q` display pane indices, `z` zoom, `!` break the pane out into its own window, `%` split vertically, `"` split horizontally, `C-Up/Down/Left/Right` resize.
- Modes: **copy-mode is entered with `[` and exited with `q` or `Escape`.**
- Movement of topology itself: `(`/`)` move a window to the previous/next session.

**The single most important structural fact for delta: tmux reserves *only the prefix*.** Everything not preceded by it goes verbatim to whatever runs in the pane — that is how a pane running vim keeps its own bindings without cooperation from tmux.

## Why tmux has no global ESC-exits-mode (and our law 13 wants one)

- The manual defines **`escape-time`** as: *"Set the time in milliseconds for which tmux waits after an escape is input to determine if it is part of a function or meta escape sequence"* ([quoted in tmux#3843](https://github.com/tmux/tmux/issues/3843)). Default is 500 ms; the universal advice in the vim-in-tmux community is `set -sg escape-time 0` or `10` because otherwise the ESC key feels delayed in vim ([superuser](https://superuser.com/questions/942677/consequences-of-escape-time-0-tmux-setting), [stackoverflow](https://stackoverflow.com/questions/12312178/tmux-and-vim-escape-key-being-seen-as-and-having-long-delay)). Setting it to 0 trades that delay for the risk of splitting a genuine function/meta sequence ([tmuxai](https://tmuxai.dev/tmux-escape-time/), third-party).
- **tmux's own modes are entered explicitly from the prefix, so ESC inside them is unambiguous.** copy-mode is *entered* by a keystroke (`[`) and only *then* does `Escape` mean "leave". Outside a mode, ESC is the app's byte and tmux does not touch it. **This was the counterexample to delta's law 13** (`normal/insert/replace/agent`, *"esc退出"*) while `insert` was being read as an editor concept. **Resolved 2026-11-08 by [project-window-ownership](../laws/window-ownership.md)'s derivation "Input modes are keyboard-routing states"**: if `insert` means delta hands keystrokes to the pane verbatim, then ESC already belongs to the app there and chrome's exit is the prefix — the same shape tmux chose, reached from delta's own law 16 instead of by copying it. tmux stays the proof that a global ESC-exits-mode is the ambiguous thing to avoid, and that `escape-time` is the price of interpreting an ESC a client already sent.

## Floating panes and stacking order (verified)

`break-pane -W` — the pane *"is lifted out of the tiled layout and made floating"*; `move-pane` accepts a **z-index** where *"zero is the front"*. So z-order over a tiled layout is upstream terminal-camp machinery. **Not verified:** whether tmux skips drawing occluded panes — the "下面的就先不画" half of our law 9 is ours, not tmux's.

## Background windows keep running (verified structurally)

*"Each session is displayed on screen by a client"*, and output in non-current windows is not shown until you switch to them; **`monitor-activity`** flags windows that had activity. This is the precedent for window sleep being *undrawn but live* — occluded ≠ suspended.

## A process that exits does not close the window (verified 2026-11-08)

Directly load-bearing for [project-window-ownership](../laws/window-ownership.md) law 19 (*"nvim有自己的退出，你别推了，窗口还在"*): tmux's default **does** destroy the pane when its command ends — *"When the shell command completes, the window closes."* — and the escape hatch is an option, not a special case: *"See the remain-on-exit window option to change this behaviour."* `respawn-pane` is documented as *"Reactivate a pane in which the command has exited (see the remain-on-exit window option)"*. So **tmux keeps a dead pane's window only when told to**, which means delta's "窗口还在" is a deliberate default choice of ours, not something to cite as inherited from tmux. Verified from [`tmux(1)` on man.openbsd.org](https://man.openbsd.org/tmux.1); a separate fetch of the man7 rendering of the same page failed this session, so the exact wording of the surrounding paragraph should be re-read before being quoted in a design doc.

## Explicitly NOT verified this session — re-read the manual before citing

`alternate-screen` (does tmux itself use DECSET 1049 for its own states), `allow-passthrough`, `pane-exit-action`, mouse-capture bypass keys, and the `exit-enter` family. A fetch for these returned nothing, so any later claim about them must come with a fresh read. **`pane-exit-action` is the one worth checking next**, because it is tmux's per-pane answer to what happens on exit and could be a precedent for delta's "released ⇒ sink to the bottom of the z-order" rule.
