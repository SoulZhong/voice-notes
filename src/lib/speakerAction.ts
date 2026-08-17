/** 说话人条(SpeakerChips)操作的失败呈现。
 *
 * 这些操作(删除/改名/选人/标记为我)全部经 Tauri invoke 打到后端命令壳,后端以
 * `Result<(), String>` 回执,守卫拒绝时 Err 里是**已按界面语言本地化**的文案
 * (tr! 宏,如「该笔记正在 Aing 中，稍后再删」)。所以这里只做透出与兜底,不再翻译。
 *
 * 存在的理由:2026-08-17 的「删除无效」——后端确实返回了带文案的 Err,但组件
 * 直接 `await onDelete(id)` 没有 catch,错误被吞掉,用户只看到面板关闭、零反馈,
 * 排查时也无从下手(错误不进 stderr.log,只走 IPC 回执)。任何失败都必须有非空文案。 */
export function describeActionError(e: unknown, fallback: string): string {
  const raw =
    typeof e === "string"
      ? e
      : e instanceof Error
        ? e.message
        : e == null
          ? ""
          : safeStringify(e);
  const text = raw.trim();
  // 空文案等于没提示,退化回静默失败——宁可给一句笼统的也不能给空串。
  // fallback 必传:本模块刻意不含任何界面文案(i18n 护栏禁裸中文,纯函数也更好测),
  // 由调用方传 t("speakers.actionFailed")。
  return text || fallback;
}

/** 非字符串失败体的兜底:优先 JSON,不可序列化(循环引用等)才退回 String()。
 *  刻意避开裸 String(obj) 产出的 "[object Object]"——那对排查毫无帮助。 */
function safeStringify(e: unknown): string {
  try {
    const s = JSON.stringify(e);
    if (s && s !== "{}") return s;
  } catch {
    // 循环引用/BigInt 等:落到下面的 String()
  }
  const s = String(e);
  return s === "[object Object]" ? "" : s;
}
