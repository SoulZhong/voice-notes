# 钩子附带笔记内容 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 钩子加 `include_note` 开关，开启后执行时注入笔记详情与全文（精修稿优先，200KB 截断），shell 走环境变量、webhook 走 payload `note` 字段。

**Architecture:** 全部改动收敛在既有 `hooks_external.rs`（纯函数层：截断/显示名/env/payload 扩展；读盘层：`note_content` 精修优先渲染；集成：run_fires 批内按 note_id 缓存一次、test_run 假内容）+ 前端三处（数据层类型、编辑页开关行、概览页文档）+ DESIGN.md。构建失败只记日志照常触发钩子。

**Tech Stack:** Rust（复用 `store::render_refined`/`NoteStore::render` 纯渲染，不落盘）/ Svelte 5。

**Spec:** `docs/superpowers/specs/2026-07-14-hooks-note-content-design.md`

**工作目录（重要）**：本计划在 worktree `/Users/teemo/workspace-soul/voice-notes-hooks` 执行（主检出被并行会话占用）。所有路径、git 操作、测试命令都在该目录下。node_modules 与 src-tauri/target 已软链主仓。

## Global Constraints

- git 提交**不加** `Co-Authored-By` / 署名尾注；提交信息中文；**禁止 git add -A**，只 add 点名文件。
- 环境变量名逐字：`VN_NOTE_TEXT` / `VN_NOTE_STARTED_AT` / `VN_NOTE_ENDED_AT` / `VN_NOTE_DURATION_SECS` / `VN_NOTE_SPEAKERS`（顿号、分隔）/ `VN_NOTE_TEXT_TRUNCATED`（仅截断时注入，值 `1`）。
- payload `note` 字段键逐字：`started_at` / `ended_at` / `duration_secs` / `speakers` / `text` / `text_truncated`。
- `NOTE_TEXT_MAX = 200_000` 字节，UTF-8 字符边界安全截断。
- 未开启 `include_note` 的钩子 env/payload 与现状**逐字一致**（回归断言）。
- **隐私红线**：测试夹具正文只用合成占位文本，任何真实会议原文不得进 git。
- 失败契约不变：内容构建失败记日志、照常触发钩子（无 note 部分）。
- 后端测试：`cargo test --manifest-path src-tauri/Cargo.toml hooks_external`；前端门禁 `npm run check`（均在 worktree 根执行）。

---

### Task 1: 配置字段 + 纯函数层（truncate_utf8 / speaker_display / NoteContent / note_envs / payload 扩展）

**Files:**
- Modify: `src-tauri/src/hooks_external.rs`
- Test: 同文件 `#[cfg(test)]`

**Interfaces:**
- Consumes: 既有 `HookCfg`、`payload(event, note_id, title, occurred_at)`（本任务改其签名，调用点全在本模块与 lib.rs——lib.rs 不调 payload，无跨文件影响）。
- Produces（Task 2/3 依赖）:
  - `HookCfg` 新字段 `#[serde(default)] pub include_note: bool`
  - `pub const NOTE_TEXT_MAX: usize = 200_000;`
  - `pub struct NoteContent { pub started_at: String, pub ended_at: String, pub duration_secs: u64, pub speakers: Vec<String>, pub text: String, pub truncated: bool }`
  - `pub fn truncate_utf8(s: String, max: usize) -> (String, bool)`
  - `pub fn speaker_display(id: &str, name: &str) -> String`（name 非空→name；否则「说话人 N」，N=id 去掉 P/S 前缀）
  - `pub fn note_envs(c: &NoteContent) -> Vec<(String, String)>`
  - `pub fn payload(event: &str, note_id: &str, title: &str, occurred_at: &str, note: Option<&NoteContent>) -> serde_json::Value`

- [ ] **Step 1: 写失败测试**（追加到 hooks_external.rs 测试模块）

```rust
    #[test]
    fn include_note_defaults_false_on_old_config() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(
            tmp.path().join("hooks.json"),
            r#"{"hooks":[{"id":"h_old","event":"recording_stopped","command":"true"}]}"#,
        )
        .unwrap();
        assert!(!load(tmp.path()).hooks[0].include_note, "老配置缺字段 → false");
    }

    #[test]
    fn truncate_utf8_respects_char_boundary() {
        // 「会」UTF-8 三字节:上限落在字符中间必须回退到边界,不产生半个字符
        let (out, cut) = truncate_utf8("会议纪要".to_string(), 4);
        assert_eq!(out, "会");
        assert!(cut);
        let (out, cut) = truncate_utf8("会议".to_string(), 6);
        assert_eq!(out, "会议");
        assert!(!cut, "恰好等长不算截断");
        let (out, cut) = truncate_utf8(String::new(), 10);
        assert_eq!(out, "");
        assert!(!cut);
    }

    #[test]
    fn speaker_display_prefers_name_falls_back_to_number() {
        assert_eq!(speaker_display("P3", "张三"), "张三");
        assert_eq!(speaker_display("P3", ""), "说话人 3");
        assert_eq!(speaker_display("S1", ""), "说话人 1");
    }

    fn content_fixture() -> NoteContent {
        NoteContent {
            started_at: "2026-07-14T10:00:00+08:00".into(),
            ended_at: "2026-07-14T11:00:00+08:00".into(),
            duration_secs: 3600,
            speakers: vec!["张三".into(), "说话人 2".into()],
            text: "# 占位正文".into(),
            truncated: false,
        }
    }

    #[test]
    fn note_envs_and_payload_shapes() {
        let c = content_fixture();
        let envs = note_envs(&c);
        assert!(envs.contains(&("VN_NOTE_TEXT".into(), "# 占位正文".into())));
        assert!(envs.contains(&("VN_NOTE_STARTED_AT".into(), "2026-07-14T10:00:00+08:00".into())));
        assert!(envs.contains(&("VN_NOTE_ENDED_AT".into(), "2026-07-14T11:00:00+08:00".into())));
        assert!(envs.contains(&("VN_NOTE_DURATION_SECS".into(), "3600".into())));
        assert!(envs.contains(&("VN_NOTE_SPEAKERS".into(), "张三、说话人 2".into())));
        assert!(!envs.iter().any(|(k, _)| k == "VN_NOTE_TEXT_TRUNCATED"), "未截断不注入标记");

        let mut cut = content_fixture();
        cut.truncated = true;
        assert!(note_envs(&cut).contains(&("VN_NOTE_TEXT_TRUNCATED".into(), "1".into())));

        let p = payload("refine_finished", "n1", "周会", "2026-07-14T11:00:01+08:00", Some(&c));
        assert_eq!(p["note"]["duration_secs"], 3600);
        assert_eq!(p["note"]["speakers"][0], "张三");
        assert_eq!(p["note"]["text"], "# 占位正文");
        assert_eq!(p["note"]["text_truncated"], false);
        // 未附带时与现状逐字一致(回归防漂移):没有 note 键
        let p0 = payload("refine_finished", "n1", "周会", "2026-07-14T11:00:01+08:00", None);
        assert!(p0.get("note").is_none());
        assert_eq!(p0["event"], "refine_finished");
    }
```

既有 `shell_envs_and_payload_shape` 与 `test_run`/`run_fires` 里的 `payload(...)` 调用点补第五参 `None`。

- [ ] **Step 2: 跑测试确认失败**

Run: `cargo test --manifest-path src-tauri/Cargo.toml hooks_external`
Expected: 编译失败（`include_note`/`truncate_utf8`/`NoteContent` 未定义）。

- [ ] **Step 3: 最小实现**

`HookCfg` 增字段（`enabled` 之后）：

```rust
    /// 附带笔记内容:开启时执行注入笔记详情与全文(精修稿优先)。默认关,
    /// serde default 兼容老 hooks.json。
    #[serde(default)]
    pub include_note: bool,
```

纯函数层（放 `shell_envs` 附近）：

```rust
/// 内嵌全文的字节上限:macOS execve 的 env+argv 总预算约 1MB,超限 spawn 直接
/// E2BIG 失败——截断是"都内嵌文本"方案的硬约束,不是优化。
pub const NOTE_TEXT_MAX: usize = 200_000;

/// 钩子附带的笔记内容(详情+全文)。text 为 markdown,可能被截断。
pub struct NoteContent {
    pub started_at: String,
    /// 空串 = 未结束。
    pub ended_at: String,
    pub duration_secs: u64,
    /// 显示名:名字 > 「说话人 N」。
    pub speakers: Vec<String>,
    pub text: String,
    pub truncated: bool,
}

/// 按 UTF-8 字符边界安全截断:上限落在多字节字符中间时回退,绝不产生半个字符。
pub fn truncate_utf8(s: String, max: usize) -> (String, bool) {
    if s.len() <= max {
        return (s, false);
    }
    let mut end = max;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    (s[..end].to_string(), true)
}

/// 说话人显示名兜底,与前端 speakerLabel 同语义:名字 > 「说话人 N」。
pub fn speaker_display(id: &str, name: &str) -> String {
    if !name.is_empty() {
        return name.to_string();
    }
    format!("说话人 {}", id.trim_start_matches(['P', 'S']))
}

/// include_note 追加的环境变量(在 shell_envs 三件之上)。截断标记只在截断时
/// 注入——存在即真,脚本用 [ -n "$VN_NOTE_TEXT_TRUNCATED" ] 判断。
pub fn note_envs(c: &NoteContent) -> Vec<(String, String)> {
    let mut v = vec![
        ("VN_NOTE_TEXT".into(), c.text.clone()),
        ("VN_NOTE_STARTED_AT".into(), c.started_at.clone()),
        ("VN_NOTE_ENDED_AT".into(), c.ended_at.clone()),
        ("VN_NOTE_DURATION_SECS".into(), c.duration_secs.to_string()),
        ("VN_NOTE_SPEAKERS".into(), c.speakers.join("、")),
    ];
    if c.truncated {
        v.push(("VN_NOTE_TEXT_TRUNCATED".into(), "1".into()));
    }
    v
}
```

`payload` 改签名（原实现整体保留，追加可选 note）：

```rust
pub fn payload(
    event: &str,
    note_id: &str,
    title: &str,
    occurred_at: &str,
    note: Option<&NoteContent>,
) -> serde_json::Value {
    let mut p = serde_json::json!({
        "event": event,
        "note_id": note_id,
        "note_title": title,
        "occurred_at": occurred_at,
    });
    if let Some(c) = note {
        p["note"] = serde_json::json!({
            "started_at": c.started_at,
            "ended_at": c.ended_at,
            "duration_secs": c.duration_secs,
            "speakers": c.speakers,
            "text": c.text,
            "text_truncated": c.truncated,
        });
    }
    p
}
```

模块内既有 `payload(...)` 调用点（run_fires/test_run/既有测试）补 `None`。

- [ ] **Step 4: 跑测试确认通过**

Run: `cargo test --manifest-path src-tauri/Cargo.toml hooks_external`
Expected: 全绿（含既有 10 个 + 新 4 个）。

- [ ] **Step 5: 提交**

```bash
git add src-tauri/src/hooks_external.rs
git commit -m "钩子附带内容纯函数层:include_note字段+UTF8截断+详情env/payload扩展"
```

---

### Task 2: note_content 读盘构建 + run_fires / test_run 集成

**Files:**
- Modify: `src-tauri/src/hooks_external.rs`（`note_content` + run_fires 批内缓存 + test_run 假内容）
- Modify: `src-tauri/src/lib.rs:131`（`fn data_root` → `pub(crate) fn data_root`，只动可见性）
- Test: hooks_external.rs `#[cfg(test)]`

**Interfaces:**
- Consumes: Task 1 全部；`crate::store::{NoteStore, load_refined, render_refined, join_library_names, VoiceprintStore}`（均已 re-export，见 store/mod.rs:11-16）；`crate::notes_dir`（已 pub(crate)）；`crate::data_root`（本任务放宽）。
- Produces: `fn note_content(app: &tauri::AppHandle, note_id: &str) -> Option<NoteContent>`（模块私有即可，run_fires/test 内消费）。

- [ ] **Step 1: 写失败测试**

读盘构建用 tempdir 搭笔记目录直测内部纯部分——`note_content` 依赖 AppHandle 不好单测，拆出**不依赖 app 的核心**：`note_content_from_dirs(notes_dir: &Path, data_root: Option<&Path>, note_id: &str) -> Option<NoteContent>`，`note_content(app, id)` 只是路径解析薄壳。测试（正文全用合成占位，隐私红线）：

```rust
    fn write_note_fixture(notes_dir: &std::path::Path, id: &str) {
        let dir = notes_dir.join(id);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("meta.json"),
            r#"{"schema_version":1,"id":"n1","title":"占位标题","started_at":"2026-07-14T10:00:00+08:00","ended_at":"2026-07-14T11:00:00+08:00","state":"complete"}"#,
        )
        .unwrap();
        std::fs::write(
            dir.join("segments.jsonl"),
            r#"{"seq":1,"source":"mic","text":"占位甲","start_ms":0,"end_ms":2000,"speaker":"S1"}
{"seq":2,"source":"mic","text":"占位乙","start_ms":2000,"end_ms":5000,"speaker":"S2"}
"#,
        )
        .unwrap();
        std::fs::write(
            dir.join("speakers.json"),
            r#"{"S1":{"name":"张三","sources":["mic"]},"S2":{"name":"","sources":["mic"]}}"#,
        )
        .unwrap();
    }

    #[test]
    fn note_content_raw_fallback_and_details() {
        let tmp = tempfile::tempdir().unwrap();
        write_note_fixture(tmp.path(), "n1");
        let c = note_content_from_dirs(tmp.path(), None, "n1").unwrap();
        assert_eq!(c.started_at, "2026-07-14T10:00:00+08:00");
        assert_eq!(c.ended_at, "2026-07-14T11:00:00+08:00");
        assert_eq!(c.duration_secs, 5, "时长=段落最大 end_ms(5000)/1000");
        assert_eq!(c.speakers, vec!["张三".to_string(), "说话人 2".to_string()]);
        assert!(c.text.contains("占位甲"), "无精修稿回落原始稿渲染");
        assert!(!c.truncated);
    }

    #[test]
    fn note_content_prefers_refined() {
        let tmp = tempfile::tempdir().unwrap();
        write_note_fixture(tmp.path(), "n1");
        std::fs::write(
            tmp.path().join("n1").join("refined.json"),
            r#"{"schema_version":1,"stages":{"filter":"done","recluster":"done","llm":"done"},"paragraphs":[{"speaker":"R1","label":"张三","start_ms":0,"end_ms":5000,"text":"精修占位正文","source_seqs":[1,2]}]}"#,
        )
        .unwrap();
        let c = note_content_from_dirs(tmp.path(), None, "n1").unwrap();
        assert!(c.text.contains("精修占位正文"), "精修稿在盘时优先");
        assert!(!c.text.contains("占位甲"), "不再是原始稿");
    }

    #[test]
    fn note_content_missing_note_is_none() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(note_content_from_dirs(tmp.path(), None, "nope").is_none());
    }
```

注意:`refined.json` 的 fixture 字段以 `store/refined.rs` 的 `RefinedDoc`/`RefinedParagraph` serde 形状为准（实现前先读该文件核对字段名，与上面示例不符时改 fixture 而不是改结构）。

- [ ] **Step 2: 跑测试确认失败**

Run: `cargo test --manifest-path src-tauri/Cargo.toml note_content`
Expected: 编译失败（`note_content_from_dirs` 未定义）。

- [ ] **Step 3: 实现**

```rust
/// 笔记内容构建核心(可测):notes_dir 定笔记,data_root 有值时精修稿做声纹库
/// 现名 join(与 export_note 同款只读语义)。任何读盘失败回 None——内容是增值
/// 信息,由调用方决定跳过附带照常执行。
fn note_content_from_dirs(
    notes_dir: &std::path::Path,
    data_root: Option<&std::path::Path>,
    note_id: &str,
) -> Option<NoteContent> {
    let store = crate::store::NoteStore::new(notes_dir.to_path_buf());
    let note = store.load(note_id).ok()?;
    let text = match crate::store::load_refined(&notes_dir.join(note_id)) {
        Some(mut doc) => {
            if doc.paragraphs.iter().any(|p| p.person_id.is_some()) {
                if let Some(root) = data_root {
                    let vp = crate::store::VoiceprintStore::new(root.to_path_buf()).load();
                    crate::store::join_library_names(&mut doc, &vp);
                }
            }
            crate::store::render_refined(&note.meta.title, &doc, true)
        }
        None => store.render(note_id, "md").ok()?,
    };
    let (text, truncated) = truncate_utf8(text, NOTE_TEXT_MAX);
    let duration_secs = note.segments.iter().map(|s| s.end_ms).max().unwrap_or(0) / 1000;
    Some(NoteContent {
        started_at: note.meta.started_at.clone(),
        ended_at: note.meta.ended_at.clone().unwrap_or_default(),
        duration_secs,
        speakers: note.speakers.iter().map(|(id, m)| speaker_display(id, &m.name)).collect(),
        text,
        truncated,
    })
}

/// AppHandle 薄壳:解析两个根目录后进核心。
fn note_content(app: &tauri::AppHandle, note_id: &str) -> Option<NoteContent> {
    let notes = crate::notes_dir(app).ok()?;
    let root = crate::data_root(app).ok();
    note_content_from_dirs(&notes, root.as_deref(), note_id)
}
```

（`VoiceprintStore::new` 的参数形状以 store/voiceprints.rs 实际签名为准，mcp/tools.rs:200 的用法 `VoiceprintStore::new(roots.data_root.clone()).load()` 是现成参照。）

lib.rs:131 可见性：`pub(crate) fn data_root(app: &AppHandle) -> anyhow::Result<PathBuf>`。

run_fires 集成（在 `let occurred_at = ...` 之前后插入，批内按 note_id 缓存）：

```rust
    // 批内内容缓存:停录+自动精修同帧多事件共享同一 note_id,只构建一次。
    // Option 也缓存——构建失败(笔记刚删)同批不再重试,只记一次日志。
    let mut contents: std::collections::HashMap<String, Option<NoteContent>> =
        std::collections::HashMap::new();
```

循环内、`for cfg in matched` 之前：

```rust
        let need_note = matched.iter().any(|c| c.include_note);
        let content = if need_note {
            contents
                .entry(f.note_id.clone())
                .or_insert_with(|| {
                    let c = note_content(app, &f.note_id);
                    if c.is_none() {
                        eprintln!("hooks: 笔记内容构建失败({}),照常触发但不附带", f.note_id);
                    }
                    c
                })
                .as_ref()
        } else {
            None
        };
```

`for cfg in matched` 内按钩子取用（未开启的钩子拿 None，行为与现状逐字一致）：

```rust
            let note = if cfg.include_note { content } else { None };
            let r = match cfg.kind.as_str() {
                "webhook" => run_webhook(&cfg.url, &payload(event, &f.note_id, &title, &occurred_at, note), WEBHOOK_LIMIT)
                    .map(|s| format!("HTTP {s}")),
                _ => {
                    let mut envs = shell_envs(event, &f.note_id, &title);
                    if let Some(c) = note {
                        envs.extend(note_envs(c));
                    }
                    match run_shell(&cfg.command, &envs, SHELL_LIMIT) {
                        Ok(0) => Ok("退出码 0".into()),
                        Ok(c) => Err(format!("退出码 {c}")),
                        Err(e) => Err(e),
                    }
                }
            };
```

（borrow 注意：`content` 是 `Option<&NoteContent>`，来自 `contents` 的 entry——`or_insert_with` 后 `.as_ref()` 借用与循环体后续 `contents.entry` 再借用会打架；每轮 `for f` 重新取即可，编译器若报错就改为先 `if !contents.contains_key(...) { contents.insert(...) }` 再 `contents.get(&f.note_id).and_then(|o| o.as_ref())` 两步，语义相同。）

test_run 假内容（不依赖库里有真笔记）：

```rust
    // 假内容:测试不读库,注入固定占位——用户看得出变量有值即可。
    let fake = cfg.include_note.then(|| NoteContent {
        started_at: "2026-07-14T10:00:00+08:00".into(),
        ended_at: "2026-07-14T11:00:00+08:00".into(),
        duration_secs: 3600,
        speakers: vec!["测试说话人".into()],
        text: "测试正文".into(),
        truncated: false,
    });
```

webhook 分支 `payload(..., fake.as_ref())`；shell 分支 envs 先 `shell_envs(...)` 再 `if let Some(c) = &fake { envs.extend(note_envs(c)); }`。

- [ ] **Step 4: 跑测试确认通过**

Run: `cargo test --manifest-path src-tauri/Cargo.toml` （全量——render/store 路径被新消费不能破）
Expected: 全绿。另跑 `cargo check --manifest-path src-tauri/Cargo.toml` 无新警告。

- [ ] **Step 5: 提交**

```bash
git add src-tauri/src/hooks_external.rs src-tauri/src/lib.rs
git commit -m "钩子附带内容接线:精修优先构建+批内缓存+test_run假内容"
```

---

### Task 3: 前端（类型 + 编辑页开关行 + 概览页文档）+ DESIGN.md

**Files:**
- Modify: `src/lib/hooks.svelte.ts`（HookCfg 类型 + newHook）
- Modify: `src/routes/hooks/[id]/+page.svelte`（「启用」行之上加开关行）
- Modify: `src/routes/hooks/+page.svelte`（env 表 + payload 示例 + 时点语义句）
- Modify: `DESIGN.md`（钩子配置条目：行清单与清空语义）

**Interfaces:**
- Consumes: Task 1 的 `include_note` 字段名（后端 serde 逐字）。
- Produces: 无后续任务。

- [ ] **Step 1: hooks.svelte.ts**

`HookCfg` 类型加 `include_note: boolean;`（`enabled` 之后）；`newHook()` 返回对象加 `include_note: false,`。

- [ ] **Step 2: 编辑页开关行**

`src/routes/hooks/[id]/+page.svelte` 在「启用」`<label class="row">`（约 205 行）**之前**插入：

```svelte
      <label class="row">
        <div class="row-info">
          <span class="row-label">附带笔记内容</span>
          <span class="row-desc">把笔记详情与全文交给命令/接口,精修稿优先;想要精修全文请挂「精修完成」</span>
        </div>
        <!-- 与启用开关不同:附带与否改变测试注入的内容,旧测试结果不再作数 -->
        <input
          type="checkbox"
          class="switch"
          bind:checked={cfg.include_note}
          onchange={() => (testResult = null)}
        />
      </label>
```

- [ ] **Step 3: 概览页文档**

`src/routes/hooks/+page.svelte`：

① 环境变量 section 的 `.rows` 末尾追加六行（形态与既有三行同构，row-desc 首行标注条件）：

```svelte
      <div class="row">
        <div class="row-info">
          <span class="row-label"><code>VN_NOTE_TEXT</code></span>
          <span class="row-desc">笔记全文 markdown,精修稿优先——仅钩子勾选「附带笔记内容」时注入,下同</span>
        </div>
      </div>
      <div class="row">
        <div class="row-info">
          <span class="row-label"><code>VN_NOTE_STARTED_AT</code> / <code>VN_NOTE_ENDED_AT</code></span>
          <span class="row-desc">开始/结束时间(RFC3339),未结束时结束为空</span>
        </div>
      </div>
      <div class="row">
        <div class="row-info">
          <span class="row-label"><code>VN_NOTE_DURATION_SECS</code></span>
          <span class="row-desc">时长秒数</span>
        </div>
      </div>
      <div class="row">
        <div class="row-info">
          <span class="row-label"><code>VN_NOTE_SPEAKERS</code></span>
          <span class="row-desc">说话人名单,顿号分隔</span>
        </div>
      </div>
      <div class="row">
        <div class="row-info">
          <span class="row-label"><code>VN_NOTE_TEXT_TRUNCATED</code></span>
          <span class="row-desc">全文超 200KB 被截断时为 1,未截断不注入</span>
        </div>
      </div>
```

② webhook snippet 的 JSON 里 `"occurred_at"` 行后补（保持缩进一致）：

```
  "note": {
    "started_at": "…", "ended_at": "…", "duration_secs": 3600,
    "speakers": ["张三"], "text": "…markdown…", "text_truncated": false
  }
```

并在该 section 末尾（`</pre>` 后）加一行说明：

```svelte
    <p class="hint">note 字段仅在钩子勾选「附带笔记内容」时出现;停止录制时通常是原始稿,想要精修全文请挂「精修完成」事件。</p>
```

`.hint` 样式（该页 style 尚无则加）：`.hint { color: var(--ink-faint); font-size: 0.8rem; margin: 0.5rem 0 0; }`

- [ ] **Step 4: DESIGN.md**

「钩子配置」条目两处微改：①编辑页行清单「五行」改「六行」，在「启用 switch 行」前插入「附带笔记内容 switch 行(说明:详情与全文交给命令/接口,精修稿优先)」；②清空语义句把「改名称/事件/方式/命令或 URL」扩为「改名称/事件/方式/命令或 URL/附带笔记内容」（附带开关改变测试注入内容，须清空；启用开关仍除外）。以文件中实际措辞为准做最小编辑。

- [ ] **Step 5: 门禁 + 提交**

Run: `npm run check`
Expected: 0 errors。

```bash
git add src/lib/hooks.svelte.ts "src/routes/hooks/[id]/+page.svelte" src/routes/hooks/+page.svelte DESIGN.md
git commit -m "钩子附带内容前端:编辑页开关行+概览页文档六变量与note字段,DESIGN.md同步"
```

---

### Task 4: 全量回归

**Files:** 无新改动（只跑门禁；有失败则修在对应文件）。

- [ ] **Step 1: 三门禁**

```bash
cargo test --manifest-path src-tauri/Cargo.toml
npm run check && npm test
```
Expected: cargo 全绿（434+ 新增用例）；check 0 errors；vitest 过。

- [ ] **Step 2: 如全绿无需提交**；有修复则按涉及文件单独提交（信息中文、无尾注、不 add -A）。

---

## Self-Review 记录

- **Spec 覆盖**：include_note 字段/前端类型→T1/T3；截断 200KB+UTF8 边界→T1；精修优先回落原始+join 现名→T2；六 env/payload note 字段→T1；批内缓存+失败跳过→T2；test_run 假内容→T2；编辑页开关行+概览页文档+时点语义→T3；DESIGN.md→T3；未开启逐字一致回归→T1 测试；老配置默认 false→T1 测试。无缺口。
- **占位符**：refined.json fixture 标注了「以 refined.rs 实际 serde 形状为准」——这是防漂移指令而非 TBD，实现者有明确动作。
- **类型一致性**：`NoteContent` 字段、`note_envs`/`payload` 签名、`include_note` 命名前后端逐字一致；`speaker_display(id, name)` 参数序在测试与实现一致。
