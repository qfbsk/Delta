//! Delta 的控制平面：类型化命令、派生状态、有序事件日志。
//!
//! 这一层没有 UI、没有 PTY。法则 1–25 里凡是能被机器检查的部分都在这里，
//! `src/laws.rs` 的测试用法则编号命名，所以违反一条法则会以测试失败的形式出现。

pub mod actor;
pub mod delta;
pub mod event;
pub mod group;
pub mod ids;
pub mod keyboard;
pub mod record;
pub mod window;

pub use actor::{Actor, ActorKind};
pub use delta::{Delta, GroupClosed, LiveEdit, NoOutputWindow, Takeover};
pub use event::{Event, EventState, Subject};
pub use group::Group;
pub use ids::{ActorId, EditId, EventId, GroupId, WindowId};
pub use keyboard::{Esc, InputMode, Keyboard};
pub use record::{LedgerRow, NullRecorder, OpKind, Recorder, RowSubject};
pub use window::{CloseDenial, KeymapOwner, OwnershipMode, Window, WindowKind};

#[cfg(test)]
mod laws;
