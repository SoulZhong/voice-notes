import type { Dict, Msg } from "../types";

// notes 领域文案分片。键一律以 "notes." 前缀命名,分片之间不得重键(有测试哨兵)。
// 覆盖:笔记详情页(+page.svelte)、MarkdownEditor/NodeView(refinedSchema/
// segmentSchema)、AudioPlayer 与 notes.ts 的兜底显示名/时长格式化。
export const zh = {
  // notes.ts:兜底显示名与时长
  "notes.untitled": "未命名",
  "notes.speaker.me": "我",
  "notes.speaker.other": "对方",
  "notes.speaker.n": "说话人 {n}",
  "notes.speaker.newN": "新说话人 {n}",
  "notes.duration.hm": "{h} 小时 {m} 分",
  "notes.duration.ms": "{m} 分 {s} 秒",
  "notes.duration.s": "{s} 秒",

  // 详情页:标题/元信息/导出/继续录制
  "notes.title.renameHint": "点击改名",
  "notes.state.interrupted": "已中断",
  "notes.export.md": "导出 MD",
  "notes.export.done": "已导出：{path}",
  "notes.export.failed": "导出失败: {e}",
  "notes.record.busy": "已有录制进行中",
  "notes.record.resume": "继续录制",
  "notes.resume.blocked": "无法继续录制:请确认没有正在进行的录制",

  // 详情页:横幅与提示
  "notes.banner.interrupted": "这场录音曾意外中断，中断前的内容已保存。点击下方播放器右侧的红色录音键可接着录。",
  "notes.banner.skipped": "有 {n} 行记录损坏被跳过。",
  "notes.banner.llmPartial": "部分段落 AI 处理失败，已保留原文，可重新执行。",
  "notes.banner.llmFailed": "在线 AI 处理失败，当前展示本地处理结果。",
  "notes.hint.emptyRefined": "（修订稿为空，可直接输入补充内容）",
  "notes.hint.emptyTranscript": "（这场会议没有转写内容）",
  "notes.seg.filtered": "已被 AI 过滤",
  "notes.loading": "加载中…",
  "notes.jumpBack": "↓ 回到播放位置",

  // 详情页:视图切换与重新 AI
  "notes.view.refined": "修订稿",
  "notes.view.raw": "原始逐字稿",
  "notes.view.noRefined": "尚无修订稿",
  "notes.refine.loseNames": "未关联搭子的说话人改名将丢失",
  "notes.refine.confirm": "确认重新 AI",
  "notes.refine.run": "执行 AI",
  "notes.refine.running": "Aing，正在执行",
  "notes.refine.completeHint": "AI 已完成，点击重新执行",
  "notes.refine.failedHint": "AI 执行失败，点击重试",
  "notes.refine.rerunFailed": "重新执行 AI 失败：{e}",

  // 详情页:文件重转写(三期)
  "notes.retrans.run": "重转写",
  "notes.retrans.hint": "用盘上音频离线重新转写全文(覆盖原始逐字稿,自动备份)",
  "notes.retrans.warn": "将覆盖原始逐字稿并重建说话人,原稿备份为 segments.orig.jsonl",
  "notes.retrans.confirmDual": "双轨重转写",
  "notes.retrans.confirmMixed": "成品轨重转写",
  "notes.retrans.running": "重转写中（{stage}）",
  "notes.retrans.failed": "重转写失败：{e}",
  "notes.retrans.staleBanner": "段落已重转写，本修订稿基于旧文本，请重新执行 AI。",
  "notes.retrans.mixedCheckFailed": "无法检测成品轨可用性",

  // 详情页:错误文案(加载/删除失败复用 common.loadFailed/common.deleteFailed)
  "notes.detail.editFailed": "编辑失败: {e}",
  "notes.detail.setSpeakerFailed": "修改说话人失败: {e}",
  "notes.detail.renameFailed": "改名失败: {e}",
  "notes.detail.saveRefinedFailed": "精修稿保存失败: {e}",
  "notes.detail.drainFailed": "精修稿离开页面前排空失败: {e}",

  // 详情页:浮层菜单/相关笔记
  "notes.menu.newSpeaker": "＋ 新说话人",
  "notes.menu.confirmDelete": "确认删除",
  "notes.cancel": "取消",
  "notes.close": "关闭",
  "notes.entity.openGraph": "打开知识图谱",
  "notes.related.title": "相关笔记",
  "notes.related.shared": "共享 {n} 个实体",

  // 编辑器 NodeView(refinedSchema/segmentSchema)
  "notes.editor.playFromHere": "从此处播放",
  "notes.editor.changeSpeaker": "点击改说话人",
  "notes.editor.delete": "删除",

  // 播放器(AudioPlayer)
  "notes.player.play": "播放",
  "notes.player.pause": "暂停",
  "notes.player.progress": "播放进度",
  "notes.player.tracks": "音轨",
  "notes.player.tracksTitle": "音轨(回放有回音时可静掉一轨)",
  "notes.player.tracksMutedTitle": "音轨(有音轨已静音)",
  "notes.player.echoHint": "回放有回音?静掉一轨",
  "notes.player.mic": "麦克风",
  "notes.player.system": "系统声",
  "notes.player.muted": "已静音",
  "notes.player.errLoad": "音轨装载",
  "notes.player.errPlay": "播放",
} as const satisfies Dict;

export const en = {
  "notes.untitled": "Untitled",
  "notes.speaker.me": "Me",
  "notes.speaker.other": "Them",
  "notes.speaker.n": "Speaker {n}",
  "notes.speaker.newN": "New speaker {n}",
  "notes.duration.hm": "{h} hr {m} min",
  "notes.duration.ms": "{m} min {s} sec",
  "notes.duration.s": "{s} sec",

  "notes.title.renameHint": "Click to rename",
  "notes.state.interrupted": "Interrupted",
  "notes.export.md": "Export MD",
  "notes.export.done": "Exported: {path}",
  "notes.export.failed": "Failed to export: {e}",
  "notes.record.busy": "A recording is already in progress",
  "notes.record.resume": "Resume recording",
  "notes.resume.blocked": "Cannot resume recording: make sure no other recording is in progress",

  "notes.banner.interrupted":
    "This recording was interrupted unexpectedly; everything before the interruption is saved. Click the red record button to the right of the player below to continue.",
  "notes.banner.skipped": (p) => `${p.n} corrupted line${p.n === 1 ? "" : "s"} skipped.`,
  "notes.banner.llmPartial": "AI failed on some paragraphs; their original text is kept. You can run it again.",
  "notes.banner.llmFailed": "Online AI processing failed; showing local results.",
  "notes.hint.emptyRefined": "(Refined draft is empty; type to add content)",
  "notes.hint.emptyTranscript": "(This meeting has no transcript)",
  "notes.seg.filtered": "Filtered out by AI",
  "notes.loading": "Loading…",
  "notes.jumpBack": "↓ Back to playback position",

  "notes.view.refined": "Refined",
  "notes.view.raw": "Raw transcript",
  "notes.view.noRefined": "No refined draft yet",
  "notes.refine.loseNames": "Speaker renames not linked to a person will be lost",
  "notes.refine.confirm": "Confirm re-run AI",
  "notes.refine.run": "Run AI",
  "notes.refine.running": "AI is running",
  "notes.refine.completeHint": "AI complete; click to run again",
  "notes.refine.failedHint": "AI failed; click to retry",
  "notes.refine.rerunFailed": "Failed to re-run AI: {e}",

  "notes.retrans.run": "Re-transcribe",
  "notes.retrans.hint": "Re-run ASR offline from the audio on disk (overwrites the raw transcript; a backup is kept)",
  "notes.retrans.warn": "This overwrites the raw transcript and rebuilds speakers. The original is backed up as segments.orig.jsonl.",
  "notes.retrans.confirmDual": "Dual-track",
  "notes.retrans.confirmMixed": "Mixed-track",
  "notes.retrans.running": "Re-transcribing ({stage})",
  "notes.retrans.failed": "Re-transcription failed: {e}",
  "notes.retrans.staleBanner": "Segments were re-transcribed. This refined doc is based on the old text — please re-run AI.",
  "notes.retrans.mixedCheckFailed": "Could not check mixed-track availability",

  "notes.detail.editFailed": "Failed to edit: {e}",
  "notes.detail.setSpeakerFailed": "Failed to change speaker: {e}",
  "notes.detail.renameFailed": "Failed to rename: {e}",
  "notes.detail.saveRefinedFailed": "Failed to save refined draft: {e}",
  "notes.detail.drainFailed": "Failed to flush refined edits before leaving: {e}",

  "notes.menu.newSpeaker": "+ New speaker",
  "notes.menu.confirmDelete": "Confirm delete",
  "notes.cancel": "Cancel",
  "notes.close": "Close",
  "notes.entity.openGraph": "Open knowledge graph",
  "notes.related.title": "Related notes",
  "notes.related.shared": (p) => `${p.n} shared ${p.n === 1 ? "entity" : "entities"}`,

  "notes.editor.playFromHere": "Play from here",
  "notes.editor.changeSpeaker": "Click to change speaker",
  "notes.editor.delete": "Delete",

  "notes.player.play": "Play",
  "notes.player.pause": "Pause",
  "notes.player.progress": "Playback progress",
  "notes.player.tracks": "Tracks",
  "notes.player.tracksTitle": "Tracks (mute one if playback echoes)",
  "notes.player.tracksMutedTitle": "Tracks (a track is muted)",
  "notes.player.echoHint": "Echo during playback? Mute one track",
  "notes.player.mic": "Microphone",
  "notes.player.system": "System audio",
  "notes.player.muted": "Muted",
  "notes.player.errLoad": "Track loading",
  "notes.player.errPlay": "Playback",
} satisfies Record<keyof typeof zh, Msg>;
