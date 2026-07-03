import { invoke } from "@tauri-apps/api/core";
import { goto } from "$app/navigation";
import {
  onPartial,
  onStatus,
  onFinal,
  onStorage,
  onSpeakers,
  type Source,
  type SystemAudio,
  type Diarization,
  type StatusEvent,
} from "./events";
import { getNote, resumeRecording } from "./notes";

export type Line = { source: Source; text: string; speaker: string | null };
export type SpeakerMap = Record<string, { name: string; sources: string[] }>;

let status = $state("idle");
let systemAudio = $state<SystemAudio>("");
let diarization = $state<Diarization>("");
let noteId = $state("");
let finals = $state<Line[]>([]);
let partialMic = $state("");
let partialSystem = $state("");
let storageDegraded = $state(false);
let speakers = $state<SpeakerMap>({});
/** recording/stopped/error 翻转时 +1，侧栏据此刷新列表。 */
let statusVersion = $state(0);
/** 笔记改名/删除后 +1，供侧栏与详情页跨组件同步刷新。 */
let notesVersion = $state(0);
let pending = $state(false);

let initialized = false;
/** 续录一次性标志：置位期间 "recording" 事件不清 finals/speakers（已由 resume() 灌注历史）。 */
let resuming = false;

/**
 * 全局录制状态：事件监听在 layout 挂载时注册一次，应用生命周期内不解绑。
 * 状态跨路由存活——侧栏按钮与录制页共读同一份。
 */
export const recording = {
  get status() { return status; },
  get systemAudio() { return systemAudio; },
  get diarization() { return diarization; },
  get noteId() { return noteId; },
  get finals() { return finals; },
  get partialMic() { return partialMic; },
  get partialSystem() { return partialSystem; },
  get storageDegraded() { return storageDegraded; },
  get speakers() { return speakers; },
  get statusVersion() { return statusVersion; },
  get notesVersion() { return notesVersion; },
  get pending() { return pending; },
  get isRecording() { return status === "recording"; },

  /** 笔记改名/删除后调用，驱动侧栏与详情页统一刷新。 */
  bumpNotes() { notesVersion++; },

  /** 幂等：注册事件监听 + 用 recording_status 重建冷启动状态。 */
  async init() {
    if (initialized) return;
    initialized = true;

    onPartial((e) => {
      if (e.source === "mic") partialMic = e.text;
      else partialSystem = e.text;
    });
    onFinal((e) => {
      if (e.text.trim())
        finals = [...finals, { source: e.source, text: e.text, speaker: e.speaker }];
      if (e.source === "mic") partialMic = "";
      else partialSystem = "";
    });
    onStatus((e) => {
      status = e.state;
      systemAudio = e.system_audio;
      diarization = e.diarization;
      if (e.state === "recording") {
        noteId = e.note_id;
        if (resuming) {
          // 续录：finals/speakers 已由 resume() 灌注历史段，此处只清瞬时状态。
          resuming = false;
          partialMic = "";
          partialSystem = "";
          storageDegraded = false;
          statusVersion++;
        } else {
          finals = [];
          partialMic = "";
          partialSystem = "";
          storageDegraded = false;
          speakers = {};
          statusVersion++;
        }
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
    onSpeakers((e) => {
      // 先把已上屏历史段的 loser id 改写为 winner，使历史徽章与合并后的新段统一。
      if (e.merged) {
        const { loser, winner } = e.merged;
        finals = finals.map((l) => (l.speaker === loser ? { ...l, speaker: winner } : l));
      }
      speakers = Object.fromEntries(
        e.speakers.map((s) => [s.id, { name: s.name, sources: s.sources }]),
      );
    });

    // 事件非粘性：冷启动/刷新时主动查询一次。返回 idle 不覆盖，避免与真实事件竞争。
    const s = await invoke<StatusEvent>("recording_status");
    if (s.state === "recording") {
      status = s.state;
      systemAudio = s.system_audio;
      diarization = s.diarization;
      noteId = s.note_id;
    }
  },

  /**
   * 一键开录。成功由 "recording" 事件驱动 UI；这里只处理同步拒绝。
   * 返回是否已发起（供调用方决定是否跳转）。
   */
  async start(): Promise<boolean> {
    if (pending || status === "recording") return false;
    pending = true;
    try {
      await invoke("start_recording");
      return true;
    } catch (err) {
      // "已在录制" = 竞态重复点击，不是错误：以后端真实状态为准，不污染 status。
      if (String(err).includes("已在录制")) {
        const s = await invoke<StatusEvent>("recording_status");
        if (s.state === "recording") {
          status = s.state;
          systemAudio = s.system_audio;
          diarization = s.diarization;
          noteId = s.note_id;
        }
        return false;
      }
      status = `error: ${err}`;
      return false;
    } finally {
      pending = false;
    }
  },

  /**
   * 续录已中断的笔记：先用历史段灌注 finals/speakers，再发起 resume_recording。
   * 返回是否已发起（供调用方决定是否跳转到 /record）。
   */
  async resume(noteId_: string): Promise<boolean> {
    if (pending || status === "recording") return false;
    pending = true;
    try {
      // getNote 失败：不灌注、不置 resuming，原样冒泡为 error 状态。
      const note = await getNote(noteId_);
      finals = note.segments
        .filter((s) => s.text.trim())
        .map((s) => ({ source: s.source, text: s.text, speaker: s.speaker }));
      speakers = { ...note.speakers };
      noteId = noteId_;
      resuming = true;
      try {
        await resumeRecording(noteId_);
        return true;
      } catch (err) {
        resuming = false;
        // "已在录制" = 竞态重复点击，不是错误：以后端真实状态为准，不污染 status。
        if (String(err).includes("已在录制")) {
          const s = await invoke<StatusEvent>("recording_status");
          if (s.state === "recording") {
            status = s.state;
            systemAudio = s.system_audio;
            diarization = s.diarization;
            noteId = s.note_id;
          }
          return false;
        }
        status = `error: ${err}`;
        return false;
      }
    } catch (err) {
      status = `error: ${err}`;
      return false;
    } finally {
      pending = false;
    }
  },

  async stop() {
    if (pending) return;
    pending = true;
    try {
      await invoke("stop_recording");
    } finally {
      pending = false;
    }
  },
};
