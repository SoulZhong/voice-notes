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
  /** 声纹嵌入模型:"campplus"(默认)/"eres2netv2"。切换触发声纹库后台重建。 */
  speaker_model: string;
  // "system" | "light" | "dark";具体枚举/校验留给后续任务,这里先补字段让 applyTheme 能读到值
  theme: string;
  // 仅录系统声(不录麦克风)
  record_system_only: boolean;
  // 录制时保持外放音量:麦克风用普通输入代替 VPIO(无 ducking,失去系统回声消除)
  keep_output_volume: boolean;
  // 转写语言过滤开关
  language_filter: boolean;
  // 是否保留原始录音音频
  keep_audio: boolean;
  // 全局快捷键开关
  shortcut_enabled: boolean;
  // 全局快捷键组合(tauri accelerator 格式,如 "Alt+CmdOrCtrl+R")
  shortcut: string;
  // 系统托盘图标开关
  tray_enabled: boolean;
  // ASR 精修开关
  refine_enabled: boolean;
  // ASR 精修服务基础 URL
  refine_base_url: string;
  // ASR 精修 LLM 模型
  refine_model: string;
  // ASR 精修 API 密钥
  refine_api_key: string;
  // 首启引导已完成(欢迎层走完或模型已就绪时静默补 true)
  onboarded: boolean;
  // 隐私敏感:开录必须用户显式授权,默认关
  mcp_allow_control: boolean;
  // 防重复引导:欢迎页走完或提示条关闭后置 true,两处引导只出现一次
  mcp_onboarded: boolean;
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
// 将当前 settings.shortcut/shortcut_enabled 应用到系统全局快捷键(失败时后端会自动把 shortcut_enabled 落回 false)。
export const applyShortcut = () => invoke<void>("apply_shortcut");
// 查询录音音频文件占用的磁盘字节数,用于设置页展示。
export const audioDiskUsage = () => invoke<number>("audio_disk_usage");
// 清理录音音频;olderThanDays 为 null 时清理全部,否则只清理超过对应天数的文件。返回释放的字节数。
export const purgeAudio = (olderThanDays: number | null) => invoke<number>("purge_audio", { olderThanDays });
export function onModelDownload(cb: (e: ModelDownloadEvent) => void) {
  return listen<ModelDownloadEvent>("model_download", (ev) => cb(ev.payload));
}
export function onMigrate(cb: (e: MigrateEvent) => void) {
  return listen<MigrateEvent>("migrate", (ev) => cb(ev.payload));
}
