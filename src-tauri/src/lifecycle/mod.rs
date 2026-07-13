//! 笔记生命周期 actor(P1:骨架+影子内核)。设计文档:
//! docs/superpowers/specs/2026-07-13-voice-notes-lifecycle-actor-design.md
pub mod actor;
pub mod consumers;
pub mod hooks;
pub mod machine;

pub use actor::{spawn, LifecycleHandle};
pub use machine::Cmd;
