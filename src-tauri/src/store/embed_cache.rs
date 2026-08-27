//! 段落声纹缓存(2026-08-22-embedding-cache-design.md):算过一次,永不重算。
//!
//! 纯缓存语义:文件可整删,丢了就重算;读损坏 = 当无缓存。命中要求
//! (seq, start_ms, end_ms, source) 与当前段完全一致且 model 与当前声纹模型一致
//! ——凡影响切片的变更(重转写换段/时基重投影/换模型)自动失效,无显式失效逻辑。
//! 并发:唯一读写方 embed_all 的两类调用方(Aing 重聚类统计/拆分分组)都持
//! FEEDBACK_GATE,天然串行;写入 tmp+rename 原子替换。

use serde::{Deserialize, Serialize};
use std::path::Path;

pub const EMBED_CACHE_FILE: &str = "embeddings.json";
const SCHEMA_VERSION: u32 = 1;

#[derive(Serialize, Deserialize, Clone)]
pub struct EmbedCacheEntry {
    pub seq: u64,
    pub start_ms: u64,
    pub end_ms: u64,
    pub source: String,
    pub vec: Vec<f32>,
}

#[derive(Serialize, Deserialize, Default)]
pub struct EmbedCache {
    pub schema_version: u32,
    /// 声纹模型标识:与当前设置不符则整份失效。
    pub model: String,
    pub entries: Vec<EmbedCacheEntry>,
}

/// 读缓存:不存在/损坏/版本不符 → None(重算,不报错)。
pub fn load(note_dir: &Path) -> Option<EmbedCache> {
    let raw = std::fs::read(note_dir.join(EMBED_CACHE_FILE)).ok()?;
    let c: EmbedCache = serde_json::from_slice(&raw).ok()?;
    (c.schema_version == SCHEMA_VERSION).then_some(c)
}

/// 增量合并写入(录音期预热用,issue #164):按 (seq,start,end,source) 键 upsert,
/// 不修剪既有条目(全量修剪仍归 embed_all 的整写)。盘上 model 与本批不符时整份
/// 弃旧起新——缓存文件只有一个 model 位,混模型无意义。
pub fn merge_save(
    note_dir: &Path,
    model: &str,
    new_entries: Vec<EmbedCacheEntry>,
) -> anyhow::Result<()> {
    let mut entries = load(note_dir)
        .filter(|c| c.model == model)
        .map(|c| c.entries)
        .unwrap_or_default();
    for ne in new_entries {
        match entries.iter_mut().find(|e| {
            e.seq == ne.seq
                && e.start_ms == ne.start_ms
                && e.end_ms == ne.end_ms
                && e.source == ne.source
        }) {
            Some(slot) => *slot = ne,
            None => entries.push(ne),
        }
    }
    save(note_dir, model, entries)
}

/// 原子写回。缓存可丢,不做父目录 fsync(与真值文件的持久性等级刻意区分)。
pub fn save(note_dir: &Path, model: &str, entries: Vec<EmbedCacheEntry>) -> anyhow::Result<()> {
    let c = EmbedCache { schema_version: SCHEMA_VERSION, model: model.to_string(), entries };
    let path = note_dir.join(EMBED_CACHE_FILE);
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, serde_json::to_vec(&c)?)?;
    std::fs::rename(&tmp, &path)?;
    Ok(())
}
