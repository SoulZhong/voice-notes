# P5 backlog 硬化实施计划

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 落地 P5 终审 defer 的 6 项硬化:非活动写锁、export/详情页语义统一、download_running drop-guard、preload 会话跳过、解压即时取消(含 416 免重下复装)、下载卡片单实例化。

**Architecture:** 全部是既有模块内的局部硬化,无新模块、无协议变化。后端 4 项(store/notes.rs、store 读侧、lib.rs 下载线程与预载、models/download.rs),前端 2 项(详情页 derived 简化、record 页卡片分支合并)。

**Tech Stack:** Rust(Tauri 2)+ SvelteKit(Svelte 5 runes)。测试:cargo test(tempfile fixture)+ npm check。

**Spec:** `docs/superpowers/specs/2026-07-04-voice-notes-p5-backlog-hardening-design.md`

## Global Constraints

- 分支 `p5-backlog-hardening`(从 master 切),单 PR squash 合入。
- 注释风格:中文、讲"为什么"(约束/不变量),不复述代码。
- 验收:cargo test 全过(含新增),npm run check 0 errors 0 warnings,cargo build 无新警告。
- 模型不进 git;涉及真模型的行为(preload)不写单测,靠冒烟。
- 锁序纪律:recognizer_cache/embedder_cache 是叶子锁,绝不在持有期间再拿其它锁;session 锁 statement-scoped 用完即放。

---

### Task 0: 建分支

- [ ] **Step 1: 从 master 切特性分支**

```bash
cd /Users/teemo/workspace-soul/voice-notes
git checkout master && git pull --ff-only
git checkout -b p5-backlog-hardening
```

预期:`git branch --show-current` 输出 `p5-backlog-hardening`。

---

### Task 1: NoteStore 全局编辑锁(非活动写者丢更新)

**Files:**
- Modify: `src-tauri/src/store/notes.rs`(结构体上方加静态锁;6 个变更方法入口持锁;tests 模块加并发测试)

**Interfaces:**
- Consumes: 既有 `NoteStore` 方法签名(全部不变)。
- Produces: 无新公开接口。锁是模块私有静态,调用方(lib.rs)零改动。

**背景**:`NoteStore` 无状态、每命令 `new(dir)`,`rename_speaker` 与 `set_segment_speaker` 等对 speakers.json / segments.jsonl 的 read-modify-write 之间无互斥,并发时后落盘者用旧基线覆盖前者(丢更新)。活动写者走 `Arc<Mutex<NoteWriter>>` 单写者,不受影响。

- [ ] **Step 1: 写失败测试**(notes.rs tests 模块末尾,`rename_speaker_persists_and_missing_file_tolerated` 之后)

```rust
    /// 并发丢更新回归:rename_speaker 与 set_segment_speaker("new") 两线程互跑,
    /// 无锁时各自 read-modify-write 整表覆盖,终态必然缺改动;EDIT_LOCK 下两者全存活。
    #[test]
    fn concurrent_speaker_edits_do_not_lose_updates() {
        let tmp = tempfile::tempdir().unwrap();
        let id = make_spk_note(tmp.path(), &[("甲", Some("S1")), ("乙", Some("S1"))], &["S1"]);
        let dir = tmp.path().to_path_buf();
        let t1 = std::thread::spawn({
            let (dir, id) = (dir.clone(), id.clone());
            move || {
                for i in 0..20 {
                    NoteStore::new(dir.clone()).rename_speaker(&id, "S1", &format!("名{i}")).unwrap();
                }
            }
        });
        let t2 = std::thread::spawn({
            let (dir, id) = (dir.clone(), id.clone());
            move || {
                for _ in 0..20 {
                    NoteStore::new(dir.clone()).set_segment_speaker(&id, 1, "乙", "new").unwrap();
                }
            }
        });
        t1.join().unwrap();
        t2.join().unwrap();
        let n = NoteStore::new(dir).load(&id).unwrap();
        assert_eq!(n.speakers["S1"].name, "名19", "rename 线程的最后写入存活");
        // S1 + 20 个新建说话人:任何一次丢更新都会让计数不足 21。
        assert_eq!(n.speakers.len(), 21, "20 次新建全部存活,无丢更新");
    }
```

- [ ] **Step 2: 跑测试确认失败(或偶发失败)**

Run: `cd src-tauri && cargo test concurrent_speaker_edits -- --nocapture`
Expected: FAIL(assert 计数不足;竞态偶发,若侥幸通过,重跑 `--test-threads=1` 数次可复现——记录现象即可,不必死磕稳定失败)。

- [ ] **Step 3: 加锁实现**(notes.rs,`pub struct NoteStore` 定义之前)

```rust
/// 非活动写者全局编辑锁。NoteStore 每命令新建、无状态,speakers.json /
/// segments.jsonl 的 read-modify-write 之间没有任何互斥,并发编辑会整表互相
/// 覆盖丢更新。锁内建于变更方法——调用方无法遗忘;编辑均为毫秒级稀有操作,
/// 跨笔记串行无感知。活动写者走 NoteWriter 自己的锁,与此无关。
/// 毒化忽略(into_inner):每次落盘各自原子,持锁线程 panic 不留半写状态。
static EDIT_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn edit_guard() -> std::sync::MutexGuard<'static, ()> {
    EDIT_LOCK.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}
```

6 个变更方法(`rename`、`delete`、`rename_speaker`、`edit_segment_text`、`delete_segment`、`set_segment_speaker`)各自函数体第一行加:

```rust
        let _guard = edit_guard();
```

(`load`/`list` 只读不加——读侧靠单次写原子性,读到新旧皆一致。)

- [ ] **Step 4: 跑测试确认通过**

Run: `cd src-tauri && cargo test store:: -- --nocapture`
Expected: 全 PASS(含新测试与既有 store 测试)。

- [ ] **Step 5: Commit**

```bash
git add src-tauri/src/store/notes.rs
git commit -m "fix(store): NoteStore 变更方法加全局编辑锁,根治非活动写者并发丢更新"
```

---

### Task 2: filter+sort 下沉 NoteStore::load(export/详情页语义统一)

**Files:**
- Modify: `src-tauri/src/store/notes.rs:48-67`(load 末尾加过滤+排序;tests 加一测)
- Modify: `src-tauri/src/store/export.rs`(tests 加一测,产品代码零改动——自动继承)
- Modify: `src/routes/notes/[id]/+page.svelte:38-45`(displaySegments 简化)

**Interfaces:**
- Consumes: `NoteStore::load(&self, id) -> anyhow::Result<Note>`(签名不变)。
- Produces: `Note.segments` 语义变更——**保证无空白段、按 (start_ms, seq) 升序**。get_note 与 export 自动继承。磁盘文件序不动(编辑走独立的 `read_jsonl_lines` 原始行路径;续录 next_seq 是 writer 自扫 jsonl,均不受影响)。

- [ ] **Step 1: 写失败测试**(notes.rs tests)

```rust
    /// 读侧单一真值源:load 过滤空白段、按 (start_ms, seq) 稳定排序——
    /// 详情页与导出共同继承,消除 ECHO hold 落盘交错。
    #[test]
    fn load_filters_blank_and_sorts_by_start_ms() {
        let tmp = tempfile::tempdir().unwrap();
        let mut w = NoteWriter::create(tmp.path(), now()).unwrap();
        let id = w.note_id().to_string();
        w.append_final("mic", "后", 5000, 6000, None).unwrap();       // seq 0
        w.append_final("system", "   ", 500, 900, None).unwrap();     // seq 1 空白段
        w.append_final("mic", "前", 1000, 1500, None).unwrap();       // seq 2
        w.append_final("system", "同前", 1000, 1400, None).unwrap();  // seq 3 同 start,按 seq 稳定
        w.finalize(now()).unwrap();
        let n = NoteStore::new(tmp.path().to_path_buf()).load(&id).unwrap();
        let texts: Vec<&str> = n.segments.iter().map(|s| s.text.as_str()).collect();
        assert_eq!(texts, ["前", "同前", "后"], "空白段滤除,start_ms 升序,同值按 seq");
        assert_eq!(n.skipped_lines, 0, "空白段不是损坏行,不计 skipped");
    }
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cd src-tauri && cargo test load_filters_blank -- --nocapture`
Expected: FAIL(实际顺序 ["后", "   ", "前", "同前"])。

- [ ] **Step 3: load 实现**(notes.rs,`load` 中 `let speakers = read_speakers(&dir);` 之前插入)

```rust
        // 读侧单一真值源:过滤空白段 + 按 start_ms 稳定排序(同值按 seq),消除
        // ECHO hold 造成的落盘交错——详情页与导出共同继承此语义,防两处漂移。
        // 磁盘文件序不动:编辑重写走 read_jsonl_lines 原始行,续录 next_seq 由
        // writer 自扫 jsonl,均不经此处。空白段非损坏,不计 skipped_lines。
        segments.retain(|s| !s.text.trim().is_empty());
        segments.sort_by(|a, b| a.start_ms.cmp(&b.start_ms).then(a.seq.cmp(&b.seq)));
```

- [ ] **Step 4: export 继承测试**(export.rs tests)

```rust
    #[test]
    fn export_inherits_display_order_and_blank_filter() {
        let tmp = tempfile::tempdir().unwrap();
        let mut w = NoteWriter::create(tmp.path(), chrono::Local::now()).unwrap();
        let id = w.note_id().to_string();
        w.append_final("mic", "后说的", 5000, 6000, None).unwrap();
        w.append_final("system", "  ", 500, 900, None).unwrap();
        w.append_final("mic", "先说的", 1000, 1500, None).unwrap();
        w.finalize(chrono::Local::now()).unwrap();
        let store = NoteStore::new(tmp.path().to_path_buf());
        let txt = std::fs::read_to_string(store.export(&id, "txt").unwrap()).unwrap();
        let (i_first, i_later) = (txt.find("先说的").unwrap(), txt.find("后说的").unwrap());
        assert!(i_first < i_later, "导出按 start_ms 序而非落盘序: {txt}");
        assert!(!txt.contains("00:00:00  \n"), "空白段不出现在导出中");
    }
```

- [ ] **Step 5: 跑 store 全部测试**

Run: `cd src-tauri && cargo test store:: writer:: export`
Expected: 全 PASS(既有 roundtrip/编辑/writer 测试无回归——它们的 fixture 时间轴本就升序)。

- [ ] **Step 6: 前端 displaySegments 简化**(notes/[id]/+page.svelte:38-45 替换)

```svelte
  /** 展示序:filter+sort 已下沉 NoteStore::load(单一真值源),后端保证无空白段、
      按 (start_ms, seq) 升序,前端直接消费。 */
  const displaySegments = $derived(note ? note.segments : []);
```

(注:`durationSecs` 继续用 `note.segments` 算最大 end_ms——过滤后若尾段恰为空白段,时长与列表页(读原始文件)差一个空白段,罕见且无害,接受。)

- [ ] **Step 7: npm check**

Run: `npm run check`
Expected: 0 errors 0 warnings。

- [ ] **Step 8: Commit**

```bash
git add src-tauri/src/store/notes.rs src-tauri/src/store/export.rs "src/routes/notes/[id]/+page.svelte"
git commit -m "fix(store): filter+sort 下沉 NoteStore::load,export 与详情页语义统一"
```

---

### Task 3: download_running RAII drop-guard

**Files:**
- Modify: `src-tauri/src/lib.rs`(`stash_model` 附近加 guard 结构体;`download_models` 线程闭包改用 guard;tests 模块加一测)

**Interfaces:**
- Consumes: `AppState.download_running: Arc<AtomicBool>`(既有)。
- Produces: `struct ResetOnDrop(Arc<AtomicBool>)`,lib.rs 私有;Task 4 不依赖它。

**背景**:清位是下载线程闭包尾部一条普通 `store(false)`(lib.rs:813),置位(771)到清位间任意 panic 都让标志永久卡 true,此后一直报"下载已在进行中",只能重启应用。

- [ ] **Step 1: 写失败测试**(lib.rs 底部 `mod tests` 内)

```rust
    #[test]
    fn download_running_resets_even_on_panic() {
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::sync::Arc;
        let flag = Arc::new(AtomicBool::new(true));
        let g = super::ResetOnDrop(flag.clone());
        let h = std::thread::spawn(move || {
            let _g = g;
            panic!("模拟下载线程 panic");
        });
        assert!(h.join().is_err());
        assert!(!flag.load(Ordering::SeqCst), "panic 展开也必须复位标志");
    }
```

- [ ] **Step 2: 编译确认失败**

Run: `cd src-tauri && cargo test download_running_resets`
Expected: 编译错误 `cannot find ResetOnDrop`。

- [ ] **Step 3: 实现 guard 并接线**(lib.rs,`stash_model` 函数之后)

```rust
/// RAII 复位守卫:下载线程无论正常结束还是 panic 展开,download_running 都必然
/// 回 false——否则一次 panic 后"下载已在进行中"永久卡死,只能重启应用。
struct ResetOnDrop(Arc<AtomicBool>);
impl Drop for ResetOnDrop {
    fn drop(&mut self) {
        self.0.store(false, Ordering::SeqCst);
    }
}
```

`download_models` 线程闭包改造(lib.rs:779 起):

```rust
    std::thread::spawn(move || {
        // guard 而非尾部手动清位:中途任何 panic 也必然复位,不卡死后续下载。
        let guard = ResetOnDrop(running);
        let root = models::root();
        ...(中间不动)...
        drop(guard); // 复位先于 done 事件,保持"收到 done 即可再次下载"的时序
        if all_ok {
            emit("all", "done", 0, 0, "");
            // 补齐后立即预载,无需重启即可开录。
            preload_models(recognizer_cache, embedder_cache);
        }
    });
```

删除原 `running.store(false, Ordering::SeqCst);` 行。

- [ ] **Step 4: 跑测试**

Run: `cd src-tauri && cargo test download_running_resets`
Expected: PASS(panic 输出属预期,勿慌)。

- [ ] **Step 5: Commit**

```bash
git add src-tauri/src/lib.rs
git commit -m "fix(models): download_running 改 RAII drop-guard,线程 panic 不再永久卡死下载"
```

---

### Task 4: preload 会话活跃跳过 + 停录补预载

**Files:**
- Modify: `src-tauri/src/lib.rs`(`preload_models` 增 session 参;三个调用点:setup、download 线程、stop_recording 新增)

**Interfaces:**
- Consumes: `AppState.session: Arc<Mutex<Option<ActiveSession>>>`、既有 `preload_models`。
- Produces: `fn preload_models(session: Arc<Mutex<Option<ActiveSession>>>, cache: Arc<Mutex<Option<Box<dyn asr::Recognizer>>>>, embedder_cache: Arc<Mutex<Option<Box<dyn diar::SpeakerEmbedder>>>>)`(签名变更,全部调用点本任务内更新)。

**背景**:开录 `take()` 空槽;录制中下载完成 → preload 见 `is_none()` 又载一份 SenseVoice(数百 MB)与会话手中那份共存(瞬时 2x),停止时 `stash_model` 无条件回灌把预载那份顶掉丢弃——白载。

**无单测**:会话跳过分支需构造 `ActiveSession`(含真实 RecordingHandle,不可测),模型加载需真模型;本任务靠 `cargo build` + 审查 + 冒烟④验证。

- [ ] **Step 1: preload_models 增会话检查**(lib.rs:740 起替换签名与函数体开头)

```rust
/// 后台预载识别器与声纹嵌入器进常驻槽(幂等:槽已有则跳过)。
/// 锁序:预载是唯一嵌套持两槽者——持 recognizer 槽锁期间嵌套获取 embedder 槽锁,
/// 消除间隙内开录线程 take 到空 embedder 的静默降级(详见原 setup 注释)。
fn preload_models(
    session: Arc<Mutex<Option<ActiveSession>>>,
    cache: Arc<Mutex<Option<Box<dyn asr::Recognizer>>>>,
    embedder_cache: Arc<Mutex<Option<Box<dyn diar::SpeakerEmbedder>>>>,
) {
    std::thread::spawn(move || {
        // 会话活跃则整体跳过:开录已 take() 空槽,此刻加载纯属双载(瞬时 2x 内存),
        // 且停录 stash 会把这份顶掉白载;停录收尾会补调预载。session 锁查完即放
        // (锁序纪律:绝不持 session 锁再拿叶子槽锁)。检查后立刻开录的窗口仍可能
        // 双载——用户级操作间隔,可忽略。
        if session.lock().unwrap().is_some() {
            eprintln!("预载跳过:录制会话进行中,停止后自动补载");
            return;
        }
        let mut slot = cache.lock().unwrap();
        ...(其余不动)...
```

- [ ] **Step 2: 更新三个调用点**

setup(lib.rs:852-854 区域):

```rust
            let st = app.state::<AppState>();
            preload_models(st.session.clone(), st.recognizer_cache.clone(), st.embedder_cache.clone());
```

download_models 线程(捕获处加 `let session = state.session.clone();`,与 recognizer_cache 并列;lib.rs:817 调用改):

```rust
            preload_models(session, recognizer_cache, embedder_cache);
```

stop_recording 末尾(status 事件 emit 之后)新增:

```rust
    // 停录补预载:录制中下载完成的模型(预载被活跃跳过)此刻补进空槽;幂等,槽有货即跳。
    preload_models(state.session.clone(), state.recognizer_cache.clone(), state.embedder_cache.clone());
```

- [ ] **Step 3: 编译与全量测试**

Run: `cd src-tauri && cargo build 2>&1 | tail -5 && cargo test`
Expected: build OK 无新警告;测试全过。

- [ ] **Step 4: Commit**

```bash
git add src-tauri/src/lib.rs
git commit -m "fix(models): preload 会话活跃跳过防瞬时双载,停录收尾补预载"
```

---

### Task 5: 解压即时取消(CancelReader)+ 416 免重下复装

**Files:**
- Modify: `src-tauri/src/models/download.rs`(CancelReader;extract_and_install 增 cancel 参;收尾抽 finalize_artifact;416 分支复用;tests 更新 4 处调用 + 新增 2 测)

**Interfaces:**
- Consumes: 既有 `download_artifact` 签名(不变,cancel 已在参数里)。
- Produces: `pub fn extract_and_install(tarball: &Path, root: &Path, dest_dir: &str, files: &[FinalFile], cancel: &AtomicBool) -> anyhow::Result<()>`(增参);`fn finalize_artifact(a: &super::Artifact, root: &Path, part: &Path, cancel: &AtomicBool, progress: &Progress, received: u64, total: u64) -> anyhow::Result<()>`(模块私有)。lib.rs 调用点零改动。

- [ ] **Step 1: 写失败测试**(download.rs tests)

```rust
    #[test]
    fn extract_cancel_is_prompt_and_preserves_tarball() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("models");
        std::fs::create_dir_all(&root).unwrap();
        let tarball = make_tarball(tmp.path(), "sv-dir", &[("model.onnx", b"MODEL")]);
        let files = [ff("sv-dir/model.onnx", b"MODEL")];
        let cancel = AtomicBool::new(true); // 预先置位:首次 read 即断
        let err = extract_and_install(&tarball, &root, "sv-dir", &files, &cancel).unwrap_err();
        assert_eq!(err.to_string(), "cancelled", "取消错误归一,供上层按消息识别");
        assert!(!root.join("sv-dir").exists(), "取消不得半安装");
        assert!(tarball.exists(), "tarball 由调用方保留(供免重下复装)");
        assert!(!tmp_extract_dir(&root).exists(), "解压残留即时清理");
    }
```

- [ ] **Step 2: 编译确认失败**

Run: `cd src-tauri && cargo test extract_cancel`
Expected: 编译错误(extract_and_install 参数个数不符)。

- [ ] **Step 3: 实现 CancelReader + extract_and_install 增参**(download.rs)

`sweep_tmp` 之后加:

```rust
/// 包装 Reader:每次 read 前查取消标志,置位即返回 Err——把取消响应性带进
/// 解压这类长同步调用(unpack 内部逐块拉取,取消在下一次 read 生效,字节级即时)。
/// ErrorKind 用 Other 而非 Interrupted:Interrupted 会被多数 Read 消费者自动重试,
/// 永远断不掉。
struct CancelReader<'a, R: Read> {
    inner: R,
    cancel: &'a AtomicBool,
}

impl<R: Read> Read for CancelReader<'_, R> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        if self.cancel.load(Ordering::Relaxed) {
            return Err(std::io::Error::new(std::io::ErrorKind::Other, "cancelled"));
        }
        self.inner.read(buf)
    }
}
```

`extract_and_install` 签名加 `cancel: &AtomicBool`(files 之后),unpack 行(原 75)替换为:

```rust
    let f = fs::File::open(tarball)?;
    let reader = CancelReader { inner: f, cancel };
    if let Err(e) = tar::Archive::new(bzip2::read::BzDecoder::new(reader)).unpack(&tmp) {
        let _ = fs::remove_dir_all(&tmp);
        // 归一取消错误:上层(download_artifact/lib.rs)以 msg=="cancelled" 识别,
        // 走保留 .part 的取消路径而非删分片的失败路径。
        if cancel.load(Ordering::Relaxed) {
            anyhow::bail!("cancelled");
        }
        return Err(e.into());
    }
```

既有 4 个 extract 测试调用点补第 5 参 `&AtomicBool::new(false)`。

- [ ] **Step 4: 跑测试**

Run: `cd src-tauri && cargo test download::`
Expected: 全 PASS(新测 + 既有 4 测)。

- [ ] **Step 5: 抽 finalize_artifact 并接 416**(download.rs)

`download_artifact` 之前加(原 171-188 的 match 块整体搬入):

```rust
/// 下载完成(或 416 判定本地已有全量 .part)后的收尾:校验/解压安装,成功清 .part。
/// 失败删 .part(脏数据不值得续)——唯 "cancelled" 例外:tarball 完好,保留供复装。
fn finalize_artifact(
    a: &super::Artifact,
    root: &Path,
    part: &Path,
    cancel: &AtomicBool,
    progress: &Progress,
    received: u64,
    total: u64,
) -> anyhow::Result<()> {
    match &a.kind {
        super::ArtifactKind::File => {
            progress(a.id, "verifying", received, total, "");
            if let Err(e) = verify_file(part, &a.files[0]) {
                let _ = fs::remove_file(part);
                return Err(e);
            }
            fs::rename(part, root.join(a.files[0].rel_path))?;
        }
        super::ArtifactKind::TarBz2 { dest_dir } => {
            progress(a.id, "extracting", received, total, "");
            if let Err(e) = extract_and_install(part, root, dest_dir, a.files, cancel) {
                if e.to_string() != "cancelled" {
                    let _ = fs::remove_file(part);
                }
                return Err(e);
            }
            let _ = fs::remove_file(part);
        }
    }
    progress(a.id, "done", received, total, "");
    Ok(())
}
```

`download_artifact` 尾部 match 块替换为:

```rust
    finalize_artifact(a, root, &part, cancel, progress, received, total)
```

(`progress(a.id, "done", ...)` 已随收尾搬入,函数末尾改为直接返回上式。)

416 分支(原 126-129)替换为:

```rust
        // 416 = 偏移 ≥ 服务端全量。两种来源:上次解压被取消(.part 是完好全量
        // tarball,直接收尾复装,免整包重下)或崩溃残留脏分片(收尾校验失败 →
        // finalize 已删分片,报错引导重试,同旧行为)。
        Err(ureq::Error::Status(416, _)) => {
            return finalize_artifact(a, root, &part, cancel, progress, offset, offset).map_err(|e| {
                if e.to_string() == "cancelled" {
                    e
                } else {
                    anyhow::anyhow!("续传偏移越界,残留分片无效已清理,请重试({e})")
                }
            });
        }
```

- [ ] **Step 6: 写 416 复装测试**(download.rs tests)

```rust
    /// 416 免重下复装的核心路径:全量有效 .part 直接 finalize 完成安装(无网络)。
    #[test]
    fn finalize_artifact_installs_valid_full_part_without_network() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("models");
        std::fs::create_dir_all(&root).unwrap();
        let tarball = make_tarball(tmp.path(), "sv-dir", &[("model.onnx", b"MODEL")]);
        let part = root.join("sv.part");
        std::fs::copy(&tarball, &part).unwrap();
        let a = crate::models::Artifact {
            id: "sv",
            label: "测试工件",
            url: "http://unused.invalid/pkg.tar.bz2",
            approx_mb: 1,
            kind: crate::models::ArtifactKind::TarBz2 { dest_dir: "sv-dir" },
            files: Box::leak(vec![ff("sv-dir/model.onnx", b"MODEL")].into_boxed_slice()),
        };
        let noop: &Progress = &|_, _, _, _, _| {};
        finalize_artifact(&a, &root, &part, &AtomicBool::new(false), noop, 0, 0).unwrap();
        assert_eq!(std::fs::read(root.join("sv-dir/model.onnx")).unwrap(), b"MODEL");
        assert!(!part.exists(), "复装成功清 .part");
    }
```

(注:`Artifact` 字段名以 `src-tauri/src/models/mod.rs` 实际定义为准——实现时先查看,字段不符则按实际调整测试构造;静态引用用 `Box::leak` 与既有 `ff` helper 同法。)

- [ ] **Step 7: 跑模块全部测试**

Run: `cd src-tauri && cargo test download::`
Expected: 全 PASS。

- [ ] **Step 8: Commit**

```bash
git add src-tauri/src/models/download.rs
git commit -m "fix(models): 解压阶段即时取消(CancelReader),416/取消后全量 .part 免重下复装"
```

---

### Task 6: 下载卡片单实例化(大小卡切换不重建)

**Files:**
- Modify: `src/routes/record/+page.svelte:43-47`(两分支合并为单实例)

**Interfaces:**
- Consumes: `ModelDownloadCard` 既有 props(`status` / `compact` / `onComplete`),组件零改动(`compact` 已是 `$props()` 响应式,仅模板层消费,onMount 订阅与其无关)。
- Produces: 无。

- [ ] **Step 1: 合并分支**(record/+page.svelte:43-47 替换)

```svelte
  <!-- 单实例:compact 由 recording_ready 驱动。若拆成两个 if 分支,识别模型下完
       切小提示条时组件会销毁重建,进行中的下载进度/订阅状态全部清零。 -->
  {#if models && !(models.recording_ready && models.diarization_ready)}
    <ModelDownloadCard status={models} compact={models.recording_ready} onComplete={refreshModels} />
  {/if}
```

- [ ] **Step 2: npm check + build**

Run: `npm run check && npm run build`
Expected: 0 errors 0 warnings;build OK。

- [ ] **Step 3: Commit**

```bash
git add src/routes/record/+page.svelte
git commit -m "fix(ui): 模型下载卡片单实例化,大小卡切换不再重建组件丢进度"
```

---

### Task 7: 全量验证 + 文档收尾

**Files:**
- Modify: `.superpowers/sdd/progress.md`(记 backlog 硬化节)

- [ ] **Step 1: 全量验证**

```bash
cd src-tauri && cargo test 2>&1 | tail -3 && cargo build 2>&1 | tail -3
cd .. && npm run check && npm run build 2>&1 | tail -3
```

Expected: cargo test 全过(≥100 + 新增 5);check 0/0;两端 build OK。

- [ ] **Step 2: progress.md 记账**(文件末尾加节)

```markdown
## P5 backlog 硬化(分支 p5-backlog-hardening)
- 6 项终审 defer 全落地:store 编辑锁 / load 下沉 filter+sort / download_running RAII / preload 会话跳过+停录补载 / 解压 CancelReader+416 复装 / 下载卡片单实例。
- spec: docs/superpowers/specs/2026-07-04-voice-notes-p5-backlog-hardening-design.md
```

- [ ] **Step 3: Commit**

```bash
git add .superpowers/sdd/progress.md
git commit -m "docs(sdd): 记录 P5 backlog 硬化"
```

(push、PR、全分支终审由执行流程的收尾阶段处理,不在本任务内。)
