//! 轻量应用设置（app_data_dir/settings.json，原子写）。目前仅镜像加速配置。

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

pub const DEFAULT_MIRROR_PREFIX: &str = "https://ghproxy.net/";

/// ASR 模型选型标识,供 settings.asr_model 与后续选型逻辑复用。
pub const ASR_SENSE_VOICE: &str = "sense_voice";
// whisper 选型标识;models::required_now 已消费,判定 whisper 工件是否录制必需。
pub const ASR_WHISPER: &str = "whisper";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Settings {
    #[serde(default)]
    pub mirror_enabled: bool,
    #[serde(default = "default_prefix")]
    pub mirror_prefix: String,
    /// 自定义数据目录(录音/转写等落盘位置);None 时回退到 app_data_dir。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data_dir: Option<String>,
    /// 自定义模型目录覆盖;None 时使用内置默认路径。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub models_dir: Option<String>,
    /// ASR 选型,见 ASR_SENSE_VOICE / ASR_WHISPER。
    #[serde(default = "default_asr")]
    pub asr_model: String,
}

fn default_prefix() -> String {
    DEFAULT_MIRROR_PREFIX.into()
}

fn default_asr() -> String {
    ASR_SENSE_VOICE.into()
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            mirror_enabled: false,
            mirror_prefix: default_prefix(),
            data_dir: None,
            models_dir: None,
            asr_model: default_asr(),
        }
    }
}

/// 数据根目录解析:配置了 data_dir 则用之,否则回退到系统 app_data_dir。
/// 纯函数,供 lib.rs 的 data_root 组装路径与本模块测试复用。
pub fn resolve_data_root(app_data: &Path, s: &Settings) -> PathBuf {
    match &s.data_dir {
        Some(d) if !d.is_empty() => PathBuf::from(d),
        _ => app_data.to_path_buf(),
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
        let s = Settings { mirror_enabled: true, mirror_prefix: "https://mirror.example/".into(), ..Default::default() };
        save(tmp.path(), &s).unwrap();
        let got = load(tmp.path());
        assert!(got.mirror_enabled);
        assert_eq!(got.mirror_prefix, "https://mirror.example/");
        assert!(!tmp.path().join("settings.json.tmp").exists(), "原子写不留 tmp");
    }

    #[test]
    fn new_fields_default_and_roundtrip() {
        let tmp = tempfile::tempdir().unwrap();
        // 旧文件(仅镜像字段)→ 新字段全默认
        std::fs::write(tmp.path().join("settings.json"), r#"{"mirror_enabled":true,"mirror_prefix":"x"}"#).unwrap();
        let s = load(tmp.path());
        assert_eq!(s.data_dir, None);
        assert_eq!(s.models_dir, None);
        assert_eq!(s.asr_model, ASR_SENSE_VOICE);
        // 新字段 roundtrip
        let s = Settings {
            data_dir: Some("/tmp/d".into()),
            models_dir: Some("/tmp/m".into()),
            asr_model: ASR_WHISPER.into(),
            ..Default::default()
        };
        save(tmp.path(), &s).unwrap();
        let got = load(tmp.path());
        assert_eq!(got.data_dir.as_deref(), Some("/tmp/d"));
        assert_eq!(got.models_dir.as_deref(), Some("/tmp/m"));
        assert_eq!(got.asr_model, "whisper");
    }

    #[test]
    fn resolve_data_root_prefers_configured() {
        let base = Path::new("/base");
        assert_eq!(resolve_data_root(base, &Settings::default()), PathBuf::from("/base"));
        let s = Settings { data_dir: Some("/custom".into()), ..Default::default() };
        assert_eq!(resolve_data_root(base, &s), PathBuf::from("/custom"));
    }
}
