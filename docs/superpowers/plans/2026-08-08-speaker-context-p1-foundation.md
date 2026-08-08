# 说话人上下文推断 P1(地基)Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

rev2:消化 Codex 计划审查(24 P1 + 15 P2)。要点:修正虚构接口(AppState 无 settings、`refine_prompt` 才是模板函数、`rms` 是 Option、门禁是严格相等);回灌语义重做(幂等账本、纠错还原、无名先前人物走 journaled 合并、专用 store 方法不再借用 `upsert_from_session`);嵌入并发上闸;评测支持 S/R 两层与 person_id 真值、修正计分。

**Goal:** 落地 spec P1 三件事:① 精修 LLM(HTTP+Agent 双路径)能看到说话人标签;② 人工指认说话人后把该人声纹回灌质心(纠错回灌,幂等、可纠正);③ 建立「簇→真人」评测工具链,为 P2b 自动应用开闸提供数据门。

**Architecture:** HTTP 路径在 `polish` 分块拼接中带 sanitize 后的说话人标签并更新 SYSTEM_PROMPT 契约;Agent 路径改 `refine_prompt` 指令模板。回灌新建 `feedback` 模块(纯逻辑核 + 磁盘壳 + 笔记级账本)+ `VoiceprintStore::reinforce_feedback` 专用方法;挂在两个指认 IPC 成功后:先前关联是无名人物时走既有 `merge_journaled`(可撤销),否则段重嵌入回灌,全程 best-effort。评测是独立 bin,读 JSON 零耦合,S(speakers.json)/R(aing.json)两层、person_id 或人名真值。

**Tech Stack:** Rust(Tauri 2 后端),sherpa-onnx CAM++ 嵌入,serde_json;测试 MockEmbedder + tempfile + hound。

**Spec:** `docs/superpowers/specs/2026-08-08-speaker-context-inference-design.md`(rev2)

## Global Constraints

- 指认主链路绝不因回灌失败而失败:回灌 best-effort,出错只 `eprintln!`。
- 模型门禁与种子注入同语义(`lib.rs:229-236`):**严格相等**——`vp.embedding_model != settings.speaker_model` 即跳过回灌(两侧默认值都是 `"campplus"`,正常路径相等)。
- 单段实际切片时长(非账面时长)<1.5s 不进回灌(`registry.rs` `MIN_CENTROID_UPDATE_SAMPLES=24_000` 同口径)。
- 每个嵌入向量先单位归一化再累加(registry 同口径);非有限值(NaN/Inf)整段弃用。
- 回灌幂等:同一(段集合→人物)只回灌一次(笔记级账本);重复指认静默跳过。
- 纠错路径:同段集合改指他人时,若目标人质心状态未被其它写动过则先还原上次回灌,否则只回灌新人并留日志(先前**有名**人物在停录时已并入的净增量无法撤销——P1 已知限制,spec P2 identify_journal 解决)。
- 嵌入并发上闸:模块级互斥,同一时刻最多一个回灌任务在嵌入(不借用 `AppState.embedder_cache`——开录会 take 走它)。
- 已知限制(如实记录,不在 P1 修):`track_pcm` 的 m4a 临时文件名固定,回灌与 Aing/重转写并发解码同一轨存在竞争窗口——回灌侧靠互斥门自串行,跨功能竞争依赖既有格局;HTTP 分块边界处的跨块话轮上下文断裂,P1 不做 overlap。
- 新增 serde 字段一律 `#[serde(default)]`;注释中文、讲"为什么";每任务收尾 `cd src-tauri && cargo test <过滤> && cargo fmt` 后提交;clippy 门用完整输出并看退出码,禁止 `| tail`。

---

### Task 1: HTTP 精修 prompt 携带说话人标签

**Files:**
- Modify: `src-tauri/src/refine/llm.rs`(`SYSTEM_PROMPT` :12、`format_chunk_paragraphs` :249、`call_chunk` :264-271、`polish` :487-506、tests :548 起)

**Interfaces:**
- Consumes: `RefinedParagraph { speaker: String, name: Option<String>, text: String, .. }`(`store/refined.rs`)
- Produces:
  - `struct ChunkPara<'a> { index: usize, label: String, text: &'a str }`(llm.rs 内 `pub(crate)`)
  - `format_chunk_paragraphs(&[ChunkPara]) -> String`
  - `speaker_label(&RefinedParagraph) -> String`(sanitize:人名优先、去控制字符与冒号、trim、空退 R 号)

- [ ] **Step 1: 写失败测试**

替换现有 `chunk_prompt_keeps_absolute_paragraph_indexes`(:950)并新增:

```rust
fn cp(index: usize, label: &str, text: &str) -> ChunkPara<'_> {
    ChunkPara { index, label: label.into(), text }
}

#[test]
fn chunk_prompt_keeps_absolute_paragraph_indexes() {
    assert_eq!(
        format_chunk_paragraphs(&[cp(7, "张伟", "第八段"), cp(12, "R2", "第十三段")]),
        "paragraph_index=7 speaker=张伟: 第八段\nparagraph_index=12 speaker=R2: 第十三段\n"
    );
}

#[test]
fn speaker_label_prefers_name_sanitizes_and_falls_back() {
    let p = |name: Option<&str>| para_with("R3", name, "x");
    assert_eq!(speaker_label(&p(Some("张伟"))), "张伟");
    // 名字是用户可编辑文本:换行/冒号会破坏行格式,一律替换成空格后 trim。
    assert_eq!(speaker_label(&p(Some("张:伟\n总"))), "张 伟 总");
    assert_eq!(speaker_label(&p(Some("   "))), "R3", "纯空白名退回簇号");
    assert_eq!(speaker_label(&p(None)), "R3");
}
```

测试工厂(本任务与 Task 3 共用思路;字段以 `store/refined.rs` 实际结构为准,缺省字段填空值):

```rust
fn para_with(speaker: &str, name: Option<&str>, text: &str) -> RefinedParagraph {
    RefinedParagraph {
        speaker: speaker.into(),
        name: name.map(Into::into),
        person_id: None,
        start_ms: 0,
        end_ms: 0,
        text: text.into(),
        source_seqs: vec![],
        mentions: vec![],
    }
}
```

- [ ] **Step 2: 跑测试确认编译失败**

Run: `cd src-tauri && cargo test --lib chunk_prompt`
Expected: 编译错误(`ChunkPara`/`speaker_label` 未定义)。

- [ ] **Step 3: 改实现**

```rust
/// 一段进块的最小信息:绝对下标 + 已 sanitize 的说话人标签 + 正文引用。
pub(crate) struct ChunkPara<'a> {
    pub index: usize,
    pub label: String,
    pub text: &'a str,
}

/// 说话人标签:人名优先(已命名/已关联),否则 R 号。名字是用户可编辑字符串,
/// 换行会拆行、冒号会截断"标签: 正文"格式,统一替换为空格;这不是安全边界
/// (笔记正文本就是用户数据),只是保住行格式不被破坏。
pub(crate) fn speaker_label(p: &RefinedParagraph) -> String {
    let cleaned = p
        .name
        .as_deref()
        .unwrap_or("")
        .replace(['\n', '\r', ':', ':'], " ")
        .trim()
        .to_string();
    if cleaned.is_empty() { p.speaker.clone() } else { cleaned }
}

fn format_chunk_paragraphs(paragraphs: &[ChunkPara]) -> String {
    paragraphs
        .iter()
        .map(|p| format!("paragraph_index={} speaker={}: {}\n", p.index, p.label, p.text))
        .collect()
}
```

`call_chunk` 参数 `paragraphs: &[(usize, &str)]` → `&[ChunkPara]`(`paragraphs.len()` 用法不变)。

`polish`(:502-506)构造处:

```rust
let inputs: Vec<ChunkPara> = chunk
    .iter()
    .map(|&i| ChunkPara { index: i, label: speaker_label(&paragraphs[i]), text: paragraphs[i].text.as_str() })
    .collect();
```

`SYSTEM_PROMPT`(:12)在「输出 JSON」句之前插入(单行,与既有风格一致):

```text
每段前的 speaker= 标注是该段说话人(人名或簇号),仅供理解上下文:用于人名/称呼错字判断与实体归一(如称呼「小王」后由 speaker=王某 的段应答,可确认「王」字写法)。禁止据此改写句式、把代词替换成人名、或把 speaker 标注/说话人名写进 texts;texts 只输出修订后的正文。
```

- [ ] **Step 4: polish 级联测试(带请求体捕获的 mock)**

现有 `mock_server`(:556)不回传请求体,`read_request` 返回 `()`;新增独立的捕获版(不改旧的):

```rust
/// mock_server 的捕获版:按 content-length 读完请求体存入共享 Vec,再回包。
fn mock_server_capturing(
    responses: Vec<String>,
) -> (String, std::sync::Arc<std::sync::Mutex<Vec<String>>>) {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap().to_string();
    let captured = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let cap = captured.clone();
    std::thread::spawn(move || {
        for body in responses {
            let (mut s, _) = listener.accept().unwrap();
            let mut buf = Vec::new();
            let mut tmp = [0u8; 4096];
            let mut header_end = 0usize;
            let mut content_len = 0usize;
            loop {
                let n = s.read(&mut tmp).unwrap_or(0);
                if n == 0 {
                    break;
                }
                buf.extend_from_slice(&tmp[..n]);
                if header_end == 0 {
                    if let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                        header_end = pos + 4;
                        let head = String::from_utf8_lossy(&buf[..pos]).to_ascii_lowercase();
                        content_len = head
                            .lines()
                            .find_map(|l| l.strip_prefix("content-length:"))
                            .and_then(|v| v.trim().parse().ok())
                            .unwrap_or(0);
                    }
                }
                if header_end > 0 && buf.len() >= header_end + content_len {
                    break;
                }
            }
            cap.lock().unwrap().push(String::from_utf8_lossy(&buf[header_end..]).to_string());
            let resp = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\n\r\n{}",
                body.len(),
                body
            );
            let _ = s.write_all(resp.as_bytes());
        }
    });
    (addr, captured)
}

#[test]
fn polish_prompt_carries_speaker_labels() {
    let body = serde_json::json!({
        "choices": [{ "message": { "content":
            "{\"glossary\":{},\"texts\":[\"甲\",\"乙\"],\"entities\":[],\"relations\":[]}" } }]
    })
    .to_string();
    let (addr, captured) = mock_server_capturing(vec![body]);
    let cfg = LlmConfig { base_url: format!("http://{addr}"), model: "m".into(), api_key: "k".into() };
    let mut ps = vec![para_with("R1", Some("张伟"), "甲"), para_with("R2", None, "乙")];
    let (outcome, _, _) = polish(&cfg, &mut ps, None);
    assert!(matches!(outcome, LlmOutcome::Done | LlmOutcome::DoneWithRelationErrors));
    let req = captured.lock().unwrap().join("");
    assert!(req.contains("speaker=张伟"), "有名字的段必须带人名标签: {req}");
    assert!(req.contains("speaker=R2"), "无名字的段用 R 号兜底");
}
```

- [ ] **Step 5: 跑测试确认通过**

Run: `cd src-tauri && cargo test --lib llm::`
Expected: 全 PASS(有断言旧 `paragraph_index={i}: ` 前缀格式的既有测试一并更新为新格式)。

- [ ] **Step 6: 提交**

```bash
git add src-tauri/src/refine/llm.rs
git commit -m "feat(refine): HTTP 精修分块携带 sanitize 后的说话人标签,更新 SYSTEM_PROMPT 契约"
```

---

### Task 2: Agent 精修指令要求利用说话人字段

**Files:**
- Modify: `src-tauri/src/refine/agent.rs`(**`refine_prompt`** :189 ——指令模板在此;`refine_command` :243 只是把生成好的 prompt 组装成进程参数,不要改它)

**Interfaces:**
- Consumes: Agent 第一步经 MCP `get_note` 读段落,返回字段只有 `speaker/name/start_ms/end_ms/text`(`mcp/tools.rs:146`,**无 person_id**);指令文本不得声称有 person_id。
- Produces: 无新接口。

- [ ] **Step 1: 写失败测试**

`agent.rs` `mod tests` 新增(现有测试直接调私有 `refine_prompt("note-1")`,照抄该用法):

```rust
#[test]
fn refine_prompt_instructs_speaker_usage() {
    let p = refine_prompt("note-1");
    assert!(p.contains("speaker/name 字段"), "必须告知 Agent 段落带说话人字段");
    assert!(p.contains("禁止修改说话人归属"), "必须禁止 Agent 改动说话人归属");
}
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cd src-tauri && cargo test --lib refine_prompt_instructs`
Expected: FAIL(断言不含该文本)。

- [ ] **Step 3: 改 `refine_prompt` 模板**

在「取返回的 paragraphs 数组(段落下标从 0 计;若返回 refined=false 说明还没有精修稿,直接结束并说明)」(:194 附近)之后插入:

```text
每个段落带 speaker/name 字段(该段说话人的簇号与人名);精修正文时利用它做人名/称呼错字判断与称呼一致性(如称呼「小王」后由王某的段应答)。禁止修改说话人归属字段,禁止据此改写句式或把代词替换成人名,禁止把说话人名当前缀写进正文。
```

- [ ] **Step 4: 跑测试确认通过**

Run: `cd src-tauri && cargo test --lib agent::`
Expected: 全 PASS。

- [ ] **Step 5: 提交**

```bash
git add src-tauri/src/refine/agent.rs
git commit -m "feat(refine): Agent 精修指令明确要求利用说话人字段并禁改归属"
```

---

### Task 3: feedback 纯逻辑核(段→嵌入→信道快照)

**Files:**
- Create: `src-tauri/src/feedback.rs`
- Modify: `src-tauri/src/lib.rs`(模块声明区加 `mod feedback;`)

**Interfaces:**
- Consumes: `SegmentRecord`(`store/mod.rs:63-74`,注意 `rms: Option<f32>`)、`diar::SpeakerEmbedder`/`MockEmbedder`(`diar/mod.rs`)。
- Produces(Task 4/5 依赖):
  - `feedback::MIN_SEG_MS: u64 = 1_500`
  - `feedback::MAX_SEGS_PER_SOURCE: usize = 30`(**分信道限额**——全局截断会让长会议的次要信道颗粒无收)
  - `feedback::SourceStat { pub centroid: Vec<f32>, pub count: u64, pub total_ms: u64 }`
  - `feedback::build_source_stats(segs: &[&SegmentRecord], pcm_by_source: &BTreeMap<String, (u64, Vec<f32>)>, embedder: &mut dyn SpeakerEmbedder) -> BTreeMap<String, SourceStat>`

- [ ] **Step 1: 写失败测试**

```rust
//! 纠错回灌(spec rev2 P1-2):人工指认「这个说话人是库里的谁」之后,把该
//! 说话人的发声段重新嵌入并并入那个人的质心——让整理动作变成训练信号。
//! 本文件是纯逻辑核:不碰磁盘不碰库,输入段+PCM,输出各信道统计。

use std::collections::BTreeMap;

use crate::diar::SpeakerEmbedder;
use crate::store::SegmentRecord;

pub const MIN_SEG_MS: u64 = 1_500;
pub const MAX_SEGS_PER_SOURCE: usize = 30;

pub struct SourceStat {
    pub centroid: Vec<f32>,
    pub count: u64,
    pub total_ms: u64,
}

pub fn build_source_stats(
    segs: &[&SegmentRecord],
    pcm_by_source: &BTreeMap<String, (u64, Vec<f32>)>,
    embedder: &mut dyn SpeakerEmbedder,
) -> BTreeMap<String, SourceStat> {
    todo!()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::diar::MockEmbedder;
    use std::collections::BTreeSet;

    fn seg(seq: u64, source: &str, start_ms: u64, end_ms: u64) -> SegmentRecord {
        SegmentRecord {
            seq,
            source: source.into(),
            text: String::new(),
            start_ms,
            end_ms,
            speaker: Some("S1".into()),
            rms: Some(0.0),
        }
    }

    fn pcm_ms(ms: u64) -> Vec<f32> {
        vec![0.1; (ms * 16) as usize]
    }

    fn unit(i: usize) -> Vec<f32> {
        let mut v = vec![0.0; 4];
        v[i] = 1.0;
        v
    }

    #[test]
    fn short_and_truncated_segments_are_skipped() {
        let s_short = seg(0, "mic", 0, 800); // 账面 <1.5s
        // 账面 2s 但 PCM 只覆盖到 1200ms:切片后实际 <1.5s,同样跳过——
        // 账面时长骗不过实际样本数。
        let s_trunc = seg(1, "mic", 1000, 3000);
        let s_ok = seg(2, "mic", 0, 2000);
        let mut pcm = BTreeMap::new();
        pcm.insert("mic".to_string(), (0u64, pcm_ms(2200)));
        let mut emb = MockEmbedder::new(vec![Ok(unit(0))]);
        let stats = build_source_stats(&[&s_short, &s_trunc, &s_ok], &pcm, &mut emb);
        let mic = stats.get("mic").expect("s_ok 应产出 mic 统计");
        assert_eq!(mic.count, 1);
        assert_eq!(mic.total_ms, 2000, "只计实际嵌入段的实际切片时长");
    }

    #[test]
    fn stats_split_by_source_and_centroid_is_unit_mean_of_unit_vectors() {
        let m = seg(0, "mic", 0, 2000);
        let s = seg(1, "system", 0, 2000);
        let mut pcm = BTreeMap::new();
        pcm.insert("mic".to_string(), (0u64, pcm_ms(2000)));
        pcm.insert("system".to_string(), (0u64, pcm_ms(2000)));
        // 故意给未归一化向量:实现必须先逐向量归一化再累加。
        let mut emb = MockEmbedder::new(vec![Ok(vec![3.0, 0.0, 0.0, 0.0]), Ok(vec![0.0, 5.0, 0.0, 0.0])]);
        let stats = build_source_stats(&[&m, &s], &pcm, &mut emb);
        assert_eq!(stats.keys().collect::<Vec<_>>(), vec!["mic", "system"]);
        for st in stats.values() {
            let norm: f32 = st.centroid.iter().map(|x| x * x).sum::<f32>().sqrt();
            assert!((norm - 1.0).abs() < 1e-5, "质心必须单位化");
        }
        assert!((stats["mic"].centroid[0] - 1.0).abs() < 1e-5);
    }

    #[test]
    fn non_finite_embeddings_and_errors_degrade_to_skip() {
        let a = seg(0, "mic", 0, 2000);
        let b = seg(1, "mic", 2000, 4000);
        let c = seg(2, "system", 0, 2000); // system 轨 PCM 缺失
        let mut pcm = BTreeMap::new();
        pcm.insert("mic".to_string(), (0u64, pcm_ms(4000)));
        let mut emb = MockEmbedder::new(vec![Ok(vec![f32::NAN, 1.0, 0.0, 0.0]), Err(anyhow::anyhow!("boom"))]);
        let stats = build_source_stats(&[&a, &b, &c], &pcm, &mut emb);
        assert!(stats.is_empty(), "NaN/报错/缺轨全部静默跳过");
    }

    #[test]
    fn per_source_cap_with_stable_order() {
        // mic 造 MAX_SEGS_PER_SOURCE+2 个等长段:限额按信道各自生效,
        // 等长时按 seq 升序稳定取,与 NoteStore::load 的重排无关。
        let segs: Vec<SegmentRecord> =
            (0..(MAX_SEGS_PER_SOURCE as u64 + 2)).map(|i| seg(i, "mic", i * 2000, i * 2000 + 2000)).collect();
        let refs: Vec<&SegmentRecord> = segs.iter().collect();
        let mut pcm = BTreeMap::new();
        pcm.insert("mic".to_string(), (0u64, pcm_ms((MAX_SEGS_PER_SOURCE as u64 + 2) * 2000)));
        let mut emb = MockEmbedder::new(vec![Ok(unit(0))]);
        let stats = build_source_stats(&refs, &pcm, &mut emb);
        assert_eq!(stats["mic"].count as usize, MAX_SEGS_PER_SOURCE);
    }

    #[test]
    fn offset_is_respected_when_slicing() {
        let m = seg(0, "mic", 1000, 3000);
        let mut pcm = BTreeMap::new();
        pcm.insert("mic".to_string(), (1000u64, pcm_ms(2000)));
        let mut emb = MockEmbedder::new(vec![Ok(unit(0))]);
        let stats = build_source_stats(&[&m], &pcm, &mut emb);
        assert_eq!(stats["mic"].total_ms, 2000);
    }
}
```

同时 `lib.rs` 加 `mod feedback;`。

- [ ] **Step 2: 跑测试确认失败**

Run: `cd src-tauri && cargo test --lib feedback::`
Expected: 5 个测试 panic 于 `todo!()`。

- [ ] **Step 3: 写实现**

```rust
pub fn build_source_stats(
    segs: &[&SegmentRecord],
    pcm_by_source: &BTreeMap<String, (u64, Vec<f32>)>,
    embedder: &mut dyn SpeakerEmbedder,
) -> BTreeMap<String, SourceStat> {
    // 分信道分组,各自(时长降序,seq 升序)稳定排序后限额——排序键里带 seq
    // 是为了让选择结果与调用方的加载顺序无关(NoteStore::load 会按时间重排)。
    let mut by_source_segs: BTreeMap<&str, Vec<&SegmentRecord>> = BTreeMap::new();
    for s in segs {
        if s.end_ms.saturating_sub(s.start_ms) >= MIN_SEG_MS {
            by_source_segs.entry(s.source.as_str()).or_default().push(s);
        }
    }

    struct Acc {
        sum: Vec<f32>,
        count: u64,
        total_ms: u64,
    }
    let mut out_acc: BTreeMap<String, Acc> = BTreeMap::new();
    for (source, mut list) in by_source_segs {
        let Some((offset_ms, pcm)) = pcm_by_source.get(source) else { continue };
        list.sort_by_key(|s| (std::cmp::Reverse(s.end_ms.saturating_sub(s.start_ms)), s.seq));
        list.truncate(MAX_SEGS_PER_SOURCE);
        for s in list {
            let start = (s.start_ms.saturating_sub(*offset_ms) as usize).saturating_mul(16);
            let end = ((s.end_ms.saturating_sub(*offset_ms) as usize).saturating_mul(16)).min(pcm.len());
            if start >= end {
                continue;
            }
            // 实际切片时长再过一次门:账面 2s 但轨尾截断到几十 ms 的段,嵌入
            // 不稳定且会虚报 total_ms。
            let actual_ms = ((end - start) / 16) as u64;
            if actual_ms < MIN_SEG_MS {
                continue;
            }
            let Ok(vec) = embedder.embed(&pcm[start..end]) else { continue };
            if vec.is_empty() || vec.iter().any(|x| !x.is_finite()) {
                continue;
            }
            let norm: f32 = vec.iter().map(|x| x * x).sum::<f32>().sqrt();
            if norm <= f32::EPSILON {
                continue;
            }
            let acc = out_acc.entry(source.to_string()).or_insert_with(|| Acc {
                sum: vec![0.0; vec.len()],
                count: 0,
                total_ms: 0,
            });
            if acc.sum.len() != vec.len() {
                continue; // 维度漂移只可能是模型异常,弃段
            }
            for (a, b) in acc.sum.iter_mut().zip(&vec) {
                *a += b / norm; // 逐向量归一化后累加(registry 同口径)
            }
            acc.count += 1;
            acc.total_ms += actual_ms;
        }
    }

    out_acc
        .into_iter()
        .filter_map(|(source, acc)| {
            let norm: f32 = acc.sum.iter().map(|x| x * x).sum::<f32>().sqrt();
            if !norm.is_finite() || norm <= f32::EPSILON || acc.count == 0 {
                return None;
            }
            Some((
                source,
                SourceStat {
                    centroid: acc.sum.iter().map(|x| x / norm).collect(),
                    count: acc.count,
                    total_ms: acc.total_ms,
                },
            ))
        })
        .collect()
}
```

- [ ] **Step 4: 跑测试确认通过**

Run: `cd src-tauri && cargo test --lib feedback::`
Expected: 5 PASS。

- [ ] **Step 5: 提交**

```bash
git add src-tauri/src/feedback.rs src-tauri/src/lib.rs
git commit -m "feat(feedback): 纠错回灌纯逻辑核——分信道限额、逐向量归一、实际切片时长门"
```

---

### Task 4: `VoiceprintStore::reinforce_feedback` 专用入库方法(含还原)

不复用 `upsert_from_session`:它是会话净增量 API(count/total_ms/session-centroid/journal 失效语义都假定输入来自 `SpeakerRegistry::snapshot()` 的未上报净增量),历史段回灌不满足契约;且它对悬空人物静默跳过,回灌需要显式报错。

**Files:**
- Modify: `src-tauri/src/store/voiceprints.rs`(新方法 + 测试;复用私有 `merge_centroid` :1069、`push_session_centroid` :726、`vp_guard`)

**Interfaces:**
- Consumes: Task 3 的 `feedback::SourceStat`(以 `(source, centroid, count, total_ms)` 元组形式传入,store 层不依赖 feedback 模块)。
- Produces(Task 5 依赖):

```rust
/// 回灌前后该人的完整序列化快照:纠错还原用「比对 after,未被动过才还原 before」。
pub struct FeedbackApplied {
    pub person_before: String,
    pub person_after: String,
}

impl VoiceprintStore {
    pub fn reinforce_feedback(
        &self,
        person_id: &str,
        stats: &[(String, Vec<f32>, u64, u64)], // (source, centroid, count, total_ms)
        now: &str,
    ) -> anyhow::Result<FeedbackApplied>;

    /// 纠错还原:当前状态仍等于 expected_after 时恢复 before,返回 true;
    /// 已被其它写(新会议/合并/再回灌)动过则不动,返回 false。
    pub fn restore_feedback(
        &self,
        person_id: &str,
        before: &str,
        expected_after: &str,
    ) -> anyhow::Result<bool>;
}
```

- [ ] **Step 1: 写失败测试**(`voiceprints.rs` `mod tests`,构造方式照本文件既有测试)

```rust
#[test]
fn reinforce_feedback_merges_and_snapshots() {
    let dir = tempfile::tempdir().unwrap();
    let store = VoiceprintStore::new(dir.path().to_path_buf());
    // 预置一个人:走既有测试同款构造(upsert_from_session 造 P1)。
    let seeded = store
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
    let pid = seeded.get("S1").unwrap().clone();

    let applied = store
        .reinforce_feedback(&pid, &[("mic".into(), vec![0.0, 1.0, 0.0, 0.0], 2, 4_000)], "2026-08-08T01:00:00+08:00")
        .unwrap();
    let vp = store.load();
    let p = vp.people.get(&pid).unwrap();
    assert_eq!(p.total_ms, 16_000);
    assert_eq!(p.last_seen, "2026-08-08T01:00:00+08:00");
    assert!(p.centroids["mic"].vec[1] > 0.0, "新方向必须并入质心");
    assert_ne!(applied.person_before, applied.person_after);
}

#[test]
fn reinforce_feedback_rejects_unknown_person() {
    let dir = tempfile::tempdir().unwrap();
    let store = VoiceprintStore::new(dir.path().to_path_buf());
    let err = store.reinforce_feedback("P999", &[("mic".into(), vec![1.0], 1, 2_000)], "t");
    assert!(err.is_err(), "悬空人物必须显式报错,不得静默成功");
}

#[test]
fn restore_feedback_only_when_untouched() {
    let dir = tempfile::tempdir().unwrap();
    let store = VoiceprintStore::new(dir.path().to_path_buf());
    let seeded = store
        .upsert_from_session(
            &[crate::diar::registry::ClusterSnapshot {
                id: "S1".into(),
                centroid: vec![1.0, 0.0, 0.0, 0.0],
                count: 4,
                sources: std::collections::BTreeSet::from(["mic".to_string()]),
                person: None,
                total_ms: 12_000,
            }],
            "t0",
        )
        .unwrap();
    let pid = seeded.get("S1").unwrap().clone();
    let applied = store.reinforce_feedback(&pid, &[("mic".into(), vec![0.0, 1.0, 0.0, 0.0], 2, 4_000)], "t1").unwrap();

    // 场景一:未被动过 → 还原成功,total_ms 回到 12_000。
    assert!(store.restore_feedback(&pid, &applied.person_before, &applied.person_after).unwrap());
    assert_eq!(store.load().people.get(&pid).unwrap().total_ms, 12_000);

    // 场景二:重放回灌后又被别的写动过 → 拒绝还原。
    let applied2 = store.reinforce_feedback(&pid, &[("mic".into(), vec![0.0, 1.0, 0.0, 0.0], 2, 4_000)], "t2").unwrap();
    store.reinforce_feedback(&pid, &[("mic".into(), vec![0.0, 0.0, 1.0, 0.0], 1, 2_000)], "t3").unwrap();
    assert!(!store.restore_feedback(&pid, &applied2.person_before, &applied2.person_after).unwrap());
}
```

- [ ] **Step 2: 跑测试确认编译失败**

Run: `cd src-tauri && cargo test --lib reinforce_feedback`
Expected: 编译错误(方法未定义)。

- [ ] **Step 3: 写实现**(加在 `upsert_from_session` 附近)

```rust
/// 纠错回灌(spec P1-2):把人工指认段的嵌入统计并入指定人物。与
/// upsert_from_session 的区别:输入是历史段重嵌入(非会话净增量)、悬空人物
/// 显式报错、返回前后快照供纠错还原。质心/会话质心/时长/last_seen 的并入
/// 口径与会话路径一致;合并建议回执按「此人有纠错回灌」失效(质心动了,
/// 旧建议的相似度数据不再可信——与"又录了新会议"同理)。
pub fn reinforce_feedback(
    &self,
    person_id: &str,
    stats: &[(String, Vec<f32>, u64, u64)],
    now: &str,
) -> anyhow::Result<FeedbackApplied> {
    let _guard = vp_guard();
    let mut vp = self.load();
    let Some(resolved) = Self::resolve(&vp, person_id).map(str::to_string) else {
        anyhow::bail!("未知人物: {person_id}");
    };
    let person = vp.people.get_mut(&resolved).expect("resolve 已校验存在");
    let person_before = serde_json::to_string(person)?;
    for (source, centroid, count, total_ms) in stats {
        if centroid.is_empty() {
            continue;
        }
        merge_centroid(
            person,
            source,
            PersonCentroid { vec: centroid.clone(), count: (*count).max(1), seen: String::new() },
        );
        person.total_ms += total_ms;
        push_session_centroid(person, source, centroid, (*count).max(1), *total_ms, now);
    }
    person.last_seen = now.to_string();
    let person_after = serde_json::to_string(person)?;
    self.save(&vp)?;
    self.journal_invalidate(&[resolved.as_str()], "此人有纠错回灌");
    Ok(FeedbackApplied { person_before, person_after })
}

pub fn restore_feedback(
    &self,
    person_id: &str,
    before: &str,
    expected_after: &str,
) -> anyhow::Result<bool> {
    let _guard = vp_guard();
    let mut vp = self.load();
    let Some(resolved) = Self::resolve(&vp, person_id).map(str::to_string) else {
        return Ok(false); // 人都没了,无从还原,也无需还原
    };
    let person = vp.people.get_mut(&resolved).expect("resolve 已校验存在");
    if serde_json::to_string(person)? != expected_after {
        return Ok(false); // 已被其它写动过:宁可留污染也不覆盖新信息
    }
    *person = serde_json::from_str(before)?;
    self.save(&vp)?;
    self.journal_invalidate(&[resolved.as_str()], "纠错回灌已撤销");
    Ok(true)
}
```

(`journal_invalidate` 的实际签名以本文件现状为准——`upsert_from_session` :718 的调用方式照抄。)

- [ ] **Step 4: 跑测试确认通过**

Run: `cd src-tauri && cargo test --lib voiceprints`
Expected: 全 PASS(既有测试不受影响)。

- [ ] **Step 5: 提交**

```bash
git add src-tauri/src/store/voiceprints.rs
git commit -m "feat(store): reinforce_feedback/restore_feedback——纠错回灌专用入库与条件还原"
```

---

### Task 5: feedback 磁盘壳(账本 + 门禁 + 解码 + 纠错分派)

**Files:**
- Modify: `src-tauri/src/feedback.rs`(追加磁盘壳与账本;测试)

**Interfaces:**
- Consumes: `store::transcode::track_pcm`(:395)、`store::audio::load_audio_meta`、Task 4 的 `reinforce_feedback`/`restore_feedback`、`store::VoiceprintStore`。
- Produces(Task 6 依赖):

```rust
pub enum SegFilter { Speakers(BTreeSet<String>), Seqs(BTreeSet<u64>) }

#[derive(Debug, PartialEq)]
pub enum ReinforceResult {
    Applied { per_source: BTreeMap<String, u64> }, // source -> 嵌入段数
    SkippedModelMismatch,
    SkippedNoSegments,
    SkippedAlreadyDone,
    SkippedUnknownPerson,
}

pub fn reinforce_person(
    note_dir: &Path,
    segs: &[SegmentRecord],
    filter: &SegFilter,
    person_id: &str,
    vp: &crate::store::VoiceprintStore,
    library_model: &str,   // vp.load().embedding_model,调用方读好传入
    expected_model: &str,  // settings.speaker_model
    embedder: &mut dyn SpeakerEmbedder,
    now: &str,
) -> anyhow::Result<ReinforceResult>;
```

- 账本 `feedback.json`(笔记目录,serde default 兼容):

```rust
/// 笔记级回灌账本:幂等(同段集合同人只灌一次)+ 纠错还原凭据。
#[derive(Default, Serialize, Deserialize)]
struct FeedbackLedger {
    /// key = 段集合指纹(排序 seq 的 sha256 前 16 hex)。
    #[serde(default)]
    entries: BTreeMap<String, LedgerEntry>,
}

#[derive(Serialize, Deserialize)]
struct LedgerEntry {
    person_id: String,
    at: String,
    /// 还原凭据(reinforce_feedback 返回的前后快照)。
    before: String,
    after: String,
}
```

- [ ] **Step 1: 写失败测试**(追加;`write_wav` 用 hound,16kHz mono i16)

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

fn seeded_store(root: &std::path::Path) -> (crate::store::VoiceprintStore, String) {
    let store = crate::store::VoiceprintStore::new(root.to_path_buf());
    let seeded = store
        .upsert_from_session(
            &[crate::diar::registry::ClusterSnapshot {
                id: "seed".into(),
                centroid: vec![1.0, 0.0, 0.0, 0.0],
                count: 4,
                sources: std::collections::BTreeSet::from(["mic".to_string()]),
                person: None,
                total_ms: 12_000,
            }],
            "t0",
        )
        .unwrap();
    let pid = seeded.get("seed").unwrap().clone();
    (store, pid)
}

#[test]
fn reinforce_is_idempotent_per_scope_and_person() {
    let note = tempfile::tempdir().unwrap();
    write_wav(&note.path().join("mic.wav"), 4000);
    let vp_root = tempfile::tempdir().unwrap();
    let (store, pid) = seeded_store(vp_root.path());
    let segs = vec![seg(0, "mic", 0, 2000), seg(1, "mic", 2000, 4000)];
    let filter = SegFilter::Speakers(BTreeSet::from(["S1".to_string()]));
    let model = store.load().embedding_model.clone();

    let mut emb = MockEmbedder::new(vec![Ok(unit(1)), Ok(unit(1))]);
    let r1 = reinforce_person(note.path(), &segs, &filter, &pid, &store, &model, &model, &mut emb, "t1").unwrap();
    assert!(matches!(r1, ReinforceResult::Applied { .. }));
    let total_after_first = store.load().people[&pid].total_ms;

    let mut emb2 = MockEmbedder::new(vec![Ok(unit(1)), Ok(unit(1))]);
    let r2 = reinforce_person(note.path(), &segs, &filter, &pid, &store, &model, &model, &mut emb2, "t2").unwrap();
    assert_eq!(r2, ReinforceResult::SkippedAlreadyDone, "同段集合同人重复指认不得重复加权");
    assert_eq!(store.load().people[&pid].total_ms, total_after_first);
}

#[test]
fn correction_restores_previous_person_when_untouched() {
    let note = tempfile::tempdir().unwrap();
    write_wav(&note.path().join("mic.wav"), 4000);
    let vp_root = tempfile::tempdir().unwrap();
    let (store, pid_a) = seeded_store(vp_root.path());
    // 再造一个 B。
    let seeded_b = store
        .upsert_from_session(
            &[crate::diar::registry::ClusterSnapshot {
                id: "seed2".into(),
                centroid: vec![0.0, 0.0, 1.0, 0.0],
                count: 4,
                sources: std::collections::BTreeSet::from(["mic".to_string()]),
                person: None,
                total_ms: 12_000,
            }],
            "t0",
        )
        .unwrap();
    let pid_b = seeded_b.get("seed2").unwrap().clone();

    let segs = vec![seg(0, "mic", 0, 2000), seg(1, "mic", 2000, 4000)];
    let filter = SegFilter::Speakers(BTreeSet::from(["S1".to_string()]));
    let model = store.load().embedding_model.clone();

    let mut emb = MockEmbedder::new(vec![Ok(unit(1)), Ok(unit(1))]);
    reinforce_person(note.path(), &segs, &filter, &pid_a, &store, &model, &model, &mut emb, "t1").unwrap();
    let a_total_before_correction = store.load().people[&pid_a].total_ms;

    // 纠错:同段集合改指 B → A 的上次回灌应被还原(未被动过),B 获得回灌。
    let mut emb2 = MockEmbedder::new(vec![Ok(unit(1)), Ok(unit(1))]);
    let r = reinforce_person(note.path(), &segs, &filter, &pid_b, &store, &model, &model, &mut emb2, "t2").unwrap();
    assert!(matches!(r, ReinforceResult::Applied { .. }));
    assert!(store.load().people[&pid_a].total_ms < a_total_before_correction, "A 的回灌应还原");
    assert_eq!(store.load().people[&pid_a].total_ms, 12_000);
}

#[test]
fn model_mismatch_and_unknown_person_short_circuit() {
    let note = tempfile::tempdir().unwrap();
    let vp_root = tempfile::tempdir().unwrap();
    let (store, _pid) = seeded_store(vp_root.path());
    let segs = vec![seg(0, "mic", 0, 2000)];
    let filter = SegFilter::Speakers(BTreeSet::from(["S1".to_string()]));
    let mut emb = MockEmbedder::new(vec![Ok(unit(0))]);
    // 门禁:严格相等,库是 "campplus"(默认),期望 "eres2netv2" → 跳过。
    let r = reinforce_person(note.path(), &segs, &filter, "P1", &store, "campplus", "eres2netv2", &mut emb, "t").unwrap();
    assert_eq!(r, ReinforceResult::SkippedModelMismatch);
    // 悬空人物:在解码/嵌入之前就短路(嵌入白做还污染账本)。
    write_wav(&note.path().join("mic.wav"), 2000);
    let model = store.load().embedding_model.clone();
    let r2 = reinforce_person(note.path(), &segs, &filter, "P999", &store, &model, &model, &mut emb, "t").unwrap();
    assert_eq!(r2, ReinforceResult::SkippedUnknownPerson);
}
```

- [ ] **Step 2: 跑测试确认编译失败**

Run: `cd src-tauri && cargo test --lib feedback::`
Expected: 编译错误(新符号未定义)。

- [ ] **Step 3: 写实现**

```rust
use std::path::Path;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

const LEDGER_FILE: &str = "feedback.json";

fn scope_key(seqs: &BTreeSet<u64>) -> String {
    let mut h = Sha256::new();
    for s in seqs {
        h.update(s.to_le_bytes());
    }
    hex::encode(&h.finalize()[..8])
}

fn load_ledger(note_dir: &Path) -> FeedbackLedger {
    std::fs::read_to_string(note_dir.join(LEDGER_FILE))
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_default()
}

fn save_ledger(note_dir: &Path, ledger: &FeedbackLedger) -> anyhow::Result<()> {
    let tmp = note_dir.join(format!("{LEDGER_FILE}.tmp"));
    std::fs::write(&tmp, serde_json::to_vec_pretty(ledger)?)?;
    std::fs::rename(&tmp, note_dir.join(LEDGER_FILE))?;
    Ok(())
}

pub fn reinforce_person(
    note_dir: &Path,
    segs: &[SegmentRecord],
    filter: &SegFilter,
    person_id: &str,
    vp: &crate::store::VoiceprintStore,
    library_model: &str,
    expected_model: &str,
    embedder: &mut dyn SpeakerEmbedder,
    now: &str,
) -> anyhow::Result<ReinforceResult> {
    // 门禁:与种子注入同一严格语义(lib.rs:229-236)——不同模型向量空间不可比。
    if library_model != expected_model {
        return Ok(ReinforceResult::SkippedModelMismatch);
    }
    let wanted: Vec<&SegmentRecord> = segs
        .iter()
        .filter(|s| match filter {
            SegFilter::Speakers(ids) => s.speaker.as_ref().map_or(false, |sp| ids.contains(sp)),
            SegFilter::Seqs(seqs) => seqs.contains(&s.seq),
        })
        .collect();
    if wanted.is_empty() {
        return Ok(ReinforceResult::SkippedNoSegments);
    }
    {
        let lib = vp.load();
        if crate::store::VoiceprintStore::resolve(&lib, person_id).is_none() {
            return Ok(ReinforceResult::SkippedUnknownPerson);
        }
    }

    let seq_set: BTreeSet<u64> = wanted.iter().map(|s| s.seq).collect();
    let key = scope_key(&seq_set);
    let mut ledger = load_ledger(note_dir);
    if let Some(prev) = ledger.entries.get(&key) {
        if prev.person_id == person_id {
            return Ok(ReinforceResult::SkippedAlreadyDone);
        }
        // 纠错:上一次灌错了人。未被动过就还原;动过则宁留污染不覆盖新信息。
        match vp.restore_feedback(&prev.person_id, &prev.before, &prev.after) {
            Ok(true) => eprintln!("feedback: 已还原 {} 的上次回灌(纠错)", prev.person_id),
            Ok(false) => eprintln!("feedback: {} 已被其它写动过,跳过还原", prev.person_id),
            Err(e) => eprintln!("feedback: 还原失败(忽略): {e}"),
        }
        ledger.entries.remove(&key);
    }

    // 解码涉及的轨(失败只收窄:回灌拿得到多少信号用多少)。
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

    let stats = build_source_stats(&wanted, &pcm_by_source, embedder);
    if stats.is_empty() {
        return Ok(ReinforceResult::SkippedNoSegments);
    }
    let tuples: Vec<(String, Vec<f32>, u64, u64)> =
        stats.iter().map(|(s, st)| (s.clone(), st.centroid.clone(), st.count, st.total_ms)).collect();
    let applied = vp.reinforce_feedback(person_id, &tuples, now)?;
    ledger.entries.insert(
        key,
        LedgerEntry { person_id: person_id.to_string(), at: now.to_string(), before: applied.person_before, after: applied.person_after },
    );
    if let Err(e) = save_ledger(note_dir, &ledger) {
        eprintln!("feedback: 账本写入失败(下次可能重复回灌): {e}");
    }
    Ok(ReinforceResult::Applied { per_source: stats.iter().map(|(s, st)| (s.clone(), st.count)).collect() })
}
```

依赖说明:`sha2`/`hex` 若不在 `Cargo.toml`,改用 `std::hash`(`DefaultHasher` 两轮不同种子拼 16 hex)——不为账本引新依赖;实现时以 `grep -E '^(sha2|hex)' src-tauri/Cargo.toml` 结果为准。

- [ ] **Step 4: 跑测试确认通过**

Run: `cd src-tauri && cargo test --lib feedback::`
Expected: 8 PASS(Task 3 的 5 条 + 本任务 3 条)。

- [ ] **Step 5: 提交**

```bash
git add src-tauri/src/feedback.rs src-tauri/Cargo.toml
git commit -m "feat(feedback): 磁盘壳——幂等账本、纠错还原、模型门禁、分轨解码回灌"
```

---

### Task 6: 指认 IPC 挂钩(同步快照 + 分派 + 异步嵌入)

**Files:**
- Modify: `src-tauri/src/lib.rs`(`assign_note_speaker_person` :2594、`assign_refined_person` :3135、新 helper)
- Modify: `src-tauri/src/feedback.rs`(新增纯函数 `plan_action` + 测试)

**Interfaces:**
- Consumes: `store::NoteStore::load`(:120,`Note{segments, speakers}`)、`store::load_refined`(`store/refined.rs`,签名以现状为准)、`VoiceprintStore::merge_journaled(loser, winner, embedder, origin, similarity, now)`(:317)、`settings::load(&app_data_dir).speaker_model`、`diar::SherpaEmbedder::new(&speaker_model_path(&app))`(:701)、`open_voiceprint_store`(:3417)。
- Produces:

```rust
/// 指认后的回灌分派决策(纯函数,可单测;挂钩壳只做 IO)。
#[derive(Debug, PartialEq)]
pub enum FeedbackAction {
    /// 先前关联的是**无名**自动人物:journaled 合并 prior→target。质心已在库,
    /// 合并比重嵌入干净(不重复计时长),且可经收件箱撤销/拆回。
    MergePrior { prior: String },
    /// 无先前关联,或先前是有名人物(用户纠正认错):段重嵌入回灌 target。
    /// 有名 prior 在停录时并入的净增量无法在 P1 撤销(spec P2 identify_journal)。
    Reinforce,
    /// 已指认给同一人:无事可做。
    Noop,
}

pub fn plan_action(prior: Option<(&str, &str)>, target: &str) -> FeedbackAction; // prior=(resolve后id, name)
```

- [ ] **Step 1: 写失败测试**(`feedback.rs` `mod tests`)

```rust
#[test]
fn plan_action_dispatches_by_prior_state() {
    assert_eq!(plan_action(None, "P2"), FeedbackAction::Reinforce);
    assert_eq!(plan_action(Some(("P2", "张伟")), "P2"), FeedbackAction::Noop);
    assert_eq!(
        plan_action(Some(("P7", "")), "P2"),
        FeedbackAction::MergePrior { prior: "P7".into() },
        "无名自动人物并入目标,不重嵌入"
    );
    assert_eq!(plan_action(Some(("P7", "李雷")), "P2"), FeedbackAction::Reinforce, "有名先前人物=纠错,只灌新人");
}
```

- [ ] **Step 2: 跑测试确认失败 → 写实现**

```rust
pub fn plan_action(prior: Option<(&str, &str)>, target: &str) -> FeedbackAction {
    match prior {
        Some((id, _)) if id == target => FeedbackAction::Noop,
        Some((id, name)) if name.trim().is_empty() => FeedbackAction::MergePrior { prior: id.to_string() },
        _ => FeedbackAction::Reinforce,
    }
}
```

Run: `cd src-tauri && cargo test --lib plan_action`
Expected: PASS。

- [ ] **Step 3: lib.rs 挂钩壳**

新增(嵌入并发上闸 + 后台执行;**指认时同步取好段快照与 prior**,后台不再回读笔记,避免"稍后状态"漂移):

```rust
/// 回灌互斥门:同一时刻最多一个回灌在嵌入。不借用 embedder_cache(开录会
/// take 走它,回灌不能卡住开录,也不能被开录饿死),自建临时嵌入器,靠此门
/// 保证 ORT 并发最多 +1。
static FEEDBACK_GATE: Mutex<()> = Mutex::new(());

fn spawn_feedback(app: &AppHandle, note_id: String, segs: Vec<store::SegmentRecord>, filter: feedback::SegFilter, prior: Option<(String, String)>, target: String) {
    let app = app.clone();
    tauri::async_runtime::spawn_blocking(move || {
        let run = || -> anyhow::Result<()> {
            let vp = open_voiceprint_store(&app).map_err(anyhow::Error::msg)?;
            let now = chrono::Local::now().to_rfc3339();
            let action = feedback::plan_action(prior.as_ref().map(|(i, n)| (i.as_str(), n.as_str())), &target);
            match action {
                feedback::FeedbackAction::Noop => Ok(()),
                feedback::FeedbackAction::MergePrior { prior } => {
                    let _gate = FEEDBACK_GATE.lock().unwrap();
                    // 无名自动人物并入目标:journaled,可在收件箱撤销/拆回。
                    let receipt = vp.merge_journaled(&prior, &target, None, "feedback-assign", None, &now)?;
                    eprintln!("feedback: 无名先前人物 {prior} 已并入 {target}(回执 {receipt})");
                    Ok(())
                }
                feedback::FeedbackAction::Reinforce => {
                    let _gate = FEEDBACK_GATE.lock().unwrap();
                    let dir = notes_dir(&app)?.join(&note_id);
                    let expected = app
                        .path()
                        .app_data_dir()
                        .map(|d| settings::load(&d).speaker_model)
                        .unwrap_or_default();
                    let library_model = vp.load().embedding_model.clone();
                    let mut embedder = diar::SherpaEmbedder::new(&speaker_model_path(&app))?;
                    let r = feedback::reinforce_person(&dir, &segs, &filter, &target, &vp, &library_model, &expected, &mut embedder, &now)?;
                    eprintln!("feedback: note={note_id} target={target} result={r:?}");
                    Ok(())
                }
            }
        };
        if let Err(e) = run() {
            eprintln!("feedback: 回灌失败(不影响指认) note={note_id}: {e}");
        }
    });
}
```

`assign_note_speaker_person`(:2594)在 `request(...)` 成功后追加(需要 prior:请求**前**从 `NoteStore::load` 的 speakers 表读旧 `person_id` 并 resolve 出 `(id, name)`;段快照同这次 load):

```rust
    let dir = notes_dir(&app).map_err(|e| e.to_string())?;
    let note = store::NoteStore::new(dir).load(&note_id).map_err(|e| e.to_string())?;
    let prior = note
        .speakers
        .get(&speaker_id)
        .and_then(|m| m.person_id.as_deref())
        .and_then(|pid| store::VoiceprintStore::resolve(&vp, pid))
        .map(|rid| (rid.to_string(), vp.people.get(rid).map(|p| p.name.clone()).unwrap_or_default()));
    // …(原 request(...) 调用,`?` 之后)
    spawn_feedback(
        &app,
        note_id_for_feedback,
        note.segments,
        feedback::SegFilter::Speakers(std::collections::BTreeSet::from([speaker_id_for_feedback])),
        prior,
        resolved,
    );
    Ok(())
```

(命令开头已 `load()` 过 vp:复用同一份;`note_id`/`speaker_id` 被 move 进 EditOp,提前 clone 出 `*_for_feedback`。)

`assign_refined_person`(:3135)在 `store::assign_refined_person(...)` 成功后追加:

```rust
    // R 段落的 source_seqs 已显式落盘:收集该 R 号全部原始 seq 供回灌;
    // prior 取该 R 号段落现有 person_id(任一段即可,全段同值)。
    if let Some(doc) = store::load_refined(&dir) {
        let paras: Vec<_> = doc.paragraphs.iter().filter(|p| p.speaker == speaker_id).collect();
        let seqs: std::collections::BTreeSet<u64> = paras.iter().flat_map(|p| p.source_seqs.iter().copied()).collect();
        if !seqs.is_empty() {
            let prior = paras
                .iter()
                .find_map(|p| p.person_id.as_deref())
                .and_then(|pid| store::VoiceprintStore::resolve(&vp, pid))
                .map(|rid| (rid.to_string(), vp.people.get(rid).map(|p| p.name.clone()).unwrap_or_default()));
            let root = notes_dir(&app).map_err(|e| e.to_string())?;
            let note = store::NoteStore::new(root).load(&note_id).map_err(|e| e.to_string())?;
            spawn_feedback(&app, note_id.clone(), note.segments, feedback::SegFilter::Seqs(seqs), prior, resolved.clone());
        }
    }
    Ok(())
```

注意:此处 `load_refined` 读的是**指认写入后**的稿——prior 必须在 `store::assign_refined_person(...)` 调用**之前**读取(把上面的读取块整体移到写入前,持 `doc`/`prior`/`seqs` 再执行写入)。Step 按此顺序实现。

- [ ] **Step 4: 编译与回归(如实说明:两个命令壳无自动化测试——Tauri 命令依赖 AppHandle,仓库现状也未为它们建测试;分派逻辑已在 `plan_action` 单测覆盖,壳的验证走真机冒烟清单)**

Run: `cd src-tauri && cargo test --lib && cargo clippy --all-targets`
Expected: 测试全 PASS;clippy 退出码 0(完整输出查看,不许管道截断)。

- [ ] **Step 5: 提交**

```bash
git add src-tauri/src/lib.rs src-tauri/src/feedback.rs
git commit -m "feat(feedback): 指认命令挂钩——同步快照、无名先前人物走 journaled 合并、异步限流嵌入"
```

---

### Task 7: 评测工具链(S/R 双层真值 + 评测 bin)

**Files:**
- Create: `src-tauri/src/bin/speaker_eval.rs`(`src/bin/` 自动发现,Cargo.toml 预计零改动)

**Interfaces:**
- Consumes: 只读磁盘 JSON(`speakers.json`、`aing.json`(修订稿;若无则回退 legacy `refined.json`)、`voiceprints.json`),serde_json::Value,**不 use 库 crate**。
- Produces: 真值文件格式(P2a 沿用):JSONL,每行 `{"note_id":"...","speaker_id":"S3"|"R2","person":"P12"|"张伟"|""}`。
  - `speaker_id` 以 `R` 开头 → 比对修订稿段落关联;否则比对 `speakers.json`。
  - `person` 形如 `^P\d+$` → 按 **person_id**(经 redirects 归一)比对——改名/同名/别名都不受影响,**优先使用**;否则按库中人名比对;空串 = 「标注过、确认无法归属」,此时预测出任何非空人物计**误认**。
  - 重复 `(note_id, speaker_id)` 或缺字段 → 整体报错退出(损坏标注不许进分母)。

- [ ] **Step 1: 写失败测试(纯函数)**

```rust
//! 说话人识别评测(spec rev2「测试与评测」)。P1 评的是纯声学基线
//! (S/R 簇的库关联 vs 人工真值);P2a 起 identify 的分档结果落
//! identify.json 后,本工具按同一真值格式扩展分档查准/查全——当前
//! 版本先给出整体 正确/误认/未识别 与 precision/recall。
//! 只读 JSON、不依赖库 crate:评测工具必须在应用任何状态下可用。

use std::collections::BTreeMap;

use serde_json::Value;

/// redirects 归一,与 store::VoiceprintStore::resolve 同语义(0..8 跳,防环)。
fn resolve<'a>(
    redirects: &'a BTreeMap<String, String>,
    people: &BTreeMap<String, Value>,
    id: &'a str,
) -> Option<&'a str> {
    let mut cur = id;
    for _ in 0..8 {
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
    labeled: usize,
    correct: usize,
    wrong: usize,
    unassigned: usize,
}

impl Metrics {
    /// precision = 正确 / 所有做出的非空预测;recall = 正确 / 全部标注。
    fn precision(&self) -> f64 {
        let made = self.correct + self.wrong;
        if made == 0 { 0.0 } else { self.correct as f64 / made as f64 }
    }
    fn recall(&self) -> f64 {
        if self.labeled == 0 { 0.0 } else { self.correct as f64 / self.labeled as f64 }
    }
}

/// truth: (note_id, speaker_id, want);predicted: (note,spk) -> (person_id, name)。
/// want 为 P id 时按 id 比;为名字时按 name 比;为空时任何非空预测都是误认。
fn score(
    truth: &[(String, String, String)],
    predicted: &BTreeMap<(String, String), (String, String)>,
) -> Metrics {
    let mut m = Metrics::default();
    for (note, spk, want) in truth {
        m.labeled += 1;
        let got = predicted.get(&(note.clone(), spk.clone()));
        let (got_id, got_name) = match got {
            Some((i, n)) if !i.is_empty() || !n.is_empty() => (i.as_str(), n.as_str()),
            _ => {
                if want.is_empty() {
                    m.correct += 1; // 真值「无法归属」且模型也没认:正确的保守
                } else {
                    m.unassigned += 1;
                }
                continue;
            }
        };
        if want.is_empty() {
            m.wrong += 1; // 真值明确无法归属,模型强行认人:最危险的错误
        } else if is_pid(want) {
            if got_id == want { m.correct += 1 } else { m.wrong += 1 }
        } else if got_name == want {
            m.correct += 1
        } else {
            m.wrong += 1
        }
    }
    m
}

fn is_pid(s: &str) -> bool {
    s.strip_prefix('P').is_some_and(|r| !r.is_empty() && r.bytes().all(|b| b.is_ascii_digit()))
}

fn main() {
    todo!()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_follows_redirects_with_hop_cap() {
        let mut redirects = BTreeMap::new();
        redirects.insert("P1".to_string(), "P2".to_string());
        redirects.insert("P2".to_string(), "P3".to_string());
        let mut people = BTreeMap::new();
        people.insert("P3".to_string(), Value::Null);
        assert_eq!(resolve(&redirects, &people, "P1"), Some("P3"));
        assert_eq!(resolve(&redirects, &people, "P9"), None);
        // 自环不挂死。
        let mut looped = BTreeMap::new();
        looped.insert("PX".to_string(), "PX".to_string());
        assert_eq!(resolve(&looped, &BTreeMap::new(), "PX"), None);
    }

    #[test]
    fn score_covers_pid_name_empty_truth_cases() {
        let truth = vec![
            ("n".into(), "S1".into(), "P3".into()),   // pid 命中
            ("n".into(), "S2".into(), "张伟".into()), // 名字误认
            ("n".into(), "S3".into(), "".into()),      // 真值无归属,模型强行认 → wrong
            ("n".into(), "S4".into(), "李雷".into()), // 无预测 → unassigned
            ("n".into(), "S5".into(), "".into()),      // 真值无归属,模型也没认 → correct
        ];
        let mut p = BTreeMap::new();
        p.insert(("n".to_string(), "S1".to_string()), ("P3".to_string(), "王五".to_string()));
        p.insert(("n".to_string(), "S2".to_string()), ("P9".to_string(), "赵六".to_string()));
        p.insert(("n".to_string(), "S3".to_string()), ("P1".to_string(), "钱七".to_string()));
        let m = score(&truth, &p);
        assert_eq!(m, Metrics { labeled: 5, correct: 2, wrong: 2, unassigned: 1 });
        assert!((m.precision() - 0.5).abs() < 1e-9);
        assert!((m.recall() - 0.4).abs() < 1e-9);
    }

    #[test]
    fn is_pid_rejects_names_and_partials() {
        assert!(is_pid("P12"));
        assert!(!is_pid("P"));
        assert!(!is_pid("P12a"));
        assert!(!is_pid("张伟"));
    }
}
```

- [ ] **Step 2: 跑测试**

Run: `cd src-tauri && cargo test --bin speaker_eval`
Expected: 3 PASS。

- [ ] **Step 3: 补 main/init/run**

```rust
fn main() {
    let args: Vec<String> = std::env::args().collect();
    let code = match args.get(1).map(String::as_str) {
        Some("init") if args.len() == 4 => init(&args[2], &args[3]),
        Some("run") if args.len() == 5 => run(&args[2], &args[3], &args[4]),
        _ => {
            eprintln!(
                "用法:\n  speaker_eval init <notes_dir> <note_id>\n  speaker_eval run <notes_dir> <voiceprints.json> <truth.jsonl>"
            );
            2
        }
    };
    std::process::exit(code);
}

fn read_json(path: &std::path::Path) -> Option<Value> {
    serde_json::from_str(&std::fs::read_to_string(path).ok()?).ok()
}

/// 标注模板:S 层逐说话人一行(含 segments 里出现但缺 metadata 的孤儿 id),
/// R 层逐修订稿说话人一行;# 注释行给名字线索与首段文本。
fn init(notes_dir: &str, note_id: &str) -> i32 {
    let dir = std::path::Path::new(notes_dir).join(note_id);
    let speakers: BTreeMap<String, Value> = read_json(&dir.join("speakers.json"))
        .and_then(|v| serde_json::from_value(v).ok())
        .unwrap_or_default();
    let segments = std::fs::read_to_string(dir.join("segments.jsonl")).unwrap_or_default();
    let seg_rows: Vec<Value> = segments.lines().filter_map(|l| serde_json::from_str(l).ok()).collect();
    // S 层:speakers.json ∪ segments 里的孤儿 speaker。
    let mut s_ids: Vec<String> = speakers.keys().cloned().collect();
    for v in &seg_rows {
        if let Some(sp) = v["speaker"].as_str() {
            if !s_ids.iter().any(|x| x == sp) {
                s_ids.push(sp.to_string());
            }
        }
    }
    for sid in &s_ids {
        let name = speakers.get(sid).and_then(|m| m["name"].as_str()).unwrap_or("");
        let sample = seg_rows
            .iter()
            .find(|v| v["speaker"].as_str() == Some(sid))
            .and_then(|v| v["text"].as_str())
            .map(|s| s.chars().take(40).collect::<String>())
            .unwrap_or_default();
        println!("# {sid} name={name} 首段:{sample}");
        println!(r#"{{"note_id":"{note_id}","speaker_id":"{sid}","person":""}}"#);
    }
    // R 层(有修订稿才有):aing.json 优先,legacy refined.json 兜底。
    let doc = read_json(&dir.join("aing.json")).or_else(|| read_json(&dir.join("refined.json")));
    if let Some(doc) = doc {
        let mut seen = std::collections::BTreeSet::new();
        for p in doc["paragraphs"].as_array().unwrap_or(&vec![]) {
            let Some(r) = p["speaker"].as_str() else { continue };
            if !seen.insert(r.to_string()) {
                continue;
            }
            let name = p["name"].as_str().unwrap_or("");
            let sample = p["text"].as_str().map(|s| s.chars().take(40).collect::<String>()).unwrap_or_default();
            println!("# {r} name={name} 首段:{sample}");
            println!(r#"{{"note_id":"{note_id}","speaker_id":"{r}","person":""}}"#);
        }
    }
    0
}

fn run(notes_dir: &str, vp_path: &str, truth_path: &str) -> i32 {
    let Some(vp) = read_json(std::path::Path::new(vp_path)) else {
        eprintln!("voiceprints.json 读取失败");
        return 2;
    };
    let people: BTreeMap<String, Value> = serde_json::from_value(vp["people"].clone()).unwrap_or_default();
    let redirects: BTreeMap<String, String> = serde_json::from_value(vp["redirects"].clone()).unwrap_or_default();

    // 解析真值:缺字段/重复键直接失败——损坏标注进分母比报错更糟。
    let mut truth: Vec<(String, String, String)> = Vec::new();
    let mut keys = std::collections::BTreeSet::new();
    let content = match std::fs::read_to_string(truth_path) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("truth 读取失败: {e}");
            return 2;
        }
    };
    for (ln, line) in content.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Ok(v) = serde_json::from_str::<Value>(line) else {
            eprintln!("truth 第 {} 行非法 JSON", ln + 1);
            return 2;
        };
        let (Some(note), Some(spk), Some(person)) =
            (v["note_id"].as_str(), v["speaker_id"].as_str(), v["person"].as_str())
        else {
            eprintln!("truth 第 {} 行缺字段(note_id/speaker_id/person)", ln + 1);
            return 2;
        };
        if !keys.insert((note.to_string(), spk.to_string())) {
            eprintln!("truth 第 {} 行重复标注 ({note},{spk})", ln + 1);
            return 2;
        }
        truth.push((note.into(), spk.into(), person.into()));
    }

    // 按 note 分组各读一次;S 层读 speakers.json,R 层读 aing.json/refined.json。
    let mut predicted: BTreeMap<(String, String), (String, String)> = BTreeMap::new();
    let mut by_note: BTreeMap<&str, Vec<&(String, String, String)>> = BTreeMap::new();
    for t in &truth {
        by_note.entry(t.0.as_str()).or_default().push(t);
    }
    for (note_id, rows) in by_note {
        let dir = std::path::Path::new(notes_dir).join(note_id);
        let speakers: BTreeMap<String, Value> = read_json(&dir.join("speakers.json"))
            .and_then(|v| serde_json::from_value(v).ok())
            .unwrap_or_default();
        let doc = read_json(&dir.join("aing.json")).or_else(|| read_json(&dir.join("refined.json")));
        for (_, spk, _) in rows {
            let pid = if spk.starts_with('R') {
                doc.as_ref().and_then(|d| {
                    d["paragraphs"]
                        .as_array()?
                        .iter()
                        .find(|p| p["speaker"].as_str() == Some(spk))
                        .and_then(|p| p["person_id"].as_str().map(str::to_string))
                })
            } else {
                speakers.get(spk).and_then(|m| m["person_id"].as_str().map(str::to_string))
            };
            let Some(pid) = pid else { continue };
            let Some(rid) = resolve(&redirects, &people, &pid) else { continue };
            let name = people[rid]["name"].as_str().unwrap_or("").to_string();
            predicted.insert((note_id.to_string(), spk.clone()), (rid.to_string(), name));
        }
    }

    let m = score(&truth, &predicted);
    println!("标注 {}  正确 {}  误认 {}  未识别 {}", m.labeled, m.correct, m.wrong, m.unassigned);
    println!(
        "precision {:.1}%  recall {:.1}%(P2b 验收线看 precision;误认是最危险错误)",
        100.0 * m.precision(),
        100.0 * m.recall()
    );
    0
}
```

- [ ] **Step 4: 端到端自动测试**(同文件 tests:tempdir 造最小 speakers.json/segments.jsonl/aing.json/voiceprints.json/truth,直接调 `init`/`run` 函数断言退出码;stdout 内容不断言,格式自由)

```rust
#[test]
fn run_end_to_end_on_fixture() {
    let root = tempfile::tempdir().unwrap();
    let note = root.path().join("n1");
    std::fs::create_dir_all(&note).unwrap();
    std::fs::write(note.join("speakers.json"), r#"{"S1":{"name":"","sources":["mic"],"centroid":[],"count":1,"person_id":"P1"}}"#).unwrap();
    std::fs::write(note.join("segments.jsonl"), r#"{"seq":0,"source":"mic","text":"你好","start_ms":0,"end_ms":2000,"speaker":"S1"}"#).unwrap();
    let vp = root.path().join("voiceprints.json");
    std::fs::write(&vp, r#"{"schema_version":1,"next_person":2,"people":{"P1":{"name":"张伟","centroids":{},"session_centroids":{},"total_ms":0,"last_seen":""}},"redirects":{},"embedding_model":"campplus"}"#).unwrap();
    let truth = root.path().join("truth.jsonl");
    std::fs::write(&truth, format!("{}\n", r#"{"note_id":"n1","speaker_id":"S1","person":"P1"}"#)).unwrap();
    assert_eq!(run(root.path().to_str().unwrap(), vp.to_str().unwrap(), truth.to_str().unwrap()), 0);
    // 重复标注必须整体报错。
    std::fs::write(&truth, format!("{0}\n{0}\n", r#"{"note_id":"n1","speaker_id":"S1","person":"P1"}"#)).unwrap();
    assert_eq!(run(root.path().to_str().unwrap(), vp.to_str().unwrap(), truth.to_str().unwrap()), 2);
}
```

注:`tempfile` 是 dependencies(非 dev-only)即可在 bin 测试用;若仅 dev-dependencies,bin 的 `#[cfg(test)]` 同样可用(dev-dependencies 对 bin 测试生效)。fixture JSON 字段以真实 serde 结构为准,实现时如反序列化失败按真实字段修正。

Run: `cd src-tauri && cargo test --bin speaker_eval`
Expected: 4 PASS。

- [ ] **Step 5: 真机手测 + 提交**

```bash
cd src-tauri
cargo run --bin speaker_eval -- init "<数据目录>/notes" <某笔记id> > /tmp/truth-template.jsonl
# 人工填 person 后:
cargo run --bin speaker_eval -- run "<数据目录>/notes" "<数据目录>/voiceprints.json" /tmp/truth-template.jsonl
git add src-tauri/src/bin/speaker_eval.rs
git commit -m "feat(eval): 说话人归属评测——S/R 双层真值、person_id 优先、precision/recall"
```

---

## 收尾核对(整计划完成后)

- [ ] `cd src-tauri && cargo test` 全绿、`cargo clippy --all-targets` 退出码 0(完整输出检视)
- [ ] 真机冒烟(留待用户,列入 PR 描述):① 指认无名说话人给库中人物 → 日志 `无名先前人物 … 已并入`,收件箱出现可撤销回执;② 对无关联说话人指认 → 日志 `result=Applied`;重复指认同一人 → `SkippedAlreadyDone`;③ 指认后改指他人 → 日志出现 `已还原 … 的上次回灌`;④ 切换嵌入模型后指认 → `SkippedModelMismatch`;⑤ 精修多人笔记,AI 日志请求体含 `speaker=` 标注且正文无标签泄漏
- [ ] PR 描述注明:本期为 spec P1;有名先前人物的历史污染不可撤销为已知限制(P2 identify_journal 解决);P2a 另立计划
