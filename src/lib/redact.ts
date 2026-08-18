/** 上报前的脱敏(前端侧)。与 Rust 侧 src-tauri/src/redact.rs 同一套规则,
 *  两边用同一组测试向量钉死——规则漂移了,两侧测试会一起红。
 *
 *  为什么必须做:spike 实测(2026-08-17)把一条仿真错误发到 PostHog,原样呈现出
 *  家目录里的真实姓名与 notes 路径里的会议标题。前端异常同理会带出 IPC 错误文案、
 *  文件路径、笔记标题、逐字稿片段——posthog-js 的自动异常捕获默认原样上传。
 */

/** 连续多少个中日韩字符就整段丢弃。与 Rust 侧 CJK_RUN_DROP 必须一致。 */
const CJK_RUN_DROP = 12;

const CJK = /[\u4E00-\u9FFF\u3400-\u4DBF\u3040-\u30FF\uAC00-\uD7AF]/;

/** 家目录路径(含 Windows)收敛。用户名常常就是真实姓名,notes 下的文件名就是会议标题。 */
function redactHomePaths(s: string): string {
  return s
    // macOS/Linux。注意 "Application Support" 含空格,不能以空白为终点。
    .replace(/(\/Users\/|\/home\/)[^\n"',;)\]]*/g, "<HOME_PATH>")
    // Windows:C:\Users\Alice\...
    .replace(/[A-Za-z]:\\Users\\[^\n"',;)\]]*/g, "<HOME_PATH>");
}

/** 密钥形态。用 includes 而非 startsWith:`key=sk-xxx` 这种带前缀的会漏网。 */
function redactKeys(s: string): string {
  return s
    .split(/\s+/)
    .map((w) => {
      const looksKey =
        w.includes("sk-") ||
        w.includes("phc_") ||
        w.includes("phx_") ||
        w.includes("A-SH-") ||
        (w.length >= 32 && /^[0-9a-fA-F]+$/.test(w));
      return looksKey ? "<REDACTED>" : w;
    })
    .join(" ");
}

/** 丢弃过长的中日韩连续串。**整段丢弃而非截断**——保留前缀等于保留内容。 */
function dropLongCjkRuns(s: string): string {
  let out = "";
  let run = "";
  const flush = () => {
    out += [...run].length >= CJK_RUN_DROP ? "<TEXT>" : run;
    run = "";
  };
  for (const c of s) {
    if (CJK.test(c)) {
      run += c;
    } else {
      flush();
      out += c;
    }
  }
  flush();
  return out;
}

export function redact(input: string): string {
  return dropLongCjkRuns(redactKeys(redactHomePaths(input)));
}

/** posthog-js 的 before_send:对异常事件的消息字段做脱敏。
 *
 *  只改属性值,不丢事件——丢事件等于看不见异常,与上报的目的相悖。 */
export function redactEvent<T extends { event?: string; properties?: Record<string, unknown> } | null>(
  ev: T,
): T {
  if (!ev || !ev.properties) return ev;
  const p = ev.properties;
  // 现代 PostHog 错误追踪用 $exception_list;旧的标量字段一并处理,两种都覆盖。
  const list = p.$exception_list;
  if (Array.isArray(list)) {
    p.$exception_list = list.map((item) => {
      if (item && typeof item === "object") {
        const o = item as Record<string, unknown>;
        if (typeof o.value === "string") o.value = redact(o.value);
        if (typeof o.type === "string") o.type = redact(o.type);
      }
      return item;
    });
  }
  for (const k of ["$exception_message", "$exception_type", "$exception_source"]) {
    if (typeof p[k] === "string") p[k] = redact(p[k] as string);
  }
  return ev;
}
