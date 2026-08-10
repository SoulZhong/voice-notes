# 配置项大梳理(默认成品轨 + 三删一藏 + 交互统一)

日期:2026-08-10
状态:已批准(用户逐项拍板:默认 B 全量生效仅显式 ab 保留 / 删 keep_audio+record_system_only+keep_output_volume、藏 language_filter / 固定普通输入+软件 AEC / 单页整频+高级折叠区)

## 背景

PR#85 落地了声音处理方案三档(a/ab/b,默认 a)。用户决定:B(成品轨)升为旗舰默认;同时对设置页 5 分区约 29 行配置做一次大梳理——删过时项、统一控件语言、低频项收进折叠高级区。

## 设计

### 1. 声音处理方案默认翻转(a → b)

`AudioScheme` 的 `#[default]` 移到 `B`,迁移守卫从 `== A` 改为 `== B`("等于默认值即未显式设置"机制不变)。迁移表:

| 旧文件状态 | 结果 |
|---|---|
| 无任何相关键(含全新安装) | **b**(新默认) |
| 仅 `mix_track: true`(显式开过对照) | **ab** |
| 仅 `mix_track: false`(旧默认,非显式选择) | **b** |
| 显式 `audio_scheme` 任意值 | 照旧 |

已知边角(沿用 PR#85 已接受的口径):显式 `"b"` + 陈旧 `mix_track:true` 并存(仅手改可造)会迁 ab。测试表全部翻新;`Settings::default().audio_scheme == B` 单独断言(unwrap_or_default 兜底口径)。前端:设置页本地兜底态 `"a"` → `"b"`;`schemeToDefaultPlayback` 未知值容错回 dual 不动;笔记页 `defaultPlayback` 初始 `"dual"` 不动(取数落定前的保守值)。

### 2. 三删一藏

- **删 `keep_audio`**:行为固定为**保留音频**。删字段(serde 忽略旧键,无需迁移代码)、删 lib.rs `keep_audio=false` 分支(录音 sink 恒装配)、删设置行与 i18n、删该字段默认值测试。
- **删 `record_system_only`**:固定**双轨都录**(mic+system)。同上清理源选择分支。
- **删 `keep_output_volume`**:固定**普通输入 + 软件 AEC**(旧 true 路径:无 VPIO ducking,外放音量不压,回声走 WebRTC AEC3 + 文本去重兜底)。**⚠️ 行为变更:旧默认是 VPIO(false),固定后所有用户走 AEC 路径,无开关退路**——用户知情拍板;AEC 个别设备翻车时的退路是手改回退版本或未来复加开关。冒烟重点。
- **藏 `language_filter`**:键与行为不动,UI 行移入「高级」折叠区。

`asr_provider` 维持 json-only 无 UI 现状。三个删除字段在 settings.rs 的默认值/解析测试同步删除,另补一条"含三个已删键的旧 json 仍可解析"测试。

### 3. 设置页重排与控件统一

- **分区**:通用 / 录制 / 语音模型 / **高级(折叠)** / 关于。原「存储」分区(数据目录/模型目录)整体并入「高级」;语言过滤入「高级」;下载镜像留在「语音模型」(下载场景就近)。「高级」默认折叠,disclosure 行(标题 + chevron 旋转,120ms 展开动画,`prefers-reduced-motion` 直切),展开态不持久化(会话内记忆即可,刷新回折叠)。
- **控件统一**:≤3 个互斥选项一律 `Segmented`(md)——主题(系统/亮/暗)、界面语言(跟随系统/中文/English)、识别方式(本地/云端)、声纹模型(campplus/eres2netv2)、云厂商(火山/阿里)。文本输入(凭证/base_url/模型名)、路径选择、快捷键录制维持现状。switch 仅剩真布尔项(日历匹配、自动采纳、快捷键、托盘、语言过滤、镜像开关、MCP 控制等)。
- 录制区删三行后重排;既有"录制中锁定"提示条(lockHint)语义不变。

### 4. 冒烟与风险

- Rust:迁移新表全锁 + 已删键旧 json 解析;vitest:i18n parity(删除行旧键移除、新增高级区键)、设置页兜底态。
- 真机冒烟:三种旧文件升级后的档位显示(无键→B/true→AB/false→B);**录制走普通输入的回声表现**(重点,无退路);外放音量不再被 duck;高级区折叠展开与内含项可用;各 Segmented 双主题;录制中锁定提示仍正确。
- 风险台账:AEC 固定路径是本次最大行为变更;若真机冒烟回声翻车,回滚方案是恢复 keep_output_volume 开关(字段删除对旧文件无损,serde 忽略,复加即回)。

## 范围与非目标

**不动**:AudioScheme 三档语义与笔记页回放逻辑(PR#85 已定)、AI 精修组配置、快捷键/托盘/主题的行为、MCP 设置、`asr_provider`、镜像行为本体。

**分支**:`settings-overhaul`,叠在 `audio-scheme-setting`(PR#85)上,第三层;下层合入后依次改 base。
