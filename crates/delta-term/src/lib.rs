//! **delta-term** —— 字节层。
//!
//! 这一层是 `laws/delta-reasoning.md` *First runnable proof* 说必须先跑通的那一段：
//! cmd / pwsh / vim 在分屏里、每个键与 IME 都对，之后才轮到 AI 层。
//!
//! **写这个 crate 的规矩（AGENTS.md 第 5 条）**：不自己发明实现，也不链接参考实现。
//! 参考代码钉在 `vendor/ghostty-1.4.0/`（git v1.4.0），按文件逐个转写，
//! 连它自己的测试一起带过来。目前转写完的是：
//!
//! - `parse_table` ← `src/terminal/parse_table.zig`
//! - `parser`     ← `src/terminal/Parser.zig`
//! - `utf8`       ← `src/terminal/UTF8Decoder.zig`
//!
//! 还没转写：`stream.zig`（字节进哪条路的判定）、`osc.zig`（OSC 命令类型）、
//! `Page.zig` / `Screen.zig`（格子与屏幕）。在这些之前不手写替代实现。

pub mod parse_table;
pub mod parser;
pub mod utf8;

pub use parse_table::{STATE_COUNT, Transition, table};
pub use parser::{
	Action, Csi, DcsHook, Esc, MAX_INTERMEDIATE, MAX_PARAMS, Parser, State, TransitionAction,
};
pub use utf8::Utf8Decoder;
