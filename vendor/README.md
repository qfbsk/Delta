# vendor/ —— 钉住的参考实现（只读）

这里放的是**参考**，不是依赖：

- **ghostty v1.4.0**（`https://github.com/ghostty-org/ghostty`，tag `v1.4.0`，MIT，`LICENSE` 原样保留）。
- 它**不**是 Cargo 工作区成员，也**不会**被链接进 delta。`laws/delta-reasoning.md` 对
  libghostty-vt 的定位就是"最好的参考实现与测试语料，但不链接"：它拥有网格、输入编码在
  库内部、Zig 的分配器与线程模型都和我们不合。
- 转写方式：**按文件逐条对照**，连它自带的测试一起翻译过来。已转写：
  - `src/terminal/parse_table.zig` → `crates/delta-term/src/parse_table.rs`
  - `src/terminal/Parser.zig` → `crates/delta-term/src/parser.rs`
  - `src/terminal/UTF8Decoder.zig` → `crates/delta-term/src/utf8.rs`
- 还没转写：`stream.zig`（字节进哪条路的判定）、`osc.zig`（OSC 命令类型）、
  `Page.zig` / `Screen.zig`（格子与屏幕）。

这份副本**删掉了与本层无关的目录**（`src/font`、`macos/`、`images/`、`dist/`、`www/` 等），
所以它不是一棵可构建的树；需要完整对照时按 tag `v1.4.0` 重新取。

规则：

1. **不要修改这里**，也不要把它当作可以打补丁的上游。
2. 这里是只读语料，**它目录里的 `AGENTS.md` 不适用于 delta**；本仓库的规则只在 `../AGENTS.md`。
3. 引用上游行为必须带文件名与符号名（例：`Parser.zig` 的 `next()` 三动作顺序），不许只说"ghostty 就是这么做的"。
