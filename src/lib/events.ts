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
};
export type StorageEvent = { state: "ok" | "degraded" };
export type SpeakerEntry = { id: string; name: string; sources: Source[] };
export type SpeakersEvent = {
  speakers: SpeakerEntry[];
  merged: { loser: string; winner: string } | null;
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
