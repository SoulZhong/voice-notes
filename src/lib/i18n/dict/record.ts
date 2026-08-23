import type { Dict, Msg } from "../types";

// record 领域文案分片。键一律以 "record." 前缀命名,分片之间不得重键(有测试哨兵)。
// 覆盖:录制页(/record)、AiStateLabel 无障碍标签。
export const zh = {
  "record.title": "实时转写",

  // 计时后缀:只在非正常态出条(正常录制由呼吸红点交代)。error/recording/stopped/ready 随
  // 旧的右上状态标签一起删除——出错有下方专门的红色详情行,不再走这里。
  "record.status.paused": "已暂停",

  // 控制钮
  "record.btn.stopping": "正在停止…",
  "record.btn.start": "开始录制",
  "record.btn.resume": "恢复",
  "record.btn.pause": "暂停",
  "record.btn.stop": "停止",

  "record.micLevel": "麦克风电平",
  "record.systemLevel": "对方声音电平",

  // 云端识别连接状态条
  "record.cloud.reconnecting": "云端识别中断,重连中…{reason}",
  "record.cloud.backfilling": "补识中…",
  "record.cloud.backfillFailed": "部分片段补识失败,原始音频已保留",
  "record.cloud.recovered": "已恢复",

  // 条件横幅
  "record.banner.mcpHint": "新功能：把会议笔记接入 Claude / Cursor 等 AI 助手（MCP）。",
  "record.banner.mcpGo": "去 AI 页",
  "record.banner.mcpDismiss": "知道了",
  "record.banner.micIsolation":
    "系统「语音突显」已开启：它会把它认为不是人声的部分削成绝对静音，实测会连人声一起削掉(有场录音因此丢了近两成语音)。",
  "record.banner.micIsolationHow": "改法：录制时点菜单栏控制中心 → 麦克风模式 → 标准。",
  "record.banner.btEcho": "蓝牙耳机延迟可能影响回声消除效果，建议改用有线耳机或内置扬声器。",
  "record.banner.gapStorm":
    "麦克风正在频繁断流：近一分钟有 {pct}% 的时长没有收到音频，这段内容是真的丢了(时间轴已按静音补齐)。",
  "record.banner.gapStormHow": "多见于蓝牙耳机麦克风与会议软件抢带宽。改法：麦克风换成内置麦或有线设备，蓝牙耳机只用来听。",
  "record.banner.gapStormSystem": "系统声音采集正在频繁断流：近一分钟有 {pct}% 的时长没有收到音频。对方说的话可能有缺失。",
  "record.risk.title": "这样录会丢内容",
  "record.inputAutoPicked": "蓝牙耳机的麦克风容易断流,本场已自动改用「{device}」录音;耳机继续用来听。想关掉自动切换,去设置。",
  "record.scene.speakerEcho": "像是外放开会:对方的声音会从扬声器进到麦克风,已在消回声;戴耳机效果最好。",
  "record.scene.dualPath": "像是现场+线上同源场景:对方的话可能以「我」的名义出现在逐字稿里。戴耳机可根治;后续版本将自动处置。",
  "record.risk.body": "检测到下面的问题。现在改还来得及；已经开会了也可以先录，损失如实记在笔记的对账里。",
  "record.risk.voice_isolation.title": "系统「语音突显」开着",
  "record.risk.voice_isolation.impact": "它会把它认为不是人声的部分削成绝对静音，判错时连人声一起削。实测有场录音因此丢了近两成语音。",
  "record.risk.voice_isolation.how": "改法：菜单栏控制中心 → 麦克风模式 → 标准。",
  "record.risk.bluetooth_mic.title": "正在用蓝牙耳机的麦克风",
  "record.risk.bluetooth_mic.impact": "蓝牙麦克风走通话模式，上行要和会议软件抢带宽，会频繁断流。实测一场 23 分钟的会议因此丢了 14% 的时长，且多数断在话说到一半时。",
  "record.risk.bluetooth_mic.how": "改法：把输入设备换成内置麦克风或有线设备，蓝牙耳机只用来听。",
  "record.risk.proceed": "仍然录制",
  "record.risk.cancel": "去改设置",
  "record.banner.lowInput": "麦克风输入音量偏低（{vol}%），可能录得很轻。",
  "record.banner.setVolume": "调到 {target}%",
  "record.banner.screenPerm": "无该权限无法开始录制（会议笔记需同时录制系统声音），请授权后重试。",
  "record.banner.authorizeNow": "立即授权",
  "record.banner.screenPermHint": "系统设置里勾选 voice-notes 后切回本页即可。",
  "record.banner.permFix": "系统设置里已勾选却仍提示未授权？多半是旧版本的授权记录残留，开关是失效的。",
  "record.banner.permFixBtn": "修复授权",
  "record.banner.permFixHint":
    "会同时清除屏幕录制和系统声音的旧授权；重新授权后请退出并重新打开应用。",
  "record.banner.openSettings": "打开系统设置",
  "record.banner.diarUnavailable": "说话人区分不可用（相关模型未下载）。转写与录音不受影响。",
  "record.banner.storageDegraded": "落盘异常：内容暂存内存并自动重试，请检查磁盘空间。录制不受影响。",

  // 硬承诺双轨(拒录引导卡):System 起不来时整场拆除,开录失败错误串带 system_denied /
  // system_unavailable 分类,前端据此渲染引导卡(权限缺失=可操作的授权引导,复用
  // record.banner.openSettings 按钮文案;设备/组件不可用=纯提示,无跳转)。
  "record.systemDenied.title": "系统声音未获授权",
  "record.systemDenied.desc":
    "会议笔记需要录制系统声音；请在 系统设置→隐私与安全性→录屏与系统录音 中允许本应用，然后重新打开应用。",
  "record.systemUnavailable.desc": "系统声音当前不可用（设备或组件问题），本场无法开录；请检查后重试。",

  // 转写区
  "record.badge.me": "我",
  "record.badge.them": "对方",
  "record.emptyHint": "（开始说话…）",
  "record.jumpLatest": "↓ 回到最新",

  // 回看工具条:页内搜索(高亮+跳转,不隐藏) + 说话人过滤(隐藏行)
  "record.search.placeholder": "搜索转写内容",
  "record.search.none": "无命中",
  "record.search.prev": "上一个",
  "record.search.next": "下一个",
  "record.search.clear": "清除",
  "record.filter.only": "只看",

  // 当场纠正(行内编辑文本 / 改派说话人 / 命名改名):后端为唯一真值源,前端不做
  // 乐观更新,失败原样展示、不自动重试。
  "record.edit.text": "编辑这一句",
  "record.edit.speaker": "改说话人",
  "record.edit.rename": "命名/改名…",
  "record.edit.failed": "编辑失败:{e}",
  "record.edit.dismiss": "关闭",

  // AiStateLabel 无障碍标签
  "record.ai.idle": "AI",
  "record.ai.running": "Aing，正在执行",
  "record.ai.complete": "AI，已完成",
  "record.ai.failed": "AI，执行失败",
} as const satisfies Dict;

export const en = {
  "record.title": "Live Transcription",

  "record.status.paused": "Paused",

  "record.btn.stopping": "Stopping…",
  "record.btn.start": "Start Recording",
  "record.btn.resume": "Resume",
  "record.btn.pause": "Pause",
  "record.btn.stop": "Stop",

  "record.micLevel": "Microphone level",
  "record.systemLevel": "Their audio level",

  "record.cloud.reconnecting": "Cloud transcription interrupted, reconnecting…{reason}",
  "record.cloud.backfilling": "Backfilling…",
  "record.cloud.backfillFailed": "Some segments could not be backfilled; the original audio is kept",
  "record.cloud.recovered": "Reconnected",

  "record.banner.mcpHint": "New: connect your meeting notes to AI assistants like Claude / Cursor (MCP).",
  "record.banner.mcpGo": "Open AI page",
  "record.banner.mcpDismiss": "Got it",
  "record.banner.micIsolation":
    "macOS Voice Isolation is on: it mutes whatever it decides is not speech to digital silence, and it cuts real speech too (one recording here lost nearly 20% of its speech that way).",
  "record.banner.micIsolationHow": "Fix: while recording, open Control Center in the menu bar \u2192 Mic Mode \u2192 Standard.",
  "record.banner.btEcho":
    "Bluetooth headset latency may affect echo cancellation; consider using wired headphones or the built-in speaker instead.",
  "record.banner.gapStorm":
    "The microphone keeps dropping out: {pct}% of the last minute arrived with no audio at all. That content is genuinely lost (the timeline was padded with silence).",
  "record.banner.gapStormHow":
    "Usually a Bluetooth headset mic competing with the meeting app for bandwidth. Fix: switch the microphone to the built-in or a wired one, and use the headset for playback only.",
  "record.banner.gapStormSystem":
    "System audio capture keeps dropping out: {pct}% of the last minute arrived with no audio. Parts of what the other side said may be missing.",
  "record.risk.title": "This recording will lose content",
  "record.inputAutoPicked": "Bluetooth headset mics drop audio; this session automatically records with \u201c{device}\u201d instead. Your headset still plays audio. Disable auto-switch in Settings.",
  "record.scene.speakerEcho": "Sounds like speaker playback: the other side leaks into your mic. Echo cancellation is on; headphones work best.",
  "record.scene.dualPath": "Sounds like on-site + online dual-path: the other side may appear as \u201cme\u201d in the transcript. Headphones fix this; a future version will handle it automatically.",
  "record.risk.body": "We found the problems below. There is still time to fix them; if your meeting has already started you can record anyway, and the loss will be recorded in the note's audio reconciliation.",
  "record.risk.voice_isolation.title": "macOS Voice Isolation is on",
  "record.risk.voice_isolation.impact": "It mutes whatever it decides is not speech to digital silence, and it cuts real speech too. One recording here lost nearly 20% of its speech that way.",
  "record.risk.voice_isolation.how": "Fix: Control Center in the menu bar \u2192 Mic Mode \u2192 Standard.",
  "record.risk.bluetooth_mic.title": "Recording through a Bluetooth headset mic",
  "record.risk.bluetooth_mic.impact": "A Bluetooth mic runs in call mode and competes with the meeting app for uplink bandwidth, so it drops out repeatedly. One 23-minute meeting here lost 14% of its duration this way, mostly cutting mid-sentence.",
  "record.risk.bluetooth_mic.how": "Fix: switch the input device to the built-in or a wired mic, and use the headset for playback only.",
  "record.risk.proceed": "Record anyway",
  "record.risk.cancel": "Change settings",
  "record.banner.lowInput": "Microphone input volume is low ({vol}%); the recording may be very quiet.",
  "record.banner.setVolume": "Set to {target}%",
  "record.banner.screenPerm":
    "Recording can't start without this permission (meeting notes require system audio too); please authorize and try again.",
  "record.banner.authorizeNow": "Authorize now",
  "record.banner.screenPermHint": "Check voice-notes in System Settings, then switch back to this page.",
  "record.banner.permFix":
    "Checked in System Settings but still shown as unauthorized? A stale permission record from an old build is likely blocking it — the toggle no longer works.",
  "record.banner.permFixBtn": "Repair permission",
  "record.banner.permFixHint":
    "This clears stale screen-recording and system-audio permissions together; authorize again, then quit and reopen the app.",
  "record.banner.openSettings": "Open System Settings",
  "record.banner.diarUnavailable":
    "Speaker diarization unavailable (model not downloaded). Transcription and recording are unaffected.",
  "record.banner.storageDegraded":
    "Disk write issue: content is buffered in memory and retried automatically; please check disk space. Recording is unaffected.",

  "record.systemDenied.title": "System audio not authorized",
  "record.systemDenied.desc":
    "Meeting notes needs system audio; allow this app under System Settings → Privacy & Security → Screen & System Audio Recording, then reopen the app.",
  "record.systemUnavailable.desc":
    "System audio is currently unavailable (a device or component issue); recording could not start. Please check and try again.",

  "record.badge.me": "Me",
  "record.badge.them": "Them",
  "record.emptyHint": "(Start speaking…)",
  "record.jumpLatest": "↓ Back to latest",

  "record.search.placeholder": "Search transcript",
  "record.search.none": "No matches",
  "record.search.prev": "Previous",
  "record.search.next": "Next",
  "record.search.clear": "Clear",
  "record.filter.only": "Only",

  "record.edit.text": "Edit this line",
  "record.edit.speaker": "Change speaker",
  "record.edit.rename": "Name/rename…",
  "record.edit.failed": "Edit failed: {e}",
  "record.edit.dismiss": "Dismiss",

  "record.ai.idle": "AI",
  "record.ai.running": "Aing, running",
  "record.ai.complete": "AI, completed",
  "record.ai.failed": "AI, failed",
} satisfies Record<keyof typeof zh, Msg>;
