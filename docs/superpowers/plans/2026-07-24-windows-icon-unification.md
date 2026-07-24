# Windows Icon Unification Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 放大 Windows 任务栏图标，并让应用、安装包、商店、favicon 和托盘统一使用最新品牌头像。

**Architecture:** 以 `src-tauri/icons/icon.png` 为输入，通过可重复运行的 PowerShell/System.Drawing 脚本裁除多余透明边距，得到单一母版；Tauri CLI 从母版生成平台图标，托盘脚本再从同一母版生成空闲与录音动画帧。测试只验证尺寸、透明占用率、配置引用和旧托盘资产不再出现。

**Tech Stack:** PowerShell 7 / System.Drawing、Tauri CLI 2、Pillow 托盘生成脚本、Vitest、Rust/Tauri Windows bundler。

## Global Constraints

- `src-tauri/icons/icon.png` 是唯一品牌母版。
- 成品透明安全边距约 3%，不得裁切圆角、头发或笔记本。
- 托盘继续使用 44×44 彩色 PNG，并保留六帧录音动画。
- 不新增运行时依赖，不改现有 Tauri 图标文件名或托盘状态机接口。

---

### Task 1: 可重复的系统图标生成与检查

**Files:**
- Create: `scripts/refresh_windows_icons.ps1`
- Create: `src/lib/systemIcons.test.ts`
- Modify: `src-tauri/icons/icon.png`
- Regenerate: `src-tauri/icons/32x32.png`
- Regenerate: `src-tauri/icons/128x128.png`
- Regenerate: `src-tauri/icons/128x128@2x.png`
- Regenerate: `src-tauri/icons/icon.ico`
- Regenerate: `src-tauri/icons/icon.icns`
- Regenerate: `src-tauri/icons/Square*Logo.png`
- Regenerate: `src-tauri/icons/StoreLogo.png`
- Modify: `static/favicon.png`

**Interfaces:**
- Consumes: 当前 `icon.png` 的非透明内容边界。
- Produces: `refresh_windows_icons.ps1`，无参数运行，幂等生成全部平台资产。

- [ ] **Step 1: 写失败测试**

测试通过 `?raw` 读取生成脚本与 Tauri 配置，断言脚本采用 3% 安全边距、调用 `tauri icon`、生成 favicon，并覆盖配置引用的全部文件。

- [ ] **Step 2: 验证测试为红**

Run: `npm.cmd test -- src/lib/systemIcons.test.ts`

Expected: FAIL，因为 `scripts/refresh_windows_icons.ps1` 尚不存在。

- [ ] **Step 3: 实现图标生成脚本**

脚本用 `System.Drawing.Bitmap` 扫描 alpha 边界，将有效内容缩放到 94% 画布并居中，覆盖 `icon.png`；随后执行：

```powershell
npm.cmd run tauri -- icon src-tauri/icons/icon.png --output src-tauri/icons
```

再从同一母版高质量缩放生成 `static/favicon.png`。脚本必须校验所有 PNG 的 alpha 边界均未触边。

- [ ] **Step 4: 运行脚本并验证测试为绿**

Run:

```powershell
powershell -ExecutionPolicy Bypass -File scripts/refresh_windows_icons.ps1
npm.cmd test -- src/lib/systemIcons.test.ts
```

Expected: PASS；任务栏图标主体占用宽高约 94%。

### Task 2: 托盘图标统一

**Files:**
- Modify: `scripts/gen_tray_logo_frames.py`
- Regenerate: `src-tauri/icons/tray-logo-idle.png`
- Regenerate: `src-tauri/icons/tray-logo-rec-0.png` … `tray-logo-rec-5.png`
- Modify: `src/lib/systemIcons.test.ts`

**Interfaces:**
- Consumes: Task 1 的新版 `src-tauri/icons/icon.png`。
- Produces: `tray.rs` 现有 `include_bytes!` 文件名不变的七张 44×44 PNG。

- [ ] **Step 1: 扩充失败测试**

断言托盘生成脚本读取唯一母版、输出七张固定文件名，并且脚本文案与实现不再描述或生成旧版抠图人物。

- [ ] **Step 2: 验证测试为红**

Run: `npm.cmd test -- src/lib/systemIcons.test.ts`

Expected: FAIL，现有脚本仍执行旧版人物抠图。

- [ ] **Step 3: 修改并运行托盘生成器**

保留六帧书写动画参数，但以新版头像母版为视觉主体；运行项目可用的 Pillow Python。若本机没有 Python，则用 Task 1 的 System.Drawing 脚本等价生成七帧，且不引入应用运行时依赖。

- [ ] **Step 4: 验证尺寸、占用率和测试**

Run: `npm.cmd test -- src/lib/systemIcons.test.ts`

Expected: PASS；七帧均为 44×44，alpha 内容不触边且不再含旧版人物。

### Task 3: 全量验证、打包、安装与冒烟测试

**Files:**
- Verify: `src-tauri/tauri.conf.json`
- Verify: `src-tauri/tauri.windows.conf.json`
- Output: `C:\tmp\voice-notes-target-fresh\release\bundle\nsis\voice-notes_0.5.0_x64-setup.exe`

**Interfaces:**
- Consumes: Tasks 1–2 的全部图标资产。
- Produces: 已安装并运行的新 Windows 构建。

- [ ] **Step 1: 运行完整自动检查**

Run:

```powershell
npm.cmd test
npm.cmd run check
cargo check --manifest-path src-tauri/Cargo.toml
git diff --check
```

Expected: 所有测试通过、Svelte 0 错误 0 警告、Rust 检查退出码 0。

- [ ] **Step 2: 构建 Windows Release**

Run:

```powershell
npm.cmd run tauri -- build --config src-tauri/tauri.windows.conf.json
```

Expected: NSIS 与 MSI 均生成成功，五个语音 DLL 被复制。

- [ ] **Step 3: 覆盖安装并启动**

关闭现有 `voice-notes`，以 `/S` 安装 NSIS，启动
`C:\Users\lishuyuan\AppData\Local\voice-notes\voice-notes.exe`。

- [ ] **Step 4: 冒烟测试**

确认进程存活、主窗口可见、EXE/任务栏使用放大后的新版图标、托盘空闲图标为新版头像；
开始并停止一次短录音，确认托盘录音动画切换且主界面保持响应。

