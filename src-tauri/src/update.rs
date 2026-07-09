//! 轻量升级检查:查 GitHub 最新 Release 的版本号,比当前新就让前端提示用户去发布页
//! 手动下载 DMG。纯读、无副作用;任何失败(断网/限流/无发布/解析失败)一律降级为
//! Err,由前端决定静默或提示,绝不影响录制/转写。不做应用内下载安装(那是自动更新档)。

use serde::Serialize;
use tauri::AppHandle;

/// 仓库(owner/name),与 README/前端硬编码一致。
const REPO: &str = "SoulZhong/voice-notes";
const REQ_TIMEOUT_S: u64 = 8;

#[derive(Debug, Serialize)]
pub struct UpdateInfo {
    /// 当前应用版本(= tauri.conf.json 的 version)。
    pub current: String,
    /// GitHub 最新 Release 版本(已剥 `v` 前缀)。
    pub latest: String,
    /// latest 是否严格新于 current。
    pub has_update: bool,
    /// 发布页 URL(含 changelog + DMG 资源),前端「查看更新」直接打开。
    pub url: String,
    /// 该版本的更新说明(Release body),可能为空。
    pub notes: String,
}

/// 检查更新:查 GitHub 最新 Release(`/releases/latest` 天然排除 draft/prerelease),
/// 与当前版本比较。失败返回 Err(前端据此静默或提示「检查失败」)。
#[tauri::command]
pub fn check_update(app: AppHandle) -> Result<UpdateInfo, String> {
    let current = app.package_info().version.to_string();
    let api = format!("https://api.github.com/repos/{REPO}/releases/latest");
    // GitHub API 强制要求 User-Agent,缺了直接 403——前端 fetch 设不了此头,故走 Rust。
    let body = ureq::get(&api)
        .timeout(std::time::Duration::from_secs(REQ_TIMEOUT_S))
        .set("User-Agent", "voice-notes")
        .set("Accept", "application/vnd.github+json")
        .call()
        .map_err(|e| format!("检查更新失败: {e}"))?
        .into_string()
        .map_err(|e| format!("读取响应失败: {e}"))?;
    let (tag, html_url, notes) =
        parse_latest(&body).ok_or_else(|| "解析 GitHub 响应失败".to_string())?;
    let latest = tag.trim_start_matches('v').to_string();
    let has_update = is_newer(&latest, &current);
    Ok(UpdateInfo { current, latest, has_update, url: html_url, notes })
}

/// 从 `releases/latest` JSON 里取 (tag_name, html_url, body)。tag_name 缺失/非 JSON → None;
/// html_url/body 缺失可容忍(回退发布列表页 / 空说明)。
fn parse_latest(json: &str) -> Option<(String, String, String)> {
    let v: serde_json::Value = serde_json::from_str(json).ok()?;
    let tag = v.get("tag_name")?.as_str()?.to_string();
    let html_url = v
        .get("html_url")
        .and_then(|x| x.as_str())
        .unwrap_or("https://github.com/SoulZhong/voice-notes/releases")
        .to_string();
    let notes = v.get("body").and_then(|x| x.as_str()).unwrap_or("").to_string();
    Some((tag, html_url, notes))
}

/// latest 是否严格新于 current。按 `x.y.z…` 数字逐段比较(非字典序:0.2.10 > 0.2.9),
/// 剥前导 `v`,缺段视为 0(1.2 == 1.2.0),段内取前导数字(忽略 -rc/+build 尾巴)。
/// 相等或更旧 → false。
fn is_newer(latest: &str, current: &str) -> bool {
    let parse = |s: &str| -> Vec<u64> {
        s.trim_start_matches('v')
            .split('.')
            .map(|p| {
                let num: String = p.chars().take_while(|c| c.is_ascii_digit()).collect();
                num.parse().unwrap_or(0)
            })
            .collect()
    };
    let (a, b) = (parse(latest), parse(current));
    for i in 0..a.len().max(b.len()) {
        let (x, y) = (a.get(i).copied().unwrap_or(0), b.get(i).copied().unwrap_or(0));
        if x != y {
            return x > y;
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn is_newer_compares_semver_numerically() {
        assert!(is_newer("0.3.0", "0.2.0"));
        assert!(is_newer("0.2.1", "0.2.0"));
        assert!(is_newer("1.0.0", "0.9.9"));
        assert!(is_newer("0.2.10", "0.2.9"), "数字比较而非字典序");
        assert!(is_newer("v0.3.0", "0.2.0"), "剥 v 前缀(latest)");
        assert!(is_newer("0.3.0", "v0.2.0"), "剥 v 前缀(current)");
        assert!(!is_newer("0.2.0", "0.2.0"), "相等不算新");
        assert!(!is_newer("0.2.0", "0.3.0"), "更旧不算新");
        assert!(!is_newer("1.2.0", "1.2"), "缺段补零:1.2.0 == 1.2");
        assert!(!is_newer("1.2", "1.2.0"), "反向也相等");
        assert!(is_newer("0.3.0-rc1", "0.2.0"), "段内取前导数字,忽略 -rc 尾巴");
    }

    #[test]
    fn parse_latest_extracts_fields_and_tolerates_missing() {
        let json = r#"{"tag_name":"v0.3.0","html_url":"https://github.com/x/y/releases/tag/v0.3.0","body":"- fix A\n- fix B"}"#;
        let (tag, url, notes) = parse_latest(json).unwrap();
        assert_eq!(tag, "v0.3.0");
        assert_eq!(url, "https://github.com/x/y/releases/tag/v0.3.0");
        assert_eq!(notes, "- fix A\n- fix B");
        // 缺 body/html_url:回退默认,notes 空
        let (tag2, url2, notes2) = parse_latest(r#"{"tag_name":"v0.4.0"}"#).unwrap();
        assert_eq!(tag2, "v0.4.0");
        assert!(url2.contains("releases"));
        assert_eq!(notes2, "");
        // 缺 tag_name → None;非 JSON → None
        assert!(parse_latest(r#"{"name":"x"}"#).is_none());
        assert!(parse_latest("not json").is_none());
    }
}
