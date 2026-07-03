# P3.5 UX 重构 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 识别器常驻复用实现秒开;左右布局(侧栏列表+主区域)+ 一键开录;全局录制状态根治导航丢状态。

**Architecture:** Rust 侧加 `recognizer_cache`(启动预载、开录取用、停录归还,`run_asr_worker`/`RecordingHandle::stop` 返还识别器);前端把事件监听收敛到 runes 全局 store(`recording.svelte.ts`),`+layout.svelte` 挂侧栏(`Sidebar.svelte`,含主录制按钮与列表),`/record` 与 `/` 精简。

**Tech Stack:** 复用 Rust + Tauri 2 + SvelteKit(Svelte 5 runes)。无新依赖。

**Spec:** `docs/superpowers/specs/2026-07-03-voice-notes-p3.5-ux-rework-design.md`

## Global Constraints

- `Recognizer: Send` 已有约束;cache 类型 `Arc<Mutex<Option<Box<dyn asr::Recognizer>>>>`。
- **cache 锁是叶子锁**:绝不与 running/generation/session_slot 嵌套持有;预载线程持锁加载(使开录 take() 自然阻塞至就绪、永不双重加载)。
- 归还路径全覆盖:正常 stop / mic 缺失 / 竞态孤儿 / `start_session` Err(错误类型携带 recognizer 返还)。asr 线程 panic → 返 None,cache 留空,下次现场加载。
- 事件契约不变(partial/final/status/storage);前端监听只在 layout `init()` 注册一次,应用生命周期不解绑。
- stopped+note_id → 跳详情的导航放在全局 store 的 status 监听里。
- P3 功能行为不变:列表过滤/改名/两步删除/徽章、录制横幅、详情只读+导出。
- 测试命令:`cargo test --manifest-path src-tauri/Cargo.toml`;前端 `npm run check`(0 errors)+ `npm run build`。
- 每 Task 一个 commit,message 末尾:`Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>`。

---

## 文件结构(P3.5 结束时)

```
src-tauri/src/
  session.rs                  # 改:run_asr_worker 返还 recognizer;StartError;stop() -> Option<Box<dyn Recognizer>>
  lib.rs                      # 改:recognizer_cache + setup 预载 + 取用/归还
src/
  lib/
    recording.svelte.ts       # 新:全局录制状态 store
    Sidebar.svelte            # 新:侧栏(主按钮+过滤+列表,自旧列表页迁移)
  routes/
    +layout.svelte            # 新:左右布局骨架
    +page.svelte              # 改:跳最近笔记/空态(列表逻辑移入 Sidebar)
    record/+page.svelte       # 改:读全局 store,去本地监听与按钮
    notes/[id]/+page.svelte   # 不变
```

---

### Task 1: 识别器常驻(session.rs + lib.rs)

**Files:**
- Modify: `src-tauri/src/session.rs`
- Modify: `src-tauri/src/lib.rs`
- Modify: `src-tauri/src/store/writer.rs`(仅集成测试调用点适配)

**Interfaces:**
- Produces:
  - `run_asr_worker(...) -> Box<dyn Recognizer>`(排干后返还)
  - `pub struct StartError { pub error: anyhow::Error, pub recognizer: Box<dyn Recognizer> }`
  - `start_session(...) -> Result<SessionStart, StartError>`
  - `RecordingHandle::stop(self) -> Option<Box<dyn Recognizer>>`(asr 线程 panic 时 None)
  - `AppState.recognizer_cache: Arc<Mutex<Option<Box<dyn asr::Recognizer>>>>` + setup 预载线程

- [ ] **Step 1: 写失败测试(session.rs)**

在 `session.rs` 的 `mod session_tests` 中追加:

```rust
    #[test]
    fn stop_returns_recognizer_for_reuse() {
        let sources: Vec<(Source, Box<dyn AudioCapture>, Box<dyn Segmenter>)> = vec![(
            Source::Mic,
            Box::new(IdlingCapture::from_fixture()),
            Box::new(MockSegmenter::new(2000)),
        )];
        let start = start_session(sources, Box::new(CountingRecognizer), 16000, 4000, |_, _, _, _| {}, |_, _| {})
            .expect("start_session");
        let returned = start.handle.stop();
        assert!(returned.is_some(), "停止后应返还 recognizer 供复用");
    }

    #[test]
    fn all_sources_fail_returns_recognizer_in_err() {
        struct FailingCapture;
        impl AudioCapture for FailingCapture {
            fn start(&mut self, _sink: Sender<AudioFrame>) -> anyhow::Result<()> {
                anyhow::bail!("unauthorized: nope")
            }
            fn stop(&mut self) {}
        }
        let sources: Vec<(Source, Box<dyn AudioCapture>, Box<dyn Segmenter>)> =
            vec![(Source::System, Box::new(FailingCapture), Box::new(MockSegmenter::new(8000)))];
        let r = start_session(sources, Box::new(CountingRecognizer), 16000, 4000, |_, _, _, _| {}, |_, _| {});
        let err = match r {
            Ok(_) => panic!("无源可启动应返回 Err"),
            Err(e) => e,
        };
        assert!(err.error.to_string().contains("没有可用音频源"));
        let _reusable: Box<dyn Recognizer> = err.recognizer; // Err 携带 recognizer 返还
    }
```

同时删除旧的 `all_sources_fail_returns_err` 测试(被上面第二个替代)。

- [ ] **Step 2: 运行确认编译失败**

Run: `cargo test --manifest-path src-tauri/Cargo.toml session`
Expected: FAIL — `stop()` 无返回值 / `StartError` 未定义。

- [ ] **Step 3: 实现 session.rs 返还链路**

`run_asr_worker`:签名改为返回 recognizer,循环 `Disconnected => break` 后:

```rust
pub fn run_asr_worker(
    mut recognizer: Box<dyn Recognizer>,
    finals_rx: Receiver<FinalJob>,
    partial_slots: Vec<(Source, Arc<Mutex<Option<PartialJob>>>)>,
    mut on_final: impl FnMut(Source, String, u64, u64),
    mut on_partial: impl FnMut(Source, String),
) -> Box<dyn Recognizer> {
```

函数末尾(loop 之后)加 `recognizer`。

`RecordingHandle`:

```rust
pub struct RecordingHandle {
    captures: Vec<Box<dyn AudioCapture>>,
    workers: Vec<std::thread::JoinHandle<()>>,
    asr: Option<std::thread::JoinHandle<Box<dyn Recognizer>>>,
}

impl RecordingHandle {
    /// 优雅停止：停各 capture（关帧通道）→ 分段 worker flush 尾段后退出并 join
    /// →（其 finals 发送端随之 drop）ASR worker 排干剩余 finals 后退出并 join，
    /// 返还 recognizer 供复用（asr 线程 panic 时返 None，调用方现场重载兜底）。
    pub fn stop(mut self) -> Option<Box<dyn Recognizer>> {
        for c in self.captures.iter_mut() {
            c.stop();
        }
        for w in self.workers.drain(..) {
            let _ = w.join();
        }
        match self.asr.take() {
            Some(a) => match a.join() {
                Ok(r) => Some(r),
                Err(_) => {
                    eprintln!("RecordingHandle::stop: asr 线程异常退出（panic），识别器不回收");
                    None
                }
            },
            None => None,
        }
    }
}
```

`StartError` + `start_session`(在 `SessionStart` 定义后):

```rust
/// start_session 失败时携带 recognizer 返还，避免常驻识别器在错误路径丢失。
pub struct StartError {
    pub error: anyhow::Error,
    pub recognizer: Box<dyn Recognizer>,
}

impl std::fmt::Debug for StartError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "StartError({})", self.error)
    }
}
```

`start_session` 返回类型改 `Result<SessionStart, StartError>`;`active.is_empty()` 分支改:

```rust
    if active.is_empty() {
        return Err(StartError {
            error: anyhow::anyhow!("没有可用音频源可启动: {failed:?}"),
            recognizer,
        });
    }
```

asr 线程闭包改为返回值:

```rust
    let asr = std::thread::spawn(move || {
        run_asr_worker(recognizer, finals_rx, slots, on_final, on_partial)
    });
```

既有测试适配:4 个直接调用 `run_asr_worker(...)` 的测试前加 `let _ =`(丢弃返还值;`services_latest_partial_when_idle` 里 spawn 闭包内同样 `let _ =`);`merges_two_sources_and_stops_cleanly` 的 `start.handle.stop();` 改 `let _ = start.handle.stop();`。

`store/writer.rs` 的 `full_session_persists_every_final`:`start.handle.stop();` 改 `let _ = start.handle.stop();`,`start_session(...)` 的 `.expect("start_session")` 保持(Err 类型变了但 expect 需要 Debug——StartError 已手写 Debug)。

- [ ] **Step 4: 实现 lib.rs 常驻取用/归还**

`AppState`:

```rust
#[derive(Default)]
struct AppState {
    running: Arc<Mutex<bool>>,
    generation: Arc<Mutex<u64>>,
    session: Arc<Mutex<Option<ActiveSession>>>,
    /// 常驻识别器（启动预载、开录取用、停录归还）。叶子锁：绝不与上面三把锁嵌套持有；
    /// 预载线程持锁加载，使开录 take() 自然阻塞至就绪且永不双重加载。
    recognizer_cache: Arc<Mutex<Option<Box<dyn asr::Recognizer>>>>,
}
```

辅助函数(放 `abort_or_finalize` 旁):

```rust
/// 归还识别器进常驻槽（None = asr 线程 panic 等，不回收）。
fn stash_recognizer(
    cache: &Arc<Mutex<Option<Box<dyn asr::Recognizer>>>>,
    r: Option<Box<dyn asr::Recognizer>>,
) {
    if let Some(r) = r {
        *cache.lock().unwrap() = Some(r);
    }
}

fn sense_voice_dir() -> PathBuf {
    models_dir().join("sherpa-onnx-sense-voice-zh-en-ja-ko-yue-2024-07-17")
}
```

`start_recording`:command 开头加 `let recognizer_cache = state.recognizer_cache.clone();`(与其它 clone 并列,move 进线程)。加载线程第 1 步替换为:

```rust
        // 1) 取常驻识别器（预载中会在锁上等待）；槽空则现场加载兜底。
        let recognizer = match recognizer_cache.lock().unwrap().take() {
            Some(r) => r,
            None => match asr::sense_voice::SenseVoiceRecognizer::new(&sense_voice_dir()) {
                Ok(r) => Box::new(r) as Box<dyn asr::Recognizer>,
                Err(e) => return fail(&app, &running, &generation, my_gen, format!("error: {e}")),
            },
        };
```

(原 `sv_dir` 两行删除,复用 `sense_voice_dir()`。)

三处归还:

```rust
                if !start.active.contains(&Source::Mic) {
                    stash_recognizer(&recognizer_cache, start.handle.stop()); // 先排干可能已产生的 system finals
                    abort_or_finalize(&writer);
                    ...
                }
```

```rust
                if !*running_guard || *gen_guard != my_gen {
                    drop(gen_guard);
                    drop(running_guard);
                    stash_recognizer(&recognizer_cache, start.handle.stop());
                    abort_or_finalize(&writer); // 被 stop/新 start 抢先：有内容则收尾保全（flush 失败时留 recording）
                    return;
                }
```

```rust
            Err(se) => {
                stash_recognizer(&recognizer_cache, Some(se.recognizer));
                abort_or_finalize(&writer);
                return fail(&app, &running, &generation, my_gen, format!("error: {}", se.error));
            }
```

`stop_recording`(需 `state` 里取 cache;在 `s.handle.stop()` 处改):

```rust
    if let Some(s) = sess {
        let returned = s.handle.stop(); // 排干 finals：所有 append 在此完成
        stash_recognizer(&state.recognizer_cache, returned);
        note_id = s.note_id;
        ...
```

`run()` 加 setup 预载(`.manage` 之后):

```rust
    tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .manage(AppState::default())
        .setup(|app| {
            // 启动预载识别器：持锁加载，开录若赶上预载会在锁上等待至就绪。
            let cache = app.state::<AppState>().recognizer_cache.clone();
            std::thread::spawn(move || {
                let mut slot = cache.lock().unwrap();
                if slot.is_none() {
                    match asr::sense_voice::SenseVoiceRecognizer::new(&sense_voice_dir()) {
                        Ok(r) => *slot = Some(Box::new(r) as Box<dyn asr::Recognizer>),
                        Err(e) => eprintln!("识别器预载失败（将在开录时现场加载）: {e}"),
                    }
                }
            });
            Ok(())
        })
```

- [ ] **Step 5: 全量测试**

Run: `cargo test --manifest-path src-tauri/Cargo.toml`
Expected: 全部 PASS(42 个,新增 2、删除 1)。
Run: `cargo build --manifest-path src-tauri/Cargo.toml`
Expected: 无新增 warning。

- [ ] **Step 6: Commit**

```bash
git add src-tauri/src/session.rs src-tauri/src/lib.rs src-tauri/src/store/writer.rs
git commit -m "P3.5 Task 1: 识别器常驻复用（启动预载/开录取用/停录归还）

Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>"
```

---

### Task 2: 全局录制状态 store + 布局骨架 + 侧栏

**Files:**
- Create: `src/lib/recording.svelte.ts`
- Create: `src/lib/Sidebar.svelte`
- Create: `src/routes/+layout.svelte`
- Modify: `src/routes/+page.svelte`(列表逻辑移入 Sidebar,改为跳最近笔记/空态)

**Interfaces:**
- Consumes: `$lib/events`(onPartial/onFinal/onStatus/onStorage)、`$lib/notes`、command `recording_status`。
- Produces(Task 3 依赖):`recording` store——getter:`status/systemAudio/noteId/finals/partialMic/partialSystem/storageDegraded/statusVersion/isRecording`;方法:`init()/start()/stop()`。

- [ ] **Step 1: 新建 recording.svelte.ts**

```ts
import { invoke } from "@tauri-apps/api/core";
import { goto } from "$app/navigation";
import {
  onPartial,
  onStatus,
  onFinal,
  onStorage,
  type Source,
  type SystemAudio,
  type StatusEvent,
} from "./events";

export type Line = { source: Source; text: string };

let status = $state("idle");
let systemAudio = $state<SystemAudio>("");
let noteId = $state("");
let finals = $state<Line[]>([]);
let partialMic = $state("");
let partialSystem = $state("");
let storageDegraded = $state(false);
/** recording/stopped/error 翻转时 +1，侧栏据此刷新列表。 */
let statusVersion = $state(0);

let initialized = false;

/**
 * 全局录制状态：事件监听在 layout 挂载时注册一次，应用生命周期内不解绑。
 * 状态跨路由存活——侧栏按钮与录制页共读同一份。
 */
export const recording = {
  get status() { return status; },
  get systemAudio() { return systemAudio; },
  get noteId() { return noteId; },
  get finals() { return finals; },
  get partialMic() { return partialMic; },
  get partialSystem() { return partialSystem; },
  get storageDegraded() { return storageDegraded; },
  get statusVersion() { return statusVersion; },
  get isRecording() { return status === "recording"; },

  /** 幂等：注册事件监听 + 用 recording_status 重建冷启动状态。 */
  async init() {
    if (initialized) return;
    initialized = true;

    onPartial((e) => {
      if (e.source === "mic") partialMic = e.text;
      else partialSystem = e.text;
    });
    onFinal((e) => {
      if (e.text.trim()) finals = [...finals, { source: e.source, text: e.text }];
      if (e.source === "mic") partialMic = "";
      else partialSystem = "";
    });
    onStatus((e) => {
      status = e.state;
      systemAudio = e.system_audio;
      if (e.state === "recording") {
        noteId = e.note_id;
        finals = [];
        partialMic = "";
        partialSystem = "";
        storageDegraded = false;
        statusVersion++;
      } else if (e.state === "stopped" || e.state.startsWith("error:")) {
        partialMic = "";
        partialSystem = "";
        storageDegraded = false;
        statusVersion++;
        if (e.state === "stopped" && e.note_id) {
          goto(`/notes/${e.note_id}`);
        }
      }
    });
    onStorage((e) => {
      storageDegraded = e.state === "degraded";
    });

    // 事件非粘性：冷启动/刷新时主动查询一次。返回 idle 不覆盖，避免与真实事件竞争。
    const s = await invoke<StatusEvent>("recording_status");
    if (s.state === "recording") {
      status = s.state;
      systemAudio = s.system_audio;
      noteId = s.note_id;
    }
  },

  /** 一键开录。成功由 "recording" 事件驱动 UI；这里只处理同步拒绝。 */
  async start() {
    try {
      await invoke("start_recording");
    } catch (err) {
      status = `error: ${err}`;
    }
  },

  async stop() {
    await invoke("stop_recording");
  },
};
```

- [ ] **Step 2: 新建 +layout.svelte**

```svelte
<script lang="ts">
  import { onMount } from "svelte";
  import Sidebar from "$lib/Sidebar.svelte";
  import { recording } from "$lib/recording.svelte";

  let { children } = $props();

  onMount(() => {
    recording.init();
  });
</script>

<div class="shell">
  <Sidebar />
  <main class="main">
    {@render children()}
  </main>
</div>

<style>
  :global(body) {
    margin: 0;
  }
  .shell {
    display: flex;
    height: 100vh;
    font-family: -apple-system, system-ui, sans-serif;
  }
  .main {
    flex: 1;
    overflow-y: auto;
    min-width: 0;
  }
</style>
```

- [ ] **Step 3: 新建 Sidebar.svelte**

逻辑自现 `src/routes/+page.svelte` 列表页迁移(过滤/改名/两步删除/徽章行为不变),加主录制按钮与刷新联动:

```svelte
<script lang="ts">
  import { page } from "$app/stores";
  import { goto } from "$app/navigation";
  import { recording } from "$lib/recording.svelte";
  import {
    listNotes,
    renameNote,
    deleteNote,
    formatDate,
    formatDuration,
    type NoteSummary,
  } from "$lib/notes";

  let notes = $state<NoteSummary[]>([]);
  let query = $state("");
  let error = $state("");
  let editingId = $state<string | null>(null);
  let editingTitle = $state("");
  let confirmingDeleteId = $state<string | null>(null);

  const filtered = $derived(
    query.trim() ? notes.filter((n) => n.title.toLowerCase().includes(query.trim().toLowerCase())) : notes,
  );

  async function refresh() {
    try {
      notes = await listNotes();
      error = "";
    } catch (e) {
      error = `加载失败: ${e}`;
    }
  }

  // 挂载时 + 录制状态翻转时刷新列表（新笔记出现/徽章变化）。
  $effect(() => {
    void recording.statusVersion;
    refresh();
  });

  async function toggleRecording() {
    if (recording.isRecording) {
      await recording.stop(); // 跳详情由全局 status 监听驱动
    } else {
      await recording.start();
      goto("/record");
    }
  }

  function beginRename(n: NoteSummary) {
    editingId = n.id;
    editingTitle = n.title;
  }

  async function commitRename() {
    if (!editingId) return;
    const id = editingId;
    editingId = null;
    try {
      await renameNote(id, editingTitle);
    } catch (e) {
      error = `改名失败: ${e}`;
    }
    await refresh();
  }

  async function confirmDelete(id: string) {
    confirmingDeleteId = null;
    try {
      await deleteNote(id);
      // 删的是当前正在看的笔记 → 回首页
      if ($page.url.pathname === `/notes/${id}`) {
        goto("/");
      }
    } catch (e) {
      error = `删除失败: ${e}`;
    }
    await refresh();
  }

  const stateBadge = (s: NoteSummary["state"]) =>
    s === "active" ? "录制中" : s === "recording" ? "已中断" : "";
</script>

<aside class="sidebar">
  <button class="record-btn" class:recording={recording.isRecording} onclick={toggleRecording}>
    {recording.isRecording ? "■ 停止" : "● 开始录制"}
  </button>

  <input class="search" type="search" placeholder="按标题过滤…" bind:value={query} />

  {#if error}
    <div class="banner">{error}</div>
  {/if}

  {#if filtered.length === 0}
    <p class="hint">{notes.length === 0 ? "还没有笔记" : "没有匹配的笔记"}</p>
  {/if}

  <ul class="list">
    {#each filtered as n (n.id)}
      <li class="item" class:current={$page.url.pathname === `/notes/${n.id}`}>
        <div class="main-line">
          {#if editingId === n.id}
            <!-- svelte-ignore a11y_autofocus -->
            <input
              class="rename"
              autofocus
              bind:value={editingTitle}
              onkeydown={(e) => {
                if (e.key === "Enter") commitRename();
                if (e.key === "Escape") editingId = null;
              }}
              onblur={commitRename}
            />
          {:else}
            <a class="title" href={n.state === "active" ? "/record" : `/notes/${n.id}`}>
              {n.title}
              {#if stateBadge(n.state)}
                <span class="state" class:interrupted={n.state === "recording"} class:active={n.state === "active"}>
                  {stateBadge(n.state)}
                </span>
              {/if}
            </a>
          {/if}
          <span class="meta">{formatDate(n.started_at)} · {formatDuration(n.duration_secs)}</span>
        </div>
        <div class="actions">
          <button class="link" onclick={() => beginRename(n)}>改名</button>
          {#if confirmingDeleteId === n.id}
            <button class="link danger" onclick={() => confirmDelete(n.id)}>确认删除</button>
            <button class="link" onclick={() => (confirmingDeleteId = null)}>取消</button>
          {:else}
            <button class="link" onclick={() => (confirmingDeleteId = n.id)}>删除</button>
          {/if}
        </div>
      </li>
    {/each}
  </ul>
</aside>

<style>
  .sidebar {
    width: 280px;
    flex-shrink: 0;
    display: flex;
    flex-direction: column;
    border-right: 1px solid #e5e5e7;
    background: #fafafa;
    padding: 0.75rem;
    box-sizing: border-box;
    overflow-y: auto;
  }
  .record-btn {
    border: none;
    border-radius: 8px;
    padding: 0.6em 1em;
    font-size: 1em;
    font-weight: 600;
    cursor: pointer;
    color: #fff;
    background: #396cd8;
  }
  .record-btn.recording {
    background: #c0392b;
  }
  .search {
    box-sizing: border-box;
    width: 100%;
    margin: 0.75rem 0;
    padding: 0.4em 0.7em;
    border-radius: 8px;
    border: 1px solid #ccc;
    font-size: 0.9em;
  }
  .list {
    list-style: none;
    margin: 0;
    padding: 0;
  }
  .item {
    padding: 0.55rem 0.4rem;
    border-bottom: 1px solid #e5e5e7;
  }
  .item.current {
    background: #eef2fb;
    border-radius: 6px;
  }
  .main-line {
    display: flex;
    flex-direction: column;
    gap: 0.1rem;
    min-width: 0;
  }
  .title {
    color: inherit;
    text-decoration: none;
    font-weight: 600;
    font-size: 0.92em;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
  .title:hover {
    color: #396cd8;
  }
  .rename {
    font-size: 0.92em;
    padding: 0.15em 0.3em;
    border-radius: 6px;
    border: 1px solid #396cd8;
  }
  .meta {
    color: #888;
    font-size: 0.75em;
  }
  .state {
    font-size: 0.72em;
    font-weight: 600;
    border-radius: 6px;
    padding: 0.05em 0.4em;
    margin-left: 0.35em;
    vertical-align: middle;
    color: #fff;
  }
  .state.interrupted {
    background: #d88a39;
  }
  .state.active {
    background: #c0392b;
  }
  .actions {
    display: flex;
    gap: 0.25rem;
    margin-top: 0.15rem;
  }
  .link {
    background: none;
    border: none;
    color: #396cd8;
    cursor: pointer;
    padding: 0.1em 0.25em;
    font-size: 0.78em;
    box-shadow: none;
  }
  .link.danger {
    color: #c0392b;
    font-weight: 600;
  }
  .banner {
    background: #fff4e5;
    border: 1px solid #f0c98a;
    color: #8a5a00;
    border-radius: 8px;
    padding: 0.5rem 0.6rem;
    margin-bottom: 0.5rem;
    font-size: 0.85rem;
  }
  .hint {
    color: #aaa;
    font-size: 0.85em;
  }
  @media (prefers-color-scheme: dark) {
    .sidebar {
      background: #1e1e1e;
      border-color: #3a3a3a;
    }
    .item {
      border-color: #3a3a3a;
    }
    .item.current {
      background: #2a3348;
    }
    .search,
    .rename {
      background: #2a2a2a;
      border-color: #444;
      color: #f0f0f0;
    }
    .banner {
      background: #3a2e18;
      border-color: #6b5426;
      color: #e8c88a;
    }
    .hint {
      color: #555;
    }
  }
</style>
```

- [ ] **Step 4: 根路由改为跳最近笔记/空态**

`src/routes/+page.svelte` 全文替换:

```svelte
<script lang="ts">
  import { onMount } from "svelte";
  import { goto } from "$app/navigation";
  import { listNotes } from "$lib/notes";

  let empty = $state(false);

  onMount(async () => {
    try {
      const notes = await listNotes();
      if (notes.length > 0) {
        goto(`/notes/${notes[0].id}`, { replaceState: true });
      } else {
        empty = true;
      }
    } catch {
      empty = true;
    }
  });
</script>

{#if empty}
  <div class="empty">
    <p>还没有会议笔记。</p>
    <p class="hint">点击左上角「● 开始录制」来第一场。</p>
  </div>
{/if}

<style>
  .empty {
    height: 100%;
    display: flex;
    flex-direction: column;
    align-items: center;
    justify-content: center;
    color: #888;
  }
  .hint {
    color: #aaa;
    font-size: 0.9em;
  }
</style>
```

- [ ] **Step 5: 检查**

Run: `npm run check`
Expected: 0 errors。(此步 record 页仍是旧版、自带监听,与全局 store 并存不冲突——Task 3 收敛。)
Run: `npm run build`
Expected: 成功。

- [ ] **Step 6: Commit**

```bash
git add src/lib/recording.svelte.ts src/lib/Sidebar.svelte src/routes/+layout.svelte src/routes/+page.svelte
git commit -m "P3.5 Task 2: 全局录制状态 store + 左右布局骨架 + 侧栏（一键开录）

Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>"
```

---

### Task 3: /record 精简(读全局 store)

**Files:**
- Modify: `src/routes/record/+page.svelte`(全文替换)

**Interfaces:**
- Consumes: `recording` store(Task 2)。

- [ ] **Step 1: 全文替换 record/+page.svelte**

```svelte
<script lang="ts">
  import { openUrl } from "@tauri-apps/plugin-opener";
  import { recording } from "$lib/recording.svelte";
  import type { Source } from "$lib/events";

  const label = (s: Source) => (s === "mic" ? "我" : "对方");

  function isError(s: string) {
    return s.startsWith("error:");
  }
  async function openScreenRecordingSettings() {
    await openUrl(
      "x-apple.systempreferences:com.apple.preference.security?Privacy_ScreenCapture",
    );
  }
</script>

<div class="container">
  <h1>实时转写</h1>
  <p class="status" class:error={isError(recording.status)}>状态：{recording.status}</p>

  {#if recording.isRecording && recording.systemAudio !== "on" && recording.systemAudio !== ""}
    <div class="banner">
      系统声音不可用（未授权屏幕录制）。仅麦克风在录。
      <button class="link" onclick={openScreenRecordingSettings}>打开系统设置</button>
      <span class="hint">授权后重新开录生效。</span>
    </div>
  {/if}

  {#if recording.storageDegraded}
    <div class="banner">落盘异常：内容暂存内存并自动重试，请检查磁盘空间。录制不受影响。</div>
  {/if}

  <div class="transcript">
    {#each recording.finals as line}
      <p class="final">
        <span class="badge" class:mic={line.source === "mic"} class:system={line.source === "system"}>
          {label(line.source)}
        </span>
        {line.text}
      </p>
    {/each}
    {#if recording.partialMic}
      <p class="partial"><span class="badge mic">我</span>{recording.partialMic}</p>
    {/if}
    {#if recording.partialSystem}
      <p class="partial"><span class="badge system">对方</span>{recording.partialSystem}</p>
    {/if}
    {#if recording.finals.length === 0 && !recording.partialMic && !recording.partialSystem}
      <p class="hint">（开始说话…）</p>
    {/if}
  </div>
</div>

<style>
  .container {
    padding: 1.5rem;
  }

  h1 {
    margin: 0 0 0.25rem;
  }

  .status {
    color: #666;
    margin: 0 0 1rem;
  }

  .status.error {
    color: #c0392b;
    font-weight: 600;
  }

  .transcript {
    min-height: 8rem;
    background: #f5f5f7;
    border-radius: 8px;
    padding: 1rem;
    font-size: 1.1rem;
    line-height: 1.6;
  }

  .transcript p {
    margin: 0 0 0.25rem 0;
  }

  .final {
    color: #1a1a1a;
  }

  .partial {
    color: #888;
    font-style: italic;
  }

  .hint {
    color: #aaa;
  }

  .badge {
    display: inline-block;
    min-width: 2.2em;
    text-align: center;
    font-size: 0.75em;
    font-weight: 600;
    border-radius: 6px;
    padding: 0.05em 0.4em;
    margin-right: 0.4em;
    color: #fff;
  }
  .badge.mic { background: #396cd8; }
  .badge.system { background: #2e9e5b; }

  .banner {
    background: #fff4e5;
    border: 1px solid #f0c98a;
    color: #8a5a00;
    border-radius: 8px;
    padding: 0.6rem 0.8rem;
    margin: 0.5rem 0 1rem;
    font-size: 0.95rem;
  }
  .banner .link {
    background: none;
    border: none;
    color: #396cd8;
    text-decoration: underline;
    cursor: pointer;
    padding: 0 0.2em;
    box-shadow: none;
    font-size: inherit;
  }
  .banner .hint { color: #a07a3a; }

  @media (prefers-color-scheme: dark) {
    .status {
      color: #aaa;
    }
    .transcript {
      background: #2a2a2a;
    }
    .final {
      color: #f0f0f0;
    }
    .partial {
      color: #888;
    }
    .hint {
      color: #555;
    }
    .banner { background: #3a2e18; border-color: #6b5426; color: #e8c88a; }
    .banner .hint { color: #c9a866; }
  }
</style>
```

(去掉:本地事件监听、`recording_status` 查询(store 已做)、开始/停止按钮(移侧栏)、返回链接(侧栏常驻)。)

- [ ] **Step 2: 检查**

Run: `npm run check`
Expected: 0 errors(旧 a11y warning 随 h1 onclick 留在详情页,数量不增)。
Run: `npm run build`
Expected: 成功。

- [ ] **Step 3: Commit**

```bash
git add src/routes/record/+page.svelte
git commit -m "P3.5 Task 3: /record 精简为全局 store 消费者

Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>"
```

---

### Task 4: 端到端验证 + 人工冒烟(需真人)

- [ ] **Step 1: 全量自动验证**

```bash
cargo test --manifest-path src-tauri/Cargo.toml
npm run check && npm run build
```

Expected: 全部通过。

- [ ] **Step 2: 人工冒烟清单**

`npm run tauri dev`(注意 Rust 侧改动需等重新编译):

1. 启动数秒后点侧栏「● 开始录制」→ **即刻**进入录制(首次可能等预载收尾 1-2 秒);说话出转写。
2. 侧栏按钮变「■ 停止」;点击停止 → 跳详情。
3. **再次点「开始录制」→ 应几乎瞬时开录**(识别器复用,无模型加载)。
4. 录制中点侧栏其它笔记 → 主区域看详情,录制不断(侧栏「录制中」徽章);点「录制中」项回 /record,转写流完整(含离开期间的段);停止正常。
5. 冷启动:`/` 自动打开最近一场笔记;删除当前查看的笔记 → 回 `/` → 自动跳下一场。
6. P3 遗留冒烟:崩溃恢复(`kill -9` → 已中断)、导出 md/txt、改名、过滤。

- [ ] **Step 3: 记录进度**

冒烟结果记入 `.superpowers/sdd/progress.md`(P3.5 小节)。

---

## Self-Review 记录

- **Spec 覆盖**:识别器常驻(T1:预载/取用/四条归还路径)、左右布局+一键开录(T2)、全局状态+stopped 导航入 store(T2)、/record 精简(T3)、`/` 跳最近笔记(T2 Step 4)、删除当前笔记回 `/`(T2 Sidebar confirmDelete)——spec §1-§5 全覆盖。
- **占位符**:无。
- **类型一致性**:`stop() -> Option<Box<dyn Recognizer>>` T1 定义、lib.rs 三处 + stop_recording 调用一致;`StartError{error, recognizer}` 与 lib.rs Err 臂一致;store getter 命名与 T3 消费一致;`statusVersion` 与 Sidebar `$effect` 一致。
- **注意**:T2 结束时 record 旧页与全局 store 双监听并存(finals 双份追加互不干扰,因各自独立 state),T3 立即收敛,属可接受中间态。
