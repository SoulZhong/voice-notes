//! 轻量应用设置（app_data_dir/settings.json，原子写）。目前仅镜像加速配置。

use serde::{Deserialize, Serialize};
use std::io::Write;
use std::path::{Path, PathBuf};

/// 镜像加速前缀。曾经是可存盘/可迁移的字段(`Settings::mirror_prefix`),但 UI 从不
/// 允许用户编辑它——值永远等于内置默认,字段与其迁移逻辑(`migrate_mirror_prefix`)
/// 纯属历史包袱。三删一藏(配置项大梳理)改为编译期常量,不再落盘、不再有迁移代码。
pub const MIRROR_PREFIX: &str = "https://ghfast.top/";

/// ASR 模型选型标识,供 settings.asr_model 与后续选型逻辑复用。
pub const ASR_SENSE_VOICE: &str = "sense_voice";
// whisper 选型标识;models::required_now 已消费,判定 whisper 工件是否录制必需。
pub const ASR_WHISPER: &str = "whisper";
/// Paraformer-large 中文选型。
pub const ASR_PARAFORMER: &str = "paraformer";
/// Qwen3-ASR 0.6B int8 选型(52 语种/中英混说,LLM 解码,支持热词)。
pub const ASR_QWEN3: &str = "qwen3";
/// FireRedASR2-AED int8(2026-08-11 调研接入:中文精度天花板,带 token 时间戳)。
pub const ASR_FIRERED: &str = "firered";
/// 识别方式:本地模型 / 云端 API(spec 2026-07-29-cloud-asr-design)。
pub const ASR_MODE_LOCAL: &str = "local";
pub const ASR_MODE_CLOUD: &str = "cloud";
/// 本地+云端精修:录制实时走本地引擎,停录 Aing 前自动用云端批式对整场重转写
/// (spec 2026-08-11 用户拍板)。凭证不齐时二遍静默跳过,实时稿保留。
pub const ASR_MODE_LOCAL_CLOUD: &str = "local_cloud";

/// 云端二遍是否应当尝试(local_cloud 模式且凭证齐)。抽纯函数:Aing 前置二遍与
/// 手动重转写的引擎决策共用同一判据,不各写一份漂移。
pub fn cloud_second_pass_wanted(s: &Settings) -> bool {
    s.asr_mode == ASR_MODE_LOCAL_CLOUD && cloud_creds_ok(s)
}

// —— AI 执行体分层(2026-08-11 设计:docs/superpowers/specs/2026-08-11-ai-executor-separation-design.md)——
// 资源层(LlmProfile/AgentProfile)与功能层(refine_executor/relations_executor 引用)
// 解耦;所有 AI 功能(整理/标题/关系分析/未来纠错)一律经 resolve_executor 取执行体,
// 不再各自读散字段。

/// 在线模型档案。id 稳定(增删改不影响引用),label 用户可改。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LlmProfile {
    pub id: String,
    pub label: String,
    pub base_url: String,
    pub model: String,
    pub api_key: String,
}

/// 本机 Agent 档案:kind 即身份。bin 空 = 按常见安装位置自动探测;
/// model 空 = 该 CLI 自己的默认模型。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AgentProfile {
    pub kind: String,
    #[serde(default)]
    pub bin: String,
    #[serde(default)]
    pub model: String,
}

/// 引用执行体的 AI 功能。新功能 = 此枚举加一项 + settings 加一个 `<feature>_executor`。
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum AiFeature {
    Refine,
    Relations,
}

/// 解析后的执行体(provider 无关的消费面)。转 refine::llm::LlmConfig /
/// refine::agent::AgentKind 在调用方边缘做,settings 层不反向依赖 refine。
#[derive(Debug, Clone, PartialEq)]
pub enum ResolvedExecutor {
    Http { base_url: String, model: String, api_key: String },
    Agent { kind: String, bin: String, model: String },
}

/// 功能 → 执行体引用字符串(Relations 空引用回落 Refine)。
fn executor_ref<'a>(s: &'a Settings, f: AiFeature) -> &'a str {
    match f {
        AiFeature::Refine => s.refine_executor.trim(),
        AiFeature::Relations => {
            let r = s.relations_executor.trim();
            if r.is_empty() { s.refine_executor.trim() } else { r }
        }
    }
}

/// 执行体解析单一真源。引用悬空(profile 已删/未配置)→ None,调用方按未配置降级。
/// agent 引用即使无档案条目也解析成功(bin/model 走默认探测)——与旧
/// refine_agent_ready "bin 探测留给运行时" 的语义一致。
pub fn resolve_executor(s: &Settings, f: AiFeature) -> Option<ResolvedExecutor> {
    let r = executor_ref(s, f);
    if let Some(id) = r.strip_prefix("llm:") {
        s.llm_profiles.iter().find(|p| p.id == id).map(|p| ResolvedExecutor::Http {
            base_url: p.base_url.trim().to_string(),
            model: p.model.trim().to_string(),
            api_key: p.api_key.trim().to_string(),
        })
    } else if let Some(kind) = r.strip_prefix("agent:") {
        let ap = s.agent_profiles.iter().find(|a| a.kind == kind);
        Some(ResolvedExecutor::Agent {
            kind: kind.to_string(),
            bin: ap.map(|a| a.bin.trim().to_string()).unwrap_or_default(),
            model: ap.map(|a| a.model.trim().to_string()).unwrap_or_default(),
        })
    } else {
        None
    }
}

/// 执行体就绪:Http 三项齐;Agent 引用即就绪(bin 探测留运行时)。
/// 注意这里不含 refine_enabled——功能开关与执行体就绪是两回事,由调用方组合。
pub fn executor_ready(s: &Settings, f: AiFeature) -> bool {
    match resolve_executor(s, f) {
        Some(ResolvedExecutor::Http { base_url, model, api_key }) => {
            !base_url.is_empty() && !model.is_empty() && !api_key.is_empty()
        }
        Some(ResolvedExecutor::Agent { .. }) => true,
        None => false,
    }
}

/// 迁移用:旧单份 HTTP 配置生成的档案固定 id(迁移幂等的一部分)。
pub const MIGRATED_LLM_PROFILE_ID: &str = "p-migrated";

/// 按 base_url 猜服务商名做迁移档案的初始 label(用户可改,猜错无害)。
fn guess_llm_label(base_url: &str) -> String {
    let b = base_url.to_lowercase();
    for (needle, label) in [
        ("deepseek", "DeepSeek"),
        ("dashscope", "Qwen"),
        ("volces", "Doubao"),
        ("moonshot", "Kimi"),
        ("openai.com", "OpenAI"),
    ] {
        if b.contains(needle) {
            return label.to_string();
        }
    }
    "Custom".to_string()
}
/// 云端厂商标识。
pub const CLOUD_VOLCANO: &str = "volcano";
pub const CLOUD_ALIYUN: &str = "aliyun";

/// `Settings` 的 (反)序列化通过 `#[serde(from = "SettingsRepr")]` 整体路由:磁盘/前端 JSON
/// 先落到 `SettingsRepr`(逐字段带各自的迁移/默认语义,`audio_scheme` 在那边是纯 `Option`
/// 用于判键存在性),再经 `From<SettingsRepr>` 一次性 resolve 成这里的普通字段。
/// 这样 `Settings` 本身对外恒是自洽的:序列化必定带上每个字段的当前真值,反序列化
/// 出来的每个字段也必定是"resolve 过的具体值",不存在中间态——不再需要 Option 影子
/// 字段、`#[serde(skip)]`、或者在 `update()`/`save()` 里补写"回写"逻辑,从根上堵死了
/// "整体替换后公开字段与影子字段不同步导致互相覆盖"这类问题(2026-08-10 review 升级为
/// Critical:旧的 raw/skip 方案下,`set_settings` 提交"未改动 audio_scheme"这条最常见路径——
/// 磁盘 a、wire 原样带回 a——会被 `update()` 的对称判定误判为"闭包没碰 raw"从而不重新
/// resolve,而 skip 字段又已经在反序列化时被重置成类型默认值,最终把 a 悄悄存成默认值,
/// 且没有任何基于"闭包做了什么"的启发式能可靠区分这种路径与其它路径)。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(from = "SettingsRepr")]
pub struct Settings {
    pub mirror_enabled: bool,
    /// 自定义数据目录(录音/转写等落盘位置);None 时回退到 app_data_dir。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data_dir: Option<String>,
    /// 自定义模型目录覆盖;None 时使用内置默认路径。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub models_dir: Option<String>,
    /// ASR 选型,见 ASR_SENSE_VOICE / ASR_WHISPER。
    pub asr_model: String,
    /// sherpa 推理 provider 覆盖(实验字段,无 UI,手改 settings.json)。空 = sherpa
    /// 默认(0.6.8 硬编码 CPU);macOS 可填 "coreml" 实验加速(见 2026-07-28 ASR 调研)。
    /// 值原样透传 sherpa/onnxruntime,不做白名单;加载失败会走既有报错路径,不静默降级。
    pub asr_provider: String,
    /// 热词/上下文偏置词表:逗号或换行分隔的专名(人名/产品名/术语)。当前仅
    /// Qwen3-ASR 引擎消费(prompt 注入偏置);其余引擎忽略。空 = 不启用。
    /// 传给识别器时还会自动并入声纹库人名(见 lib.rs qwen3_hotwords)。
    pub asr_hotwords: String,
    /// 识别方式:"local"(默认,现状) / "cloud"。录制中禁改(set_settings 保护)。
    pub asr_mode: String,
    /// 云端厂商:"volcano" / "aliyun"。
    pub cloud_asr_provider: String,
    /// 火山凭证(APP ID / Access Token)。明文存储,同 refine_api_key 先例。
    pub volc_app_key: String,
    pub volc_access_key: String,
    /// 阿里 DashScope API Key。明文,同上。
    pub dashscope_api_key: String,
    /// 声纹嵌入模型选型:"campplus"(默认)/"eres2netv2"。不同模型嵌入空间不可混用,
    /// 切换会触发声纹库从录音样本后台重建(见 lib.rs set_settings)。
    pub speaker_model: String,
    /// 外观主题,消费任务:主题切换。"system"/"light"/"dark"。
    pub theme: String,
    /// UI 语言:"system"(跟随系统)/"zh"/"en"。前端界面与托盘/后端用户可见文案共用。
    /// 注意与 language_filter(转写乱码过滤)语义无关。
    pub ui_lang: String,
    /// 语言过滤开关,消费任务:转写语言过滤;默认开启。
    pub language_filter: bool,
    /// 全局快捷键开关,消费任务:快捷键;默认关闭(避免未经用户同意即占用系统快捷键)。
    pub shortcut_enabled: bool,
    /// 全局快捷键组合,消费任务:快捷键。
    pub shortcut: String,
    /// 系统托盘图标开关,消费任务:托盘;默认开启。
    pub tray_enabled: bool,
    /// 会后 LLM Aing 总开关(A2)。默认关,配好执行体后由用户打开。
    pub refine_enabled: bool,
    /// 资源层:在线模型档案(2026-08-11 执行体分层设计)。可被多个 AI 功能引用,
    /// 配置一次处处可用;api_key 明文存本机(单机应用,设置页已注明)。
    pub llm_profiles: Vec<LlmProfile>,
    /// 资源层:本机 Agent 档案。kind 即身份(claude|codex|gemini|cursor),
    /// 至多一条/种;探测状态是运行时信息,不落盘。
    pub agent_profiles: Vec<AgentProfile>,
    /// 功能层:AI 整理(含标题生成)的执行体引用。
    /// 格式 "llm:<profile_id>" | "agent:<kind>" | ""(未配置)。
    pub refine_executor: String,
    /// 功能层:关系分析执行体;空 = 跟随 refine_executor(默认,免二次配置)。
    pub relations_executor: String,
    /// 首启引导已完成(欢迎层「开始使用」下载完成或进入「高级设置」时置 true)。
    /// 老用户升级(字段缺失)反序列化为 false,但 layout 侧发现模型已就绪会静默补 true,
    /// 不会对老用户弹引导。
    pub onboarded: bool,
    /// 已完成的功能引导 ID。每项功能/重大版本独立记账，不能让一个全局 bool
    /// 永久吞掉后续新增功能的引导。
    pub completed_guides: Vec<String>,
    /// 允许 MCP(AI 助手)控制录制(start/stop/pause/resume)。默认关:开录是隐私
    /// 敏感操作,必须用户显式授权。
    pub mcp_allow_control: bool,
    /// MCP 接入引导已展示过(欢迎页步骤走完,或存量用户提示条被关闭)。
    pub mcp_onboarded: bool,
    /// 声音处理方案(spec 2026-08-10,2026-08-10 用户拍板默认翻 B):录制期混音与笔记页
    /// 默认回放的统一档位。a=双轨(不混音);ab=对照(混音,默认回放仍双轨);
    /// b=成品轨(默认,混音,默认回放成品轨)。混音开启后每分钟多约 1.9MB 磁盘
    /// (转码 m4a 后大幅缩小),仅影响新录制。普通字段,序列化恒写出当前真值——
    /// 键存在性迁移(旧 `mix_track` 布尔键、Codex P1#1 的"默认翻转后显式 b 不能被误判成
    /// 未设置")全部在 `SettingsRepr` → `Settings` 的 `From` 转换里一次性做完,见该 impl。
    pub audio_scheme: AudioScheme,
    /// 采集路径逃生舱(json-only 无 UI,同 asr_provider 先例):aec=普通输入+软件AEC(默认),
    /// vpio=系统通话模式(蓝牙击穿/设备格式不兼容时的手改退路)。
    pub capture_path: CapturePath,
    /// 音频自动保留期:到期笔记仅清音频轨(转写/精修稿永留)。默认永久。
    pub audio_retention: AudioRetention,
    /// P3 日历匹配:录制停止后按时间窗匹配日历事件(标题+参会人入 identify 先验)。
    /// 默认开——但真正生效还需系统日历授权(授权只能由设置页说明卡触发,自动
    /// 路径未授权即静默跳过),默认开不会造成 surprise 弹窗。
    pub calendar_match_enabled: bool,
    /// P2b 自动应用:high 档身份推断自动关联+回灌(回执可撤销)。默认关——
    /// 开启门槛是评测数据达标(spec:≥20 场标注、high 档 ≥50 样本误认 ≤1%),
    /// 由用户在设置页自行拨开。
    pub identify_auto_apply: bool,
}

/// `Settings` 反序列化的中间表征:逐字段镜像 `Settings`,携带解析期需要的默认值/重命名
/// 属性(原本挂在 `Settings` 字段上的那些 `#[serde(default = ...)]`/`rename` 全部搬到这里)。
/// 与 `Settings` 唯一的结构性差异是 `audio_scheme`(纯 `Option`,没有 rename,用来判"键
/// 是否在场")和额外的 `legacy_mix_track`(旧 `mix_track` 布尔键,只在这里出现,`Settings`
/// 上不再暴露)。`From<SettingsRepr> for Settings` 用显式逐字段映射(不用 `..Default::default()`
/// 展开),新增字段忘记在 `From` 里补上会直接编译失败,不会被 spread 语法悄悄吞掉。
///
/// **护栏:新增配置字段必须同时改三处**——`Settings`(公开字段)、`SettingsRepr`(本
/// 结构,带默认值/重命名)、`From<SettingsRepr> for Settings`(逐字段映射)。三处任一
/// 漏改都会编译失败(`Settings` 缺字段 / `From` 里少一行 struct literal 字段),这是
/// 故意设计的编译期后盾,不是三份重复代码的疏漏。
#[derive(Debug, Clone, Deserialize)]
struct SettingsRepr {
    #[serde(default = "default_true")]
    mirror_enabled: bool,
    #[serde(default)]
    data_dir: Option<String>,
    #[serde(default)]
    models_dir: Option<String>,
    #[serde(default = "default_asr")]
    asr_model: String,
    #[serde(default)]
    asr_provider: String,
    #[serde(default)]
    asr_hotwords: String,
    #[serde(default = "default_asr_mode")]
    asr_mode: String,
    #[serde(default = "default_cloud_provider")]
    cloud_asr_provider: String,
    #[serde(default)]
    volc_app_key: String,
    #[serde(default)]
    volc_access_key: String,
    #[serde(default)]
    dashscope_api_key: String,
    #[serde(default = "default_speaker_model")]
    speaker_model: String,
    #[serde(default = "default_theme")]
    theme: String,
    #[serde(default = "default_ui_lang")]
    ui_lang: String,
    #[serde(default = "default_true")]
    language_filter: bool,
    #[serde(default)]
    shortcut_enabled: bool,
    #[serde(default = "default_shortcut")]
    shortcut: String,
    #[serde(default = "default_true")]
    tray_enabled: bool,
    #[serde(default)]
    refine_enabled: bool,
    // —— 新键(2026-08-11 执行体分层) ——
    #[serde(default)]
    llm_profiles: Vec<LlmProfile>,
    #[serde(default)]
    agent_profiles: Vec<AgentProfile>,
    #[serde(default)]
    refine_executor: String,
    #[serde(default)]
    relations_executor: String,
    // —— 旧键(仅迁移期反序列化,Settings 不再持有,写出时不再出现) ——
    #[serde(default = "default_refine_provider")]
    refine_provider: String,
    #[serde(default = "default_refine_agent")]
    refine_agent: String,
    #[serde(default)]
    refine_agent_bin: String,
    #[serde(default)]
    refine_agent_model: String,
    #[serde(default)]
    refine_base_url: String,
    #[serde(default)]
    refine_model: String,
    #[serde(default)]
    refine_api_key: String,
    #[serde(default)]
    onboarded: bool,
    #[serde(default)]
    completed_guides: Vec<String>,
    #[serde(default)]
    mcp_allow_control: bool,
    #[serde(default)]
    mcp_onboarded: bool,
    /// 键存在性判定的核心:没有 rename(键名就是 "audio_scheme"),纯 `Option`——
    /// 在场则 `Some(任意值)`,不论是不是恰好等于新默认 B(Codex P1#1);缺失则 `None`,
    /// 由 `From` impl 再看 `legacy_mix_track` 决定落 Ab 还是新默认 B。
    #[serde(default)]
    audio_scheme: Option<AudioScheme>,
    /// 旧布尔键「录制期混出成品轨」(≤2026-08-09):只在这个中间表征里出现,`Settings`
    /// 本体不再暴露,也不会被序列化回磁盘(`Settings` 没有这个字段,自然写不出来)。
    #[serde(default, rename = "mix_track")]
    legacy_mix_track: Option<bool>,
    #[serde(default)]
    capture_path: CapturePath,
    #[serde(default)]
    audio_retention: AudioRetention,
    #[serde(default = "default_true")]
    calendar_match_enabled: bool,
    #[serde(default)]
    identify_auto_apply: bool,
}

impl From<SettingsRepr> for Settings {
    fn from(r: SettingsRepr) -> Self {
        // 旧 refine_* 散字段 → 资源档案 + executor 引用的一次性迁移。幂等判据:
        // 新键任一有内容(档案非空或 refine_executor 非空)即视为已迁移/新世界写入,
        // 完全忽略旧键——set_settings 回写的全量 JSON 只带新键,不会重复触发。
        // 三种老形态:纯 HTTP(base_url 非空)/纯 agent(provider=agent)/两者都配过
        // ——资源全迁,executor 按旧 provider 定向;失败面为零(纯字段搬运,无 IO)。
        let migrated = !r.llm_profiles.is_empty()
            || !r.agent_profiles.is_empty()
            || !r.refine_executor.trim().is_empty();
        let mut llm_profiles = r.llm_profiles.clone();
        let mut agent_profiles = r.agent_profiles.clone();
        let mut refine_executor = r.refine_executor.clone();
        if !migrated {
            if !r.refine_base_url.trim().is_empty() {
                llm_profiles.push(LlmProfile {
                    id: MIGRATED_LLM_PROFILE_ID.into(),
                    label: guess_llm_label(&r.refine_base_url),
                    base_url: r.refine_base_url.trim().into(),
                    model: r.refine_model.trim().into(),
                    api_key: r.refine_api_key.trim().into(),
                });
            }
            let agent_configured =
                !r.refine_agent_bin.trim().is_empty() || !r.refine_agent_model.trim().is_empty();
            if r.refine_provider == "agent" || agent_configured {
                agent_profiles.push(AgentProfile {
                    kind: r.refine_agent.clone(),
                    bin: r.refine_agent_bin.trim().into(),
                    model: r.refine_agent_model.trim().into(),
                });
            }
            refine_executor = if r.refine_provider == "agent" {
                format!("agent:{}", r.refine_agent)
            } else if !r.refine_base_url.trim().is_empty() {
                format!("llm:{MIGRATED_LLM_PROFILE_ID}")
            } else {
                String::new()
            };
        }
        Self {
            mirror_enabled: r.mirror_enabled,
            data_dir: r.data_dir,
            models_dir: r.models_dir,
            asr_model: r.asr_model,
            asr_provider: r.asr_provider,
            asr_hotwords: r.asr_hotwords,
            asr_mode: r.asr_mode,
            cloud_asr_provider: r.cloud_asr_provider,
            volc_app_key: r.volc_app_key,
            volc_access_key: r.volc_access_key,
            dashscope_api_key: r.dashscope_api_key,
            speaker_model: r.speaker_model,
            theme: r.theme,
            ui_lang: r.ui_lang,
            language_filter: r.language_filter,
            shortcut_enabled: r.shortcut_enabled,
            shortcut: r.shortcut,
            tray_enabled: r.tray_enabled,
            refine_enabled: r.refine_enabled,
            llm_profiles,
            agent_profiles,
            refine_executor,
            relations_executor: r.relations_executor,
            onboarded: r.onboarded,
            completed_guides: r.completed_guides,
            mcp_allow_control: r.mcp_allow_control,
            mcp_onboarded: r.mcp_onboarded,
            // 迁移 resolve 的唯一落点:键在场(`Some`)恒照旧,任意值都算用户显式选择,
            // 不受旧 `mix_track` 影响(Codex P1#1 的翻车组合:`{"audio_scheme":"b",
            // "mix_track":true}` 必须停在 B,不能被旧键拖回 Ab)。键缺失时才看旧
            // `mix_track`;都缺 → 新默认 B。
            audio_scheme: match r.audio_scheme {
                Some(v) => v,
                None => match r.legacy_mix_track {
                    Some(true) => AudioScheme::Ab,
                    _ => AudioScheme::B,
                },
            },
            capture_path: r.capture_path,
            audio_retention: r.audio_retention,
            calendar_match_enabled: r.calendar_match_enabled,
            identify_auto_apply: r.identify_auto_apply,
        }
    }
}

/// 声音处理方案档位。serde 小写:"a"/"ab"/"b"。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AudioScheme {
    A,
    Ab,
    #[default]
    B,
}

impl AudioScheme {
    /// 录制期是否混出成品轨(ab/b 档)。
    pub fn mix_track(self) -> bool {
        self != AudioScheme::A
    }
}

/// 采集路径逃生舱档位。serde 小写:"aec"/"vpio"。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CapturePath {
    #[default]
    Aec,
    Vpio,
}

/// 音频自动保留期档位。serde:"forever"/"90d"/"30d"。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum AudioRetention {
    #[default]
    #[serde(rename = "forever")]
    Forever,
    #[serde(rename = "90d")]
    D90,
    #[serde(rename = "30d")]
    D30,
}

impl AudioRetention {
    /// 保留天数;Forever = None(永不清理)。启动后台清理任务(lib.rs setup 内)据此
    /// 判定是否需要跑一次 `purge_audio_older_than`。
    pub fn days(self) -> Option<u32> {
        match self {
            Self::Forever => None,
            Self::D90 => Some(90),
            Self::D30 => Some(30),
        }
    }
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
/// `Default::default()`(bool → false)。language_filter/tray_enabled 等字段的
/// 产品默认值是 true,所以必须显式挂这个辅助函数,不能偷懒裸写 default。
fn default_true() -> bool {
    true
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            mirror_enabled: true,
            data_dir: None,
            models_dir: None,
            asr_model: default_asr(),
            asr_provider: String::new(),
            asr_hotwords: String::new(),
            asr_mode: default_asr_mode(),
            cloud_asr_provider: default_cloud_provider(),
            volc_app_key: String::new(),
            volc_access_key: String::new(),
            dashscope_api_key: String::new(),
            speaker_model: default_speaker_model(),
            theme: default_theme(),
            ui_lang: default_ui_lang(),
            language_filter: true,
            shortcut_enabled: false,
            shortcut: default_shortcut(),
            tray_enabled: true,
            refine_enabled: false,
            llm_profiles: Vec::new(),
            agent_profiles: Vec::new(),
            refine_executor: String::new(),
            relations_executor: String::new(),
            onboarded: false,
            completed_guides: Vec::new(),
            mcp_allow_control: false,
            mcp_onboarded: false,
            audio_scheme: AudioScheme::B,
            capture_path: CapturePath::Aec,
            audio_retention: AudioRetention::Forever,
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

/// 旧格式判定关键字:任一存在即视为"升级前"的文件,触发一次性整文件备份。
const LEGACY_MARKERS: [&str; 5] = [
    "\"mix_track\"",
    "\"keep_audio\"",
    "\"record_system_only\"",
    "\"keep_output_volume\"",
    "\"mirror_prefix\"",
];

/// 启动自愈探测:磁盘文件存在,且(整体反序列化失败 或 命中任一 `LEGACY_MARKERS` 关键字)。
///
/// 为什么需要这个探测:`load()` 对损坏/旧格式文件只在内存里 salvage/迁移,从不回写
/// 磁盘——这是它的正确职责边界(纯读路径不该有写副作用)。但这意味着,如果调用方
/// 只是反复 `load()` 而从不落盘一次,同一份坏文件每次启动都会重新触发"整体解析失败→
/// 尸检备份"流程,在 app_data 目录里累积无穷多具 `settings.json.corrupt-*` 尸体,旧键
/// 也永远学不会离开磁盘(2026-08-10 review Important:setup 里的启动调用一度退化成
/// 纯 `load`,这条自愈链路随之消失)。调用方据此判断是否需要追加一次性
/// `update(&d, |_| {})`,把 salvage/迁移结果落盘——之后磁盘上就是干净的新格式,后续
/// 启动 `needs_heal` 归 false,不再重复触发抢救,也不再新增尸体。
///
/// 只在文件确实存在时才可能返回 true:全新安装(没有 settings.json)没有"愈合"这回事,
/// 不能被这个探测误判去触发一次无意义的落盘。
pub fn needs_heal(app_data: &Path) -> bool {
    let path = app_data.join("settings.json");
    let Ok(text) = std::fs::read_to_string(&path) else {
        return false;
    };
    serde_json::from_str::<Settings>(&text).is_err() || LEGACY_MARKERS.iter().any(|k| text.contains(k))
}

/// 尸检文件序号:同一进程内短时间连续触发抢救(比如批量场景,或一次 load 里先后遇到
/// 类型错和截断两种坏文件)按 unix 秒起名会撞同名,后一次覆盖掉前一次的尸体——叠加
/// 进程内单调计数,保证同一秒内多次调用文件名也不同,不互相覆盖(Minor 3)。
static CORRUPT_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// 含凭证的整文件副本落盘,权限须与正文 settings.json 一致(0600,仅本人可读)。
/// 升级备份(`.bak-pre-overhaul`)与尸检(`.corrupt-*`)都是逐字节拷贝,若沿用
/// `std::fs::write` 默认的 0644,会把 API key 等凭证经这两份 derivative 泄露给
/// 同机其它用户——这正是 save() 特意收紧正文权限要防的事,备份/尸检不能是漏洞。
fn write_owner_only(path: &Path, content: &str) -> std::io::Result<()> {
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        // 同 save() 的理由:mode() 只对新建文件生效，已存在的文件(比如上次崩溃残留)
        // 权限可能更宽，显式收紧。
        file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
    }
    file.write_all(content.as_bytes())?;
    Ok(())
}

/// 缺失/损坏 → 默认值（容忍，不报错）。旧 mix_track 布尔键在此迁移(见 `SettingsRepr` →
/// `Settings` 的 `From` 实现)。升级备份 + 逐字段抢救,详见 `salvage`。反序列化本身已经
/// 自洽(`Settings` 的 `#[serde(from = "SettingsRepr")]`),这里不需要再补一步 resolve。
pub fn load(app_data: &Path) -> Settings {
    let path = app_data.join("settings.json");
    let raw = std::fs::read_to_string(&path).ok();
    // 升级备份:旧格式文件(命中任一旧键关键字)首次见到即整文件备份,已有备份不覆盖
    //(Codex P1#2:save 会立刻用新键覆盖/抹掉旧键,备份是升级后唯一的手工回退路径)。
    if let Some(text) = &raw {
        let looks_legacy = LEGACY_MARKERS.iter().any(|k| text.contains(k));
        let bak = app_data.join("settings.json.bak-pre-overhaul");
        if looks_legacy && !bak.exists() {
            let _ = write_owner_only(&bak, text);
        }
    }
    match raw.as_deref().map(serde_json::from_str::<Settings>) {
        Some(Ok(s)) => s,
        Some(Err(e)) => {
            // 整对象反序列化失败(单字段类型错也会拖垮整体,Codex P1#9)→ 尸检备份 +
            // 逐字段抢救,不能静默整体重置(会把凭证等好字段一起丢)。
            eprintln!("settings.json 解析失败,逐字段抢救: {e}");
            let ts = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0);
            let seq = CORRUPT_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let _ = write_owner_only(
                &app_data.join(format!("settings.json.corrupt-{ts}-{seq}")),
                raw.as_deref().unwrap_or(""),
            );
            salvage(raw.as_deref().unwrap_or(""))
        }
        None => Settings::default(),
    }
}

/// 逐字段抢救:整体 JSON 不合法或字段类型错时,能从 Value 读出的字段保留,读不出的用默认。
/// 增量叠加而非"整体覆盖再逐键剔除试探":后一种做法(剔除法)每一步只检验"剔除/替换这个
/// 键之后能否整体反序列化成功",而不比较剔除前是否本就能成功——对一个本来就合法的好字段
/// (比如 theme:"dark"),把它替换成默认值 "system" 之后一样能整体解析成功,于是会被贪心
/// 判定为"坏源"一并打回默认,好字段反而丢失。这里改用从自洽的默认对象出发、逐键尝试叠加
/// 原始值的增量法:只有"叠加后仍能整体解析"的字段才会被采纳,天然不会误伤好字段。
fn salvage(text: &str) -> Settings {
    let Ok(v) = serde_json::from_str::<serde_json::Value>(text) else {
        return Settings::default();
    };
    let Some(obj) = v.as_object() else {
        return Settings::default();
    };
    let mut base = match serde_json::to_value(Settings::default()) {
        Ok(serde_json::Value::Object(m)) => m,
        _ => return Settings::default(),
    };
    // `base` 来自 `Settings::default()` 序列化,必然带着 "audio_scheme":"b"(新默认)——
    // 若不处理,一份根本没有 audio_scheme 键的坏源文件(比如
    // `{"mix_track":true,"theme":123}`)会被这份 base 快照的默认值抢跑,抢救出 B
    // 而不是走 `SettingsRepr` 的 None 分支去看 legacy_mix_track,本该迁移成 Ab 的
    // 存量反而丢了迁移结果。源对象缺这个键时,先从 base 里也去掉它,让最终反序列化
    // 落回 repr 的键存在性判定(同 `load()` 正常路径的迁移语义)。
    if !obj.contains_key("audio_scheme") {
        base.remove("audio_scheme");
    }
    for (k, val) in obj {
        let mut probe = base.clone();
        probe.insert(k.clone(), val.clone());
        if serde_json::from_value::<Settings>(serde_json::Value::Object(probe.clone())).is_ok() {
            base = probe;
        }
    }
    serde_json::from_value(serde_json::Value::Object(base)).unwrap_or_default()
}

pub fn save(app_data: &Path, s: &Settings) -> anyhow::Result<()> {
    std::fs::create_dir_all(app_data)?;
    // audio_scheme 现在是普通字段,Serialize 恒写出当前真值——不再需要"从公开字段回写
    // 影子 raw 字段"这一步,序列化结果天然自洽,键不会因为任何 skip 机关而消失。
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
///
/// 不再需要针对 `audio_scheme` 的特判(对比旧版本:曾经因为影子 raw 字段 + `#[serde(skip)]`
/// 公开字段这套机关,需要在这里判断闭包到底改没改 raw 才决定要不要重新 resolve——
/// 但这个"猜闭包意图"的启发式挡不住 `set_settings` 整体替换且档位未变这条最常见路径
/// (磁盘 a、闭包整体换成 wire 里同样是 a 的 `Settings`:raw 前后相等,判定为"没改",
/// 于是不重新 resolve,而 skip 字段在反序列化时已经被悄悄重置成类型默认值,最终把 a
/// 存丢),2026-08-10 review 升级为 Critical。改用 `#[serde(from = "SettingsRepr")]`
/// 后,`f(&mut s)` 不管是整体替换 `*s = new_settings` 还是直接改单个字段 `s.audio_scheme
/// = X`,`s.audio_scheme` 在闭包跑完之后永远就是调用方想要的最终值——因为它不再有
/// "反序列化不经过它"这个特例,`save()` 也不需要再从别的字段回写它。
pub fn update(app_data: &Path, f: impl FnOnce(&mut Settings)) -> anyhow::Result<Settings> {
    let _guard = WRITE_LOCK.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let mut s = load(app_data);
    f(&mut s);
    save(app_data, &s)?;
    Ok(s)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn load_missing_or_corrupt_falls_back_to_default() {
        let tmp = tempfile::tempdir().unwrap();
        let s = load(tmp.path());
        assert!(s.mirror_enabled);
        std::fs::write(tmp.path().join("settings.json"), "not json").unwrap();
        assert!(load(tmp.path()).mirror_enabled, "损坏 → 默认值");
    }

    #[test]
    fn save_then_load_roundtrip() {
        let tmp = tempfile::tempdir().unwrap();
        let s = Settings { mirror_enabled: true, ..Default::default() };
        save(tmp.path(), &s).unwrap();
        let got = load(tmp.path());
        assert!(got.mirror_enabled);
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
        std::fs::write(tmp.path().join("settings.json"), r#"{"mirror_enabled":false}"#).unwrap();
        let s = load(tmp.path());
        assert_eq!(s.theme, "system");
        assert_eq!(s.ui_lang, "system", "老配置缺 ui_lang 应回落跟随系统");
        assert!(s.language_filter);
        assert!(!s.shortcut_enabled);
        assert_eq!(s.shortcut, "Alt+CmdOrCtrl+R");
        assert!(s.tray_enabled);
        let s = Settings { theme: "dark".into(), ui_lang: "en".into(),
            language_filter: false, shortcut_enabled: true,
            shortcut: "Alt+CmdOrCtrl+K".into(), tray_enabled: false, ..Default::default() };
        save(tmp.path(), &s).unwrap();
        let got = load(tmp.path());
        assert_eq!(got.theme, "dark");
        assert_eq!(got.ui_lang, "en");
        assert!(!got.language_filter);
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
        assert!(s.llm_profiles.is_empty() && s.agent_profiles.is_empty());
        assert!(s.refine_executor.is_empty() && s.relations_executor.is_empty());
        assert_eq!(resolve_executor(&s, AiFeature::Refine), None, "未配置 → 无执行体");
        assert!(!executor_ready(&s, AiFeature::Refine));
        assert_eq!(ASR_PARAFORMER, "paraformer");
    }

    #[test]
    fn old_settings_json_without_refine_fields_loads() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("settings.json"), r#"{"asr_model":"whisper"}"#).unwrap();
        let s = load(dir.path());
        assert_eq!(s.asr_model, "whisper");
        assert!(!s.refine_enabled);
        assert!(s.llm_profiles.is_empty() && s.refine_executor.is_empty(), "无旧配置无可迁移");
    }

    // —— 执行体分层:迁移与解析(2026-08-11 设计) ——

    #[test]
    fn migration_pure_http_becomes_profile_and_executor() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("settings.json"),
            r#"{"refine_enabled":true,"refine_provider":"openai",
                "refine_base_url":"https://api.deepseek.com/v1",
                "refine_model":"deepseek-chat","refine_api_key":"sk-x"}"#,
        )
        .unwrap();
        let s = load(dir.path());
        assert_eq!(s.llm_profiles.len(), 1);
        let p = &s.llm_profiles[0];
        assert_eq!((p.id.as_str(), p.label.as_str()), (MIGRATED_LLM_PROFILE_ID, "DeepSeek"));
        assert_eq!(s.refine_executor, format!("llm:{MIGRATED_LLM_PROFILE_ID}"));
        assert_eq!(
            resolve_executor(&s, AiFeature::Refine),
            Some(ResolvedExecutor::Http {
                base_url: "https://api.deepseek.com/v1".into(),
                model: "deepseek-chat".into(),
                api_key: "sk-x".into()
            })
        );
        assert!(executor_ready(&s, AiFeature::Refine));
        // Relations 空引用回落 Refine。
        assert_eq!(resolve_executor(&s, AiFeature::Relations), resolve_executor(&s, AiFeature::Refine));
    }

    #[test]
    fn migration_agent_and_mixed_configs() {
        // 纯 agent。
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("settings.json"),
            r#"{"refine_provider":"agent","refine_agent":"codex","refine_agent_model":"gpt-5-codex"}"#,
        )
        .unwrap();
        let s = load(dir.path());
        assert_eq!(s.refine_executor, "agent:codex");
        assert_eq!(s.agent_profiles.len(), 1);
        assert_eq!(
            resolve_executor(&s, AiFeature::Refine),
            Some(ResolvedExecutor::Agent { kind: "codex".into(), bin: String::new(), model: "gpt-5-codex".into() })
        );
        // 两者都配过:资源全迁,executor 按旧 provider(agent)定向。
        let dir2 = tempfile::tempdir().unwrap();
        std::fs::write(
            dir2.path().join("settings.json"),
            r#"{"refine_provider":"agent","refine_agent":"claude",
                "refine_base_url":"https://api.moonshot.cn/v1","refine_model":"m","refine_api_key":"k"}"#,
        )
        .unwrap();
        let s2 = load(dir2.path());
        assert_eq!(s2.llm_profiles.len(), 1, "HTTP 配置也保留为档案");
        assert_eq!(s2.llm_profiles[0].label, "Kimi");
        assert_eq!(s2.refine_executor, "agent:claude");
    }

    #[test]
    fn migration_idempotent_new_keys_win() {
        // 新旧键并存(迁移后又被旧版本写过旧键的极端情形):新键在场即忽略旧键。
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("settings.json"),
            r#"{"llm_profiles":[{"id":"p1","label":"A","base_url":"https://a/v1","model":"m1","api_key":"k1"}],
                "refine_executor":"llm:p1",
                "refine_base_url":"https://legacy/v1","refine_model":"legacy","refine_api_key":"lk"}"#,
        )
        .unwrap();
        let s = load(dir.path());
        assert_eq!(s.llm_profiles.len(), 1, "不重复生成迁移档案");
        assert_eq!(s.llm_profiles[0].id, "p1");
        assert_eq!(s.refine_executor, "llm:p1");
        // 落盘回读(新世界只写新键)仍稳定。
        save(dir.path(), &s).unwrap();
        let s2 = load(dir.path());
        assert_eq!(s2.llm_profiles, s.llm_profiles);
        assert_eq!(s2.refine_executor, "llm:p1");
    }

    #[test]
    fn executor_resolution_edge_cases() {
        let mut s = Settings::default();
        s.llm_profiles.push(LlmProfile {
            id: "p1".into(),
            label: "A".into(),
            base_url: "https://a/v1".into(),
            model: "m".into(),
            api_key: String::new(),
        });
        // 悬空引用 → None(profile 已删)。
        s.refine_executor = "llm:ghost".into();
        assert_eq!(resolve_executor(&s, AiFeature::Refine), None);
        // key 缺 → 可解析但未就绪。
        s.refine_executor = "llm:p1".into();
        assert!(resolve_executor(&s, AiFeature::Refine).is_some());
        assert!(!executor_ready(&s, AiFeature::Refine), "HTTP 缺 key 未就绪");
        // agent 引用无档案条目:解析成功(bin/model 空,探测留运行时),即就绪。
        s.refine_executor = "agent:claude".into();
        assert!(executor_ready(&s, AiFeature::Refine));
        // Relations 显式引用优先于跟随。
        s.relations_executor = "llm:p1".into();
        assert!(matches!(
            resolve_executor(&s, AiFeature::Relations),
            Some(ResolvedExecutor::Http { .. })
        ));
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
    fn cloud_second_pass_only_in_local_cloud_mode_with_creds() {
        // local_cloud + 凭证齐 → 二遍;缺任一条件都不做(录制照常,不挡不弹)。
        let ready = Settings {
            asr_mode: ASR_MODE_LOCAL_CLOUD.into(),
            cloud_asr_provider: CLOUD_ALIYUN.into(),
            dashscope_api_key: "sk-x".into(),
            ..Default::default()
        };
        assert!(cloud_second_pass_wanted(&ready));
        let no_creds = Settings { asr_mode: ASR_MODE_LOCAL_CLOUD.into(), ..Default::default() };
        assert!(!cloud_second_pass_wanted(&no_creds), "凭证不齐:静默跳过二遍");
        let cloud_only = Settings {
            asr_mode: ASR_MODE_CLOUD.into(),
            cloud_asr_provider: CLOUD_ALIYUN.into(),
            dashscope_api_key: "sk-x".into(),
            ..Default::default()
        };
        assert!(!cloud_second_pass_wanted(&cloud_only), "纯云端模式识别已在云端,无二遍");
        // local_cloud 的 serde 往返:字符串字段,老版本读到未知值也只是当普通字符串。
        let tmp = tempfile::tempdir().unwrap();
        save(tmp.path(), &ready).unwrap();
        assert_eq!(load(tmp.path()).asr_mode, ASR_MODE_LOCAL_CLOUD);
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
    fn audio_scheme_defaults_b_for_fresh_and_untouched_files() {
        // 全新安装(无文件)与旧默认文件(空对象)都落新默认 B
        let tmp = tempfile::tempdir().unwrap();
        assert_eq!(load(tmp.path()).audio_scheme, AudioScheme::B);
        std::fs::write(tmp.path().join("settings.json"), "{}").unwrap();
        assert_eq!(load(tmp.path()).audio_scheme, AudioScheme::B);
        assert_eq!(Settings::default().audio_scheme, AudioScheme::B, "unwrap 兜底口径");
    }

    #[test]
    fn explicit_audio_scheme_always_wins_regardless_of_legacy() {
        // 键在场任意值照旧——含与陈旧 mix_track 并存(Codex P1#1 的翻车组合)
        for (raw, want) in [
            (r#"{"audio_scheme":"a"}"#, AudioScheme::A),
            (r#"{"audio_scheme":"b","mix_track":true}"#, AudioScheme::B),
            (r#"{"audio_scheme":"ab","mix_track":false}"#, AudioScheme::Ab),
        ] {
            let tmp = tempfile::tempdir().unwrap();
            std::fs::write(tmp.path().join("settings.json"), raw).unwrap();
            assert_eq!(load(tmp.path()).audio_scheme, want, "raw={raw}");
        }
    }

    #[test]
    fn legacy_mix_track_migrates_only_when_new_key_absent() {
        for (raw, want) in [
            (r#"{"mix_track":true}"#, AudioScheme::Ab),
            (r#"{"mix_track":false}"#, AudioScheme::B), // 旧默认非显式选择,随新默认(用户拍板)
        ] {
            let tmp = tempfile::tempdir().unwrap();
            std::fs::write(tmp.path().join("settings.json"), raw).unwrap();
            assert_eq!(load(tmp.path()).audio_scheme, want, "raw={raw}");
        }
    }

    #[test]
    fn load_backs_up_original_file_once_before_first_rewrite() {
        // 升级备份:load 发现旧格式(存在 mix_track 键)时拷贝一份,
        // 已有备份不覆盖(Codex P1#2:save 会立刻抹掉旧键,备份是唯一回退路径)
        let tmp = tempfile::tempdir().unwrap();
        let orig = r#"{"mix_track":true,"keep_audio":false}"#;
        std::fs::write(tmp.path().join("settings.json"), orig).unwrap();
        let _ = load(tmp.path());
        let bak = tmp.path().join("settings.json.bak-pre-overhaul");
        assert_eq!(std::fs::read_to_string(&bak).unwrap(), orig, "备份=原文");
        // 二次 load(备份已存在)不得覆盖
        std::fs::write(tmp.path().join("settings.json"), r#"{"audio_scheme":"a"}"#).unwrap();
        let _ = load(tmp.path());
        assert_eq!(std::fs::read_to_string(&bak).unwrap(), orig, "备份不被后续覆盖");
    }

    #[test]
    fn corrupt_file_is_salvaged_field_by_field_not_reset() {
        // 单字段类型错不再拖垮整对象(Codex P1#9):坏字段用默认,好字段保留
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(
            tmp.path().join("settings.json"),
            r#"{"audio_scheme":123,"dashscope_api_key":"sk-live","theme":"dark"}"#,
        )
        .unwrap();
        let s = load(tmp.path());
        assert_eq!(s.audio_scheme, AudioScheme::B, "坏字段回默认");
        assert_eq!(s.dashscope_api_key, "sk-live", "好字段(凭证)不得丢");
        assert_eq!(s.theme, "dark");
        // 上面这次 load 本身(audio_scheme:123 类型错)也会触发一次尸检写入;截断段要
        // 证明的是"这次 load 自己又新留了一具尸体",不能靠"目录里非空"这种弱断言被上一次
        // 遗留的尸体蒙混过关(Minor 3)——改成比较该段前后的尸检文件集合确实有新增。
        let corpse_names = |dir: &Path| -> std::collections::HashSet<std::ffi::OsString> {
            std::fs::read_dir(dir)
                .unwrap()
                .filter_map(|e| e.ok())
                .map(|e| e.file_name())
                .filter(|n| n.to_string_lossy().starts_with("settings.json.corrupt-"))
                .collect()
        };
        let before = corpse_names(tmp.path());
        // 整文件截断:抢救不出任何字段 → 默认,但坏文件要留尸检
        std::fs::write(tmp.path().join("settings.json"), r#"{"broken"#).unwrap();
        let s = load(tmp.path());
        assert_eq!(s.audio_scheme, AudioScheme::B);
        let after = corpse_names(tmp.path());
        assert!(
            after.len() > before.len(),
            "截断文件须新增一份 corrupt-* 尸检,而非复用/掩盖之前的尸体:before={before:?} after={after:?}"
        );
    }

    #[test]
    fn capture_path_and_retention_default_and_parse() {
        let s: Settings = serde_json::from_str("{}").unwrap();
        assert_eq!(s.capture_path, CapturePath::Aec);
        assert_eq!(s.audio_retention, AudioRetention::Forever);
        let s: Settings =
            serde_json::from_str(r#"{"capture_path":"vpio","audio_retention":"30d"}"#).unwrap();
        assert_eq!(s.capture_path, CapturePath::Vpio);
        assert_eq!(s.audio_retention, AudioRetention::D30);
    }

    #[test]
    fn save_reload_roundtrip_preserves_explicit_scheme_including_default_valued_b() {
        // save→reload 必须保住显式值,含"恰好等于新默认"的显式 b(不能被误判成未设置而
        // 走迁移分支,这正是 P1#1 要防的翻车)。
        let tmp = tempfile::tempdir().unwrap();
        let s = Settings { audio_scheme: AudioScheme::A, ..Default::default() };
        save(tmp.path(), &s).unwrap();
        assert_eq!(load(tmp.path()).audio_scheme, AudioScheme::A, "非默认显式值须存活");

        let tmp = tempfile::tempdir().unwrap();
        let s = Settings { audio_scheme: AudioScheme::B, ..Default::default() };
        save(tmp.path(), &s).unwrap();
        assert_eq!(load(tmp.path()).audio_scheme, AudioScheme::B, "默认值同样须显式存活");
        let raw = std::fs::read_to_string(tmp.path().join("settings.json")).unwrap();
        assert!(!raw.contains("mix_track"), "旧键不得再写盘: {raw}");
        assert!(raw.contains(r#""audio_scheme": "b""#), "落盘小写契约,真实序列化格式: {raw}");
    }

    #[test]
    fn explicit_b_file_survives_save_reload_and_key_never_vanishes() {
        // 保护 skip/raw 同步:显式写了 "audio_scheme":"b" 的文件,load→save→reload 后
        // 值仍是 b,且磁盘文件必须继续含 audio_scheme 键(不能因为 skip 序列化把键写丢)。
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("settings.json"), r#"{"audio_scheme":"b"}"#).unwrap();
        let s = load(tmp.path());
        assert_eq!(s.audio_scheme, AudioScheme::B);
        save(tmp.path(), &s).unwrap();
        let raw = std::fs::read_to_string(tmp.path().join("settings.json")).unwrap();
        assert!(raw.contains("\"audio_scheme\""), "audio_scheme 键不得从磁盘消失: {raw}");
        assert_eq!(load(tmp.path()).audio_scheme, AudioScheme::B, "reload 后仍是 b");
    }

    #[test]
    fn update_full_struct_replace_like_set_settings_keeps_submitted_audio_scheme() {
        // 复刻 lib.rs::set_settings 的写入形状:闭包整体替换 `*s = new_settings`
        //(new_settings 由前端 JSON 反序列化而来,提交的新档位与磁盘存量不同)。改用
        // `#[serde(from = "SettingsRepr")]` 之后,`new_settings.audio_scheme` 在反序列化
        // 那一刻就已经是 resolve 过的具体值(键存在性判定在 `SettingsRepr::From` 里做完),
        // 不存在"公开字段被重置成类型默认值、事后还要在 update() 里补一次 resolve"这回事——
        // 这个测试锁的是"整体替换且档位确实变了"这条路径依旧正确。
        let tmp = tempfile::tempdir().unwrap();
        // 存量:已显式选过 b(新默认,当心测试值不要恰好等于 AudioScheme::default(),
        // 否则回归会被类型默认值巧合掩盖,测试失去意义)。
        std::fs::write(tmp.path().join("settings.json"), r#"{"audio_scheme":"b"}"#).unwrap();
        assert_eq!(load(tmp.path()).audio_scheme, AudioScheme::B);

        // 前端把新值经 JSON 传回来,模拟 tauri command 参数
        // `new_settings: settings::Settings` 的反序列化产物。刻意选与磁盘存量不同的档位。
        let wire = r#"{"audio_scheme":"a"}"#;
        let new_settings: Settings = serde_json::from_str(wire).unwrap();

        let got = update(tmp.path(), |s| {
            *s = new_settings.clone();
        })
        .unwrap();
        assert_eq!(got.audio_scheme, AudioScheme::A, "update() 返回值须是前端刚提交的档位");
        assert_eq!(load(tmp.path()).audio_scheme, AudioScheme::A, "落盘也须是前端提交的档位");
    }

    #[test]
    fn update_closure_writing_audio_scheme_field_directly_is_not_overwritten() {
        // 闭包直接改公开字段 `s.audio_scheme = X`(不同于 set_settings 的整体替换)这种
        // 写法。旧的 raw/skip 方案下 update() 需要一段"猜闭包有没有碰 raw"的对称判定
        // 才能不吞掉这种写法;现在 `audio_scheme` 是普通字段,`update()` 里完全没有特判,
        // 这个场景"自动"就是对的——保留这个测试是为了锁住"不要再往 update() 里加特判"
        // 这个不变式,回归会立刻在这里炸。
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("settings.json"), r#"{"audio_scheme":"b"}"#).unwrap();
        assert_eq!(load(tmp.path()).audio_scheme, AudioScheme::B);

        let got = update(tmp.path(), |s| s.audio_scheme = AudioScheme::A).unwrap();
        assert_eq!(got.audio_scheme, AudioScheme::A, "闭包直接写公开字段的改动不能被吞");
        assert_eq!(load(tmp.path()).audio_scheme, AudioScheme::A, "落盘也须是闭包写入的值");
    }

    #[test]
    fn full_replace_with_unchanged_scheme_preserves_disk_value() {
        // set_settings 最常见的主路径,也是 2026-08-10 review 升级为 Critical 的那条:
        // 磁盘 a,用户只改了别的设置,前端把整份 Settings(含未改动的 audio_scheme:"a")
        // 原样带回来,`*s = wire` 整体替换。旧的 raw/skip 方案下,这条路径 raw 前后相等
        // (都是 Some(A)),会被 update() 的"raw 没变就不 resolve"判定为"闭包没碰它",
        // 于是不重新 resolve;但 skip 字段在 `new_settings` 反序列化那一刻已经被重置成
        // 类型默认值(B),没人把它纠正回来,save() 又是"从公开字段回写 raw",于是把 A
        // 静默存成了 B。serde(from) 重建后,`wire.audio_scheme` 从反序列化那一刻起就已经
        // 是 A(没有中间态),这个类别的 bug 从架构上不可能再发生。
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("settings.json"), r#"{"audio_scheme":"a"}"#).unwrap();
        let wire: Settings = serde_json::from_str(r#"{"audio_scheme":"a"}"#).unwrap();
        update(tmp.path(), |s| *s = wire.clone()).unwrap();
        assert_eq!(load(tmp.path()).audio_scheme, AudioScheme::A);
    }

    #[cfg(unix)]
    #[test]
    fn backup_and_corrupt_files_are_owner_only_like_settings_json() {
        // 备份/尸检是含凭证的整文件逐字节拷贝,权限不能比正文本体(0600)更宽松,
        // 否则凭证经这两份 derivative 泄露给同机其它用户(Review Important 2)。
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();

        std::fs::write(
            tmp.path().join("settings.json"),
            r#"{"mix_track":true,"dashscope_api_key":"sk-live"}"#,
        )
        .unwrap();
        let _ = load(tmp.path());
        let bak = tmp.path().join("settings.json.bak-pre-overhaul");
        let mode = std::fs::metadata(&bak).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "升级备份文件含凭证,权限须与正文一致");

        std::fs::write(tmp.path().join("settings.json"), r#"{"broken"#).unwrap();
        let _ = load(tmp.path());
        let corpse = std::fs::read_dir(tmp.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .find(|e| e.file_name().to_string_lossy().starts_with("settings.json.corrupt-"))
            .expect("尸检文件应存在");
        let mode = std::fs::metadata(corpse.path()).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "尸检文件含凭证,权限须与正文一致");
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

    /// 三删一藏(配置项大梳理):`keep_audio`/`record_system_only`/`keep_output_volume`/
    /// `mirror_prefix` 四个旧键从 `Settings`/`SettingsRepr` 上彻底移除后,存量文件里
    /// 携带这些键必须仍能正常 load(serde 未知字段默认忽略,不报错),且 save 落盘后
    /// 这些键不得复活(Settings 已没有对应字段,序列化天然写不出)。
    #[test]
    fn deleted_legacy_keys_still_parse_and_are_dropped_on_save() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(
            tmp.path().join("settings.json"),
            r#"{"keep_audio":false,"record_system_only":true,"keep_output_volume":true,"mirror_prefix":"https://x/"}"#,
        )
        .unwrap();
        let s = load(tmp.path());
        save(tmp.path(), &s).unwrap();
        let raw = std::fs::read_to_string(tmp.path().join("settings.json")).unwrap();
        for k in ["keep_audio", "record_system_only", "keep_output_volume", "mirror_prefix"] {
            assert!(!raw.contains(k), "旧键 {k} 不得复活: {raw}");
        }
        // 且备份已存在(Task 1 的 looks_legacy 覆盖这些键)
        assert!(tmp.path().join("settings.json.bak-pre-overhaul").exists());
    }

    /// salvage 键缺失语义(Task 1 review 折入本任务):`salvage()` 的 `base` 快照来自
    /// `Settings::default()` 序列化,必然带着新默认 "audio_scheme":"b"——若不特殊处理,
    /// 一份根本没有 audio_scheme 键的坏源文件会被这份默认值抢跑,得到 B 而不是走
    /// `SettingsRepr` 的键缺失分支去看旧 `mix_track`,本该迁移成 Ab 的存量反而丢了
    /// 迁移结果。`{"mix_track":true,"theme":123}`:theme 类型错拖垮整体反序列化 →
    /// 落到 salvage 路径;theme 坏字段回默认,mix_track 好字段应驱动迁移到 Ab。
    #[test]
    fn salvage_missing_audio_scheme_key_still_migrates_legacy_mix_track() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("settings.json"), r#"{"mix_track":true,"theme":123}"#).unwrap();
        let s = load(tmp.path());
        assert_eq!(
            s.audio_scheme,
            AudioScheme::Ab,
            "缺 audio_scheme 键的坏文件抢救后仍须走 mix_track 迁移,不能被 base 默认 B 抢跑"
        );
        assert_eq!(s.theme, "system", "theme 类型错,坏字段回默认");
    }

    /// 启动自愈堵尸检累积回归:PR#66 后 lib.rs setup 的启动调用退化成纯 `load()`——
    /// `load()` 对损坏/旧格式文件只在内存里 salvage/迁移,从不回写磁盘,于是同一份坏
    /// 文件每次启动都会重新触发一次"解析失败→尸检备份"流程,在 app_data 目录里累积
    /// 无穷多具 `settings.json.corrupt-*` 尸体,旧键也永远学不会离开磁盘。这里验证
    /// `needs_heal` 探测 + 一次性 `update(&d, |_| {})` round-trip 能把这条回归堵死:
    /// 愈合后 `needs_heal` 归 false,且再次 `load` 不会新增尸检文件(尸检去重证据)。
    #[test]
    fn needs_heal_true_for_corrupt_file_then_false_after_one_shot_heal_no_new_corpse() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(
            tmp.path().join("settings.json"),
            r#"{"audio_scheme":123,"dashscope_api_key":"sk-live","theme":"dark"}"#,
        )
        .unwrap();
        assert!(needs_heal(tmp.path()), "整体解析失败的坏文件应判定需要自愈");

        // 一次性自愈:load(salvage)→save round-trip,把抢救结果落盘。
        update(tmp.path(), |_| {}).unwrap();
        assert!(!needs_heal(tmp.path()), "自愈落盘后磁盘文件已是干净新格式,不再需要愈合");
        assert_eq!(load(tmp.path()).theme, "dark", "自愈不得丢好字段");
        assert_eq!(
            load(tmp.path()).dashscope_api_key,
            "sk-live",
            "自愈不得丢凭证等好字段"
        );

        // 尸检去重证据:自愈之后再 load 同一份(已干净)文件,不应新增 corrupt-* 尸体。
        let corpse_names = |dir: &Path| -> std::collections::HashSet<std::ffi::OsString> {
            std::fs::read_dir(dir)
                .unwrap()
                .filter_map(|e| e.ok())
                .map(|e| e.file_name())
                .filter(|n| n.to_string_lossy().starts_with("settings.json.corrupt-"))
                .collect()
        };
        let before = corpse_names(tmp.path());
        let _ = load(tmp.path());
        let after = corpse_names(tmp.path());
        assert_eq!(
            before, after,
            "自愈后再次 load 不得新增尸检文件(堵住尸检累积回归):before={before:?} after={after:?}"
        );
    }

    /// 旧键文件(命中 LEGACY_MARKERS,但本身能整体解析成功——比如仅带 `mix_track`)
    /// 同样须判定需要自愈:一次性 `update` 落盘后旧键从磁盘消失,`needs_heal` 归 false。
    #[test]
    fn needs_heal_true_for_legacy_keys_then_heals_to_clean_keys_on_disk() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(
            tmp.path().join("settings.json"),
            r#"{"mix_track":true,"mirror_prefix":"https://x/"}"#,
        )
        .unwrap();
        assert!(needs_heal(tmp.path()), "命中旧键关键字的文件应判定需要自愈");

        update(tmp.path(), |_| {}).unwrap();
        let raw = std::fs::read_to_string(tmp.path().join("settings.json")).unwrap();
        assert!(
            !raw.contains("mix_track") && !raw.contains("mirror_prefix"),
            "自愈落盘后旧键不得残留: {raw}"
        );
        assert!(!needs_heal(tmp.path()), "自愈后应不再需要愈合");
        assert_eq!(
            load(tmp.path()).audio_scheme,
            AudioScheme::Ab,
            "旧 mix_track:true 的迁移结果应已落盘"
        );
    }

    /// 反向断言:全新安装(文件不存在)与已是干净新格式的文件都不应判定需要愈合——
    /// 前者是"没有可愈合的东西",后者是避免无谓重写(每次启动都 save 一遍不是免费的,
    /// 且违背"只在真正需要时才写盘"的最小惊讶)。
    #[test]
    fn needs_heal_false_for_missing_file_and_pristine_new_format_file() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(!needs_heal(tmp.path()), "全新安装(文件不存在)不应判定需要愈合");

        let s = Settings { theme: "dark".into(), ..Default::default() };
        save(tmp.path(), &s).unwrap();
        assert!(!needs_heal(tmp.path()), "干净新格式文件不应触发无谓的自愈重写");
    }

    /// 双尸检回归(2026-08-10 二审 Important):setup 旧写法是先纯 `load(d)` 拿 `s`(坏文件
    /// 在这一步就当场写一具 `settings.json.corrupt-*` 尸体),再 `if needs_heal { update(d,
    /// |_|{}) }`——`update` 内部又对同一份坏文件重新 `load` 一次,再写第二具尸体。一次启动
    /// 堵出两具尸检,而磁盘上其实只坏了一份文件。lib.rs setup 现已改为:先探测 `needs_heal`
    /// (纯 `from_str`,不写盘),为真时才用 `update` 的返回值直接当 `s`(那一次 `update`
    /// 内部的 `load` 就是唯一一次可能写尸检的 load);为假则直接 `load`,全程不落盘。
    /// setup 本身不可单测,这里在 settings.rs 层复刻同样的探测→按需 update→(否则)load 顺序,
    /// 断言全程只堵出一具尸体。
    #[test]
    fn heal_first_flow_probe_then_update_produces_exactly_one_corpse() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(
            tmp.path().join("settings.json"),
            r#"{"audio_scheme":123,"dashscope_api_key":"sk-live","theme":"dark"}"#,
        )
        .unwrap();

        let corpse_names = |dir: &Path| -> std::collections::HashSet<std::ffi::OsString> {
            std::fs::read_dir(dir)
                .unwrap()
                .filter_map(|e| e.ok())
                .map(|e| e.file_name())
                .filter(|n| n.to_string_lossy().starts_with("settings.json.corrupt-"))
                .collect()
        };

        // 复刻 lib.rs setup 改后的顺序:先探测,为真才 update(拿它的返回值当 s),否则纯 load。
        let s = if needs_heal(tmp.path()) {
            update(tmp.path(), |_| {}).unwrap_or_else(|_| load(tmp.path()))
        } else {
            load(tmp.path())
        };

        assert_eq!(s.theme, "dark", "自愈不得丢好字段");
        assert_eq!(s.dashscope_api_key, "sk-live", "自愈不得丢凭证等好字段");
        assert!(!needs_heal(tmp.path()), "自愈落盘后不应再判定需要愈合");

        let corpses = corpse_names(tmp.path());
        assert_eq!(
            corpses.len(),
            1,
            "先探测再按需 update 应只堵出一具尸体(旧的 load-then-heal 顺序会堵出两具): {corpses:?}"
        );

        // 后续启动(文件已干净)再走同样的流程,不应新增尸体。
        let before = corpses;
        let _ = if needs_heal(tmp.path()) {
            update(tmp.path(), |_| {}).unwrap_or_else(|_| load(tmp.path()))
        } else {
            load(tmp.path())
        };
        assert_eq!(before, corpse_names(tmp.path()), "文件已干净的后续启动不应再新增尸体");
    }

}
