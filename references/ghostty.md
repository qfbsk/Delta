# 参考：ghostty（已验证）

> 本文是 Delta 设计记录的正式文本。同步自 Qoder 记忆条目 `reference-ghostty`（脚本 `tools/sync-from-memory.sh`，最近同步 2026-11-08）。


Ghostty is the terminal emulator Mitchell Hashimoto shipped as 1.0 in early 2025; delta's user named it on 2026-11-08 as the aesthetic to learn, together with tmux, and explicitly excluded kite (*"之前kite完全别学。太丑了，学习ghosty，tmux的美学"*).

## Verified option names and semantics (official [Option Reference](https://ghostty.org/docs/config/reference), read 2026-11-08)

- **`unfocused-split-opacity`** — *"The opacity level (opposite of transparency) of an unfocused split. Unfocused splits by default are slightly faded out to make it easier to see which split is focused."* So ghostty's answer to "which split is mine" is **dimming, not hiding**. Delta's window-sleep law is *stronger* (occluded ⇒ not drawn at all), which means the fade is available as the visual language for the layer stack without weakening the performance rule: faded-but-drawn is ghostty's, unsleeped-but-sunk is ours.
- **`background-opacity`** — *"A value of 1 is fully opaque and a value of 0 is fully transparent."* (Also `window-theme-variant`-style platform theming exists in the same family; not re-verified this session.)
- **`minimum-contrast`** — *"The minimum contrast ratio between the foreground and background colors. The contrast ratio is a value between 1 and 21."* This is the option that makes translucency safe rather than pretty-but-illegible — worth carrying as a principle, not just a knob: **any background we choose must be able to enforce a contrast floor on the glyphs drawn over it**, including AI highlight overlays.
- **`window-decoration`** — *"Configure a preference for window decorations. This setting specifies a _preference_; the actual OS, desktop environment, window manager, etc. may override this preference."* Note the wording discipline: it declares a preference and admits defeat. That is the correct posture for a terminal on Windows, where the chrome is partly the OS's.
- **`resize-overlay`** — *"Resize overlays are a transient popup that shows the size of the terminal while the surfaces are being resized."* Directly relevant to our minimum-pane-size contract: show dimensions during the gesture instead of reserving chrome for them permanently.

## Version notes seen, not verified in depth

- 1.2.0 release notes describe a *"simple, basic progress bar at the top of the terminal"* with tabs to carry progress later — a precedent for surfacing long-running task state without a status panel.
- 1.3.0 release notes: *"we use a minimal overlay-style scrollbar so that the terminal grid doesn't have to reserve space for the scrollbar"* — the same money-the-column idea herdr exposes as `pane_scrollbars = false`.

## Not to import

Ghostty has no file tree, no project object, no agent concept, and no permission model; its aesthetic vocabulary is about **a grid in a window and nothing else**. Anything we build that looks like a sidebar is by definition not learned from here.
