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
pos = 该源首帧相对共同时钟原点的 16k 样本偏移 + 该源在本场已产出的样本数
```

> **本节已按一期实现重写。** 原文写的是 `pos = base_ms × 16 + 该轨已写样本数`,即
> `player.rs` 头注既有约定「文件内毫秒 + offset_ms == 时间轴毫秒」。那个口径把两源
> 当成**同时**开始:实际 `start_session` 是顺序起源的,SCK 建流耗时可达数百毫秒到秒级,
> 这段错峰是 §背景 里 0.5~0.9s 顽固残余的一个未建模来源。现在 `frame_tap` 让全部源
> 共享一个单调时钟原点(`OnceLock<Instant>`,由**第一个**真实帧钉住),各源记下自己首帧
> 相对该原点的偏移(`SourceHealth::first_frame_offset_16k`),混音器用它作为该源时间轴
> 的起点——后启动的源在 mixed 里因此带一段前导静音,这正是它真实缺席的那段时间。

混音器维护按位置索引的滑动累加窗,两路各自写进自己的位置:

- 某路暂时无数据 → 该位置只有另一路贡献,**不拿静音顶替**
- 某路滞后到达 → 按位置落到正确槽位,**不与更晚的对面窗错配**
- 水位线 = `min(各源当前位置) − 安全余量`;低于水位的位置定稿刷盘。安全余量初值取
  **400ms**(与 `player_gate` 的 system 回看窗同量级,覆盖已知的 165~245ms 声学回路
  延迟与设备抖动);该值为待实测校准的初值,不是定论

meetily 那类永久错位在此结构下不可能发生:位置从不靠推断。

#### 由此产生的口径差:mixed 的时间轴 ≠ 源轨/段落的时间轴

这不是缺陷,是新口径的必然结果,但**二期消费前必须知道**。记 `offset_sys` 为 system
源的首帧偏移(样本数,mic 通常先起流故 `offset_mic` 多为 0):

| 同一个 `system.wav` 第 k 个样本 | 落在哪个时间轴位置 |
|---|---|
| 在 `mixed.wav` 里 | `base_ms + (offset_sys + k) / 16` |
| 在 `system.wav` 自身里 | `base_ms + k / 16`(「文件内毫秒 + offset_ms == 时间轴毫秒」仍成立) |
| 在段落时间戳里 | `base_ms + k / 16`(`segment_worker::emit_finished` 按**每源流内**样本数换算,不含首帧偏移) |

三条后果:

1. 二期的「点段落 → 在 mixed 里 seek」对 **system 段会系统性偏 `offset_sys`**(mic 段
   偏 `offset_mic`,通常为 0)。要么 seek 时补上该源的首帧偏移,要么把偏移落盘供回放侧读。
2. `mixed.wav` 的 `duration_ms` 会比两条源轨都长出这段前导,与 `TrackMeta.sync` 的
   `track_ms` 直接比较会有同量级的差,交叉核对时须扣掉。
3. 首帧偏移目前**只活在内存里**(`SourceHealth`),停录不落盘。二期若要用它做 seek 修正,
   得先把它写进 `audio.json`。

### 存储:`mixed` 作为第三个轨源(读取端必须隔离)

> **本节已按一期实现重写。** 原文写的是"新增 `mixed` 轨不需要改存储层,`list_tracks()`
> 自动认领,转码/波形/offset 全部白拿"——实现下来**恰恰相反**:读取端隔离是计划外插入
> 的必修任务。照原文去 `list_tracks()` 里找 `mixed` 会找不到。

`store/audio.rs` 的 `known_sources()` = 内建 `{mic, system}` ∪ audio.json 记录过的源,
`mixed` 确实会被它自动认领——**转码 m4a、陈旧头修复、offset 这三项是白拿的**,它们都走
`known_sources()`。

但**枚举必须分叉**:`list_tracks()` 是详情页播放器的轨列表,播放器把返回的每条轨**叠加
播放**。`mixed` 本就是 `mic + system` 混出来的,三条一起播就是三重叠加——音量翻倍、听感
像回声,而成品轨的本意恰恰是消除重影。故:

- `list_tracks()` **显式过滤掉** `MIXED_TRACK`,只报源轨;
- 另起 `mixed_track()` 单独取那一条,复用同一个 `track_info_for` 保证口径不分叉;
- 第二期的回放方案切换走 `mixed_track()`,不经 `list_tracks()`。

**"波形白拿"只对已转码的 `mixed` 成立**:转码路径会在删 WAV 前预计算波形写进 meta,
这条 `mixed` 拿得到;但**未转码的 `mixed.wav`(中断笔记/转码失败降级)拿不到懒回填**
——`note_audio_info` 的后台回填循环遍历的是 `list_tracks()` 的结果,`mixed` 已被过滤在外。
二期若要给成品轨画波形,需自行触发 `backfill_wav_waveform(note_dir, "mixed")`。

另见 `mixed_track()` 的文档注释:它返回 `Some` **不等于**这条轨内容完整(见 §错误处理)。

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
| 混音器任一环节异常(**非续录**) | 停写 `mixed.wav` 并删除已写出的残轨,双轨照常落盘,该笔记退回只有方案 A 可选 |
| 混音器任一环节异常(**续录场景**) | 停写,并把 `mixed.wav` 截回**本场开始前的对齐基线**(`store::audio::pre_session_track_len`),重写头。上一场内容原样保留,本场字节一个不留;该笔记仍被判定为有方案 B 产物,但那条轨的内容是上一场的**真前缀**,不是混合体 |
| 混音器异常但**本场一个样本都没写出去** | 完全不碰盘上文件。`AudioTrackWriter` 惰性建档,没 append 过就没 `open()` 过,文件本就还是装配前的样子;此时再 `set_len` + 重写头是纯破坏面(路径上若是个同名的非 WAV 文件,前 44 字节会被写坏) |
| `mixed.wav` 存在但损坏 | 复用 `list_tracks` 既有空轨/损坏跳过逻辑,不新造判据 |
| 离线补生成 | 录制中拒绝(与整理六命令同口径);写 tmp 后原子改名,杜绝半成品被当成品消费 |
| 重转写覆盖 segments | 破坏性 → 走 `store/notelock`;新结果先写旁文件,成功后原子切换 |
| 重转写 ⊥ 录制 | 互斥,后端拒绝 + 前端置灰 |

**为什么回滚基线不能取"装配时的文件长度"**(踩过,别改回去):`base_ms` 来自
`StoreWriter::base_ms()`,是**续录前最大 `end_ms`**——最后一句话结束的位置,不是墙钟
时长。用户按停止键前必然有一段没进任何 segment 的尾部静音(VAD 尾巴 + 手动停止的反应
时间),所以 `base_ms < 上一场 mixed.wav 实际时长` 是**常态**,`AudioTrackWriter::open()`
的续录对齐恒走**截短**分支,截掉的字节不可逆。此时若按装配时的长度 `set_len` 回滚,文件
会被拉回**比对齐后更长**,空出的那截正好装着本场刚混出来的内容,拼成一条「上一场前段 +
本场开头」的混合体;而它的 `duration_ms` 与放弃前一模一样,下游任何交叉核对都发现不了。
对齐若是补零方向,那段零同样会冒充上一场的内容。故基线取
`min(装配时已有内容, base_ms − offset_ms 对应字节)`,保证文件恒为**真前缀**。

**残留轨仍没有盘上标记**:回滚本身也可能失败(权限/文件被占用),或混音线程 panic 时
`AudioTrackWriter::Drop` 照样补完合法头而回滚逻辑根本够不着。这两条路径的唯一线索都只是
一行 eprintln,进程重启后连这行都没有。一期无人消费这条轨,故刻意不加标记;**二期消费前
必须自行校验,或先补上盘上标记**(`set_track_sync` 是可照抄的模板)。详见 `mixed_track()`
的文档注释。

## 度量(本设计的核心价值)

当前三种离线量法在 0.2~0.9s 区间互不吻合,分歧已达阈值量级,短窗法自身还会误配出
±2.5s 假点。**没有可信判据,调参即盲调。**

方案二给出跳出该困局的机会:**录制期我们掌握真值**。停录时把每轨的墙钟-轨时间轴
对账落进 `audio.json`(`TrackMeta.sync`,见 `store/audio.rs` 的 `SyncInfo`):

1. 新录笔记的两轨错位是**读出来的**,不是估出来的
2. 有此基准可反向标定三种离线量法,判定哪个可信
3. 方案 A/B 的对比第一次有客观判据,而非靠听

### 口径:轨时长量 WAV,不量采集侧计数器

`track_ms` 取自 WAV 实际字节数(`bytes_to_ms(wav_len − 44) − (base_ms − offset_ms)`),
不是 `frame_tap` 的 `SourceHealth.samples`。后者有两处口径错误,曾把这条基准算偏 3~6 倍:

- **不是 16k 口径**:它累加的是设备原生率、交错多声道的原始样本(48k×1 / 44.1k×2ch /
  48k×2ch,四条采集路径没有一条是 16k),重采样发生在它下游。
- **不是净时长**:它在暂停闸之前累加,暂停期照涨;而 `wall_ms` 是扣暂停的净时长。

WAV 两条都天然满足:它写在重采样之后、暂停闸之后。`samples` 仍保留在 `SyncInfo` 里,
但只作排障用(看量级/是否为零),不可换算毫秒。

### 新口径的固有残余:首帧偏移量的是"到达 tap",不是"被声源发出"

共享时钟原点(见 §架构)把"两路 capture 启动错峰"这个百毫秒到秒级的误差建模掉了,是净
改善;但它换来一项**新的、更小的**固有残余,记在这里,本期不修:

`first_frame_offset` 量的是**原始帧到达 tap 线程**的时刻,里面含该源自己的端到端采集
延迟——设备缓冲 + 驱动 + 采集通道调度。这项在两路上并不相等:mic(VPIO / cpal)是十几
毫秒量级,system(macOS SCK / Windows WASAPI loopback)是数十至一两百毫秒量级。差值会
成为 mixed 里两路之间一个**符号未知的常数偏移**,量级几十毫秒。

它比改前那项(整个 capture 启动错峰,百毫秒到秒级)小一个量级,但已经进不了"单帧量级"。
标定办法是 click 测试:同一声脉冲同时进两路,量它在 mixed 里的两个峰的间距,即为该组
设备上的常数偏移。标定出来之后要不要在 `first_frame_offset` 上做常数补偿,留到二期定。

### 验收判据:两轨 drift **之差**,不是各自的绝对值

**判据**:新录笔记(方案 B)`|drift_ms(mic) − drift_ms(system)|` 恒 < 20ms(单帧量级)。
历史笔记补生成的成品轨不设此要求,其残余用于对照。

不用绝对值,是因为 `drift_ms` 含一段**已知且不修**的系统性正偏置:各路 capture 在
`start_session` **内部**就已起流产帧,而墙钟起点 `started` 取于 `start_session` 返回
**之后**,这段启动窗被算进 `track_ms` 却没算进 `wall_ms`。消除它要动录制主链路的启动
时序,风险大于收益。另有两处更小的同向偏置:mic 路软件 AEC 按 10ms 整帧输出,尾部余量
滞留在 AEC 内部不落盘;轨长换算按字节整除的亚毫秒截断。

启动窗与暂停对两轨的影响大体同向,**相减可抵消大部分**。且回放对齐关心的本来就是两轨的
**相对**关系,不是各自对墙钟的绝对偏差——绝对值大只说明"录制起点没对齐",两轨之差大才
说明"回放会听出重影"。此口径与 `SyncInfo` 的文档注释一致,两处必须同步改。

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
5. **A/B 对照条件不公平:A 侧比 B 侧多一级回声抑制**(见下,比之前必读)

### 对照条件:mic 轨的离线回声清洗只给 A 侧

`store/transcode.rs` 的 `clean_mic_before_encode` 在停录转码**之前**会给 `mic.wav` 再过
一道**离线**回声清洗(`audio::echo_clean::clean_wav`),清洗产物**原地替换** `mic.wav`。
触发条件三个全满足才跑:

- 平台是 macOS(`transcode_note_dir` 非 macOS 直接返回);
- `mic` 轨在 audio.json 里带 `soft_aec: true` 标记(即走的是软件 AEC 路径;VPIO 场次和
  旧笔记没这个标记,不清洗);
- 清洗引擎实际检出显著回声,过 `CONFIDENCE_GATE` / `PEAK_GATE` 门限(检不出返回
  `Ok(None)`,mic.wav 字节不动)。

而 `mixed.wav` 是**录制期**用实时 AEC3 的输出混出来的,停录时那道离线清洗**永远轮不到
它**——`clean_mic_before_encode` 只认 `mic.wav` 这一个文件名。

**后果**:凡是触发了清洗的场次,方案 A 侧(mic + system 双轨)比方案 B 侧(mixed 单轨)
**多一级回声抑制**。而这恰恰发生在「保持外放音量」开着、回声最严重、最值得做 A/B 的那
批场次上。此时 B 听感更差**可能纯粹是少了一道后处理**,与"录制期混音"这个待验变量毫无
关系。**照这个口径比出来的第一轮 A/B 结论会是错的。**

比较时必须控制这个变量,三选一:

1. **优先比未触发清洗的场次**(最省事,推荐):判据在 audio.json —— `mic` 轨有
   `clean` 记录(`CleanInfo`)即说明清洗跑了且改写了文件,该场次不适合直接对比;
   没有 `clean` 记录的场次(非 macOS、无 `soft_aec` 标记、或未过门限)两侧条件对等。
2. **比之前先把清洗关掉**重录一批,让 A 侧退回"只有实时 AEC"的状态,与 B 侧对齐。
3. **二期把同一道清洗补给 `mixed`**(结构上可行:`clean_wav` 需要 mic/system 两条参考
   轨,而这两条在转码前都还在盘上;但 mixed 已是混合信号,清洗引擎的延迟估计是否仍
   成立需要先验证),从根上消除这项不对称。

在做到上述任一条之前,任何 A/B 听感结论都只能标注为"含未控变量"。
