# 语言幻觉过滤 + 段级 RMS 埋点 实施计划

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 日/韩语幻觉 final 段从源头丢弃(不 embed/不落盘/不污染说话人表);每个落盘段带 rms 诊断字段,为后续能量门槛攒数据。

**Architecture:** 三层贯通:asr 层 `Transcript` 增 lang 透传 → store 层 `SegmentRecord`/`append_final` 增 rms → session worker 在 recognize 后、处理链前做语言过滤并计算 rms。过滤落点与既有 ECHO 命中丢弃同层(零副作用先例)。

**Tech Stack:** Rust(sherpa-rs 0.6.8 `OfflineRecognizerResult.lang`)+ serde 向后兼容字段。

**Spec:** `docs/superpowers/specs/2026-07-04-voice-notes-lang-filter-rms-design.md`

## Global Constraints

- 分支 `lang-filter-rms`(已建),单 PR squash 合入。
- 注释风格:中文、讲"为什么",不复述代码。
- cargo test 全过,npm check 0/0,无新警告。
- "[识别失败]" 占位段绝不能被语言过滤误杀(tag 空、占比 0 → 天然放行,测试锁死)。
- 旧笔记 segments.jsonl(无 rms 字段)反序列化必须不破(serde default)。
- partial 路径不做语言过滤(转瞬即逝)。

---

### Task 1: Transcript.lang 透传 + 语言判定纯函数

**Files:**
- Modify: `src-tauri/src/asr/mod.rs:6-8`(Transcript 增 lang + Default)
- Modify: `src-tauri/src/asr/sense_voice.rs:37`(透传 result.lang)
- Modify: `src-tauri/src/session.rs`(判定函数 + 常量 + 单测;6 处测试 mock Transcript 构造补默认)
- Modify: `src-tauri/src/store/writer.rs:488,859`(2 处测试 mock 同上)

**Interfaces:**
- Produces: `Transcript { text: String, lang: String }`(derive Default);`session.rs` 模块级 `fn is_foreign_final(lang: &str, text: &str) -> bool` 与 `const FOREIGN_RATIO_THRESHOLD: f32 = 0.3`。Task 3 在 worker 内调用 `is_foreign_final`。

- [ ] **Step 1: 写失败测试**(session.rs tests 模块)

```rust
    #[test]
    fn foreign_final_detection() {
        // 模型标签命中(sherpa 原样格式与裸格式都认)
        assert!(is_foreign_final("<|ja|>", "任意文本"));
        assert!(is_foreign_final("ko", "任意文本"));
        assert!(!is_foreign_final("<|zh|>", "正常中文"));
        assert!(!is_foreign_final("en", "hello world"));
        // 字符占比兜底(标签缺失时)
        assert!(is_foreign_final("", "でかし"), "纯假名");
        assert!(is_foreign_final("", "美国のポ調スパ"), "假名混杂占比过阈");
        assert!(is_foreign_final("", "안녕하세요"), "谚文");
        assert!(!is_foreign_final("", "中英 mixed 正常句子 ok"), "中英混合放行");
        assert!(!is_foreign_final("", "純漢字幻覺讀作中文"), "纯汉字不拦(无损)");
        assert!(!is_foreign_final("", "[识别失败]"), "占位段绝不误杀");
        assert!(!is_foreign_final("", ""), "空串放行");
    }
```

- [ ] **Step 2: 跑测试确认编译失败**

Run: `cd src-tauri && cargo test foreign_final_detection`
Expected: 编译错误 `cannot find function is_foreign_final`。

- [ ] **Step 3: 实现**

asr/mod.rs(Transcript 定义替换):

```rust
#[derive(Debug, Clone, Default)]
pub struct Transcript {
    pub text: String,
    /// 模型判定的语言标签(SenseVoice 经 sherpa 输出如 "<|zh|>";其它模型/mock 可为空)。
    pub lang: String,
}
```

sense_voice.rs:37:

```rust
        Ok(Transcript { text: result.text, lang: result.lang })
```

session.rs(`text_prefix20` 附近):

```rust
/// 字符占比兜底的阈值:字母类字符中假名/谚文超三成即视为外语幻觉。
const FOREIGN_RATIO_THRESHOLD: f32 = 0.3;

/// 语言白名单过滤(会议场景仅中英):模型标签为日/韩,或文本假名/谚文占比过阈 → 外语
/// 幻觉段。SenseVoice 短段常把 AEC 残渣误判成日语;此类段漏过文本回声去重(残渣文
/// 本与 system 段不相似)且会开出垃圾说话人,须在处理链之前整段丢弃。
/// 纯汉字的日语幻觉读作中文,不拦(无损);占位段/空串占比为 0,天然放行。
fn is_foreign_final(lang: &str, text: &str) -> bool {
    let tag: String = lang
        .trim_matches(|c: char| c == '<' || c == '|' || c == '>')
        .to_ascii_lowercase();
    if tag == "ja" || tag == "ko" {
        return true;
    }
    let (mut letters, mut foreign) = (0u32, 0u32);
    for c in text.chars() {
        if !c.is_alphabetic() {
            continue;
        }
        letters += 1;
        let u = c as u32;
        let is_kana = (0x3040..=0x30FF).contains(&u) || (0x31F0..=0x31FF).contains(&u);
        let is_hangul = (0xAC00..=0xD7AF).contains(&u)
            || (0x1100..=0x11FF).contains(&u)
            || (0x3130..=0x318F).contains(&u);
        if is_kana || is_hangul {
            foreign += 1;
        }
    }
    letters > 0 && foreign as f32 / letters as f32 > FOREIGN_RATIO_THRESHOLD
}
```

8 处测试 mock 构造(session.rs:565,576,645,887,1131,1146 与 writer.rs:488,859)统一改为补默认,例:

```rust
            Ok(Transcript { text: format!("len={}", s.len()), ..Default::default() })
```

- [ ] **Step 4: 跑测试**

Run: `cd src-tauri && cargo test foreign_final_detection && cargo build 2>&1 | tail -3`
Expected: PASS;build 无新警告。

- [ ] **Step 5: Commit**

```bash
git add src-tauri/src/asr/mod.rs src-tauri/src/asr/sense_voice.rs src-tauri/src/session.rs src-tauri/src/store/writer.rs
git commit -m "feat(asr): Transcript 透传语言标签,session 增外语幻觉判定(标签+字符占比双保险)"
```

---

### Task 2: SegmentRecord.rms 字段贯通(store 层)

**Files:**
- Modify: `src-tauri/src/store/mod.rs`(SegmentRecord 增 rms)
- Modify: `src-tauri/src/store/writer.rs`(append_final 签名增参;本文件测试调用点补 None)
- Modify: `src-tauri/src/store/notes.rs`、`src-tauri/src/store/export.rs`(测试里 append_final 调用与 SegmentRecord 字面量补默认)
- Modify: `src-tauri/src/lib.rs`(on_final 落盘闭包调用点先传 None,Task 3 换真值)
- Modify: `src/lib/notes.ts`(TS 类型加可选 rms)

**Interfaces:**
- Consumes: 既有 `append_final(&mut self, source, text, start_ms, end_ms, speaker) `。
- Produces: `append_final(&mut self, source: &str, text: &str, start_ms: u64, end_ms: u64, speaker: Option<&str>, rms: Option<f32>) -> anyhow::Result<()>`;`SegmentRecord.rms: Option<f32>`(serde default + skip_serializing_if)。

- [ ] **Step 1: 写失败测试**(writer.rs tests)

```rust
    #[test]
    fn append_final_persists_rms_and_old_lines_tolerated() {
        let tmp = tempfile::tempdir().unwrap();
        let mut w = NoteWriter::create(tmp.path(), chrono::Local::now()).unwrap();
        let id = w.note_id().to_string();
        w.append_final("mic", "有能量", 0, 900, None, Some(0.123)).unwrap();
        w.append_final("mic", "无能量数据", 1000, 1900, None, None).unwrap();
        w.finalize(chrono::Local::now()).unwrap();
        let store = crate::store::NoteStore::new(tmp.path().to_path_buf());
        let n = store.load(&id).unwrap();
        assert_eq!(n.segments[0].rms, Some(0.123));
        assert_eq!(n.segments[1].rms, None);
        // None 不序列化该键(旧行等价形状,双向兼容)
        let raw = std::fs::read_to_string(tmp.path().join(&id).join("segments.jsonl")).unwrap();
        assert!(raw.lines().next().unwrap().contains("\"rms\""));
        assert!(!raw.lines().nth(1).unwrap().contains("\"rms\""));
    }
```

- [ ] **Step 2: 编译确认失败**

Run: `cd src-tauri && cargo test append_final_persists_rms`
Expected: 编译错误(参数个数不符)。

- [ ] **Step 3: 实现**

store/mod.rs 的 `SegmentRecord` 增字段(放 speaker 之后):

```rust
    /// 段音频均方根(16k f32),纯诊断:为 AEC 残渣能量门槛攒真实数据(A1 backlog)。
    /// 旧笔记无此键 → None;None 不写盘,新旧行形状双向兼容。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rms: Option<f32>,
```

writer.rs `append_final` 签名加 `rms: Option<f32>` 尾参,构造 SegmentRecord 时带上。
全仓 `append_final(` 调用点(约 22 处,几乎全在 store 测试与 notes/export 测试 helper)补尾参 `None`;lib.rs 生产调用点(lib.rs:309 区域)同样先补 `None`(Task 3 换真值)。SegmentRecord 字面量构造(export.rs 测试 3 处等)补 `rms: None`。

grep 核查:`grep -rn "append_final(" src-tauri/src | grep -v "fn append_final"` 与 `grep -rn "SegmentRecord {" src-tauri/src`,全部调用/构造点无遗漏。

src/lib/notes.ts 的 SegmentRecord 类型加:

```ts
  rms?: number;
```

- [ ] **Step 4: 全量测试**

Run: `cd src-tauri && cargo test 2>&1 | grep "test result" | head -1 && cd .. && npm run check 2>&1 | tail -1`
Expected: 全 PASS;check 0/0。

- [ ] **Step 5: Commit**

```bash
git add src-tauri/src/store src-tauri/src/lib.rs src/lib/notes.ts
git commit -m "feat(store): SegmentRecord 增 rms 诊断字段(serde 双向兼容),append_final 贯通"
```

---

### Task 3: worker 接线——语言过滤丢弃 + rms 计算透传

**Files:**
- Modify: `src-tauri/src/session.rs`(recognize 后过滤;rms 计算;PendingMic/process_final/on_final 链路增 rms;本文件测试闭包签名更新)
- Modify: `src-tauri/src/lib.rs`(on_final 闭包签名增 rms 参,落盘传真值)
- Modify: `src-tauri/src/store/writer.rs`(若有 run_asr_worker 集成测试闭包,同步签名)

**Interfaces:**
- Consumes: Task 1 `is_foreign_final`、Task 2 `append_final(.., rms)`。
- Produces: `on_final: impl FnMut(Source, String, u64, u64, Option<String>, Option<f32>)`(增 rms 尾参);`process_final` 与 `PendingMic` 增 rms 贯通;session.rs 模块级 `fn rms_of(samples: &[f32]) -> f32`。

- [ ] **Step 1: 写失败测试**(session.rs tests,用既有 mock recognizer/脚本模式仿写)

```rust
    /// 外语幻觉段整段丢弃:不 emit、不进说话人表;正常段带 rms 落到 on_final。
    #[test]
    fn worker_drops_foreign_final_and_reports_rms() {
        // ScriptRecognizer: 第一条返回日语标签,第二条正常中文(lang 空,兜底不命中)
        struct ScriptRecognizer(std::collections::VecDeque<Transcript>);
        impl Recognizer for ScriptRecognizer {
            fn recognize(&mut self, _s: &[f32]) -> anyhow::Result<Transcript> {
                Ok(self.0.pop_front().unwrap_or_default())
            }
        }
        let script = vec![
            Transcript { text: "でかし".into(), lang: "<|ja|>".into() },
            Transcript { text: "正常句子".into(), lang: "<|zh|>".into() },
        ];
        let (tx, rx) = crossbeam_channel::unbounded::<FinalJob>();
        tx.send(FinalJob { source: Source::Mic, samples: vec![0.5; 1600], start_ms: 0, end_ms: 100 }).unwrap();
        tx.send(FinalJob { source: Source::Mic, samples: vec![0.5; 1600], start_ms: 200, end_ms: 300 }).unwrap();
        drop(tx);
        let mut finals: Vec<(String, Option<f32>)> = Vec::new();
        run_asr_worker(
            Box::new(ScriptRecognizer(script.into())),
            None,
            SpeakerRegistry::new(),
            rx,
            Duration::from_millis(0), // hold 归零,立即 release
            Vec::new(),
            |_src, text, _s, _e, _spk, rms| finals.push((text, rms)),
            |_, _| {},
            |_| {},
        );
        assert_eq!(finals.len(), 1, "日语幻觉段被丢弃");
        assert_eq!(finals[0].0, "正常句子");
        let rms = finals[0].1.expect("正常段必须带 rms");
        assert!((rms - 0.5).abs() < 1e-3, "全 0.5 样本的 RMS 应为 0.5,得 {rms}");
    }
```

(注:`run_asr_worker`/`SpeakerRegistry`/mock 的确切用法以既有测试(session.rs:560+ 起)为准——先读一个既有 worker 测试,借用其构造模式;hold 参数名/类型不符时按实际调整,断言语义不变。)

- [ ] **Step 2: 编译确认失败**

Run: `cd src-tauri && cargo test worker_drops_foreign`
Expected: 编译错误(on_final 闭包参数个数不符)。

- [ ] **Step 3: 实现**

session.rs 增 rms 函数(`text_prefix20` 附近):

```rust
/// 段音频均方根。空段为 0(理论不出现,防御)。
fn rms_of(samples: &[f32]) -> f32 {
    if samples.is_empty() {
        return 0.0;
    }
    (samples.iter().map(|x| x * x).sum::<f32>() / samples.len() as f32).sqrt()
}
```

worker 内 recognize 处(session.rs:256-268)改为同时取 lang 并过滤:

```rust
                let (text, lang) = match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    recognizer.recognize(&job.samples)
                })) {
                    Ok(Ok(t)) => (t.text, t.lang),
                    Ok(Err(_)) => ("[识别失败]".to_string(), String::new()),
                    Err(_) => {
                        eprintln!(
                            "run_asr_worker: recognize panicked on a {:?} final; 以占位继续",
                            job.source
                        );
                        ("[识别失败]".to_string(), String::new())
                    }
                };
                // 语言白名单:外语幻觉段与 ECHO 命中同待遇——不 embed/不 assign/
                // 不 emit/不落盘,从源头杜绝垃圾段污染说话人表。占位段占比 0 天然放行。
                if is_foreign_final(&lang, &text) {
                    eprintln!(
                        "语言过滤: 丢弃 {:?} 段 lang=\"{lang}\" text=\"{}\"",
                        job.source,
                        text_prefix20(&text)
                    );
                    continue;
                }
                let seg_rms = rms_of(&job.samples);
```

rms 贯通(机械改动,全在 session.rs + lib.rs):
- `PendingMic` 增 `rms: f32`;两处构造(push_back)带 `rms: seg_rms`;
- `process_final` 增 `rms: f32` 参,`on_final(source, text, start_ms, end_ms, speaker, Some(rms))`;
- `release_pending!` 与三处 `process_final(` 直调点补 rms 实参;
- `run_asr_worker` 的 `on_final` 泛型签名与文档注释更新为 6 参;
- 本文件既有测试的 on_final 闭包(grep `on_final` / 六参不符的闭包)补 `_rms` 参;writer.rs 若有集成测试闭包同步。

lib.rs on_final 闭包(约 279-320 区域):签名增 `rms`,`append_final(src.as_str(), &text, start_ms, end_ms, spk.as_deref(), rms)`(替换 Task 2 的临时 None)。ipc 事件不带 rms(UI 不消费)。

- [ ] **Step 4: 全量验证**

Run: `cd src-tauri && cargo test 2>&1 | grep "test result" | head -1 && cargo build 2>&1 | tail -2 && cd .. && npm run check 2>&1 | tail -1 && npm run build 2>&1 | tail -2`
Expected: 全 PASS(新增 2 测),无新警告,check 0/0,build OK。

- [ ] **Step 5: progress.md 记账 + Commit**

`.superpowers/sdd/progress.md` 末尾加:

```markdown
## 语言幻觉过滤 + RMS 埋点(分支 lang-filter-rms)
- 外语(日/韩)幻觉 final 段源头丢弃(标签+字符占比双保险);段级 rms 落盘攒 A1 能量门槛数据。
- spec: docs/superpowers/specs/2026-07-04-voice-notes-lang-filter-rms-design.md
```

```bash
git add src-tauri/src/session.rs src-tauri/src/lib.rs src-tauri/src/store/writer.rs .superpowers/sdd/progress.md
git commit -m "feat(session): 外语幻觉段源头丢弃 + 段级 rms 计算落盘"
```

(push、PR、终审由执行流程收尾阶段处理。)
