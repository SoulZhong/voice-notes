//! P3 日历(非 macOS 桩):与 calendar.rs 同形 API。日历匹配是 macOS EventKit
//! 专属能力,其它平台恒 Unavailable/空结果——设置页据此隐藏整个区块,
//! 停止挂钩与 backfill 自然短路,消费方零平台分叉。

include!("calendar_common.rs");

pub fn permission_status() -> Permission {
    Permission::Unavailable
}

pub fn request_permission() -> AuthOutcome {
    AuthOutcome::Error
}

pub fn events_between(_start_ms: i64, _end_ms: i64) -> anyhow::Result<Vec<EventInfo>> {
    Ok(Vec::new())
}
