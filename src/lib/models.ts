import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";

export type ArtifactState = {
  id: string;
  label: string;
  approx_mb: number;
  required_for_recording: boolean;
  present: boolean;
  /** 原始下载地址(GitHub release 直链),设置页展开展示用。 */
  url: string;
};
export type ModelsStatus = {
  artifacts: ArtifactState[];
  recording_ready: boolean;
  diarization_ready: boolean;
  /** 模型存储目录(设置页展示,点击经 open_models_dir 在文件管理器中打开)。 */
  root: string;
};
export type Settings = {
  mirror_enabled: boolean;
  mirror_prefix: string;
  data_dir?: string | null;
  models_dir?: string | null;
  asr_model: string;
  /** sherpa 推理 provider 覆盖(实验字段,无 UI,手改 settings.json;空 = CPU)。 */
  asr_provider: string;
  /** 识别方式:"local"(默认,现状) / "cloud"。录制中禁改(后端 set_settings 拦截)。 */
  asr_mode: string;
  /** 云端厂商:"volcano" / "aliyun"。 */
  cloud_asr_provider: string;
  /** 火山凭证(APP ID)。明文存储,同 refine_api_key 先例。 */
  volc_app_key: string;
  /** 火山凭证(Access Token)。 */
  volc_access_key: string;
  /** 阿里 DashScope API Key。明文,同上。 */
  dashscope_api_key: string;
  /** 声纹嵌入模型:"campplus"(默认)/"eres2netv2"。切换触发声纹库后台重建。 */
  speaker_model: string;
  // "system" | "light" | "dark";具体枚举/校验留给后续任务,这里先补字段让 applyTheme 能读到值
  theme: string;
  // UI 语言:"system" | "zh" | "en"。注意与 language_filter(转写乱码过滤)无关。
  ui_lang: string;
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
  // ASR Aing 开关
  refine_enabled: boolean;
  // Aing 执行体:"openai"(HTTP 接口)| "agent"(本机 Agent CLI 经 MCP 读写回)
  refine_provider: string;
  // provider=agent 时用哪家 CLI:"claude" | "codex" | "gemini" | "cursor"
  refine_agent: string;
  // Agent CLI 可执行文件路径覆盖;空 = 自动探测
  refine_agent_bin: string;
  // Agent 模型名(--model);空 = CLI 默认
  refine_agent_model: string;
  // ASR Aing 服务基础 URL
  refine_base_url: string;
  // ASR Aing LLM 模型
  refine_model: string;
  // ASR Aing API 密钥
  refine_api_key: string;
  // 首启引导已完成(欢迎层走完或模型已就绪时静默补 true)
  onboarded: boolean;
  // 已完成的功能引导 ID；每项功能独立记录，新增功能不能复用 onboarded 判断
  completed_guides: string[];
  // 隐私敏感:开录必须用户显式授权,默认关
  mcp_allow_control: boolean;
  // 防重复引导:欢迎页走完或提示条关闭后置 true,两处引导只出现一次
  mcp_onboarded: boolean;
  /** 声音处理方案:"a" 双轨(默认)/"ab" 对照(混音+默认双轨)/"b" 成品轨(混音+默认成品轨)。 */
  audio_scheme: "a" | "ab" | "b";
  calendar_match_enabled: boolean;
  identify_auto_apply: boolean;
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
export const openModelsDir = () => invoke<void>("open_models_dir");
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
// —— 外部集成配置测试(失败时命令 reject,前端 catch 出归类原因) ——
export const testRefineLlm = (baseUrl: string, model: string, apiKey: string) =>
  invoke<string>("test_refine_llm", { baseUrl, model, apiKey });
export const testRefineAgent = (provider: string, bin: string, model: string) =>
  invoke<string>("test_refine_agent", { provider, bin, model });
export const testMirror = (prefix: string) => invoke<string>("test_mirror", { prefix });
// 云端 ASR 测试连接:直接测试表单当前值,不依赖 onblur 先把凭证落盘。
// 这样用户粘贴 key 后立刻点测试也不会与异步保存竞态；录制中仍由后端拒绝。
export const testCloudAsr = (
  provider: string,
  volcAppKey: string,
  volcAccessKey: string,
  dashscopeApiKey: string,
) =>
  invoke<string>("test_cloud_asr", {
    provider,
    volcAppKey,
    volcAccessKey,
    dashscopeApiKey,
  });

/** P3 日历授权态:full | write_only | denied | not_determined | unavailable。 */
export const calendarPermission = () => invoke<string>("calendar_permission");
/** 发起系统日历授权(只能由设置页说明卡触发):granted | denied | insufficient | error | timeout。 */
export const requestCalendarPermission = () => invoke<string>("request_calendar_permission");
