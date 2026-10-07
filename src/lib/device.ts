// 设备听写(AI Passport / Vibe Voice)前端接口:命令包装、事件订阅、类型。
// 后端见 src-tauri/src/device;领域词见 CONTEXT.md「设备听写」。
import { invoke } from "@tauri-apps/api/core";
import { listen, type UnlistenFn } from "@tauri-apps/api/event";

/** 链路状态(后端 vibe_device::ble::LinkState,serde tag = kind)。 */
export type LinkState =
  | { kind: "unavailable" }
  | { kind: "searching" }
  | { kind: "connecting"; name: string }
  | { kind: "pairing"; name: string }
  | { kind: "pairing_failed"; name: string; reason: string }
  | { kind: "bond_lost"; name: string }
  | { kind: "connected"; name: string }
  | { kind: "retrying" };

/** 链路为什么停下、需要用户做什么。 */
export type LinkEnded = "pairing_failed" | "bond_lost" | "standalone_running";

export type LinkView = {
  state: LinkState | null;
  text: string;
  ended: LinkEnded | null;
  /** 协议版本不一致:设备固件旧了 / Voice Notes 旧了。 */
  mismatch: "device_older" | "device_newer" | null;
  /** 设备上报的固件版本串。 */
  firmware: string | null;
};

export type DeviceStatus = {
  enabled: boolean;
  device_name: string | null;
  running: boolean;
  link: LinkView;
  engine: string;
  /** 听写还缺的模型工件 id(交给 download_models)。 */
  missing_models: string[];
  /** macOS 辅助功能授权;其它平台恒 true。 */
  accessibility: boolean;
  /** macOS 语音识别授权("authorized"/"denied"/"restricted"/"not determined");其它平台 "n/a"。 */
  speech_permission: string;
  platform: "macos" | "windows" | string;
  /** 端到端测试用的模拟设备(环境变量 VN_DEVICE_SIM),平时恒 false。 */
  sim: boolean;
};

export type FoundDevice = { name: string; rssi: number | null; paired: boolean | null };

export type DictationSummary = {
  id: string;
  app: string;
  label: string;
  created_at: string;
  updated_at: string;
  count: number;
  preview: string;
  search_text: string;
};

export type DictationRecord = {
  rid: string;
  at: string;
  text: string;
  undone: boolean;
  audio: string | null;
  duration_ms: number;
};

export type DictationNote = {
  meta: { id: string; key: string; app: string; label: string; created_at: string; updated_at: string };
  records: DictationRecord[];
  audio_dir: string;
};

export const deviceStatus = () => invoke<DeviceStatus>("device_status");
export const deviceScan = () => invoke<FoundDevice[]>("device_scan");
export const deviceConnect = (name: string) => invoke<DeviceStatus>("device_connect", { name });
export const deviceReconnect = () => invoke<DeviceStatus>("device_reconnect");
export const deviceRepair = () => invoke<DeviceStatus>("device_repair");
export const deviceForget = () => invoke<DeviceStatus>("device_forget");
/** macOS:把 Voice Notes 登记进「辅助功能」列表并打开那一页;返回是否已授权。 */
export const deviceOpenAccessibility = () => invoke<boolean>("device_open_accessibility");
/** macOS:申请语音识别授权(没问过则弹系统框,被拒过则打开系统设置);返回授权状态名。 */
export const deviceGrantSpeech = () => invoke<string>("device_grant_speech");
export const deviceSubmitPin = (pin: string | null) => invoke<void>("device_submit_pin", { pin });

export const listDictationNotes = () => invoke<DictationSummary[]>("list_dictation_notes");
export const getDictationNote = (id: string) => invoke<DictationNote>("get_dictation_note", { id });
export const deleteDictationNote = (id: string) => invoke<void>("delete_dictation_note", { id });
export const deleteDictationRecord = (id: string, rid: string) =>
  invoke<void>("delete_dictation_record", { id, rid });

export const onDeviceState = (cb: (v: LinkView) => void): Promise<UnlistenFn> =>
  listen<LinkView>("device_state", (e) => cb(e.payload));
export const onPinRequest = (cb: (name: string) => void): Promise<UnlistenFn> =>
  listen<{ name: string }>("device_pin_request", (e) => cb(e.payload.name));
export const onPinClosed = (cb: () => void): Promise<UnlistenFn> => listen("device_pin_closed", () => cb());
export const onDictationNotesChanged = (cb: (id: string) => void): Promise<UnlistenFn> =>
  listen<string>("dictation_notes_changed", (e) => cb(e.payload));

/** 听写笔记 id 以 "d" + 16 位十六进制开头,与会议笔记的时间戳 id 天然不撞。 */
export const isDictationId = (id: string) => /^d[0-9a-f]{16}$/.test(id);

/** 配对码:设备屏幕上的 6 位数字。 */
export const normalizePin = (raw: string) => raw.replace(/\D/g, "").slice(0, 6);
