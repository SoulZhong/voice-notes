//! 升级:`check_update` 查 GitHub 最新 Release 的版本号供前端提示(纯读、无副作用,
//! 任何失败降级为 Err,绝不影响录制/转写);`apply_update` 是一键更新的下载与安装。

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
    let body = crate::netproxy::agent_for(&api).get(&api)
        .timeout(std::time::Duration::from_secs(REQ_TIMEOUT_S))
        .set("User-Agent", "voice-notes")
        .set("Accept", "application/vnd.github+json")
        .call()
        .map_err(|e| crate::tr!("检查更新失败: {e}", "Failed to check for updates: {e}"))?
        .into_string()
        .map_err(|e| crate::tr!("读取响应失败: {e}", "Failed to read the response: {e}"))?;
    let (tag, html_url, notes) =
        parse_latest(&body).ok_or_else(|| crate::tr!("解析 GitHub 响应失败", "Failed to parse the GitHub response"))?;
    let latest = tag.trim_start_matches('v').to_string();
    let has_update = is_newer(&latest, &current);
    Ok(UpdateInfo { current, latest, has_update, url: html_url, notes })
}

/// 一键更新进度(事件 `update_progress`)。
#[derive(Debug, Clone, Serialize)]
pub struct UpdateProgress {
    pub received: u64,
    /// 0 = 总长未知。
    pub total: u64,
    /// 已换到加速线路(界面提示「网络较慢,已切换加速线路」)。
    pub mirror: bool,
    /// 下载完毕,正在校验与安装。
    pub installing: bool,
}

/// 一键更新:查 latest.json → 经下载线路下载升级包(自动切换直连/镜像、断点续传)→
/// 校验签名 → 安装。返回 false = 没有更新;true = 已装好,由前端重启应用
/// (Windows 上安装器会先结束本进程,走不到返回)。
///
/// 不用插件自带的 downloadAndInstall:它的 timeout 是**整次下载**的总时限(reqwest
/// 语义),25MB 的包在每秒不到 0.85MB 的网络上 30 秒必然超时,进度条走到一半就报失败;
/// 也不能续传、不能换线路。签名校验因此在这里做,口径与插件一致(minisign + 签名里
/// 记录的版本号必须等于 latest.json 宣布的版本)。
#[tauri::command]
pub async fn apply_update(app: AppHandle) -> Result<bool, String> {
    use tauri::{Emitter, Manager};
    use tauri_plugin_updater::UpdaterExt;
    let fail = |e: String| crate::tr!("一键更新失败: {e}", "Update failed: {e}", e = e);
    let mut builder = app.updater_builder().timeout(std::time::Duration::from_secs(30));
    if let Some(proxy) = crate::netproxy::system_proxy_url("https://github.com/".into()).and_then(|p| p.parse().ok()) {
        builder = builder.proxy(proxy);
    }
    let updater = builder.build().map_err(|e| fail(e.to_string()))?;
    let Some(update) = updater.check().await.map_err(|e| fail(e.to_string()))? else {
        return Ok(false);
    };
    let pubkey = app
        .config()
        .plugins
        .0
        .get("updater")
        .and_then(|u| u.get("pubkey"))
        .and_then(|k| k.as_str())
        .ok_or_else(|| fail("missing updater pubkey".into()))?
        .to_string();
    let cache = app.path().app_cache_dir().map_err(|e| fail(e.to_string()))?;
    let app_data = app.path().app_data_dir().map_err(|e| fail(e.to_string()))?;
    let url = update.download_url.to_string();
    let version = update.version.clone();
    let signature = update.signature.clone();
    let app_dl = app.clone();
    let bytes = tauri::async_runtime::spawn_blocking(move || -> Result<Vec<u8>, String> {
        std::fs::create_dir_all(&cache).map_err(|e| e.to_string())?;
        let part = cache.join(format!("update-{version}.part"));
        // 旧版本残留的分片没用了。
        if let Ok(rd) = std::fs::read_dir(&cache) {
            for e in rd.flatten() {
                let name = e.file_name().to_string_lossy().into_owned();
                if name.starts_with("update-") && name.ends_with(".part") && e.path() != part {
                    let _ = std::fs::remove_file(e.path());
                }
            }
        }
        let memory = crate::netroute::RouteMemory::new(&app_data);
        let cancel = std::sync::atomic::AtomicBool::new(false);
        let emit = |p: crate::netroute::Progress| {
            let _ = app_dl.emit(
                "update_progress",
                UpdateProgress { received: p.received, total: p.total, mirror: p.switched && p.mirror, installing: false },
            );
        };
        crate::netroute::fetch(&url, &part, &memory, &cancel, &emit)?;
        let bytes = std::fs::read(&part).map_err(|e| e.to_string())?;
        if let Err(e) = verify_signature(&bytes, &signature, &pubkey, &version) {
            // 坏分片(或被篡改)不留着续传。
            let _ = std::fs::remove_file(&part);
            return Err(e);
        }
        let _ = std::fs::remove_file(&part);
        Ok(bytes)
    })
    .await
    .map_err(|e| fail(e.to_string()))?
    .map_err(fail)?;
    let _ = app.emit("update_progress", UpdateProgress { received: 0, total: 0, mirror: false, installing: true });
    tauri::async_runtime::spawn_blocking(move || update.install(bytes))
        .await
        .map_err(|e| fail(e.to_string()))?
        .map_err(|e| fail(e.to_string()))?;
    Ok(true)
}

/// 校验升级包签名,口径同 tauri-plugin-updater:pubkey 与 signature 都是 base64 包着的
/// minisign 文本;签名可信注释里若记了 `version:`,必须等于 latest.json 宣布的版本
/// (防止篡改的清单把新版本号配上旧安装包)。
fn verify_signature(data: &[u8], signature_b64: &str, pubkey_b64: &str, announced: &str) -> Result<(), String> {
    use base64::Engine;
    let decode = |s: &str| -> Result<String, String> {
        let raw = base64::engine::general_purpose::STANDARD.decode(s).map_err(|e| e.to_string())?;
        String::from_utf8(raw).map_err(|e| e.to_string())
    };
    let key = minisign_verify::PublicKey::decode(&decode(pubkey_b64)?).map_err(|e| e.to_string())?;
    let sig = minisign_verify::Signature::decode(&decode(signature_b64)?).map_err(|e| e.to_string())?;
    key.verify(data, &sig, true)
        .map_err(|e| crate::tr!("签名校验失败: {e}", "Signature check failed: {e}", e = e))?;
    let signed = sig.trusted_comment().split('\t').find_map(|f| f.strip_prefix("version:"));
    match signed {
        Some(v) if v.trim_start_matches('v') != announced.trim_start_matches('v') => Err(crate::tr!(
            "签名版本 {v} 与宣布的 {a} 不符",
            "Signed version {v} does not match announced {a}",
            v = v,
            a = announced
        )),
        _ => Ok(()),
    }
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

    /// 联网:经下载线路拉真实的 v0.20.0 升级包与签名,用 tauri.conf 的公钥校验;
    /// 改一个字节必须校验失败。(现有签名的可信注释没记 version,版本比对不生效,
    /// 与插件 require_signed_version=false 的口径一致。)
    #[test]
    #[ignore]
    fn real_release_package_verifies_through_netroute() {
        let conf: serde_json::Value = serde_json::from_str(include_str!("../tauri.conf.json")).unwrap();
        let pubkey = conf["plugins"]["updater"]["pubkey"].as_str().unwrap();
        let base = "https://github.com/SoulZhong/voice-notes/releases/download/v0.20.0/voice-notes_aarch64.app.tar.gz";
        let dir = tempfile::tempdir().unwrap();
        let mem = crate::netroute::RouteMemory::new(dir.path());
        let cancel = std::sync::atomic::AtomicBool::new(false);
        let part = dir.path().join("pkg.part");
        let t0 = std::time::Instant::now();
        let n = crate::netroute::fetch(base, &part, &mem, &cancel, &|_| {}).unwrap();
        eprintln!("{n} bytes in {:?}, mirror={}", t0.elapsed(), mem.prefers_mirror());
        let sig_part = dir.path().join("sig.part");
        crate::netroute::fetch(&format!("{base}.sig"), &sig_part, &mem, &cancel, &|_| {}).unwrap();
        let sig = std::fs::read_to_string(&sig_part).unwrap();
        let mut bytes = std::fs::read(&part).unwrap();
        verify_signature(&bytes, sig.trim(), pubkey, "0.20.0").unwrap();
        bytes[1000] ^= 1;
        assert!(verify_signature(&bytes, sig.trim(), pubkey, "0.20.0").is_err(), "篡改必须拒");
    }
}
