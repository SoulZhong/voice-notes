// 提示分级与折叠选择(2026-08-22 提示系统重设计,方案甲):
// 一次只打扰一条——错误各自成行永不折叠;其余按数组序取最高优先级一条上条,
// 剩余进抽屉。"知道了"按 (key, epoch) 记忆:数据变了(epoch 变)提示自动回来。
export type NoticeLevel = "error" | "action" | "suggest" | "info";

export type NoticeAction = {
  label: string;
  run: () => void;
  disabled?: boolean;
};

export type Notice = {
  /** 稳定标识(记忆"知道了"用)。 */
  key: string;
  level: NoticeLevel;
  text: string;
  /** 次行细节(抽屉展开后显示;上条只显示 text 一句话)。 */
  detail?: string;
  /** 数据版本:变了则撤销"知道了"。缺省 "1"(按笔记记住一次即永久)。 */
  epoch?: string;
  actions?: NoticeAction[];
  /** 默认 true;错误默认不可点掉(级别为 error 时忽略此项恒 false)。 */
  dismissible?: boolean;
};

export type NoticeSplit = {
  errors: Notice[];
  head: Notice | null;
  others: Notice[];
};

export const noticeEpoch = (n: Notice) => n.epoch ?? "1";

/** 过滤已"知道了"的,再拆成 错误/上条/抽屉。数组序即优先级。 */
export function splitNotices(notices: Notice[], dismissed: Record<string, string>): NoticeSplit {
  const visible = notices.filter((n) => dismissed[n.key] !== noticeEpoch(n));
  const errors = visible.filter((n) => n.level === "error");
  const rest = visible.filter((n) => n.level !== "error");
  return { errors, head: rest[0] ?? null, others: rest.slice(1) };
}
