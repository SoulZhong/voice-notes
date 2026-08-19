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

/** 路径的硬终点:出现即收尾,不再往下吃。 */
const PATH_STOP = new Set(['"', "'", ",", ";", "\n", "\r", ")", "]"]);

/** 空格后面这个词还属于路径吗?
 *
 *  与 Rust 侧 `path_end` 的判据逐条对齐。会议标题里常有空格(`Q3 roadmap.json`),
 *  只看"是否大写开头"会在 `Q3 ` 处停下、把 roadmap.json 漏出去;而以空白为终点又会
 *  漏掉 macOS 的 `Application Support`。判据:含分隔符、含扩展名点、或大写开头。 */
function looksPath(word: string): boolean {
  if (!word) return false;
  if (word.includes("/") || word.includes("\\")) return true;
  if (word.includes(".")) {
    const ext = word.split(".").pop() ?? "";
    if (ext.length > 0 && ext.length <= 5 && /^[0-9a-zA-Z]+$/.test(ext)) return true;
  }
  const first = word[0];
  return first !== first.toLowerCase() && first === first.toUpperCase();
}

/** 从路径起点算到终点。**与 Rust 侧同算法,而不是各写各的正则**:原先 TS 那条
 *  `[^\n"',;)\]]*` 会一路吃到行尾,把后面的英文措辞一起吞掉,而 Rust 用的是逐字符
 *  判据。两侧号称共用同一组测试向量,可规则本身早就漂了——漂移正是这组向量要防的事。 */
function pathEnd(tail: string): number {
  for (let i = 1; i < tail.length; i++) {
    const c = tail[i];
    if (PATH_STOP.has(c)) return i;
    if (c === " ") {
      const word = tail.slice(i + 1).split(/\s/)[0] ?? "";
      if (!looksPath(word)) return i;
    }
  }
  return tail.length;
}

/** 家目录路径(含 Windows)收敛。用户名常常就是真实姓名,notes 下的文件名就是会议标题。 */
function redactHomePaths(input: string): string {
  let out = "";
  let rest = input;
  for (;;) {
    const starts = ["/Users/", "/home/"].map((k) => rest.indexOf(k)).filter((i) => i >= 0);
    // Windows:C:\Users\Alice\... 盘符是 ":\Users\" 前面那一个字符。
    const win = rest.search(/[A-Za-z]:\\Users\\/);
    if (win >= 0) starts.push(win);
    if (starts.length === 0) break;
    const pos = Math.min(...starts);
    out += rest.slice(0, pos) + "<HOME_PATH>";
    const tail = rest.slice(pos);
    rest = tail.slice(pathEnd(tail));
  }
  return out + rest;
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
        // 栈帧里的 filename/abs_path 是文件系统路径,与 value 一样在禁传之列。
        // Rust 侧在第二轮 codex review 时补过这一处,TS 侧没跟上——同一个漏洞
        // 在两端只修了一半,正是"两端各写一套"最典型的失败形态。
        redactFrames(o.stacktrace);
      }
      return item;
    });
  }
  for (const k of [
    "$exception_message",
    "$exception_type",
    "$exception_source",
    // panic/错误事件还带这些独立的路径属性,只脱 value 会把它们漏出去。
    "$exception_panic_file",
    "$exception_stack_trace_raw",
  ]) {
    if (typeof p[k] === "string") p[k] = redact(p[k] as string);
  }
  return ev;
}

/** 脱一份 stacktrace 里所有帧的路径字段。结构不认识就原样放过——
 *  宁可放过结构变化,也不能因为解析失败把事件丢掉。 */
function redactFrames(stacktrace: unknown): void {
  if (!stacktrace || typeof stacktrace !== "object") return;
  const frames = (stacktrace as Record<string, unknown>).frames;
  if (!Array.isArray(frames)) return;
  for (const f of frames) {
    if (!f || typeof f !== "object") continue;
    const fm = f as Record<string, unknown>;
    for (const key of ["filename", "abs_path", "module"]) {
      if (typeof fm[key] === "string") fm[key] = redact(fm[key] as string);
    }
  }
}
