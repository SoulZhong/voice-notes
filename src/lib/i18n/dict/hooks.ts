import type { Dict, Msg } from "../types";

// hooks 领域文案分片。键一律以 "hooks." 前缀命名,分片之间不得重键(有测试哨兵)。
export const zh = {
  // 事件标签(hooks.svelte.ts HOOK_EVENTS,下拉与侧栏分组共用)
  "hooks.event.recordingStarted": "录制开始",
  "hooks.event.recordingStopped": "录制停止",
  "hooks.event.recordingPaused": "录制暂停",
  "hooks.event.recordingResumed": "录制恢复",
  "hooks.event.refineStarted": "Aing 开始",
  "hooks.event.refineFinished": "Aing 结束",

  // 概览页(/hooks)
  "hooks.title": "钩子",
  "hooks.intro":
    "笔记状态变化时自动执行你的命令或调用接口：停止录制后归档、Aing 结束后发通知——左侧新建一条钩子,选事件、填命令,配置完可以立即测试。",
  "hooks.events.title": "可用事件",
  "hooks.flow.aria":
    "笔记生命周期状态图：录制中停止后进入录制完成，执行 Aing 后按结果进入 AI 已完成或 AI 失败状态",
  "hooks.flow.recording": "录制中",
  "hooks.flow.recorded": "录制完成",
  "hooks.flow.paused": "已暂停",
  "hooks.flow.success": "成功",
  "hooks.flow.failure": "失败",
  "hooks.flow.caption":
    "方框表示状态，连线表示动作。录制完成后执行 Aing；成功后标记为「AI ✓」，失败后标记为「AI ×」并可重新执行。钩子可在 Aing 开始（refine_started）或结束（refine_finished）时触发。",
  "hooks.env.title": "Shell 命令收到的环境变量",
  "hooks.env.event": "事件名,如 recording_stopped",
  "hooks.env.noteId": "笔记 id",
  "hooks.env.noteTitle": "笔记标题(取不到时为空)",
  "hooks.env.noteText": "笔记全文 markdown,修订稿优先——仅钩子勾选「附带笔记内容」时注入,下同",
  "hooks.env.noteTimes": "开始/结束时间(RFC3339),未结束时结束为空",
  "hooks.env.duration": "时长秒数",
  "hooks.env.speakers": "说话人名单,顿号分隔",
  "hooks.env.truncated": "全文超 200KB 被截断时为 1,未截断不注入",
  "hooks.webhook.title": "Webhook 收到的 JSON",
  "hooks.webhook.sample": `POST <你的 URL>
content-type: application/json

{
  "event": "recording_stopped",
  "note_id": "…",
  "note_title": "…",
  "occurred_at": "2026-07-14T10:00:00+08:00",
  "note": {
    "started_at": "…", "ended_at": "…", "duration_secs": 3600,
    "speakers": ["张三"], "text": "…markdown…", "text_truncated": false
  }
}`,
  "hooks.webhook.hint":
    "note 字段仅在钩子勾选「附带笔记内容」时出现；停止录制时通常是原始稿，想要 AI 整理后的全文请挂「Aing 结束」事件。",

  // 编辑页(/hooks/[id])
  "hooks.edit.newTitle": "新建钩子",
  "hooks.edit.editTitle": "编辑钩子",
  "hooks.edit.notFound": "钩子不存在,可能已被删除",
  "hooks.unnamed": "未命名钩子",
  "hooks.delete.title": "删除钩子",
  "hooks.delete.confirm": "「{name}」将被删除,此操作不可恢复。",
  "hooks.action.cancel": "取消",
  "hooks.action.delete": "删除",
  "hooks.action.save": "保存",
  "hooks.action.saving": "保存中…",
  "hooks.action.test": "测试一次",
  "hooks.action.testing": "测试中…",
  "hooks.field.name.label": "名称",
  "hooks.field.name.desc": "给这条钩子起个好认的名字",
  "hooks.field.name.placeholder": "如:停录后归档",
  "hooks.field.event.label": "触发事件",
  "hooks.field.event.desc": "事件发生的那一刻执行",
  "hooks.field.kind.label": "执行方式",
  "hooks.kind.shellDesc": "本机运行命令，事件信息在环境变量里",
  "hooks.kind.feishu": "飞书",
  "hooks.kind.wecom": "企业微信",
  "hooks.kind.generic": "通用 Webhook",
  "hooks.kind.feishuDesc": "发送成飞书群机器人可以直接显示的文字消息",
  "hooks.kind.wecomDesc": "发送成企业微信群机器人可以直接显示的文字消息",
  "hooks.kind.genericDesc": "向自建服务或自动化工具 POST 标准 JSON",
  "hooks.kind.feishuHint":
    "打开飞书群设置 → 群机器人 → 添加机器人 → 自定义机器人，复制 Webhook 地址粘贴到这里。",
  "hooks.kind.wecomHint": "打开企业微信群设置 → 群机器人 → 添加机器人，复制 Webhook 地址粘贴到这里。",
  "hooks.kind.genericHint": "适合自建服务、n8n、Zapier 等工具；请求体字段可在钩子概览页查看。",
  "hooks.field.command.label": "命令",
  "hooks.field.command.desc":
    "经 /bin/sh -c 执行;可用 $VN_EVENT、$VN_NOTE_ID、$VN_NOTE_TITLE,30 秒超时",
  "hooks.field.command.placeholder": 'say "会议结束"',
  "hooks.field.url.label": "Webhook 地址",
  "hooks.field.style.label": "消息样式",
  "hooks.style.feishuDesc": "富文本会以飞书消息标题与分段正文展示",
  "hooks.style.wecomDesc": "Markdown 支持标题、加粗、引用和链接",
  "hooks.style.rich": "富文本",
  "hooks.style.plain": "纯文本",
  "hooks.field.secret.label": "签名密钥（推荐）",
  "hooks.field.secret.desc": "若机器人安全设置启用了“签名校验”，粘贴页面显示的密钥；只保存在本机",
  "hooks.field.secret.placeholder": "未启用签名校验可留空",
  "hooks.field.mentionAll.label": "提醒所有人",
  "hooks.field.mentionAll.desc": "每次触发都会 @ 全体成员，请谨慎开启",
  "hooks.tip.feishu":
    "飞书还可在机器人后台设置关键词或 IP 白名单；关键词必须出现在消息中，建议填“voice-notes”。",
  "hooks.tip.wecom": "企业微信单个机器人最多发送 20 条/分钟；高频事件请避免同时开启 @ 所有人。",
  "hooks.tip.generic": "通用 Webhook 会发送标准 JSON，适合自建服务、n8n、Zapier 等自动化工具。",
  "hooks.field.includeNote.label": "附带笔记内容",
  "hooks.includeNote.imDesc":
    "开启后，群消息会带上笔记全文（修订稿优先）；想接收 AI 整理结果请选择「Aing 结束」",
  "hooks.includeNote.otherDesc":
    "把笔记详情与全文交给命令/接口，修订稿优先；想要 AI 整理后的全文请挂「Aing 结束」，变量清单见概览页",
  "hooks.field.enabled.label": "启用",
  "hooks.field.enabled.desc": "停用后保留配置,事件不再触发",
  "hooks.test.ok": "测试成功({msg})",
  "hooks.test.err": "测试失败: {msg}",
} as const satisfies Dict;

export const en = {
  "hooks.event.recordingStarted": "Recording started",
  "hooks.event.recordingStopped": "Recording stopped",
  "hooks.event.recordingPaused": "Recording paused",
  "hooks.event.recordingResumed": "Recording resumed",
  "hooks.event.refineStarted": "Aing started",
  "hooks.event.refineFinished": "Aing finished",

  "hooks.title": "Hooks",
  "hooks.intro":
    "Run your own command or call an API when a note changes state: archive after recording stops, get notified when Aing finishes — create a hook on the left, pick an event, fill in the command, and test it right away.",
  "hooks.events.title": "Available events",
  "hooks.flow.aria":
    "Note lifecycle state diagram: recording moves to recorded when stopped; after Aing runs, the note ends up in AI done or AI failed depending on the result",
  "hooks.flow.recording": "Recording",
  "hooks.flow.recorded": "Recorded",
  "hooks.flow.paused": "Paused",
  "hooks.flow.success": "Success",
  "hooks.flow.failure": "Failure",
  "hooks.flow.caption":
    "Boxes are states, lines are actions. After recording completes, Aing runs; success marks the note “AI ✓”, failure marks it “AI ×” and it can be re-run. Hooks can fire when Aing starts (refine_started) or finishes (refine_finished).",
  "hooks.env.title": "Environment variables passed to shell commands",
  "hooks.env.event": "Event name, e.g. recording_stopped",
  "hooks.env.noteId": "Note id",
  "hooks.env.noteTitle": "Note title (empty if unavailable)",
  "hooks.env.noteText":
    "Full note markdown, refined version preferred — only injected when the hook has “Include note content” enabled; same below",
  "hooks.env.noteTimes": "Start/end time (RFC3339); end is empty until the note is finished",
  "hooks.env.duration": "Duration in seconds",
  "hooks.env.speakers": "Speaker names, joined with 、",
  "hooks.env.truncated": "Set to 1 when the full text exceeds 200KB and was truncated; not injected otherwise",
  "hooks.webhook.title": "JSON received by webhooks",
  "hooks.webhook.sample": `POST <your URL>
content-type: application/json

{
  "event": "recording_stopped",
  "note_id": "…",
  "note_title": "…",
  "occurred_at": "2026-07-14T10:00:00+08:00",
  "note": {
    "started_at": "…", "ended_at": "…", "duration_secs": 3600,
    "speakers": ["Alice"], "text": "…markdown…", "text_truncated": false
  }
}`,
  "hooks.webhook.hint":
    "The note field only appears when the hook has “Include note content” enabled; at recording stop it is usually the raw transcript — to get the AI-refined text, hook the “Aing finished” event.",

  "hooks.edit.newTitle": "New hook",
  "hooks.edit.editTitle": "Edit hook",
  "hooks.edit.notFound": "Hook not found; it may have been deleted",
  "hooks.unnamed": "Untitled hook",
  "hooks.delete.title": "Delete hook",
  "hooks.delete.confirm": "“{name}” will be deleted. This cannot be undone.",
  "hooks.action.cancel": "Cancel",
  "hooks.action.delete": "Delete",
  "hooks.action.save": "Save",
  "hooks.action.saving": "Saving…",
  "hooks.action.test": "Run test",
  "hooks.action.testing": "Testing…",
  "hooks.field.name.label": "Name",
  "hooks.field.name.desc": "Give this hook a recognizable name",
  "hooks.field.name.placeholder": "e.g. Archive after recording",
  "hooks.field.event.label": "Trigger event",
  "hooks.field.event.desc": "Runs the moment the event occurs",
  "hooks.field.kind.label": "Action type",
  "hooks.kind.shellDesc": "Runs a command locally; event info is passed via environment variables",
  "hooks.kind.feishu": "Feishu",
  "hooks.kind.wecom": "WeCom",
  "hooks.kind.generic": "Generic webhook",
  "hooks.kind.feishuDesc": "Sends a message a Feishu group bot can display directly",
  "hooks.kind.wecomDesc": "Sends a message a WeCom group bot can display directly",
  "hooks.kind.genericDesc": "POSTs standard JSON to your own service or automation tool",
  "hooks.kind.feishuHint":
    "Open Feishu group settings → Group bots → Add bot → Custom bot, then copy the webhook URL here.",
  "hooks.kind.wecomHint": "Open WeCom group settings → Group bots → Add bot, then copy the webhook URL here.",
  "hooks.kind.genericHint":
    "Works with self-hosted services, n8n, Zapier and similar tools; see the hooks overview page for the request body fields.",
  "hooks.field.command.label": "Command",
  "hooks.field.command.desc":
    "Runs via /bin/sh -c; $VN_EVENT, $VN_NOTE_ID and $VN_NOTE_TITLE are available; 30s timeout",
  "hooks.field.command.placeholder": 'say "Meeting ended"',
  "hooks.field.url.label": "Webhook URL",
  "hooks.field.style.label": "Message style",
  "hooks.style.feishuDesc": "Rich text renders as a Feishu message with a title and sectioned body",
  "hooks.style.wecomDesc": "Markdown supports headings, bold, quotes and links",
  "hooks.style.rich": "Rich text",
  "hooks.style.plain": "Plain text",
  "hooks.field.secret.label": "Signing secret (recommended)",
  "hooks.field.secret.desc":
    "If the bot’s security settings enable “signature verification”, paste the secret shown there; stored on this device only",
  "hooks.field.secret.placeholder": "Leave empty if signature verification is off",
  "hooks.field.mentionAll.label": "Mention everyone",
  "hooks.field.mentionAll.desc": "Every trigger will @ all members; enable with care",
  "hooks.tip.feishu":
    "Feishu also lets you set keywords or an IP allowlist in the bot settings; the keyword must appear in the message — “voice-notes” is a good choice.",
  "hooks.tip.wecom":
    "A single WeCom bot can send at most 20 messages per minute; avoid combining high-frequency events with @ everyone.",
  "hooks.tip.generic":
    "Generic webhooks send standard JSON, suitable for self-hosted services and automation tools like n8n and Zapier.",
  "hooks.field.includeNote.label": "Include note content",
  "hooks.includeNote.imDesc":
    "When enabled, group messages include the full note text (refined version preferred); to receive AI-refined results, choose “Aing finished”",
  "hooks.includeNote.otherDesc":
    "Passes note details and full text to the command/endpoint, refined version preferred; to get the AI-refined text, hook “Aing finished” — see the overview page for the variable list",
  "hooks.field.enabled.label": "Enabled",
  "hooks.field.enabled.desc": "When disabled, the config is kept but events no longer trigger it",
  "hooks.test.ok": "Test passed ({msg})",
  "hooks.test.err": "Test failed: {msg}",
} satisfies Record<keyof typeof zh, Msg>;
