import type { Dict, Msg } from "../types";

// record 领域文案分片。键一律以 "record." 前缀命名,分片之间不得重键(有测试哨兵)。
// 覆盖:录制页(/record)、AiStateLabel 无障碍标签。
export const zh = {
  "record.title": "实时转写",

  // 右上状态短标签(状态机原值→友好文案)
  "record.status.error": "出错",
  "record.status.recording": "录制中",
  "record.status.paused": "已暂停",
  "record.status.stopped": "已停止",
  "record.status.ready": "就绪",

  // 控制钮
  "record.btn.stopping": "正在停止…",
  "record.btn.start": "开始录制",
  "record.btn.resume": "恢复",
  "record.btn.pause": "暂停",
  "record.btn.stop": "停止",

  "record.micLevel": "麦克风电平",

  // 云端识别连接状态条
  "record.cloud.reconnecting": "云端识别中断,重连中…{reason}",
  "record.cloud.backfilling": "补识中…",
  "record.cloud.backfillFailed": "部分片段补识失败,原始音频已保留",
  "record.cloud.recovered": "已恢复",

  // 条件横幅
  "record.banner.mcpHint": "新功能：把会议笔记接入 Claude / Cursor 等 AI 助手（MCP）。",
  "record.banner.mcpGo": "去 AI 页",
  "record.banner.mcpDismiss": "知道了",
  "record.banner.btEcho":
    "检测到蓝牙外放 + 「保持外放音量」：蓝牙延迟会让回声消除失效，录音会混入对方声音（回放像回音）。建议改用有线外放/耳机，或到设置关闭「保持外放音量」。",
  "record.banner.lowInput": "麦克风输入音量偏低（{vol}%），可能录得很轻。",
  "record.banner.setVolume": "调到 {target}%",
  "record.banner.screenPerm": "系统声音未授权：只能录到麦克风，对方/外放的声音不会进笔记。",
  "record.banner.authorizeNow": "立即授权",
  "record.banner.screenPermHint": "系统设置里勾选 voice-notes 后切回本页即可。",
  "record.banner.permFix": "系统设置里已勾选却仍提示未授权？多半是旧版本的授权记录残留，开关是失效的。",
  "record.banner.permFixBtn": "修复授权",
  "record.banner.permFixHint":
    "清除残留后重新弹出系统授权；若未弹出，退出并重新打开应用后再点「立即授权」。",
  "record.banner.sysAudioOff": "系统声音不可用（未授权屏幕录制）。仅麦克风在录。",
  "record.banner.openSettings": "打开系统设置",
  "record.banner.sysAudioOffHint": "授权后重新开录生效。",
  "record.banner.diarUnavailable": "说话人区分不可用（相关模型未下载）。转写与录音不受影响。",
  "record.banner.storageDegraded": "落盘异常：内容暂存内存并自动重试，请检查磁盘空间。录制不受影响。",

  // 硬承诺双轨(拒录引导卡):System 起不来时整场拆除,开录失败错误串带 system_denied /
  // system_unavailable 分类,前端据此渲染引导卡(权限缺失=可操作的授权引导;设备/组件
  // 不可用=纯提示,无跳转)。
  "record.systemDenied.title": "系统声音未获授权",
  "record.systemDenied.desc":
    "会议笔记需要录制系统声音；请在 系统设置→隐私与安全性→屏幕录制 中允许本应用，然后重试。",
  "record.systemDenied.openSettings": "打开系统设置",
  "record.systemUnavailable.desc": "系统声音当前不可用（设备或组件问题），本场无法开录；请检查后重试。",

  // 转写区
  "record.badge.me": "我",
  "record.badge.them": "对方",
  "record.emptyHint": "（开始说话…）",
  "record.jumpLatest": "↓ 回到最新",

  // AiStateLabel 无障碍标签
  "record.ai.idle": "AI",
  "record.ai.running": "Aing，正在执行",
  "record.ai.complete": "AI，已完成",
  "record.ai.failed": "AI，执行失败",
} as const satisfies Dict;

export const en = {
  "record.title": "Live Transcription",

  "record.status.error": "Error",
  "record.status.recording": "Recording",
  "record.status.paused": "Paused",
  "record.status.stopped": "Stopped",
  "record.status.ready": "Ready",

  "record.btn.stopping": "Stopping…",
  "record.btn.start": "Start Recording",
  "record.btn.resume": "Resume",
  "record.btn.pause": "Pause",
  "record.btn.stop": "Stop",

  "record.micLevel": "Microphone level",

  "record.cloud.reconnecting": "Cloud transcription interrupted, reconnecting…{reason}",
  "record.cloud.backfilling": "Backfilling…",
  "record.cloud.backfillFailed": "Some segments could not be backfilled; the original audio is kept",
  "record.cloud.recovered": "Reconnected",

  "record.banner.mcpHint": "New: connect your meeting notes to AI assistants like Claude / Cursor (MCP).",
  "record.banner.mcpGo": "Open AI page",
  "record.banner.mcpDismiss": "Got it",
  "record.banner.btEcho":
    "Bluetooth output with \"Keep output volume\" detected: Bluetooth latency defeats echo cancellation, so the other side's audio will bleed into the recording (playback sounds like an echo). Use wired speakers/headphones, or turn off \"Keep output volume\" in Settings.",
  "record.banner.lowInput": "Microphone input volume is low ({vol}%); the recording may be very quiet.",
  "record.banner.setVolume": "Set to {target}%",
  "record.banner.screenPerm":
    "System audio not authorized: only your microphone will be recorded; the other side's / speaker audio won't make it into notes.",
  "record.banner.authorizeNow": "Authorize now",
  "record.banner.screenPermHint": "Check voice-notes in System Settings, then switch back to this page.",
  "record.banner.permFix":
    "Checked in System Settings but still shown as unauthorized? A stale permission record from an old build is likely blocking it — the toggle no longer works.",
  "record.banner.permFixBtn": "Repair permission",
  "record.banner.permFixHint":
    "Clearing the stale record re-triggers the system prompt; if it doesn't appear, quit and reopen the app, then click \"Authorize now\" again.",
  "record.banner.sysAudioOff": "System audio unavailable (screen recording not authorized). Recording microphone only.",
  "record.banner.openSettings": "Open System Settings",
  "record.banner.sysAudioOffHint": "Takes effect after you start a new recording once authorized.",
  "record.banner.diarUnavailable":
    "Speaker diarization unavailable (model not downloaded). Transcription and recording are unaffected.",
  "record.banner.storageDegraded":
    "Disk write issue: content is buffered in memory and retried automatically; please check disk space. Recording is unaffected.",

  "record.systemDenied.title": "System audio not authorized",
  "record.systemDenied.desc":
    "Meeting notes needs to record system audio; please allow this app under System Settings → Privacy & Security → Screen Recording, then try again.",
  "record.systemDenied.openSettings": "Open System Settings",
  "record.systemUnavailable.desc":
    "System audio is currently unavailable (a device or component issue); recording could not start. Please check and try again.",

  "record.badge.me": "Me",
  "record.badge.them": "Them",
  "record.emptyHint": "(Start speaking…)",
  "record.jumpLatest": "↓ Back to latest",

  "record.ai.idle": "AI",
  "record.ai.running": "Aing, running",
  "record.ai.complete": "AI, completed",
  "record.ai.failed": "AI, failed",
} satisfies Record<keyof typeof zh, Msg>;
