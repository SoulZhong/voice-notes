# 说话人上下文推断 P1(地基)Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 落地 spec P1 三件事:① 精修 LLM(HTTP+Agent 双路径)能看到说话人标签;② 人工指认说话人后把该人声纹回灌质心(纠错回灌);③ 建立「簇→真人」评测工具链,为 P2b 自动应用开闸提供数据门。

**Architecture:** 全部在既有链路上做增量:HTTP 路径在 `polish` 的分块拼接中带上 speaker 标签并更新 SYSTEM_PROMPT 契约;Agent 路径在 `refine_command` 指令模板中要求利用已可见的 speaker 字段。回灌新建 `feedback` 模块(纯逻辑核 + 磁盘壳),复用 `track_pcm` 解码与 `upsert_from_session` 质心并入,挂在两个指认 IPC 命令成功之后异步执行、best-effort。评测是独立 bin,直接读 JSON 文件零耦合。

**Tech Stack:** Rust(Tauri 2 后端),sherpa-onnx CAM++ 嵌入,serde_json;测试用 MockEmbedder + tempfile + hound。

**Spec:** `docs/superpowers/specs/2026-08-08-speaker-context-inference-design.md`(rev2)

## Global Constraints

- 录制与指认主链路绝不因新功能失败而失败:回灌是 best-effort,出错只 `eprintln!`,不向用户报错(spec「错误处理」节)。
- 回灌受模型门禁约束:`voiceprints.embedding_model` 与 `settings.speaker_model` 不一致时跳过回灌(spec:「模型门禁」;现有语义参照 `src-tauri/src/lib.rs:219-238` 种子注入门禁)。
- 单段 <1.5s 不进回灌(与 `registry.rs` `MIN_CENTROID_UPDATE_SAMPLES=24_000` 即 1.5s 同口径)。
- 新增 serde 字段一律 `#[serde(default)]`;本计划实际不改任何落盘 schema。
- 注释风格跟随仓库:中文、讲"为什么"。
- 每个任务收尾:`cd src-tauri && cargo test <本任务测试> && cargo fmt`,然后提交。
- 工作目录是仓库根;所有 cargo 命令在 `src-tauri/` 下执行。

---

### Task 1: HTTP 精修 prompt 携带说话人标签

**Files:**
- Modify: `src-tauri/src/refine/llm.rs`(`SYSTEM_PROMPT` :12、`format_chunk_paragraphs` :249、`call_chunk` :264-271、`polish` :487-506、测试 :950)

**Interfaces:**
- Consumes: `RefinedParagraph { speaker: String, name: Option<String>, text: String, .. }`(`store/refined.rs`)
- Produces: `format_chunk_paragraphs(&[(usize, &str, &str)]) -> String`,新元组含义 `(绝对段落下标, 说话人标签, 正文)`。`polish` 公开签名不变。

- [ ] **Step 1: 写失败测试**

在 `src-tauri/src/refine/llm.rs` 的 `mod tests` 中,把现有 `chunk_prompt_keeps_absolute_paragraph_indexes`(:950)改为新格式断言,并新增标签兜底测试:

```rust
#[test]
fn chunk_prompt_keeps_absolute_paragraph_indexes() {
    assert_eq!(
        format_chunk_paragraphs(&[(7, "张伟", "第八段"), (12, "R2", "第十三段")]),
        "paragraph_index=7 speaker=张伟: 第八段\nparagraph_index=12 speaker=R2: 第十三段\n"
    );
}

/// 无 name 的段用 R 号兜底;polish 侧的标签取值逻辑(name 优先)在
/// polish_prompt_carries_speaker_labels 里连同分块一起验证。
#[test]
fn chunk_prompt_speaker_label_falls_back_to_cluster_id() {
    assert_eq!(
        format_chunk_paragraphs(&[(0, "R1", "你好")]),
        "paragraph_index=0 speaker=R1: 你好\n"
    );
}
```

- [ ] **Step 2: 跑测试确认编译失败**

Run: `cd src-tauri && cargo test --lib chunk_prompt -- --nocapture`
Expected: 编译错误 `format_chunk_paragraphs` 元组元数不符(现签名是 `&[(usize, &str)]`)。

- [ ] **Step 3: 改实现**

`format_chunk_paragraphs`(:249)改为:

```rust
fn format_chunk_paragraphs(paragraphs: &[(usize, &str, &str)]) -> String {
    paragraphs
        .iter()
        .map(|(absolute_index, speaker, text)| {
            format!("paragraph_index={absolute_index} speaker={speaker}: {text}\n")
        })
        .collect()
}
```

`call_chunk`(:264)参数 `paragraphs: &[(usize, &str)]` 同步改为 `&[(usize, &str, &str)]`(`paragraphs.len()` 用法不变)。

`polish`(:502-506)构造 inputs 处改为:

```rust
let inputs: Vec<(usize, &str, &str)> = chunk
    .iter()
    .map(|&i| {
        let p = &paragraphs[i];
        // 标签取值:人名优先(已关联/已命名),否则退回 R 号——LLM 至少能
        // 分辨"同一人/不同人",这正是指代消解需要的最小信息。
        (i, p.name.as_deref().filter(|n| !n.is_empty()).unwrap_or(&p.speaker), p.text.as_str())
    })
    .collect();
```

`SYSTEM_PROMPT`(:12)在「输出 JSON」句之前插入一句(保持单行字符串风格):

```text
每段前的 speaker= 标注是该段说话人(人名或簇号),仅供理解上下文——指代消解、称呼与人名错字判断(如称呼「小王」后由 speaker=王某 的段应答);texts 只输出修订后的正文,禁止把 speaker 标注或说话人名前缀写进 texts。
```

- [ ] **Step 4: 补一条 polish 级联测试**

在 `mod tests` 新增(mock_server 模式仿照现有 `polish_logs_request_and_response_per_chunk` :744,断言请求体里带了标签):

```rust
#[test]
fn polish_prompt_carries_speaker_labels() {
    // 两段:一段有 name,一段只有 R 号。mock 返回原文,重点断言请求体。
    let body = serde_json::json!({
        "choices": [{ "message": { "content":
            "{\"glossary\":{},\"texts\":[\"甲\",\"乙\"],\"entities\":[],\"relations\":[]}" } }]
    })
    .to_string();
    let (addr, captured) = mock_server_capturing(vec![body]); // 见 Step 4 说明
    let cfg = LlmConfig { base_url: format!("http://{addr}"), model: "m".into(), api_key: "k".into() };
    let mut ps = vec![
        para_with("R1", Some("张伟"), "甲"),
        para_with("R2", None, "乙"),
    ];
    let (outcome, _, _) = polish(&cfg, &mut ps, None);
    assert!(matches!(outcome, LlmOutcome::Done | LlmOutcome::DoneWithRelationErrors));
    let req = captured.lock().unwrap().join("");
    assert!(req.contains("speaker=张伟"), "有名字的段必须带人名标签");
    assert!(req.contains("speaker=R2"), "无名字的段用 R 号兜底");
}
```

说明:现有 `mock_server`(:556)不回传请求体;仿照它加一个 `mock_server_capturing`,把 `read_request` 读到的字节存进 `Arc<Mutex<Vec<String>>>` 一并返回。`para_with(speaker, name, text)` 是测试内小工厂,构造字段齐全的 `RefinedParagraph`(其余字段填默认值,参照本文件其它测试的构造方式)。

- [ ] **Step 5: 跑测试确认通过**

Run: `cd src-tauri && cargo test --lib llm:: -- --nocapture`
Expected: 全 PASS(含既有 polish/chunk 测试——它们不断言旧前缀格式的,不受影响;若有断言旧格式的一并更新)。

- [ ] **Step 6: 提交**

```bash
git add src-tauri/src/refine/llm.rs
git commit -m "feat(refine): HTTP 精修分块携带说话人标签,更新 SYSTEM_PROMPT 契约"
```

---

### Task 2: Agent 精修指令要求利用说话人字段

**Files:**
- Modify: `src-tauri/src/refine/agent.rs`(`refine_command` :243 内的用户指令模板,及其测试)

**Interfaces:**
- Consumes: Agent 经 MCP `get_aing_context` 已能读到每段 `speaker`/`name`(`mcp/tools.rs:143`),本任务只改指令文本,不改工具面。
- Produces: 无新接口。

- [ ] **Step 1: 写失败测试**

在 `agent.rs` `mod tests` 新增(仿照 `refine_command_claude_has_strict_mcp_and_allowlist` :1165 取到命令 prompt 文本的方式):

```rust
#[test]
fn refine_command_instructs_speaker_usage() {
    // 取任一 AgentKind 构造的 refine 指令文本,断言说话人使用条款存在。
    let prompt = refine_prompt_for_test(); // 与现有测试同一取文本途径
    assert!(prompt.contains("speaker/name 字段"), "必须告知 Agent 段落带说话人字段");
    assert!(prompt.contains("禁止修改 speaker"), "必须禁止 Agent 改动说话人归属");
}
```

(`refine_prompt_for_test` 指代现有测试拿 prompt 字符串的同一路径——`refine_command` 返回值或其内部 format 的产物;按 :1165 测试的现成写法取,不新造机制。)

- [ ] **Step 2: 跑测试确认失败**

Run: `cd src-tauri && cargo test --lib refine_command_instructs -- --nocapture`
Expected: FAIL(断言不含该文本)。

- [ ] **Step 3: 改指令模板**

在 `refine_command`(:243)的用户指令 format 中,紧跟「取返回的 paragraphs 数组(段落下标从 0 计;若返回 refined=false 说明还没有精修稿,直接结束并说明)」(:194 附近)之后插入:

```text
每个段落带 speaker/name 字段(该段说话人的簇号与人名);精修正文时利用它做指代消解、称呼一致性与人名错字判断(如称呼「小王」后由王某的段应答)。禁止修改 speaker/name/person_id 等归属字段,禁止把说话人名当前缀写进正文。
```

- [ ] **Step 4: 跑测试确认通过**

Run: `cd src-tauri && cargo test --lib agent:: -- --nocapture`
Expected: 全 PASS(既有 refine_command 系测试不断言这段新文本,不受影响)。

- [ ] **Step 5: 提交**

```bash
git add src-tauri/src/refine/agent.rs
git commit -m "feat(refine): Agent 精修指令明确要求利用说话人字段并禁改归属"
```

---

### Task 3: feedback 模块纯逻辑核(段→嵌入→信道快照)

**Files:**
- Create: `src-tauri/src/feedback.rs`
- Modify: `src-tauri/src/lib.rs`(加 `mod feedback;`,与现有 `mod diar;` 等并排)

**Interfaces:**
- Consumes: `SegmentRecord`(`store/mod.rs:63-74`,含 `seq/source/start_ms/end_ms/speaker`)、`diar::SpeakerEmbedder`(`diar/mod.rs:8`)、`diar::registry::ClusterSnapshot`(`registry.rs:70-79`)。
- Produces:
  - `feedback::MIN_SEG_MS: u64 = 1_500`
  - `feedback::MAX_SEGS: usize = 50`
  - `feedback::build_snapshots(segs: &[&SegmentRecord], pcm_by_source: &BTreeMap<String, (u64, Vec<f32>)>, person_id: &str, embedder: &mut dyn SpeakerEmbedder) -> Vec<ClusterSnapshot>`(Task 4/5 依赖)

- [ ] **Step 1: 写失败测试**

`src-tauri/src/feedback.rs` 先建骨架 + 测试(TDD:测试先行,函数体 `todo!()` 也可,先让断言表达契约):

```rust
//! 纠错回灌:人工指认「这个说话人是库里的谁」之后,把该说话人在本笔记的
//! 发声段重新嵌入并并入那个人的质心——让整理动作变成训练信号
//! (spec rev2 P1-2;文献称轻量纠错回灌相对 DER -32%)。
//! 本文件是纯逻辑核:不碰磁盘、不碰库,输入段+PCM,输出信道快照。

use std::collections::{BTreeMap, BTreeSet};

use crate::diar::registry::ClusterSnapshot;
use crate::diar::SpeakerEmbedder;
use crate::store::SegmentRecord;

/// 单段最短 1.5s:与 registry::MIN_CENTROID_UPDATE_SAMPLES(24_000 采样)同口径,
/// 更短的段嵌入不稳定,进质心是污染不是信号。
pub const MIN_SEG_MS: u64 = 1_500;
/// 单次回灌最多嵌入的段数(按时长降序取):指认是同步用户动作触发的后台任务,
/// 超长会议不该让它嵌几百段——最长的 50 段已足够代表这个人。
pub const MAX_SEGS: usize = 50;

pub fn build_snapshots(
    segs: &[&SegmentRecord],
    pcm_by_source: &BTreeMap<String, (u64, Vec<f32>)>,
    person_id: &str,
    embedder: &mut dyn SpeakerEmbedder,
) -> Vec<ClusterSnapshot> {
    todo!()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::diar::MockEmbedder;

    fn seg(seq: u64, source: &str, start_ms: u64, end_ms: u64) -> SegmentRecord {
        SegmentRecord {
            seq,
            source: source.into(),
            text: String::new(),
            start_ms,
            end_ms,
            speaker: Some("S1".into()),
            rms: 0.0,
        }
    }

    /// 16kHz:1ms = 16 采样。造 pcm 长度按 (end_ms-offset)*16 覆盖到位即可。
    fn pcm_ms(ms: u64) -> Vec<f32> {
        vec![0.1; (ms * 16) as usize]
    }

    fn unit(i: usize) -> Vec<f32> {
        let mut v = vec![0.0; 4];
        v[i] = 1.0;
        v
    }

    #[test]
    fn short_segments_are_skipped() {
        let s1 = seg(0, "mic", 0, 800); // <1.5s,不进回灌
        let s2 = seg(1, "mic", 1000, 3000); // 2s,进
        let mut pcm = BTreeMap::new();
        pcm.insert("mic".to_string(), (0u64, pcm_ms(4000)));
        let mut emb = MockEmbedder::new(vec![Ok(unit(0))]);
        let snaps = build_snapshots(&[&s1, &s2], &pcm, "P3", &mut emb);
        assert_eq!(snaps.len(), 1);
        assert_eq!(snaps[0].count, 1, "只有 2s 的段被嵌入");
        assert_eq!(snaps[0].total_ms, 2000);
        assert_eq!(snaps[0].person.as_deref(), Some("P3"));
        assert_eq!(snaps[0].sources, BTreeSet::from(["mic".to_string()]));
    }

    #[test]
    fn snapshots_split_by_source_and_centroid_is_unit_mean() {
        let m = seg(0, "mic", 0, 2000);
        let s = seg(1, "system", 0, 2000);
        let mut pcm = BTreeMap::new();
        pcm.insert("mic".to_string(), (0u64, pcm_ms(2000)));
        pcm.insert("system".to_string(), (0u64, pcm_ms(2000)));
        let mut emb = MockEmbedder::new(vec![Ok(unit(0)), Ok(unit(1))]);
        let snaps = build_snapshots(&[&m, &s], &pcm, "P1", &mut emb);
        assert_eq!(snaps.len(), 2, "mic/system 各一条快照");
        for sn in &snaps {
            let norm: f32 = sn.centroid.iter().map(|x| x * x).sum::<f32>().sqrt();
            assert!((norm - 1.0).abs() < 1e-5, "质心必须归一化(库内一律单位向量)");
        }
    }

    #[test]
    fn missing_track_pcm_and_embed_errors_degrade_to_skip() {
        // system 轨 PCM 缺失(单轨笔记)+ mic 段嵌入报错:全部静默跳过,零快照。
        let m = seg(0, "mic", 0, 2000);
        let s = seg(1, "system", 0, 2000);
        let mut pcm = BTreeMap::new();
        pcm.insert("mic".to_string(), (0u64, pcm_ms(2000)));
        let mut emb = MockEmbedder::new(vec![Err(anyhow::anyhow!("段过短"))]);
        let snaps = build_snapshots(&[&m, &s], &pcm, "P1", &mut emb);
        assert!(snaps.is_empty());
    }

    #[test]
    fn offset_is_respected_when_slicing() {
        // 轨 offset 1000ms:段 [1000,3000)ms 应切 PCM [0,32000) 采样;
        // PCM 只有 2000ms 长,切片越界说明没减 offset。
        let m = seg(0, "mic", 1000, 3000);
        let mut pcm = BTreeMap::new();
        pcm.insert("mic".to_string(), (1000u64, pcm_ms(2000)));
        let mut emb = MockEmbedder::new(vec![Ok(unit(0))]);
        let snaps = build_snapshots(&[&m], &pcm, "P1", &mut emb);
        assert_eq!(snaps.len(), 1);
        assert_eq!(snaps[0].total_ms, 2000);
    }
}
```

同时在 `lib.rs` 模块声明区加 `mod feedback;`。

- [ ] **Step 2: 跑测试确认失败**

Run: `cd src-tauri && cargo test --lib feedback:: -- --nocapture`
Expected: 4 个测试全部 panic 于 `todo!()`。

- [ ] **Step 3: 写实现**

替换 `build_snapshots` 的 `todo!()`:

```rust
pub fn build_snapshots(
    segs: &[&SegmentRecord],
    pcm_by_source: &BTreeMap<String, (u64, Vec<f32>)>,
    person_id: &str,
    embedder: &mut dyn SpeakerEmbedder,
) -> Vec<ClusterSnapshot> {
    // 时长降序取前 MAX_SEGS:长段嵌入最稳,也给计算量封顶。
    let mut picked: Vec<&SegmentRecord> = segs
        .iter()
        .copied()
        .filter(|s| s.end_ms.saturating_sub(s.start_ms) >= MIN_SEG_MS)
        .collect();
    picked.sort_by_key(|s| std::cmp::Reverse(s.end_ms.saturating_sub(s.start_ms)));
    picked.truncate(MAX_SEGS);

    // 按信道累计:sum 向量 + 段数 + 时长。嵌入/切片失败只跳该段——回灌是
    // 增值层,任何失败都不该冒泡成用户可见错误(Global Constraints)。
    struct Acc {
        sum: Vec<f32>,
        count: u64,
        total_ms: u64,
    }
    let mut by_source: BTreeMap<String, Acc> = BTreeMap::new();
    for s in picked {
        let Some((offset_ms, pcm)) = pcm_by_source.get(&s.source) else { continue };
        let start = (s.start_ms.saturating_sub(*offset_ms) as usize).saturating_mul(16);
        let end = (s.end_ms.saturating_sub(*offset_ms) as usize).saturating_mul(16);
        let end = end.min(pcm.len());
        if start >= end {
            continue;
        }
        let Ok(vec) = embedder.embed(&pcm[start..end]) else { continue };
        if vec.is_empty() {
            continue;
        }
        let acc = by_source.entry(s.source.clone()).or_insert_with(|| Acc {
            sum: vec![0.0; vec.len()],
            count: 0,
            total_ms: 0,
        });
        if acc.sum.len() != vec.len() {
            continue; // 维度不一致只可能是模型异常,弃段
        }
        for (a, b) in acc.sum.iter_mut().zip(&vec) {
            *a += b;
        }
        acc.count += 1;
        acc.total_ms += s.end_ms.saturating_sub(s.start_ms);
    }

    by_source
        .into_iter()
        .filter_map(|(source, acc)| {
            let norm: f32 = acc.sum.iter().map(|x| x * x).sum::<f32>().sqrt();
            if norm <= f32::EPSILON {
                return None;
            }
            Some(ClusterSnapshot {
                id: format!("fb-{source}"),
                centroid: acc.sum.iter().map(|x| x / norm).collect(),
                count: acc.count,
                sources: BTreeSet::from([source]),
                person: Some(person_id.to_string()),
                total_ms: acc.total_ms,
            })
        })
        .collect()
}
```

- [ ] **Step 4: 跑测试确认通过**

Run: `cd src-tauri && cargo test --lib feedback:: -- --nocapture`
Expected: 4 PASS。

- [ ] **Step 5: 提交**

```bash
git add src-tauri/src/feedback.rs src-tauri/src/lib.rs
git commit -m "feat(feedback): 纠错回灌纯逻辑核——指认段按信道嵌入成质心快照"
```

---

### Task 4: feedback 磁盘壳(解码 + 模型门禁 + 入库)

**Files:**
- Modify: `src-tauri/src/feedback.rs`(追加磁盘壳函数与测试)

**Interfaces:**
- Consumes: `store::transcode::track_pcm(note_dir, source)`(`transcode.rs:395`,wav 优先、m4a 兜底)、`store::audio::load_audio_meta(note_dir)`(`.tracks[source].offset_ms`)、`store::VoiceprintStore`(`load()` / `upsert_from_session` :667)。
- Produces: `feedback::reinforce_person(note_dir: &Path, segs: &[SegmentRecord], seg_filter: &SegFilter, person_id: &str, vp: &VoiceprintStore, expected_model: &str, embedder: &mut dyn SpeakerEmbedder, now: &str) -> anyhow::Result<ReinforceOutcome>` 与 `enum SegFilter { Speakers(BTreeSet<String>), Seqs(BTreeSet<u64>) }`(Task 5 依赖)。

- [ ] **Step 1: 写失败测试**

追加到 `feedback.rs` `mod tests`(磁盘壳用真 wav:hound 已在 dependencies):

```rust
fn write_wav(path: &std::path::Path, ms: u64) {
    let spec = hound::WavSpec {
        channels: 1,
        sample_rate: 16000,
        bits_per_sample: 16,
        sample_format: hound::SampleFormat::Int,
    };
    let mut w = hound::WavWriter::create(path, spec).unwrap();
    for _ in 0..(ms * 16) {
        w.write_sample(3000i16).unwrap();
    }
    w.finalize().unwrap();
}

#[test]
fn reinforce_merges_into_person_centroid_via_upsert() {
    let dir = tempfile::tempdir().unwrap();
    write_wav(&dir.path().join("mic.wav"), 4000);
    // 声纹库:预置 P1(空名即可),embedding_model 置空(空=未定,门禁放行,
    // 与 lib.rs 种子门禁同语义)。
    let vp_root = tempfile::tempdir().unwrap();
    let vp = crate::store::VoiceprintStore::new(vp_root.path().to_path_buf());
    // 用 upsert_from_session 造出 P1:一个够 AUTO_ENROLL_MS 的无主簇。
    let seeded = vp
        .upsert_from_session(
            &[crate::diar::registry::ClusterSnapshot {
                id: "S1".into(),
                centroid: vec![1.0, 0.0, 0.0, 0.0],
                count: 4,
                sources: std::collections::BTreeSet::from(["mic".to_string()]),
                person: None,
                total_ms: 12_000,
            }],
            "2026-08-08T00:00:00+08:00",
        )
        .unwrap();
    let person_id = seeded.get("S1").unwrap().clone();

    let segs = vec![seg(0, "mic", 0, 2000), seg(1, "mic", 2000, 4000)];
    let mut emb = crate::diar::MockEmbedder::new(vec![Ok(unit(1)), Ok(unit(1))]);
    let out = reinforce_person(
        dir.path(),
        &segs,
        &SegFilter::Speakers(std::collections::BTreeSet::from(["S1".to_string()])),
        &person_id,
        &vp,
        "", // expected_model 空 → 门禁不拦
        &mut emb,
        "2026-08-08T01:00:00+08:00",
    )
    .unwrap();
    assert_eq!(out.embedded_segments, 2);
    let lib = vp.load();
    let p = lib.people.get(&person_id).unwrap();
    assert!(p.total_ms >= 12_000 + 4_000, "回灌时长应累计入库");
    // 质心被朝 unit(1) 方向拉动:与原 unit(0) 的点积必然下降。
    let c = &p.centroids.get("mic").unwrap().vec;
    assert!(c[1] > 0.0, "新方向分量必须并入质心");
}

#[test]
fn reinforce_respects_model_gate() {
    let dir = tempfile::tempdir().unwrap();
    write_wav(&dir.path().join("mic.wav"), 2000);
    let vp_root = tempfile::tempdir().unwrap();
    let vp = crate::store::VoiceprintStore::new(vp_root.path().to_path_buf());
    let segs = vec![seg(0, "mic", 0, 2000)];
    let mut emb = crate::diar::MockEmbedder::new(vec![Ok(unit(0))]);
    // 库 embedding_model 为空视为与任何期望一致;先造一个非空且不一致的场景:
    // 直接经 rebuild 场景太重,这里用 expected 与库不一致的最小面——
    // 库空 + expected 非空 = 允许(老库兼容);之后把库写成 "model-a" 再期望 "model-b"。
    let out = reinforce_person(
        dir.path(), &segs,
        &SegFilter::Speakers(std::collections::BTreeSet::from(["S1".to_string()])),
        "P999", &vp, "model-b", &mut emb, "2026-08-08T01:00:00+08:00",
    ).unwrap();
    // P999 不存在:upsert 对悬空引用静默跳过,不报错(容错与 upsert 一致)。
    assert_eq!(out.embedded_segments, 1);
    assert!(vp.load().people.is_empty());
}

#[test]
fn reinforce_skips_when_model_mismatch() {
    let dir = tempfile::tempdir().unwrap();
    let vp_root = tempfile::tempdir().unwrap();
    let vp = crate::store::VoiceprintStore::new(vp_root.path().to_path_buf());
    // 把库标成 model-a(手写最小 voiceprints.json 即可,load 走正常反序列化)。
    std::fs::write(
        vp_root.path().join("voiceprints.json"),
        r#"{"schema_version":1,"next_person":1,"people":{},"redirects":{},"embedding_model":"model-a"}"#,
    )
    .unwrap();
    let segs = vec![seg(0, "mic", 0, 2000)];
    let mut emb = crate::diar::MockEmbedder::new(vec![Ok(unit(0))]);
    let out = reinforce_person(
        dir.path(), &segs,
        &SegFilter::Speakers(std::collections::BTreeSet::from(["S1".to_string()])),
        "P1", &vp, "model-b", &mut emb, "t",
    ).unwrap();
    assert_eq!(out.skipped_reason.as_deref(), Some("embedding-model-mismatch"));
    assert_eq!(out.embedded_segments, 0);
}
```

注:`voiceprints.json` 手写内容的字段名以 `store/voiceprints.rs:36-85` 实际 serde 定义为准,实现时如字段有出入按真实结构修正测试数据。`VoiceprintStore::new` 的构造签名同理(若真名不同,用 `store/voiceprints.rs` 中现有测试的构造方式)。

- [ ] **Step 2: 跑测试确认编译失败**

Run: `cd src-tauri && cargo test --lib feedback:: -- --nocapture`
Expected: 编译错误(`reinforce_person`/`SegFilter`/`ReinforceOutcome` 未定义)。

- [ ] **Step 3: 写实现**

追加到 `feedback.rs`:

```rust
use std::path::Path;

/// 指认范围:原始稿按 S 号(speakers.json 域),修订稿按 source_seqs(R 段落已
/// 把 seq 集合显式落盘,不必反查 R→S 映射)。
pub enum SegFilter {
    Speakers(BTreeSet<String>),
    Seqs(BTreeSet<u64>),
}

#[derive(Debug, Default)]
pub struct ReinforceOutcome {
    pub embedded_segments: usize,
    pub skipped_reason: Option<&'static str>,
}

/// 磁盘壳:模型门禁 → 选段 → 逐轨解码 → 纯逻辑核 → upsert_from_session 入库。
/// 解码失败/轨缺失都只收窄不报错——回灌拿得到多少信号用多少;门禁不一致
/// 整体跳过(不同模型的向量空间不可比,错灌比不灌糟)。
pub fn reinforce_person(
    note_dir: &Path,
    segs: &[SegmentRecord],
    filter: &SegFilter,
    person_id: &str,
    vp: &crate::store::VoiceprintStore,
    expected_model: &str,
    embedder: &mut dyn SpeakerEmbedder,
    now: &str,
) -> anyhow::Result<ReinforceOutcome> {
    let lib = vp.load();
    // 门禁语义与 lib.rs:219-238 种子注入一致:库侧为空(老库/新库)不拦,
    // 两侧都非空且不等才拦。
    if !lib.embedding_model.is_empty() && !expected_model.is_empty() && lib.embedding_model != expected_model
    {
        return Ok(ReinforceOutcome { embedded_segments: 0, skipped_reason: Some("embedding-model-mismatch") });
    }

    let wanted: Vec<&SegmentRecord> = segs
        .iter()
        .filter(|s| match filter {
            SegFilter::Speakers(ids) => s.speaker.as_ref().map_or(false, |sp| ids.contains(sp)),
            SegFilter::Seqs(seqs) => seqs.contains(&s.seq),
        })
        .collect();
    if wanted.is_empty() {
        return Ok(ReinforceOutcome { embedded_segments: 0, skipped_reason: Some("no-segments") });
    }

    let meta = crate::store::audio::load_audio_meta(note_dir);
    let mut pcm_by_source: BTreeMap<String, (u64, Vec<f32>)> = BTreeMap::new();
    for source in wanted.iter().map(|s| s.source.as_str()).collect::<BTreeSet<_>>() {
        match crate::store::transcode::track_pcm(note_dir, source) {
            Ok(pcm) => {
                let offset = meta.tracks.get(source).map(|t| t.offset_ms).unwrap_or(0);
                pcm_by_source.insert(source.to_string(), (offset, pcm));
            }
            Err(e) => eprintln!("feedback: 音轨 {source} 解码失败,该轨不回灌: {e}"),
        }
    }

    let snaps = build_snapshots(&wanted, &pcm_by_source, person_id, embedder);
    let embedded_segments = snaps.iter().map(|s| s.count as usize).sum();
    if !snaps.is_empty() {
        vp.upsert_from_session(&snaps, now)?;
    }
    Ok(ReinforceOutcome { embedded_segments, skipped_reason: None })
}
```

- [ ] **Step 4: 跑测试确认通过**

Run: `cd src-tauri && cargo test --lib feedback:: -- --nocapture`
Expected: 7 PASS(Task 3 的 4 条 + 本任务 3 条)。

- [ ] **Step 5: 提交**

```bash
git add src-tauri/src/feedback.rs
git commit -m "feat(feedback): 磁盘壳——解码指认段、模型门禁、经 upsert_from_session 回灌质心"
```

---

### Task 5: 指认 IPC 挂钩回灌(异步 best-effort)

**Files:**
- Modify: `src-tauri/src/lib.rs`(`assign_note_speaker_person` :2594、`assign_refined_person` :3135,新增私有 helper)

**Interfaces:**
- Consumes: Task 4 的 `feedback::reinforce_person`/`SegFilter`;`store::NoteStore::load(id) -> Note{segments, speakers}`(`notes.rs:120`);`store::load_refined(note_dir)`(取 R 段落的 `source_seqs`);`diar::SherpaEmbedder::new(&speaker_model_path(app))`(:701);`open_voiceprint_store(&app)`(:3417);`settings` 里的 `speaker_model`。
- Produces: 无新公开接口;两个既有命令行为增强(指认成功后后台回灌)。

- [ ] **Step 1: 写 helper(无独立单测,逻辑全在已测的 feedback 内;本任务的可测面是「命令成功路径不因回灌失败而失败」,由既有命令测试覆盖)**

在 `lib.rs` 新增:

```rust
/// 指认成功后的纠错回灌(spec P1-2):后台 best-effort,任何失败只留日志。
/// scope:原始稿指认传 S 号,修订稿指认传 R 段落的 source_seqs。
fn spawn_feedback_reinforce(app: &AppHandle, note_id: String, filter: feedback::SegFilter, person_id: String) {
    let app = app.clone();
    tauri::async_runtime::spawn_blocking(move || {
        let run = || -> anyhow::Result<()> {
            let dir = notes_dir(&app)?;
            let note = store::NoteStore::new(dir.clone()).load(&note_id)?;
            let note_dir = dir.join(&note_id);
            let vp = open_voiceprint_store(&app).map_err(anyhow::Error::msg)?;
            let expected_model = { app.state::<AppState>().settings.lock().unwrap().speaker_model.clone() };
            let mut embedder = diar::SherpaEmbedder::new(&speaker_model_path(&app))?;
            let now = chrono::Local::now().to_rfc3339();
            let out = feedback::reinforce_person(
                &note_dir, &note.segments, &filter, &person_id, &vp, &expected_model, &mut embedder, &now,
            )?;
            eprintln!(
                "feedback: 回灌完成 note={note_id} person={person_id} segments={} skipped={:?}",
                out.embedded_segments, out.skipped_reason
            );
            Ok(())
        };
        if let Err(e) = run() {
            eprintln!("feedback: 纠错回灌失败(不影响指认) note={note_id}: {e}");
        }
    });
}
```

(`settings` 的锁与访问方式照 `lib.rs` 内现有读法;`AppState.settings` 若非 Mutex 直取字段,以现状为准。)

- [ ] **Step 2: 两个命令挂钩**

`assign_note_speaker_person`(:2594)把结尾的 `request(...)` 改为:

```rust
    let speaker_for_feedback = speaker_id.clone();
    let note_for_feedback = note_id.clone();
    app.state::<lifecycle::LifecycleHandle>().request(lifecycle::machine::Msg::EditNote {
        op: lifecycle::machine::EditOp::AssignPerson {
            id: note_id,
            speaker_id,
            person_id: resolved.clone(),
        },
    })?;
    spawn_feedback_reinforce(
        &app,
        note_for_feedback,
        feedback::SegFilter::Speakers(std::collections::BTreeSet::from([speaker_for_feedback])),
        resolved,
    );
    Ok(())
```

`assign_refined_person`(:3135)在 `store::assign_refined_person(...)` 成功后追加:

```rust
    store::assign_refined_person(&dir, &speaker_id, &resolved, &name).map_err(|e| e.to_string())?;
    // R 段落的 source_seqs 已显式落盘:收集该 R 号全部原始 seq 供回灌。
    if let Some(doc) = store::load_refined(&dir) {
        let seqs: std::collections::BTreeSet<u64> = doc
            .paragraphs
            .iter()
            .filter(|p| p.speaker == speaker_id)
            .flat_map(|p| p.source_seqs.iter().copied())
            .collect();
        if !seqs.is_empty() {
            spawn_feedback_reinforce(&app, note_id, feedback::SegFilter::Seqs(seqs), resolved);
        }
    }
    Ok(())
```

(`load_refined` 的真名/返回按 `store/refined.rs` 现状;若签名是 `(note_dir) -> Option<RefinedDoc>` 即如上,不同则按实调整调用。)

- [ ] **Step 3: 全量回归**

Run: `cd src-tauri && cargo test --lib`
Expected: 全 PASS(两个命令的既有测试路径不触发真嵌入器——spawn_blocking 里 SherpaEmbedder 加载失败只打日志,不影响命令返回值)。

- [ ] **Step 4: 编译门**

Run: `cd src-tauri && cargo clippy --all-targets 2>&1 | tail -5`
Expected: 无新增 warning/error。

- [ ] **Step 5: 提交**

```bash
git add src-tauri/src/lib.rs
git commit -m "feat(feedback): 两条指认命令成功后异步回灌质心(best-effort)"
```

---

### Task 6: 评测工具链(ground truth 标注 + 评测 bin)

**Files:**
- Create: `src-tauri/src/bin/speaker_eval.rs`
- Modify: `src-tauri/Cargo.toml`(如 bin 不被自动发现才需要;`src/bin/` 下默认自动发现,预计零改动)

**Interfaces:**
- Consumes: 只读磁盘 JSON(`<notes>/<id>/speakers.json`、`voiceprints.json`),serde_json::Value 解析,**不 use 库 crate**(评测工具与应用零耦合,库损坏也能跑)。
- Produces: 真值文件格式(P2a 沿用):JSONL,每行 `{"note_id":"...","speaker_id":"S3","person":"张伟"}`(`person` 为空串表示「标注过但确认无法归属」,参与分母)。

- [ ] **Step 1: 写失败测试(纯函数)**

`src-tauri/src/bin/speaker_eval.rs`:

```rust
//! 说话人识别评测(spec rev2「测试与评测」):对人工标注的「簇→真人」真值,
//! 统计当前声学链路(P1 基线)/后续 identify(P2a 起)的归属质量。
//! 只读 JSON 文件、不依赖库 crate:评测工具必须在应用任何状态下可用。
//!
//! 用法:
//!   标注模板:speaker_eval init <notes_dir> <note_id>           # 打印待标注模板行
//!   评测:    speaker_eval run <notes_dir> <voiceprints.json> <truth.jsonl>

use std::collections::BTreeMap;

use serde_json::Value;

/// redirects 归一(与 store::VoiceprintStore::resolve 同语义,上限 8 跳)。
fn resolve<'a>(redirects: &'a BTreeMap<String, String>, people: &BTreeMap<String, Value>, id: &'a str) -> Option<&'a str> {
    let mut cur = id;
    for _ in 0..=8 {
        if people.contains_key(cur) {
            return Some(cur);
        }
        match redirects.get(cur) {
            Some(next) => cur = next,
            None => return None,
        }
    }
    None
}

#[derive(Debug, Default, PartialEq)]
struct Metrics {
    labeled: usize,     // 标注总数(分母)
    correct: usize,     // 命中:预测人名 == 真值人名
    wrong: usize,       // 误认:预测了别人(错认比不认糟,单列)
    unassigned: usize,  // 未识别:无 person 关联或库中无名
}

fn score(
    truth: &[(String, String, String)],                 // (note_id, speaker_id, person_name)
    predicted: &BTreeMap<(String, String), String>,     // (note_id, speaker_id) -> person_name
) -> Metrics {
    let mut m = Metrics::default();
    for (note, spk, want) in truth {
        m.labeled += 1;
        match predicted.get(&(note.clone(), spk.clone())) {
            Some(got) if !got.is_empty() && got == want => m.correct += 1,
            Some(got) if !got.is_empty() && !want.is_empty() && got != want => m.wrong += 1,
            _ => m.unassigned += 1,
        }
    }
    m
}

fn main() {
    // Step 3 填充:init / run 两个子命令。
    todo!()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_follows_redirect_chain_with_cap() {
        let mut redirects = BTreeMap::new();
        redirects.insert("P1".to_string(), "P2".to_string());
        redirects.insert("P2".to_string(), "P3".to_string());
        let mut people = BTreeMap::new();
        people.insert("P3".to_string(), Value::Null);
        assert_eq!(resolve(&redirects, &people, "P1"), Some("P3"));
        assert_eq!(resolve(&redirects, &people, "P9"), None);
    }

    #[test]
    fn score_separates_wrong_from_unassigned() {
        let truth = vec![
            ("n1".into(), "S1".into(), "张伟".into()),
            ("n1".into(), "S2".into(), "李雷".into()),
            ("n1".into(), "S3".into(), "韩梅".into()),
        ];
        let mut predicted = BTreeMap::new();
        predicted.insert(("n1".to_string(), "S1".to_string()), "张伟".to_string()); // 命中
        predicted.insert(("n1".to_string(), "S2".to_string()), "张伟".to_string()); // 误认
        // S3 无预测 → 未识别
        let m = score(&truth, &predicted);
        assert_eq!(m, Metrics { labeled: 3, correct: 1, wrong: 1, unassigned: 1 });
    }
}
```

- [ ] **Step 2: 跑测试确认(纯函数测试先绿,main 是 todo 不影响 lib 测试)**

Run: `cd src-tauri && cargo test --bin speaker_eval`
Expected: 2 PASS(`main` 的 `todo!()` 不被测试调用)。

- [ ] **Step 3: 补 main(init/run 两个子命令)**

```rust
fn main() {
    let args: Vec<String> = std::env::args().collect();
    match args.get(1).map(String::as_str) {
        Some("init") if args.len() == 4 => init(&args[2], &args[3]),
        Some("run") if args.len() == 5 => run(&args[2], &args[3], &args[4]),
        _ => {
            eprintln!("用法:\n  speaker_eval init <notes_dir> <note_id>\n  speaker_eval run <notes_dir> <voiceprints.json> <truth.jsonl>");
            std::process::exit(2);
        }
    }
}

/// 打印待标注模板:该笔记每个说话人一行 JSONL + 注释行给出名字线索与首段文本。
fn init(notes_dir: &str, note_id: &str) {
    let dir = std::path::Path::new(notes_dir).join(note_id);
    let speakers: BTreeMap<String, Value> = std::fs::read_to_string(dir.join("speakers.json"))
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_default();
    let segments = std::fs::read_to_string(dir.join("segments.jsonl")).unwrap_or_default();
    for (sid, meta) in &speakers {
        let sample = segments
            .lines()
            .filter_map(|l| serde_json::from_str::<Value>(l).ok())
            .find(|v| v["speaker"].as_str() == Some(sid))
            .and_then(|v| v["text"].as_str().map(|s| s.chars().take(40).collect::<String>()))
            .unwrap_or_default();
        println!("# {sid} name={} person_id={:?} 首段:{sample}", meta["name"].as_str().unwrap_or(""), meta["person_id"].as_str());
        println!(r#"{{"note_id":"{note_id}","speaker_id":"{sid}","person":""}}"#);
    }
}

/// 评测:truth 每行与 speakers.json 预测比对,按 Metrics 输出。
fn run(notes_dir: &str, vp_path: &str, truth_path: &str) {
    let vp: Value = serde_json::from_str(&std::fs::read_to_string(vp_path).expect("读 voiceprints.json 失败"))
        .expect("voiceprints.json 非法");
    let people: BTreeMap<String, Value> =
        serde_json::from_value(vp["people"].clone()).unwrap_or_default();
    let redirects: BTreeMap<String, String> =
        serde_json::from_value(vp["redirects"].clone()).unwrap_or_default();

    let mut truth: Vec<(String, String, String)> = Vec::new();
    for line in std::fs::read_to_string(truth_path).expect("读 truth 失败").lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let v: Value = serde_json::from_str(line).expect("truth 行非法 JSON");
        truth.push((
            v["note_id"].as_str().unwrap_or_default().to_string(),
            v["speaker_id"].as_str().unwrap_or_default().to_string(),
            v["person"].as_str().unwrap_or_default().to_string(),
        ));
    }

    let mut predicted: BTreeMap<(String, String), String> = BTreeMap::new();
    for (note_id, speaker_id, _) in &truth {
        let path = std::path::Path::new(notes_dir).join(note_id).join("speakers.json");
        let Some(speakers) = std::fs::read_to_string(&path)
            .ok()
            .and_then(|t| serde_json::from_str::<BTreeMap<String, Value>>(&t).ok())
        else {
            continue;
        };
        let Some(meta) = speakers.get(speaker_id) else { continue };
        let name = meta["person_id"]
            .as_str()
            .and_then(|pid| resolve(&redirects, &people, pid))
            .and_then(|rid| people[rid]["name"].as_str())
            .unwrap_or_default();
        predicted.insert((note_id.clone(), speaker_id.clone()), name.to_string());
    }

    let m = score(&truth, &predicted);
    println!("标注 {}  命中 {}  误认 {}  未识别 {}", m.labeled, m.correct, m.wrong, m.unassigned);
    if m.labeled > 0 {
        println!(
            "准确率 {:.1}%  误认率 {:.1}%(P2b 验收线关注此值)",
            100.0 * m.correct as f64 / m.labeled as f64,
            100.0 * m.wrong as f64 / m.labeled as f64
        );
    }
}
```

- [ ] **Step 4: 端到端手测**

Run(任选一条真实笔记,`<数据目录>` 是 app_data_dir):
```bash
cd src-tauri
cargo run --bin speaker_eval -- init "<数据目录>/notes" <某笔记id> > /tmp/truth-template.jsonl
cat /tmp/truth-template.jsonl   # 人工把 person 填上后:
cargo run --bin speaker_eval -- run "<数据目录>/notes" "<数据目录>/voiceprints.json" /tmp/truth-template.jsonl
```
Expected: init 打出每说话人模板行;run 打出四项计数 + 两个百分比。

- [ ] **Step 5: 全量回归 + 提交**

Run: `cd src-tauri && cargo test --bin speaker_eval && cargo test --lib && cargo fmt`
Expected: 全 PASS。

```bash
git add src-tauri/src/bin/speaker_eval.rs
git commit -m "feat(eval): 说话人归属评测工具——truth 标注模板与命中/误认/未识别统计"
```

---

## 收尾核对(整计划完成后)

- [ ] `cd src-tauri && cargo test` 全绿、`cargo clippy --all-targets` 无新告警
- [ ] 真机冒烟(留待用户,列入 PR 描述):① 指认一个说话人给库中人物,日志出现「feedback: 回灌完成」;② 换嵌入模型后指认,日志出现 `embedding-model-mismatch`;③ 精修一场带多人对话的笔记,AI 日志中请求体含 `speaker=` 标注
- [ ] PR 描述注明:本期为 spec P1;P2a(identify 只读期)另立计划
