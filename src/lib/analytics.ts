/** 产品分析(前端侧)。
 *
 *  分工:前端埋「用户看到了、点了什么」(页面浏览、关键点击),后端埋「事情真的
 *  发生了」(开录成功、转写完成、AI 整理完成),经 MCP/UDS 触发的也算——见
 *  src-tauri/src/telemetry.rs。两端共用同一个 distinct_id,否则漏斗跨不了端。
 *
 *  设计:docs/superpowers/specs/2026-08-17-posthog-analytics-and-error-tracking-design.md
 *  可行性实测:docs/superpowers/research/2026-08-17-posthog-tauri-spike.md
 */
import posthog from "posthog-js";
import { invoke } from "@tauri-apps/api/core";
import { redactEvent } from "$lib/redact";

/** 与后端同一个值。公开写入端点,不是机密。 */
export const PROJECT_KEY = "phc_qgqdrtaowrPfMPzmD9b7e9JSUPRc3RY3oGAeeKtAAV7E";
export const API_HOST = "https://us.i.posthog.com";

/** 匿名 id 的本地键。由前端生成并持久化,单向传给后端(见 set_analytics_id)。 */
const ID_KEY = "vn_analytics_id";

/** posthog.init 的配置。**独立导出是为了让防回归测试能直接断言它的形状**——
 *  下面每一项都是 spike 实测出的「默认值倒在危险一侧」,靠默认就会出事。 */
export function analyticsConfig() {
  return {
    api_host: API_HOST,
    // 官方 onboarding 给的默认值版本。它一次性设定 capture_pageview:"history_change"、
    // session_recording 的 strictMinimumDuration/canvasCapture、rageclick、
    // persistence_save_debounce_ms 等九项(逐项含义见 spike 裁决文档)。
    defaults: "2026-05-30" as const,

    // ① 元素文本遮蔽。**默认 false**,而本应用列表项的文本就是会议标题——
    //    spike 实测捕获到 `clicked link with text "近期工作安排通报"` 这类事件,
    //    与会话回放无关,光开 autocapture 就会泄漏。
    mask_all_text: true,
    //    mask_all_text 只遮元素**文本**,不遮**属性**。本应用把会议内容放进了属性:
    //    MiniPlayer 用笔记标题做 title、ForceGraph 把实体名放进 aria-label,
    //    点这些元素时会随 $elements 一起送出去(codex review 发现)。
    mask_all_element_attributes: true,

    // ② 会话回放:**已上线**(产品决策 2026-08-18)。
    //
    //    风险明示:spike 的判据 4(遮蔽实测硬门)从未通过验证——没有人实测过
    //    maskTextSelector:"*" 在 WKWebView/WebView2 里是否真的把文本遮住。本应用
    //    满屏都是会议内容,遮蔽若失效,上传的就是真实逐字稿。四道遮蔽都已配齐并被
    //    analytics.test.ts 锁死,但配置正确 ≠ 运行时生效,这两件事只能靠实测分开。
    //
    //    上线后第一件该做的事:打开第一条回放,确认笔记标题、说话人名、正文
    //    全部呈现为遮蔽块。看得到任何一个字就立刻关掉这一行并删除录制。
    disable_session_recording: false,
    session_recording: {
      maskAllInputs: true,
      maskTextSelector: "*",
      recordBody: false,
    },
    // ③ 控制台日志:默认值是 undefined,等于把行为交给服务端远端配置。
    //    本应用的 console.error 可能打印笔记内容,显式关死。
    enable_recording_console_log: false,

    // ④ 异常自动捕获:不显式设置时默认值同样由远端配置决定
    //    (posthog-js 源码 `wr(t) ? this.sl : t`)。console 错误刻意不捕获,同 ③。
    // 自动捕获的异常默认原样上传消息文本,而它常含 IPC 错误文案、路径、
    // 笔记标题、逐字稿片段。统一在此脱敏,不丢事件——丢了就看不见异常。
    before_send: redactEvent,
    capture_exceptions: {
      capture_unhandled_errors: true,
      capture_unhandled_rejections: true,
      capture_console_errors: false,
    },

    // ⑤ 内部/测试用户判定。defaults:"2026-05-30" 会把它设成
    //    /^(localhost|127\.0\.0\.1)$/,而 Tauri 生产在 macOS 上的来源正是
    //    tauri://localhost —— hostname 恰为 localhost。不覆盖的话,打包后**全部
    //    真实用户事件都会被标成内部测试流量**,看板默认过滤,症状是「接完之后
    //    看板一直是空的」且极难归因。
    //
    //    **必须用 null,不能用 undefined**:posthog-js 的配置合并是
    //    `for (s in n) void 0 !== n[s] && (t[s] = n[s])`,undefined 会被整个跳过,
    //    日期默认值的正则原样生效——写 undefined 等于什么都没做(codex review 发现)。
    internal_or_test_user_hostname: null,

    autocapture: true,
    // SPA:layout 常驻,goto 不触发整页加载。true 只捕获首次加载,
    // 之后的页面访问全丢,页面漏斗算不出来。
    capture_pageview: "history_change" as const,
  };
}

/** 取(必要时生成)本机匿名 id。只是随机串,不关联邮箱/设备序列号,不跨设备合并。 */
export function ensureDistinctId(store: Pick<Storage, "getItem" | "setItem">): string {
  const existing = store.getItem(ID_KEY);
  if (existing) return existing;
  const id = crypto.randomUUID();
  store.setItem(ID_KEY, id);
  return id;
}

let started = false;

/** 初始化。幂等;缺 key 时静默跳过(便于本地开发)。 */
export function initAnalytics(): void {
  if (started || !PROJECT_KEY) return;
  started = true;
  try {
    posthog.init(PROJECT_KEY, analyticsConfig());
    const id = ensureDistinctId(localStorage);
    posthog.identify(id);
    // 单向下发:后端一律接收、绝不自造 id。失败静默——上报不该影响任何主流程。
    void invoke("set_analytics_id", { id }).catch(() => {});
  } catch {
    // 初始化失败不得影响应用启动
  }
}

/** 前端事件。名字统一 vn_ 前缀,与 PostHog 自带的 $ 事件区分开。 */
export function capture(name: string, props?: Record<string, string | number | boolean>): void {
  if (!started) return;
  try {
    posthog.capture(name, props);
  } catch {
    // 静默
  }
}
