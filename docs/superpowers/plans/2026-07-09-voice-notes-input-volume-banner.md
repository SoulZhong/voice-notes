# 输入音量过低横幅(A3)Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 普通麦克风模式下,系统输入音量被拉低时录制页出一条横幅,一键把它调回 75%。

**Architecture:** 两个 macOS-only Tauri 命令经 `osascript` 读/写系统输入音量(读值交给纯函数 `parse_input_volume` 解析,带 cargo 单测);录制页镜像现有 `btEchoRisk` 横幅——mount + 窗口 focus + 每 4s 轮询检测,满足「普通麦克风模式 && 会录麦克风 && 音量<50」时显示,开录前与录制中都显示。

**Tech Stack:** Rust / Tauri command、`std::process::Command`(osascript)、Svelte 5 (runes)。

## Global Constraints

- 后端命令 macOS-only:非 macOS 分支 `input_volume → None`、`set_input_volume → false`。
- 无新依赖(后端 `std::process::Command`;前端不引 vitest,gating 内联)。
- 触发条件必须三者全满足:`keep_output_volume === true` && `record_system_only === false` && 输入音量 `< 50`。常量:`LOW_INPUT_THRESHOLD = 50`、`INPUT_TARGET = 75`、`POLL_MS = 4000`。
- 横幅**不加 `!recording.isLive` 门控**(开录前 + 录制中都显示)。
- 复用现有 `.banner` / `.link` 样式,不引新组件;文案口语化,无 Unicode 符号字符。
- `cargo test`(在 `src-tauri/`)全绿;`npm run check` 0 error / 0 warning。

---

### Task 1: 后端 —— input_volume / set_input_volume 命令 + parse 纯函数

**Files:**
- Modify: `src-tauri/src/lib.rs`(在 `request_screen_capture_permission`(~:2181)与 `extern "C"` 块(~:2188)之间加两命令 + parse 函数 + 测试;在 `generate_handler!`(~:2329,`request_screen_capture_permission,` 后)注册)

**Interfaces:**
- Produces(供 Task 2 前端消费):
  - Tauri 命令 `input_volume() -> Option<u8>`(前端 `invoke<number | null>("input_volume")`)。
  - Tauri 命令 `set_input_volume(v: u8) -> bool`(前端 `invoke("set_input_volume", { v: 75 })`)。
  - 纯函数 `fn parse_input_volume(stdout: &str) -> Option<u8>`(仅后端内部 + 测试)。

- [ ] **Step 1: 写失败测试**

在 `src-tauri/src/lib.rs` 末尾追加:
```rust
#[cfg(test)]
mod input_volume_parse_tests {
    use super::parse_input_volume;

    #[test]
    fn parses_trims_and_clamps() {
        assert_eq!(parse_input_volume("30\n"), Some(30));
        assert_eq!(parse_input_volume("100"), Some(100));
        assert_eq!(parse_input_volume("150"), Some(100)); // 越界截到 100
        assert_eq!(parse_input_volume(" 42 \n"), Some(42)); // 含空白
        assert_eq!(parse_input_volume(""), None);
        assert_eq!(parse_input_volume("abc"), None);
    }
}
```

- [ ] **Step 2: 运行测试确认失败**

Run:
```bash
cd src-tauri && cargo test input_volume_parse 2>&1 | tail -20
```
Expected: 编译失败 —— `cannot find function parse_input_volume`。

- [ ] **Step 3: 写 parse 函数 + 两命令**

在 `src-tauri/src/lib.rs` 的 `request_screen_capture_permission` 函数(结尾 `}`,约 :2186)之后、`#[cfg(target_os = "macos")] #[link(name = "CoreGraphics"…)] extern "C"` 块(约 :2188)之前,插入:
```rust
/// 解析 `osascript -e 'input volume of (get volume settings)'` 的 stdout(0..100)。
/// trim 后按十进制解析,越界截到 100,空/非数字 → None。
fn parse_input_volume(stdout: &str) -> Option<u8> {
    let v: u32 = stdout.trim().parse().ok()?;
    Some(v.min(100) as u8)
}

/// 读取 macOS 系统输入音量(0..100)。非 macOS / 读取失败 → None。录制页据此在普通
/// 麦克风模式下预警"输入音量被会议软件拉低,会录得很轻"。
#[tauri::command]
fn input_volume() -> Option<u8> {
    #[cfg(not(target_os = "macos"))]
    return None;
    #[cfg(target_os = "macos")]
    {
        let out = std::process::Command::new("osascript")
            .args(["-e", "input volume of (get volume settings)"])
            .output()
            .ok()?;
        if !out.status.success() {
            return None;
        }
        parse_input_volume(&String::from_utf8_lossy(&out.stdout))
    }
}

/// 设置 macOS 系统输入音量(0..100)。成功返回 true。非 macOS → false。
#[tauri::command]
fn set_input_volume(v: u8) -> bool {
    #[cfg(not(target_os = "macos"))]
    {
        let _ = v;
        return false;
    }
    #[cfg(target_os = "macos")]
    {
        let v = v.min(100);
        std::process::Command::new("osascript")
            .args(["-e", &format!("set volume input volume {v}")])
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    }
}
```

- [ ] **Step 4: 运行测试确认通过**

Run:
```bash
cd src-tauri && cargo test input_volume_parse 2>&1 | tail -20
```
Expected: PASS(`test input_volume_parse_tests::parses_trims_and_clamps ... ok`)。

- [ ] **Step 5: 注册命令**

在 `src-tauri/src/lib.rs` 的 `tauri::generate_handler![` 列表里,找到 `request_screen_capture_permission,` 那一行(约 :2330),在其后新增一行:
```rust
            input_volume,
            set_input_volume,
```

- [ ] **Step 6: 编译确认注册无误**

Run:
```bash
cd src-tauri && cargo build 2>&1 | tail -15
```
Expected: 编译成功(无 error;既有 warning 不新增)。

- [ ] **Step 7: 提交**

```bash
git add src-tauri/src/lib.rs
git commit -m "feat(record): input_volume/set_input_volume 命令 + parse 单测"
```

---

### Task 2: 录制页 —— 输入音量过低横幅 + 检测 + 轮询

**Files:**
- Modify: `src/routes/record/+page.svelte`(脚本区加状态/刷新/修复;`onMount` 加首检+focus+轮询;`.banner` 组加横幅)
- 依赖运行:`localhost:1420`(`npm run tauri dev`)——真机冒烟由控制器执行

**Interfaces:**
- Consumes(Task 1):`invoke<number | null>("input_volume")`、`invoke("set_input_volume", { v })`。
- 组件既有:`getSettings()`、`invoke`(已 import,`refreshBtRisk` 在用)、`recording`、`onMount` 现有 focus 清理。
- Produces:无(终端 UI)。

- [ ] **Step 1: 加状态与刷新/修复函数**

在 `src/routes/record/+page.svelte` 的 `refreshBtRisk` 函数(结尾 `}`,约 :58)之后插入:
```ts
  // 输入音量过低预警(普通麦克风模式):系统输入音量被会议软件拉低会录得很轻。
  // 开录前 + 录制中都检测,一键调回可用电平;VPIO 模式(自带 AGC)/仅系统声不检测。
  const LOW_INPUT_THRESHOLD = 50;
  const INPUT_TARGET = 75;
  const POLL_MS = 4000;
  let lowInputVol = $state<{ vol: number } | null>(null);
  async function refreshInputVol() {
    try {
      const [s, vol] = await Promise.all([
        getSettings(),
        invoke<number | null>("input_volume"),
      ]);
      lowInputVol =
        s.keep_output_volume && !s.record_system_only && vol != null && vol < LOW_INPUT_THRESHOLD
          ? { vol }
          : null;
    } catch {
      lowInputVol = null;
    }
  }
  async function fixInputVol() {
    try {
      await invoke("set_input_volume", { v: INPUT_TARGET });
    } catch {
      /* 设置失败:回读后横幅仍在,用户可见未生效 */
    }
    await refreshInputVol();
  }
```

- [ ] **Step 2: onMount 首检 + focus + 轮询**

把 `onMount(() => { … })`(约 :74–88)整体替换为:
```ts
  onMount(() => {
    refreshModels();
    refreshScreenPerm();
    refreshBtRisk();
    refreshInputVol();
    getSettings().then((s) => {
      showMcpHint = s.onboarded && !s.mcp_onboarded;
    }).catch(() => {});
    // 用户去系统设置勾选/换音频设备后切回来,焦点事件驱动横幅刷新,无需重启页面。
    const onFocus = () => {
      refreshScreenPerm();
      refreshBtRisk();
      refreshInputVol();
    };
    window.addEventListener("focus", onFocus);
    // 录制中也检测(会议软件中途拉低输入音量):轮询与录制状态无关,一直跑。
    const volTimer = setInterval(refreshInputVol, POLL_MS);
    return () => {
      window.removeEventListener("focus", onFocus);
      clearInterval(volTimer);
    };
  });
```

- [ ] **Step 3: 加横幅**

在 `.banner` 组里、`{#if btEchoRisk && !recording.isLive} … {/if}` 块(约 :234–238)之后插入(注意**不加** `!recording.isLive`,录制中也显示):
```svelte
    {#if lowInputVol}
      <div class="banner">
        麦克风输入音量偏低（{lowInputVol.vol}%），可能录得很轻。
        <button class="link" onclick={fixInputVol}>调到 {INPUT_TARGET}%</button>
      </div>
    {/if}
```

- [ ] **Step 4: 类型检查**

Run:
```bash
npm run check
```
Expected: 0 error, 0 warning。

- [ ] **Step 5: 提交**

```bash
git add src/routes/record/+page.svelte
git commit -m "feat(record): 输入音量过低横幅 + 开录前/录制中检测"
```

---

## 收尾(控制器执行)

- [ ] `cargo test`(src-tauri)全绿;`npm run check` 0/0。
- [ ] **真机冒烟(macOS)**:
  - `osascript -e 'set volume input volume 30'` 造低音量 → 普通麦克风模式录制页出现横幅显示「30%」;点「调到 75%」→ `osascript -e 'input volume of (get volume settings)'` 核对已 75、横幅消失。
  - 设置关「保持外放音量」(VPIO 模式)→ 同样低音量**不**出横幅。
  - 开「仅系统声音」→ 不出横幅。
  - 录制中把音量 set 回 30 → 数秒(≤4s)内横幅出现(轮询生效)。
- [ ] 推分支 `input-volume-banner` → 开 PR(用户真机冒烟后 squash 合入 master)。
