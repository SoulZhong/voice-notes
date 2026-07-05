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
export type Settings = {
  mirror_enabled: boolean;
  mirror_prefix: string;
  data_dir?: string | null;
  models_dir?: string | null;
  asr_model: string;
  // "system" | "light" | "dark";具体枚举/校验留给后续任务,这里先补字段让 applyTheme 能读到值
  theme: string;
};
export type ModelDownloadEvent = {
  artifact: string;
  phase: "downloading" | "verifying" | "extracting" | "done" | "error" | "cancelled";
  received_bytes: number;
  total_bytes: number;
  message: string;
};
export type MigrateEvent = { kind: "data" | "models"; phase: "copying" | "done" | "error"; message: string };

export const modelsStatus = () => invoke<ModelsStatus>("models_status");
export const downloadModels = (ids?: string[]) => invoke<void>("download_models", { ids: ids ?? null });
export const deleteModel = (id: string) => invoke<void>("delete_model", { id });
export const cancelModelsDownload = () => invoke<void>("cancel_models_download");
export const getSettings = () => invoke<Settings>("get_settings");
export const setSettings = (s: Settings) => invoke<void>("set_settings", { newSettings: s });
export const migrateDataDir = (newDir: string) => invoke<void>("migrate_data_dir", { newDir });
export const migrateModelsDir = (newDir: string) => invoke<void>("migrate_models_dir", { newDir });
export function onModelDownload(cb: (e: ModelDownloadEvent) => void) {
  return listen<ModelDownloadEvent>("model_download", (ev) => cb(ev.payload));
}
export function onMigrate(cb: (e: MigrateEvent) => void) {
  return listen<MigrateEvent>("migrate", (ev) => cb(ev.payload));
}
