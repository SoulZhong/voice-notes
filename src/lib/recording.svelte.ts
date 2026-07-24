import { invoke } from "@tauri-apps/api/core";
import { goto } from "$app/navigation";
import {
  onPartial,
  onStatus,
  onFinal,
  onStorage,
  onSpeakers,
  onRetract,
  onLevel,
  type Source,
  type SystemAudio,
  type Diarization,
  type StatusEvent,
} from "./events";
import { getNote, resumeRecording } from "./notes";

/** start_ms:回声撤回按 (source, start_ms, text) 精确定位行,展示层不消费。 */
export type Line = { source: Source; text: string; speaker: string | null; start_ms: number };
export type SpeakerMap = Record<
  string,
  { name: string; sources: string[]; person_id?: string | null }
>;

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
/** 声纹库人物改名/合并/删除后 +1，侧栏声纹库页签简表据此重拉,不滞留旧名。 */
let peopleVersion = $state(0);
let pending = $state(false);
let paused = $state(false);
/** 计时基线（后端 elapsed_ms 快照）+ 本地锚点（recording 态才走表）。 */
let elapsedBaseMs = $state(0);
let tickAnchor = $state<number | null>(null);
let nowTick = $state(Date.now());
let level = $state(0);

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
  get paused() { return paused; },
  get stopping() { return status === "stopping"; },
  get isLive() { return status === "recording" || status === "paused"; },
  get level() { return level; },
  /** 活跃录制毫秒：后端快照 + 本地走表（暂停/停止时不走）。 */
  get elapsedMs() { return elapsedBaseMs + (tickAnchor !== null ? nowTick - tickAnchor : 0); },

  /** 笔记改名/删除后调用，驱动侧栏与详情页统一刷新。 */
  bumpNotes() { notesVersion++; },
  get peopleVersion() { return peopleVersion; },
  /** 声纹库人物变更后调用（管理页改名/合并/删除），驱动侧栏简表刷新。 */
  bumpPeople() { peopleVersion++; },

  async pause() {
    if (pending || status !== "recording") return;
    pending = true;
    try { await invoke("pause_recording"); } finally { pending = false; }
  },
  async unpause() {
    if (pending || status !== "paused") return;
    pending = true;
    try { await invoke("unpause_recording"); } finally { pending = false; }
  },

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
        finals = [...finals, { source: e.source, text: e.text, speaker: e.speaker, start_ms: e.start_ms }];
      if (e.source === "mic") partialMic = "";
      else partialSystem = "";
    });
    onRetract((e) => {
      // 追溯回声撤回:该行事后被确认为对方声音的回声,从已上屏内容移除(磁盘已同步删)。
      const idx = finals.findIndex(
        (l) => l.source === e.source && l.start_ms === e.start_ms && l.text === e.text,
      );
      if (idx >= 0) finals = [...finals.slice(0, idx), ...finals.slice(idx + 1)];
    });
    onStatus((e) => {
      if (e.state === "recording") {
        // 同一笔记且此前已是 live（暂停恢复/重复对账）：只更新计时与暂停位，不清屏。
        const isUnpause = e.note_id === noteId && (status === "recording" || status === "paused");
        status = e.state;
        systemAudio = e.system_audio;
        diarization = e.diarization;
        paused = false;
        elapsedBaseMs = e.elapsed_ms;
        tickAnchor = Date.now();
        if (isUnpause) return;
        noteId = e.note_id;
        if (resuming) {
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
      } else if (e.state === "paused") {
        status = "paused";
        paused = true;
        elapsedBaseMs = e.elapsed_ms;
        tickAnchor = null;
        partialMic = "";
        partialSystem = "";
      } else if (e.state === "stopped" || e.state.startsWith("error:")) {
        status = e.state;
        systemAudio = e.system_audio;
        diarization = e.diarization;
        resuming = false;
        paused = false;
        elapsedBaseMs = 0;
        tickAnchor = null;
        level = 0;
        partialMic = "";
        partialSystem = "";
        storageDegraded = false;
        statusVersion++;
        if (e.state === "stopped" && e.note_id) {
          goto(`/notes/${e.note_id}`);
        }
      } else {
        status = e.state;
      }
    });
    onLevel((e) => {
      level = e.rms;
    });
    setInterval(() => {
      nowTick = Date.now();
    }, 1000);
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
        e.speakers.map((s) => [s.id, { name: s.name, sources: s.sources, person_id: s.person_id }]),
      );
    });

    // 事件非粘性：冷启动/刷新时主动查询一次。返回 idle 不覆盖，避免与真实事件竞争。
    const s = await invoke<StatusEvent>("recording_status");
    if (s.state === "recording" || s.state === "paused") {
      status = s.state;
      systemAudio = s.system_audio;
      diarization = s.diarization;
      noteId = s.note_id;
      paused = s.state === "paused";
      elapsedBaseMs = s.elapsed_ms;
      tickAnchor = s.state === "recording" ? Date.now() : null;
      await hydrateFromDisk(s.note_id);
    }
  },

  /**
   * 一键开录。成功由 "recording" 事件驱动 UI；这里只处理同步拒绝。
   * 返回是否已发起（供调用方决定是否跳转）。
   */
  async start(): Promise<boolean> {
    if (pending || this.isLive) return false;
    pending = true;
    try {
      await invoke("start_recording");
      return true;
    } catch (err) {
      // "已在录制" = 竞态重复点击，不是错误：以后端真实状态为准，不污染 status。
      if (String(err).includes("已在录制")) {
        const s = await invoke<StatusEvent>("recording_status");
        if (s.state === "recording" || s.state === "paused") {
          status = s.state;
          systemAudio = s.system_audio;
          diarization = s.diarization;
          noteId = s.note_id;
          paused = s.state === "paused";
          elapsedBaseMs = s.elapsed_ms;
          tickAnchor = s.state === "recording" ? Date.now() : null;
          await hydrateFromDisk(s.note_id);
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
    if (pending || this.isLive) return false;
    pending = true;
    try {
      // getNote 失败：不灌注、不置 resuming，原样冒泡为 error 状态。
      const note = await getNote(noteId_);
      finals = note.segments.map((s) => ({ source: s.source, text: s.text, speaker: s.speaker, start_ms: s.start_ms }));
      speakers = { ...note.speakers };
      noteId = noteId_;
      resuming = true;
      try {
        await resumeRecording(noteId_);
        return true;
      } catch (err) {
        resuming = false;
        finals = []; // 回滚预灌注
        speakers = {};
        noteId = ""; // 回滚预灌注（下方"已在录制"对账分支会用真实 noteId 覆盖）
        // "已在录制" = 竞态重复点击，不是错误：以后端真实状态为准，不污染 status。
        if (String(err).includes("已在录制")) {
          const s = await invoke<StatusEvent>("recording_status");
          if (s.state === "recording" || s.state === "paused") {
            status = s.state;
            systemAudio = s.system_audio;
            diarization = s.diarization;
            noteId = s.note_id;
            paused = s.state === "paused";
            elapsedBaseMs = s.elapsed_ms;
            tickAnchor = s.state === "recording" ? Date.now() : null;
            await hydrateFromDisk(s.note_id);
          }
          return false;
        }
        status = `error: ${err}`;
        return false;
      }
    } catch (err) {
      resuming = false; // 回滚预灌注
      finals = [];
      speakers = {};
      noteId = "";
      status = `error: ${err}`;
      return false;
    } finally {
      pending = false;
    }
  },

  async stop() {
    if (pending || !this.isLive) return;
    const previousStatus = status;
    pending = true;
    // 后端仍会安全排空识别/音频管线并完成落盘；先给出即时反馈，避免数秒收尾看似没点中。
    status = "stopping";
    paused = false;
    tickAnchor = null;
    level = 0;
    try {
      await invoke("stop_recording");
    } catch (err) {
      status = previousStatus;
      paused = previousStatus === "paused";
      tickAnchor = previousStatus === "recording" ? Date.now() : null;
      throw err;
    } finally {
      pending = false;
    }
  },
};

/** 冷刷新/对账时用磁盘内容回灌 finals+speakers（录制中笔记边录边落盘，直接可读）。 */
async function hydrateFromDisk(id: string) {
  if (!id) return;
  try {
    const note = await getNote(id);
    finals = note.segments.map((s) => ({ source: s.source, text: s.text, speaker: s.speaker, start_ms: s.start_ms }));
    speakers = { ...note.speakers };
  } catch {
    // 水合失败仅影响历史段回显，不阻塞录制状态重建。
  }
}
