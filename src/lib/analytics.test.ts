import { describe, expect, it } from "vitest";
import { analyticsConfig, ensureDistinctId } from "./analytics";

/** 配置形状锁定。规格对齐后端的 payload_shape_locked:改动必须先改测试,
 *  强制走一次隐私红线审视。
 *
 *  这里锁的每一项都是 spike(2026-08-17)实测出的「默认值倒在危险一侧」——
 *  靠默认就会泄漏内容或让看板一直空着,而代码里看不出问题。 */
describe("analyticsConfig(危险默认值必须被显式覆盖)", () => {
  const cfg = analyticsConfig();

  it("mask_all_text 必须为 true——否则 autocapture 会把会议标题当元素文本上报", () => {
    // spike 实测泄漏样本:clicked link with text "近期工作安排通报"
    expect(cfg.mask_all_text).toBe(true);
  });

  it("会话回放默认不上线——遮蔽硬门(判据 4)未通过验证前不得开启", () => {
    expect(cfg.disable_session_recording).toBe(true);
    // 遮蔽配置须已就位,开启只需翻上面那一个开关
    expect(cfg.session_recording.maskAllInputs).toBe(true);
    expect(cfg.session_recording.maskTextSelector).toBe("*");
    expect(cfg.session_recording.recordBody).toBe(false);
  });

  it("控制台日志不得录制——本应用 console.error 可能打印笔记内容", () => {
    // 默认值是 undefined(交给远端配置),必须显式 false
    expect(cfg.enable_recording_console_log).toBe(false);
  });

  it("异常捕获显式写死,且不捕获 console 错误", () => {
    // 不显式设置时默认值由远端配置决定,行为会随服务端开关漂移
    expect(cfg.capture_exceptions.capture_unhandled_errors).toBe(true);
    expect(cfg.capture_exceptions.capture_unhandled_rejections).toBe(true);
    expect(cfg.capture_exceptions.capture_console_errors).toBe(false);
  });

  it("mask_all_element_attributes 必须为 true——标题藏在 title/aria-label 属性里", () => {
    // mask_all_text 只遮文本不遮属性:MiniPlayer 用笔记标题做 title,
    // ForceGraph 把实体名放进 aria-label,点击时随 $elements 送出。
    expect(cfg.mask_all_element_attributes).toBe(true);
  });

  it("before_send 必须挂上脱敏——自动捕获的异常默认原样上传", () => {
    expect(typeof cfg.before_send).toBe("function");
  });

  it("capture_pageview 必须是 history_change——SPA 里 true 只捕获首次加载", () => {
    expect(cfg.capture_pageview).toBe("history_change");
  });

  it("必须关掉 internal_or_test_user_hostname——否则 macOS 全量真实流量被判成内部测试", () => {
    // defaults:"2026-05-30" 会把它设成 /^(localhost|127\.0\.0\.1)$/,
    // 而 Tauri 生产在 macOS 的来源是 tauri://localhost,hostname 正是 localhost。
    // 不覆盖的症状:看板一直是空的,且极难归因。
    // **必须是 null,不能是 undefined**:posthog-js 的配置合并
    // `void 0 !== n[s] && (t[s] = n[s])` 会跳过 undefined,写 undefined 等于没写。
    expect(cfg.internal_or_test_user_hostname).toBeNull();
  });

  it("host 与项目区域一致", () => {
    expect(cfg.api_host).toBe("https://us.i.posthog.com");
  });
});

describe("ensureDistinctId(匿名 id 只生成一次)", () => {
  function memStore() {
    const m = new Map<string, string>();
    return {
      getItem: (k: string) => m.get(k) ?? null,
      setItem: (k: string, v: string) => void m.set(k, v),
    };
  }

  it("首次生成并持久化,再次调用返回同一个", () => {
    const s = memStore();
    const first = ensureDistinctId(s);
    expect(first).toBeTruthy();
    expect(ensureDistinctId(s)).toBe(first);
  });

  it("已有值时不覆盖——覆盖会把同一个人算成两个人", () => {
    const s = memStore();
    s.setItem("vn_analytics_id", "existing-id");
    expect(ensureDistinctId(s)).toBe("existing-id");
  });
});
