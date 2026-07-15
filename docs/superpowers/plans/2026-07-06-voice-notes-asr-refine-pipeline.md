# ASR 精修管线 + Paraformer 选型 实施计划

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 会话停止后自动跑「幻觉过滤 → 离线重聚类 → LLM 精修」三段管线产出 refined.json,并接入 Paraformer-large 作为第三个 ASR 选型。

**Architecture:** 录制实时链路不动。新增 `refine` 模块在 stop→finalize 成功后接管后台线程:先跑纯本地两段(过滤/重聚类,读 finalize 后仍在盘上的 WAV),然后移交转码队列,最后可选跑 LLM 精修(OpenAI 兼容 chat completions,ureq 阻塞)。产物 `notes/<id>/refined.json` 独立落盘,原始三文件永不改写。

**Tech Stack:** Rust (Tauri 2, sherpa-rs 0.6.8, ureq 2, serde_json), Svelte 5 runes, afconvert(macOS 内建)。

**Spec:** `docs/superpowers/specs/2026-07-06-voice-notes-asr-tuning-design.md`

## Global Constraints

- 原始 segments.jsonl / speakers.json / meta.json / voiceprints.json 只读,精修一切产物只写 refined.json。
- 重聚类不回写声纹库累计(停止时已入库,防双计)。
- A2 默认关(`refine_enabled: false`);api_key 明文存 settings.json;UI 明示「会议文本将发送至所选服务商」。
- 不做热词;不引入 tokio/reqwest,HTTP 一律 ureq 2;JSON 一律 serde_json。
- 后台一律 `std::thread::spawn`(项目惯例,无 async runtime)。
- 所有 settings 修改走 `settings::update()`(读-改-写串行锁);落盘原子写(tmp+rename,参照 store/mod.rs `write_*_atomic`)。
- 前端禁 emoji/Unicode 符号图标;区块样式照抄 settings 页现有 `.rows/.radios/.toggles` 内联模式;色值一律 `var(--xxx)` token。
- 真实会议数据(golden 样本)只在本机引用,**绝不 commit 进仓库**(隐私)。
- 提交信息结尾:`Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>`。
- 每个任务收尾跑 `cargo test`(在 src-tauri/ 下)必须全绿再 commit;涉及前端的任务另跑 `npm run check`。

## 既有代码坐标(实施时的接线点)

- 停止链:`do_stop_recording` lib.rs:821-870;finalize 成功后 `state.transcode.enqueue(note_dir)` 在 lib.rs:854 —— **Task 11 改此处**。
- `SegmentRecord{seq,source,text,start_ms,end_ms,speaker,rms}` store/mod.rs:34-46;`SpeakerMeta` store/mod.rs:53-65;`NoteMeta` store/mod.rs:22-31。
- WAV 解码:`afconvert_decode`(m4a→16k 单声道 s16le wav) transcode.rs:55-61;`extract_wav_data`(wav→纯 PCM 字节) transcode.rs:228-246;`AUDIO_SAMPLE_RATE=16_000` audio.rs:17。
- 嵌入:`SpeakerEmbedder::embed(&mut self,&[f32])->Result<Vec<f32>>` diar/mod.rs:8-10;`SherpaEmbedder::new(&Path)` diar/mod.rs:18;模型路径 `speaker_model_path()` lib.rs:265。
- 声纹种子:`load_voiceprint_seeds(&AppHandle)->Vec<SeedCluster>` lib.rs:168-191;`SeedCluster{person,name,centroid,count}` registry.rs:45-51;`SEED_ASSIGN_THRESHOLD=0.68` registry.rs:18。
- 语言过滤:`is_foreign_final(lang,text)` session.rs:164-187(整段拦截点 session.rs:565)。
- 工厂:`new_recognizer(asr_model)` lib.rs:237-242;`current_asr(app)` lib.rs:248;`set_settings` 里 asr_changed 清槽+preload lib.rs:1411-1417。
- manifest:`Artifact/FinalFile/ArtifactKind` models/mod.rs:54-80;`ARTIFACTS` 84-161;`required_now(id,asr_model)` 166-173。
- command 注册表 lib.rs:1707-1739;事件 payload 全在 ipc.rs;前端事件封装 src/lib/events.ts,command 封装 src/lib/models.ts + notes.ts。
- 设置页 src/routes/settings/+page.svelte(区块顺序:外观410/录制453/磁盘502/系统548/存储位置597/语音模型606/语音识别677);`saveSetting()` 254-267;toggle 样式 1022-1058,radio 975-1016,banner 1111-1126。
- 笔记详情页 src/routes/notes/[id]/+page.svelte;段渲染 320-389;`displaySegments` 第 50 行。

---

### Task 1: settings 增加 Paraformer 常量与精修字段

**Files:**
- Modify: `src-tauri/src/settings.rs`

**Interfaces:**
- Produces: `pub const ASR_PARAFORMER: &str = "paraformer"`;Settings 新字段 `refine_enabled: bool` / `refine_base_url: String` / `refine_model: String` / `refine_api_key: String`(均 serde default,老 settings.json 全兼容)。

- [ ] **Step 1: 写失败测试**(追加到 settings.rs 既有 `#[cfg(test)] mod tests`)

```rust
#[test]
fn refine_defaults_off_and_empty() {
    let s = Settings::default();
    assert!(!s.refine_enabled);
    assert!(s.refine_base_url.is_empty() && s.refine_model.is_empty() && s.refine_api_key.is_empty());
    assert_eq!(ASR_PARAFORMER, "paraformer");
}

#[test]
fn old_settings_json_without_refine_fields_loads() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("settings.json"), r#"{"asr_model":"whisper"}"#).unwrap();
    let s = load(dir.path());
    assert_eq!(s.asr_model, "whisper");
    assert!(!s.refine_enabled);
}
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cargo test refine_defaults -- --nocapture`(在 src-tauri/ 下)
Expected: 编译错误 `cannot find value ASR_PARAFORMER` / `no field refine_enabled`

- [ ] **Step 3: 最小实现**

在 `pub const ASR_WHISPER: &str = "whisper";`(行11)后加:

```rust
/// Paraformer-large 中文选型。
pub const ASR_PARAFORMER: &str = "paraformer";
```

在 Settings 结构体 `tray_enabled` 字段后追加(serde 默认全兼容旧文件):

```rust
    /// 会后 LLM 精修总开关(A2)。默认关,配好 key 后由用户打开。
    #[serde(default)]
    pub refine_enabled: bool,
    /// OpenAI 兼容 chat completions 的 base_url,如 https://api.deepseek.com。
    #[serde(default)]
    pub refine_base_url: String,
    /// 模型名,如 deepseek-chat。
    #[serde(default)]
    pub refine_model: String,
    /// API key。明文存本机 settings.json(单机应用,设置页已注明)。
    #[serde(default)]
    pub refine_api_key: String,
```

`impl Default for Settings`(行74-91)对应补四行:`refine_enabled: false, refine_base_url: String::new(), refine_model: String::new(), refine_api_key: String::new(),`

- [ ] **Step 4: 跑测试确认通过**

Run: `cargo test --lib settings`
Expected: 全绿(含既有测试)

- [ ] **Step 5: Commit**

```bash
git add src-tauri/src/settings.rs
git commit -m "feat(settings): 精修字段(refine_*) + ASR_PARAFORMER 常量"
```

---

### Task 2: models manifest 增加 paraformer 工件

**Files:**
- Modify: `src-tauri/src/models/mod.rs`

**Interfaces:**
- Consumes: Task 1 的 `settings::ASR_PARAFORMER`。
- Produces: `ARTIFACTS` 新条目 id=`"paraformer"`;`required_now` 三选型语义;`paraformer` 目录名常量 `PF_DIR`。

工件实测值(2026-07-06 由 ghproxy 镜像下载全量校验,tarball 234,051,698 字节,sha256 `9c49fd9c6fb63de8e18c1054cf3d100f804741b7e608e187923cd8ff09fa9f03`;包内**只有 int8 权重,无 fp32**):

| 文件 | bytes | sha256 |
|------|-------|--------|
| `sherpa-onnx-paraformer-zh-2023-09-14/model.int8.onnx` | `243_371_218` | `f36a0433bcf096bd6d6f11b80a3ac8bed110bdca632fe0d731df8d1a84475945` |
| `sherpa-onnx-paraformer-zh-2023-09-14/tokens.txt` | `75_756` | `59aba8873a2ed1e122c25fee421e25f283b63290efbde85c1f01a853d83cb6e6` |

- [ ] **Step 1: 写失败测试**(追加到 models/mod.rs 既有 tests)

```rust
#[test]
fn paraformer_artifact_registered_and_required_semantics() {
    let a = ARTIFACTS.iter().find(|a| a.id == "paraformer").expect("paraformer 工件已注册");
    assert!(matches!(a.kind, ArtifactKind::TarBz2 { dest_dir: PF_DIR }));
    assert!(a.files.iter().any(|f| f.rel_path.ends_with("model.int8.onnx")));
    // 三选型互斥语义
    assert!(required_now("paraformer", crate::settings::ASR_PARAFORMER));
    assert!(!required_now("paraformer", crate::settings::ASR_SENSE_VOICE));
    assert!(!required_now("asr", crate::settings::ASR_PARAFORMER));
    assert!(required_now("asr", crate::settings::ASR_SENSE_VOICE));
    assert!(!required_now("whisper", crate::settings::ASR_PARAFORMER));
}
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cargo test paraformer_artifact -- --nocapture`
Expected: FAIL `paraformer 工件已注册`

- [ ] **Step 3: 实现**

`const SV_DIR`(行82)旁加 `pub const PF_DIR: &str = "sherpa-onnx-paraformer-zh-2023-09-14";`。`ARTIFACTS` 数组 whisper 条目后追加(bytes/sha256 用上方 PIN 区实测值,禁止臆造):

```rust
    Artifact {
        id: "paraformer",
        label: "语音识别（Paraformer 中文大模型）",
        url: "https://github.com/k2-fsa/sherpa-onnx/releases/download/asr-models/sherpa-onnx-paraformer-zh-2023-09-14.tar.bz2",
        kind: ArtifactKind::TarBz2 { dest_dir: PF_DIR },
        approx_mb: 224,
        prune: &["sherpa-onnx-paraformer-zh-2023-09-14/test_wavs"],
        files: &[
            FinalFile {
                rel_path: "sherpa-onnx-paraformer-zh-2023-09-14/model.int8.onnx",
                bytes: 243_371_218,
                sha256: "f36a0433bcf096bd6d6f11b80a3ac8bed110bdca632fe0d731df8d1a84475945",
            },
            FinalFile {
                rel_path: "sherpa-onnx-paraformer-zh-2023-09-14/tokens.txt",
                bytes: 75_756,
                sha256: "59aba8873a2ed1e122c25fee421e25f283b63290efbde85c1f01a853d83cb6e6",
            },
        ],
    },
```

`required_now`(行166-173)改为:

```rust
pub fn required_now(id: &str, asr_model: &str) -> bool {
    match id {
        "vad" => true,
        "asr" => {
            asr_model != crate::settings::ASR_WHISPER
                && asr_model != crate::settings::ASR_PARAFORMER
        }
        "whisper" => asr_model == crate::settings::ASR_WHISPER,
        "paraformer" => asr_model == crate::settings::ASR_PARAFORMER,
        _ => false,
    }
}
```

- [ ] **Step 4: 跑测试确认通过**

Run: `cargo test --lib models`
Expected: 全绿

- [ ] **Step 5: Commit**

```bash
git add src-tauri/src/models/mod.rs
git commit -m "feat(models): paraformer-zh-2023-09-14 工件入 manifest,三选型 required 语义"
```

---

### Task 3: ParaformerRecognizer + 工厂分支

**Files:**
- Create: `src-tauri/src/asr/paraformer.rs`
- Modify: `src-tauri/src/asr/mod.rs`(加 `pub mod paraformer;`)
- Modify: `src-tauri/src/lib.rs:231-242`(`paraformer_dir()` + `new_recognizer` 分支)

**Interfaces:**
- Consumes: `asr::{Recognizer, Transcript}`;sherpa-rs `paraformer::{ParaformerRecognizer as Inner, ParaformerConfig}`(0.6.8 已封装,结果类型 `OfflineRecognizerResult{lang,text,timestamps,tokens}`)。
- Produces: `pub struct ParaformerRecognizer`,`ParaformerRecognizer::new(model_dir: &Path) -> anyhow::Result<Self>`,实现 `Recognizer`。

- [ ] **Step 1: 写失败测试**(paraformer.rs 文件尾;模型未下载场景必须可跑,真模型测试用 `#[ignore]` 门控,与 whisper 先例一致)

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_model_dir_errors_cleanly() {
        let err = ParaformerRecognizer::new(std::path::Path::new("/nonexistent-pf-dir"))
            .err()
            .expect("目录不存在应报错而非 panic");
        assert!(err.to_string().contains("nonexistent-pf-dir"));
    }

    /// 需本机已下载 paraformer 工件:cargo test --lib asr::paraformer -- --ignored
    #[test]
    #[ignore]
    fn transcribes_nonempty_with_timestamps() {
        let dir = crate::models::root().join(crate::models::PF_DIR);
        let mut r = ParaformerRecognizer::new(&dir).unwrap();
        // 1s 静音也应返回结构完整(text 可空);真语音断言用 golden 脚本做
        let t = r.recognize(&vec![0.0f32; 16000]).unwrap();
        assert_eq!(t.tokens.len(), t.timestamps.len());
    }
}
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cargo test missing_model_dir_errors -- --nocapture`
Expected: 编译错误(模块不存在)

- [ ] **Step 3: 实现 paraformer.rs**

```rust
use super::{Recognizer, Transcript};
use std::path::Path;

/// 基于 sherpa-onnx 的离线 Paraformer-large 识别器(中文,带 token 级时间戳)。
pub struct ParaformerRecognizer {
    inner: sherpa_rs::paraformer::ParaformerRecognizer,
}

impl ParaformerRecognizer {
    /// model_dir 应包含 model.int8.onnx 与 tokens.txt(manifest PF_DIR 解压布局)。
    pub fn new(model_dir: &Path) -> anyhow::Result<Self> {
        let model = model_dir.join("model.int8.onnx");
        let tokens = model_dir.join("tokens.txt");
        if !model.exists() || !tokens.exists() {
            anyhow::bail!("在 {:?} 找不到 model.int8.onnx / tokens.txt", model_dir);
        }
        let num_threads = std::thread::available_parallelism()
            .map(|n| n.get().min(8) as i32)
            .unwrap_or(4);
        let config = sherpa_rs::paraformer::ParaformerConfig {
            model: model.to_string_lossy().into_owned(),
            tokens: tokens.to_string_lossy().into_owned(),
            num_threads: Some(num_threads),
            ..Default::default()
        };
        let inner = sherpa_rs::paraformer::ParaformerRecognizer::new(config)
            .map_err(|e| anyhow::anyhow!("加载 Paraformer 失败: {e}"))?;
        Ok(Self { inner })
    }
}

impl Recognizer for ParaformerRecognizer {
    fn recognize(&mut self, samples: &[f32]) -> anyhow::Result<Transcript> {
        let result = self.inner.transcribe(16000, samples);
        Ok(Transcript {
            text: result.text,
            lang: result.lang, // paraformer 无语言标签时为空串:语言过滤走文本兜底(whisper 同路径)
            tokens: result.tokens,
            timestamps: result.timestamps,
        })
    }
}
```

asr/mod.rs 第 2 行后加 `pub mod paraformer;`。

- [ ] **Step 4: lib.rs 工厂接线**

`whisper_dir()`(行235)后加:

```rust
fn paraformer_dir() -> std::path::PathBuf {
    models::root().join(models::PF_DIR)
}
```

`new_recognizer`(行237-242)改为三分支:

```rust
fn new_recognizer(asr_model: &str) -> anyhow::Result<Box<dyn asr::Recognizer>> {
    if asr_model == settings::ASR_WHISPER {
        Ok(Box::new(asr::whisper::WhisperRecognizer::new(&whisper_dir())?))
    } else if asr_model == settings::ASR_PARAFORMER {
        Ok(Box::new(asr::paraformer::ParaformerRecognizer::new(&paraformer_dir())?))
    } else {
        Ok(Box::new(asr::sense_voice::SenseVoiceRecognizer::new(&sense_voice_dir())?))
    }
}
```

- [ ] **Step 5: 跑测试确认通过**

Run: `cargo test --lib`(全量,确认无回归)
Expected: 全绿;`missing_model_dir_errors_cleanly` PASS

- [ ] **Step 6: Commit**

```bash
git add src-tauri/src/asr/paraformer.rs src-tauri/src/asr/mod.rs src-tauri/src/lib.rs
git commit -m "feat(asr): ParaformerRecognizer 接入工厂,三选型可切"
```

---

### Task 4: refine/filter.rs 幻觉过滤(A3)

**Files:**
- Create: `src-tauri/src/refine/mod.rs`(本任务只放 `pub mod filter;`,orchestrator 在 Task 10 填)
- Create: `src-tauri/src/refine/filter.rs`
- Modify: `src-tauri/src/lib.rs`(声明 `mod refine;`)

**Interfaces:**
- Consumes: `store::SegmentRecord`(只读字段 text/start_ms/end_ms/rms)。
- Produces: `pub fn is_hallucination(text: &str, dur_ms: u64, rms: Option<f32>, lang: &str) -> bool`;`pub fn discarded_seqs(segs: &[crate::store::SegmentRecord]) -> Vec<u64>`(注:SegmentRecord 无 lang 字段,整段判定只用 text/dur/rms 三信号;lang 参数供 Task 10 内存路径可选传入,盘上路径恒传 `""`)。

规则(常量全 pub,供 golden 校准调整):

```
WHITELIST: 好|对|嗯|行|是|好的|对的|嗯嗯|行吧|可以|OK|ok|噢|哦|喔|欸|诶|嗯哼|没了|没有 → 永不过滤
dur_ms < 2000 且 有效字符数(去标点/空白) == 0            → 过滤
dur_ms < 2000 且 有效字符数 <= 2 且 不在白名单            → 过滤
dur_ms < 3000 且 lang ∈ {yue,ja,ko} 且 有效字符数 <= 4   → 过滤
```

本场 golden 预期:seq 1,2,21,26,27,63,233,246,319,333,394?,399?,414,446 中——**必须命中**:1(「。」),2(「男人。」),21,26(「播放。」),27(「隔你。」),63,233(「那个。」),246(「闭画。」),319,333,414,446;**必须放过**:394(「好。」),399(「对.」)。

- [ ] **Step 1: 写失败测试**(filter.rs 文件尾)

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn junk_short_segments_hit() {
        assert!(is_hallucination("。", 1530, Some(0.0159), ""));
        assert!(is_hallucination("男人。", 642, Some(0.0146), ""));
        assert!(is_hallucination("播放。", 1800, Some(0.0044), ""));
        assert!(is_hallucination("隔你。", 1900, Some(0.0050), ""));
        assert!(is_hallucination("那个。", 1500, Some(0.0092), ""));
        assert!(is_hallucination("", 1000, Some(0.0005), ""));
    }

    #[test]
    fn real_short_acks_pass() {
        assert!(!is_hallucination("好。", 1400, Some(0.0050), ""));
        assert!(!is_hallucination("对.", 1200, Some(0.0048), ""));
        assert!(!is_hallucination("OK。", 900, Some(0.0100), ""));
        assert!(!is_hallucination("嗯嗯。", 800, Some(0.0060), ""));
    }

    #[test]
    fn long_segments_never_filtered() {
        assert!(!is_hallucination("男", 2100, Some(0.001), "")); // 过 2s 不适用短段规则
        assert!(!is_hallucination("这是一个正常长度的句子内容。", 5000, Some(0.01), ""));
    }

    #[test]
    fn lang_drift_short_hit_but_longer_pass() {
        assert!(is_hallucination("唔，好嘅", 2500, Some(0.008), "yue"));
        assert!(!is_hallucination("唔该借歪唔该借歪要落车", 2500, Some(0.02), "yue")); // >4 有效字,可能真粤语
    }

    #[test]
    fn discarded_seqs_maps_over_records() {
        let mk = |seq, text: &str, dur: u64, rms| crate::store::SegmentRecord {
            seq, source: "mic".into(), text: text.into(),
            start_ms: 0, end_ms: dur, speaker: None, rms: Some(rms),
        };
        let segs = vec![mk(0, "男人。", 642, 0.0146), mk(1, "好。", 1400, 0.005), mk(2, "正常说话内容在这里。", 4000, 0.02)];
        assert_eq!(discarded_seqs(&segs), vec![0]);
    }
}
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cargo test refine::filter -- --nocapture`
Expected: 编译错误(模块不存在)

- [ ] **Step 3: 实现 filter.rs**

```rust
//! A3 幻觉过滤:短段联合判定(时长 × 有效字符 × 语种漂移),白名单保真实短应答。
//! 常量 pub,由 scripts/refine_golden.py 对真实会议样本校准。

/// 短段判定上限(毫秒)。
pub const SHORT_MS: u64 = 2000;
/// 语种漂移判定上限(毫秒)。
pub const DRIFT_MS: u64 = 3000;
/// 语种漂移下的有效字符上限。
pub const DRIFT_MAX_CHARS: usize = 4;

/// 真实短应答白名单(去标点后全匹配)。
const WHITELIST: &[&str] = &[
    "好", "对", "嗯", "行", "是", "好的", "对的", "嗯嗯", "行吧", "可以",
    "ok", "噢", "哦", "喔", "欸", "诶", "嗯哼", "没了", "没有",
];

/// 去标点/空白后的有效字符序列(小写)。
fn effective_chars(text: &str) -> String {
    text.chars()
        .filter(|c| c.is_alphanumeric())
        .collect::<String>()
        .to_lowercase()
}

/// 幻觉判定。lang 为 SenseVoice 标签(可含 <|..|> 包裹)或空串。
pub fn is_hallucination(text: &str, dur_ms: u64, _rms: Option<f32>, lang: &str) -> bool {
    let eff = effective_chars(text);
    if WHITELIST.contains(&eff.as_str()) {
        return false;
    }
    let n = eff.chars().count();
    if dur_ms < SHORT_MS && n == 0 {
        return true;
    }
    if dur_ms < SHORT_MS && n <= 2 {
        return true;
    }
    let tag = lang.trim_start_matches("<|").trim_end_matches("|>").to_lowercase();
    if dur_ms < DRIFT_MS && matches!(tag.as_str(), "yue" | "ja" | "ko") && n <= DRIFT_MAX_CHARS {
        return true;
    }
    false
}

/// 对整份 segments 求应丢弃 seq 集(盘上无 lang,恒传空)。
pub fn discarded_seqs(segs: &[crate::store::SegmentRecord]) -> Vec<u64> {
    segs.iter()
        .filter(|s| is_hallucination(&s.text, s.end_ms.saturating_sub(s.start_ms), s.rms, ""))
        .map(|s| s.seq)
        .collect()
}
```

refine/mod.rs 暂只有一行 `pub mod filter;`。lib.rs mod 声明区(`mod diar;` 附近)加 `mod refine;`。

- [ ] **Step 4: 跑测试确认通过**

Run: `cargo test refine::filter`
Expected: 5 个测试全绿

- [ ] **Step 5: Commit**

```bash
git add src-tauri/src/refine/ src-tauri/src/lib.rs
git commit -m "feat(refine): A3 幻觉过滤,白名单+短段联合判定"
```

---

### Task 5: PCM 解码切片助手

**Files:**
- Modify: `src-tauri/src/store/transcode.rs`(新增两个 pub 函数,复用私有 `afconvert_decode`/`extract_wav_data`)

**Interfaces:**
- Produces:
  - `pub fn read_wav_f32(wav: &Path) -> anyhow::Result<Vec<f32>>`(16k 单声道 s16le wav → f32 samples)
  - `pub fn track_pcm(note_dir: &Path, source: &str) -> anyhow::Result<Vec<f32>>`(优先读 `<source>.wav`;没有则解码 `<source>.m4a` 到临时文件再读,用后即删)
- Consumes: Task 10 orchestrator 按 `start_ms/end_ms` 切片:`&pcm[(start_ms*16) as usize .. (end_ms*16) as usize]`(16 samples/ms)。

- [ ] **Step 1: 写失败测试**(transcode.rs tests 区,项目已有 hound dev 依赖写 wav 的先例:audio.rs:405)

```rust
#[test]
fn read_wav_f32_roundtrip() {
    let dir = tempfile::tempdir().unwrap();
    let wav = dir.path().join("t.wav");
    let spec = hound::WavSpec { channels: 1, sample_rate: 16000, bits_per_sample: 16, sample_format: hound::SampleFormat::Int };
    let mut w = hound::WavWriter::create(&wav, spec).unwrap();
    for i in 0..1600 { w.write_sample(((i % 100) * 300) as i16).unwrap(); }
    w.finalize().unwrap();
    let pcm = read_wav_f32(&wav).unwrap();
    assert_eq!(pcm.len(), 1600);
    assert!(pcm.iter().all(|x| x.abs() <= 1.0));
}

#[test]
fn track_pcm_prefers_wav_and_falls_back_to_m4a() {
    let dir = tempfile::tempdir().unwrap();
    // 只有 wav:直读
    let wav = dir.path().join("mic.wav");
    let spec = hound::WavSpec { channels: 1, sample_rate: 16000, bits_per_sample: 16, sample_format: hound::SampleFormat::Int };
    let mut w = hound::WavWriter::create(&wav, spec).unwrap();
    for _ in 0..320 { w.write_sample(1000i16).unwrap(); }
    w.finalize().unwrap();
    assert_eq!(track_pcm(dir.path(), "mic").unwrap().len(), 320);
    // 两者皆无:报错带路径
    let err = track_pcm(dir.path(), "system").unwrap_err();
    assert!(err.to_string().contains("system"));
}
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cargo test track_pcm -- --nocapture`
Expected: 编译错误(函数不存在)

- [ ] **Step 3: 实现**(transcode.rs 尾部,tests 前)

```rust
/// 16k 单声道 s16le WAV → f32 samples(复用 extract_wav_data 的胖头兼容)。
pub fn read_wav_f32(wav: &Path) -> anyhow::Result<Vec<f32>> {
    let data = extract_wav_data(wav)?;
    Ok(data
        .chunks_exact(2)
        .map(|b| i16::from_le_bytes([b[0], b[1]]) as f32 / 32768.0)
        .collect())
}

/// 取某音轨全场 PCM:停止后 wav 尚在盘上直读;转码完成后仅剩 m4a 则解码到临时 wav 再读。
pub fn track_pcm(note_dir: &Path, source: &str) -> anyhow::Result<Vec<f32>> {
    let wav = note_dir.join(format!("{source}.wav"));
    if wav.exists() {
        return read_wav_f32(&wav);
    }
    let m4a = note_dir.join(format!("{source}.m4a"));
    if !m4a.exists() {
        anyhow::bail!("音轨 {source} 的 wav/m4a 均不存在于 {:?}", note_dir);
    }
    let tmp = note_dir.join(format!(".{source}.refine.wav.tmp"));
    let _ = std::fs::remove_file(&tmp);
    afconvert_decode(&m4a, &tmp)?;
    let out = read_wav_f32(&tmp);
    let _ = std::fs::remove_file(&tmp);
    out
}
```

- [ ] **Step 4: 跑测试确认通过**

Run: `cargo test --lib store::transcode`
Expected: 全绿(注:`track_pcm` 的 m4a 回退分支依赖 afconvert,真实解码已被 `decode_note_to_wav` 既有测试覆盖,此处不重复)

- [ ] **Step 5: Commit**

```bash
git add src-tauri/src/store/transcode.rs
git commit -m "feat(store): track_pcm/read_wav_f32 供精修管线取全场音频"
```

---

### Task 6: refine/recluster.rs 离线重聚类(A1)

**Files:**
- Create: `src-tauri/src/refine/recluster.rs`
- Modify: `src-tauri/src/refine/mod.rs`(加 `pub mod recluster;`)

**Interfaces:**
- Consumes: `diar::SpeakerEmbedder`(Task 10 传入 `SherpaEmbedder` 或测试 Mock);`registry::SeedCluster`。
- Produces:
  - `pub struct SegInput { pub seq: u64, pub start_ms: u64, pub end_ms: u64, pub source: String, pub old_speaker: Option<String> }`
  - `pub fn recluster(inputs: &[SegInput], embs: &[Option<Vec<f32>>], seeds: &[crate::diar::registry::SeedCluster]) -> Vec<Assignment>`,其中 `pub struct Assignment { pub seq: u64, pub speaker: String, pub name: Option<String> }`(speaker 为重聚类新标签 "R1".."Rk",按总时长降序编号)
  - 常量:`pub const AHC_THRESHOLD: f32 = 0.60;`(golden 校准)/`pub const MIN_CLUSTER_MS: u64 = 8000;`/`pub const MIN_EMBED_MS: u64 = 1500;`
- 纯逻辑无 IO/无模型依赖(嵌入由调用方算好传入,与 registry.rs 同风格),embs 与 inputs 等长对位,`None` = 短段/丢弃段不参与聚类。

算法(n≈450 上限,O(n³) 可接受):

1. 有嵌入的段各自成簇(单位化向量);无嵌入段搁置。
2. 迭代:找全局最相似簇对(质心余弦),≥ `AHC_THRESHOLD` 则合并(质心=成员均值再归一化,时长相加),否则停。
3. 碎片治理:总时长 < `MIN_CLUSTER_MS` 的簇并入质心最近的大簇(无条件,不看阈值;若全场无大簇则保留)。
4. 编号:按簇总时长降序命名 R1..Rk。
5. 种子命名:簇质心对每个 seed 算余弦,最高且 ≥ `registry::SEED_ASSIGN_THRESHOLD` 者取其 name(非空才用)。
6. 无嵌入段归属:取时间上最近的有标签邻段(前后各找,取 gap 小者)的新标签;全场无簇时保留 old_speaker 原值(speaker 取 old_speaker.unwrap_or("R1"))。

- [ ] **Step 1: 写失败测试**

```rust
#[cfg(test)]
mod tests {
    use super::*;

    fn seg(seq: u64, start: u64, end: u64) -> SegInput {
        SegInput { seq, start_ms: start, end_ms: end, source: "mic".into(), old_speaker: None }
    }
    /// 三维玩具向量:同人同方向+微噪
    fn v(base: [f32; 3], jitter: f32) -> Option<Vec<f32>> {
        Some(vec![base[0] + jitter, base[1] - jitter, base[2]])
    }

    #[test]
    fn two_speakers_separate_and_fragments_absorbed() {
        let a = [1.0, 0.0, 0.0];
        let b = [0.0, 1.0, 0.0];
        let inputs = vec![
            seg(0, 0, 10_000), seg(1, 10_000, 20_000), seg(2, 20_000, 30_000), // A 30s
            seg(3, 30_000, 40_000), seg(4, 40_000, 50_000),                     // B 20s
            seg(5, 50_000, 52_000),                                             // A 碎片 2s(独立会成小簇)
        ];
        let embs = vec![v(a, 0.01), v(a, 0.02), v(a, 0.0), v(b, 0.01), v(b, 0.0), v(a, 0.03)];
        let out = recluster(&inputs, &embs, &[]);
        let l = |q: u64| out.iter().find(|x| x.seq == q).unwrap().speaker.clone();
        assert_eq!(l(0), l(1));
        assert_eq!(l(0), l(2));
        assert_eq!(l(3), l(4));
        assert_ne!(l(0), l(3));
        assert_eq!(l(5), l(0), "2s 碎片簇应并入最近大簇 A");
        assert_eq!(l(0), "R1", "A 总时长最长应为 R1");
    }

    #[test]
    fn short_segment_without_embedding_follows_nearest_neighbor() {
        let a = [1.0, 0.0, 0.0];
        let inputs = vec![seg(0, 0, 10_000), seg(1, 10_100, 11_000), seg(2, 20_000, 30_000)];
        let embs = vec![v(a, 0.0), None, v(a, 0.01)];
        let out = recluster(&inputs, &embs, &[]);
        assert_eq!(out[1].speaker, out[0].speaker, "无嵌入短段跟时间最近邻(gap 100ms < 9s)");
    }

    #[test]
    fn seed_naming_applies_above_threshold() {
        let a = [1.0, 0.0, 0.0];
        let inputs = vec![seg(0, 0, 10_000), seg(1, 10_000, 20_000)];
        let embs = vec![v(a, 0.0), v(a, 0.01)];
        let seeds = vec![crate::diar::registry::SeedCluster {
            person: "P1".into(), name: "张三".into(), centroid: vec![1.0, 0.0, 0.0], count: 5,
        }];
        let out = recluster(&inputs, &embs, &seeds);
        assert_eq!(out[0].name.as_deref(), Some("张三"));
    }

    #[test]
    fn all_none_embeddings_keeps_old_speakers() {
        let mut i0 = seg(0, 0, 1000); i0.old_speaker = Some("S8".into());
        let out = recluster(&[i0], &[None], &[]);
        assert_eq!(out[0].speaker, "S8");
    }
}
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cargo test refine::recluster -- --nocapture`
Expected: 编译错误(模块不存在)

- [ ] **Step 3: 实现 recluster.rs**

```rust
//! A1 离线全局重聚类:AHC 平均链接(质心近似)。纯逻辑,嵌入由调用方提供。
//! 在线单遍聚类(registry.rs)只做录制中临时标签;本模块产终稿。

use crate::diar::registry::{SeedCluster, SEED_ASSIGN_THRESHOLD};

/// AHC 合并阈值(余弦)。低于在线 MERGE_THRESHOLD(0.74):全局视角下可更宽。golden 校准。
pub const AHC_THRESHOLD: f32 = 0.60;
/// 小于此总时长(ms)的簇为碎片,无条件并入最近大簇。
pub const MIN_CLUSTER_MS: u64 = 8000;
/// 段时长低于此值(ms)不提嵌入(调用方遵守;本模块按 embs=None 处理)。
pub const MIN_EMBED_MS: u64 = 1500;

pub struct SegInput {
    pub seq: u64,
    pub start_ms: u64,
    pub end_ms: u64,
    pub source: String,
    pub old_speaker: Option<String>,
}

pub struct Assignment {
    pub seq: u64,
    pub speaker: String,
    pub name: Option<String>,
}

fn normalize(v: &[f32]) -> Option<Vec<f32>> {
    let n: f32 = v.iter().map(|x| x * x).sum::<f32>().sqrt();
    if !n.is_finite() || n < 1e-6 { return None; }
    Some(v.iter().map(|x| x / n).collect())
}

fn dot(a: &[f32], b: &[f32]) -> f32 {
    a.iter().zip(b).map(|(x, y)| x * y).sum()
}

struct Cl {
    centroid: Vec<f32>, // 单位化
    members: Vec<usize>, // inputs 下标
    total_ms: u64,
}

fn merge_centroid(a: &Cl, b: &Cl) -> Vec<f32> {
    let wa = a.members.len() as f32;
    let wb = b.members.len() as f32;
    let mixed: Vec<f32> = a.centroid.iter().zip(&b.centroid)
        .map(|(x, y)| x * wa + y * wb).collect();
    normalize(&mixed).unwrap_or_else(|| a.centroid.clone())
}

pub fn recluster(inputs: &[SegInput], embs: &[Option<Vec<f32>>], seeds: &[SeedCluster]) -> Vec<Assignment> {
    assert_eq!(inputs.len(), embs.len());
    // 1. 建初始簇
    let mut cls: Vec<Cl> = Vec::new();
    for (i, e) in embs.iter().enumerate() {
        if let Some(u) = e.as_ref().and_then(|v| normalize(v)) {
            cls.push(Cl { centroid: u, members: vec![i], total_ms: inputs[i].end_ms.saturating_sub(inputs[i].start_ms) });
        }
    }
    // 2. AHC:每轮合并全局最相似对
    loop {
        let mut best: Option<(usize, usize, f32)> = None;
        for i in 0..cls.len() {
            for j in (i + 1)..cls.len() {
                let sim = dot(&cls[i].centroid, &cls[j].centroid);
                if best.map_or(true, |(_, _, s)| sim > s) {
                    best = Some((i, j, sim));
                }
            }
        }
        match best {
            Some((i, j, sim)) if sim >= AHC_THRESHOLD => {
                let b = cls.swap_remove(j);
                let a = &mut cls[if i < j { i } else { i - 0 }]; // swap_remove(j) 不影响 i<j 的下标
                a.centroid = merge_centroid(a, &b);
                a.members.extend(b.members);
                a.total_ms += b.total_ms;
            }
            _ => break,
        }
    }
    // 3. 碎片并入最近大簇
    loop {
        let Some(frag_idx) = cls.iter().enumerate()
            .filter(|(_, c)| c.total_ms < MIN_CLUSTER_MS)
            .min_by_key(|(_, c)| c.total_ms)
            .map(|(i, _)| i) else { break };
        let bigs: Vec<usize> = (0..cls.len()).filter(|&i| i != frag_idx && cls[i].total_ms >= MIN_CLUSTER_MS).collect();
        if bigs.is_empty() { break; }
        let tgt = *bigs.iter()
            .max_by(|&&a, &&b| dot(&cls[frag_idx].centroid, &cls[a].centroid)
                .total_cmp(&dot(&cls[frag_idx].centroid, &cls[b].centroid)))
            .unwrap();
        let f = cls.swap_remove(frag_idx);
        let tgt = if tgt > frag_idx { tgt - 1 } else { tgt };
        let t = &mut cls[tgt];
        t.centroid = merge_centroid(t, &f);
        t.members.extend(f.members);
        t.total_ms += f.total_ms;
    }
    // 4. 按总时长降序编号 R1..Rk
    cls.sort_by(|a, b| b.total_ms.cmp(&a.total_ms));
    // 5. 种子命名
    let names: Vec<Option<String>> = cls.iter().map(|c| {
        seeds.iter()
            .filter(|s| !s.name.is_empty())
            .filter_map(|s| normalize(&s.centroid).map(|u| (s, dot(&c.centroid, &u))))
            .filter(|(_, sim)| *sim >= SEED_ASSIGN_THRESHOLD)
            .max_by(|a, b| a.1.total_cmp(&b.1))
            .map(|(s, _)| s.name.clone())
    }).collect();
    // 6. 输出:先落有嵌入段,再给无嵌入段找时间最近邻
    let mut label: Vec<Option<usize>> = vec![None; inputs.len()];
    for (k, c) in cls.iter().enumerate() {
        for &m in &c.members { label[m] = Some(k); }
    }
    let labeled: Vec<usize> = (0..inputs.len()).filter(|&i| label[i].is_some()).collect();
    (0..inputs.len()).map(|i| {
        let k = label[i].or_else(|| {
            labeled.iter()
                .min_by_key(|&&j| {
                    let (a, b) = (&inputs[i], &inputs[j]);
                    if b.end_ms <= a.start_ms { a.start_ms - b.end_ms }
                    else if a.end_ms <= b.start_ms { b.start_ms - a.end_ms }
                    else { 0 }
                })
                .and_then(|&j| label[j])
        });
        match k {
            Some(k) => Assignment { seq: inputs[i].seq, speaker: format!("R{}", k + 1), name: names[k].clone() },
            None => Assignment {
                seq: inputs[i].seq,
                speaker: inputs[i].old_speaker.clone().unwrap_or_else(|| "R1".into()),
                name: None,
            },
        }
    }).collect()
}
```

refine/mod.rs 加 `pub mod recluster;`。

- [ ] **Step 4: 跑测试确认通过**

Run: `cargo test refine::recluster`
Expected: 4 个测试全绿

- [ ] **Step 5: Commit**

```bash
git add src-tauri/src/refine/
git commit -m "feat(refine): A1 AHC 离线重聚类,碎片归并+种子命名+近邻兜底"
```

---

### Task 7: store/refined.rs 精修产物读写

**Files:**
- Create: `src-tauri/src/store/refined.rs`
- Modify: `src-tauri/src/store/mod.rs`(声明 `pub mod refined;` 并 re-export)

**Interfaces:**
- Produces(serde 结构,前端 TS 镜像在 Task 12):

```rust
pub struct RefinedParagraph { pub speaker: String, pub name: Option<String>, pub start_ms: u64, pub end_ms: u64, pub text: String, pub source_seqs: Vec<u64> }
pub struct RefineStages { pub filter: String, pub recluster: String, pub llm: String } // "done"|"failed"|"skipped"|"partial"|"off"
pub struct RefinedDoc { pub schema_version: u32, pub generated_at: String, pub llm_model: Option<String>, pub stages: RefineStages, pub discarded_seqs: Vec<u64>, pub paragraphs: Vec<RefinedParagraph> }
pub fn write_refined_atomic(note_dir: &Path, doc: &RefinedDoc) -> anyhow::Result<()>
pub fn load_refined(note_dir: &Path) -> Option<RefinedDoc>   // 缺失/损坏 → None
```

- [ ] **Step 1: 写失败测试**(refined.rs 尾)

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_and_corrupt_returns_none() {
        let dir = tempfile::tempdir().unwrap();
        assert!(load_refined(dir.path()).is_none(), "缺失返回 None");
        let doc = RefinedDoc {
            schema_version: 1,
            generated_at: "2026-07-06T15:00:00+08:00".into(),
            llm_model: Some("deepseek-chat".into()),
            stages: RefineStages { filter: "done".into(), recluster: "done".into(), llm: "off".into() },
            discarded_seqs: vec![1, 2],
            paragraphs: vec![RefinedParagraph {
                speaker: "R1".into(), name: Some("张三".into()),
                start_ms: 0, end_ms: 5000, text: "你好。".into(), source_seqs: vec![0, 3],
            }],
        };
        write_refined_atomic(dir.path(), &doc).unwrap();
        let got = load_refined(dir.path()).expect("写后可读");
        assert_eq!(got.paragraphs.len(), 1);
        assert_eq!(got.discarded_seqs, vec![1, 2]);
        assert_eq!(got.paragraphs[0].name.as_deref(), Some("张三"));
        std::fs::write(dir.path().join("refined.json"), "{broken").unwrap();
        assert!(load_refined(dir.path()).is_none(), "损坏返回 None 不 panic");
    }
}
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cargo test store::refined -- --nocapture`
Expected: 编译错误(模块不存在)

- [ ] **Step 3: 实现 refined.rs**

```rust
//! 精修产物 refined.json:原始三文件之外的独立终稿,损坏/缺失时 UI 回落原始逐字稿。

use serde::{Deserialize, Serialize};
use std::path::Path;

pub const REFINED_SCHEMA_VERSION: u32 = 1;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RefinedParagraph {
    pub speaker: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    pub start_ms: u64,
    pub end_ms: u64,
    pub text: String,
    pub source_seqs: Vec<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RefineStages {
    pub filter: String,
    pub recluster: String,
    pub llm: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RefinedDoc {
    pub schema_version: u32,
    pub generated_at: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub llm_model: Option<String>,
    pub stages: RefineStages,
    #[serde(default)]
    pub discarded_seqs: Vec<u64>,
    pub paragraphs: Vec<RefinedParagraph>,
}

pub fn write_refined_atomic(note_dir: &Path, doc: &RefinedDoc) -> anyhow::Result<()> {
    let tmp = note_dir.join("refined.json.tmp");
    std::fs::write(&tmp, serde_json::to_vec_pretty(doc)?)?;
    std::fs::rename(&tmp, note_dir.join("refined.json"))?;
    Ok(())
}

pub fn load_refined(note_dir: &Path) -> Option<RefinedDoc> {
    let bytes = std::fs::read(note_dir.join("refined.json")).ok()?;
    serde_json::from_slice(&bytes).ok()
}
```

store/mod.rs 声明区加 `pub mod refined;` 与 `pub use refined::{load_refined, write_refined_atomic, RefineStages, RefinedDoc, RefinedParagraph};`(参照既有 re-export 风格)。

- [ ] **Step 4: 跑测试确认通过**

Run: `cargo test store::refined`
Expected: 全绿

- [ ] **Step 5: Commit**

```bash
git add src-tauri/src/store/
git commit -m "feat(store): refined.json 读写(原子写,损坏回落 None)"
```

---

### Task 8: refine/llm.rs LLM 精修客户端(A2)

**Files:**
- Create: `src-tauri/src/refine/llm.rs`
- Modify: `src-tauri/src/refine/mod.rs`(加 `pub mod llm;`)

**Interfaces:**
- Consumes: `store::RefinedParagraph`(改写其 text);Task 1 settings 的 base_url/model/key。
- Produces:
  - `pub struct LlmConfig { pub base_url: String, pub model: String, pub api_key: String }`(base_url **含版本段**,如 `https://api.deepseek.com/v1`、`https://ark.cn-beijing.volces.com/api/v3`;客户端只拼 `/chat/completions`,以兼容豆包 Ark 等非 `/v1` 路径)
  - `pub fn polish(cfg: &LlmConfig, paragraphs: &mut [crate::store::RefinedParagraph]) -> LlmOutcome`,`pub enum LlmOutcome { Done, Partial(usize /*失败块数*/), Failed }`
  - 常量 `pub const CHUNK_CHARS: usize = 3000;` `pub const REQ_TIMEOUT_S: u64 = 60;`
- 行为:段落按顺序切块(累计字符 ≤ CHUNK_CHARS);每块一次 chat completions 调用;响应解析出 `{glossary, texts[]}`;glossary(实体归一表)串行传递给下一块保持全文一致;某块失败(网络/解析/长度不符)则该块原文保留、计入 Partial。

**Prompt(常量,系统消息原文):**

```text
你是会议逐字稿精修助手。对输入的每个段落做四件事,除此之外禁止任何改动:
1. 纠正同音/近音错字(如「肯计→肯定」),不确定时保留原文,禁止改写句式或语义;
2. 实体归一:同一人名/产品名/术语全文统一为最常见或术语表给定的写法;
3. 轻度清理口头语:删除无意义的「嗯」「呃」及紧邻重复(「我们我们→我们」),保留语气词「吧」「啊」等;
4. 英文与数字排版:英文词组与中文之间加空格,产品名保持原大小写。
输出 JSON:{"glossary":{"错误写法":"统一写法",...},"texts":["段落1修订文","段落2修订文",...]}。
texts 数组长度必须与输入段落数一致,顺序一致。glossary 只收实体类归一项。
```

用户消息:`术语表(沿用并可扩充):{glossary_json}\n段落:\n1. <text>\n2. <text>...`

- [ ] **Step 1: 写失败测试**(llm.rs 尾;mock 用 std TcpListener 手写最小 HTTP 响应,零新依赖)

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};

    /// 起一个只响应一次的本地 mock,返回给定 body 的 200 JSON。
    fn mock_server(responses: Vec<String>) -> String {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        std::thread::spawn(move || {
            for body in responses {
                let (mut s, _) = listener.accept().unwrap();
                let mut buf = [0u8; 65536];
                let _ = s.read(&mut buf); // 丢弃请求
                let resp = format!(
                    "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\n\r\n{}",
                    body.len(), body
                );
                let _ = s.write_all(resp.as_bytes());
            }
        });
        format!("http://{addr}")
    }

    fn chat_body(texts: &[&str], glossary: &str) -> String {
        let content = serde_json::json!({ "glossary": serde_json::from_str::<serde_json::Value>(glossary).unwrap(), "texts": texts })
            .to_string();
        serde_json::json!({ "choices": [{ "message": { "content": content } }] }).to_string()
    }

    fn para(text: &str) -> crate::store::RefinedParagraph {
        crate::store::RefinedParagraph {
            speaker: "R1".into(), name: None, start_ms: 0, end_ms: 1000,
            text: text.into(), source_seqs: vec![0],
        }
    }

    #[test]
    fn polish_rewrites_texts_on_success() {
        let base = mock_server(vec![chat_body(&["我们肯定要做。"], r#"{"肯计":"肯定"}"#)]);
        let cfg = LlmConfig { base_url: base, model: "m".into(), api_key: "k".into() };
        let mut ps = vec![para("我们肯计要做。")];
        assert!(matches!(polish(&cfg, &mut ps), LlmOutcome::Done));
        assert_eq!(ps[0].text, "我们肯定要做。");
    }

    #[test]
    fn length_mismatch_keeps_originals_as_partial() {
        let base = mock_server(vec![chat_body(&["只有一段", "但输入两段之外多了一段"], "{}")]);
        let cfg = LlmConfig { base_url: base, model: "m".into(), api_key: "k".into() };
        let mut ps = vec![para("原文一")];
        assert!(matches!(polish(&cfg, &mut ps), LlmOutcome::Partial(1)));
        assert_eq!(ps[0].text, "原文一", "长度不符必须保留原文");
    }

    #[test]
    fn connection_refused_is_failed_and_keeps_originals() {
        let cfg = LlmConfig { base_url: "http://127.0.0.1:1".into(), model: "m".into(), api_key: "k".into() };
        let mut ps = vec![para("原文")];
        assert!(matches!(polish(&cfg, &mut ps), LlmOutcome::Failed));
        assert_eq!(ps[0].text, "原文");
    }

    #[test]
    fn chunking_respects_char_budget() {
        let texts: Vec<String> = (0..4).map(|i| format!("{}", "字".repeat(1600 + i))).collect();
        let ps: Vec<_> = texts.iter().map(|t| para(t)).collect();
        let chunks = chunk_indices(&ps);
        assert!(chunks.len() >= 2, "4×1600 字必须切多块");
        for c in &chunks {
            let total: usize = c.iter().map(|&i| ps[i].text.chars().count()).sum();
            assert!(total <= CHUNK_CHARS || c.len() == 1);
        }
    }
}
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cargo test refine::llm -- --nocapture`
Expected: 编译错误(模块不存在)

- [ ] **Step 3: 实现 llm.rs**

```rust
//! A2 LLM 精修:OpenAI 兼容 chat completions,分块+术语表前传,失败块保原文。

use crate::store::RefinedParagraph;
use serde_json::{json, Value};

pub const CHUNK_CHARS: usize = 3000;
pub const REQ_TIMEOUT_S: u64 = 60;

const SYSTEM_PROMPT: &str = "你是会议逐字稿精修助手。对输入的每个段落做四件事,除此之外禁止任何改动:\n1. 纠正同音/近音错字(如「肯计→肯定」),不确定时保留原文,禁止改写句式或语义;\n2. 实体归一:同一人名/产品名/术语全文统一为最常见或术语表给定的写法;\n3. 轻度清理口头语:删除无意义的「嗯」「呃」及紧邻重复(「我们我们→我们」),保留语气词「吧」「啊」等;\n4. 英文与数字排版:英文词组与中文之间加空格,产品名保持原大小写。\n输出 JSON:{\"glossary\":{\"错误写法\":\"统一写法\"},\"texts\":[\"段落1修订文\",\"段落2修订文\"]}。\ntexts 数组长度必须与输入段落数一致,顺序一致。glossary 只收实体类归一项。";

pub struct LlmConfig {
    pub base_url: String,
    pub model: String,
    pub api_key: String,
}

pub enum LlmOutcome {
    Done,
    Partial(usize),
    Failed,
}

/// 按累计字符预算切块,返回每块的段落下标。单段超预算独占一块。
pub(crate) fn chunk_indices(ps: &[RefinedParagraph]) -> Vec<Vec<usize>> {
    let mut out: Vec<Vec<usize>> = Vec::new();
    let mut cur: Vec<usize> = Vec::new();
    let mut budget = 0usize;
    for (i, p) in ps.iter().enumerate() {
        let n = p.text.chars().count();
        if !cur.is_empty() && budget + n > CHUNK_CHARS {
            out.push(std::mem::take(&mut cur));
            budget = 0;
        }
        cur.push(i);
        budget += n;
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

fn call_chunk(cfg: &LlmConfig, glossary: &Value, texts: &[&str]) -> anyhow::Result<(Value, Vec<String>)> {
    let numbered: String = texts.iter().enumerate()
        .map(|(i, t)| format!("{}. {}\n", i + 1, t))
        .collect();
    let user = format!("术语表(沿用并可扩充):{glossary}\n段落:\n{numbered}");
    let url = format!("{}/chat/completions", cfg.base_url.trim_end_matches('/'));
    let resp: Value = ureq::post(&url)
        .timeout(std::time::Duration::from_secs(REQ_TIMEOUT_S))
        .set("authorization", &format!("Bearer {}", cfg.api_key))
        .send_json(json!({
            "model": cfg.model,
            "temperature": 0.1,
            "response_format": { "type": "json_object" },
            "messages": [
                { "role": "system", "content": SYSTEM_PROMPT },
                { "role": "user", "content": user },
            ],
        }))?
        .into_json()?;
    let content = resp["choices"][0]["message"]["content"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("响应缺 choices[0].message.content"))?;
    let parsed: Value = serde_json::from_str(content)?;
    let texts_out: Vec<String> = parsed["texts"]
        .as_array()
        .ok_or_else(|| anyhow::anyhow!("响应缺 texts 数组"))?
        .iter()
        .map(|v| v.as_str().unwrap_or_default().to_string())
        .collect();
    if texts_out.len() != texts.len() {
        anyhow::bail!("texts 长度不符: 期望 {} 实得 {}", texts.len(), texts_out.len());
    }
    Ok((parsed["glossary"].clone(), texts_out))
}

/// 逐块精修,glossary 串行前传。全部成功 Done;部分失败 Partial(n);全部失败 Failed。
pub fn polish(cfg: &LlmConfig, paragraphs: &mut [RefinedParagraph]) -> LlmOutcome {
    let chunks = chunk_indices(paragraphs);
    if chunks.is_empty() {
        return LlmOutcome::Done;
    }
    let mut glossary = json!({});
    let mut failed = 0usize;
    for chunk in &chunks {
        let texts: Vec<&str> = chunk.iter().map(|&i| paragraphs[i].text.as_str()).collect();
        match call_chunk(cfg, &glossary, &texts) {
            Ok((g, outs)) => {
                if let Value::Object(map) = g {
                    if let Value::Object(acc) = &mut glossary {
                        acc.extend(map);
                    }
                }
                for (&i, t) in chunk.iter().zip(outs) {
                    if !t.trim().is_empty() {
                        paragraphs[i].text = t;
                    }
                }
            }
            Err(e) => {
                eprintln!("refine llm: 块失败保留原文: {e}");
                failed += 1;
            }
        }
    }
    match failed {
        0 => LlmOutcome::Done,
        n if n == chunks.len() => LlmOutcome::Failed,
        n => LlmOutcome::Partial(n),
    }
}
```

refine/mod.rs 加 `pub mod llm;`。

- [ ] **Step 4: 跑测试确认通过**

Run: `cargo test refine::llm`
Expected: 4 个测试全绿

- [ ] **Step 5: Commit**

```bash
git add src-tauri/src/refine/
git commit -m "feat(refine): A2 LLM 精修客户端,分块+术语表前传+失败保原文"
```

### Task 9: refine/mod.rs 管线编排器

**Files:**
- Modify: `src-tauri/src/refine/mod.rs`(替换只有 mod 声明的占位,加入编排逻辑)

**Interfaces:**
- Consumes: `filter::discarded_seqs`,`recluster::{recluster, SegInput, MIN_EMBED_MS}`,`llm::{polish, LlmConfig, LlmOutcome}`,`store::{SegmentRecord, SpeakerMeta, RefinedDoc, RefinedParagraph, RefineStages, write_refined_atomic}`,`store::transcode::track_pcm`,`diar::SpeakerEmbedder`,`registry::SeedCluster`。
- Produces:
  - `pub fn run_local(note_dir: &Path, segs: &[SegmentRecord], speakers: &BTreeMap<String, SpeakerMeta>, embedder: Option<&mut dyn SpeakerEmbedder>, seeds: &[SeedCluster], generated_at: &str) -> RefinedDoc`(过滤+重聚类+建段落,llm=off,**内部已 write_refined_atomic**)
  - `pub fn run_llm(note_dir: &Path, doc: &mut RefinedDoc, cfg: &LlmConfig, llm_model: &str) -> anyhow::Result<()>`(polish 后改 stages.llm/llm_model 并重写盘)
  - `pub(crate) fn build_paragraphs(segs: &[SegmentRecord], discarded: &[u64], assign: &[recluster::Assignment], speakers: &BTreeMap<String, SpeakerMeta>) -> Vec<RefinedParagraph>`
  - `pub const MAX_PARA_MS: u64 = 60_000;`(同说话人段落最长 60s,超出另起段,对齐豆包排版粒度)

编排规则:
- `run_local`:① `discarded_seqs`;② 嵌入:对每个非丢弃段,时长 ≥ `MIN_EMBED_MS` 时从 `track_pcm(note_dir, source)`(每 source 惰性取一次)按 `start_ms*16..end_ms*16` 切片(越界 clamp 到 pcm.len())喂 `embedder.embed`,失败记 None;embedder 为 None(声纹模型缺失)或 track_pcm 失败 → stages.recluster="skipped"/"failed",assignments 回退旧标签;③ `recluster`;④ `build_paragraphs`;⑤ 写盘。任何一步 panic 由调用方(Task 10 线程)捕获。
- `build_paragraphs`:按 seq 序跳过 discarded;相邻段「同 assignment.speaker 且累计时长 ≤ MAX_PARA_MS」并段,text 直接拼接(SenseVoice 已带句读),start/end 取组首尾,source_seqs 收全组;name 取 assignment.name,否则旧标签在 speakers.json 里的非空 `name`(重聚类回退路径仍显示用户已改的名字)。
- `run_llm`:`polish` → Done→"done"/Partial→"partial"/Failed→"failed",设 `llm_model`,重写盘。

- [ ] **Step 1: 写失败测试**(mod.rs 尾;嵌入器用测试内闭包实现 trait,PCM 用 hound 造 60s wav)

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::{SegmentRecord, SpeakerMeta};
    use std::collections::BTreeMap;

    /// 前半场返回方向 A、后半场返回方向 B 的假嵌入器(按段起点判断)。
    struct TwoVoice { split_ms: u64, cursor: std::cell::Cell<u64> }
    // 说明:embed 无段位置信息,测试用「调用顺序」近似——所有段按 seq 序喂入。
    struct SeqEmbedder { dirs: Vec<[f32; 3]>, i: usize }
    impl crate::diar::SpeakerEmbedder for SeqEmbedder {
        fn embed(&mut self, _s: &[f32]) -> anyhow::Result<Vec<f32>> {
            let d = self.dirs[self.i.min(self.dirs.len() - 1)];
            self.i += 1;
            Ok(vec![d[0], d[1], d[2]])
        }
    }

    fn seg(seq: u64, source: &str, text: &str, start: u64, end: u64, spk: &str) -> SegmentRecord {
        SegmentRecord { seq, source: source.into(), text: text.into(), start_ms: start, end_ms: end, speaker: Some(spk.into()), rms: Some(0.02) }
    }

    fn write_wav(dir: &std::path::Path, name: &str, secs: u32) {
        let spec = hound::WavSpec { channels: 1, sample_rate: 16000, bits_per_sample: 16, sample_format: hound::SampleFormat::Int };
        let mut w = hound::WavWriter::create(dir.join(name), spec).unwrap();
        for _ in 0..(16000 * secs) { w.write_sample(2000i16).unwrap(); }
        w.finalize().unwrap();
    }

    #[test]
    fn run_local_filters_reclusters_and_builds_paragraphs() {
        let dir = tempfile::tempdir().unwrap();
        write_wav(dir.path(), "mic.wav", 30);
        let segs = vec![
            seg(0, "mic", "大家好,今天讲三点。", 0, 5000, "S1"),
            seg(1, "mic", "男人。", 5000, 5600, "S9"),               // 应被过滤
            seg(2, "mic", "第一点是架构。", 6000, 11_000, "S2"),      // 与 seq0 同人(嵌入同向)
            seg(3, "mic", "我有个问题。", 12_000, 17_000, "S3"),      // 另一人
        ];
        let a = [1.0, 0.0, 0.0]; let b = [0.0, 1.0, 0.0];
        let mut e = SeqEmbedder { dirs: vec![a, a, b], i: 0 };
        let doc = run_local(dir.path(), &segs, &BTreeMap::new(), Some(&mut e), &[], "2026-07-06T15:00:00+08:00");
        assert_eq!(doc.discarded_seqs, vec![1]);
        assert_eq!(doc.stages.filter, "done");
        assert_eq!(doc.stages.recluster, "done");
        assert_eq!(doc.stages.llm, "off");
        assert_eq!(doc.paragraphs.len(), 2, "seq0+seq2 并段,seq3 独立");
        assert_eq!(doc.paragraphs[0].source_seqs, vec![0, 2]);
        assert_ne!(doc.paragraphs[0].speaker, doc.paragraphs[1].speaker);
        assert!(crate::store::load_refined(dir.path()).is_some(), "run_local 已落盘");
    }

    #[test]
    fn run_local_without_embedder_skips_recluster_keeps_old_labels() {
        let dir = tempfile::tempdir().unwrap();
        let mut speakers = BTreeMap::new();
        speakers.insert("S1".into(), SpeakerMeta { name: "老板".into(), sources: vec!["mic".into()], centroid: None, count: 1, person_id: None });
        let segs = vec![seg(0, "mic", "就这样定了。", 0, 4000, "S1")];
        let doc = run_local(dir.path(), &segs, &speakers, None, &[], "t");
        assert_eq!(doc.stages.recluster, "skipped");
        assert_eq!(doc.paragraphs[0].speaker, "S1");
        assert_eq!(doc.paragraphs[0].name.as_deref(), Some("老板"), "旧标签沿用用户改名");
    }

    #[test]
    fn paragraphs_split_at_max_duration() {
        let segs: Vec<SegmentRecord> = (0..5).map(|i| seg(i, "mic", "内容。", i * 20_000, (i + 1) * 20_000, "S1")).collect();
        let assign: Vec<_> = (0..5).map(|i| recluster::Assignment { seq: i, speaker: "R1".into(), name: None }).collect();
        let ps = build_paragraphs(&segs, &[], &assign, &BTreeMap::new());
        assert!(ps.len() >= 2, "100s 同人内容必须按 MAX_PARA_MS 切段");
        assert!(ps.iter().all(|p| p.end_ms - p.start_ms <= MAX_PARA_MS + 20_000));
    }
}
```

(注:`TwoVoice` 结构在最终实现测试里删除,保留 `SeqEmbedder` 即可——此处照抄会有未使用告警,直接不要写 `TwoVoice`。)

- [ ] **Step 2: 跑测试确认失败**

Run: `cargo test refine::tests -- --nocapture`
Expected: 编译错误(函数不存在)

- [ ] **Step 3: 实现 mod.rs**

```rust
//! 会后精修管线编排:过滤(A3)→重聚类(A1)→段落化,可选 LLM 精修(A2)。
//! 原始三文件只读;一切产物写 refined.json。

pub mod filter;
pub mod llm;
pub mod recluster;

use crate::diar::SpeakerEmbedder;
use crate::diar::registry::SeedCluster;
use crate::store::{
    write_refined_atomic, RefineStages, RefinedDoc, RefinedParagraph, SegmentRecord, SpeakerMeta,
};
use std::collections::BTreeMap;
use std::path::Path;

/// 同说话人合并段落的时长上限(对齐豆包排版粒度)。
pub const MAX_PARA_MS: u64 = 60_000;

pub fn run_local(
    note_dir: &Path,
    segs: &[SegmentRecord],
    speakers: &BTreeMap<String, SpeakerMeta>,
    embedder: Option<&mut dyn SpeakerEmbedder>,
    seeds: &[SeedCluster],
    generated_at: &str,
) -> RefinedDoc {
    let discarded = filter::discarded_seqs(segs);
    let kept: Vec<&SegmentRecord> = segs.iter().filter(|s| !discarded.contains(&s.seq)).collect();

    let inputs: Vec<recluster::SegInput> = kept.iter().map(|s| recluster::SegInput {
        seq: s.seq,
        start_ms: s.start_ms,
        end_ms: s.end_ms,
        source: s.source.clone(),
        old_speaker: s.speaker.clone(),
    }).collect();

    let (assign, recluster_state) = match embedder {
        Some(e) => match embed_all(note_dir, &kept, e) {
            Ok(embs) => (recluster::recluster(&inputs, &embs, seeds), "done"),
            Err(err) => {
                eprintln!("refine: 嵌入失败,重聚类降级: {err}");
                (fallback_assign(&inputs), "failed")
            }
        },
        None => (fallback_assign(&inputs), "skipped"),
    };

    let paragraphs = build_paragraphs(segs, &discarded, &assign, speakers);
    let doc = RefinedDoc {
        schema_version: crate::store::refined::REFINED_SCHEMA_VERSION,
        generated_at: generated_at.to_string(),
        llm_model: None,
        stages: RefineStages {
            filter: "done".into(),
            recluster: recluster_state.into(),
            llm: "off".into(),
        },
        discarded_seqs: discarded,
        paragraphs,
    };
    if let Err(e) = write_refined_atomic(note_dir, &doc) {
        eprintln!("refine: refined.json 写盘失败: {e}");
    }
    doc
}

/// 每个 source 惰性取一次全场 PCM,按段切片提嵌入;短段 None。
fn embed_all(
    note_dir: &Path,
    kept: &[&SegmentRecord],
    embedder: &mut dyn SpeakerEmbedder,
) -> anyhow::Result<Vec<Option<Vec<f32>>>> {
    let mut pcm_cache: BTreeMap<String, Vec<f32>> = BTreeMap::new();
    let mut out = Vec::with_capacity(kept.len());
    for s in kept {
        let dur = s.end_ms.saturating_sub(s.start_ms);
        if dur < recluster::MIN_EMBED_MS {
            out.push(None);
            continue;
        }
        if !pcm_cache.contains_key(&s.source) {
            let pcm = crate::store::transcode::track_pcm(note_dir, &s.source)?;
            pcm_cache.insert(s.source.clone(), pcm);
        }
        let pcm = &pcm_cache[&s.source];
        let a = ((s.start_ms as usize) * 16).min(pcm.len());
        let b = ((s.end_ms as usize) * 16).min(pcm.len());
        if b <= a {
            out.push(None);
            continue;
        }
        out.push(embedder.embed(&pcm[a..b]).ok());
    }
    Ok(out)
}

fn fallback_assign(inputs: &[recluster::SegInput]) -> Vec<recluster::Assignment> {
    inputs.iter().map(|i| recluster::Assignment {
        seq: i.seq,
        speaker: i.old_speaker.clone().unwrap_or_else(|| "R1".into()),
        name: None,
    }).collect()
}

pub(crate) fn build_paragraphs(
    segs: &[SegmentRecord],
    discarded: &[u64],
    assign: &[recluster::Assignment],
    speakers: &BTreeMap<String, SpeakerMeta>,
) -> Vec<RefinedParagraph> {
    let by_seq: BTreeMap<u64, &recluster::Assignment> = assign.iter().map(|a| (a.seq, a)).collect();
    let mut out: Vec<RefinedParagraph> = Vec::new();
    for s in segs {
        if discarded.contains(&s.seq) {
            continue;
        }
        let Some(a) = by_seq.get(&s.seq) else { continue };
        let name = a.name.clone().or_else(|| {
            s.speaker.as_ref()
                .and_then(|old| speakers.get(old))
                .filter(|m| !m.name.is_empty())
                .map(|m| m.name.clone())
        });
        let merge = out.last().map_or(false, |p: &RefinedParagraph| {
            p.speaker == a.speaker && s.end_ms.saturating_sub(p.start_ms) <= MAX_PARA_MS
        });
        if merge {
            let p = out.last_mut().unwrap();
            p.text.push_str(&s.text);
            p.end_ms = s.end_ms;
            p.source_seqs.push(s.seq);
        } else {
            out.push(RefinedParagraph {
                speaker: a.speaker.clone(),
                name,
                start_ms: s.start_ms,
                end_ms: s.end_ms,
                text: s.text.clone(),
                source_seqs: vec![s.seq],
            });
        }
    }
    out
}

pub fn run_llm(note_dir: &Path, doc: &mut RefinedDoc, cfg: &llm::LlmConfig, llm_model: &str) -> anyhow::Result<()> {
    let state = match llm::polish(cfg, &mut doc.paragraphs) {
        llm::LlmOutcome::Done => "done",
        llm::LlmOutcome::Partial(_) => "partial",
        llm::LlmOutcome::Failed => "failed",
    };
    doc.stages.llm = state.into();
    doc.llm_model = Some(llm_model.to_string());
    write_refined_atomic(note_dir, doc)
}
```

- [ ] **Step 4: 跑测试确认通过**

Run: `cargo test refine`
Expected: filter/recluster/llm/mod 全绿

- [ ] **Step 5: Commit**

```bash
git add src-tauri/src/refine/
git commit -m "feat(refine): 管线编排 run_local/run_llm,段落化+降级路径"
```

---

### Task 10: lib.rs 接线:停止钩子、refine_note/get_refined 命令、refine 事件

**Files:**
- Modify: `src-tauri/src/ipc.rs`(加 RefineEvent)
- Modify: `src-tauri/src/lib.rs`(AppState 加 refining 集;spawn_refine;改 do_stop_recording lib.rs:854;两个新 command;注册表 lib.rs:1707-1739)

**Interfaces:**
- Consumes: Task 9 `refine::{run_local, run_llm}`、`refine::llm::LlmConfig`;既有 `load_voiceprint_seeds`(lib.rs:168)、`speaker_model_path`(lib.rs:265)、`notes_dir`(lib.rs:128)、`store::NoteStore` 的段/speakers 加载(get_note 同款)、`models::status`。
- Produces:
  - ipc: `RefineEvent { note_id: String, stage: String, state: String }`,事件名 `"refine"`(stage: "filter"|"recluster"|"llm"|"all", state: "running"|"done"|"failed"|"partial"|"skipped"|"off")
  - command `refine_note(id: String) -> Result<(), String>`(录制中该 id / 正在精修 → Err)
  - command `get_refined(id: String) -> Result<Option<store::RefinedDoc>, String>`
  - `fn spawn_refine(app: AppHandle, note_id: String, enqueue_transcode_after_local: bool)`

- [ ] **Step 1: ipc.rs 加事件结构**(照 FinalEvent 风格)

```rust
/// 会后精修进度(emit 名 "refine")。
#[derive(Debug, Clone, Serialize)]
pub struct RefineEvent {
    pub note_id: String,
    pub stage: String,
    pub state: String,
}
```

- [ ] **Step 2: AppState 与 spawn_refine**

AppState(lib.rs:84-88 附近)加:

```rust
    /// 正在精修的 note id 集,防重入;停止钩子与手动重跑共用。
    refining: Arc<Mutex<std::collections::HashSet<String>>>,
```

(初始化处照其它 Arc 字段补 `refining: Arc::new(Mutex::new(HashSet::new()))`。)

lib.rs 新增(放 `abort_or_finalize` 附近):

```rust
/// 会后精修:后台线程跑 filter+recluster(读 WAV)→ 可选移交转码 → 可选 LLM。
/// 全程 catch_unwind,失败只留日志与事件,绝不影响既有数据。
fn spawn_refine(app: tauri::AppHandle, note_id: String, enqueue_transcode_after_local: bool) {
    let state: tauri::State<AppState> = app.state();
    {
        let mut set = state.refining.lock().unwrap();
        if !set.insert(note_id.clone()) {
            return; // 已在精修
        }
    }
    let refining = state.refining.clone();
    let transcode = state.transcode.clone();
    std::thread::spawn(move || {
        let emit = |stage: &str, st: &str| {
            let _ = app.emit("refine", ipc::RefineEvent {
                note_id: note_id.clone(), stage: stage.into(), state: st.into(),
            });
        };
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            emit("all", "running");
            let dir = notes_dir(&app)?.join(&note_id);
            let note = store::NoteStore::load(&dir)?; // 与 get_note 同款只读加载
            let mut embedder = match diar::SherpaEmbedder::new(&speaker_model_path()) {
                Ok(e) => Some(e),
                Err(e) => { eprintln!("refine: 声纹模型不可用,跳过重聚类: {e}"); None }
            };
            let seeds = load_voiceprint_seeds(&app);
            let mut doc = refine::run_local(
                &dir, &note.segments, &note.speakers,
                embedder.as_mut().map(|e| e as &mut dyn diar::SpeakerEmbedder),
                &seeds,
                &chrono::Local::now().to_rfc3339(),
            );
            emit("recluster", &doc.stages.recluster);
            if enqueue_transcode_after_local {
                transcode.enqueue(dir.clone()); // WAV 已读完,尽早压缩
            }
            let s = settings::load(&app.path().app_data_dir()?);
            if s.refine_enabled && !s.refine_base_url.is_empty() && !s.refine_model.is_empty() && !s.refine_api_key.is_empty() {
                emit("llm", "running");
                let cfg = refine::llm::LlmConfig {
                    base_url: s.refine_base_url.clone(),
                    model: s.refine_model.clone(),
                    api_key: s.refine_api_key.clone(),
                };
                if let Err(e) = refine::run_llm(&dir, &mut doc, &cfg, &s.refine_model) {
                    eprintln!("refine: llm 落盘失败: {e}");
                }
            }
            emit("llm", &doc.stages.llm);
            anyhow::Ok(())
        }));
        match result {
            Ok(Ok(())) => emit("all", "done"),
            Ok(Err(e)) => { eprintln!("refine: 管线失败: {e}"); emit("all", "failed"); }
            Err(_) => { eprintln!("refine: 管线 panic"); emit("all", "failed"); }
        }
        refining.lock().unwrap().remove(&note_id);
    });
}
```

(实现时按真实符号对齐:`NoteStore::load` 的实际签名/返回体从 store/notes.rs 现场核对,`note.segments`/`note.speakers` 字段名以 `get_note` command 用法为准;`transcode` 字段若非 Arc 则改为在线程外 clone 其句柄——`TranscodeQueue` 已被 AppState 持有,查其现有跨线程用法照搬。)

- [ ] **Step 3: 停止钩子改造**(lib.rs:854)

原:

```rust
state.transcode.enqueue(note_dir);
```

改:

```rust
// 精修管线接管:本地两段读完 WAV 后由它移交转码(refine 线程内 enqueue)。
spawn_refine(app.clone(), note_id.clone(), true);
```

(`note_id` 在 do_stop_recording 上下文已有——emit status 用到,现场核对变量名。)

- [ ] **Step 4: 两个 command + 注册**

```rust
#[tauri::command]
fn refine_note(app: tauri::AppHandle, state: tauri::State<AppState>, id: String) -> Result<(), String> {
    if *state.running.lock().unwrap() {
        if let Some(s) = state.session.lock().unwrap().as_ref() {
            if s.note_id == id {
                return Err("该笔记正在录制,停止后才能精修".into());
            }
        }
    }
    if state.refining.lock().unwrap().contains(&id) {
        return Err("该笔记正在精修中".into());
    }
    spawn_refine(app.clone(), id, false); // 手动重跑:m4a 已在,无需再排转码
    Ok(())
}

#[tauri::command]
fn get_refined(app: tauri::AppHandle, id: String) -> Result<Option<store::RefinedDoc>, String> {
    let dir = notes_dir(&app).map_err(|e| e.to_string())?.join(&id);
    Ok(store::load_refined(&dir))
}
```

(session 里 note id 字段名现场核对——StatusEvent 的 note_id 来源即是。)注册表 lib.rs:1707-1739 加 `refine_note, get_refined`。

- [ ] **Step 5: 编译 + 全量测试**

Run: `cargo test --lib`
Expected: 全绿。**注意**:既有停止链路测试若断言 stop 后 transcode 立刻入队,会因改为 refine 线程内异步入队而变化——现场跑出失败再按新语义修断言(转码仍必然发生,只是时点后移),不许删测试。

- [ ] **Step 6: Commit**

```bash
git add src-tauri/src/lib.rs src-tauri/src/ipc.rs
git commit -m "feat(refine): 停止钩子接管精修管线,refine_note/get_refined 命令与 refine 事件"
```

---

### Task 11: 前端封装:类型、命令、事件

**Files:**
- Modify: `src/lib/notes.ts`(RefinedDoc 类型 + getRefined/refineNote)
- Modify: `src/lib/models.ts`(Settings 类型加 refine_* 四字段)
- Modify: `src/lib/events.ts`(onRefine)

**Interfaces:**
- Produces(TS,与 Task 7/10 的 serde 输出一一对应):

```ts
// notes.ts
export interface RefinedParagraph {
  speaker: string; name?: string; start_ms: number; end_ms: number;
  text: string; source_seqs: number[];
}
export interface RefineStages { filter: string; recluster: string; llm: string }
export interface RefinedDoc {
  schema_version: number; generated_at: string; llm_model?: string;
  stages: RefineStages; discarded_seqs: number[]; paragraphs: RefinedParagraph[];
}
export const getRefined = (id: string) => invoke<RefinedDoc | null>("get_refined", { id });
export const refineNote = (id: string) => invoke<void>("refine_note", { id });

// models.ts Settings 接口追加
refine_enabled: boolean; refine_base_url: string; refine_model: string; refine_api_key: string;

// events.ts
export interface RefineEvent { note_id: string; stage: string; state: string }
export const onRefine = (cb: (e: RefineEvent) => void) => listen<RefineEvent>("refine", (ev) => cb(ev.payload));
```

- [ ] **Step 1: 照上方接口实现三处修改**(events.ts 照 onFinal 包装风格,notes.ts 照 getNote 风格)

- [ ] **Step 2: 类型检查**

Run: `npm run check`
Expected: 0 errors(引用未到位前不 import 即可)

- [ ] **Step 3: Commit**

```bash
git add src/lib/notes.ts src/lib/models.ts src/lib/events.ts
git commit -m "feat(front): refined 类型/命令/事件封装,Settings 加 refine 字段"
```

---

### Task 12: 设置页「智能精修」区块 + Paraformer 单选

**Files:**
- Modify: `src/routes/settings/+page.svelte`

**Interfaces:**
- Consumes: Task 11 Settings 字段;既有 `saveSetting()`(254-267)、`.toggles`(1022-1058)/`.radios`(975-1016)/`.banner`(1111-1126)/输入框(959-974)样式、`asrChoice`/`changeAsr`(356-376)、`asrArtifactId`(74)。

改动点:

1. **Paraformer 单选**:「语音识别」radios(676-719)追加第三项,照抄 whisper 结构:

```svelte
<label class="radio" class:disabled={recording.isLive}>
  <input type="radio" name="asr" value="paraformer"
    bind:group={asrChoice}
    disabled={recording.isLive || !settings}
    onchange={() => changeAsr("paraformer")} />
  <span class="radio-body">
    <span class="radio-title">Paraformer</span>
    <span class="radio-desc">中文准确率更高,英文较弱,约 230MB。保留段内说话人分离;语言过滤按文本兜底。切换后下一场录制生效。</span>
  </span>
</label>
```

`asrArtifactId`(settings:74)改三态:

```ts
const asrArtifactId = $derived(
  settings?.asr_model === "whisper" ? "whisper"
  : settings?.asr_model === "paraformer" ? "paraformer"
  : "asr",
);
```

(模型区块 `status.artifacts` 由后端 manifest 驱动,Task 2 完成后 paraformer 行自动出现,无前端改动。)

2. **「智能精修」新区块**,插在「语音识别」section 之后。本地状态与预设:

```ts
let refineOn = $state(false);
let refineBaseUrl = $state("");
let refineModel = $state("");
let refineKey = $state("");
// settings 加载处(既有 onMount 里 getSettings 后)同步:
//   refineOn = s.refine_enabled; refineBaseUrl = s.refine_base_url; ...
const REFINE_PRESETS = [
  { label: "DeepSeek", base: "https://api.deepseek.com/v1", model: "deepseek-chat" },
  { label: "通义千问", base: "https://dashscope.aliyuncs.com/compatible-mode/v1", model: "qwen-plus" },
  { label: "豆包", base: "https://ark.cn-beijing.volces.com/api/v3", model: "doubao-seed-1-6-250615" },
  { label: "Kimi", base: "https://api.moonshot.cn/v1", model: "moonshot-v1-auto" },
  { label: "OpenAI", base: "https://api.openai.com/v1", model: "gpt-4o-mini" },
];
function applyPreset(p: { base: string; model: string }) {
  refineBaseUrl = p.base;
  refineModel = p.model;
  saveRefine();
}
function saveRefine() {
  saveSetting((s) => {
    s.refine_enabled = refineOn;
    s.refine_base_url = refineBaseUrl.trim();
    s.refine_model = refineModel.trim();
    s.refine_api_key = refineKey.trim();
  });
}
```

标记(toggle 照 456-496 结构,输入照 shortcut-input 类):

```svelte
<section>
  <h2 class="section-title">智能精修</h2>
  <div class="toggles">
    <label class="toggle">
      <input type="checkbox" bind:checked={refineOn} onchange={saveRefine} />
      <span class="toggle-body">
        <span class="toggle-title">会后 LLM 精修</span>
        <span class="toggle-desc">录制结束后用大模型纠错字、统一术语、清理口头语并合并段落。会议文本将发送至下方所选服务商;key 明文存于本机设置文件。</span>
      </span>
    </label>
  </div>
  {#if refineOn}
    <div class="preset-row">
      {#each REFINE_PRESETS as p}
        <button class="btn-secondary" onclick={() => applyPreset(p)}>{p.label}</button>
      {/each}
    </div>
    <div class="refine-fields">
      <label class="field"><span>接口地址(含版本段)</span>
        <input class="shortcut-input" placeholder="https://api.deepseek.com/v1" bind:value={refineBaseUrl} onblur={saveRefine} /></label>
      <label class="field"><span>模型</span>
        <input class="shortcut-input" placeholder="deepseek-chat" bind:value={refineModel} onblur={saveRefine} /></label>
      <label class="field"><span>API Key</span>
        <input class="shortcut-input" type="password" placeholder="sk-..." bind:value={refineKey} onblur={saveRefine} /></label>
    </div>
    {#if refineOn && (!refineBaseUrl || !refineModel || !refineKey)}
      <div class="banner warn">接口地址、模型、API Key 三项配齐后精修才会生效。</div>
    {/if}
  {/if}
</section>
```

新样式(照既有 hairline/token 规范,追加到 `<style>`):

```css
.preset-row { display: flex; gap: 0.5rem; flex-wrap: wrap; margin-top: 0.8rem; }
.refine-fields { display: flex; flex-direction: column; gap: 0.6rem; margin-top: 0.8rem; }
.field { display: flex; flex-direction: column; gap: 0.3rem; }
.field > span { font-size: 0.78rem; color: var(--ink-secondary); }
```

- [ ] **Step 1: 实现上述两处改动**
- [ ] **Step 2: 类型检查 + 手动冒烟**

Run: `npm run check`
Expected: 0 errors
Run: `npm run tauri dev`,设置页确认:三选 ASR 可切且模型区出现 Paraformer 行可下载;精修开关开后显三字段,预设一键填充,改动落 settings.json(`cat ~/Library/Application\ Support/com.teemo.voice-notes/settings.json` 验证)。

- [ ] **Step 3: Commit**

```bash
git add src/routes/settings/+page.svelte
git commit -m "feat(settings-ui): 智能精修区块(预设/开关/三字段) + Paraformer 选型"
```

---

### Task 13: 笔记详情页:精修稿视图

**Files:**
- Modify: `src/routes/notes/[id]/+page.svelte`

**Interfaces:**
- Consumes: Task 11 `getRefined/refineNote/onRefine/RefinedDoc`;既有 `speakerColor/speakerInk/speakerLabel`、`formatTs`(notes.ts:127)、`playFrom`、`refresh()`(68-75)。

行为:

- 加载:`refresh()` 里并行 `refined = await getRefined(id)`;`onRefine` 监听(note_id 匹配当前页时:state=running 置 `refining=true`,done/failed 后重新 `getRefined` 并复位)。监听在 `$effect` 按 id 注册,页面卸载解绑(listen 返回 unlisten,照 AudioPlayer 内既有清理模式;若项目惯例不解绑则跟随惯例)。
- 视图切换:`viewMode = $state<"refined" | "raw">("refined")`;`refined && meta.state === "complete"` 时默认精修稿,否则强制 raw。头部两枚分段按钮「精修稿 / 原始逐字稿」(btn-link 样式,当前态高亮)。
- 精修稿渲染:每段 `RefinedParagraph` 一块——speaker 徽章(复用 badge 样式,标签优先 `p.name`,否则 `p.speaker`)、`formatTs(p.start_ms)`(可点 `playFrom` 等价逻辑:构造 `{start_ms: p.start_ms, source: "mic"}` 传入)、`p.text` 只读(不进 contenteditable——精修稿不做行内编辑,真值在原始稿)。
- 状态条:`refined.stages.llm === "partial"` → warn banner「部分段落精修失败,已保留原文。可重试。」;`"failed"` → danger banner;`refining` → 「正在精修…」。
- 「重新精修」按钮(btn-secondary,视图切换旁):`await refineNote(id)`,Err 弹 banner。
- raw 视图:`refined?.discarded_seqs` 命中的段加 `.discarded` class(`opacity: 0.38;`),标题气泡「已被精修过滤」。

- [ ] **Step 1: 实现**(标记骨架示意:)

```svelte
<div class="view-switch">
  <button class="btn-link" class:active={viewMode === "refined"} disabled={!refined}
    onclick={() => (viewMode = "refined")}>精修稿</button>
  <button class="btn-link" class:active={viewMode === "raw"}
    onclick={() => (viewMode = "raw")}>原始逐字稿</button>
  <button class="btn-secondary" disabled={refining || note?.meta.state !== "complete"}
    onclick={rerunRefine}>{refining ? "正在精修…" : "重新精修"}</button>
</div>

{#if viewMode === "refined" && refined}
  {#if refined.stages.llm === "partial"}<div class="banner warn">部分段落精修失败,已保留原文,可重新精修。</div>{/if}
  {#if refined.stages.llm === "failed"}<div class="banner danger">LLM 精修失败,当前为本地精修结果。</div>{/if}
  {#each refined.paragraphs as p}
    <div class="para">
      <span class="badge" style="background: {speakerColor(p.speaker, 'mic')}; color: {speakerInk(p.speaker, 'mic')}">{p.name ?? p.speaker}</span>
      <button class="ts ts-btn" onclick={() => playFromMs(p.start_ms)}>{formatTs(p.start_ms)}</button>
      <p class="para-text">{p.text}</p>
    </div>
  {/each}
{:else}
  <!-- 既有 seg 列表,加 class:discarded={refined?.discarded_seqs.includes(seg.seq)} -->
{/if}
```

- [ ] **Step 2: 类型检查 + 手动冒烟**

Run: `npm run check` → 0 errors。
Run: `npm run tauri dev`,打开旧笔记(无 refined)→ 默认原始稿、切换钮禁用;点「重新精修」→ 事件推进 → 自动出精修稿;discarded 段在原始稿灰显。

- [ ] **Step 3: Commit**

```bash
git add src/routes/notes/[id]/+page.svelte
git commit -m "feat(note-ui): 精修稿视图/原始稿切换,重新精修与过滤段灰显"
```

---

### Task 14: golden 校准与回归

**Files:**
- Create: `scripts/refine_golden.py`(python3 标准库,无第三方依赖)
- Modify: `src-tauri/src/refine/recluster.rs` / `filter.rs`(仅当校准结果要求调常量)

**背景与数据(不入库,本机路径):** 一场真实会话录音(具体笔记 id 与第三方对照纪要文件均不入库,见本机 `~/Documents/voice-notes/notes/` 与 `samples/`,时间轴偏移 0)。真实值:7 人;垃圾段 seq {1,2,21,26,27,63,233,246,319,333,414,446};保护段 seq {394,399}。

- [ ] **Step 1: 写 scripts/refine_golden.py**

功能(读 refined.json 与 segments.jsonl,对照豆包 md):

```python
#!/usr/bin/env python3
"""精修 golden 回归:对真实会议样本检验聚类数/纯度/过滤命中。
用法: python3 scripts/refine_golden.py <note_dir> <doubao_md>
数据不入库;本脚本只依赖标准库。"""
import json, re, sys
from collections import defaultdict, Counter

EXPECT_JUNK = {1, 2, 21, 26, 27, 63, 233, 246, 319, 333, 414, 446}
EXPECT_KEEP = {394, 399}
MAX_SPEAKERS = 12          # 真实 7 人,留余量
MIN_TOP2_PURITY = 0.80     # 最大两簇对豆包说话人的纯度下限

def parse_doubao(path):
    entries, cur = [], None
    for line in open(path, encoding="utf-8"):
        line = line.strip()
        m = re.match(r"^@(说话人\s*\d+)\s+(\d+):(\d+)(?::(\d+))?$", line)
        if m:
            sec = (int(m.group(2)) * 3600 + int(m.group(3)) * 60 + int(m.group(4))) if m.group(4) \
                  else (int(m.group(2)) * 60 + int(m.group(3)))
            cur = [m.group(1), sec, ""]
            entries.append(cur)
        elif cur and line and not line.startswith(("#", "录音时间")):
            cur[2] += line
    for i, e in enumerate(entries):
        e.append(entries[i + 1][1] if i + 1 < len(entries) else e[1] + 60)
    return entries

def main(note_dir, doubao_md):
    refined = json.load(open(f"{note_dir}/refined.json", encoding="utf-8"))
    segs = {s["seq"]: s for s in map(json.loads, open(f"{note_dir}/segments.jsonl", encoding="utf-8"))}
    fails = []
    # 1. 过滤命中/误杀
    got = set(refined["discarded_seqs"])
    missed = EXPECT_JUNK - got
    killed = EXPECT_KEEP & got
    if missed: fails.append(f"漏杀垃圾段: {sorted(missed)}")
    if killed: fails.append(f"误杀真实段: {sorted(killed)}")
    # 2. 聚类数
    labels = {p["speaker"] for p in refined["paragraphs"]}
    print(f"聚类标签数: {len(labels)} (原始 45, 真实 7)")
    if len(labels) > MAX_SPEAKERS: fails.append(f"标签数 {len(labels)} > {MAX_SPEAKERS}")
    # 3. 纯度:每段落经 source_seqs 摊回时间区间,对豆包重叠
    entries = parse_doubao(doubao_md)
    overlap = defaultdict(Counter)
    for p in refined["paragraphs"]:
        for q in p["source_seqs"]:
            s = segs[q]
            a, b = s["start_ms"] / 1000, s["end_ms"] / 1000
            for sp, st, _, en in entries:
                ov = min(b, en) - max(a, st)
                if ov > 0: overlap[p["speaker"]][sp] += ov
    for lab, c in sorted(overlap.items(), key=lambda kv: -sum(kv[1].values()))[:2]:
        total = sum(c.values())
        top = c.most_common(1)[0]
        purity = top[1] / total
        print(f"{lab}: {total:.0f}s 主对应 {top[0]} 纯度 {purity:.2f}")
        if purity < MIN_TOP2_PURITY: fails.append(f"{lab} 纯度 {purity:.2f} < {MIN_TOP2_PURITY}")
    if fails:
        print("FAIL"); [print(" -", f) for f in fails]; sys.exit(1)
    print("PASS")

if __name__ == "__main__":
    main(sys.argv[1], sys.argv[2])
```

- [ ] **Step 2: 生成 refined 并跑 golden**

Run(依次):
1. `npm run tauri dev` 起应用,对 golden 会议点「重新精修」(LLM 关,先校本地两段);或后续加 dev 命令直跑。
2. `python3 scripts/refine_golden.py ~/Documents/voice-notes/notes/<笔记id> "samples/<第三方对照纪要>.md"`（具体文件本机选取，不入库）
Expected: `PASS`,聚类标签数 ≤ 12,Top2 纯度 ≥ 0.80

- [ ] **Step 3: 不达标则调参重跑**

顺序:`AHC_THRESHOLD`(0.55-0.68 步进 0.02,标签过多调低/污染调高)→ `MIN_CLUSTER_MS`(8000→12000)→ filter 白名单补漏。每轮改常量 → 重新精修 → 重跑脚本。记录最终值于 commit message。

- [ ] **Step 4: Commit**

```bash
git add scripts/refine_golden.py src-tauri/src/refine/
git commit -m "feat(refine): golden 回归脚本与真实会议校准(最终阈值见 diff)"
```

---

### Task 15: Paraformer 真机冒烟

**Files:** 无新增(验证性任务;发现问题按需修 Task 3 产物)

- [ ] **Step 1: 安装工件到 dev models 根**

```bash
mkdir -p src-tauri/models
cp -r /private/tmp/claude-501/-Users-teemo-workspace-soul-voice-notes/912ca21f-f7c4-4a6f-ba4f-42934235e482/scratchpad/sherpa-onnx-paraformer-zh-2023-09-14 src-tauri/models/
```

(若 scratchpad 已清理:设置页切 Paraformer → 模型区块「下载」走正式链路,校验 manifest 哈希即 Task 2 实测值。)

- [ ] **Step 2: ignored 测试真机跑**

Run: `cargo test --lib asr::paraformer -- --ignored --nocapture`
Expected: `transcribes_nonempty_with_timestamps` PASS(tokens 与 timestamps 等长)

- [ ] **Step 3: 应用内录一段中文验证**

`npm run tauri dev` → 设置切 Paraformer → 录 30s 中文 → 停止:出稿、说话人正常、无 panic;设置切回 SenseVoice 再录一段确认工厂来回切换无残留。

- [ ] **Step 4: Commit**(若有修复)

```bash
git add -u && git commit -m "fix(asr): paraformer 真机冒烟修复"
```

---

### Task 16: 收尾验证与合入准备

- [ ] **Step 1: 全量回归**

Run: `cargo test --lib`(src-tauri/)与 `npm run check`
Expected: 全绿 / 0 errors

- [ ] **Step 2: 端到端手动清单**(逐项确认)

1. SenseVoice 录 1 分钟多人对话 → 停止 → refined.json 生成,精修稿默认展示,段落合并正确;
2. 配 DeepSeek key 开精修 → 「重新精修」→ 错字/口头语可见改善,partial/failed 路径给 banner;
3. 断网重跑 → llm=failed,本地两段仍完好;
4. 关精修开关 → 重跑 → stages.llm="off";
5. golden 会议脚本 PASS(Task 14 结果复核);
6. 旧笔记(无 refined)打开不报错,默认原始稿。

- [ ] **Step 3: 合入**

用 superpowers:finishing-a-development-branch 技能走分支收尾(worktree-asr-tuning → PR → master,PR 描述带 golden 前后对比数据:45 标签→实测值、垃圾段 12/12 命中、0 误杀)。

---

## Self-Review 记录

- **Spec 覆盖**:A3→Task 4;A1→Task 5/6/9;A2→Task 8/9/10/12;B→Task 2/3/15;refined 数据模型→Task 7;UI→Task 12/13;golden 验证→Task 14;边界(续录/录制中拒绝/失败降级)→Task 10 守卫与 stages。设计文档六节全部有对应任务。
- **占位符扫描**:Task 2 工件值已实测钉死;Task 10 标注两处「现场核对变量名」属于符号对齐(NoteStore 返回体/字段名),非逻辑 TBD——实施者按 get_note 现有用法照搬即可。
- **类型一致性**:`RefinedParagraph/RefineStages/RefinedDoc` 在 Task 7(Rust)/Task 11(TS)字段一一对应;`LlmConfig{base_url,model,api_key}` Task 8 定义、Task 10/12 消费一致;`Assignment{seq,speaker,name}` Task 6 定义、Task 9 消费一致;base_url 含版本段约定在 Task 8/12 一致(`/chat/completions` 拼接)。
