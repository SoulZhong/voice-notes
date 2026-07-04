import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";

export type ArtifactState = {
  id: string;
  label: string;
  approx_mb: number;
  required_for_recording: boolean;
  present: boolean;
};
export type ModelsStatus = {
  artifacts: ArtifactState[];
  recording_ready: boolean;
  diarization_ready: boolean;
};
export type Settings = { mirror_enabled: boolean; mirror_prefix: string };
export type ModelDownloadEvent = {
  artifact: string;
  phase: "downloading" | "verifying" | "extracting" | "done" | "error" | "cancelled";
  received_bytes: number;
  total_bytes: number;
  message: string;
};

export const modelsStatus = () => invoke<ModelsStatus>("models_status");
export const downloadModels = () => invoke<void>("download_models");
export const cancelModelsDownload = () => invoke<void>("cancel_models_download");
export const getSettings = () => invoke<Settings>("get_settings");
export const setSettings = (s: Settings) => invoke<void>("set_settings", { newSettings: s });
export function onModelDownload(cb: (e: ModelDownloadEvent) => void) {
  return listen<ModelDownloadEvent>("model_download", (ev) => cb(ev.payload));
}
