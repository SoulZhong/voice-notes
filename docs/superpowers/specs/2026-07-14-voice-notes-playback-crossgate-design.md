# 回放跨轨门控（crossgate）：消除混音重影 — 设计

日期：2026-07-14
状态：已确认（P3a；与软件回声消除一/二期正交，独立分支独立合入）

## 背景与根因

回放是双轨混音（`player.rs`：mic + system 按 offset 逐采样相加）。软件 AEC 场次
清洗后 mic 轨仍有对方声音残余（真实笔记实测：清洗把互相关 peak 从 >0.30 压到
0.128@170ms），单听 mic 轨已明显干净，但混音时与 system 轨全电平同内容叠加——
人耳对「同源迟到复本」极敏感，-25dB/170ms 的残影在混音里呈现为重影/梳状感。
残余低于清洗门限，却高于混音可闻门限：这是回放混音架构问题，不是 AEC 强度问题。

业界对照（2026-07-14 深度调研，103 代理逐条对抗验证）：多轨消重影的成熟做法是
Auphonic 式 crossgate——检测说话人在哪条轨活跃，回放/缩混时衰减其他轨的相关
信号，而非直接混音。我们有现成的 VAD 转写分段（segments.jsonl 带
source/start_ms/end_ms），比信号包络检测更准。

## 用户决策（约束）

- **温和压低 -15dB、零配置**：非活跃段 mic 增益 ≈0.178；VAD 漏标的真人声不会
  完全丢失。无 UI 开关、无设置项，全部笔记默认生效。
- 双讲（mic 段与 system 段时间重叠）→ mic 全量：本人声音永远优先。
- 门控单向：只压 mic。system 轨是数字采集（ScreenCaptureKit），无声学串音，
  无需对称处理。

## 架构

```
player load(note_id)
  → 读 segments.jsonl(复用既有解析:空白段过滤+start_ms 排序)
  → player_gate::build_gate(&segments) -> Vec<GateSpan>   ← 纯函数
  → spans 挂在 mic 轨的 Track 上(system 轨恒空表)
mix_frames(逐采样)
  → mic 采样 × player_gate::gain_at(spans, cursor_sample)
     (回调块起点二分定位区间索引,块内线性推进;seek 天然按 cursor 正确)
```

### 门控核心 `player_gate.rs`（新纯模块）

- `pub struct GateSpan { pub start: u64, pub end: u64 }` — 16k 源域采样数，
  表示「压低区间」。
- `pub fn build_gate(segments: &[(String, u64, u64)]) -> Vec<GateSpan>`
  输入 (source, start_ms, end_ms) 列表：
  1. 收集 system 段区间并集；
  2. 减去 mic 段区间并集（重叠即双讲，剔除）；
  3. 相邻压低区间间隙 <300ms 合并（防颤振）；
  4. 短于 200ms 的孤立压低区间丢弃（不值得为其动增益）。
- `pub fn gain_at(spans: &[GateSpan], sample: u64) -> f32`
  区间外 1.0；区间内 `DUCK_GAIN = 0.178`（-15dB）；区间边沿 80ms（1280 采样）
  线性渐变（防咔嗒）。渐变落在区间内侧（区间外增益严格 1.0）。
- 常量集中定义并注释依据：`DUCK_GAIN`/`RAMP_SAMPLES`/`MERGE_GAP_MS`/`MIN_SPAN_MS`。

### 混音接入（`player.rs` 最小侵入）

- `Track` 增 `gate: Vec<GateSpan>`（system/未匹配轨为空 Vec——空表增益恒 1.0，
  行为与现状逐采样一致）。
- `mix_frames` 内 per-track 采样值乘 `gain_at`；实现允许在回调块起点二分一次、
  块内单调推进区间索引（性能),语义等价于逐采样独立查询。
- load 侧：读 segments 失败/缺失/空 → 空表降级，等同现状；eprintln 一行。

## 错误处理

增值层哲学：门控构建任何失败（segments 缺失/损坏/解析错）→ 空表 + eprintln，
回放行为等同现状。门控永不 panic、永不阻塞播放。

## 测试与验收

纯函数单测（player_gate）：
- 仅 system 活跃区间 → 产出压低区间；
- mic/system 重叠（双讲）→ 该区间不压；
- 间隙 <300ms 合并、孤立 <200ms 丢弃；
- gain_at 边沿渐变形状（区间边界处 1.0→0.178 线性 80ms）、区间外恒 1.0。

混音集成测试（player.rs 既有 Mem 轨模式）：
- 双轨常值波形 + 已知门控区间 → 断言混音输出幅度分段符合(全量/压低/渐变)。

真机验收：笔记 20260714-170547（实测残余 0.128@170ms）回放对比，重影感应
消失；双讲段本人声音无衰减。

回归：既有全部测试保持绿（player 既有单测覆盖混音核心,门控空表路径必须逐采样
等价）。

## 非目标

- 不做信号分析式 crossgate（分段数据更准且现成）；
- 不烘焙进解码缓存（缓存保持原始字节，排障可对照）；
- 不加 UI/设置项；不动录制与清洗链路。

## 分支

`playback-crossgate` 基于 master，独立 PR，不依赖 #41/#43。
