//! **delta-term** —— 字节层。
//!
//! 这一层是 `laws/delta-reasoning.md` *First runnable proof* 说必须先跑通的那一段：
//! cmd / pwsh / vim 在分屏里、每个键与 IME 都对，之后才轮到 AI 层。
//! 所以这里没有窗口、没有事件、没有 AI：只有字节、序列、格子和 scrollback。
//!
//! 验收地板由 `laws/status-rollup.md` 第 26 条定：vttest / esctest2 / xterm ctlseqs /
//! xterm.js 的序列表可以拿来当对照与测试语料，但都不作为依赖。

pub mod grid;
pub mod parser;
pub mod vt;

pub use grid::{Cell, DEFAULT_SCROLLBACK_BYTES, Grid, char_width};
pub use parser::{Action, Csi, Parser};
pub use vt::Vt;
