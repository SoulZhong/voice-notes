import { invoke } from "@tauri-apps/api/core";
import { goto } from "$app/navigation";
import {
  onPartial,
  onStatus,
  onFinal,
  onStorage,
  type Source,
  type SystemAudio,
  type StatusEvent,
} from "./events";

export type Line = { source: Source; text: string };

let status = $state("idle");
let systemAudio = $state<SystemAudio>("");
let noteId = $state("");
let finals = $state<Line[]>([]);
let partialMic = $state("");
let partialSystem = $state("");
let storageDegraded = $state(false);
/** recording/stopped/error 翻转时 +1，侧栏据此刷新列表。 */
let statusVersion = $state(0);

let initialized = false;

/**
 * 全局录制状态：事件监听在 layout 挂载时注册一次，应用生命周期内不解绑。
 * 状态跨路由存活——侧栏按钮与录制页共读同一份。
 */
export const recording = {
  get status() { return status; },
  get systemAudio() { return systemAudio; },
  get noteId() { return noteId; },
  get finals() { return finals; },
  get partialMic() { return partialMic; },
  get partialSystem() { return partialSystem; },
  get storageDegraded() { return storageDegraded; },
  get statusVersion() { return statusVersion; },
  get isRecording() { return status === "recording"; },

  /** 幂等：注册事件监听 + 用 recording_status 重建冷启动状态。 */
  async init() {
    if (initialized) return;
    initialized = true;

    onPartial((e) => {
      if (e.source === "mic") partialMic = e.text;
      else partialSystem = e.text;
    });
    onFinal((e) => {
      if (e.text.trim()) finals = [...finals, { source: e.source, text: e.text }];
      if (e.source === "mic") partialMic = "";
      else partialSystem = "";
    });
    onStatus((e) => {
      status = e.state;
      systemAudio = e.system_audio;
      if (e.state === "recording") {
        noteId = e.note_id;
        finals = [];
        partialMic = "";
        partialSystem = "";
        storageDegraded = false;
        statusVersion++;
      } else if (e.state === "stopped" || e.state.startsWith("error:")) {
        partialMic = "";
        partialSystem = "";
        storageDegraded = false;
        statusVersion++;
        if (e.state === "stopped" && e.note_id) {
          goto(`/notes/${e.note_id}`);
        }
      }
    });
    onStorage((e) => {
      storageDegraded = e.state === "degraded";
    });

    // 事件非粘性：冷启动/刷新时主动查询一次。返回 idle 不覆盖，避免与真实事件竞争。
    const s = await invoke<StatusEvent>("recording_status");
    if (s.state === "recording") {
      status = s.state;
      systemAudio = s.system_audio;
      noteId = s.note_id;
    }
  },

  /** 一键开录。成功由 "recording" 事件驱动 UI；这里只处理同步拒绝。 */
  async start() {
    try {
      await invoke("start_recording");
    } catch (err) {
      status = `error: ${err}`;
    }
  },

  async stop() {
    await invoke("stop_recording");
  },
};
