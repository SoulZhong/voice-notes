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

/** 路径起点:一个 `/`,前面是行首或分隔符,且这条路径至少两段。
 *
 *  **前一个字符不能是 `:` 或 `/`**——否则 `tauri://localhost/notes/note-1` 这类 URL
 *  会被整条吃掉。URL 里没有用户内容(路由动态段一律是 id),却是定位前端异常现场的
 *  依据,不该误伤。与 Rust 侧 `find_path_start` 同判据。 */
const PATH_BOUNDARY = new Set([" ", "\t", '"', "'", "(", "[", ",", ";", "=", "\n", "\r"]);

function findPathStart(s: string): number {
  for (let i = 0; i < s.length; i++) {
    if (s[i] !== "/") continue;
    if (i > 0 && !PATH_BOUNDARY.has(s[i - 1])) continue;
    const end = pathEnd(s.slice(i));
    const seg = s.slice(i, i + end);
    if ((seg.match(/\//g) ?? []).length >= 2) return i;
  }
  return -1;
}

/** unix 绝对路径收敛。家目录一档 `<HOME_PATH>`、其余一档 `<PATH>`。
 *
 *  家目录之外的也要收:数据目录与模型目录可被用户指到任何地方(`/Volumes/客户名/…`、
 *  网络盘),那些路径既不在家目录下、中文串又常短于整段丢弃的阈值,不收就原样出站。 */
function redactUnixPaths(input: string): string {
  let out = "";
  let rest = input;
  for (;;) {
    const pos = findPathStart(rest);
    if (pos < 0) break;
    const tail = rest.slice(pos);
    const end = pathEnd(tail);
    const seg = tail.slice(0, end);
    out += rest.slice(0, pos);
    out += seg.startsWith("/Users/") || seg.startsWith("/home/") ? "<HOME_PATH>" : "<PATH>";
    rest = tail.slice(end);
  }
  return out + rest;
}

/** Windows 绝对路径:`C:\Users\Alice\…` 与自定义目录可能落在的 `D:\客户\…`。
 *  终点判据与 unix 分支共用 pathEnd,与 Rust 侧 redact_windows_paths 同判据。 */
function redactWindowsPaths(input: string): string {
  let out = "";
  let rest = input;
  for (;;) {
    const m = /[A-Za-z]:\\/.exec(rest);
    if (!m) break;
    const pos = m.index;
    const tail = rest.slice(pos);
    const end = pathEnd(tail);
    const seg = tail.slice(0, end);
    out += rest.slice(0, pos);
    out += seg.slice(2).toLowerCase().startsWith("\\users\\") ? "<HOME_PATH>" : "<PATH>";
    rest = tail.slice(end);
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
  return dropLongCjkRuns(redactKeys(redactUnixPaths(redactWindowsPaths(input))));
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
