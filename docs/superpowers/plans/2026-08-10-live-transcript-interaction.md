# 实时转写页交互升级 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 录制页实时转写从纯只读升级为可交互:控制条状态醒目化+双通道电平+停止防误触,页内搜索/说话人过滤/迷你时间轴回看,以及录制中当场改说话人/编辑文本。

**Architecture:** 前端 Svelte 5(runes)+ Tauri IPC;后端 lifecycle actor 单写者模型——live 编辑经新增 Msg 路由到 actor 在 `owned.writer`(NoteWriter)上串行执行,与 ASR 定稿追加天然互斥;冷编辑路径(NoteStore+flock)不动。电平事件扩展 source 字段覆盖系统声。seq 从落盘层透传到 FinalEvent 作为编辑锚点。

**Tech Stack:** Rust(Tauri 2/crossbeam/serde)、Svelte 5、vitest、cargo test。

**Spec:** `docs/superpowers/specs/2026-08-10-live-transcript-interaction-design.md`

## Global Constraints

- 提交信息**不加** Claude co-author trailer(仓库全史已清除,红线)。
- **勿跑全量 `cargo fmt`**(仓库约定,只格式化自己改的行)。
- i18n:所有新文案 zh/en 双侧齐平(`src/lib/i18n/dict/`,parity 哨兵测试会抓漏);Rust 侧用户可见错误用 `tr!` 宏。
- 前后端捆绑发布:IPC/事件结构变更无需兼容层,但同一 Task 内前后端要一起改完。
- Svelte 5 runes 语法($state/$derived/$effect);`{@const}` 只能是 `{#if}`/`{:else}`/`{#each}` 等的直接子级。
- 破坏性/二段确认交互沿用 #84 警示胶囊模式(warning-tint 行内淡入,不跳版)。
- reduced-motion:所有新动画在 `prefers-reduced-motion: reduce` 下退化为静止。

---

### Task 1: 后端 LevelEvent 双通道(source 字段 + 系统声电平)

**Files:**
- Modify: `src-tauri/src/ipc.rs:25-29`(LevelEvent)
- Modify: `src-tauri/src/session.rs:1959`(签名)、`src-tauri/src/session.rs:1990`(per-source 回调)
- Modify: `src-tauri/src/lib.rs:1690-1694`(emit 闭包)
- Test: `src-tauri/src/ipc.rs`(新增序列化契约测试,文件内 tests 模块;无则新建)

**Interfaces:**
- Produces: `LevelEvent { source: String("mic"|"system"), rms: f32 }`,事件名 "level" 不变,mic/system 各 ~10Hz。
- `session` 参数 `on_mic_level` 更名 `on_level`,类型 `Option<std::sync::Arc<dyn Fn(crate::audio::Source, f32) + Send + Sync>>`。`run_segment_worker` 签名**不变**(仍收 `Option<Box<dyn Fn(f32) + Send>>`)。

- [ ] **Step 1: 写失败的序列化契约测试**(ipc.rs 底部 tests 模块)

```rust
#[cfg(test)]
mod tests {
    use super::*;

    /// 前端按 source 分路(recording store levels.mic/levels.system),
    /// 字段名与取值是跨语言契约。
    #[test]
    fn level_event_carries_source() {
        let json = serde_json::to_string(&LevelEvent { source: "system".into(), rms: 0.5 }).unwrap();
        assert!(json.contains("\"source\":\"system\""), "{json}");
        assert!(json.contains("\"rms\":0.5"), "{json}");
    }
}
```

- [ ] **Step 2: 跑测试确认编译失败**

Run: `cd src-tauri && cargo test level_event_carries_source`
Expected: FAIL(LevelEvent 无 source 字段)

- [ ] **Step 3: 实现**

ipc.rs(改 doc 注释与结构):

```rust
/// 采集电平(闸前 RMS,0..1 量级),事件名 "level",每源约 10Hz。
#[derive(Debug, Clone, Serialize)]
pub struct LevelEvent {
    pub source: String, // "mic" | "system"
    pub rms: f32,
}
```

session.rs:1959 参数改名换型:

```rust
    on_level: Option<std::sync::Arc<dyn Fn(Source, f32) + Send + Sync>>,
```

删除函数体内 `let mut mic_level = on_mic_level;`;1990 行改为(worker 收到的仍是 `Box<dyn Fn(f32)>`,source 烘焙进闭包):

```rust
        let level_cb: Option<Box<dyn Fn(f32) + Send>> = on_level.as_ref().map(|cb| {
            let cb = cb.clone();
            Box::new(move |r: f32| cb(source, r)) as Box<dyn Fn(f32) + Send>
        });
```

lib.rs:1690 emit 闭包:

```rust
            {
                let app_l = app.clone();
                Some(std::sync::Arc::new(move |source: crate::audio::Source, rms: f32| {
                    let _ = app_l.emit("level", ipc::LevelEvent { source: source.as_str().into(), rms });
                }) as std::sync::Arc<dyn Fn(crate::audio::Source, f32) + Send + Sync>)
            },
```

- [ ] **Step 4: cargo check 清理所有调用点**

Run: `cd src-tauri && cargo check 2>&1 | head -30`
session.rs 内部测试若有传 `None`/`Some(Box::new(...))` 的调用点,`None` 不用动,`Some(Box::new(...))` 改 `Some(std::sync::Arc::new(...))` 并按新签名补 source 参数。

- [ ] **Step 5: 跑测试确认通过**

Run: `cd src-tauri && cargo test level`
Expected: `level_event_carries_source` PASS,segment_worker 既有 level 测试(`segment_worker.rs:317` 一带)PASS。

- [ ] **Step 6: Commit**

```bash
git add src-tauri/src/ipc.rs src-tauri/src/session.rs src-tauri/src/lib.rs
git commit -m "feat(level): LevelEvent 带 source,系统声电平与 mic 同管道发射"
```

---

### Task 2: 前端双通道电平(store 分路 + 双波形条)

**Files:**
- Modify: `src/lib/events.ts:104-108`(LevelEvent 类型)
- Modify: `src/lib/recording.svelte.ts`(`level` 单值 → `levels` 双通道;行 46/73/157/172/298)
- Modify: `src/routes/record/+page.svelte:255-275`(levelPct/liveBars 泛化)、`378-385`(波形区)
- Modify: `src/lib/i18n/dict/record.ts`(新 key `record.systemLevel`,zh/en)

**Interfaces:**
- Consumes: Task 1 的 `LevelEvent { source, rms }`。
- Produces: `recording.levels: { mic: number; system: number }`(替换 `recording.level`,全库唯一消费方是 record 页)。

- [ ] **Step 1: store 分路**

recording.svelte.ts:行 46 `let level = $state(0);` 改:

```ts
let levels = $state({ mic: 0, system: 0 });
```

行 73 getter 改 `get levels() { return levels; },`(删 `get level()`)。行 172 的 onLevel 处理改:

```ts
      levels = { ...levels, [e.source]: e.rms };
```

行 157/298 的清零改 `levels = { mic: 0, system: 0 };`。

events.ts:

```ts
export type LevelEvent = { source: Source; rms: number };
```

- [ ] **Step 2: record 页双波形**

`levelPct`(255 行)泛化为函数,`liveBars` 变双数组(mic 逻辑照旧,system 平行一份):

```ts
  const pctOf = (rms: number) => {
    if (!recording.isLive || rms <= 0) return 0;
    const db = 20 * Math.log10(rms);
    // 以下映射照抄原 levelPct 函数体(db 区间 → 0..100),两路共用
    ...原实现...
  };
```

`liveBars` 推进 effect(263-275 行)对 `recording.levels.mic`/`recording.levels.system` 各推一份(`liveBarsMic`/`liveBarsSys`,常量 LIVE_BARS 共用)。波形区(378-385)改双行,行首加来源徽章(复用现有 `.badge mic/system` 样式类):

```svelte
        {#if recording.isLive}
          <div class="wave-stack" aria-hidden="true">
            <div class="wave-live" class:frozen={recording.paused} title={t("record.micLevel")}>
              <span class="wave-tag mic">{t("record.badge.me")}</span>
              {#each liveBarsMicView as h, i (i)}<span class="bar" style="height: {Math.max(6, h)}%"></span>{/each}
            </div>
            <div class="wave-live" class:frozen={recording.paused} title={t("record.systemLevel")}>
              <span class="wave-tag system">{t("record.badge.them")}</span>
              {#each liveBarsSysView as h, i (i)}<span class="bar" style="height: {Math.max(6, h)}%"></span>{/each}
            </div>
          </div>
        {/if}
```

`liveBarsMicView`/`liveBarsSysView` 沿用原 `liveBarsView` 的派生方式(逐一复制)。样式:`.wave-stack` 纵排 2 行、行高减半(原单行高度的一半,总高不变);`.wave-tag` 12px 次级墨小标。

- [ ] **Step 3: i18n**

record.ts zh 侧(`record.micLevel` 旁)加 `"record.systemLevel": "对方声音电平"`,en 侧加 `"record.systemLevel": "Their audio level"`。

- [ ] **Step 4: 校验**

Run: `npm run check && npm test`
Expected: 0 错;vitest 全绿(i18n parity 哨兵覆盖新 key)。

- [ ] **Step 5: Commit**

```bash
git add src/lib/events.ts src/lib/recording.svelte.ts src/routes/record/+page.svelte src/lib/i18n/dict/record.ts
git commit -m "feat(record): mic/系统声双通道电平波形——系统声是否在收音一眼可见"
```

---

### Task 3: 控制条重设计(状态醒目化 + 图标钮 + 停止二段确认)

**Files:**
- Modify: `src/routes/record/+page.svelte:352-396`(controls 区)+ 对应 style 段
- Modify: `src/lib/i18n/dict/record.ts`(停止确认文案,zh/en)

**Interfaces:**
- Consumes: `recording.paused` / `recording.stopping` / `recording.pending` / `recording.stop()`。
- 无对外接口;纯页面重构。

- [ ] **Step 1: 暂停整条变调 + 呼吸红点**

`.controls` 容器加 `class:paused={recording.paused}`:

```svelte
      <div class="controls" class:paused={recording.paused}>
```

样式:

```css
  .controls.paused {
    background: var(--warning-tint);
    border: 1px solid var(--warning-line);
    border-radius: var(--radius-lg);
    padding: 0.5rem 0.75rem;
  }
  /* 录制中红点呼吸;暂停/减弱动态下静止 */
  .status-dot.live {
    animation: breathe 1.6s ease-in-out infinite;
  }
  @keyframes breathe { 0%, 100% { opacity: 1; } 50% { opacity: 0.35; } }
  @media (prefers-reduced-motion: reduce) {
    .status-dot.live { animation: none; }
  }
```

`live-meta` 的状态标签在暂停时升格(非小灰点):`status-inline` 加 `class:pausedTag={recording.paused}`,样式 `.status-inline.pausedTag { color: var(--warning-ink); font-weight: 600; }`。

- [ ] **Step 2: 暂停/恢复图标 + 停止二段确认**

暂停/恢复钮加 CSS 图标(`.sym` 家族已有 dot/square,补 pause 双竖杠与 play 三角):

```svelte
            {#if recording.paused}
              <button class="ctl" disabled={recording.pending} onclick={() => recording.unpause()}>
                <span class="sym play"></span>{t("record.btn.resume")}
              </button>
            {:else}
              <button class="ctl" disabled={recording.pending} onclick={() => recording.pause()}>
                <span class="sym pause"></span>{t("record.btn.pause")}
              </button>
            {/if}
            {#if confirmStop}
              <span class="stop-confirm">
                {t("record.btn.stopConfirmMsg")}
                <button class="ctl danger" onclick={() => { confirmStop = false; recording.stop().catch((err) => console.error("停止录制失败", err)); }}>{t("record.btn.stopConfirmYes")}</button>
                <button class="ctl" onclick={() => (confirmStop = false)}>{t("record.btn.stopConfirmNo")}</button>
              </span>
            {:else}
              <button class="ctl danger" disabled={recording.pending} onclick={() => (confirmStop = true)}>
                <span class="sym square"></span>{t("record.btn.stop")}
              </button>
            {/if}
```

script 区加 `let confirmStop = $state(false);`,并在 `recording.isLive` 翻 false 时复位(挂进现有 status 变化 effect 或新 `$effect(() => { if (!recording.isLive) confirmStop = false; });`)。样式:

```css
  .sym.pause { /* 双竖杠 */
    width: 8px; height: 10px;
    border-left: 3px solid currentColor; border-right: 3px solid currentColor;
  }
  .sym.play { /* 实心三角 */
    width: 0; height: 0;
    border-left: 9px solid currentColor;
    border-top: 5px solid transparent; border-bottom: 5px solid transparent;
  }
  /* #84 警示胶囊同模式:warning-tint 行内胶囊,120ms 淡入,不跳版 */
  .stop-confirm {
    display: inline-flex; align-items: center; gap: 0.5rem;
    background: var(--warning-tint); border: 1px solid var(--warning-line);
    color: var(--warning-ink); border-radius: var(--radius-full);
    padding: 0.15rem 0.6rem; animation: fadein 120ms ease-out;
  }
  @keyframes fadein { from { opacity: 0; } to { opacity: 1; } }
  @media (prefers-reduced-motion: reduce) { .stop-confirm { animation: none; } }
  .ctl-group { display: flex; gap: 0.75rem; } /* 暂停与停止拉开间距(原 gap 若已 ≥0.75rem 则不动) */
```

- [ ] **Step 3: i18n**

record.ts zh:`"record.btn.stopConfirmMsg": "确认停止?"`、`"record.btn.stopConfirmYes": "停止"`、`"record.btn.stopConfirmNo": "继续录"`;en:`"Stop recording?"`、`"Stop"`、`"Keep going"`。

- [ ] **Step 4: 校验**

Run: `npm run check && npm test`
Expected: 全绿。

- [ ] **Step 5: Commit**

```bash
git add src/routes/record/+page.svelte src/lib/i18n/dict/record.ts
git commit -m "feat(record): 控制条重设计——暂停整条变调/红点呼吸/图标钮/停止二段确认"
```

---

### Task 4: FinalEvent 透传 seq(编辑锚点)

**Files:**
- Modify: `src-tauri/src/store/writer.rs:232` 附近(新增 next_seq getter)
- Modify: `src-tauri/src/lifecycle/actor.rs:199-222`(Final 分支)
- Modify: `src-tauri/src/ipc.rs:31-43`(FinalEvent)
- Modify: `src/lib/events.ts:7-13`、`src/lib/recording.svelte.ts:19,105,248,317`
- Test: writer.rs tests 模块

**Interfaces:**
- Produces: `FinalEvent.seq: u64`;前端 `Line.seq: number`(冷回灌路径从 `NoteSegment.seq` 取,恒有值)。
- `NoteWriter::next_seq() -> u64`:下一段将被分配的 seq(actor 在 append 前读取即本段 seq;actor 单线程,无竞态)。

- [ ] **Step 1: 失败测试**(writer.rs tests 模块,沿用模块内既有 `now()` 助手)

```rust
    /// actor 在 append_final 前读 next_seq 作为本段 seq 并随 FinalEvent 透传;
    /// 该值必须等于落盘记录里的 seq。
    #[test]
    fn next_seq_previews_the_seq_append_will_assign() {
        let tmp = tempfile::tempdir().unwrap();
        let mut w = NoteWriter::create(tmp.path(), now()).unwrap();
        assert_eq!(w.next_seq(), 0);
        w.append_final("mic", "第一句", 0, 900, None, None).unwrap();
        assert_eq!(w.next_seq(), 1);
        let content = std::fs::read_to_string(w.dir().join("segments.jsonl")).unwrap();
        assert!(content.contains("\"seq\":0"), "{content}");
    }
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cd src-tauri && cargo test next_seq_previews`
Expected: FAIL(next_seq 方法不存在)

- [ ] **Step 3: 实现后端**

writer.rs(note_id() 旁):

```rust
    /// 下一段将被分配的 seq。actor 在 append_final 前读取即得本段 seq
    /// (actor 单线程串行,读取与 append 之间无并发写入)。
    pub fn next_seq(&self) -> u64 {
        self.next_seq
    }
```

ipc.rs FinalEvent 加字段(source 上方):

```rust
    /// 磁盘段 seq(跨续录单调唯一),录制中段编辑的寻址锚点。
    pub seq: u64,
```

actor.rs Final 分支:

```rust
        PipelineOp::Final { source, text, start_ms, end_ms, speaker, rms } => {
            let seq = owned.writer.next_seq();
            // ……append_final 调用与 degraded 处理原样不动……
            let _ = app.emit(
                "final",
                crate::ipc::FinalEvent { seq, source, text, start_ms, end_ms, speaker },
            );
        }
```

- [ ] **Step 4: 前端接线**

events.ts FinalEvent 加 `seq: number;`。recording.svelte.ts:

```ts
export type Line = { seq: number; source: Source; text: string; speaker: string | null; start_ms: number };
```

行 105 push 加 `seq: e.seq`;行 248 与 317 的回灌 map 加 `seq: s.seq`(NoteSegment.seq 已存在,`src/lib/notes.ts:42`)。

- [ ] **Step 5: 全量校验**

Run: `cd src-tauri && cargo test && cd .. && npm run check && npm test`
Expected: 全绿(FinalEvent 相关既有断言若有,补 seq 字段)。

- [ ] **Step 6: Commit**

```bash
git add src-tauri/src/store/writer.rs src-tauri/src/lifecycle/actor.rs src-tauri/src/ipc.rs src/lib/events.ts src/lib/recording.svelte.ts
git commit -m "feat(transcript): FinalEvent 透传磁盘段 seq,实时行获得编辑锚点"
```

---

### Task 5: NoteWriter 录制期段编辑方法

**Files:**
- Modify: `src-tauri/src/store/writer.rs`(merge_speaker 后新增)
- Test: writer.rs tests 模块

**Interfaces:**
- Produces: `NoteWriter::edit_segment_text(seq: u64, expected_text: &str, new_text: &str) -> anyhow::Result<()>`、`NoteWriter::set_segment_speaker_live(seq: u64, expected_text: &str, speaker_id: &str) -> anyhow::Result<()>`(Task 6 的 actor 执行体消费)。
- 语义:flush 待写队列 → 按 seq 定位 + expected_text 乐观校验 → 改行 → tmp+rename 原子替换 → 丢弃旧句柄(`self.file = None`,重写替换 inode,照 merge_speaker:456-484 先例)。speaker 仅限本场已有(`self.speakers` 表内),**不支持 "new" 分配**(避免与 diar 注册表的 S-id 分配冲突);不更新 SpeakerMeta.sources(展示层不消费单段来源翻转,finalize 不受影响)。

- [ ] **Step 1: 失败测试**

```rust
    #[test]
    fn live_edit_text_and_speaker_rewrite_segments() {
        let tmp = tempfile::tempdir().unwrap();
        let mut w = NoteWriter::create(tmp.path(), now()).unwrap();
        w.append_final("mic", "原文一", 0, 900, Some("S1"), None).unwrap();
        w.append_final("system", "原文二", 1000, 1900, None, None).unwrap();
        w.sync_speakers(&[("S1".into(), vec!["mic".into()]), ("S2".into(), vec!["system".into()])]).unwrap();

        // 文本编辑:命中 + 乐观校验通过
        w.edit_segment_text(0, "原文一", "改后一").unwrap();
        // 说话人改派:目标必须在本场表内
        w.set_segment_speaker_live(1, "原文二", "S2").unwrap();
        let content = std::fs::read_to_string(w.dir().join("segments.jsonl")).unwrap();
        assert!(content.contains("改后一") && !content.contains("原文一"), "{content}");
        assert!(content.contains("\"speaker\":\"S2\""), "{content}");

        // 三类拒绝:expected 失配 / seq 不存在 / 说话人不在表内
        assert!(w.edit_segment_text(0, "原文一", "x").is_err());
        assert!(w.edit_segment_text(99, "改后一", "x").is_err());
        assert!(w.set_segment_speaker_live(0, "改后一", "S99").is_err());

        // 重写后追加不丢(句柄按需重开)
        w.append_final("mic", "第三句", 2000, 2900, None, None).unwrap();
        let content = std::fs::read_to_string(w.dir().join("segments.jsonl")).unwrap();
        assert!(content.contains("第三句"), "{content}");
    }
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cd src-tauri && cargo test live_edit_text_and_speaker`
Expected: FAIL(方法不存在)

- [ ] **Step 3: 实现**(merge_speaker 之后;错误文案走 `crate::tr!` 与既有 writer 错误风格一致,若 writer 内未用 tr! 则保持 anyhow 中文+英文同段落风格照 notes.rs `find_seg`)

```rust
    /// 录制中段编辑共用骨架:flush 待写队列 → 按 seq 定位并做 expected_text 乐观
    /// 校验 → edit 改行 → tmp+rename 原子替换 → 丢弃旧句柄(重写换了 inode,照
    /// merge_speaker 先例)。读失败中止,绝不以空内容覆写(同 merge_speaker 注释)。
    fn rewrite_segment(
        &mut self,
        seq: u64,
        expected_text: &str,
        edit: impl Fn(&mut SegmentRecord),
    ) -> anyhow::Result<()> {
        self.flush_pending()?;
        let path = self.dir.join("segments.jsonl");
        let content = std::fs::read_to_string(&path)
            .map_err(|e| anyhow::anyhow!("读 segments.jsonl 失败(编辑中止,避免清空): {e}"))?;
        let mut found = false;
        let mut out = String::new();
        for line in content.lines() {
            match serde_json::from_str::<SegmentRecord>(line) {
                Ok(mut rec) => {
                    if rec.seq == seq {
                        anyhow::ensure!(rec.text == expected_text, "段内容已变化");
                        edit(&mut rec);
                        found = true;
                    }
                    out.push_str(&serde_json::to_string(&rec)?);
                }
                Err(_) => out.push_str(line),
            }
            out.push('\n');
        }
        anyhow::ensure!(found, "段落不存在(seq={seq})");
        let tmp = self.dir.join("segments.jsonl.tmp");
        std::fs::write(&tmp, out)?;
        std::fs::rename(&tmp, &path)?;
        self.file = None;
        Ok(())
    }

    /// 录制中编辑段文本(actor 串行执行,与定稿追加互斥)。
    pub fn edit_segment_text(&mut self, seq: u64, expected_text: &str, new_text: &str) -> anyhow::Result<()> {
        anyhow::ensure!(!new_text.trim().is_empty(), "文本不能为空");
        self.rewrite_segment(seq, expected_text, |rec| rec.text = new_text.to_string())
    }

    /// 录制中改派段说话人:目标限本场表内已有 id——"new" 分配与 diar 注册表的
    /// S-id 空间会撞车,录制中一律不开放。
    pub fn set_segment_speaker_live(&mut self, seq: u64, expected_text: &str, speaker_id: &str) -> anyhow::Result<()> {
        anyhow::ensure!(self.speakers.contains_key(speaker_id), "录制中只能改派为本场已有说话人");
        self.rewrite_segment(seq, expected_text, |rec| rec.speaker = Some(speaker_id.to_string()))
    }
```

- [ ] **Step 4: 跑测试确认通过**

Run: `cd src-tauri && cargo test live_edit_text_and_speaker`
Expected: PASS;`cargo test writer` 全绿。

- [ ] **Step 5: Commit**

```bash
git add src-tauri/src/store/writer.rs
git commit -m "feat(writer): 录制期段编辑——edit_segment_text/set_segment_speaker_live(重写+句柄重开)"
```

---

### Task 6: actor 消息路由 + segment_edited 事件 + 命令壳 active 分流

**Files:**
- Modify: `src-tauri/src/lifecycle/machine.rs`(Msg/Effect 各 2 新变体 + handle 迁移臂,照 165-166/215-217 的 SetTitle/RenameActiveSpeaker 先例)
- Modify: `src-tauri/src/lifecycle/actor.rs`(效果执行体,照 DoRenameActiveSpeaker 执行体先例:note_id 与槽内 writer 对账,不匹配报错)
- Modify: `src-tauri/src/ipc.rs`(SegmentEditedEvent)
- Modify: `src-tauri/src/lib.rs:4886-4899`(edit_segment)、`src-tauri/src/lib.rs` set_segment_speaker 壳(4910 附近)
- Test: machine.rs 迁移矩阵测试(照 SetTitle 既有条目扩展)

**Interfaces:**
- Consumes: Task 5 的两个 writer 方法。
- Produces:
  - `Msg::EditActiveSegment { note_id: String, seq: u64, expected_text: String, new_text: String }`
  - `Msg::SetActiveSegmentSpeaker { note_id: String, seq: u64, expected_text: String, speaker_id: String }`
  - 事件 "segment_edited":`SegmentEditedEvent { note_id: String, seq: u64, text: Option<String>, speaker: Option<String> }`(编辑后终值,未动字段 None;Task 7 前端消费)。
  - 命令 `edit_segment`/`set_segment_speaker` 对活动笔记自动走 live 路由(前端调用方**无感**,继续用 `src/lib/notes.ts` 既有 `editSegment`/`setSegmentSpeaker`)。

- [ ] **Step 1: machine.rs 新变体与迁移臂**

Msg(SetTitle/RenameActiveSpeaker 之后):

```rust
    /// 录制中段编辑(P/live):与 SetTitle/RenameActiveSpeaker 同类——writer 语义
    /// 消息,与录制主时间轴正交,不改会话态。
    EditActiveSegment { note_id: String, seq: u64, expected_text: String, new_text: String },
    SetActiveSegmentSpeaker { note_id: String, seq: u64, expected_text: String, speaker_id: String },
```

Effect(DoRenameActiveSpeaker 之后):

```rust
    DoEditActiveSegment { note_id: String, seq: u64, expected_text: String, new_text: String },
    DoSetActiveSegmentSpeaker { note_id: String, seq: u64, expected_text: String, speaker_id: String },
```

handle 迁移臂(357-364 的 SetTitle/RenameActiveSpeaker 臂之后,字段逐一 clone,状态原样返回):

```rust
        Msg::EditActiveSegment { note_id, seq, expected_text, new_text } => (
            state,
            vec![DoEditActiveSegment {
                note_id: note_id.clone(), seq: *seq,
                expected_text: expected_text.clone(), new_text: new_text.clone(),
            }],
        ),
        Msg::SetActiveSegmentSpeaker { note_id, seq, expected_text, speaker_id } => (
            state,
            vec![DoSetActiveSegmentSpeaker {
                note_id: note_id.clone(), seq: *seq,
                expected_text: expected_text.clone(), speaker_id: speaker_id.clone(),
            }],
        ),
```

(handle 若按值匹配则去掉 `*`/`clone` 直接移动,以既有 SetTitle 臂的写法为准。)迁移矩阵注释(598-599 行「5 类新消息 × 4 状态」)同步为 7 类。

- [ ] **Step 2: 跑 machine 测试,把矩阵测试修到绿**

Run: `cd src-tauri && cargo test machine`
矩阵测试(every_report_in_every_state_reconciles 等)若枚举 Msg 变体,按 SetTitle 既有条目样式为两个新变体补「任意状态 × 消息 → 状态不变 + 对应 Do 效果」的断言。

- [ ] **Step 3: ipc.rs 事件结构**

```rust
/// 录制中段编辑落盘成功,事件名 "segment_edited"。text/speaker 为编辑后终值
/// (未动的字段为 None)。前端按 seq 更新 finals——事件是唯一真值源,UI 不做乐观更新。
#[derive(Debug, Clone, Serialize)]
pub struct SegmentEditedEvent {
    pub note_id: String,
    pub seq: u64,
    pub text: Option<String>,
    pub speaker: Option<String>,
}
```

- [ ] **Step 4: actor.rs 执行体**(DoRenameActiveSpeaker 执行体旁,完全照其对账模式:槽空或 note_id 不匹配 → Err;执行成功后 emit)

```rust
            Effect::DoEditActiveSegment { note_id, seq, expected_text, new_text } => {
                // 对账 + writer 调用照 DoRenameActiveSpeaker 骨架
                ...owned.writer.edit_segment_text(seq, &expected_text, &new_text)...
                // 成功分支:
                let _ = app.emit("segment_edited", crate::ipc::SegmentEditedEvent {
                    note_id, seq, text: Some(new_text), speaker: None,
                });
            }
            Effect::DoSetActiveSegmentSpeaker { note_id, seq, expected_text, speaker_id } => {
                ...owned.writer.set_segment_speaker_live(seq, &expected_text, &speaker_id)...
                let _ = app.emit("segment_edited", crate::ipc::SegmentEditedEvent {
                    note_id, seq, text: None, speaker: Some(speaker_id),
                });
            }
```

错误经 sticky-error 回执返回(Request 路径既有机制,不新增通道)。

- [ ] **Step 5: lib.rs 命令壳分流**(照 rename_speaker:4820-4855 的 active 判定,statement-scoped 取 session 槽)

edit_segment(4886):

```rust
    let active = state.session.lock().unwrap().as_ref().map(|s| s.note_id == note_id).unwrap_or(false);
    if active {
        return app.state::<lifecycle::LifecycleHandle>().request(
            lifecycle::machine::Msg::EditActiveSegment { note_id, seq, expected_text, new_text },
        );
    }
    // 非活动:既有 EditNote 冷路径原样
```

set_segment_speaker 壳同构;live 分支先拒 "new"(返回类型 `Result<String>`,live 成功返回入参 id):

```rust
    if active {
        if speaker_id == "new" {
            return Err(tr!("录制中不能新建说话人,请先停止录制", "Cannot create a new speaker while recording"));
        }
        app.state::<lifecycle::LifecycleHandle>().request(
            lifecycle::machine::Msg::SetActiveSegmentSpeaker {
                note_id, seq, expected_text, speaker_id: speaker_id.clone(),
            },
        )?;
        return Ok(speaker_id);
    }
```

- [ ] **Step 6: 全量后端测试**

Run: `cd src-tauri && cargo test`
Expected: 全绿。

- [ ] **Step 7: Commit**

```bash
git add src-tauri/src/lifecycle/machine.rs src-tauri/src/lifecycle/actor.rs src-tauri/src/ipc.rs src-tauri/src/lib.rs
git commit -m "feat(live-edit): 段编辑经 actor 路由到活动 writer,命令壳按活动笔记自动分流"
```

---

### Task 7: 前端当场纠正 UI(hover 操作钮 + 行内编辑 + 说话人菜单)

**Files:**
- Modify: `src/lib/events.ts`(SegmentEditedEvent + onSegmentEdited)
- Modify: `src/lib/recording.svelte.ts`(订阅 segment_edited,按 seq 更新 finals;订阅点与既有 onLevel/onFinal 同处)
- Modify: `src/routes/record/+page.svelte:489-507`(transcript 行)
- Modify: `src/lib/i18n/dict/record.ts`(zh/en)

**Interfaces:**
- Consumes: Task 6 的 "segment_edited" 事件;`src/lib/notes.ts` 既有 `editSegment(noteId, seq, expectedText, newText)`、`setSegmentSpeaker(noteId, seq, expectedText, speakerId)`、`renameSpeaker(noteId, speakerId, name)`(三者对活动笔记已被后端自动 live 路由)。
- Produces: 行级交互——hover 浮现「改说话人▾ / 编辑」;仅 `recording.isLive && !recording.stopping` 时展示;partial 行不可编辑。

- [ ] **Step 1: events.ts**

```ts
/** 录制中段编辑落盘成功(后端为唯一真值源,前端不做乐观更新)。未动字段为 null。 */
export type SegmentEditedEvent = { note_id: string; seq: number; text: string | null; speaker: string | null };
export function onSegmentEdited(cb: (e: SegmentEditedEvent) => void) {
  return listen<SegmentEditedEvent>("segment_edited", (ev) => cb(ev.payload));
}
```

- [ ] **Step 2: recording store 订阅**(与 onFinal 等相邻注册)

```ts
  onSegmentEdited((e) => {
    if (e.note_id !== noteId) return;
    finals = finals.map((l) =>
      l.seq === e.seq ? { ...l, text: e.text ?? l.text, speaker: e.speaker ?? l.speaker } : l,
    );
  });
```

- [ ] **Step 3: 行级 UI**

script 区状态:

```ts
  let editingSeq = $state<number | null>(null);
  let editingText = $state("");
  let speakerMenuSeq = $state<number | null>(null);
  let editError = $state("");

  async function commitEdit(line: Line) {
    try {
      await editSegment(recording.noteId!, line.seq, line.text, editingText.trim());
      editingSeq = null;
    } catch (e) {
      editError = t("record.edit.failed", { e });
    }
  }
  async function pickSpeaker(line: Line, speakerId: string) {
    speakerMenuSeq = null;
    try {
      await setSegmentSpeaker(recording.noteId!, line.seq, line.text, speakerId);
    } catch (e) {
      editError = t("record.edit.failed", { e });
    }
  }
```

transcript 行(489-496)改造——hover 操作钮(幽灵形态)、行内编辑、行首徽章点击开说话人菜单:

```svelte
      {#each recording.finals as line, i (line.seq)}
        <p class="final" class:current={...原判定...}>
          <button
            class="badge as-btn"
            style="background: {speakerColor(line.speaker, line.source, recording.speakers)}; color: {speakerInk(line.speaker, line.source, recording.speakers)}"
            disabled={!recording.isLive || recording.stopping}
            onclick={() => (speakerMenuSeq = speakerMenuSeq === line.seq ? null : line.seq)}
          >{speakerLabel(line.speaker, line.source, recording.speakers)}</button>
          {#if speakerMenuSeq === line.seq}
            <span class="spk-menu">
              {#each recording.speakers as s (s.id)}
                <button class="spk-item" onclick={() => pickSpeaker(line, s.id)}>{s.name || s.id}</button>
              {/each}
            </span>
          {/if}
          {#if editingSeq === line.seq}
            <!-- svelte-ignore a11y_autofocus -->
            <input class="edit-inline" autofocus bind:value={editingText}
              onkeydown={(e) => { if (e.key === "Enter") commitEdit(line); if (e.key === "Escape") editingSeq = null; }}
              onblur={() => (editingSeq = null)} />
          {:else}
            {line.text}
            {#if recording.isLive && !recording.stopping}
              <button class="row-act" title={t("record.edit.text")}
                onclick={() => { editingSeq = line.seq; editingText = line.text; }}>✎</button>
            {/if}
          {/if}
        </p>
      {/each}
```

(`✎` 为占位符号;实现时换 16px 线性 SVG 图标,与 #84 幽灵钮同风格——`class="row-act"` 默认 `opacity: 0`,`p.final:hover .row-act { opacity: 1; }`,focus-visible 同样显影。)`editError` 展示在 transcript 上方一条 `.banner`,可点击关闭。说话人改名入口:菜单底部加「命名/改名…」项,行内 input 提交 `renameSpeaker(recording.noteId!, s.id, name)`(live 路由已存在)。

- [ ] **Step 4: i18n**

record.ts zh:`"record.edit.text": "编辑这一句"`、`"record.edit.speaker": "改说话人"`、`"record.edit.rename": "命名/改名…"`、`"record.edit.failed": "编辑失败:{e}"`;en 对应 `"Edit this line"`、`"Change speaker"`、`"Name/rename…"`、`"Edit failed: {e}"`。

- [ ] **Step 5: 校验**

Run: `npm run check && npm test`
Expected: 全绿。`(line.seq)` keyed each 要求 finals 全部带 seq(Task 4 已保证)。

- [ ] **Step 6: Commit**

```bash
git add src/lib/events.ts src/lib/recording.svelte.ts src/routes/record/+page.svelte src/lib/i18n/dict/record.ts
git commit -m "feat(record): 当场纠正——行内编辑文本/改派说话人/命名,事件驱动更新"
```

---

### Task 8: 回看纯函数层 liveView.ts(搜索/过滤/时间轴)

**Files:**
- Create: `src/lib/liveView.ts`
- Test: `src/lib/liveView.test.ts`

**Interfaces:**
- Produces(Task 9/10 消费):
  - `searchHits(lines: { text: string }[], query: string): number[]` — 大小写不敏感子串命中下标;空/全空白 query → `[]`。
  - `matchesSpeakerFilter(line: { speaker: string | null }, selected: ReadonlySet<string>): boolean` — 空集 = 不过滤全显;非空 = 并集,speaker 为 null 的行隐藏。
  - `nearestIndexByMs(lines: { start_ms: number }[], targetMs: number): number` — start_ms 最接近目标的行下标;空数组 → -1。

- [ ] **Step 1: 失败测试**

```ts
import { describe, expect, it } from "vitest";
import { matchesSpeakerFilter, nearestIndexByMs, searchHits } from "./liveView";

describe("searchHits", () => {
  const lines = [{ text: "预算下周对齐" }, { text: "Budget 已批" }, { text: "无关" }];
  it("大小写不敏感子串命中", () => {
    expect(searchHits(lines, "budget")).toEqual([1]);
    expect(searchHits(lines, "预算")).toEqual([0]);
  });
  it("空/全空白 query 不命中(避免全场高亮)", () => {
    expect(searchHits(lines, "")).toEqual([]);
    expect(searchHits(lines, "  ")).toEqual([]);
  });
});

describe("matchesSpeakerFilter", () => {
  it("空集不过滤;非空并集;未标注行在过滤态隐藏", () => {
    expect(matchesSpeakerFilter({ speaker: null }, new Set())).toBe(true);
    expect(matchesSpeakerFilter({ speaker: "S1" }, new Set(["S1", "S2"]))).toBe(true);
    expect(matchesSpeakerFilter({ speaker: "S3" }, new Set(["S1"]))).toBe(false);
    expect(matchesSpeakerFilter({ speaker: null }, new Set(["S1"]))).toBe(false);
  });
});

describe("nearestIndexByMs", () => {
  const lines = [{ start_ms: 0 }, { start_ms: 60_000 }, { start_ms: 120_000 }];
  it("取 start_ms 最近的行;空数组 -1", () => {
    expect(nearestIndexByMs(lines, 55_000)).toBe(1);
    expect(nearestIndexByMs(lines, 200_000)).toBe(2);
    expect(nearestIndexByMs([], 0)).toBe(-1);
  });
});
```

- [ ] **Step 2: 跑测试确认失败**

Run: `npx vitest run src/lib/liveView.test.ts`
Expected: FAIL(模块不存在)

- [ ] **Step 3: 实现**

```ts
/// 实时转写回看的纯函数层:搜索命中/说话人过滤/时间轴定位。
/// UI 状态(query、选中集、follow 联动)留在页面,这里只做无状态判定,便于单测锁定口径。

export function searchHits(lines: { text: string }[], query: string): number[] {
  const q = query.trim().toLowerCase();
  if (!q) return [];
  const hits: number[] = [];
  lines.forEach((l, i) => {
    if (l.text.toLowerCase().includes(q)) hits.push(i);
  });
  return hits;
}

export function matchesSpeakerFilter(
  line: { speaker: string | null },
  selected: ReadonlySet<string>,
): boolean {
  return selected.size === 0 || (line.speaker !== null && selected.has(line.speaker));
}

export function nearestIndexByMs(lines: { start_ms: number }[], targetMs: number): number {
  let best = -1;
  let bestD = Infinity;
  lines.forEach((l, i) => {
    const d = Math.abs(l.start_ms - targetMs);
    if (d < bestD) { bestD = d; best = i; }
  });
  return best;
}
```

- [ ] **Step 4: 跑测试确认通过**

Run: `npx vitest run src/lib/liveView.test.ts`
Expected: 3 组全 PASS。

- [ ] **Step 5: Commit**

```bash
git add src/lib/liveView.ts src/lib/liveView.test.ts
git commit -m "feat(record): 回看纯函数层——searchHits/matchesSpeakerFilter/nearestIndexByMs"
```

---

### Task 9: 搜索框 + 说话人过滤 chips(UI 接线)

**Files:**
- Modify: `src/routes/record/+page.svelte`(controls 下方新增回看工具条;transcript each 行加可见性/高亮)
- Modify: `src/lib/i18n/dict/record.ts`(zh/en)

**Interfaces:**
- Consumes: Task 8 三函数、`recording.speakers`、既有 `follow`/`jumpToLatest()`(`+page.svelte:289,303`)。
- 行为口径:**搜索=高亮+跳转(不隐藏行);过滤=隐藏行**。搜索或过滤任一激活 → `follow = false`;两者都清空 → `jumpToLatest()` 恢复跟随。

- [ ] **Step 1: 状态与派生**

```ts
  import { matchesSpeakerFilter, nearestIndexByMs, searchHits } from "$lib/liveView";

  let searchQuery = $state("");
  let activeHit = $state(0); // hits 内下标
  let selectedSpeakers = $state<Set<string>>(new Set());
  const hits = $derived(searchHits(recording.finals, searchQuery));
  const reviewActive = $derived(searchQuery.trim() !== "" || selectedSpeakers.size > 0);

  $effect(() => {
    if (reviewActive) follow = false;
  });
  function clearReview() {
    searchQuery = "";
    selectedSpeakers = new Set();
    activeHit = 0;
    jumpToLatest();
  }
  function gotoHit(delta: number) {
    if (!hits.length) return;
    activeHit = (activeHit + delta + hits.length) % hits.length;
    document.getElementById(`seg-${recording.finals[hits[activeHit]].seq}`)
      ?.scrollIntoView({ block: "center", behavior: "smooth" });
  }
  function toggleSpeaker(id: string) {
    const next = new Set(selectedSpeakers);
    next.has(id) ? next.delete(id) : next.add(id);
    selectedSpeakers = next;
  }
```

- [ ] **Step 2: 工具条标记**(controls 之后、banner 区之前;仅 `recording.isLive || recording.finals.length > 0` 时显示)

```svelte
      <div class="review-bar">
        <input class="search" placeholder={t("record.search.placeholder")} bind:value={searchQuery}
          onkeydown={(e) => {
            if (e.key === "Enter") gotoHit(e.shiftKey ? -1 : 1);
            if (e.key === "Escape") clearReview();
          }} />
        {#if searchQuery.trim()}
          <span class="hit-count">{hits.length ? `${activeHit + 1}/${hits.length}` : t("record.search.none")}</span>
          <button class="ghosty" onclick={() => gotoHit(-1)} title={t("record.search.prev")}>↑</button>
          <button class="ghosty" onclick={() => gotoHit(1)} title={t("record.search.next")}>↓</button>
        {/if}
        {#each recording.speakers as s (s.id)}
          <button class="chip" class:on={selectedSpeakers.has(s.id)} onclick={() => toggleSpeaker(s.id)}>
            {s.name || s.id}
          </button>
        {/each}
        {#if reviewActive}
          <button class="ghosty" onclick={clearReview}>{t("record.search.clear")}</button>
        {/if}
      </div>
```

- [ ] **Step 3: 行可见性与高亮**(Task 7 的 each 行上叠加)

每行 `<p>` 加 `id="seg-{line.seq}"`、`class:hidden={!matchesSpeakerFilter(line, selectedSpeakers)}`、`class:hit={hits.includes(i)}`、`class:hit-active={hits[activeHit] === i}`。样式:

```css
  p.final.hidden { display: none; }
  p.final.hit { background: var(--accent-tint); border-radius: var(--radius-md); }
  p.final.hit-active { outline: 2px solid var(--accent); }
  .review-bar { display: flex; align-items: center; gap: 0.4rem; flex-wrap: wrap; margin-top: 0.4rem; }
  .review-bar .chip { border-radius: var(--radius-full); padding: 0.1em 0.6em; border: 1px solid var(--hairline); background: transparent; }
  .review-bar .chip.on { background: var(--accent-tint); border-color: var(--accent); }
```

`hits.includes(i)` 在 each 内是 O(n²);finals 行数(数百)可接受,若 svelte-check/审查提出,派生 `hitSet = $derived(new Set(hits))` 后 `hitSet.has(i)`。

- [ ] **Step 4: i18n**

zh:`"record.search.placeholder": "搜索转写内容"`、`"record.search.none": "无命中"`、`"record.search.prev": "上一个"`、`"record.search.next": "下一个"`、`"record.search.clear": "清除"`;en:`"Search transcript"`、`"No matches"`、`"Previous"`、`"Next"`、`"Clear"`。

- [ ] **Step 5: 校验 + Commit**

Run: `npm run check && npm test`

```bash
git add src/routes/record/+page.svelte src/lib/i18n/dict/record.ts
git commit -m "feat(record): 页内搜索高亮跳转 + 说话人过滤 chips,与跟随最新联动"
```

---

### Task 10: 迷你时间轴

**Files:**
- Modify: `src/routes/record/+page.svelte`(transcript 容器右缘)

**Interfaces:**
- Consumes: `nearestIndexByMs`(Task 8)、`recording.elapsedMs`、`recording.finals`、`follow`。
- 行为:细轨映射 0..elapsedMs;点击 → 定位最近行(scrollIntoView)并 `follow = false`;每 5 分钟一个刻度点。

- [ ] **Step 1: 实现**

transcript 外包一层相对定位容器,右缘细轨:

```svelte
    <div class="transcript-wrap">
      <div class="transcript" ...原属性不动...>...</div>
      {#if recording.finals.length > 1 && recording.elapsedMs > 60_000}
        <!-- svelte-ignore a11y_no_static_element_interactions, a11y_click_events_have_key_events -->
        <div class="timeline" onclick={(e) => {
          const rect = e.currentTarget.getBoundingClientRect();
          const ms = ((e.clientY - rect.top) / rect.height) * recording.elapsedMs;
          const idx = nearestIndexByMs(recording.finals, ms);
          if (idx >= 0) {
            follow = false;
            document.getElementById(`seg-${recording.finals[idx].seq}`)
              ?.scrollIntoView({ block: "center", behavior: "smooth" });
          }
        }}>
          {#each ticksView as topPct (topPct)}
            <span class="tick" style="top: {topPct}%"></span>
          {/each}
        </div>
      {/if}
    </div>
```

```ts
  /** 每 5 分钟一个刻度的纵向百分比。elapsedMs 是走表值,派生即可。 */
  const ticksView = $derived.by(() => {
    const total = recording.elapsedMs;
    if (total < 60_000) return [] as number[];
    const out: number[] = [];
    for (let ms = 300_000; ms < total; ms += 300_000) out.push((ms / total) * 100);
    return out;
  });
```

```css
  .transcript-wrap { position: relative; }
  .timeline {
    position: absolute; right: 0; top: 0.5rem; bottom: 0.5rem; width: 14px;
    cursor: pointer; border-left: 2px solid var(--hairline);
  }
  .timeline:hover { border-left-color: var(--hairline-strong); }
  .timeline .tick {
    position: absolute; left: -4px; width: 6px; height: 2px;
    background: var(--ink-faint); border-radius: 1px;
  }
```

- [ ] **Step 2: 校验 + Commit**

Run: `npm run check && npm test`

```bash
git add src/routes/record/+page.svelte
git commit -m "feat(record): 右缘迷你时间轴——按 start_ms 点击定位,5 分钟刻度"
```

---

### Task 11: 收尾——全量校验、真机冒烟清单、PR

**Files:**
- 无新改动(修复校验暴露的问题除外)

- [ ] **Step 1: 全量校验**

Run: `cd src-tauri && cargo test && cargo check && cd .. && npm run check && npm test`
Expected: 全绿、0 新告警。

- [ ] **Step 2: push + PR**(base master;正文含真机冒烟清单)

```bash
git push -u origin live-transcript-interaction
gh pr create --base master --title "feat(record): 实时转写页交互升级——控制条重设计/回看辅助/当场纠正" --body "<按 spec 概要 + 下方冒烟清单>"
```

冒烟清单(照 spec §4):

- [ ] 暂停:整条控制带变 warning 调、计时停走、列表徽标「已暂停」灰底
- [ ] 录制中红点呼吸;系统「减弱动态效果」后静止
- [ ] 双电平条:自己说话 mic 条跳、播放外放 system 条跳
- [ ] 停止二段确认:误触「停止」可取消;确认后正常停录
- [ ] 搜索:输入关键词高亮+计数,Enter/Shift+Enter 跳转,Esc 清空恢复跟随
- [ ] 说话人过滤:选中 chips 只显该说话人;清除恢复
- [ ] 时间轴:长录制(>5min)出现刻度,点击跳转对应时段
- [ ] 录制中改说话人:点行首徽章改派,行即时更新;停录后笔记页核对落盘
- [ ] 录制中编辑文本:改一句提交,行即时更新;停录后笔记页核对落盘
- [ ] 录制中命名说话人:菜单改名,chips/徽章同步
- [ ] partial(斜体未定稿)行无编辑入口
