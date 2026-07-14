//! 外部钩子(用户配置 shell/webhook):配置持久化 + 业务事件映射 + 执行体。
//!
//! 配置存 app_data_dir/hooks.json(原子写,模式同 settings.rs;独立文件,
//! 不与设置页抢 settings.json 的读-改-写窗口)。后端每次事件读快照,无内存
//! 状态同步。执行契约与 lifecycle::hooks::HookBus 一致:任何失败只记日志,
//! 绝不影响录制/精修主流程。

use serde::{Deserialize, Serialize};
use std::path::Path;

/// 一条钩子配置。event/kind 存字符串而非枚举:未知值只让该条失配,不让整个
/// hooks.json 反序列化失败(枚举会连带炸掉全表,老文件升级即中招)。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HookCfg {
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub name: String,
    /// 事件白名单值,见 HookEvent::as_str。
    #[serde(default)]
    pub event: String,
    /// "shell" | "webhook"。
    #[serde(default = "default_kind")]
    pub kind: String,
    #[serde(default)]
    pub command: String,
    #[serde(default)]
    pub url: String,
    #[serde(default = "default_true")]
    pub enabled: bool,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct HooksFile {
    #[serde(default)]
    pub hooks: Vec<HookCfg>,
}

fn default_kind() -> String {
    "shell".into()
}

fn default_true() -> bool {
    true
}

/// 缺失/损坏 → 空表(容忍,不报错;与 settings::load 同策略)。
pub fn load(app_data: &Path) -> HooksFile {
    std::fs::read_to_string(app_data.join("hooks.json"))
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

pub fn save(app_data: &Path, f: &HooksFile) -> anyhow::Result<()> {
    std::fs::create_dir_all(app_data)?;
    let tmp = app_data.join("hooks.json.tmp");
    std::fs::write(&tmp, serde_json::to_string_pretty(f)?)?;
    std::fs::rename(&tmp, app_data.join("hooks.json"))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn load_missing_or_corrupt_falls_back_to_empty() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(load(tmp.path()).hooks.is_empty(), "缺文件 → 空表");
        std::fs::write(tmp.path().join("hooks.json"), "not json").unwrap();
        assert!(load(tmp.path()).hooks.is_empty(), "损坏 → 空表");
    }

    #[test]
    fn save_then_load_roundtrip_atomic() {
        let tmp = tempfile::tempdir().unwrap();
        let f = HooksFile {
            hooks: vec![HookCfg {
                id: "h_1".into(),
                name: "停录归档".into(),
                event: "recording_stopped".into(),
                kind: "shell".into(),
                command: "echo done".into(),
                url: String::new(),
                enabled: true,
            }],
        };
        save(tmp.path(), &f).unwrap();
        let got = load(tmp.path());
        assert_eq!(got.hooks.len(), 1);
        assert_eq!(got.hooks[0].event, "recording_stopped");
        assert!(!tmp.path().join("hooks.json.tmp").exists(), "原子写不留 tmp");
    }

    #[test]
    fn missing_fields_take_defaults() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(
            tmp.path().join("hooks.json"),
            r#"{"hooks":[{"id":"h_2","event":"recording_started","command":"true"}]}"#,
        )
        .unwrap();
        let got = load(tmp.path());
        assert_eq!(got.hooks[0].kind, "shell", "kind 缺省 shell");
        assert!(got.hooks[0].enabled, "enabled 缺省 true");
    }
}
