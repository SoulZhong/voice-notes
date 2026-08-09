# 声音处理方案设置(三选一) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 设置页一个三档单选(方案 A 双轨 / A+B 对照 / 方案 B 成品轨)统一控制录制期混音与笔记页默认回放;旧 `mix_track` 布尔键语义等价迁移。

**Architecture:** 后端 `settings.rs` 用 `AudioScheme` 枚举替换 `mix_track: bool`(load 时迁移旧键,save 只写新键),录制管线消费点改读 `audio_scheme.mix_track()`;前端 `models.ts` 类型同步,新纯函数 `schemeToDefaultPlayback` 决定笔记页默认回放,设置页用 `Segmented` 组件承载三档。

**Tech Stack:** Rust/serde(枚举 `rename_all = "lowercase"`)、Svelte 5 runes、vitest、既有 `Segmented.svelte`(本分支基底 note-header-redesign 已含)。

**Spec:** `docs/superpowers/specs/2026-08-10-audio-scheme-setting-design.md`

## Global Constraints

- 语义等价迁移:旧 `mix_track=true` → `ab`(旧行为=混音+默认双轨),`false`/缺失 → `a`;`Settings::default()` 必须是 `a`(读设置失败 unwrap_or_default 兜底口径,既有 mix_track 测试同款纪律)。
- 写盘只写新键 `audio_scheme`,不再输出 `mix_track`。
- 笔记页会话级 A/B 切换语义不变;"mixed 不可用强制回落 dual" 的既有 effect 不动。
- i18n:替换 `settings.record.mixTrack.*` 为 `settings.record.audioScheme.*`(5 key,zh/en 齐平过 parity);无硬编码 CJK(哨兵)。
- 提交信息不加任何 Co-Authored-By trailer;不跑全量 cargo fmt。
- 工作分支:`audio-scheme-setting`(已存在,基于 note-header-redesign)。
- 每任务收尾按其验证命令全绿才提交。

---

### Task 1: 后端——AudioScheme 枚举、迁移与消费点

**Files:**
- Modify: `src-tauri/src/settings.rs`(结构体 ~L137-141、Default ~L240、load ~L257、测试区 ~L596)
- Modify: `src-tauri/src/lib.rs`(消费点 ~L984-996 与 ~L1445-1455)

**Interfaces:**
- Produces: `settings::AudioScheme`(serde 值 `"a"|"ab"|"b"`,`Default = A`,方法 `mix_track(self) -> bool`);`Settings.audio_scheme: AudioScheme`(前端 JSON 里键名 `audio_scheme`,值为小写字符串——Task 2 的 TS 类型按此写)。

- [ ] **Step 1: 写失败测试**

替换 settings.rs 测试区的 `mix_track_defaults_false_and_old_files_parse` 为四个新测试(tempfile 已是 dev-dependency,与仓库其它测试同款):

```rust
#[test]
fn audio_scheme_defaults_a_and_old_files_parse() {
    // 旧配置文件无该字段,必须仍可解析(仓库既有约定:新字段 serde(default))
    let s: Settings = serde_json::from_str("{}").expect("旧文件应可解析");
    assert_eq!(s.audio_scheme, AudioScheme::A, "默认方案 A:不改变现有用户行为");
    assert!(!s.audio_scheme.mix_track(), "A 档不混音");
    // lib.rs 读设置失败走 unwrap_or_default,真正生效的是手写 Default——单独断言
    assert_eq!(Settings::default().audio_scheme, AudioScheme::A);
}

#[test]
fn legacy_mix_track_true_migrates_to_ab() {
    // 旧键 true 的行为=混音+默认双轨,语义等价档位是 Ab
    let tmp = tempfile::tempdir().unwrap();
    std::fs::write(tmp.path().join("settings.json"), r#"{"mix_track":true}"#).unwrap();
    let s = load(tmp.path());
    assert_eq!(s.audio_scheme, AudioScheme::Ab);
    assert!(s.audio_scheme.mix_track());
}

#[test]
fn legacy_mix_track_false_stays_a() {
    let tmp = tempfile::tempdir().unwrap();
    std::fs::write(tmp.path().join("settings.json"), r#"{"mix_track":false}"#).unwrap();
    assert_eq!(load(tmp.path()).audio_scheme, AudioScheme::A);
}

#[test]
fn save_writes_scheme_and_drops_legacy_key() {
    // 写盘只写新键:round-trip 保留档位,mix_track 不再出现
    let tmp = tempfile::tempdir().unwrap();
    let s = Settings { audio_scheme: AudioScheme::B, ..Default::default() };
    save(tmp.path(), &s).unwrap();
    let raw = std::fs::read_to_string(tmp.path().join("settings.json")).unwrap();
    assert!(!raw.contains("mix_track"), "旧键不得再写盘: {raw}");
    assert_eq!(load(tmp.path()).audio_scheme, AudioScheme::B);
}
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cargo test --manifest-path src-tauri/Cargo.toml audio_scheme`
Expected: 编译错误(`AudioScheme` 不存在)——RED 证据。

- [ ] **Step 3: 实现枚举与迁移**

settings.rs 中,`mix_track` 字段位置(~L137)替换为:

```rust
/// 声音处理方案(spec 2026-08-10):录制期混音与笔记页默认回放的统一档位。
/// a=双轨(默认,不混音);ab=对照(混音,默认回放仍双轨);b=成品轨(混音,默认回放成品轨)。
/// 混音开启后每分钟多约 1.9MB 磁盘(转码 m4a 后大幅缩小),仅影响新录制。
#[serde(default)]
pub audio_scheme: AudioScheme,
/// 旧布尔键「录制期混出成品轨」(≤2026-08-09):仅为 load 迁移而保留读取,
/// save 不再写出(skip_serializing)。语义等价:true=混音+默认双轨=Ab。
#[serde(default, rename = "mix_track", skip_serializing)]
pub legacy_mix_track: Option<bool>,
```

结构体外(挨着 Settings 定义)新增:

```rust
/// 声音处理方案档位。serde 小写:"a"/"ab"/"b"。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AudioScheme {
    #[default]
    A,
    Ab,
    B,
}

impl AudioScheme {
    /// 录制期是否混出成品轨(ab/b 档)。
    pub fn mix_track(self) -> bool {
        self != AudioScheme::A
    }
}
```

`impl Default for Settings` 中 `mix_track: false,` 替换为:

```rust
audio_scheme: AudioScheme::A,
legacy_mix_track: None,
```

`load()`(~L257)改为解析后做旧键迁移:

```rust
/// 缺失/损坏 → 默认值（容忍，不报错）。旧 mix_track 布尔键在此迁移(见字段注释)。
pub fn load(app_data: &Path) -> Settings {
    let mut s: Settings = std::fs::read_to_string(app_data.join("settings.json"))
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default();
    // 新键在场(非默认 a)则旧键忽略;新写盘不再输出旧键,此分支只服务存量文件。
    if s.legacy_mix_track == Some(true) && s.audio_scheme == AudioScheme::A {
        s.audio_scheme = AudioScheme::Ab;
    }
    s
}
```

- [ ] **Step 4: 跑测试确认通过**

Run: `cargo test --manifest-path src-tauri/Cargo.toml audio_scheme && cargo test --manifest-path src-tauri/Cargo.toml legacy_mix && cargo test --manifest-path src-tauri/Cargo.toml settings`
Expected: 新 4 例 + settings 其余测试全绿。

- [ ] **Step 5: lib.rs 消费点(两处)**

~L984 快照区:注释里的 `mix_track` 提法改为 `audio_scheme(录制期混音)`,元组行:

```rust
let (record_system_only, keep_audio, language_filter, keep_output_volume, mix_track) = (
    cfg.record_system_only,
    cfg.keep_audio,
    cfg.language_filter,
    cfg.keep_output_volume,
    cfg.audio_scheme.mix_track(),
);
```

(局部变量名 `mix_track` 保留——~L1453 的 `build_sinks_with_first_offsets(..., mix_track)` 传参零改动。)

grep 确认无其它 `cfg.mix_track` / `.mix_track` 字段访问残留:`grep -n "\.mix_track" src-tauri/src/lib.rs` 应只剩 `audio_scheme.mix_track()` 一处与局部变量。

- [ ] **Step 6: 验证**

Run: `cargo check --manifest-path src-tauri/Cargo.toml && cargo test --manifest-path src-tauri/Cargo.toml settings`
Expected: 编译过、无新警告、settings 测试全绿。

- [ ] **Step 7: Commit**

```bash
git add src-tauri/src/settings.rs src-tauri/src/lib.rs
git commit -m "feat(settings): AudioScheme 三档枚举替换 mix_track——旧键语义等价迁移(true→ab)"
```

---

### Task 2: 前端 lib——类型同步与默认回放纯函数

**Files:**
- Modify: `src/lib/models.ts`(~L83 `mix_track: boolean;`)
- Create: `src/lib/audioScheme.ts`
- Create: `src/lib/audioScheme.test.ts`

**Interfaces:**
- Consumes: Task 1 的 serde 键 `audio_scheme: "a"|"ab"|"b"`。
- Produces: `Settings.audio_scheme: "a" | "ab" | "b"`(models.ts);`schemeToDefaultPlayback(scheme: string): "dual" | "mixed"` 自 `$lib/audioScheme` 导出——Task 3/4 按此引用。

- [ ] **Step 1: 写失败测试**

`src/lib/audioScheme.test.ts`:

```ts
import { describe, expect, it } from "vitest";
import { schemeToDefaultPlayback } from "./audioScheme";

describe("schemeToDefaultPlayback(声音处理方案→默认回放)", () => {
  it("b 档默认成品轨", () => {
    expect(schemeToDefaultPlayback("b")).toBe("mixed");
  });
  it("a/ab 档默认双轨", () => {
    expect(schemeToDefaultPlayback("a")).toBe("dual");
    expect(schemeToDefaultPlayback("ab")).toBe("dual");
  });
  it("未知值容错按双轨(设置文件被手改坏也不炸回放)", () => {
    expect(schemeToDefaultPlayback("")).toBe("dual");
    expect(schemeToDefaultPlayback("banana")).toBe("dual");
  });
});
```

Run: `npx vitest run src/lib/audioScheme.test.ts`
Expected: FAIL(模块不存在)。

- [ ] **Step 2: 实现**

`src/lib/audioScheme.ts`:

```ts
/** 声音处理方案(spec 2026-08-10)→ 笔记页默认回放:b 档默认成品轨,其余(含未知值)双轨。 */
export function schemeToDefaultPlayback(scheme: string): "dual" | "mixed" {
  return scheme === "b" ? "mixed" : "dual";
}
```

`src/lib/models.ts` ~L83 替换:

```ts
/** 声音处理方案:"a" 双轨(默认)/"ab" 对照(混音+默认双轨)/"b" 成品轨(混音+默认成品轨)。 */
audio_scheme: "a" | "ab" | "b";
```

- [ ] **Step 3: 验证**

Run: `npx vitest run src/lib/audioScheme.test.ts && npm run check`
Expected: 测试绿;check 会在设置页报 `mix_track` 类型错误——**属预期**(Task 3 修复),若报错仅限 settings/+page.svelte 的 mix_track 引用即算通过本步;为不留红提交,本任务与 Task 3 **合并提交**(见 Task 3 Step 5)。

---

### Task 3: 设置页——三档 Segmented + i18n

**Files:**
- Modify: `src/routes/settings/+page.svelte`(state ~L146、回填 ~L244、行标记 ~L852-864)
- Modify: `src/lib/i18n/dict/settings.ts`(zh ~L80、en ~L257)

**Interfaces:**
- Consumes: Task 2 的 `Settings.audio_scheme` 类型;`$lib/Segmented.svelte` + `SegmentedItem`(基底分支已有)。

- [ ] **Step 1: i18n 键替换(zh/en 两块,`settings.record.mixTrack.*` 两条删除,新增五条)**

zh:

```ts
"settings.record.audioScheme.label": "声音处理方案",
"settings.record.audioScheme.desc": "A:双轨对齐回放;A+B:录制期混出成品轨供对照(默认仍放双轨);B:默认直接播成品轨。混音仅影响新录制,每分钟约多占 1.9MB(转码后大幅缩小)",
"settings.record.audioScheme.a": "方案 A · 双轨",
"settings.record.audioScheme.ab": "A+B 对照",
"settings.record.audioScheme.b": "方案 B · 成品轨",
```

en:

```ts
"settings.record.audioScheme.label": "Audio processing scheme",
"settings.record.audioScheme.desc": "A: aligned dual-track playback; A+B: also mix a combined track while recording for comparison (playback still defaults to dual); B: play the mixed track by default. Mixing affects new recordings only; ~1.9MB more per minute (much smaller after transcoding)",
"settings.record.audioScheme.a": "Scheme A · Dual",
"settings.record.audioScheme.ab": "A+B compare",
"settings.record.audioScheme.b": "Scheme B · Mixed",
```

- [ ] **Step 2: script 区改造**

import 区加:

```ts
import Segmented from "$lib/Segmented.svelte";
import type { SegmentedItem } from "$lib/segmented";
```

`let mixTrack = $state(false);` 替换为:

```ts
let audioScheme = $state<"a" | "ab" | "b">("a");
```

回填处(~L244)`mixTrack = s.mix_track;` 替换为 `audioScheme = s.audio_scheme;`。

items derived(与其它 derived 同区,禁用态跟 `!settings` 同其它控件纪律):

```ts
// 声音处理方案三档(spec 2026-08-10)。settings 未回填前整组禁用,与其它开关同纪律。
const audioSchemeItems = $derived<SegmentedItem[]>([
  { id: "a", label: t("settings.record.audioScheme.a"), disabled: !settings },
  { id: "ab", label: t("settings.record.audioScheme.ab"), disabled: !settings },
  { id: "b", label: t("settings.record.audioScheme.b"), disabled: !settings },
]);
```

- [ ] **Step 3: 行标记替换(~L852-864 的 mixTrack `<label class="row">` 整块)**

```svelte
<div class="row">
  <div class="row-info">
    <span class="row-label">{t("settings.record.audioScheme.label")}</span>
    <span class="row-desc">{t("settings.record.audioScheme.desc")}</span>
  </div>
  <Segmented
    items={audioSchemeItems}
    value={audioScheme}
    onSelect={(id) => {
      audioScheme = id as "a" | "ab" | "b";
      saveSetting((s) => (s.audio_scheme = audioScheme));
    }}
  />
</div>
```

注意:原行是 `<label>`(为 checkbox 服务),换 `<div>` 后确认 `.row` 系样式按 class 命中不受元素名影响(该页 .row 是 class 选择器,应无差);若 `.row` 有 `label.row` 形态的选择器,同步放宽。

- [ ] **Step 4: 死引用扫尾**

`grep -rn "mixTrack\|mix_track" src/routes/settings/+page.svelte src/lib/models.ts` 应无输出;`grep -rn "settings.record.mixTrack" src` 应无输出。

- [ ] **Step 5: 验证 + 与 Task 2 合并提交**

Run: `npm run check && npm test`
Expected: check 0 错 0 警(Task 2 遗留的类型错在此步消失);vitest 全绿(含 i18n parity 哨兵)。

```bash
git add src/lib/models.ts src/lib/audioScheme.ts src/lib/audioScheme.test.ts src/lib/i18n/dict/settings.ts src/routes/settings/+page.svelte
git commit -m "feat(settings): 设置页声音处理方案三档 Segmented——类型同步+默认回放纯函数"
```

---

### Task 4: 笔记页默认回放 + 全量验证

**Files:**
- Modify: `src/routes/notes/[id]/+page.svelte`(playbackScheme 初始 ~L122、id 复位 ~L711、script 初始化区)

**Interfaces:**
- Consumes: `schemeToDefaultPlayback`(`$lib/audioScheme`)、`getSettings`(`$lib/models`,已存在)。

- [ ] **Step 1: 引入默认回放**

import 区加(`getSettings` 若未 import 则一并加,来自 `$lib/models`):

```ts
import { schemeToDefaultPlayback } from "$lib/audioScheme";
```

`let playbackScheme = $state<"dual" | "mixed">("dual");`(~L122)后新增:

```ts
/** 设置页三档决定的默认回放(spec 2026-08-10)。增值层:取失败按 a(双轨)不打扰。
    挂载取一次即定,id 切换复位用它;会话内手动切换语义不变。 */
let defaultPlayback = $state<"dual" | "mixed">("dual");
```

script 初始化区(与其它一次性回填调用同区,如 people/retranscribeStatus 的挂载取数处)新增:

```ts
getSettings()
  .then((s) => {
    defaultPlayback = schemeToDefaultPlayback(s.audio_scheme);
    playbackScheme = defaultPlayback;
  })
  .catch(() => {});
```

注:紧随其后的既有 effect("mixed 不可用即强制回落 dual")原样兜底 b 档打开无成品轨笔记的场景,勿动。

- [ ] **Step 2: id 复位改用默认值**

~L711 复位块中 `playbackScheme = "dual";` 替换为:

```ts
playbackScheme = defaultPlayback;
```

- [ ] **Step 3: 全量验证**

Run: `npm run check && npm test && cargo test --manifest-path src-tauri/Cargo.toml settings`
Expected: 全绿。

- [ ] **Step 4: Commit**

```bash
git add "src/routes/notes/[id]/+page.svelte"
git commit -m "feat(notes): 默认回放跟随声音处理方案——b 档进页即成品轨,不可用回落双轨"
```

---

### 收尾(不在任务内,提醒执行者)

- 真机冒烟(用户做,写进 PR 描述):三档各录一段——a 无 mixed 产物 / ab 有产物且默认双轨 / b 有产物且默认成品轨;b 档打开无成品轨旧笔记自动回落双轨;旧 settings.json(mix_track:true)升级后档位显示 A+B 对照。
- PR 基于 note-header-redesign 叠放;PR#84 合并后 rebase 到 master 再提。提交信息无 Co-Authored-By。
