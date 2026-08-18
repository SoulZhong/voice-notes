# PostHog Tauri WebView Spike 裁决

日期：2026-08-17 起，2026-08-18 更新
状态：进行中（判据 3/4 未做，判据 2 生产侧待确认；Error tracking 已加测通过）
计划：`docs/superpowers/plans/2026-08-17-posthog-tauri-spike.md`
设计：`docs/superpowers/specs/2026-08-17-posthog-analytics-and-error-tracking-design.md`

环境：posthog-js 1.417.4，PostHog Cloud US，`defaults: "2026-05-30"`，macOS。

## 判据实测记录

### 判据 1 · posthog-js 能否初始化并发出事件

**通过（dev 模式）。** 应用启动后 PostHog Activity 出现 `spike_app_started`，同批还有 `$pageview`、`$autocapture`、`Web vitals`、`Set person properties`，共 18 条。匿名 ID 正常生成（`01a012a3-5269-71d8-ae55-7ae0689cded7`）。

### 判据 2 · 请求能否到达 PostHog 域

**dev 模式通过，生产构建待确认。**

dev 下所有事件的 URL 为 `http://localhost:1420/...`——这是 vite dev server 的来源，**不是生产来源**。Tauri 打包后前端走自定义协议：macOS/Linux 为 `tauri://localhost`，Windows 为 `http://tauri.localhost`。来源不同，跨域策略与 localStorage 可用性都可能不同，而 PostHog 依赖 localStorage 存匿名 ID。

已构建生产包并启动，等待确认生产来源下事件是否照常到达。**在确认前，判据 2 不得记为通过。**

### 判据 5 · 点击能否带出元素信息（热图原料）

**通过，但暴露出一个比判据本身更重要的问题。**

`$autocapture` 正常产生并带元素信息。但带出来的是**元素文本**，而本应用的列表项文本就是会议标题，实测捕获到：

- `clicked link with text "近期工作安排通报"`
- `clicked link with text "推进技术与产品集成"`
- `clicked link with text "周一下午的会议"`
- `clicked span with text "2026-08-17 19:18 · 24 分 19 秒"`

即：**默认配置的 autocapture 会把会议标题上报，与会话回放无关**。原以为标题泄漏风险在回放侧，实际光开 autocapture 就足够。

URL 侧安全，符合设计预期：`/notes/20260817-191831` 只含 note-id，不含标题。

用户裁决（2026-08-18）：**接受标题上报，不清理已上传数据。** 据此，设计文档里「绝不包含任何会议内容」这条红线需要改写为准确表述（详见下方待办），否则文档与实际行为不一致。

说话人姓名是否被 autocapture 带出，尚未确认（点击 chip 那条未在列表中辨认出来）。

## 额外验证：Error tracking（超出原五条判据，用户要求加测）

**两条自动捕获路径均通过。** 未捕获同步错误（`window.onerror`）与未处理 Promise 拒绝（`unhandledrejection`）都到达了 PostHog。

注意读法陷阱：Issues 列表页显示 `Showing one entry` 指的是**一个 issue**，不是一条异常。两条异常被归并进同一 issue，点进去才看得到并列的两条（`OCCURRENCES 2`）。据此曾一度误判「拒绝类未捕获」，实为归并所致。

### 隐私：异常载荷原样带出敏感内容

刻意仿造 spec 中真实泄漏形态的测试异常，PostHog 中原样呈现：

```
ZZSPIKEERRZZ refine(note-140751): 写入 /Users/ZZNAMEZZ张伟/Library/Application Support/voice-notes/notes/ZZTITLEZZ季度复盘会.json 失败
```

家目录中的姓名与会议标题**一字未漏**。阶段 3 的脱敏由此从「设计上应该做」变成「实测证明必须做」。

### 堆栈可用性

dev 下栈帧为 `/src/lib/spikePosthog.ts@41:20`，可直接定位；但无源码上下文（帧旁 `?`/⚠️ 图标）。样本只有一帧是因为错误抛在 `setTimeout` 回调内，栈本就浅，非能力上限。

**生产是另一回事**：前端被压缩为 `_app/immutable/*.js`，栈帧将退化为 `xxx.js@1:23456`，无定位价值。**阶段 3 必须在构建流程中加入 source map 上传**，否则生产异常只能知道「出错了」而不知「在哪」，相对现有 `stderr.log` 无实质提升。与 Rust 侧上传 debug symbols 是同一件事的两端。

### 归并行为需要 fingerprint

「写文件失败」与「未处理 Promise 拒绝」这两条语义无关的错误被归并进同一 issue（推测因均为裸 `Error` 且来自同一模块）。阶段 3 必须设计 fingerprint，否则线上会出现一个 issue 混装无关错误，既看不清，也无法按 fingerprint 限流——而 spec 的限流方案正是按 fingerprint 折叠的。

### 配置发现

`capture_exceptions` 不显式设置时，其默认值**由远端配置决定**（源码 `wr(t) ? this.sl : t`），即异常捕获是否开启会受服务端开关影响。必须在代码中显式写死。本 spike 设为 `{capture_unhandled_errors: true, capture_unhandled_rejections: true, capture_console_errors: false}`——console 错误刻意关闭，本应用的 `console.error` 可能带笔记内容。

### 判据 3 · 回放是否可播放

**通过。** 开启项目级录制开关后产出了可播放的记录（2026-08-18 由用户在 PostHog 后台确认）。

读法陷阱记一笔：客户端配置齐全**不等于会录**——posthog-js 源码里有 `.sessionRecording &&` 这道远端配置门，服务端不返回该配置就一帧不录。也就是说回放的开关实际在服务端，而遮蔽配置在客户端：**后台一开录制、客户端某版本遮蔽没跟上，就会在无人察觉时开始录真实内容。**

### 判据 4 · 全文本遮蔽是否生效（硬门）

**未验证。** 用户于 2026-08-18 决定不做该验证，并决定回放照常上线。

四道遮蔽已在生产代码中配齐（`mask_all_text`、`maskAllInputs`、`maskTextSelector: "*"`、`recordBody: false`）并被配置形状锁定测试守住，但**没有任何实测证据表明它们在 WKWebView / WebView2 的运行时里真的生效**。这是本次接入唯一一处「有配置、无证据」的地方，风险与处置写在设计文档中。

## 额外发现：`defaults: "2026-05-30"` 的实际含义

从本地安装的 posthog-js 1.417.4 产物（`dist/array.full.js`）中直接解出，非文档转述。该值实际设定：

| 选项 | 值 |
|---|---|
| `capture_pageview` | `"history_change"` |
| `session_recording` | `{strictMinimumDuration: true, canvasCapture: {resolutionScale: 0.6}}` |
| `rageclick` | `{content_ignorelist: …, ignore_text_selection: true}` |
| `internal_or_test_user_hostname` | `/^(localhost\|127\.0\.0\.1)$/` |
| `persistence_save_debounce_ms` | `250` |
| `split_storage` | `true` |
| `detect_google_search_app` | `true` |
| `external_scripts_inject_target` | `"head"` |
| `disable_capture_url_hashes` | `false` |

**`internal_or_test_user_hostname` 是个陷阱。** 它把 hostname 为 `localhost` 的流量标记为「内部/测试用户」，而 PostHog 看板默认过滤这类流量。

Tauri 生产构建在 macOS/Linux 上的来源是 `tauri://localhost`，hostname 恰为 `localhost` —— **打包后全部真实用户事件都会被标成内部测试流量，看板默认看不见**。Windows 的 `tauri.localhost` 不匹配该正则（正则两端锚定），不受影响。**同一套代码，两个平台行为不一致。**

阶段 2 必须显式覆盖 `internal_or_test_user_hostname`。这一条若不处理，症状是「接完之后看板一直是空的」，且极难归因。

## 对后续阶段的影响

1. **阶段 2 必须显式设 `internal_or_test_user_hostname`**，否则 macOS 全量真实流量被默认过滤。
2. **阶段 2 必须决定 autocapture 的元素文本策略**：关掉、或用文本脱敏配置屏蔽元素文本（热图保留，「点了哪个按钮」的语义损失一部分）。
3. **设计文档的红线表述要改**：现表述「绝不包含任何会议内容」与「接受标题上报」冲突，需改为准确描述实际边界（如「不包含逐字稿正文」）。文档与行为不一致比红线本身更危险。
4. `session_recording` 的 `canvasCapture` 由 defaults 打开，阶段 4 做回放时需一并评估（本应用有波形画布）。

## 未解决

- 判据 2 生产来源确认。
- 判据 3、判据 4（硬门）未做。
- Windows/WebView2 完全未验证。
- 说话人姓名是否经 autocapture 外泄。
