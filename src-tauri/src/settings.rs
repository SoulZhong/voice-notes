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
    /// 声音处理方案(spec 2026-08-10,2026-08-10 用户拍板默认翻 B):录制期混音与笔记页
    /// 默认回放的统一档位。a=双轨(不混音);ab=对照(混音,默认回放仍双轨);
    /// b=成品轨(默认,混音,默认回放成品轨)。混音开启后每分钟多约 1.9MB 磁盘
    /// (转码 m4a 后大幅缩小),仅影响新录制。
    /// 内部 `Option` 仅为判"键是否在场":迁移必须看原始键存在性,不能比较值==默认——
    /// 默认翻转后显式写入的 "b" 会被误判为"未设置"(Codex P1#1)。`load()` resolve 后
    /// `audio_scheme` 恒为具体值;`save()` 前从 `audio_scheme` 回写本字段,否则 skip
    /// 序列化会把键从磁盘上写丢。
    // pub(crate) 而非纯私有:同 crate 内(lib.rs 测试用的 `Settings { .., ..Default::default() }`
    // 函数式更新语法)即便不显式点名该字段,也要求它在字面量构造点可见,否则 E0451——
    // 跨模块仍不可见,对外 API 面不变,符合"私有,serde 专用"的本意。
    #[serde(default, skip_serializing_if = "Option::is_none", rename = "audio_scheme")]
    pub(crate) audio_scheme_raw: Option<AudioScheme>,
    /// resolve 后的对外真值。跳过 (反)序列化——落盘/收发都走上面的 `audio_scheme_raw`,
    /// 外部代码(lib.rs 等)照常读这个字段,不感知内部 Option 机制。
    #[serde(skip)]
    pub audio_scheme: AudioScheme,
    /// 旧布尔键「录制期混出成品轨」(≤2026-08-09):仅为 load 迁移而保留读取,
    /// save 不再写出(skip_serializing)。语义等价:true=混音+默认双轨=Ab。
    #[serde(default, rename = "mix_track", skip_serializing)]
    pub legacy_mix_track: Option<bool>,
    /// 采集路径逃生舱(json-only 无 UI,同 asr_provider 先例):aec=普通输入+软件AEC(默认),
    /// vpio=系统通话模式(蓝牙击穿/设备格式不兼容时的手改退路)。
    #[serde(default)]
    pub capture_path: CapturePath,
    /// 音频自动保留期:到期笔记仅清音频轨(转写/精修稿永留)。默认永久。
    #[serde(default)]
    pub audio_retention: AudioRetention,
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
    /// 保留天数;Forever = None(永不清理)。
    /// #[allow(dead_code)]:本任务(Task 1)只加字段/枚举地基,尚无消费者调用此方法做
    /// 实际清理判定——留给后续接线音频保留期清理逻辑的任务,届时移除本 allow。
    #[allow(dead_code)]
    pub fn days(self) -> Option<u32> {
        match self {
            Self::Forever => None,
            Self::D90 => Some(90),
            Self::D30 => Some(30),
        }
    }
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
            audio_scheme_raw: None,
            audio_scheme: AudioScheme::B,
            legacy_mix_track: None,
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

/// audio_scheme 迁移 resolve:原始键在场(`audio_scheme_raw`)则照旧,任意值都算用户
/// 显式选择,不受旧 `mix_track` 影响(Codex P1#1 的翻车组合:`{"audio_scheme":"b",
/// "mix_track":true}` 必须停在 B,不能被旧键拖回 Ab)。键缺失时才看旧 `mix_track`;
/// 都缺 → 新默认 B。
///
/// 也被 `update()` 条件性复用(仅当闭包改动了 `audio_scheme_raw` 时才调用,见
/// `update()` 内的对称判定注释):`set_settings` 这类整体替换 `Settings`
/// (`*s = new_settings`)的闭包会让 skip 序列化的 `audio_scheme` 字段被重置为类型
/// 默认值(因为它不参与 deserialize),必须在 save 前重新从随结构体一起被替换的
/// `audio_scheme_raw` 派生一次,否则 `save()` 的"从 audio_scheme 回写 raw"会用这个
/// 陈旧默认值覆盖前端刚提交的档位。
fn resolve_audio_scheme(s: &mut Settings) {
    s.audio_scheme = match s.audio_scheme_raw {
        Some(v) => v,
        None => match s.legacy_mix_track {
            Some(true) => AudioScheme::Ab,
            _ => AudioScheme::B,
        },
    };
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

/// 缺失/损坏 → 默认值（容忍，不报错）。旧 mix_track 布尔键在此迁移(见字段注释)。
/// 升级备份 + 逐字段抢救,详见 `resolve_audio_scheme` 与 `salvage`。
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
    let mut s: Settings = match raw.as_deref().map(serde_json::from_str::<Settings>) {
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
    };
    resolve_audio_scheme(&mut s);
    // 回写 raw:保证 get_settings 经 IPC 序列化给前端时 audio_scheme 键不会因为
    //(旧文件迁移/全新安装场景下)raw 本就是 None 而被 skip 字段吞掉;也让随后若
    // 直接对这份 Settings 调 save() 时天然带着正确值,无需依赖调用方记得回写。
    s.audio_scheme_raw = Some(s.audio_scheme);
    s
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
    // audio_scheme 是 skip 序列化的对外字段,真正落盘的是 audio_scheme_raw;写盘前必须
    // 从当前真值回写一次,否则(比如测试/调用方直接构造 Settings 而不知道 raw 内部机制时)
    // 键会从磁盘上消失,下次 load 又摔回默认(见 P1#1/P1#2 相关注释)。克隆而非改 &self,
    // 保持 save 对调用方传入值只读的既有契约。
    let mut s2 = s.clone();
    s2.audio_scheme_raw = Some(s2.audio_scheme);
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
    file.write_all(serde_json::to_string_pretty(&s2)?.as_bytes())?;
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
    // load() 之后 audio_scheme_raw 恒为 Some(当前真值)(见 load() 尾部注释)。记下闭包前
    // 的快照,用来对称判定闭包到底动没动 raw——不能无条件 resolve:那样会把 load() 时
    // 缓存的旧 raw 覆盖回闭包刚直接写的 `s.audio_scheme`,静默吞掉调用方的修改。
    let before_raw = s.audio_scheme_raw;
    f(&mut s);
    // 仅当闭包让 audio_scheme_raw 变化了(典型场景:lib.rs::set_settings 的
    // `*s = new_settings` 整体替换,把前端提交的新 raw 带进来,同时把 skip 序列化的
    // 公开字段 audio_scheme 重置回类型默认值,因为它不参与 deserialize)才需要重新
    // resolve,让公开字段跟上刚替换进来的 raw,save() 的"公开字段回写 raw"才不会用
    // 这个陈旧默认值覆盖前端刚提交的档位。若闭包只是直接改公开字段(比如未来某个任务
    // 写 `s.audio_scheme = X` 这种单字段改法,raw 未被触碰),这里必须放过——公开字段
    // 此时就是真值,交给 save() 的"公开→raw"同步落盘,而不是被这里的 resolve 覆盖回旧值。
    if s.audio_scheme_raw != before_raw {
        resolve_audio_scheme(&mut s);
    }
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
        //(new_settings 由前端 JSON 反序列化而来)。这会让 skip 序列化的公开字段
        // audio_scheme 被重置为类型默认值(反序列化不经过它),必须验证 update() 事后
        // 重新 resolve,否则 save() 的"公开字段回写 raw"会用这个陈旧默认值覆盖前端刚
        // 提交的档位——前端选 b,落盘却变成别的值,静默丢用户设置。
        let tmp = tempfile::tempdir().unwrap();
        // 存量:已显式选过 b(新默认,当心测试值不要恰好等于 AudioScheme::default(),
        // 否则"忘了 resolve"这个回归会被类型默认值巧合掩盖,测试失去意义)。
        std::fs::write(tmp.path().join("settings.json"), r#"{"audio_scheme":"b"}"#).unwrap();
        assert_eq!(load(tmp.path()).audio_scheme, AudioScheme::B);

        // 前端把新值经 JSON 传回来(rename 目标键 "audio_scheme"),模拟 tauri command
        // 参数 `new_settings: settings::Settings` 的反序列化产物。刻意选与
        // AudioScheme::default() 不同的档位(a),这样"忘了在 update() 里重新 resolve"
        // 这个回归不会被类型默认值碰巧等于提交值给掩盖掉。
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
        // 对称判定的另一半:闭包不碰 audio_scheme_raw、只直接改公开字段
        // `s.audio_scheme`(不同于 set_settings 的整体替换)时,update() 事后的
        // resolve 不能无条件执行——那样会用 load() 时缓存的旧 raw 把这次直接写入覆盖
        // 回旧值,静默吞掉调用方的修改,给后续任务埋雷(Review Important 1)。
        let tmp = tempfile::tempdir().unwrap();
        // 起始档位与目标档位都不是 AudioScheme::default(),避免巧合掩盖判定错误。
        std::fs::write(tmp.path().join("settings.json"), r#"{"audio_scheme":"b"}"#).unwrap();
        assert_eq!(load(tmp.path()).audio_scheme, AudioScheme::B);

        let got = update(tmp.path(), |s| s.audio_scheme = AudioScheme::A).unwrap();
        assert_eq!(got.audio_scheme, AudioScheme::A, "闭包直接写公开字段的改动不能被吞");
        assert_eq!(load(tmp.path()).audio_scheme, AudioScheme::A, "落盘也须是闭包写入的值");
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

}