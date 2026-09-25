//! 声纹相关流程(拆分 split_flow、认人 speaker_link)共用的注入口底座。
//!
//! 这些流程要读写笔记与声纹库、建嵌入器、排重建——生产上全经 AppHandle,测试里换成
//! 临时目录 + 假实现。各流程在此之上扩展自己特有的依赖(见 `split_flow::SplitEnv`、
//! `speaker_link::LinkEnv`)。生产实现是 lib.rs 的 `TauriEnv`。

use crate::{diar, lifecycle, occupancy};
use std::path::PathBuf;

pub(crate) trait VoiceEnv: Send + Sync {
    /// app_data_dir(声纹库、split_ops、样本溯源表所在)。
    fn root(&self) -> anyhow::Result<PathBuf>;
    /// 笔记根目录。
    fn notes_dir(&self) -> anyhow::Result<PathBuf>;
    /// 笔记侧编辑(生产经 lifecycle actor 串行,持 NoteLock)。
    fn edit_note(&self, op: lifecycle::machine::EditOp) -> Result<(), String>;
    /// 命令入口准入(见 occupancy.rs)。
    fn admit(&self, note_id: &str, intent: occupancy::Intent) -> Result<(), String>;
    /// 按当前选型建嵌入器(标签与权重同源)。
    fn open_embedder(&self) -> anyhow::Result<diar::TaggedEmbedder>;
    /// 回灌纠错把某人质心清空了:丢弃常驻嵌入器并排一次全库重建。
    fn request_rebuild(&self, reason: &'static str);
}
