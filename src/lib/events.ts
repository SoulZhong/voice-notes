import { listen } from "@tauri-apps/api/event";

export type PartialEvent = { text: string };
export type StatusEvent = { state: string };

export function onPartial(cb: (e: PartialEvent) => void) {
  return listen<PartialEvent>("partial", (ev) => cb(ev.payload));
}

export function onStatus(cb: (e: StatusEvent) => void) {
  return listen<StatusEvent>("status", (ev) => cb(ev.payload));
}
