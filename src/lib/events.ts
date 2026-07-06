import { listen } from "@tauri-apps/api/event";

export type Source = "mic" | "system";
export type SystemAudio = "on" | "denied" | "unavailable" | "";

export type PartialEvent = { source: Source; text: string };
export type FinalEvent = {
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

export type LevelEvent = { rms: number };

export function onLevel(cb: (e: LevelEvent) => void) {
  return listen<LevelEvent>("level", (ev) => cb(ev.payload));
}

export function onRefine(cb: (e: RefineEvent) => void) {
  return listen<RefineEvent>("refine", (ev) => cb(ev.payload));
}
