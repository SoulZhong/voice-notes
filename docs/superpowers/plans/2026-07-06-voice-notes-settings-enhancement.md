# 设置增强 + 系统集成 实施计划

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 设置页四个新区块:外观(主题三档)、录制(仅系统声音/语言过滤/保留音频)、磁盘(占用统计+按时间清理)、系统(可配置全局快捷键/开机自启/菜单栏常驻+关窗隐藏)。

**Architecture:** settings.json 加 7 字段驱动一切;主题走 CSS `light-dark()` 单点 token + colorScheme 切换;录制三开关是 spawn_session/start_session 的接线参数(不改算法);磁盘两命令走 store 纯逻辑;快捷键/托盘在 Rust 侧由 settings 驱动注册与重建,与前端解耦(托盘状态在 lib.rs 录制状态变化点同步);autostart 真值源是系统 LaunchAgent(JS 插件绑定直连,不进 settings)。

**Tech Stack:** tauri-plugin-global-shortcut / tauri-plugin-autostart / Tauri v2 内建 TrayIcon;CSS light-dark()。

**Spec:** `docs/superpowers/specs/2026-07-06-voice-notes-settings-enhancement-design.md`(字段表/语义/取舍以 spec 为准)

## Global Constraints

- 特性分支 `settings-enhancement`(从 master 建),每任务一提交,最终 push→PR→squash。
- 注释中文讲"为什么";cargo test 全过、npm run check 0/0、双端 build 无新警告;TDD(有单测面的任务先测后码)。
- 增值层姿态:快捷键注册失败回落关、托盘创建失败降级打日志、清理逐笔记 continue,绝不影响录制转写。
- 锁序纪律沿 lib.rs 顶部注释;toggle 录制共用函数绝不复制 start/stop 逻辑。
- 录制中允许改:仅系统声音/语言过滤/保留音频(下一场生效);主题/快捷键/托盘即时生效;set_settings 现有 asr_model/目录守卫不动。
- UI 按 DESIGN.md v2(区块卡/开关/radio 沿设置页既有形态;禁 emoji)。

---

### Task 1: settings 七字段

**Files:** Modify `src-tauri/src/settings.rs`

**Interfaces:** `Settings` 增(全部 `#[serde(default = ...)]`):`theme: String`("system")、`record_system_only: bool`(false)、`language_filter: bool`(true)、`keep_audio: bool`(true)、`shortcut_enabled: bool`(false)、`shortcut: String`("Alt+CmdOrCtrl+R")、`tray_enabled: bool`(true)。注意 bool 默认 true 的字段需 `default = "default_true"` 辅助函数(serde default 裸用是 false)。

- [ ] **Step 1: 失败测试**(既有 roundtrip 测试模式扩展):

```rust
    #[test]
    fn enhancement_fields_default_and_roundtrip() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("settings.json"), r#"{"mirror_enabled":false,"mirror_prefix":"x"}"#).unwrap();
        let s = load(tmp.path());
        assert_eq!(s.theme, "system");
        assert!(!s.record_system_only && s.language_filter && s.keep_audio);
        assert!(!s.shortcut_enabled);
        assert_eq!(s.shortcut, "Alt+CmdOrCtrl+R");
        assert!(s.tray_enabled);
        let s = Settings { theme: "dark".into(), record_system_only: true, language_filter: false,
            keep_audio: false, shortcut_enabled: true, shortcut: "Alt+CmdOrCtrl+K".into(),
            tray_enabled: false, ..Default::default() };
        save(tmp.path(), &s).unwrap();
        let got = load(tmp.path());
        assert_eq!(got.theme, "dark");
        assert!(got.record_system_only && !got.language_filter && !got.keep_audio);
        assert!(got.shortcut_enabled && !got.tray_enabled);
        assert_eq!(got.shortcut, "Alt+CmdOrCtrl+K");
    }
```

- [ ] **Step 2-4:** RED → 实现(Default impl 同步)→ GREEN(`cargo test settings`)。
- [ ] **Step 5: Commit** `feat(settings): 设置增强七字段`

---

### Task 2: 主题——app.css light-dark() 重构 + 应用层切换

**Files:** Modify `src/app.css`、`src/routes/+layout.svelte`、`DESIGN.md`(落地方式注记一段)

**要点:**
- app.css:`:root` 开头加 `color-scheme: light dark;`;每个双主题 token 改为单条 `--x: light-dark(亮值, 暗值);`(值逐一取自现文件两块,**不改任何色值**);双主题同值 token(radius/tint 背景/record/on-record 等)保持原样;整个 `@media (prefers-color-scheme: dark)` 块删除。注释说明"为什么 light-dark:token 单点定义,主题切换只动 color-scheme"。
- +layout.svelte:onMount 读 getSettings().theme 应用 `document.documentElement.style.colorScheme = theme === "system" ? "" : theme`;导出纯函数 `applyTheme(theme: string)` 放 `src/lib/models.ts` 旁或新 `src/lib/theme.ts`(设置页切换时也调它,单一实现)。
- DESIGN.md「双主题同权」原则处补一句落地方式(light-dark() 单点,手动主题 = color-scheme 覆盖)。

- [ ] **Step 1: 实现**(机械转换 25+ token,逐值核对是本任务唯一风险)。
- [ ] **Step 2: 验证** `npm run check` 0/0、`npm run build`;`grep -c "prefers-color-scheme" src/app.css` = 0;`grep -c "light-dark(" src/app.css` ≥ 20。
- [ ] **Step 3: Commit** `feat(design): app.css 改 light-dark() 单点 token,主题可 colorScheme 切换`

---

### Task 3: 语言过滤开关接线

**Files:** Modify `src-tauri/src/session.rs`、`src-tauri/src/lib.rs`

**Interfaces:** `start_session` 增参 `language_filter: bool`(位置放 echo hold 附近的配置参数群);session 内两处 `is_foreign_final(...)` 调用改为 `language_filter && is_foreign_final(...)`。lib.rs spawn_session 读 settings 传入(spawn 线程内已有 settings 读取点则复用)。

- [ ] **Step 1: 失败测试**:session.rs 既有语言过滤测试(`worker_...language...` 系)参数化——现测试传 true 语义不变;新增一例 `language_filter=false` 时日语标签段不被丢弃(复用既有 ScriptRecognizer fixture 姿态)。
- [ ] **Step 2-4:** RED → 实现 → 全量 `cargo test` GREEN(所有既有 start_session 调用点补参)。
- [ ] **Step 5: Commit** `feat(session): 语言幻觉过滤可开关(默认开)`

---

### Task 4: 仅系统声音 + 保留音频接线

**Files:** Modify `src-tauri/src/lib.rs`

**Interfaces:**
- `fn required_sources(system_only: bool) -> Vec<audio::Source>`:false → `[Mic]`(现状:mic 必备,system 可降级);true → `[System]`。纯函数带单测。
- spawn_session:线程内读 settings(已有读取点);`record_system_only=true` 时源列表只构建 System(macOS)——不建 VPIO mic、不建 mic VAD;Fix A 守卫改为 `required_sources(...)` 逐一检查 `start.active.contains`,任一缺失即 tear down 报错(错误文案带源名:「麦克风未能启动」/「系统声音未能启动」);classify_system 逻辑保持(system_only 下 system 必在 active,天然 "on")。
- `keep_audio=false` 时跳过整个 audio_sinks/audio_joins 构建段(空 Vec 传入,start_session 签名不变)。

- [ ] **Step 1: 失败测试**:

```rust
    #[test]
    fn required_sources_follow_system_only() {
        use crate::audio::Source;
        assert_eq!(super::required_sources(false), vec![Source::Mic]);
        assert_eq!(super::required_sources(true), vec![Source::System]);
    }
```

- [ ] **Step 2-4:** RED → 实现 → 全量 GREEN + `cargo build`。
- [ ] **Step 5: Commit** `feat: 仅系统声音录制与音频保留开关(必备源集合泛化)`

---

### Task 5: 磁盘统计与按时间清理

**Files:** Create `src-tauri/src/store/disk.rs`;Modify `src-tauri/src/store/mod.rs`、`src-tauri/src/lib.rs`

**Interfaces(disk.rs 纯文件逻辑):**
- `pub fn audio_usage_bytes(notes_root: &Path) -> u64`:walk 一层笔记目录,累计 `*.m4a`/`*.wav`/`*.m4a.bad` 字节。
- `pub fn purge_note_audio(note_dir: &Path)`:删上述三类文件 + `audio::clear_track_compressed` 逐 track(或直接重写 audio.json 清 codec/duration,offset 保留);失败 eprintln continue。
- `pub fn should_purge(note_dir: &Path, cutoff_rfc3339: Option<&str>) -> bool`:meta 可解析且 state=="complete" 且(cutoff 为 None 或 `ended_at`(空回退 started_at)< cutoff)。
- lib.rs 命令:`audio_disk_usage(app) -> Result<u64, String>`;`purge_audio(app, state, older_than_days: Option<u32>) -> Result<u64, String>`——守卫(录制中拒;`transcode.pause_and_wait()`,defer unpause)→ 遍历 notes 目录,`should_purge` 且非活动笔记 → 记录清理前字节 → `purge_note_audio` → 返回释放总字节。cutoff 由 `chrono::Local::now() - Duration::days(n)` 转 RFC3339(与 meta 格式同源可比)。invoke_handler 注册两命令。

- [ ] **Step 1: 失败测试**(disk.rs tests,tempdir 造笔记:meta.json+假 m4a/wav):usage 统计口径;should_purge 三态(complete+过期 true / recording false / 未过期 false / meta 损坏 false);purge 后音频文件没了、meta.json/segments 完好、audio.json codec 清除。
- [ ] **Step 2-4:** RED → 实现 → GREEN(`cargo test disk`)+ 全量。
- [ ] **Step 5: Commit** `feat(store): 录音音频磁盘统计与按时间清理`

---

### Task 6: 插件接线(global-shortcut / autostart)

**Files:** Modify `src-tauri/Cargo.toml`、`package.json`(+npm install)、`src-tauri/src/lib.rs`、`src-tauri/capabilities/default.json`

- Cargo:`tauri-plugin-global-shortcut = "2"`、`tauri-plugin-autostart = "2"`;npm:`@tauri-apps/plugin-autostart@^2`(global-shortcut 全在 Rust 侧,不需要 JS 包)。
- builder:`.plugin(tauri_plugin_autostart::init(tauri_plugin_autostart::MacosLauncher::LaunchAgent, None))`、`.plugin(tauri_plugin_global_shortcut::Builder::new().with_handler(shortcuts::on_shortcut).build())`(handler 函数 Task 7 提供,本任务先放空实现占位并注明)。
- capabilities permissions 加:`"autostart:allow-enable"`、`"autostart:allow-disable"`、`"autostart:allow-is-enabled"`。

- [ ] **Step 1: 实现** → **Step 2:** `cargo build` + `npm run build` 通过 → **Step 3: Commit** `chore: 接入 global-shortcut/autostart 插件`

---

### Task 7: 快捷键注册模块 + 切换录制共用函数

**Files:** Create `src-tauri/src/shortcuts.rs`;Modify `src-tauri/src/lib.rs`

**Interfaces:**
- lib.rs:`pub(crate) fn toggle_recording(app: &AppHandle)`——读 `app.state::<AppState>()`:有活动会话(或 running)→ 调 stop_recording 的**内部实现**;否则 recording_ready 且无互斥 → 调 start 的内部实现。为此把 start_recording/stop_recording 命令体抽成 `fn do_start_recording(app: &AppHandle) -> Result<(), String>` / `fn do_stop_recording(app: &AppHandle)`(命令变薄壳,托盘/快捷键共用;State 参数经 `app.state()` 取)。
- shortcuts.rs:
  - `pub fn on_shortcut(app: &AppHandle, _sc: &Shortcut, ev: ShortcutEvent)`:只响应 `ShortcutState::Pressed`,调 `toggle_recording`。
  - `pub fn apply_from_settings(app: &AppHandle) -> Result<(), String>`:`app.global_shortcut().unregister_all()` 后,若 `shortcut_enabled` 则 `register(shortcut.parse::<Shortcut>()...)`;parse/register 失败返回中文错误(设置页显示)。
  - setup 调一次(失败仅 eprintln——启动不因坏快捷键挡);新命令 `apply_shortcut(app) -> Result<(), String>`(设置页保存后调,错误上抛;失败时命令内把 shortcut_enabled 写回 false 再返回 Err,即"注册失败回落关")。
- invoke_handler 注册 `apply_shortcut`。

- [ ] **Step 1: 失败测试**:快捷键字符串解析守卫纯逻辑薄(parse 由插件承担),单测面小——补一个 `apply_from_settings` 对 `shortcut_enabled=false` 时 unregister-only 不报错的集成性测试不可行(需 AppHandle)→ 以编译+全量回归为门,`toggle_recording` 的抽取以「start/stop 命令测试面为既有 lib 测试+冒烟」交代,不新增假测试。
- [ ] **Step 2: 实现** → **Step 3:** `cargo test` 全量 + `cargo build` → **Step 4: Commit** `feat: 可配置全局快捷键(注册失败回落)+ 录制切换共用函数`

---

### Task 8: 菜单栏托盘 + 关窗隐藏

**Files:** Create `src-tauri/src/tray.rs`、`scripts/gen_tray_icons.py`、`src-tauri/icons/tray-idle.png`、`src-tauri/icons/tray-recording.png`;Modify `src-tauri/src/lib.rs`

**Interfaces:**
- `scripts/gen_tray_icons.py`:纯 stdlib(struct+zlib)渲染两枚 44×44 PNG——idle:黑色圆环(模板图,macOS 自适应亮暗菜单栏);recording:实心红圆 `#ff6161`。脚本可重跑,产物提交入库。
- tray.rs:
  - `pub fn setup(app: &AppHandle)`:`tray_enabled` 时 `TrayIconBuilder::with_id("main-tray")` + idle 图标(`icon_as_template(true)`)+ 菜单(`toggle`:「开始录制」/ `show`:「打开主窗口」/ `quit`:「退出」)+ `on_menu_event`(toggle → `toggle_recording`;show → 主窗 show+set_focus;quit → 有会话先 `do_stop_recording` 再 `app.exit(0)`);失败 eprintln 降级。
  - `pub fn set_recording(app: &AppHandle, recording: bool)`:`tray_by_id("main-tray")` → set_icon(recording ? 红点(icon_as_template(false)) : 模板环) + 重建菜单文案「停止录制」/「开始录制」。
  - `pub fn apply_enabled(app: &AppHandle)`:开关变更时建/销托盘(销 = `app.remove_tray_by_id`)。
- lib.rs:
  - setup:tray::setup + 主窗 `on_window_event`:`CloseRequested` 且 settings.tray_enabled → `api.prevent_close()` + `window.hide()`(注释:录制不中断;读 settings 每次事件现读,开关即时生效)。
  - 录制状态变化点调 `tray::set_recording`:spawn_session 成功入槽后(true)、do_stop_recording 尾部(false)、spawn 失败 fail 路径(false)。
  - set_settings:tray_enabled 变更 → `tray::apply_enabled`。

- [ ] **Step 1: 生成图标**(跑脚本,肉眼查 PNG)→ **Step 2: 实现** → **Step 3:** `cargo test` 全量 + `cargo build` + `npm run build` → **Step 4: Commit** `feat: 菜单栏常驻托盘(录制红点态)+ 关窗隐藏`

---

### Task 9: 前端 API 与快捷键录入纯函数

**Files:** Modify `src/lib/models.ts`;Create `src/lib/shortcut.ts`

- models.ts:`Settings` 类型补七字段;`applyShortcut = () => invoke<void>("apply_shortcut")`、`audioDiskUsage = () => invoke<number>("audio_disk_usage")`、`purgeAudio = (olderThanDays: number | null) => invoke<number>("purge_audio", { olderThanDays })`。
- shortcut.ts(纯函数,无 tauri 依赖):
  - `acceleratorFromEvent(e: KeyboardEvent): string | null`——修饰键组合(metaKey→"CmdOrCtrl"、altKey→"Alt"、ctrlKey→"Ctrl"、shiftKey→"Shift")+ 主键(e.code 转字母/数字/F 键;纯修饰键返回 null);
  - `displayShortcut(acc: string): string`——"Alt+CmdOrCtrl+R" → "⌥⌘R"(mac 符号序:⌃⌥⇧⌘)。
- theme.ts(若 Task 2 未建则本任务建):`applyTheme(theme: string)`。

- [ ] **Step 1: 实现** → **Step 2:** `npm run check` 0/0 → **Step 3: Commit** `feat(ui): 设置增强前端 API 与快捷键组装纯函数`

---

### Task 10: 设置页四区块 UI

**Files:** Modify `src/routes/settings/+page.svelte`、`src/routes/+layout.svelte`(主题启动应用,若 Task 2 未完成接线)

**行为规格(视觉沿设置页既有区块卡形态):**
- **外观**(置顶):radio 三选 亮色/暗色/跟随系统;onchange 存 settings + `applyTheme`,即时生效。
- **录制**:三开关(仅系统声音/语言幻觉过滤/保留录音音频),各带一行说明(spec 语义文案);录制中**允许改**,旁注「下一场录制生效」。
- **磁盘**:「录音音频占用 X」(audioDiskUsage 格式化 MB/GB)+「清理…」secondary 按钮 → 行内展开三选(30 天前/90 天前/全部)+ 两段确认(danger,文案「只删音频,笔记文字与说话人保留」);完成后显示「已释放 Y」并刷新统计;录制中禁用。
- **系统**:①全局快捷键:开关 + 录入框(readonly input,聚焦提示「按下组合键」,keydown 经 acceleratorFromEvent 组装,显示 displayShortcut,Esc 取消)——变更后存 settings 并 `applyShortcut()`,Err → 红字 + 开关回弹(后端已回落 false,重新 getSettings 同步);②开机自启:开关直连 `@tauri-apps/plugin-autostart` 的 enable/disable/isEnabled(onMount 读真值);③菜单栏常驻:开关存 settings(后端 set_settings 钩子生效),说明「开启时关闭窗口只隐藏,录制不中断」。
- 区块顺序:外观/录制/磁盘/系统在既有存储/模型/语音识别**之前**(高频项前置)。

- [ ] **Step 1: 实现** → **Step 2:** `npm run check` 0/0 + `npm run build`;Playwright 截 localhost:1420/settings 目检区块形态 → **Step 3: Commit** `feat(ui): 设置页外观/录制/磁盘/系统四区块`

---

### Task 11: 全量验证与 PR

- [ ] **Step 1:** cargo test 全过、npm check 0/0、双端 build;svelte 硬编码色扫描 0。
- [ ] **Step 2:** push + `gh pr create`:变更概览、spec 的 8 项冒烟清单全文、已知取舍(autostart 真值源/托盘退出收尾秒级延迟/清理跳过已中断笔记/light-dark 回退方案)。

---

## Self-Review 记录

- spec 覆盖:七字段(T1)、主题(T2)、语言过滤(T3)、仅系统声音+keep_audio(T4)、磁盘(T5)、插件(T6)、快捷键(T7)、托盘+关窗(T8)、前端(T9/T10)、验收(T11+冒烟)。autostart 无独立后端任务系有意(JS 插件直连,T6 权限+T10 UI 即全部)。
- 占位符:无 TBD;T6 的 handler 占位在 T7 立即收口,两任务 Step 里都写明。
- 类型一致:`toggle_recording`/`do_start_recording`/`do_stop_recording`、`tray::{setup,set_recording,apply_enabled}`、`shortcuts::{on_shortcut,apply_from_settings}`、三条新命令名在各任务 Interfaces 一致。
- 顺序依赖:T7 依赖 T6 插件与 T1 字段;T8 依赖 T7 的 toggle;T10 依赖 T9;串行执行成立。
