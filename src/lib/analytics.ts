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

/** 后端算好的环境快照(命令 `app_env`)。**两端必须是同一份值**——见 stampEnv。 */
export type AppEnv = {
  app_version: string;
  os: string;
  os_version: string;
  arch: string;
  locale: string;
  is_debug: boolean;
};

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
    // 笔记标题、逐字稿片段。统一在出口处理,见 beforeSend。
    before_send: beforeSend,
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

// ---------------------------------------------------------------------------
// 出站唯一关卡
// ---------------------------------------------------------------------------

/** 同 fingerprint 每会话最多几条,以及全会话总上限。与 Rust 侧同值同语义
 *  (src-tauri/src/telemetry.rs 的 EXCEPTION_CAP_*)。设计文档「限流与额度」明写:
 *  异常按 fingerprint 限流,否则断流风暴那类高频错误一场会议就能打满月额度。 */
const EXCEPTION_CAP_PER_KIND = 5;
const EXCEPTION_CAP_TOTAL = 50;
const exceptionCounts = new Map<string, number>();
let exceptionTotal = 0;

/** 这条异常还能不能发。取不到 key 也要落进一个桶——**绝不因为取不到就放行**。 */
function exceptionAllowed(props: Record<string, unknown>): boolean {
  const list = props.$exception_list;
  const first = Array.isArray(list) && list[0] && typeof list[0] === "object"
    ? (list[0] as Record<string, unknown>)
    : undefined;
  const key =
    (typeof props.$exception_fingerprint === "string" && props.$exception_fingerprint) ||
    (typeof first?.type === "string" && first.type) ||
    (typeof props.$exception_type === "string" && props.$exception_type) ||
    "unknown";
  if (exceptionTotal >= EXCEPTION_CAP_TOTAL) return false;
  const n = exceptionCounts.get(key) ?? 0;
  if (n >= EXCEPTION_CAP_PER_KIND) return false;
  exceptionCounts.set(key, n + 1);
  exceptionTotal += 1;
  return true;
}

/** 后端给的环境快照。init 之前取到,之后每条事件都盖上。 */
let env: AppEnv | null = null;

/** 环境属性:每条事件都盖,**包括 posthog-js 自动捕获的异常与回放元数据**。
 *
 *  为什么要盖掉 posthog-js 自己算的 `$os`/`$os_version`:它从 UA 正则解析,而
 *  WKWebView 的 UA 冻结在 `Mac OS X 10_15_7`、WebView2 冻结在 `Windows NT 10.0`
 *  (Win11 也报 10.0)。看板上所有 mac 用户都会显示 10.15.7,Win11 与 Win10 分不开
 *  ——而本应用的采集行为恰恰按 macOS 大版本分叉(ScreenCaptureKit/CATap/授权)。
 *  `$app_version` 则是 posthog-js 压根不知道的东西:它只认得浏览器引擎版本。 */
export function stampEnv(props: Record<string, unknown>, e: AppEnv | null): void {
  if (!e) return;
  props.$app_version = e.app_version;
  props.$os = e.os;
  props.$os_version = e.os_version;
  props.app_arch = e.arch;
  props.app_locale = e.locale;
  props.app_is_debug = e.is_debug;
}

/** 全部出站事件的唯一关卡。三件事,顺序固定——总开关 → 异常限流 → 环境属性 → 脱敏。
 *  与 Rust 侧 `telemetry::before_send` 同结构;前两步会丢事件,后两步只改属性值。 */
export function beforeSend<
  T extends { event?: string; properties?: Record<string, unknown> } | null,
>(ev: T): T | null {
  if (!ev) return ev;
  if (optedOut) return null;
  if (ev.properties) {
    if (ev.event === "$exception" && !exceptionAllowed(ev.properties)) return null;
    stampEnv(ev.properties, env);
  }
  return redactEvent(ev);
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
/** 用户在设置里关掉了上报。**独立于 started**:关掉时已经 init 过的实例仍在,
 *  由 beforeSend 兜底拦住每一条——包括 posthog-js 自动捕获的异常与回放。 */
let optedOut = false;

/** 初始化。幂等;缺 key 或用户关掉时静默跳过。
 *
 *  **先取环境快照再 init**:少一次往返换来的是首个 pageview 带不带版本号。首启那条
 *  事件恰恰是激活漏斗的第一步,让它成为唯一一条不知道自己属于哪个版本的事件,
 *  等于把"某版本首启转化"这个问题永久留了个洞。 */
export async function initAnalytics(): Promise<void> {
  if (started || !PROJECT_KEY) return;
  try {
    const enabled = await invoke<boolean>("telemetry_enabled").catch(() => true);
    optedOut = !enabled;
    if (optedOut) return;
    await start();
  } catch {
    // 初始化失败不得影响应用启动
  }
}

/** 真正起 posthog。**不查开关**——调用方已经判定过了。
 *
 *  分出这一层是因为设置页那条路径不能再查一次:它在落盘之前调,后端读到的还是旧值,
 *  会把用户刚打开的开关立刻关回去(改完之后才落盘,症状是"打开了但要重启才生效")。 */
async function start(): Promise<void> {
  if (started) return;
  // 环境快照拿不到(IPC 不可用)也照常起:上报少几个维度,好过整块不工作。
  env = await invoke<AppEnv>("app_env").catch(() => null);
  // await 之后必须重查:这一跳期间用户完全可能又把开关关掉,不查就照样 init
  // 并开始录制(codex review P1#2)。
  if (optedOut || started) return;
  started = true;
  posthog.init(PROJECT_KEY, analyticsConfig());
  const id = ensureDistinctId(localStorage);
  posthog.identify(id);
  // 单向下发:后端一律接收、绝不自造 id。失败静默——上报不该影响任何主流程。
  void invoke("set_analytics_id", { id }).catch(() => {});
}

/** 设置页切换开关时调用。落盘由 set_settings 负责(它会同步后端的总开关),
 *  这里只管前端实例:关掉要当场停,不能等下次启动。 */
export function applyTelemetrySetting(on: boolean): void {
  optedOut = !on;
  try {
    if (!on) {
      if (started) {
        posthog.stopSessionRecording();
        posthog.opt_out_capturing();
      }
      return;
    }
    if (started) posthog.opt_in_capturing();
    else void start().catch(() => {});
  } catch {
    // 静默:开关本身绝不能因为上报库出错而失灵
  }
}

/** 前端事件。名字统一 vn_ 前缀,与 PostHog 自带的 $ 事件区分开。 */
export function capture(name: string, props?: Record<string, string | number | boolean>): void {
  if (!started || optedOut) return;
  try {
    posthog.capture(name, props);
  } catch {
    // 静默
  }
}

/** 前端上报一次失败。**走后端命令而不是本地 posthog.capture**:异常事件的形状
 *  (fingerprint 分组、$exception_list、脱敏)在 Rust 侧已有唯一实现,前端再写一份
 *  就是两套规则各自漂移——这正是本次要收掉的那类问题。kind 必须是后端
 *  `telemetry::ErrorKind` 的白名单值,认不出会被整条丢弃。 */
export function reportError(kind: string, detail: string): void {
  if (optedOut) return;
  void invoke("report_frontend_error", { kind, detail }).catch(() => {});
}
