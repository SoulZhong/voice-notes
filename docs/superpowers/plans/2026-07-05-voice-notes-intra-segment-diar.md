# 段内说话人分离(B)实施计划

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 混说段(两人接话 < VAD 静音阈值被切进一段)按声纹变更点切成子段,各自贴说话人标签——"多人合一"结构性缺陷根治。

**Architecture:** 三层:①`Transcript` 透传 sherpa 的 tokens/timestamps;②`diar/split.rs` 纯逻辑(变更点检测 + token 分组,全常量集中、可单测);③worker 在语言过滤后对 ≥3s 段做滑窗嵌入 → 检测变更点 → 切 SubFinal 列表 → 每个子段等价独立 final 走完全既有处理链(ECHO/assign/on_final)。无变更点时单元素列表,原路径零变化。

**Tech Stack:** Rust。无新依赖、无前端改动、无 schema 改动。

**Spec:** `docs/superpowers/specs/2026-07-05-voice-notes-intra-segment-diar-design.md`(常量/边界/安全失败模式以 spec 为准)

## Global Constraints

- 分支 `intra-segment-diar`(已建,基于 master),单 PR squash 合入。
- 注释中文讲"为什么";cargo test 全过、npm check 0/0(前端零改动预期)、无新警告。
- **不丢内容不变式**:切分路径任何失败(嵌入全挂/时间戳缺失且重识别失败)都必须回退为整段单 final,绝不丢文本。
- 常量集中 `diar/split.rs` 顶部,注释标注"待真实会议数据校准":`SPLIT_MIN_SEGMENT_MS=3000`、`SPLIT_WIN_MS=1500`、`SPLIT_HOP_MS=500`、`CHANGE_SIM_THRESHOLD=0.55`、`MIN_SUBSEG_MS=1200`。
- partial 路径、语言过滤(整段判一次)、ECHO 占位防误杀行为均不变。
- TDD:每任务新逻辑先测后码。

---

### Task 1: Transcript 透传 tokens/timestamps

**Files:**
- Modify: `src-tauri/src/asr/mod.rs`(Transcript 增两字段)
- Modify: `src-tauri/src/asr/sense_voice.rs`(透传 result.tokens / result.timestamps)

**Interfaces:**
- Produces: `Transcript { text, lang, tokens: Vec<String>, timestamps: Vec<f32> }`(derive Default 已有,新字段默认空;mock/whisper 构造点用 `..Default::default()` 的零改动)。timestamps 单位秒、相对段首,与 tokens 等长(异常时可能为空——消费方自行防御)。

- [ ] **Step 1: 写失败测试**(sense_voice.rs 无法单测网络外模型;以编译驱动 + asr/mod.rs 加一个形状测试)

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transcript_default_has_empty_token_fields() {
        let t = Transcript { text: "x".into(), ..Default::default() };
        assert!(t.tokens.is_empty() && t.timestamps.is_empty());
    }
}
```

- [ ] **Step 2: 编译确认失败** — `cd src-tauri && cargo test transcript_default` → E0063(缺字段)。

- [ ] **Step 3: 实现**

asr/mod.rs Transcript 增:

```rust
    /// token 级时间戳(秒,相对段首,与 tokens 等长;模型异常时可能为空)。
    /// 供段内说话人分离按变更点切分文本——识别只跑一次,不重复 ASR。
    pub tokens: Vec<String>,
    pub timestamps: Vec<f32>,
```

sense_voice.rs recognize:

```rust
        Ok(Transcript {
            text: result.text,
            lang: result.lang,
            tokens: result.tokens,
            timestamps: result.timestamps,
        })
```

(whisper.rs 与全部测试 mock 已用 `..Default::default()`,零改动——grep 核实无遗漏构造点。)

- [ ] **Step 4: 跑测试** — `cargo test transcript_default && cargo build 2>&1 | tail -2` 全过无新警告。
- [ ] **Step 5: Commit** — `feat(asr): Transcript 透传 token 级时间戳,供段内切分`

---

### Task 2: diar/split.rs 纯逻辑(变更点检测 + token 分组)

**Files:**
- Create: `src-tauri/src/diar/split.rs`
- Modify: `src-tauri/src/diar/mod.rs`(挂模块)

**Interfaces:**
- Produces:

```rust
pub const SPLIT_MIN_SEGMENT_MS: u64 = 3000;
pub const SPLIT_WIN_MS: u64 = 1500;
pub const SPLIT_HOP_MS: u64 = 500;
pub const CHANGE_SIM_THRESHOLD: f32 = 0.55;
pub const MIN_SUBSEG_MS: u64 = 1200;

/// 相邻滑窗嵌入余弦跌破阈值处 → 变更点(ms,相对段首,升序)。
/// embs[i] 是第 i 窗(起点 i*hop_ms)的单位化嵌入;None = 该窗嵌入失败,
/// 视为与两侧相似(宁可漏切不误切)。变更点取"低谷 run 中最低的相邻对"的
/// 交界中点;切出的任一子段 < MIN_SUBSEG_MS 则该点丢弃。
pub fn detect_change_points(embs: &[Option<Vec<f32>>], total_ms: u64) -> Vec<u64>;

/// 按变更点把 tokens 分组拼接为子文本(变更点 n 个 → n+1 段;token 时刻(秒)
/// 换算 ms 后 < 边界归前段)。返回 None 表示时间戳不可用(空/与 tokens 不等长),
/// 调用方走"子段重识别"回退。子文本 trim 后可为空,由调用方丢弃该子段。
pub fn group_tokens_by_boundaries(
    tokens: &[String],
    timestamps: &[f32],
    boundaries_ms: &[u64],
) -> Option<Vec<String>>;
```

- 变更点时刻定义:相邻窗对 (i, i+1) 的交界中点 = `i*hop + (win + hop)/2` ms(两窗中心的中点)。
- 低谷 run 归并:连续多个低于阈值的相邻对(同一次说话人切换在重叠窗上会连续触发)只取相似度最低的那一对产一个点。
- 嵌入内部单位化(normalize;调用方传原始嵌入即可),normalize 失败(零向量)等同 None。

- [ ] **Step 1: 写失败测试**(同文件 tests;用三维正交基构造)

覆盖(每行为一测,断言精确值):
1. 全同嵌入 → 无变更点;
2. 前 4 窗 e1 后 4 窗 e2(正交)→ 恰 1 个变更点,时刻 = 交界对的中点公式值;
3. 连续低谷(e1→混合→e2 渐变)→ 仍只 1 个点(取最低对);
4. 变更点导致尾子段 < MIN_SUBSEG_MS → 该点被丢弃(构造 total_ms 恰好卡界);
5. 中间窗 None → 不在该处产点(e1, None, e1 → 无点;e1, None, e2 → 相邻有效对是 0-2?**设计澄清**:None 窗跳过,相似度只在相邻**有效**窗间计算,交界中点用两有效窗的实际位置);
6. 空序列/单窗 → 无点;
7. group_tokens:两段分组拼接正确;时间戳空 → None;长度不符 → None;边界后无 token → 尾段空串(调用方丢)。

- [ ] **Step 2: 编译确认失败。**
- [ ] **Step 3: 实现**(纯函数,无 unsafe、无外部依赖;dot/normalize 可复制 registry.rs 的私有实现——两处各自私有,不为省 6 行制造跨模块耦合,注释说明)。
- [ ] **Step 4: `cargo test diar::split` 全过。**
- [ ] **Step 5: Commit** — `feat(diar): 段内变更点检测与 token 分组纯逻辑(split.rs)`

---

### Task 3: worker 接线(滑窗嵌入 → 切 SubFinal → 子段走既有链)

**Files:**
- Modify: `src-tauri/src/session.rs`

**Interfaces:**
- Consumes: T1 Transcript 全字段;T2 split.rs 全部导出;既有 `SpeakerEmbedder::embed`、`is_foreign_final`、`rms_of`、ECHO 状态机、`process_final`。
- Produces(session.rs 内部):

```rust
/// 一个母段切出的子段:等价一个独立 final。
struct SubFinal { text: String, samples: Vec<f32>, start_ms: u64, end_ms: u64 }

/// 母段 → 子段列表(len ≥ 1)。无 embedder / 段太短 / 无变更点 / 任何失败 → 单元素原段。
/// 时间戳缺失时子段重识别回退(打日志);子文本 trim 空的子段丢弃;全部被丢时回退单元素原段。
fn split_final(
    job_samples: Vec<f32>, job_start_ms: u64, job_end_ms: u64,
    transcript: &crate::asr::Transcript,
    recognizer: &mut Box<dyn Recognizer>,
    embedder: &mut Option<Box<dyn SpeakerEmbedder>>,
) -> Vec<SubFinal>;
```

**实现要点(逐条落):**

1. `split_final`:
   - `embedder` 为 None 或 `job_end_ms - job_start_ms < SPLIT_MIN_SEGMENT_MS` → 直接单元素返回。
   - 滑窗:窗起点 `i*SPLIT_HOP_MS`,窗长 SPLIT_WIN_MS,末窗不足窗长则止;每窗 `catch_unwind(embed)`,失败记 None(与既有 embed 防护同款)。
   - `detect_change_points` 无点 → 单元素返回。
   - 文本:`group_tokens_by_boundaries` Some → 按组;None → 逐子段 `recognize(子段样本)`(catch_unwind,失败该子段文本 "[识别失败]"),`eprintln!("段内切分: 时间戳缺失,子段重识别回退")`。
   - 子段样本切片:`ms * 16` 换算样本 idx;start/end = 母段 start_ms + 边界偏移。
   - trim 空文本子段丢弃;若全被丢 → 回退单元素原段(不丢内容不变式)。
   - 切分发生时 `eprintln!("段内切分: {:?} 段 {}ms 切为 {} 子段", source...)`(source 由调用处打,或函数不打日志由调用处打——实现自定,保留一条可观测日志)。
2. worker 接线(语言过滤 `continue` 之后、`let seg_rms` 处起重构):
   - recognize 的解构改为保留完整 `t: Transcript`(占位分支构造 `Transcript { text: "[识别失败]".into(), ..Default::default() }`),`text`/`lang` 取自 t;
   - 占位段("[识别失败]")**不切分**(没时间戳也没意义),沿既有专用路径;
   - `let subs = split_final(job.samples, job.start_ms, job.end_ms, &t, &mut recognizer, &mut embedder);`(注意 job.samples 被 move,后续统一用 sub.samples);
   - `for sub in subs { match job.source { System => {...}, Mic => {...} } }`——既有两个 arm 的体内所有 `job.samples/job.start_ms/job.end_ms/text` 替换为 `sub.samples/sub.start_ms/sub.end_ms/sub.text`,`seg_rms` 改为每子段 `rms_of(&sub.samples)`;ECHO 的 PendingMic/RecentSystem 逐子段构建,语义不变;
   - `recent_system` 窗口裁剪的 `newest_end` 用子段 end_ms(循环内自然成立)。
3. 借用注意:`split_final` 需要 `&mut recognizer`,与 worker 顶部 recognize 调用不同时活跃(顺序执行),无借用冲突;embedder 在 split 与 process_final 间顺序使用,同理。

- [ ] **Step 1: 写失败集成测试**(session.rs tests;沿既有 mock 模式)

```rust
    /// 双说话人混说段被切成两个 final,各自说话人;单说话人段不乱切。
    #[test]
    fn worker_splits_mixed_segment_into_two_finals() {
        // ContentEmbedder: 按窗样本均值返回 e1(<0.5) / e2(≥0.5)——前半 0.1、后半 0.9
        // 的 8s 段,滑窗序列前半 e1 后半 e2,应检出 1 个变更点。
        struct ContentEmbedder;
        impl SpeakerEmbedder for ContentEmbedder {
            fn embed(&mut self, s: &[f32]) -> anyhow::Result<Vec<f32>> {
                let mean = s.iter().sum::<f32>() / s.len() as f32;
                Ok(if mean < 0.5 { vec![1.0, 0.0, 0.0] } else { vec![0.0, 1.0, 0.0] })
            }
        }
        // TimedRecognizer: 8 个 token,时间戳均匀分布 0..8s,文本 t0..t7
        struct TimedRecognizer;
        impl Recognizer for TimedRecognizer {
            fn recognize(&mut self, _s: &[f32]) -> anyhow::Result<Transcript> {
                Ok(Transcript {
                    text: "t0t1t2t3t4t5t6t7".into(),
                    tokens: (0..8).map(|i| format!("t{i}")).collect(),
                    timestamps: (0..8).map(|i| i as f32).collect(),
                    ..Default::default()
                })
            }
        }
        let mut samples = vec![0.1f32; 4 * 16000];
        samples.extend(vec![0.9f32; 4 * 16000]);
        let (tx, rx) = crossbeam_channel::unbounded::<FinalJob>();
        tx.send(FinalJob { source: Source::System, samples, start_ms: 0, end_ms: 8000 }).unwrap();
        drop(tx);
        let mut finals: Vec<(String, u64, u64, Option<String>)> = Vec::new();
        run_asr_worker(
            Box::new(TimedRecognizer),
            Some(Box::new(ContentEmbedder)),
            SpeakerRegistry::new(),
            rx,
            Duration::from_millis(0),
            Vec::new(),
            |_src, text, s, e, spk, _rms| finals.push((text, s, e, spk)),
            |_, _| {},
            |_| {},
        );
        assert_eq!(finals.len(), 2, "混说段应切成两个 final: {finals:?}");
        assert!(finals[0].3 != finals[1].3, "两子段说话人应不同");
        assert_eq!(finals[0].1, 0);
        assert_eq!(finals[1].2, 8000, "时间轴首尾衔接母段");
        assert!(finals[0].2 == finals[1].1, "子段边界无缝");
        assert_eq!(format!("{}{}", finals[0].0, finals[1].0), "t0t1t2t3t4t5t6t7", "文本无损");
    }

    /// 单说话人长段:嵌入恒同 → 不切,单 final(现状不回归)。
    #[test]
    fn worker_keeps_uniform_segment_whole() { /* 同上骨架,样本全 0.1,断言 finals.len()==1 */ }
```

(mock 的确切构造/断言以既有 worker 测试模式为准,brief 允许按实际调整;变更点具体时刻依赖公式,时间断言用"边界无缝+首尾衔接"而非硬编码中点值。)

- [ ] **Step 2: 编译/断言确认失败**(split_final 不存在)。
- [ ] **Step 3: 实现**(按上文实现要点)。
- [ ] **Step 4: 全量验证** — `cargo test 2>&1 | grep "test result" | head -1 && cargo build 2>&1 | tail -2`;既有 worker/ECHO/writer 测试零回归(它们的段普遍 <3s 或 mock embedder 恒同,不触发切分——若有测试因新行为失败,先判语义:确属应切分的构造则更新断言并报告)。
- [ ] **Step 5: Commit** — `feat(session): 段内说话人分离——滑窗变更点切子段,各自贴标签走既有链`

---

### Task 4: 全量验证 + 记账(控制器执行)

- [ ] cargo test 全过 + npm check 0/0 + 双端 build;progress.md 加节;push + PR(冒烟:圆桌派素材两人合一段被切开/单人长段不乱切/暂停续录导出照常)。
