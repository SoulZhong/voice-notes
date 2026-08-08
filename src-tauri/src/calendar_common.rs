// P3 日历:平台无关的类型与纯逻辑。本文件被 calendar.rs(macOS)与
// calendar_stub.rs(其它平台)include!,保证两侧对外接口完全同形;
// 纯函数测试也在此,双平台都跑。

/// 日历读取授权态。macOS 13 的 Authorized 与 14+ 的 FullAccess 同值,统一映射 Full;
/// WriteOnly(14+ 只写授权)读不了事件但≠用户拒读,前端单列「权限不足」文案;
/// Restricted(家长控制等)视同 Denied;非 macOS 恒 Unavailable。
#[derive(Debug, Clone, Copy, PartialEq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Permission {
    Full,
    WriteOnly,
    Denied,
    NotDetermined,
    Unavailable,
}

/// 授权请求结果:区分用户拒绝/权限不足/系统错误/超时——前端文案不同,
/// 不能把系统错误误报成「用户拒绝」。
#[derive(Debug, Clone, Copy, PartialEq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AuthOutcome {
    Granted,
    Denied,
    Insufficient,
    Error,
    Timeout,
}

/// 与 EventKit 解耦的事件视图:匹配逻辑只认它,可单测。
#[derive(Debug, Clone)]
pub struct EventInfo {
    pub event_id: String,
    pub title: String,
    pub start_ms: i64,
    pub end_ms: i64,
    pub all_day: bool,
    pub attendees: Vec<crate::store::CalendarAttendee>,
}

/// 两区间重叠时长(ms),无重叠为 0。
pub fn overlap_ms(a_start: i64, a_end: i64, b_start: i64, b_end: i64) -> i64 {
    (a_end.min(b_end) - a_start.max(b_start)).max(0)
}

/// 平手判定窗:两候选重叠时长差小于 1s 视为并列,不自动绑定(留给用户改选)。
pub const TIE_MS: i64 = 1000;

/// 纯匹配:排除全天事件;与录音窗 [start,end) 的**重叠时长**最大者胜;
/// 最优与次优差 < TIE_MS 视为平手返回 None;零重叠返回 None。
pub fn best_match(events: &[EventInfo], start_ms: i64, end_ms: i64) -> Option<&EventInfo> {
    let mut scored: Vec<(i64, &EventInfo)> = events
        .iter()
        .filter(|e| !e.all_day)
        .map(|e| (overlap_ms(start_ms, end_ms, e.start_ms, e.end_ms), e))
        .filter(|(ov, _)| *ov > 0)
        .collect();
    scored.sort_by(|a, b| b.0.cmp(&a.0));
    match scored.as_slice() {
        [] => None,
        [(_, best)] => Some(best),
        [(top, best), (second, _), ..] => (top - second >= TIE_MS).then_some(*best),
    }
}

/// 参会人邮箱规范化:trim、小写、大小写不敏感剥 `mailto:` 前缀、percent-decode。
/// 裸 strip_prefix 不够——EventKit 的 URL 可能是 `MAILTO:Zhang%40x.com`。
pub fn normalize_email(raw: &str) -> String {
    let s = raw.trim();
    let s = if s.len() >= 7 && s[..7].eq_ignore_ascii_case("mailto:") {
        &s[7..]
    } else {
        s
    };
    percent_decode(s).trim().to_ascii_lowercase()
}

fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let (Some(h), Some(l)) = (hex_val(bytes[i + 1]), hex_val(bytes[i + 2])) {
                out.push(h * 16 + l);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn hex_val(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

#[cfg(test)]
mod calendar_common_tests {
    use super::*;

    fn ev(id: &str, start_ms: i64, end_ms: i64, all_day: bool) -> EventInfo {
        EventInfo {
            event_id: id.into(),
            title: id.into(),
            start_ms,
            end_ms,
            all_day,
            attendees: vec![],
        }
    }

    #[test]
    fn best_match_excludes_all_day_and_zero_overlap() {
        let events = vec![ev("allday", 0, 86_400_000, true), ev("far", 900_000, 1_000_000, false)];
        assert!(best_match(&events, 0, 600_000).is_none(), "全天排除、无重叠 None");
    }

    #[test]
    fn best_match_picks_longest_overlap() {
        let events = vec![ev("short", 0, 120_000, false), ev("long", 0, 500_000, false)];
        let got = best_match(&events, 0, 600_000).unwrap();
        assert_eq!(got.event_id, "long");
    }

    #[test]
    fn best_match_tie_returns_none() {
        // 两事件与录音窗重叠时长相同(差 <1s):不自动绑。
        let events = vec![ev("a", 0, 300_000, false), ev("b", 300_000, 600_000, false)];
        assert!(best_match(&events, 0, 600_000).is_none());
        // 差距拉开 ≥1s 则取大者。
        let events2 = vec![ev("a", 0, 300_000, false), ev("b", 300_000, 601_500, false)];
        assert_eq!(best_match(&events2, 0, 601_500).unwrap().event_id, "b");
    }

    #[test]
    fn normalize_email_strips_mailto_and_percent() {
        assert_eq!(normalize_email("MAILTO:Zhang%40X.com "), "zhang@x.com");
        assert_eq!(normalize_email("mailto:a@b.c"), "a@b.c");
        assert_eq!(normalize_email("  A@B.C"), "a@b.c");
        assert_eq!(normalize_email("%zz"), "%zz", "非法转义原样保留");
    }
}
