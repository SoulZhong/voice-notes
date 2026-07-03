# P4 说话人区分 + AEC Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 段定稿即带全局说话人标签(两路声纹同一在线聚类,1..N 可改名),麦克风采集换 VoiceProcessingIO 消除外放回声。

**Architecture:** 新模块 `diar/`(`SpeakerEmbedder` trait 包 sherpa-onnx 声纹模型 + `SpeakerRegistry` 纯逻辑在线聚类);ASR worker 定稿时提嵌入→聚类→`on_final` 带 speaker,`on_diar` 回调驱动 speakers.json 与合并重写;前端 chips 改名 + 徽章按说话人着色。AEC 先 spike 验证 coreaudio VPIO,再以 `VpioMicrophone`(实现既有 `AudioCapture` trait)替换 mic 源,失败运行时回退 cpal。

**Tech Stack:** 复用 sherpa-rs 0.6.8(`speaker_id::EmbeddingExtractor` 已内建,无需升级);新增 `coreaudio-rs`(仅 macOS,Task 8);模型 3D-Speaker CAM++ zh-cn(~28MB)。

**Spec:** `docs/superpowers/specs/2026-07-03-voice-notes-p4-diarization-aec-design.md`

## Global Constraints

- 嵌入输入 16kHz 单声道 f32(与管线一致);`EmbeddingExtractor::compute_speaker_embedding(samples: Vec<f32>, sample_rate: u32) -> eyre::Result<Vec<f32>>`(已核对 0.6.8 源码,ctor 用 `ExtractorConfig { model, provider: None, num_threads: Some(1), debug: false }`)。
- **聚类阈值初值**:`ASSIGN_THRESHOLD = 0.55`、`MERGE_THRESHOLD = 0.68`、`MIN_NEW_CLUSTER_SAMPLES = 16000`(1 秒)、`MERGE_CHECK_INTERVAL = 8`(次 assign)——常量集中定义并注释「fixture 校准初值」,Task 9 冒烟后可调。
- 嵌入向量入 Registry 前**归一化为单位向量**,余弦相似度 = 点积;质心为成员单位向量均值再归一化。
- **说话人 id 格式 `"S{n}"`**(S1 起);默认显示名「说话人 {n}」由**前端/导出兜底生成**,speakers.json 只存改过的名字与 sources——未改名者 name 为空串。
- **Registry 不管名字**(只管簇/id/sources/合并);名字归 NoteWriter 的 speakers.json 与前端。
- 降级链:embedder 模型缺失/加载失败 → 会话无 speaker(全 null)→ UI 回「我/对方」徽章 + 横幅;**录制永不因 diarization 失败中断**。
- 常驻复用:`embedder_cache` 与 `recognizer_cache` 并列同策略(setup 预载持锁、开录 take、六个归还点对称)。
- schema_version 不变(speaker 字段 P3 已预留);speakers.json 原子写(临时文件+rename)。
- 事件名新增 `"speakers"`;`FinalEvent` 增加 `speaker: Option<String>`。
- 测试命令:`cargo test --manifest-path src-tauri/Cargo.toml`;前端 `npm run check` 0 errors + `npm run build`。
- 每 Task 一个 commit,message 末尾:`Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>`。

---

## 文件结构(P4 结束时)

```
src-tauri/
  Cargo.toml                    # 改:+coreaudio-rs(macOS, Task 8)
  src/
    diar/
      mod.rs                    # 新:SpeakerEmbedder trait + SherpaEmbedder + MockEmbedder
      registry.rs               # 新:SpeakerRegistry 在线聚类(纯逻辑)
    session.rs                  # 改:worker 带 embedder/registry;on_final+speaker;on_diar;stop 返还二元组
    ipc.rs                      # 改:FinalEvent.speaker;SpeakersEvent
    store/
      writer.rs                 # 改:speakers.json 维护 + rewrite_speaker(合并重写 jsonl)
      notes.rs                  # 改:load 返回 speakers 表
      export.rs                 # 改:label 优先说话人名
      mod.rs                    # 改:SpeakerMeta 类型 + Note.speakers
    audio/
      vpio.rs                   # 新(Task 8):VpioMicrophone(macOS)
    lib.rs                      # 改:embedder_cache;rename_speaker command;merge/speakers 闭包;mic 源换 VPIO
  tests/
    embedder_it.rs              # 新:真模型嵌入 #[ignore] 集成测试
scripts/fetch_models.sh         # 改:+CAM++ 声纹模型
src/
  lib/
    events.ts                   # 改:FinalEvent.speaker;onSpeakers
    notes.ts                    # 改:Note.speakers;renameSpeaker;speakerLabel/speakerColor 工具
    recording.svelte.ts         # 改:speakers 状态;finals 带 speaker
    SpeakerChips.svelte         # 新:说话人 chips 条(改名/这是我)
  routes/
    record/+page.svelte         # 改:chips + 徽章按说话人
    notes/[id]/+page.svelte     # 改:chips + 徽章按说话人
.superpowers/sdd/p4-vpio-spike.md  # Task 1 产出:spike 报告
```

---

### Task 1: VPIO spike(最大不确定性先打掉)

**Files:**
- Create: `.superpowers/sdd/p4-vpio-spike.md`(报告)
- Create: `src-tauri/examples/vpio_probe.rs`(探针,可保留)
- Modify: `src-tauri/Cargo.toml`(macOS target 加 `coreaudio-rs = "0.12"`;若 API 不合适可换 `coreaudio-sys` 直调,报告中说明)

**Interfaces:** Produces spike 结论,决定 Task 8 的实现路线(coreaudio-rs / coreaudio-sys / Swift 垫片)。

- [ ] **Step 1: 写探针**

`src-tauri/examples/vpio_probe.rs`:目标——创建 `kAudioUnitSubType_VoiceProcessingIO` AudioUnit,启用 input scope,设置 16kHz(或拿原生率)单声道 f32 流格式,注册 input callback,采集 5 秒写 `vpio_probe.wav`(用已有 `hound` 依赖),打印实际采样率/声道/帧大小。coreaudio-rs 的 `AudioUnit::new(IOType::VoiceProcessingIO)` 若存在直接用;否则用 `AudioUnitSubType` 常量构造。参考 cpal 的 CoreAudio 后端与 coreaudio-rs 的 `feedback.rs` 示例改造。**探针允许粗糙**——这是 spike,不是产品代码。

- [ ] **Step 2: 运行验证**

Run: `cargo run --manifest-path src-tauri/Cargo.toml --example vpio_probe`
预期:拿到非静音帧,wav 可播放。**人工对照实验**(需真人,可与控制器协作):外放播放一段讲话,同时对麦克风说话 → 分别用探针(VPIO)与现有 cpal 路径录 5 秒,对比外放声音的残留强度;把两个 wav 各跑一次 SenseVoice(可用现有 `#[ignore]` 测试基建手动喂),确认 VPIO 处理后的音频转写质量无明显劣化。

- [ ] **Step 3: 写报告并提交**

`.superpowers/sdd/p4-vpio-spike.md`:可行性结论、实际输出格式(采样率/声道)、AEC 效果观察、ASR 质量观察、Task 8 推荐路线与关键代码要点、遇到的坑。若 coreaudio-rs 走不通,记录原因并给出 coreaudio-sys 或 Swift 垫片的判断。

```bash
git add .superpowers/sdd/p4-vpio-spike.md src-tauri/examples/vpio_probe.rs src-tauri/Cargo.toml src-tauri/Cargo.lock
git commit -m "P4 Task 1: VPIO spike——AEC 可行性验证

Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>"
```

> 若 spike 结论为「不可行且垫片代价过大」:停下,报告控制器,AEC 部分回到设计层重议(文本去重兜底);Task 2-7(diarization)不受影响继续。

---

### Task 2: SpeakerEmbedder trait + 模型下载 + 常驻槽

**Files:**
- Create: `src-tauri/src/diar/mod.rs`
- Create: `src-tauri/tests/embedder_it.rs`
- Modify: `scripts/fetch_models.sh`、`src-tauri/src/lib.rs`(挂 `pub mod diar;` + embedder_cache + 预载)

**Interfaces:**
- Produces:
  - `diar::SpeakerEmbedder: Send`,`fn embed(&mut self, samples: &[f32]) -> anyhow::Result<Vec<f32>>`
  - `diar::SherpaEmbedder::new(model_path: &Path) -> anyhow::Result<SherpaEmbedder>`
  - `diar::MockEmbedder`(测试:按脚本队列返回向量)
  - `AppState.embedder_cache: Arc<Mutex<Option<Box<dyn diar::SpeakerEmbedder>>>>` + `speaker_model_path()` helper + setup 预载(与 recognizer 同线程顺序加载)

- [ ] **Step 1: fetch_models.sh 追加声纹模型**

```bash
# 3D-Speaker CAM++ 中文声纹模型(说话人区分用)
SPK_MODEL="3dspeaker_speech_campplus_sv_zh-cn_16k-common.onnx"
if [ ! -f "$SPK_MODEL" ]; then
  SPK_URL="https://github.com/k2-fsa/sherpa-onnx/releases/download/speaker-recongition-models/$SPK_MODEL"
  echo "下载声纹模型 $SPK_URL ..."
  curl -fL -o "$SPK_MODEL" "$SPK_URL"
  echo "声纹模型已就绪：$DIR/$SPK_MODEL"
fi
```

(注意 release tag `speaker-recongition-models` 是上游原始拼写。若 404,打开 https://github.com/k2-fsa/sherpa-onnx/releases 搜该 tag 核对文件名并修正,报告说明。)执行 `bash scripts/fetch_models.sh` 确认模型落地。

- [ ] **Step 2: 写 diar/mod.rs(trait + 实现 + mock)**

```rust
pub mod registry;

use std::path::Path;

/// 声纹嵌入提取器:段音频(16kHz 单声道 f32)→ 嵌入向量。
/// 真实现包 sherpa-onnx speaker embedding 模型;测试用 MockEmbedder。
pub trait SpeakerEmbedder: Send {
    fn embed(&mut self, samples: &[f32]) -> anyhow::Result<Vec<f32>>;
}

/// sherpa-onnx CAM++ 声纹模型。
pub struct SherpaEmbedder {
    inner: sherpa_rs::speaker_id::EmbeddingExtractor,
}

impl SherpaEmbedder {
    pub fn new(model_path: &Path) -> anyhow::Result<Self> {
        let config = sherpa_rs::speaker_id::ExtractorConfig {
            model: model_path.to_string_lossy().into_owned(),
            num_threads: Some(1),
            ..Default::default()
        };
        let inner = sherpa_rs::speaker_id::EmbeddingExtractor::new(config)
            .map_err(|e| anyhow::anyhow!("加载声纹模型失败: {e}"))?;
        Ok(Self { inner })
    }
}

impl SpeakerEmbedder for SherpaEmbedder {
    fn embed(&mut self, samples: &[f32]) -> anyhow::Result<Vec<f32>> {
        self.inner
            .compute_speaker_embedding(samples.to_vec(), 16000)
            .map_err(|e| anyhow::anyhow!("提取声纹失败: {e}"))
    }
}

/// 测试用:按预置脚本依次返回向量,耗尽后返回最后一个;可注入失败。
pub struct MockEmbedder {
    script: std::collections::VecDeque<anyhow::Result<Vec<f32>>>,
    last: Option<Vec<f32>>,
}

impl MockEmbedder {
    pub fn new(script: Vec<anyhow::Result<Vec<f32>>>) -> Self {
        Self { script: script.into(), last: None }
    }
}

impl SpeakerEmbedder for MockEmbedder {
    fn embed(&mut self, _samples: &[f32]) -> anyhow::Result<Vec<f32>> {
        match self.script.pop_front() {
            Some(Ok(v)) => {
                self.last = Some(v.clone());
                Ok(v)
            }
            Some(Err(e)) => Err(e),
            None => self
                .last
                .clone()
                .ok_or_else(|| anyhow::anyhow!("MockEmbedder 脚本已耗尽")),
        }
    }
}
```

- [ ] **Step 3: 真模型集成测试(#[ignore])**

`src-tauri/tests/embedder_it.rs`:

```rust
//! 需真实声纹模型:VN_MODELS=1 cargo test --test embedder_it -- --ignored
use app_lib::diar::{SherpaEmbedder, SpeakerEmbedder};

#[test]
#[ignore]
fn embeds_fixture_to_fixed_dim_unit_scale_vector() {
    let model = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/models/3dspeaker_speech_campplus_sv_zh-cn_16k-common.onnx"
    );
    let mut e = SherpaEmbedder::new(std::path::Path::new(model)).expect("load");
    let wav = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/sample_16k.wav");
    let mut reader = hound::WavReader::open(wav).expect("fixture");
    let samples: Vec<f32> = reader
        .samples::<i16>()
        .map(|s| s.unwrap() as f32 / i16::MAX as f32)
        .collect();
    let v1 = e.embed(&samples).expect("embed");
    assert!(!v1.is_empty(), "维度非零");
    let v2 = e.embed(&samples).expect("embed again");
    assert_eq!(v1.len(), v2.len(), "维度稳定");
    // 同段音频两次嵌入应几乎一致(余弦 ≈ 1)
    let dot: f32 = v1.iter().zip(&v2).map(|(a, b)| a * b).sum();
    let n1: f32 = v1.iter().map(|x| x * x).sum::<f32>().sqrt();
    let n2: f32 = v2.iter().map(|x| x * x).sum::<f32>().sqrt();
    assert!(dot / (n1 * n2) > 0.99, "同段自相似应≈1");
}
```

- [ ] **Step 4: lib.rs 挂模块 + 常驻槽 + 预载**

`lib.rs`:`mod store;` 后加 `pub mod diar;`(tests/ 需要);`AppState` 加:

```rust
    /// 常驻声纹嵌入器,策略与 recognizer_cache 完全一致(叶子锁、预载持锁)。
    embedder_cache: Arc<Mutex<Option<Box<dyn diar::SpeakerEmbedder>>>>,
```

helper(`sense_voice_dir()` 旁):

```rust
fn speaker_model_path() -> PathBuf {
    models_dir().join("3dspeaker_speech_campplus_sv_zh-cn_16k-common.onnx")
}
```

setup 预载线程里,recognizer 装载之后追加:

```rust
            let mut eslot = embedder_cache.lock().unwrap();
            if eslot.is_none() {
                match diar::SherpaEmbedder::new(&speaker_model_path()) {
                    Ok(e) => *eslot = Some(Box::new(e) as Box<dyn diar::SpeakerEmbedder>),
                    Err(e) => eprintln!("声纹模型预载失败（说话人区分将不可用）: {e}"),
                }
            }
```

(setup 闭包需一并 clone `embedder_cache`。本任务只建槽与预载;取用/归还在 Task 6。)

- [ ] **Step 5: 验证与提交**

Run: `cargo test --manifest-path src-tauri/Cargo.toml`(全量,新增文件编译)
Run: `VN_MODELS=1 cargo test --manifest-path src-tauri/Cargo.toml --test embedder_it -- --ignored --nocapture`
Expected: 真模型测试 PASS(模型已在 Step 1 下载)。

```bash
git add scripts/fetch_models.sh src-tauri/src/diar/ src-tauri/tests/embedder_it.rs src-tauri/src/lib.rs
git commit -m "P4 Task 2: SpeakerEmbedder(sherpa CAM++)+ 模型下载 + 常驻槽预载

Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>"
```

(注:diar/mod.rs 声明了 `pub mod registry;`,本步先建空文件 `registry.rs` 占位可编译——只放 `//! 在线聚类,Task 3 实现`。)

---

### Task 3: SpeakerRegistry 在线聚类(纯逻辑)

**Files:**
- Modify: `src-tauri/src/diar/registry.rs`(替换占位)

**Interfaces:**
- Produces:
  - `SpeakerInfo { id: String, sources: BTreeSet<String> }`
  - `SpeakerRegistry::new() -> Self`
  - `assign(&mut self, embedding: &[f32], source: &str, num_samples: usize) -> Option<String>`
  - `take_merges(&mut self) -> Vec<(String, String)>`(返回 `(被并 id, 并入 id)`,按 assign 周期内部检测)
  - `speakers(&self) -> Vec<SpeakerInfo>`
  - 常量 `ASSIGN_THRESHOLD/MERGE_THRESHOLD/MIN_NEW_CLUSTER_SAMPLES/MERGE_CHECK_INTERVAL`

- [ ] **Step 1: 写测试(合成向量,确定性)**

`registry.rs` 末尾:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    /// 三维正交基方便构造:e1/e2 相似度 0,混合向量可控。
    fn v(x: f32, y: f32, z: f32) -> Vec<f32> {
        vec![x, y, z]
    }
    const LONG: usize = 32000; // 2s,足以建簇

    #[test]
    fn first_assign_creates_s1() {
        let mut r = SpeakerRegistry::new();
        assert_eq!(r.assign(&v(1.0, 0.0, 0.0), "mic", LONG), Some("S1".into()));
        let sp = r.speakers();
        assert_eq!(sp.len(), 1);
        assert_eq!(sp[0].id, "S1");
        assert!(sp[0].sources.contains("mic"));
    }

    #[test]
    fn similar_joins_dissimilar_creates_new() {
        let mut r = SpeakerRegistry::new();
        r.assign(&v(1.0, 0.0, 0.0), "mic", LONG);
        // 与 e1 余弦 ≈ 0.995,归入 S1
        assert_eq!(r.assign(&v(1.0, 0.1, 0.0), "system", LONG), Some("S1".into()));
        // 正交,新建 S2
        assert_eq!(r.assign(&v(0.0, 1.0, 0.0), "system", LONG), Some("S2".into()));
        // S1 记录了两个来源
        let sp = r.speakers();
        let s1 = sp.iter().find(|s| s.id == "S1").unwrap();
        assert!(s1.sources.contains("mic") && s1.sources.contains("system"));
    }

    #[test]
    fn centroid_tracks_running_mean() {
        let mut r = SpeakerRegistry::new();
        r.assign(&v(1.0, 0.0, 0.0), "mic", LONG);
        // 多次喂入偏向 e1+e2 混合的向量后,质心偏移,原纯 e2 向量也能归入
        for _ in 0..8 {
            r.assign(&v(1.0, 0.8, 0.0), "mic", LONG);
        }
        assert_eq!(
            r.assign(&v(0.55, 0.75, 0.0), "mic", LONG),
            Some("S1".into()),
            "质心应随成员漂移"
        );
    }

    #[test]
    fn short_segment_never_creates_cluster_but_can_join() {
        let mut r = SpeakerRegistry::new();
        // 短段 + 无既有簇 → None
        assert_eq!(r.assign(&v(1.0, 0.0, 0.0), "mic", 8000), None);
        // 建立 S1 后,短段相似 → 归入
        r.assign(&v(1.0, 0.0, 0.0), "mic", LONG);
        assert_eq!(r.assign(&v(1.0, 0.05, 0.0), "mic", 8000), Some("S1".into()));
        // 短段不相似 → None(不建新簇)
        assert_eq!(r.assign(&v(0.0, 1.0, 0.0), "mic", 8000), None);
        assert_eq!(r.speakers().len(), 1);
    }

    #[test]
    fn drifting_clusters_get_merged_small_into_large() {
        let mut r = SpeakerRegistry::new();
        // 两簇初始正交
        for _ in 0..6 {
            r.assign(&v(1.0, 0.0, 0.0), "mic", LONG); // S1(大簇)
        }
        r.assign(&v(0.0, 1.0, 0.0), "system", LONG); // S2(小簇)
        assert!(r.take_merges().is_empty(), "正交簇不该合并");
        // 把 S2 的质心喂到与 S1 高度相似
        for _ in 0..12 {
            r.assign(&v(0.9, 0.435, 0.0), "system", LONG); // 与 e1 余弦≈0.9 → 落 S1? 不——
            // 注:该向量与 S1 质心(≈e1)余弦 ≈ 0.9 > ASSIGN_THRESHOLD,会直接归入 S1,
            // 这正是在线聚类的常态;为构造"两簇漂移到相似"的场景,直接喂与 S2 相似、
            // 同时逐渐偏向 e1 的序列:
        }
        let mut r = SpeakerRegistry::new();
        for _ in 0..6 {
            r.assign(&v(1.0, 0.0, 0.0), "mic", LONG); // S1 大簇
        }
        r.assign(&v(0.30, 0.954, 0.0), "system", LONG); // 与 e1 余弦 0.30 → 新建 S2
        // S2 的后续成员逐渐偏向 e1(与 S2 现质心保持 >0.55 归入 S2,但拉动质心靠近 e1)
        for k in 1..=10 {
            let t = 0.30 + 0.05 * k as f32; // 0.35..0.80
            let y = (1.0 - t * t).max(0.0).sqrt();
            r.assign(&v(t, y, 0.0), "system", LONG);
        }
        // 触发周期性合并检查(take_merges 内部在每 MERGE_CHECK_INTERVAL 次 assign 后检测)
        let merges = r.take_merges();
        assert_eq!(merges.len(), 1, "漂移后两簇应合并");
        let (loser, winner) = &merges[0];
        assert_eq!(winner, "S1", "小簇并入大簇");
        assert_eq!(loser, "S2");
        assert_eq!(r.speakers().len(), 1);
        // 合并后 sources 汇总
        assert!(r.speakers()[0].sources.contains("system"));
    }

    #[test]
    fn zero_or_mismatched_dim_embedding_returns_none() {
        let mut r = SpeakerRegistry::new();
        assert_eq!(r.assign(&[], "mic", LONG), None);
        r.assign(&v(1.0, 0.0, 0.0), "mic", LONG);
        assert_eq!(r.assign(&[1.0, 0.0], "mic", LONG), None, "维度不符丢弃");
        assert_eq!(r.assign(&[0.0, 0.0, 0.0], "mic", LONG), None, "零向量丢弃");
    }
}
```

- [ ] **Step 2: 运行确认编译失败**

Run: `cargo test --manifest-path src-tauri/Cargo.toml diar::registry`
Expected: FAIL — `SpeakerRegistry` 未定义。

- [ ] **Step 3: 实现**

`registry.rs` 测试之前:

```rust
//! 在线增量声纹聚类:两路(mic/system)嵌入汇入同一 Registry,
//! 得全局「S1..Sn」。纯逻辑、无模型依赖、单线程持有于 ASR worker。

use std::collections::BTreeSet;

/// 归簇阈值(余弦)。fixture 校准初值,冒烟后可调。
pub const ASSIGN_THRESHOLD: f32 = 0.55;
/// 簇间合并阈值(余弦,高于归簇阈值防过度合并)。fixture 校准初值。
pub const MERGE_THRESHOLD: f32 = 0.68;
/// 低于此样本数(16kHz)的段不允许新建簇(短段声纹不可靠)。
pub const MIN_NEW_CLUSTER_SAMPLES: usize = 16000;
/// 每 N 次 assign 做一次簇间合并检查。
pub const MERGE_CHECK_INTERVAL: u64 = 8;

#[derive(Debug, Clone, PartialEq)]
pub struct SpeakerInfo {
    pub id: String,
    pub sources: BTreeSet<String>,
}

struct Cluster {
    id: String,
    /// 成员单位向量的均值,再归一化。
    centroid: Vec<f32>,
    count: u64,
    sources: BTreeSet<String>,
}

pub struct SpeakerRegistry {
    clusters: Vec<Cluster>,
    next_id: u32,
    assigns: u64,
    pending_merges: Vec<(String, String)>,
}

fn normalize(v: &[f32]) -> Option<Vec<f32>> {
    let norm: f32 = v.iter().map(|x| x * x).sum::<f32>().sqrt();
    if !norm.is_finite() || norm < 1e-6 {
        return None;
    }
    Some(v.iter().map(|x| x / norm).collect())
}

fn dot(a: &[f32], b: &[f32]) -> f32 {
    a.iter().zip(b).map(|(x, y)| x * y).sum()
}

impl Default for SpeakerRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl SpeakerRegistry {
    pub fn new() -> Self {
        Self { clusters: Vec::new(), next_id: 1, assigns: 0, pending_merges: Vec::new() }
    }

    /// 归簇:与各质心比余弦,≥ 阈值归入最相似簇并更新质心;
    /// 否则段够长才新建簇。返回说话人 id;不可用嵌入/短段无归属返回 None。
    pub fn assign(&mut self, embedding: &[f32], source: &str, num_samples: usize) -> Option<String> {
        let unit = normalize(embedding)?;
        if let Some(c) = self.clusters.first() {
            if c.centroid.len() != unit.len() {
                return None; // 维度不符(模型换了?)丢弃
            }
        }
        self.assigns += 1;
        if self.assigns % MERGE_CHECK_INTERVAL == 0 {
            self.detect_merges();
        }

        let best = self
            .clusters
            .iter_mut()
            .map(|c| (dot(&c.centroid, &unit), c))
            .max_by(|(a, _), (b, _)| a.total_cmp(b));

        if let Some((sim, cluster)) = best {
            if sim >= ASSIGN_THRESHOLD {
                // 质心 running mean(在单位向量上),再归一化
                let n = cluster.count as f32;
                for (ci, ui) in cluster.centroid.iter_mut().zip(&unit) {
                    *ci = (*ci * n + ui) / (n + 1.0);
                }
                if let Some(renorm) = normalize(&cluster.centroid) {
                    cluster.centroid = renorm;
                }
                cluster.count += 1;
                cluster.sources.insert(source.to_string());
                return Some(cluster.id.clone());
            }
        }

        if num_samples < MIN_NEW_CLUSTER_SAMPLES {
            return None; // 短段不建簇
        }
        let id = format!("S{}", self.next_id);
        self.next_id += 1;
        self.clusters.push(Cluster {
            id: id.clone(),
            centroid: unit,
            count: 1,
            sources: BTreeSet::from([source.to_string()]),
        });
        Some(id)
    }

    /// 取走自上次调用以来检测到的合并对 (被并 id, 并入 id)。
    pub fn take_merges(&mut self) -> Vec<(String, String)> {
        self.detect_merges();
        std::mem::take(&mut self.pending_merges)
    }

    fn detect_merges(&mut self) {
        loop {
            let mut found: Option<(usize, usize)> = None;
            'outer: for i in 0..self.clusters.len() {
                for j in (i + 1)..self.clusters.len() {
                    if dot(&self.clusters[i].centroid, &self.clusters[j].centroid) >= MERGE_THRESHOLD {
                        found = Some((i, j));
                        break 'outer;
                    }
                }
            }
            let Some((i, j)) = found else { break };
            // 小簇并入大簇(计数大者胜;平局取 i)
            let (win, lose) = if self.clusters[j].count > self.clusters[i].count { (j, i) } else { (i, j) };
            let loser = self.clusters.remove(lose);
            let win = if lose < win { win - 1 } else { win };
            let winner = &mut self.clusters[win];
            let (wn, ln) = (winner.count as f32, loser.count as f32);
            for (wc, lc) in winner.centroid.iter_mut().zip(&loser.centroid) {
                *wc = (*wc * wn + *lc * ln) / (wn + ln);
            }
            if let Some(renorm) = normalize(&winner.centroid) {
                winner.centroid = renorm;
            }
            winner.count += loser.count;
            winner.sources.extend(loser.sources.iter().cloned());
            self.pending_merges.push((loser.id.clone(), winner.id.clone()));
        }
    }

    pub fn speakers(&self) -> Vec<SpeakerInfo> {
        self.clusters
            .iter()
            .map(|c| SpeakerInfo { id: c.id.clone(), sources: c.sources.clone() })
            .collect()
    }
}
```

- [ ] **Step 4: 运行测试确认通过**

Run: `cargo test --manifest-path src-tauri/Cargo.toml diar::registry`
Expected: PASS(6 个)。若 `centroid_tracks_running_mean` / 合并测试因阈值几何不成立而失败:**调整测试向量而非阈值常量**,保持常量为设计值,并在报告注明。

- [ ] **Step 5: Commit**

```bash
git add src-tauri/src/diar/registry.rs
git commit -m "P4 Task 3: SpeakerRegistry 在线增量聚类(归簇/质心/短段守卫/合并)

Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>"
```

---

### Task 4: ASR worker 接入声纹(session.rs)

**Files:**
- Modify: `src-tauri/src/session.rs`
- Modify: `src-tauri/src/store/writer.rs`(仅既有集成测试适配新签名)

**Interfaces:**
- Consumes: `SpeakerEmbedder`/`MockEmbedder`(Task 2)、`SpeakerRegistry`(Task 3)。
- Produces:
  - `DiarEvent`(枚举):`SpeakersChanged(Vec<SpeakerInfo>)` | `Merged { loser: String, winner: String }`
  - `run_asr_worker(recognizer, embedder: Option<Box<dyn SpeakerEmbedder>>, finals_rx, partial_slots, on_final: impl FnMut(Source, String, u64, u64, Option<String>), on_partial, on_diar: impl FnMut(DiarEvent)) -> (Box<dyn Recognizer>, Option<Box<dyn SpeakerEmbedder>>)`
  - `start_session(..., embedder: Option<Box<dyn SpeakerEmbedder>>, ..., on_final(5 参), on_partial, on_diar) -> Result<SessionStart, StartError>`
  - `RecordingHandle::stop(self) -> (Option<Box<dyn Recognizer>>, Option<Box<dyn SpeakerEmbedder>>)`

- [ ] **Step 1: 写失败测试**

`session.rs` `asr_worker_tests` 追加(需 `use crate::diar::{MockEmbedder, SpeakerEmbedder};` 与 `use crate::session::DiarEvent;` 按模块实际路径调整):

```rust
    #[test]
    fn finals_get_speaker_labels_and_diar_events() {
        let (tx, rx) = crossbeam_channel::unbounded::<FinalJob>();
        // 两段长音频:第一段 → S1;第二段正交向量 → S2
        tx.send(FinalJob { source: Source::Mic, samples: vec![0.1; 32000], start_ms: 0, end_ms: 2000 }).unwrap();
        tx.send(FinalJob { source: Source::System, samples: vec![0.1; 32000], start_ms: 2000, end_ms: 4000 }).unwrap();
        drop(tx);

        let embedder = MockEmbedder::new(vec![
            Ok(vec![1.0, 0.0, 0.0]),
            Ok(vec![0.0, 1.0, 0.0]),
        ]);
        let finals = Arc::new(Mutex::new(Vec::<Option<String>>::new()));
        let diar_events = Arc::new(Mutex::new(0usize));
        let (f2, d2) = (finals.clone(), diar_events.clone());
        let (_r, e) = run_asr_worker(
            Box::new(CountingRecognizer),
            Some(Box::new(embedder)),
            rx,
            vec![],
            move |_, _, _, _, spk| f2.lock().unwrap().push(spk),
            |_, _| {},
            move |_ev| *d2.lock().unwrap() += 1,
        );
        assert!(e.is_some(), "embedder 应返还");
        assert_eq!(
            *finals.lock().unwrap(),
            vec![Some("S1".into()), Some("S2".into())]
        );
        assert!(*diar_events.lock().unwrap() >= 2, "每个新说话人应发 SpeakersChanged");
    }

    #[test]
    fn embed_failure_degrades_to_null_speaker() {
        let (tx, rx) = crossbeam_channel::unbounded::<FinalJob>();
        tx.send(FinalJob { source: Source::Mic, samples: vec![0.1; 32000], start_ms: 0, end_ms: 2000 }).unwrap();
        drop(tx);
        let embedder = MockEmbedder::new(vec![Err(anyhow::anyhow!("boom"))]);
        let finals = Arc::new(Mutex::new(Vec::<Option<String>>::new()));
        let f2 = finals.clone();
        let _ = run_asr_worker(
            Box::new(CountingRecognizer),
            Some(Box::new(embedder)),
            rx,
            vec![],
            move |_, _, _, _, spk| f2.lock().unwrap().push(spk),
            |_, _| {},
            |_| {},
        );
        assert_eq!(*finals.lock().unwrap(), vec![None], "嵌入失败段 speaker 为 null,不影响文本");
    }

    #[test]
    fn no_embedder_all_speakers_null() {
        let (tx, rx) = crossbeam_channel::unbounded::<FinalJob>();
        tx.send(FinalJob { source: Source::Mic, samples: vec![0.1; 32000], start_ms: 0, end_ms: 2000 }).unwrap();
        drop(tx);
        let finals = Arc::new(Mutex::new(Vec::<Option<String>>::new()));
        let f2 = finals.clone();
        let (_r, e) = run_asr_worker(
            Box::new(CountingRecognizer),
            None,
            rx,
            vec![],
            move |_, _, _, _, spk| f2.lock().unwrap().push(spk),
            |_, _| {},
            |_| {},
        );
        assert!(e.is_none());
        assert_eq!(*finals.lock().unwrap(), vec![None]);
    }
```

- [ ] **Step 2: 运行确认编译失败**

Run: `cargo test --manifest-path src-tauri/Cargo.toml session`
Expected: FAIL(签名不符)。

- [ ] **Step 3: 实现**

`session.rs` 顶部:

```rust
use crate::diar::registry::SpeakerRegistry;
use crate::diar::SpeakerEmbedder;
```

新枚举(FinalJob 定义旁):

```rust
/// diarization 侧事件:说话人表变化 / 簇合并(需回写落盘与 UI)。
#[derive(Debug, Clone)]
pub enum DiarEvent {
    SpeakersChanged(Vec<crate::diar::registry::SpeakerInfo>),
    Merged { loser: String, winner: String },
}
```

`run_asr_worker` 重写(保持既有 finals 优先/partial 空闲语义,嵌入只对 final 做):

```rust
pub fn run_asr_worker(
    mut recognizer: Box<dyn Recognizer>,
    mut embedder: Option<Box<dyn SpeakerEmbedder>>,
    finals_rx: Receiver<FinalJob>,
    partial_slots: Vec<(Source, Arc<Mutex<Option<PartialJob>>>)>,
    mut on_final: impl FnMut(Source, String, u64, u64, Option<String>),
    mut on_partial: impl FnMut(Source, String),
    mut on_diar: impl FnMut(DiarEvent),
) -> (Box<dyn Recognizer>, Option<Box<dyn SpeakerEmbedder>>) {
    let mut registry = SpeakerRegistry::new();
    let mut known_speakers = 0usize;
    loop {
        match finals_rx.recv_timeout(Duration::from_millis(100)) {
            Ok(job) => {
                let text = /* 原有 recognize + catch_unwind 逻辑不变 */;
                // 声纹:嵌入失败/无 embedder → None,绝不影响文本
                let speaker = embedder.as_mut().and_then(|e| {
                    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        e.embed(&job.samples)
                    })) {
                        Ok(Ok(v)) => registry.assign(&v, job.source.as_str(), job.samples.len()),
                        Ok(Err(err)) => {
                            eprintln!("声纹提取失败({:?} 段): {err}", job.source);
                            None
                        }
                        Err(_) => {
                            eprintln!("声纹提取 panic({:?} 段),该段无标签", job.source);
                            None
                        }
                    }
                });
                for (loser, winner) in registry.take_merges() {
                    on_diar(DiarEvent::Merged { loser, winner });
                }
                let speakers = registry.speakers();
                if speakers.len() != known_speakers {
                    known_speakers = speakers.len();
                    on_diar(DiarEvent::SpeakersChanged(speakers));
                }
                on_final(job.source, text, job.start_ms, job.end_ms, speaker);
            }
            Err(crossbeam_channel::RecvTimeoutError::Timeout) => {
                /* 原有 partial 服务逻辑不变 */
            }
            Err(crossbeam_channel::RecvTimeoutError::Disconnected) => break,
        }
    }
    (recognizer, embedder)
}
```

(实现时把注释处的原逻辑原样保留;`Merged` 后 `SpeakersChanged` 也会因数量变化触发,顺序:先 Merged 后 SpeakersChanged。)

`start_session`:参数插入 `embedder: Option<Box<dyn SpeakerEmbedder>>`(recognizer 之后)与 `on_diar: impl FnMut(DiarEvent) + Send + 'static`(末尾);asr 线程闭包传入;`RecordingHandle.asr` 类型改 `JoinHandle<(Box<dyn Recognizer>, Option<Box<dyn SpeakerEmbedder>>)>`;`stop`:

```rust
    pub fn stop(mut self) -> (Option<Box<dyn Recognizer>>, Option<Box<dyn SpeakerEmbedder>>) {
        for c in self.captures.iter_mut() {
            c.stop();
        }
        for w in self.workers.drain(..) {
            let _ = w.join();
        }
        match self.asr.take() {
            Some(a) => match a.join() {
                Ok((r, e)) => (Some(r), e),
                Err(_) => {
                    eprintln!("RecordingHandle::stop: asr 线程异常退出（panic），模型不回收");
                    (None, None)
                }
            },
            None => (None, None),
        }
    }
```

`StartError` 增加 `pub embedder: Option<Box<dyn SpeakerEmbedder>>`(active.is_empty() 分支携带返还)。

- [ ] **Step 4: 既有测试适配**

session.rs:所有 `run_asr_worker(...)` 调用点补 `None`(embedder 位)与 `|_| {}`(on_diar 位),final 回调补第 5 参 `_`;`start_session` 调用点同理;`stop()` 返回二元组,既有 `let _ = start.handle.stop();` 不变(元组也可 `let _ =`),`stop_returns_recognizer_for_reuse` 断言改 `let (r, _e) = start.handle.stop(); assert!(r.is_some(), ...)`;`all_sources_fail_returns_recognizer_in_err` 保持(StartError 多字段不影响)。`store/writer.rs` 的 `full_session_persists_every_final`:`start_session` 补 `None` 与 `|_| {}`,final 闭包补 `_spk` 参(落盘暂仍传 null,Task 5 扩展 append_final)。

- [ ] **Step 5: 全量测试与提交**

Run: `cargo test --manifest-path src-tauri/Cargo.toml`
Expected: 全部 PASS(新增 3)。

```bash
git add src-tauri/src/session.rs src-tauri/src/store/writer.rs
git commit -m "P4 Task 4: ASR worker 段级声纹归簇,final 带 speaker,DiarEvent 回调

Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>"
```

---

### Task 5: 落盘扩展(speakers.json / 合并重写 / 读取 / 导出)

**Files:**
- Modify: `src-tauri/src/store/mod.rs`、`writer.rs`、`notes.rs`、`export.rs`

**Interfaces:**
- Produces:
  - `store::SpeakerMeta { name: String, sources: Vec<String> }`(Serialize/Deserialize/Clone/Debug/PartialEq;name 空串 = 未改名,显示端兜底「说话人 N」)
  - `Note` 增加 `speakers: std::collections::BTreeMap<String, SpeakerMeta>`
  - `NoteWriter::append_final(..., speaker: Option<&str>)`(第 5 参)
  - `NoteWriter::sync_speakers(&mut self, infos: &[(String, Vec<String>)]) -> anyhow::Result<()>`(合入新 id/sources,保留已有名字,原子写 speakers.json)
  - `NoteWriter::merge_speaker(&mut self, loser: &str, winner: &str) -> anyhow::Result<()>`(jsonl 逐行重写 loser→winner 临时文件+原子替换;speakers.json 去除 loser、sources 并入 winner)
  - `NoteStore::rename_speaker(&self, id: &str, speaker_id: &str, name: &str) -> anyhow::Result<()>`
  - `export`:label 优先 speakers 名,空名 → 「说话人 N」(id 尾数),speaker null → 我/对方

- [ ] **Step 1: 写测试**(writer.rs 与 notes.rs 测试模块追加;export.rs 改既有 speaker 测试)

```rust
    // writer.rs tests 追加
    #[test]
    fn speakers_sync_merge_and_rewrite() {
        let tmp = tempfile::tempdir().unwrap();
        let mut w = NoteWriter::create(tmp.path(), now()).unwrap();
        let id = w.note_id().to_string();
        w.append_final("mic", "甲说", 0, 2000, Some("S1")).unwrap();
        w.append_final("system", "乙说", 2000, 4000, Some("S2")).unwrap();
        w.sync_speakers(&[("S1".into(), vec!["mic".into()]), ("S2".into(), vec!["system".into()])]).unwrap();
        // 合并 S2 → S1:jsonl 重写 + speakers 表收缩
        w.merge_speaker("S2", "S1").unwrap();
        w.finalize(now()).unwrap();

        let store = crate::store::NoteStore::new(tmp.path().to_path_buf());
        let note = store.load(&id).unwrap();
        assert!(note.segments.iter().all(|s| s.speaker.as_deref() == Some("S1")), "S2 段已重写为 S1");
        assert!(note.speakers.contains_key("S1"));
        assert!(!note.speakers.contains_key("S2"));
        assert!(note.speakers["S1"].sources.contains(&"system".to_string()), "sources 并入");
    }

    // notes.rs tests 追加
    #[test]
    fn rename_speaker_persists_and_missing_file_tolerated() {
        let tmp = tempfile::tempdir().unwrap();
        let mut w = NoteWriter::create(tmp.path(), now()).unwrap();
        let id = w.note_id().to_string();
        w.append_final("mic", "x", 0, 2000, Some("S1")).unwrap();
        w.sync_speakers(&[("S1".into(), vec!["mic".into()])]).unwrap();
        w.finalize(now()).unwrap();
        let store = NoteStore::new(tmp.path().to_path_buf());
        store.rename_speaker(&id, "S1", "张三").unwrap();
        assert_eq!(store.load(&id).unwrap().speakers["S1"].name, "张三");
        // speakers.json 缺失的旧笔记(P3 产物):load 正常,speakers 为空表
        let id2 = make_note(tmp.path(), &["旧"], true);
        let n2 = store.load(&id2).unwrap();
        assert!(n2.speakers.is_empty());
    }
```

export.rs 的 `export_uses_speaker_name_when_present` 改造:构造 `Note` 时 `speakers` 塞 `{"S1": SpeakerMeta { name: "张三", sources: vec![] }}`,段 `speaker: Some("S1")`,断言 `**[张三] ...`;再加一段 `speaker: Some("S2")` 但表中无名 → 断言 `**[说话人 2] ...`;`speaker: None` 段仍走 我/对方。既有其它导出测试构造 `Note` 处补 `speakers: Default::default()`。

- [ ] **Step 2: 确认编译失败 → 实现**

要点(完整实现随测试驱动):
- `mod.rs`:`SpeakerMeta` 定义;`Note.speakers: BTreeMap<String, SpeakerMeta>`;`pub(crate) fn write_speakers_atomic(note_dir, &BTreeMap<..>)`(同 meta 策略,文件名 `speakers.json`)。
- `writer.rs`:`speakers: BTreeMap<String, SpeakerMeta>` 字段;`append_final` 第 5 参 `speaker: Option<&str>` → `SegmentRecord.speaker = speaker.map(String::from)`;`sync_speakers` 只增不删、已有 name 保留、sources 取并集,变化时原子写;`merge_speaker`:先 `flush_pending()?`(保证 jsonl 完整),读 segments.jsonl 逐行解析(不可解析行原样保留),`speaker == Some(loser)` 改 winner,写 `segments.jsonl.tmp` 后 rename;speakers 表 loser 条目移除、sources 并入 winner(名字:winner 已有名保留,否则继承 loser 的名),原子写。
- `notes.rs`:`load` 读 `speakers.json`(缺失/损坏 → 空表);`rename_speaker`:`note_dir` 校验 → 读表(缺失则新建)→ 设 name → 原子写。
- `export.rs`:`label` 签名改 `fn label<'a>(seg: &'a SegmentRecord, speakers: &'a BTreeMap<String, SpeakerMeta>) -> String`:speaker Some 且表有非空名 → 名字;Some 但无名 → `format!("说话人 {}", id.trim_start_matches('S'))`;None → 我/对方。
- P3 既有调用点:`append_final(...)` 各测试与集成调用补 `None`/`Some("S1")` 第 5 参。

- [ ] **Step 3: 全量测试与提交**

Run: `cargo test --manifest-path src-tauri/Cargo.toml`
Expected: 全部 PASS。

```bash
git add src-tauri/src/store/
git commit -m "P4 Task 5: speakers.json 持久化 + 合并重写 jsonl + 导出用说话人名

Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>"
```

---

### Task 6: lib.rs 集成 + IPC(embedder 取还 / speakers 事件 / rename_speaker)

**Files:**
- Modify: `src-tauri/src/lib.rs`、`src-tauri/src/ipc.rs`

**Interfaces:**
- Produces(前端依赖):
  - `FinalEvent` 增加 `speaker: Option<String>`
  - 新事件 `"speakers"`:`SpeakersEvent { speakers: Vec<SpeakerEntry> }`,`SpeakerEntry { id: String, name: String, sources: Vec<String> }`(name 空串=未改名)
  - command `rename_speaker(note_id: String, speaker_id: String, name: String)`(录制中的笔记**允许**改说话人名——speakers.json 不经 finalize 内存态覆写,与标题不同;实现走 NoteStore 直写 + 若为活动会话同时更新 writer 内存表并 emit)
  - `get_note` 返回的 `Note` 已含 speakers(Task 5)
  - embedder 取用/归还:与 recognizer 完全对称(take → start_session → 六个归还点 stash)

- [ ] **Step 1: ipc.rs**

`FinalEvent` 加 `pub speaker: Option<String>`;新增:

```rust
/// 说话人表(全量推送),事件名 "speakers"。name 空串 = 未改名(前端按 id 兜底)。
#[derive(Debug, Clone, Serialize)]
pub struct SpeakerEntry {
    pub id: String,
    pub name: String,
    pub sources: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct SpeakersEvent {
    pub speakers: Vec<SpeakerEntry>,
}
```

- [ ] **Step 2: lib.rs**

- `ActiveSession` 增加 `writer` 已有——无需变;`stash` 扩展:`stash_models(cache_r, cache_e, (r, e))` 或直接两次 stash(实现取简洁)。
- start_recording 加载线程:`let taken_e = embedder_cache.lock().unwrap().take();`(在 recognizer take 之后;None 时**不现场加载**——预载失败即降级,横幅由前端据 speakers 缺席判断,错误已在预载日志);六个归还点对称补 embedder(`StartError.embedder` / `stop()` 二元组 / 中间失败路径把 `taken_e` 直接 stash 回)。
- `on_final` 闭包:第 5 参 `speaker: Option<String>` → `append_final(..., speaker.as_deref())` + `FinalEvent { ..., speaker }`。
- 新 `on_diar` 闭包(move writer/app clone):

```rust
            move |ev| match ev {
                session::DiarEvent::SpeakersChanged(infos) => {
                    let pairs: Vec<(String, Vec<String>)> = infos
                        .iter()
                        .map(|s| (s.id.clone(), s.sources.iter().cloned().collect()))
                        .collect();
                    let mut w = writer_d.lock().unwrap();
                    if let Err(e) = w.sync_speakers(&pairs) {
                        eprintln!("speakers.json 写入失败: {e}");
                    }
                    let speakers = w
                        .speakers()
                        .iter()
                        .map(|(id, m)| ipc::SpeakerEntry {
                            id: id.clone(),
                            name: m.name.clone(),
                            sources: m.sources.clone(),
                        })
                        .collect();
                    drop(w);
                    let _ = app_d.emit("speakers", ipc::SpeakersEvent { speakers });
                }
                session::DiarEvent::Merged { loser, winner } => {
                    let mut w = writer_d.lock().unwrap();
                    if let Err(e) = w.merge_speaker(&loser, &winner) {
                        eprintln!("说话人合并重写失败({loser}->{winner}): {e}");
                        let _ = app_d.emit("storage", ipc::StorageEvent { state: "degraded".into() });
                    }
                    let speakers = w.speakers().iter().map(/* 同上 */).collect();
                    drop(w);
                    let _ = app_d.emit("speakers", ipc::SpeakersEvent { speakers });
                }
            },
```

(`NoteWriter` 需暴露 `pub fn speakers(&self) -> &BTreeMap<String, SpeakerMeta>`——Task 5 已有内部表,加只读访问器。)
- `rename_speaker` command:

```rust
#[tauri::command]
fn rename_speaker(app: AppHandle, state: State<AppState>, note_id: String, speaker_id: String, name: String) -> Result<(), String> {
    let name = name.trim();
    if name.is_empty() {
        return Err("名字不能为空".into());
    }
    let dir = notes_dir(&app).map_err(|e| e.to_string())?;
    store::NoteStore::new(dir).rename_speaker(&note_id, &speaker_id, name).map_err(|e| e.to_string())?;
    // 活动会话:同步 writer 内存表(防后续 sync_speakers 覆写)并广播
    if let Some(s) = state.session.lock().unwrap().as_ref() {
        if s.note_id == note_id {
            let mut w = s.writer.lock().unwrap();
            w.set_speaker_name(&speaker_id, name);
            let speakers = w.speakers().iter().map(|(id, m)| ipc::SpeakerEntry { id: id.clone(), name: m.name.clone(), sources: m.sources.clone() }).collect();
            drop(w);
            let _ = app.emit("speakers", ipc::SpeakersEvent { speakers });
        }
    }
    Ok(())
}
```

(`NoteWriter::set_speaker_name(&mut self, id, name)` 内存表更新,Task 5 的 `sync_speakers` 已保证不覆写非空名——此处补一个 setter,直接改表不落盘,落盘由 NoteStore 那次完成。)注册进 `generate_handler!`。
- 预载线程与 `speaker_model_path` 已在 Task 2;确认 setup 里 recognizer→embedder 顺序加载共用同一线程。

- [ ] **Step 3: 全量验证与提交**

Run: `cargo test --manifest-path src-tauri/Cargo.toml` && `cargo build --manifest-path src-tauri/Cargo.toml`
Expected: PASS,无新增 warning。

```bash
git add src-tauri/src/lib.rs src-tauri/src/ipc.rs src-tauri/src/store/writer.rs
git commit -m "P4 Task 6: 会话集成声纹(embedder 取还/speakers 事件/合并落盘/rename_speaker)

Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>"
```

---

### Task 7: 前端(chips 改名 / 徽章按说话人 / speakers 状态)

**Files:**
- Modify: `src/lib/events.ts`、`src/lib/notes.ts`、`src/lib/recording.svelte.ts`
- Create: `src/lib/SpeakerChips.svelte`
- Modify: `src/routes/record/+page.svelte`、`src/routes/notes/[id]/+page.svelte`

**Interfaces(要点,代码实现随之):**
- `events.ts`:`FinalEvent` 加 `speaker: string | null`;`SpeakerEntry = { id: string; name: string; sources: Source[] }`;`onSpeakers(cb)`。
- `notes.ts`:`Note` 加 `speakers: Record<string, { name: string; sources: string[] }>`;`renameSpeaker(noteId, speakerId, name)`;工具:

```ts
/** 显示名:名字 > 「说话人 N」;null → 按来源 我/对方 */
export function speakerLabel(speaker: string | null, source: Source, speakers: Record<string, { name: string }>): string {
  if (!speaker) return source === "mic" ? "我" : "对方";
  const name = speakers[speaker]?.name;
  return name || `说话人 ${speaker.replace(/^S/, "")}`;
}
/** 稳定调色板:S1..Sn 循环取色(亮/暗色下均可读) */
const PALETTE = ["#396cd8", "#2e9e5b", "#b5651d", "#8e44ad", "#c0392b", "#16808a", "#946200", "#5d6d7e"];
export function speakerColor(speaker: string | null, source: Source): string {
  if (!speaker) return source === "mic" ? "#396cd8" : "#2e9e5b";
  const n = parseInt(speaker.replace(/^S/, ""), 10) || 0;
  return PALETTE[(n - 1) % PALETTE.length];
}
```

- `recording.svelte.ts`:`speakers = $state<Record<string, { name: string; sources: string[] }>>({})` + getter;`Line` 加 `speaker: string | null`(onFinal 记录);`init()` 注册 `onSpeakers`(数组转 map);`"recording"` 时清空 speakers;暴露 `renameSpeaker` 包装(录制中改名后本地表由 speakers 事件回推,无需手动改)。
- `SpeakerChips.svelte`:props `{ speakers, noteId, editable }`;chips 列表(名字或兜底名 + 色点),点击 → 就地输入(Enter 提交 `renameSpeaker`/Escape 取消)+「这是我」快捷按钮(等价改名为「我」);空表隐藏。详情页改名成功后调用方 `refresh` + `recording.bumpNotes()`(标题同步逻辑复用);录制页由 speakers 事件驱动。
- 录制页:chips 条(`speakers` 来自 store,editable,noteId=recording.noteId);徽章:`style="background: {speakerColor(line.speaker, line.source)}"`,文本 `speakerLabel(...)`(store.speakers)。partial 徽章不变(我/对方)。
- 详情页:chips(note.speakers,editable,改名后刷新);徽章同上,speakers 取 note.speakers;导出无需改(后端已用名)。

- [ ] **Step 1-3**:按上述实现 → `npm run check`(0 errors)+ `npm run build` → 提交:

```bash
git add src/lib/ src/routes/
git commit -m "P4 Task 7: 说话人 chips 改名 + 徽章按说话人着色 + speakers 全局状态

Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>"
```

---

### Task 8: VpioMicrophone(按 spike 结论实现)

**Files:**
- Create: `src-tauri/src/audio/vpio.rs`(macOS only,`#[cfg(target_os = "macos")]`)
- Modify: `src-tauri/src/audio/mod.rs`(挂模块)、`src-tauri/src/lib.rs`(mic 源选择)

**Interfaces:**
- Produces: `VpioMicrophone::new() -> Self`,实现 `AudioCapture`(`start(&mut self, sink: Sender<AudioFrame>) -> anyhow::Result<()>`、`stop(&mut self)`),输出帧带真实 `sample_rate/channels`(下游已有重采样/mono 归一)。
- lib.rs mic 源:

```rust
        #[cfg(target_os = "macos")]
        let mic: Box<dyn AudioCapture> = Box::new(audio::vpio::VpioMicrophone::new());
        #[cfg(not(target_os = "macos"))]
        let mic: Box<dyn AudioCapture> = Box::new(audio::microphone::Microphone::new());
```

- **运行时回退**:`VpioMicrophone::start` 内部初始化失败 → 返回 Err;由于 mic 是必备源,直接 Err 会终止会话——所以在 `start` 失败时**内部回退**:new 一个 `Microphone`(cpal)代打,持有 `enum Backend { Vpio(...), Cpal(Microphone) }`,日志说明回退;`stop` 分发。start 的 ready 语义与 cpal 版一致(阻塞至流确认打开)。
- 实现细节以 Task 1 spike 报告为准(采样率/回调线程/AudioUnit 生命周期);探针代码提炼成产品实现,补错误处理与停止(uninitialize + dispose)。
- 测试:设备相关无法单测;`cargo build` + 既有 mock 管线测试不回归;人工冒烟在 Task 9。

- [ ] **Step 1-3**:实现 → `cargo test`/`cargo build` 全绿 → 提交:

```bash
git add src-tauri/src/audio/ src-tauri/src/lib.rs src-tauri/Cargo.toml src-tauri/Cargo.lock
git commit -m "P4 Task 8: VpioMicrophone(AEC)替换 mic 采集,失败回退 cpal

Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>"
```

---

### Task 9: 端到端验证 + 人工冒烟(需真人)

- [ ] **Step 1: 全量自动验证**

```bash
cargo test --manifest-path src-tauri/Cargo.toml
VN_MODELS=1 cargo test --manifest-path src-tauri/Cargo.toml --test embedder_it -- --ignored
npm run check && npm run build
```

- [ ] **Step 2: 人工冒烟清单**(`npm run tauri dev`)

1. **多方区分**:开一场线上会议(≥2 远端发言人),外放录制 → 转写段出现「说话人 1/2/…」不同徽章色;本人发言聚为独立说话人。
2. **回声**:外放场景下,对方的话**只出现一次**(system 路),mic 不再复录(AEC 生效);戴耳机场景不回归。
3. **chips 改名**:录制中把某说话人改名「张三」→ 全文即时回填;点「这是我」→ 变「我」;停止后详情页/导出用新名;重开应用仍在(speakers.json)。
4. **合并**:长会议中早期被拆成两簇的同一人若被自动合并,历史段徽章统一(观察日志 Merged 事件)。
5. **降级**:临时改名声纹模型文件 → 重启录制,横幅提示、徽章回「我/对方」,录制正常;恢复文件后重启复原。
6. P3.5 不回归:秒开、崩溃恢复、列表/详情。

- [ ] **Step 3: 阈值校准与记录**

冒烟发现聚类过碎/过合 → 调 `ASSIGN_THRESHOLD/MERGE_THRESHOLD`(±0.05 步进)重测;最终值连同观察记录进 `.superpowers/sdd/progress.md`(P4 小节),未尽项开后续任务。

---

## Self-Review 记录

- **Spec 覆盖**:管线扩展(T2/T3/T4)、speakers.json+改名+「我」认领(T5/T6/T7)、合并事件与重写(T3/T5/T6)、AEC spike+实现+回退(T1/T8)、降级链(T4 null 降级/T6 不现场加载/T7 label 兜底/T9.5)、UI chips+调色板(T7)——spec §2-§7 全覆盖。
- **占位符**:T4 Step 3 两处「原有逻辑不变」为对既有代码的保留指令(implementer 可见原文件),非 TBD;T7/T8 为要点式(前端无测试基建、VPIO 依赖 spike 结论),接口与行为均已定义。
- **类型一致性**:`append_final` 5 参在 T5 定义、T6 调用一致;`run_asr_worker` 7 参/返回二元组在 T4 定义、T6 与测试一致;`SpeakerEntry{id,name,sources}` ipc 与 events.ts 一致;`speakerLabel/speakerColor` T7 定义与两页使用一致。
- **风险排序**:VPIO(T1 spike 先行)与聚类阈值(T9 校准)是两大不确定性,均有回退/调参路径,不阻塞其余任务。
