import { listen } from "@tauri-apps/api/event";

export type Source = "mic" | "system";
export type SystemAudio = "on" | "denied" | "unavailable" | "";

export type PartialEvent = { source: Source; text: string };
export type FinalEvent = {
  seq: number;
  source: Source;
  text: string;
  start_ms: number;
  end_ms: number;
  speaker: string | null;
};
export type Diarization = "on" | "unavailable" | "";
export type StatusEvent = {
  state: string;
  system_audio: SystemAudio;
  note_id: string;
  diarization: Diarization;
  elapsed_ms: number;
};
export type StorageEvent = { state: "ok" | "degraded" };
/** 追溯回声撤回:已上屏的 mic 段被 system 定稿追认为回声,应从 finals 移除匹配行。 */
export type RetractEvent = { source: Source; start_ms: number; end_ms: number; text: string };
export type SpeakerEntry = {
  id: string;
  name: string;
  sources: Source[];
  /** 全局声纹库人物 id(P<n>)：实时入库/种子命中后即有；null = 尚未够料入库。 */
  person_id: string | null;
};
export type SpeakersEvent = {
  speakers: SpeakerEntry[];
  merged: { loser: string; winner: string } | null;
};
export type RefineEvent = {
  note_id: string;
  stage: string;
  state: string;
};

/** 文件重转写进度("retranscribe")。stage: decode/transcribe/attribute/commit/all;
 * state: running/ok/error。message 仅 error 带;summary 仅 all/ok 带。 */
export type RetranscribeSummary = {
  old_segments: number;
  new_segments: number;
  seed_matched: number;
  inherited: number;
  echo_dropped: number;
  failed_segments: number;
};
export type RetranscribeEvent = {
  note_id: string;
  stage: string;
  state: string;
  message?: string;
  summary?: RetranscribeSummary;
};
export function onRetranscribe(cb: (e: RetranscribeEvent) => void) {
  return listen<RetranscribeEvent>("retranscribe", (ev) => cb(ev.payload));
}

/** 补生成成品轨进度(二期)。stage: decode/align/mix/finish;state: running/ok/error。 */
export type MixedRegenEvent = {
  note_id: string;
  stage: string;
  state: string;
  message?: string;
};
export function onMixedRegen(cb: (e: MixedRegenEvent) => void) {
  return listen<MixedRegenEvent>("mixed_regen", (ev) => cb(ev.payload));
}

export function onPartial(cb: (e: PartialEvent) => void) {
  return listen<PartialEvent>("partial", (ev) => cb(ev.payload));
}

export function onStatus(cb: (e: StatusEvent) => void) {
  return listen<StatusEvent>("status", (ev) => cb(ev.payload));
}

export function onFinal(cb: (e: FinalEvent) => void) {
  return listen<FinalEvent>("final", (ev) => cb(ev.payload));
}

export function onStorage(cb: (e: StorageEvent) => void) {
  return listen<StorageEvent>("storage", (ev) => cb(ev.payload));
}

export function onSpeakers(cb: (e: SpeakersEvent) => void) {
  return listen<SpeakersEvent>("speakers", (ev) => cb(ev.payload));
}

export function onRetract(cb: (e: RetractEvent) => void) {
  return listen<RetractEvent>("final_retract", (ev) => cb(ev.payload));
}

/** 停录后音频转码完成(源 WAV 已删):详情页应重拉音轨,否则播放器握着失效引用无声播放。 */
export type TranscodeEvent = { note_id: string };
export function onTranscodeDone(cb: (e: TranscodeEvent) => void) {
  return listen<TranscodeEvent>("transcode_done", (ev) => cb(ev.payload));
}

export type LevelEvent = { source: Source; rms: number };

export function onLevel(cb: (e: LevelEvent) => void) {
  return listen<LevelEvent>("level", (ev) => cb(ev.payload));
}

export function onRefine(cb: (e: RefineEvent) => void) {
  return listen<RefineEvent>("refine", (ev) => cb(ev.payload));
}

/** 原生播放器位置事件(~200ms 一发,播/停/seek 立即补发):前端只画 UI,时钟在 Rust。
 * gen 是装载代次(生产环境从 1 起,永不为 0),播放会话 store 靠它辨认事件归属。 */
export type PlayerPosEvent = { pos_ms: number; playing: boolean; gen: number };
export function onPlayerPos(cb: (e: PlayerPosEvent) => void) {
  return listen<PlayerPosEvent>("player_pos", (ev) => cb(ev.payload));
}

/** 后端主动停掉了播放(目前唯一发源地:托盘「停止播放」)。前端发起的 player_stop 不发
 * 这条——它自己知道。gen = 被停掉的那次装载代次,收方据此只清自己名下的会话/播放器。 */
export type PlayerStoppedEvent = { gen: number };
export function onPlayerStopped(cb: (e: PlayerStoppedEvent) => void) {
  return listen<PlayerStoppedEvent>("player_stopped", (ev) => cb(ev.payload));
}

/** 云端识别连接状态(仅云端模式录制时产生),事件名 "cloud-asr-status"。录制页据此显示
 * 「重连中/补识中/补识失败」的细提示条,未接监听不影响录制。 */
export type CloudAsrStatusEvent = {
  state: "reconnecting" | "recovered" | "backfilling" | "backfill_failed";
  source: Source;
  /** 断连原因原文(仅 reconnecting 可能带),状态条截断附在「重连中…」后面。 */
  message?: string;
};
export function onCloudAsrStatus(cb: (e: CloudAsrStatusEvent) => void) {
  return listen<CloudAsrStatusEvent>("cloud-asr-status", (ev) => cb(ev.payload));
}

/** 托盘菜单请求导航(目前只有「打开设置」)。托盘在 Rust 侧、路由在前端,
 * 只能由后端发事件、前端自己 goto。 */
export type TrayNavigateEvent = { path: string };
export function onTrayNavigate(cb: (e: TrayNavigateEvent) => void) {
  return listen<TrayNavigateEvent>("tray_navigate", (ev) => cb(ev.payload));
}

/** 后端自动改名(LLM 主题标题)。侧栏与详情页据此刷新标题。 */
export type NoteRenamedEvent = { note_id: string; title: string };

export function onNoteRenamed(cb: (e: NoteRenamedEvent) => void) {
  return listen<NoteRenamedEvent>("note_renamed", (ev) => cb(ev.payload));
}

/** 录制中段编辑落盘成功(后端为唯一真值源,前端不做乐观更新)。未动字段为 null。 */
export type SegmentEditedEvent = { note_id: string; seq: number; text: string | null; speaker: string | null };
export function onSegmentEdited(cb: (e: SegmentEditedEvent) => void) {
  return listen<SegmentEditedEvent>("segment_edited", (ev) => cb(ev.payload));
}

/** 跨轨时基已纠正(mic 轨时钟漂移过,回放侧实测出映射并落盘)。详情页手里那份转写段
    的时间戳还停在旧时基上——高亮跟不上、点段落跳错位置、mic 行与 system 行的次序也
    是错的,须整页重拉。 */
export type NoteRealignedEvent = { note_id: string; drift_ms: number };

export function onNoteRealigned(cb: (e: NoteRealignedEvent) => void) {
  return listen<NoteRealignedEvent>("note_realigned", (ev) => cb(ev.payload));
}
