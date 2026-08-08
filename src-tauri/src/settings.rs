//! 轻量应用设置（app_data_dir/settings.json，原子写）。目前仅镜像加速配置。

use serde::{Deserialize, Serialize};
use std::io::Write;
use std::path::{Path, PathBuf};

pub const DEFAULT_MIRROR_PREFIX: &str = "https://ghfast.top/";
/// 旧默认前缀(v0.4.1 及之前)。仅用于一次性迁移判定:UI 从不允许编辑前缀,故存量等于此
/// 值者必是旧默认而非用户自定义,可安全抬到新默认。
pub const LEGACY_MIRROR_PREFIX: &str = "https://ghproxy.net/";

/// ASR 模型选型标识,供 settings.asr_model 与后续选型逻辑复用。
pub const ASR_SENSE_VOICE: &str = "sense_voice";
// whisper 选型标识;models::required_now 已消费,判定 whisper 工件是否录制必需。
pub const ASR_WHISPER: &str = "whisper";
/// Paraformer-large 中文选型。
pub const ASR_PARAFORMER: &str = "paraformer";
/// Qwen3-ASR 0.6B int8 选型(52 语种/中英混说,LLM 解码,支持热词)。
pub const ASR_QWEN3: &str = "qwen3";
/// 识别方式:本地模型 / 云端 API(spec 2026-07-29-cloud-asr-design)。
pub const ASR_MODE_LOCAL: &str = "local";
pub const ASR_MODE_CLOUD: &str = "cloud";
/// 云端厂商标识。
pub const CLOUD_VOLCANO: &str = "volcano";
pub const CLOUD_ALIYUN: &str = "aliyun";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Settings {
    #[serde(default = "default_true")]
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
    /// sherpa 推理 provider 覆盖(实验字段,无 UI,手改 settings.json)。空 = sherpa
    /// 默认(0.6.8 硬编码 CPU);macOS 可填 "coreml" 实验加速(见 2026-07-28 ASR 调研)。
    /// 值原样透传 sherpa/onnxruntime,不做白名单;加载失败会走既有报错路径,不静默降级。
    #[serde(default)]
    pub asr_provider: String,
    /// 识别方式:"local"(默认,现状) / "cloud"。录制中禁改(set_settings 保护)。
    #[serde(default = "default_asr_mode")]
    pub asr_mode: String,
    /// 云端厂商:"volcano" / "aliyun"。
    #[serde(default = "default_cloud_provider")]
    pub cloud_asr_provider: String,
    /// 火山凭证(APP ID / Access Token)。明文存储,同 refine_api_key 先例。
    #[serde(default)]
    pub volc_app_key: String,
    #[serde(default)]
    pub volc_access_key: String,
    /// 阿里 DashScope API Key。明文,同上。
    #[serde(default)]
    pub dashscope_api_key: String,
    /// 声纹嵌入模型选型:"campplus"(默认)/"eres2netv2"。不同模型嵌入空间不可混用,
    /// 切换会触发声纹库从录音样本后台重建(见 lib.rs set_settings)。
    #[serde(default = "default_speaker_model")]
    pub speaker_model: String,
    /// 外观主题,消费任务:主题切换。"system"/"light"/"dark"。
    #[serde(default = "default_theme")]
    pub theme: String,
    /// UI 语言:"system"(跟随系统)/"zh"/"en"。前端界面与托盘/后端用户可见文案共用。
    /// 注意与 language_filter(转写乱码过滤)语义无关。
    #[serde(default = "default_ui_lang")]
    pub ui_lang: String,
    /// 仅录系统声(不录麦克风),消费任务:录制开关。
    #[serde(default)]
    pub record_system_only: bool,
    /// 录制时保持外放音量:麦克风采集用普通输入代替 VPIO(通话模式)。VPIO 启动即触发
    /// macOS 把其它音频压低 12-16dB(ducking,Min 档仍生效,固有行为);普通输入无 ducking,
    /// 回声改由软件 AEC(WebRTC AEC3,system 采集流为参考,见 audio::aec)消除,
    /// 文本回声去重链保留为兜底。默认关(走 VPIO)。
    #[serde(default)]
    pub keep_output_volume: bool,
    /// 语言过滤开关,消费任务:转写语言过滤;默认开启。
    #[serde(default = "default_true")]
    pub language_filter: bool,
    /// 保留原始录音音频,消费任务:录制开关;默认开启。
    #[serde(default = "default_true")]
    pub keep_audio: bool,
    /// 全局快捷键开关,消费任务:快捷键;默认关闭(避免未经用户同意即占用系统快捷键)。
    #[serde(default)]
    pub shortcut_enabled: bool,
    /// 全局快捷键组合,消费任务:快捷键。
    #[serde(default = "default_shortcut")]
    pub shortcut: String,
    /// 系统托盘图标开关,消费任务:托盘;默认开启。
    #[serde(default = "default_true")]
    pub tray_enabled: bool,
    /// 会后 LLM Aing 总开关(A2)。默认关,配好 key 后由用户打开。
    #[serde(default)]
    pub refine_enabled: bool,
    /// A2 执行体:"openai"(HTTP chat completions)| "agent"(本机 Agent CLI 经
    /// MCP 读写回)。老配置缺字段 → openai,行为不变。
    #[serde(default = "default_refine_provider")]
    pub refine_provider: String,
    /// provider=agent 时用哪家 CLI:claude|codex|gemini|cursor。
    #[serde(default = "default_refine_agent")]
    pub refine_agent: String,
    /// Agent CLI 可执行文件路径覆盖;空 = 按常见安装位置自动探测。
    #[serde(default)]
    pub refine_agent_bin: String,
    /// Agent 模型名(传给 CLI 的 --model/-m);空 = 该 CLI 自己的默认模型。
    #[serde(default)]
    pub refine_agent_model: String,
    /// OpenAI 兼容 chat completions 的 base_url,如 https://api.deepseek.com。
    #[serde(default)]
    pub refine_base_url: String,
    /// 模型名,如 deepseek-chat。
    #[serde(default)]
    pub refine_model: String,
    /// API key。明文存本机 settings.json(单机应用,设置页已注明)。
    #[serde(default)]
    pub refine_api_key: String,
    /// 首启引导已完成(欢迎层「开始使用」下载完成或进入「高级设置」时置 true)。
    /// 老用户升级(字段缺失)反序列化为 false,但 layout 侧发现模型已就绪会静默补 true,
    /// 不会对老用户弹引导。
    #[serde(default)]
    pub onboarded: bool,
    /// 已完成的功能引导 ID。每项功能/重大版本独立记账，不能让一个全局 bool
    /// 永久吞掉后续新增功能的引导。
    #[serde(default)]
    pub completed_guides: Vec<String>,
    /// 允许 MCP(AI 助手)控制录制(start/stop/pause/resume)。默认关:开录是隐私
    /// 敏感操作,必须用户显式授权。
    #[serde(default)]
    pub mcp_allow_control: bool,
    /// MCP 接入引导已展示过(欢迎页步骤走完,或存量用户提示条被关闭)。
    #[serde(default)]
    pub mcp_onboarded: bool,
    /// 方案 B:录制期把两轨混成 mixed.wav。默认关——实验特性,开启后每分钟多约
    /// 1.9MB 磁盘(转码 m4a 后大幅缩小)。仅影响新录制,已有笔记不受影响。
    /// 实验字段,无 UI,手改 settings.json。
    #[serde(default)]
    pub mix_track: bool,
    /// P3 日历匹配:录制停止后按时间窗匹配日历事件(标题+参会人入 identify 先验)。
    /// 默认开——但真正生效还需系统日历授权(授权只能由设置页说明卡触发,自动
    /// 路径未授权即静默跳过),默认开不会造成 surprise 弹窗。
    #[serde(default = "default_true")]
    pub calendar_match_enabled: bool,
    /// P2b 自动应用:high 档身份推断自动关联+回灌(回执可撤销)。默认关——
    /// 开启门槛是评测数据达标(spec:≥20 场标注、high 档 ≥50 样本误认 ≤1%),
    /// 由用户在设置页自行拨开。
    #[serde(default)]
    pub identify_auto_apply: bool,
}

fn default_prefix() -> String {
    DEFAULT_MIRROR_PREFIX.into()
}

fn default_speaker_model() -> String {
    "campplus".into()
}

fn default_asr() -> String {
    ASR_SENSE_VOICE.into()
}

fn default_theme() -> String {
    "system".into()
}

fn default_ui_lang() -> String {
    "system".into()
}

fn default_shortcut() -> String {
    "Alt+CmdOrCtrl+R".into()
}

fn default_refine_provider() -> String {
    "openai".into()
}

fn default_refine_agent() -> String {
    "claude".into()
}

fn default_asr_mode() -> String { ASR_MODE_LOCAL.into() }
fn default_cloud_provider() -> String { CLOUD_VOLCANO.into() }

/// 当前选中厂商的凭证是否齐全(云端模式录制就绪的必要条件)。
pub fn cloud_creds_ok(s: &Settings) -> bool {
    match s.cloud_asr_provider.as_str() {
        CLOUD_ALIYUN => !s.dashscope_api_key.trim().is_empty(),
        _ => !s.volc_app_key.trim().is_empty() && !s.volc_access_key.trim().is_empty(),
    }
}

/// serde `#[derive(Deserialize)]` 的裸 `#[serde(default)]` 总是取字段类型的
/// `Default::default()`(bool → false)。language_filter/keep_audio/tray_enabled
/// 三个字段的产品默认值是 true,所以必须显式挂这个辅助函数,不能偷懒裸写 default。
fn default_true() -> bool {
    true
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            mirror_enabled: true,
            mirror_prefix: default_prefix(),
            data_dir: None,
            models_dir: None,
            asr_model: default_asr(),
            asr_provider: String::new(),
            asr_mode: default_asr_mode(),
            cloud_asr_provider: default_cloud_provider(),
            volc_app_key: String::new(),
            volc_access_key: String::new(),
            dashscope_api_key: String::new(),
            speaker_model: default_speaker_model(),
            theme: default_theme(),
            ui_lang: default_ui_lang(),
            record_system_only: false,
            keep_output_volume: false,
            language_filter: true,
            keep_audio: true,
            shortcut_enabled: false,
            shortcut: default_shortcut(),
            tray_enabled: true,
            refine_enabled: false,
            refine_provider: default_refine_provider(),
            refine_agent: default_refine_agent(),
            refine_agent_bin: String::new(),
            refine_agent_model: String::new(),
            refine_base_url: String::new(),
            refine_model: String::new(),
            refine_api_key: String::new(),
            onboarded: false,
            completed_guides: Vec::new(),
            mcp_allow_control: false,
            mcp_onboarded: false,
            mix_track: false,
            calendar_match_enabled: true,
            identify_auto_apply: false,
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
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(&tmp)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        // tmp 可能来自上次崩溃且权限较宽；mode() 只对新建文件生效，显式收紧。
        file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
    }
    file.write_all(serde_json::to_string_pretty(s)?.as_bytes())?;
    drop(file);
    std::fs::rename(&tmp, app_data.join("settings.json"))?;
    Ok(())
}

/// settings.json 读-改-写串行化锁。为什么需要:load→改→save 这个序列若被并发穿插,会
/// 发生丢写——例如迁移线程刚把 data_dir 指针 save 提交,而镜像开关命令用它更早 load 的
/// 旧快照 save 覆盖回去 → 指针丢失,随后迁移的删旧逻辑把旧数据删掉 → 笔记"凭空消失"。
/// 进程内单锁,单文件写入量小,串行代价可忽略。
static WRITE_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// 原子化读-改-写:锁内 load → f(&mut s) → save,返回落盘后的新值。所有会修改
/// settings.json 的路径都应走这里(而非各自 load 后 save),否则并发写互相覆盖(见
/// WRITE_LOCK 注释)。中毒锁降级取值继续(设置写入不该因一次 panic 永久卡死)。
pub fn update(app_data: &Path, f: impl FnOnce(&mut Settings)) -> anyhow::Result<Settings> {
    let _guard = WRITE_LOCK.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let mut s = load(app_data);
    f(&mut s);
    save(app_data, &s)?;
    Ok(s)
}

/// 一次性迁移:存量 mirror_prefix 若等于旧默认(ghproxy.net),抬到新默认(ghfast.top)。
/// 幂等——非旧默认值(新默认 / 未来自定义)不动。走 update 复用 WRITE_LOCK 串行化。
pub fn migrate_mirror_prefix(app_data: &Path) -> anyhow::Result<Settings> {
    update(app_data, |s| {
        if s.mirror_prefix == LEGACY_MIRROR_PREFIX {
            s.mirror_prefix = DEFAULT_MIRROR_PREFIX.into();
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn load_missing_or_corrupt_falls_back_to_default() {
        let tmp = tempfile::tempdir().unwrap();
        let s = load(tmp.path());
        assert!(s.mirror_enabled);
        assert_eq!(s.mirror_prefix, DEFAULT_MIRROR_PREFIX);
        std::fs::write(tmp.path().join("settings.json"), "not json").unwrap();
        assert!(load(tmp.path()).mirror_enabled, "损坏 → 默认值");
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

    #[cfg(unix)]
    #[test]
    fn save_keeps_credentials_private_to_current_user() {
        use std::os::unix::fs::PermissionsExt;

        let tmp = tempfile::tempdir().unwrap();
        let s = Settings {
            volc_access_key: "secret".into(),
            dashscope_api_key: "secret".into(),
            ..Default::default()
        };
        save(tmp.path(), &s).unwrap();
        let mode = std::fs::metadata(tmp.path().join("settings.json"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o600, "含 API key 的 settings.json 不得向同机其他用户开放");
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
        std::fs::write(tmp.path().join("settings.json"), "{}").unwrap();
        assert!(load(tmp.path()).mirror_enabled, "旧配置缺镜像字段时应默认启用内置加速");
        // 新字段 roundtrip
        let s = Settings {
            data_dir: Some("/tmp/d".into()),
            models_dir: Some("/tmp/m".into()),
            asr_model: ASR_WHISPER.into(),
            speaker_model: default_speaker_model(),
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
        // 空串视同未配置,回落默认根(防止 Some("") 把根设成当前目录)。
        let s = Settings { data_dir: Some("".into()), ..Default::default() };
        assert_eq!(resolve_data_root(base, &s), PathBuf::from("/base"), "空串回落默认");
    }

    #[test]
    fn update_roundtrip_applies_and_persists() {
        let tmp = tempfile::tempdir().unwrap();
        let got = update(tmp.path(), |s| s.mirror_enabled = true).unwrap();
        assert!(got.mirror_enabled, "返回落盘后的新值");
        assert!(load(tmp.path()).mirror_enabled, "已持久化到磁盘");
    }

    #[test]
    fn enhancement_fields_default_and_roundtrip() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("settings.json"), r#"{"mirror_enabled":false,"mirror_prefix":"x"}"#).unwrap();
        let s = load(tmp.path());
        assert_eq!(s.theme, "system");
        assert_eq!(s.ui_lang, "system", "老配置缺 ui_lang 应回落跟随系统");
        assert!(!s.record_system_only && s.language_filter && s.keep_audio);
        assert!(!s.keep_output_volume, "保持外放音量默认关(保留 AEC)");
        assert!(!s.shortcut_enabled);
        assert_eq!(s.shortcut, "Alt+CmdOrCtrl+R");
        assert!(s.tray_enabled);
        let s = Settings { theme: "dark".into(), ui_lang: "en".into(), record_system_only: true,
            language_filter: false, keep_audio: false, keep_output_volume: true, shortcut_enabled: true,
            shortcut: "Alt+CmdOrCtrl+K".into(), tray_enabled: false, ..Default::default() };
        save(tmp.path(), &s).unwrap();
        let got = load(tmp.path());
        assert_eq!(got.theme, "dark");
        assert_eq!(got.ui_lang, "en");
        assert!(got.record_system_only && !got.language_filter && !got.keep_audio);
        assert!(got.keep_output_volume);
        assert!(got.shortcut_enabled && !got.tray_enabled);
        assert_eq!(got.shortcut, "Alt+CmdOrCtrl+K");
    }

    #[test]
    fn concurrent_update_different_fields_no_lost_write() {
        use std::sync::Arc;
        let tmp = tempfile::tempdir().unwrap();
        let dir = Arc::new(tmp.path().to_path_buf());
        // 两线程各反复改一字段:WRITE_LOCK 串行化 load-modify-save,终态两字段都应是新值。
        // 无锁时后写者会用自己更早的 load 快照覆盖掉前写者刚提交的另一字段(丢写)。
        let d1 = dir.clone();
        let h1 = std::thread::spawn(move || {
            for _ in 0..100 {
                update(&d1, |s| s.mirror_enabled = true).unwrap();
            }
        });
        let d2 = dir.clone();
        let h2 = std::thread::spawn(move || {
            for _ in 0..100 {
                update(&d2, |s| s.asr_model = ASR_WHISPER.into()).unwrap();
            }
        });
        h1.join().unwrap();
        h2.join().unwrap();
        let got = load(&dir);
        assert!(got.mirror_enabled, "线程1 的写未被丢");
        assert_eq!(got.asr_model, ASR_WHISPER, "线程2 的写未被丢");
    }

    #[test]
    fn refine_defaults_off_and_empty() {
        let s = Settings::default();
        assert!(!s.refine_enabled);
        assert!(s.refine_base_url.is_empty() && s.refine_model.is_empty() && s.refine_api_key.is_empty());
        assert_eq!(s.refine_provider, "openai", "默认执行体是 HTTP,老用户行为不变");
        assert_eq!(s.refine_agent, "claude");
        assert!(s.refine_agent_bin.is_empty() && s.refine_agent_model.is_empty());
        assert_eq!(ASR_PARAFORMER, "paraformer");
    }

    #[test]
    fn old_settings_json_without_refine_fields_loads() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("settings.json"), r#"{"asr_model":"whisper"}"#).unwrap();
        let s = load(dir.path());
        assert_eq!(s.asr_model, "whisper");
        assert!(!s.refine_enabled);
        assert_eq!(s.refine_provider, "openai", "缺字段回落 openai");
        assert_eq!(s.refine_agent, "claude");
    }

    #[test]
    fn mcp_fields_default_off_and_roundtrip() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("settings.json"), r#"{"asr_model":"whisper"}"#).unwrap();
        let s = load(tmp.path());
        assert!(!s.mcp_allow_control, "控制录制默认关(隐私敏感)");
        assert!(!s.mcp_onboarded);
        let s = Settings { mcp_allow_control: true, mcp_onboarded: true, ..Default::default() };
        save(tmp.path(), &s).unwrap();
        let got = load(tmp.path());
        assert!(got.mcp_allow_control && got.mcp_onboarded);
    }

    #[test]
    fn feature_guides_default_empty_and_roundtrip() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("settings.json"), r#"{"onboarded":true}"#).unwrap();
        let legacy = load(tmp.path());
        assert!(legacy.onboarded);
        assert!(legacy.completed_guides.is_empty(), "旧用户不能被视为已看过未来功能引导");

        let s = Settings {
            completed_guides: vec!["ai-tools-v1".into(), "future-feature-v1".into()],
            ..Default::default()
        };
        save(tmp.path(), &s).unwrap();
        assert_eq!(load(tmp.path()).completed_guides, s.completed_guides);
    }

    #[test]
    fn migrate_bumps_legacy_prefix_to_new_default() {
        let tmp = tempfile::tempdir().unwrap();
        // 存量:旧默认 ghproxy.net
        std::fs::write(
            tmp.path().join("settings.json"),
            format!(r#"{{"mirror_enabled":true,"mirror_prefix":"{LEGACY_MIRROR_PREFIX}"}}"#),
        )
        .unwrap();
        let got = migrate_mirror_prefix(tmp.path()).unwrap();
        assert_eq!(got.mirror_prefix, DEFAULT_MIRROR_PREFIX, "旧默认应被抬到新默认");
        assert_eq!(load(tmp.path()).mirror_prefix, DEFAULT_MIRROR_PREFIX, "已持久化");
    }

    #[test]
    fn migrate_leaves_non_legacy_prefix_untouched() {
        let tmp = tempfile::tempdir().unwrap();
        // 非旧默认值(模拟用户/未来自定义)不应被迁移改动。
        std::fs::write(
            tmp.path().join("settings.json"),
            r#"{"mirror_enabled":true,"mirror_prefix":"https://custom.example/"}"#,
        )
        .unwrap();
        let got = migrate_mirror_prefix(tmp.path()).unwrap();
        assert_eq!(got.mirror_prefix, "https://custom.example/", "自定义值不动");
    }

    #[test]
    fn migrate_is_idempotent_on_new_default() {
        let tmp = tempfile::tempdir().unwrap();
        // 无文件:load 得新默认;迁移后仍是新默认,不误改。
        let got = migrate_mirror_prefix(tmp.path()).unwrap();
        assert_eq!(got.mirror_prefix, DEFAULT_MIRROR_PREFIX);
        let again = migrate_mirror_prefix(tmp.path()).unwrap();
        assert_eq!(again.mirror_prefix, DEFAULT_MIRROR_PREFIX);
    }

    #[test]
    fn asr_provider_defaults_empty_and_roundtrips() {
        let tmp = tempfile::tempdir().unwrap();
        // 老配置缺字段 → 空串 = 不覆盖,沿用 sherpa 默认(CPU),行为与历史版本一致。
        std::fs::write(tmp.path().join("settings.json"), r#"{"asr_model":"whisper"}"#).unwrap();
        assert_eq!(load(tmp.path()).asr_provider, "");
        let s = Settings { asr_provider: "coreml".into(), ..Default::default() };
        save(tmp.path(), &s).unwrap();
        assert_eq!(load(tmp.path()).asr_provider, "coreml");
    }

    /// 遥测改为无开关常开(2026-07-29 产品决策):历史 settings.json 可能残留
    /// telemetry_enabled 键(含显式 false),反序列化必须静默忽略而非报错。
    #[test]
    fn legacy_telemetry_key_is_ignored() {
        let json = serde_json::to_string(&Settings::default()).unwrap();
        let mut v: serde_json::Value = serde_json::from_str(&json).unwrap();
        v.as_object_mut()
            .unwrap()
            .insert("telemetry_enabled".into(), serde_json::Value::Bool(false));
        assert!(serde_json::from_value::<Settings>(v).is_ok());
    }

    #[test]
    fn cloud_asr_fields_default_local_and_roundtrip() {
        let tmp = tempfile::tempdir().unwrap();
        // 老配置缺字段 → local + volcano + 空凭证,老用户行为零变化。
        std::fs::write(tmp.path().join("settings.json"), r#"{"asr_model":"whisper"}"#).unwrap();
        let s = load(tmp.path());
        assert_eq!(s.asr_mode, ASR_MODE_LOCAL);
        assert_eq!(s.cloud_asr_provider, CLOUD_VOLCANO);
        assert!(s.volc_app_key.is_empty() && s.volc_access_key.is_empty() && s.dashscope_api_key.is_empty());
        assert!(!cloud_creds_ok(&s), "空凭证不算就绪");
        let s = Settings {
            asr_mode: ASR_MODE_CLOUD.into(),
            cloud_asr_provider: CLOUD_ALIYUN.into(),
            dashscope_api_key: "sk-x".into(),
            ..Default::default()
        };
        assert!(cloud_creds_ok(&s), "阿里只看 dashscope_api_key");
        save(tmp.path(), &s).unwrap();
        let got = load(tmp.path());
        assert_eq!(got.asr_mode, "cloud");
        assert_eq!(got.cloud_asr_provider, "aliyun");
        assert_eq!(got.dashscope_api_key, "sk-x");
        // 火山凭证要求两个都非空。
        let v = Settings { cloud_asr_provider: CLOUD_VOLCANO.into(), volc_app_key: "a".into(), ..Default::default() };
        assert!(!cloud_creds_ok(&v));
        let v = Settings { volc_app_key: "a".into(), volc_access_key: "t".into(), ..Default::default() };
        assert!(cloud_creds_ok(&v));
    }

    #[test]
    fn mix_track_defaults_false_and_old_files_parse() {
        // 旧配置文件无该字段,必须仍可解析(仓库既有约定:新字段 serde(default))
        let s: Settings = serde_json::from_str("{}").expect("旧文件应可解析");
        assert!(!s.mix_track, "默认关闭:方案 B 是实验特性,不改变现有用户行为");
        // 上面走的是 serde 字段级 #[serde(default)](bool → false),不覆盖手写的
        // impl Default for Settings。lib.rs 的 app_data_dir() 不可用时读设置走
        // unwrap_or_default(),真正生效的是这里的手写默认值——必须单独断言,
        // 否则日后有人把 impl Default 里的 mix_track 改成 true,本测试仍然全绿。
        assert!(!Settings::default().mix_track, "读设置失败时走 unwrap_or_default,默认必须是关的");
    }
    #[test]
    fn calendar_match_defaults_to_true_for_legacy_settings() {
        // 旧 settings.json(无该键):default_true 兜底——裸 #[serde(default)] 会变 false。
        let s: Settings = serde_json::from_str("{}").unwrap();
        assert!(s.calendar_match_enabled);
        assert!(Settings::default().calendar_match_enabled);
        // 显式 false 尊重用户选择。
        let off: Settings = serde_json::from_str(r#"{"calendar_match_enabled":false}"#).unwrap();
        assert!(!off.calendar_match_enabled);
    }

    #[test]
    fn identify_auto_apply_defaults_to_false() {
        let s: Settings = serde_json::from_str("{}").unwrap();
        assert!(!s.identify_auto_apply, "自动应用必须默认关(评测数据门未过)");
        assert!(!Settings::default().identify_auto_apply);
    }

}