# 录音/回放/文件 ASR 双方案可切换设计

> 2026-08-06。目标:让「双轨事后补救」与「录制期混成成品轨」两套方案在同一篇笔记上
> 可切换、可对比,把回放效果的争议从听感judgment 变成可量的数。

## 背景

回放效果长期不达标。现状链路是**双轨落盘 + 回放期事后补救**:

- mic/system 两路各自落 16k 单声道 WAV,靠 `offset_ms` 铺进同一时间轴
- 回放时 `player_align` 估 mic 轨时钟漂移并重采样,`player_gate` 按电平压低 mic 轨消回声残影
- 两者合计 2743 行(`player.rs` 1063 + `player_align.rs` 1311 + `player_gate.rs` 369)

`player_align.rs` 的头注已自陈未达标:

> 错位从 147.7s 压到中位 ~0.16s,但**没有全程压进回放门控的 400ms 回看窗**,
> t≈100~350s 一段稳定在 0.5~0.9s……目前三种量法(估计器自测、外部梅尔谱扫拉伸、
> 短窗直测)在 0.2~0.9s 区间互不吻合,分歧本身已达阈值量级。

**真正的根问题不是"补救得不够好",而是"连残余有多大都测不准"**——没有可信判据,任何
调参都是盲调。本设计的最终价值主张落在这一点上(见 §度量)。

## 调研结论:meetily

调研了 Zackriya-Solutions/meetily v0.4.0(`0281737`,Rust/Tauri 同栈,28.3k stars,MIT)。

它回放没有这类问题,原因是**录制期就混成一条轨,磁盘上只有一个文件**
(`incremental_saver.rs`: "we only store mixed audio"),回放即单文件直放。

但它是靠丢掉问题来源换到的,不是解决:

| 事项 | meetily 状态 |
|---|---|
| 回声消除 | **没有**。全仓 `echo.?cancel\|aec\|acoustic echo` 命中 0;唯一 "webrtc" 是 `WebRtcVad`(活动检测) |
| 时基对齐 | **没有**。靠墙钟到达顺序拼,`can_mix()` 用 `\|\|`+零填充,一路滞后即错位且不可恢复 |
| mic/system 出处 | **混音时丢失**,见其 issue #642(开着) |
| 说话人分离 | README 宣称有、topics 挂 `sortformer`,但 Rust 源码 `diariz\|sortformer` 命中 **0** |
| 蓝牙 | `BLUETOOTH_PLAYBACK_NOTICE.md` 是告用户书("回放别用蓝牙耳机"),无技术处理 |

其 README/CLAUDE.md 与代码存在系统性出入:宣传的 "intelligent ducking" 位于
`FFmpegAudioMixer`,该结构是死代码(仅 `mod.rs` re-export + 自身单测,零生产调用);
真混音器 `ProfessionalAudioMixer` 自注 "Simple audio mixer **without** aggressive
ducking";`window_ms = 600.0` 而注释与日志均写 50ms(差 12 倍)。

其 issue #220(开着)描述的正是本仓已用 soft-aec 一期/二期/P3b + 回放跨轨门控打过的
回声问题,用户的解法是"换带麦的头戴式耳机"。

**结论:不采纳其实现,只采纳「录制期混一条成品轨」这个方向。** 本仓 `frame_tap` 的
设计本身即来自 2026-07-18 对 meetily 的对比调研(见该文件头注),那一轮已把它的
SourceBuffer 思路翻译成逐源补零;这一轮取的是产物形态,不是代码。

## 目标 / 非目标

**目标**

1. 录制期产出一条已混音的成品轨,回放直放,不经 align/gate
2. 已有笔记可**离线补生成**成品轨,历史录音直接成为回归集
3. 同一篇笔记上两套方案可当场切换对比(回放与文件 ASR 各自独立切)
4. 建立可信的错位度量基准,替代当前互不吻合的三种估计法

**非目标**

- 不改设备采集层(`AudioCapture` 及其实现)——两套方案用同一批设备与同一套 AEC
- 不做 meetily 那样的"导入外部音频文件"(本期只处理本仓已录笔记)
- 不改声纹算法本身;成品轨模式只定义降级口径,不调阈值

## 架构

混音器挂在 `frame_tap` 与 AEC **之后**。这是全设计最关键的位置决定——由此白拿采样率
纠正、断流补零、回声消除三样。meetily 的混音器在这三样之前(它无前两样),这是它错位
不可恢复的根源。

```
mic 采集 ─→ frame_tap ─→ AEC Capture ─┬─→ AudioTrackWriter(mic.wav)
            (率纠正      (消回声)      └─→ ┐
             +补零)                        │
                                           ├─→ MixWriter ─→ mixed.wav
sys 采集 ─→ frame_tap ─→ AEC Render  ─┬─→ │
            (率纠正)     (喂参考)      └─→ ┘
                          └─→ AudioTrackWriter(system.wav)
```

### 混音器:按时间轴索引,不按到达顺序

与 meetily 的本质差别。每块样本的时间轴位置是**算出来的**:

```
pos = base_ms × 16 + 该轨已写样本数
```

即 `player.rs` 头注既有约定「文件内毫秒 + offset_ms == 时间轴毫秒」。混音器维护按
位置索引的滑动累加窗,两路各自写进自己的位置:

- 某路暂时无数据 → 该位置只有另一路贡献,**不拿静音顶替**
- 某路滞后到达 → 按位置落到正确槽位,**不与更晚的对面窗错配**
- 水位线 = `min(各源当前位置) − 安全余量`;低于水位的位置定稿刷盘。安全余量初值取
  **400ms**(与 `player_gate` 的 system 回看窗同量级,覆盖已知的 165~245ms 声学回路
  延迟与设备抖动);该值为待实测校准的初值,不是定论

meetily 那类永久错位在此结构下不可能发生:位置从不靠推断。

### 存储:`mixed` 作为第三个轨源

`store/audio.rs` 的 `known_sources()` = 内建 `{mic, system}` ∪ audio.json 记录过的源。
新增 `mixed` 轨**不需要改存储层**:写 `mixed.wav` + audio.json 条目,`list_tracks()`
自动认领,转码 m4a、波形、offset 全部白拿。

### 离线补生成

走**同一个混音核心**,喂料换成从 `mic.wav`/`system.wav` 按各自 `offset_ms` 读出的
样本流。同一份代码两个入口,保证实时产物与补生成产物一致。

注意:离线路径**不继承** `frame_tap` 的实时率纠正(历史轨已按错的率落盘)。故补生成时
须调 `player_align` 估计器把 mic 轨重采样回 system 时基,其已知残余(0.5~0.9s)被**烘进**
产物。这与 §接口二 `MixedDirect → align=off` 不矛盾:对齐发生在**生成期**,回放期读到的
永远是已定稿的单轨,不再做任何估计。
**这是刻意保留的对照**:新录笔记(源头无漂移)与历史笔记(事后估计)的成品轨质量差,
本身就是"漂移是否在源头被消掉"的直接证据。

## 三个接口

### `RecordingSink`(录制期产物)

`session.rs` 现有 `audio_sinks: Vec<(Source, Box<dyn FnMut(&[f32]) + Send>)>` 是其雏形。

```rust
pub trait RecordingSink: Send {
    /// 某源一块 post-AEC、post-frame_tap 样本(16k 单声道)
    fn accept(&mut self, source: Source, samples: &[f32]);
    /// 停录:定稿所有产物
    fn finalize(&mut self) -> anyhow::Result<()>;
}
```

- `DualTrackSink` — 现状,两个 `AudioTrackWriter`
- `MixedSink` — 包住 `DualTrackSink` + 时间轴混音器

### `PlaybackSource`(回放)

`player_load(tracks: Vec<LoadTrack>)` 已按轨列表装载,`LoadTrack{path, offset_ms, source}`。
切换 = 传哪几条轨 + 是否做预处理。

```rust
pub trait PlaybackSource {
    fn plan(&self, note_dir: &Path) -> anyhow::Result<PlaybackPlan>;
}
// DualTrackRender → tracks=[mic, system], gate=build, align=on
// MixedDirect     → tracks=[mixed],       gate=none,  align=off
```

### `TranscribeInput`(文件 ASR)—— 新建能力

**本仓目前无从文件重转写的能力**(`retranscri|重转写|transcribe_file` 命中 0);ASR 只在
录制期经 `segment_worker` 实时跑。本接口需从零建链路:读 WAV → 分段 → 喂 `Recognizer`
→ 写回 segments。

```rust
pub trait TranscribeInput {
    /// 产出待识别段:(样本, 时间轴起点 ms, 归属 Source)
    fn segments(&mut self) -> anyhow::Result<Vec<PendingSegment>>;
}
// DualTrackInput → 双轨分别切段,Source 保真,声纹三闸完整
// MixedInput     → 单轨切段,Source=Mixed,按 §降级口径
```

复用既有 `Recognizer` trait(`asr/mod.rs`),不新造识别抽象。

## 开关

两处,语义不同:

| 位置 | 控制什么 | 生效范围 |
|---|---|---|
| 设置页 · 全局 | 新录制用哪个 `RecordingSink` | 只影响以后录的 |
| 笔记详情页 · 按钮 | 该笔记回放/重转写消费哪份产物 | 当场切,可反复 A/B |

详情页按钮的**可选项由该笔记实际拥有的产物决定**:`list_tracks()` 无 `mixed` 即置灰,
并给出「离线生成成品轨」动作。老笔记零迁移。

回放与文件 ASR **各自独立切**——这是刻意的:验证"混音是否伤转写准确率"时需要固定回放、
只切 ASR。(单一大 trait 管三件事的方案因此被否。)

## 降级口径(成品轨模式)

`Source` 枚举增 `Mixed` 变体,`as_str() = "mixed"`(该处 IPC 字符串本即稳定契约)。

| 能力 | 双轨模式 | 成品轨模式 |
|---|---|---|
| 转写文本 | 正常 | 正常 |
| 段落 `Source` | `mic` / `system` 保真 | 一律 `mixed` |
| 段内说话人切分(`diar/split`) | 正常 | 正常(按变更点切,不依赖信道) |
| 同信道裸阈值命中(0.68) | 正常 | 正常 |
| 跨信道 AS-Norm z 通道 | 正常 | **关闭**(无信道可分,`SEED_ASSIGN_Z` 无意义) |
| 写入种子簇 | 正常 | **不写** |
| 自动归并 | 参与 | **不参与**(仍可人工合并) |

最后两条是硬要求:**成品轨模式只读消费声纹库,绝不回写**。实验做砸也不留需清理的残骸。
meetily 混完丢源、脏数据无退路,正是反面教材(其 issue #642)。

## 错误处理

**硬约束:混音是旁路,录制主链路绝不因它失败。**

| 故障 | 处理 |
|---|---|
| 混音器任一环节异常 | 停写 `mixed.wav`,双轨照常落盘,该笔记退回只有方案 A 可选 |
| `mixed.wav` 存在但损坏 | 复用 `list_tracks` 既有空轨/损坏跳过逻辑,不新造判据 |
| 离线补生成 | 录制中拒绝(与整理六命令同口径);写 tmp 后原子改名,杜绝半成品被当成品消费 |
| 重转写覆盖 segments | 破坏性 → 走 `store/notelock`;新结果先写旁文件,成功后原子切换 |
| 重转写 ⊥ 录制 | 互斥,后端拒绝 + 前端置灰 |

## 度量(本设计的核心价值)

当前三种离线量法在 0.2~0.9s 区间互不吻合,分歧已达阈值量级,短窗法自身还会误配出
±2.5s 假点。**没有可信判据,调参即盲调。**

方案二给出跳出该困局的机会:**录制期我们掌握真值**。在 `frame_tap` 已有的墙钟-样本
对账基础上,把每路对账序列直接落进 `audio.json`:

1. 新录笔记的两轨错位是**读出来的**,不是估出来的
2. 有此基准可反向标定三种离线量法,判定哪个可信
3. 方案 A/B 的对比第一次有客观判据,而非靠听

**验收判据**:新录笔记(方案 B)全程错位读数应恒 < 20ms(单帧量级)。历史笔记补生成的
成品轨不设此要求,其残余用于对照。

## 测试

- **混音器时间轴 = 必须锁死的核心**:纯逻辑单测,喂构造的错位/缺口/乱序样本流,
  断言输出位置逐样本正确。(meetily 那 4 个单测只测了"存得进去""RMS 算得对",
  无一测同步——本仓不重蹈。)
- **端到端无头**:复用 `session.rs` 既有 `MockCapture::from_wav` + IPC 仿真层
- **回归集**:27 场历史录音离线补生成后逐场对照
- **真机冒烟必做**:Chromium 会假通过(PR#65/#66 均栽于此)

## 已知限制

1. 历史笔记的补生成成品轨仍依赖 `player_align` 估计器,继承其 0.5~0.9s 已知残余
2. 成品轨模式下声纹跨信道能力关闭,该模式识别率预期低于双轨模式——这是设计选择,
   不是缺陷,对比时须以双轨模式为基线
3. 磁盘占用增加一条轨(16k 单声道 s16 ≈ 1.9MB/分钟,转码 m4a 后大幅缩小)
4. `frame_tap` 检测窗内的内容已按错的率发出,粗偏差场景约 0.44s 相位补不回——
   方案 B 亦不能消除此项,它先于混音器发生
