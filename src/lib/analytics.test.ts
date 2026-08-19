import { describe, expect, it } from "vitest";
import { analyticsConfig, beforeSend, ensureDistinctId, stampEnv } from "./analytics";

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

  it("回放已上线,但四道遮蔽必须同时就位——遮蔽是回放唯一的防线", () => {
    // 产品决策 2026-08-18 开启回放。判据 4(遮蔽实测)仍未通过,
    // 因此这条测试的重点从"不许开"转为"开了就必须带齐遮蔽"。
    expect(cfg.disable_session_recording).toBe(false);
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

describe("stampEnv(环境属性:两端同一份值)", () => {
  const env = {
    app_version: "0.12.0",
    os: "macOS",
    os_version: "15.6.1",
    arch: "aarch64",
    locale: "zh-CN",
    is_debug: false,
  };

  it("盖掉 posthog-js 那份被 UA 冻结的系统版本", () => {
    // WKWebView 的 UA 自 macOS 11 起冻结在 Mac OS X 10_15_7,posthog-js 就是从
    // 这条 UA 正则解析出 $os_version 的 —— 不盖掉,看板上所有 mac 用户都是 10.15.7,
    // 而本应用的采集行为恰恰按 macOS 大版本分叉。
    const props: Record<string, unknown> = { $os: "Mac OS X", $os_version: "10.15.7" };
    stampEnv(props, env);
    expect(props.$os_version).toBe("15.6.1");
    expect(props.$os).toBe("macOS");
  });

  it("补上 posthog-js 压根不知道的应用版本与架构", () => {
    const props: Record<string, unknown> = {};
    stampEnv(props, env);
    expect(props.$app_version).toBe("0.12.0");
    expect(props.app_arch).toBe("aarch64");
    expect(props.app_locale).toBe("zh-CN");
    expect(props.app_is_debug).toBe(false);
  });

  it("环境还没取到时原样放过,绝不写 undefined 进属性", () => {
    const props: Record<string, unknown> = { $os_version: "10.15.7" };
    stampEnv(props, null);
    expect(props.$app_version).toBeUndefined();
    expect(props.$os_version).toBe("10.15.7");
  });
});

describe("beforeSend(出站唯一关卡)", () => {
  function exception(fingerprint: string, value = "boom") {
    return {
      event: "$exception",
      properties: {
        $exception_fingerprint: fingerprint,
        $exception_list: [{ type: "Error", value }],
      },
    };
  }

  it("同 fingerprint 超过上限即丢弃——断流风暴一场会议就能打满月额度", () => {
    // 设计文档「限流与额度」要求的,此前两端都只有注释没有实现。
    for (let i = 0; i < 5; i++) {
      expect(beforeSend(exception("gap_storm"))).not.toBeNull();
    }
    expect(beforeSend(exception("gap_storm"))).toBeNull();
    // 别的 fingerprint 不受连累
    expect(beforeSend(exception("other_kind"))).not.toBeNull();
  });

  it("普通事件绝不丢,且顺带脱敏", () => {
    const out = beforeSend({
      event: "$pageview",
      properties: { $current_url: "tauri://localhost/notes/note-1", msg: "写入 /Users/张伟/x.json 失败" },
    });
    expect(out).not.toBeNull();
    expect(out?.properties.$current_url).toBe("tauri://localhost/notes/note-1");
  });

  it("异常事件的内容照样脱敏,而不是因为限流放过", () => {
    const out = beforeSend(exception("redact_probe", "写入 /Users/张伟/notes/季度复盘会.json 失败"));
    const v = (out?.properties.$exception_list as Array<{ value: string }>)[0].value;
    expect(v).not.toContain("张伟");
    expect(v).not.toContain("季度复盘会");
  });
});
