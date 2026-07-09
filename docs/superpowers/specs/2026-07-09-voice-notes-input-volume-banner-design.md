# 输入音量过低横幅(A3)— 设计

日期:2026-07-09
状态:待实现
关联:backlog A3;「录音没声音」第二起定案(2026-07-06,root cause = 系统输入音量被会议软件拉低到 30);录制页 `src/routes/record/+page.svelte` 现有 `btEchoRisk` / `screenPerm` 横幅

## 背景与目标

普通麦克风模式(`keep_output_volume=true`,不走 VPIO)下,macOS 系统输入音量会被会议软件/系统 AGC 拉低且不自动恢复,应用完全受它摆布,录出近乎无声的波形。此前只能事后从 `segments.jsonl` 的 rms 诊断出来。本功能在**录制页**主动检测:输入音量过低时出一条横幅,一键把它调回可用电平。

**成功标准**:普通麦克风模式下开录前、录制中,若系统输入音量低于阈值,录制页出现横幅并可一键修复;修复后横幅消失;VPIO 模式 / 仅系统声模式 / 非 macOS 下永不误报。

**非目标(YAGNI)**:
- 不处理不支持软件调音量的设备的额外兜底(点了没调动就让横幅留着,真遇到再说)。
- 不做输入设备切换 / 声音设置深链(v1 只一键设音量)。
- 不改录制/采集链路,不碰 AGC。
- VPIO 模式不检测(其自带增益管理,不受系统输入音量摆布)。

## 决策记录(brainstorm 拍板)

1. **触发阈值 = 输入音量 < 50%**;**一键修复目标 = 75%**(来自 2026-07-06「拉到 30、手动 set 75 修好」的诊断结论;常量,可调)。
2. **显示时机 = 开录前 + 录制中**(录制中周期轮询,会议软件中途拉低也能发现)。
3. **仅普通麦克风模式**(`keep_output_volume=true`)且本场会采集麦克风(`!record_system_only`)。

## 架构

### 后端:两个 macOS-only Tauri 命令(`src-tauri/src/lib.rs`,紧邻现有 `screen_capture_permission`)

- `input_volume() -> Option<u8>`:执行 `osascript -e 'input volume of (get volume settings)'`,解析 stdout 为 0..100。非 macOS(`#[cfg(not(target_os="macos"))]`)或读取/解析失败 → `None`。前端把 `None` 视作「未知,不出横幅」。
- `set_input_volume(v: u8) -> bool`:执行 `osascript -e "set volume input volume <v>"`;成功返回 true。非 macOS → false。

命令均注册进 `invoke_handler`(与 `screen_capture_permission` / `request_screen_capture_permission` 并列)。

**纯函数(可测)**:`fn parse_input_volume(stdout: &str) -> Option<u8>` —— trim 后解析十进制,越界(>100)截断到 100,空/非数字 → None。osascript 调用壳把 stdout 交给它。这是本任务唯一的单测点(osascript 调用本身系统相关,靠真机冒烟)。

### 前端:录制页横幅(`src/routes/record/+page.svelte`)

镜像现有 `btEchoRisk`:

- 状态 `let lowInputVol = $state<{ vol: number } | null>(null)`(非空即出横幅,带当前音量用于文案)。
- `async function refreshInputVol()`:
  - `const [s, vol] = await Promise.all([getSettings(), invoke<number | null>("input_volume")])`
  - 条件:`s.keep_output_volume && !s.record_system_only && vol != null && vol < LOW_INPUT_THRESHOLD` → `lowInputVol = { vol }`,否则 `null`。
  - 整体 try/catch → 失败置 `null`(不误伤)。
- `async function fixInputVol()`:`await invoke("set_input_volume", { v: INPUT_TARGET })` → `await refreshInputVol()`。
- 触发时机:
  - `onMount` 里调一次 `refreshInputVol()`;并入现有 `onFocus`(窗口切回时刷新,与 `refreshScreenPerm`/`refreshBtRisk` 同处)。
  - **录制中轮询**:`onMount` 起一个 `setInterval(refreshInputVol, POLL_MS)`,`onMount` 的 cleanup 里 `clearInterval`。覆盖开录前与录制中两种状态(轮询与录制状态无关,一直跑;osascript 每次 ~100ms,间隔数秒,开销可忽略)。
- 常量(集中在该文件顶部脚本区):`LOW_INPUT_THRESHOLD = 50`、`INPUT_TARGET = 75`、`POLL_MS = 4000`。
- 横幅位置:与 `btEchoRisk` 横幅同区(`{#if models || models.recording_ready}` 横幅组内),但**不加 `!recording.isLive` 门控**(录制中也要显示)。文案示例:「麦克风输入音量偏低({vol}%),可能录得很轻」+ 按钮「调到 75%」(按钮点击 `fixInputVol`)。

## 边界与错误处理

1. 非 macOS / osascript 不可用 → `input_volume` 返回 `None` → 无横幅。
2. VPIO 模式(`keep_output_volume=false`)或仅系统声(`record_system_only=true`)→ 条件不满足,无横幅。
3. 设备不支持软件调音量:`set_input_volume` 后回读仍低 → 横幅继续显示(v1 不加兜底)。
4. 轮询在页面卸载时 `clearInterval`,不泄漏;`refreshInputVol` 全程 try/catch,任一失败静默置 null。
5. 录制中调音量:即时抬高,改善后续录音(已录部分不受影响,符合预期)。
6. 阈值边界:恰好等于 50 不提示(`< 50`);修复目标 75 高于阈值,修完必然不再触发。

## 测试与验证

- **cargo 单元测试** `parse_input_volume`:`"30\n" → Some(30)`、`"100" → Some(100)`、`"150" → Some(100)`(截断)、`"" → None`、`"abc" → None`、含空白 `" 42 \n" → Some(42)`。
- **真机冒烟**(需 macOS):
  - 手动 `osascript -e 'set volume input volume 30'` 造低音量 → 开录制页,普通麦克风模式下出现横幅显示「30%」;点「调到 75%」→ 音量抬到 75(`osascript -e 'input volume of (get volume settings)'` 核对)、横幅消失。
  - 切到 VPIO 模式(设置关「录制时保持外放音量」)→ 同样低音量下**不**出横幅。
  - 切「仅系统声音」→ 不出横幅。
  - 录制中把音量 set 回 30 → 数秒内横幅出现(轮询生效)。

## 影响面

- 改:`src-tauri/src/lib.rs`(两命令 + `parse_input_volume` + 注册)、`src/routes/record/+page.svelte`(状态/刷新/轮询/横幅)。
- 新增测试:`parse_input_volume` 的 `#[cfg(test)]` 用例(同文件或就近)。
- 前端:无新依赖;不使用 vitest(未在 master;gating 逻辑简单内联)。
- 后端:无新 crate 依赖(`std::process::Command` 调 osascript)。
