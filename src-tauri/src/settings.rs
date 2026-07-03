//! 轻量应用设置（app_data_dir/settings.json，原子写）。目前仅镜像加速配置。

use serde::{Deserialize, Serialize};
use std::path::Path;

pub const DEFAULT_MIRROR_PREFIX: &str = "https://ghproxy.net/";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Settings {
    #[serde(default)]
    pub mirror_enabled: bool,
    #[serde(default = "default_prefix")]
    pub mirror_prefix: String,
}

fn default_prefix() -> String {
    DEFAULT_MIRROR_PREFIX.into()
}

impl Default for Settings {
    fn default() -> Self {
        Self { mirror_enabled: false, mirror_prefix: default_prefix() }
    }
}

/// 缺失/损坏 → 默认值（容忍，不报错）。
pub fn load(app_data: &Path) -> Settings {
    std::fs::read_to_string(app_data.join("settings.json"))
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

pub fn save(app_data: &Path, s: &Settings) -> anyhow::Result<()> {
    std::fs::create_dir_all(app_data)?;
    let tmp = app_data.join("settings.json.tmp");
    std::fs::write(&tmp, serde_json::to_string_pretty(s)?)?;
    std::fs::rename(&tmp, app_data.join("settings.json"))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn load_missing_or_corrupt_falls_back_to_default() {
        let tmp = tempfile::tempdir().unwrap();
        let s = load(tmp.path());
        assert!(!s.mirror_enabled);
        assert_eq!(s.mirror_prefix, DEFAULT_MIRROR_PREFIX);
        std::fs::write(tmp.path().join("settings.json"), "not json").unwrap();
        assert!(!load(tmp.path()).mirror_enabled, "损坏 → 默认值");
    }

    #[test]
    fn save_then_load_roundtrip() {
        let tmp = tempfile::tempdir().unwrap();
        let s = Settings { mirror_enabled: true, mirror_prefix: "https://mirror.example/".into() };
        save(tmp.path(), &s).unwrap();
        let got = load(tmp.path());
        assert!(got.mirror_enabled);
        assert_eq!(got.mirror_prefix, "https://mirror.example/");
        assert!(!tmp.path().join("settings.json.tmp").exists(), "原子写不留 tmp");
    }
}
