import type { Msg } from "../types";

// 会议备注(memo)文案分片。键一律以 "memo." 前缀命名。
// 覆盖:备注栏(录制页/笔记页共用的 Markdown 文档)、备注浮窗、设置页备注快捷键、侧栏备注搜索。
// 「备注」不叫「笔记」(笔记指整场会议);没有文字的一行叫「时间标记」。
export const zh = {
  "memo.column.title": "备注",
  "memo.column.open": "展开备注栏",
  "memo.column.close": "收起备注栏",
  "memo.failed": "备注保存失败:{e}",
  "memo.shortcutBusy": "{key} 用不了({reason}),可在设置里换一个",
  "memo.panel.pinned": "已钉住 {t}",
  "memo.panel.placeholder": "补一句(回车保存,Esc 关闭)",
  "memo.panel.hint": "不写也行:时间标记已经记下了",
  "memo.note.add": "加备注",
  "memo.settings.label": "会议备注快捷键",
  "memo.settings.desc": "录音中按下即钉住当前时间并弹出小输入框;停录后释放给其它应用。",
  "memo.settings.sameAsRecord": "不能与录制快捷键相同",
  "memo.search.label": "备注",
  "memo.search.empty": "没有备注含「{q}」",
  "memo.doc.placeholder": "记点什么…支持 Markdown:# 标题、- 列表、- [ ] 待办、**加粗**",
  "memo.doc.jump": "跳到原文这个时间点",
  "memo.doc.stamps": "{n} 处时间点",
  "memo.doc.tooLong": "备注太长(上限 {n} 字),没有保存",
} satisfies Record<string, Msg>;

export const en = {
  "memo.column.title": "Memos",
  "memo.column.open": "Show memos",
  "memo.column.close": "Hide memos",
  "memo.failed": "Couldn't save memo: {e}",
  "memo.shortcutBusy": "{key} is unavailable ({reason}); pick another in Settings",
  "memo.panel.pinned": "Pinned at {t}",
  "memo.panel.placeholder": "Add a line (Enter to save, Esc to close)",
  "memo.panel.hint": "No text needed: the time marker is already saved",
  "memo.note.add": "Add memo",
  "memo.settings.label": "Meeting memo shortcut",
  "memo.settings.desc": "While recording, pins the current time and opens a small input box; released to other apps once recording stops.",
  "memo.settings.sameAsRecord": "Must differ from the recording shortcut",
  "memo.search.label": "Memos",
  "memo.search.empty": "No memos contain “{q}”",
  "memo.doc.placeholder": "Write anything… Markdown works: # heading, - list, - [ ] to-do, **bold**",
  "memo.doc.jump": "Jump to this moment in the transcript",
  "memo.doc.stamps": (p) => (Number(p.n) === 1 ? "1 time point" : `${p.n} time points`),
  "memo.doc.tooLong": "Memos too long (max {n} characters); not saved",
} satisfies Record<keyof typeof zh, Msg>;
