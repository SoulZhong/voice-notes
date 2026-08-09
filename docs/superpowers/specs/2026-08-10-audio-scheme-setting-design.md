# 声音处理方案设置(三选一)

日期:2026-08-10
状态:已批准(用户拍板:统一单选,且支持 A、B 同开的对照档)

## 背景

声音处理有两个方案:方案 A(双轨对齐+门控回放)与方案 B(录制期时间轴混音,产成品轨,PR#75/#82)。现状是"半套"控制:

- 设置页只有「录制期混出成品轨(方案 B)」布尔开关(`mix_track`,默认关)——只管产不产成品轨;
- 回放听哪路是笔记页会话级切换,每次切换笔记重置回双轨,无全局记忆。

用户要一个统一设置控制启用哪个方案,且允许 A、B 同开(对照)。

## 设计

### 1. 设置项形态

设置页「录音」区,现有 mixTrack 开关行升级为**三选一**(用 `Segmented` 组件承载,md 尺寸,设置页首个复用点):

| 档位 | serde 值 | 录制期混音 | 默认回放 | 语义 |
|---|---|---|---|---|
| 方案 A · 双轨(默认) | `a` | 关 | 双轨 | 今天的默认行为 |
| A+B 对照 | `ab` | 开 | 双轨 | 今天 mix_track=true 的行为,笔记页随时切成品轨对比 |
| 方案 B · 成品轨 | `b` | 开 | 成品轨 | 无成品轨/不可信自动回落双轨(复用既有回落逻辑) |

### 2. 存储与迁移(settings.rs)

`mix_track: bool` 替换为 `audio_scheme: AudioScheme`(serde `"a" | "ab" | "b"`,缺省 `a`)。

- 旧文件兼容:load 时文件里只有 `mix_track` → 语义等价迁移:`true → ab`(旧行为=混音+默认双轨)、`false → a`;写盘只写新键。
- Rust 测试锁三条:缺省 a / 旧 true→ab / 旧 false→a;另锁 `Settings::default()` 为 a(读失败 unwrap_or_default 兜底口径,与既有 mix_track 测试同款纪律)。

### 3. 消费端(两处)

- **录制管线**(lib.rs 现读 `settings.mix_track` 处):改读 `audio_scheme != A`——ab 与 b 都产成品轨;
- **笔记页默认回放**:`playbackScheme` 初始值与 id 切换复位值(现硬编码 `"dual"`)改按设置:`b → "mixed"`,其余 `"dual"`。挂载时 `getSettings()` 取一次(增值层,取失败按 a 处理不打扰);既有"mixed 不可用强制回落 dual"的 effect 原样兜底。映射提为纯函数 `schemeToDefaultPlayback` 配 vitest。

### 4. UI 与 i18n

settings-row 结构不变:左 label+desc,右 `Segmented`(三段)。i18n 替换 `settings.record.mixTrack.*` 为 `settings.record.audioScheme.*`(label/desc/三个段名,zh+en 齐平过 parity);desc 一句话说清三档差别与 B 档成本(沿用现 desc 的每分钟体积数据)。

## 范围与非目标

**不动**:笔记页 segmented 临时切换(会话级 override 语义不变)、成品轨补生成入口、A/B 口径护栏提示(ab_caveat)、离线回声清洗管线。

**分支**:`audio-scheme-setting`,基于 `note-header-redesign`(设置页 UI 依赖其 Segmented 组件);PR#84 合并后 rebase 到 master 再提 PR。

## 测试

- Rust:设置迁移三例 + default 口径一例;录制管线消费点如有既有测试引用 mix_track 同步更新。
- vitest:`schemeToDefaultPlayback` 三档映射;i18n parity 哨兵自动覆盖新 key。
- 真机冒烟:三档各录一段——a 无 mixed 产物、ab 有产物且默认双轨、b 有产物且默认成品轨;b 档打开无成品轨的旧笔记自动回落双轨。
