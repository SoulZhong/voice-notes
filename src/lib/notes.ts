import { invoke } from "@tauri-apps/api/core";
import { t } from "$lib/i18n/index.svelte";
import type { Source } from "./events";

export type NoteState = "active" | "recording" | "complete";

export type NoteSummary = {
  id: string;
  title: string;
  started_at: string;
  duration_secs: number | null;
  state: NoteState;
};

export type CalendarAttendee = {
  name: string;
  email: string;
  is_me: boolean;
};

/** 日历事件快照(P3):落盘即快照,不依赖 event_id 活性。 */
export type CalendarSnapshot = {
  event_id: string;
  title: string;
  attendees: CalendarAttendee[];
  matched_at: string;
  match_kind: string; // "auto" | "manual"
};

export type NoteMeta = {
  schema_version: number;
  id: string;
  title: string;
  started_at: string;
  ended_at: string | null;
  state: string;
  calendar?: CalendarSnapshot | null;
  calendar_cleared?: boolean;
};

export type SegmentRecord = {
  seq: number;
  source: Source;
  text: string;
  start_ms: number;
  end_ms: number;
  speaker: string | null;
  rms?: number;
};

export type Note = {
  meta: NoteMeta;
  segments: SegmentRecord[];
  /** 自动规则隐藏但仍保留在原始转写中的段，用于诊断与恢复。 */
  suppressed_segments: SegmentRecord[];
  skipped_lines: number;
  // centroid/count 是后端质心快照（P4.5 续录铺底），person_id 是关联的全局声纹库
  // 人物 id（P5.5 铺底），随 get_note 下发；前端目前不消费这三者，仅补齐类型以
  // 匹配后端 SpeakerMeta 的实际字段（name 已经过后端只读 join，可能是库现名）。
  speakers: Record<
    string,
    {
      name: string;
      sources: string[];
      centroid?: number[];
      count?: number;
      person_id?: string;
    }
  >;
};

/** 一条音频轨道(对应后端 store::audio::TrackInfo)。offset_ms:该 WAV 的 0 时刻
    对应笔记时间轴的毫秒(轨道可中途出现:续录旧笔记/某源第二场才授权)。 */
export type TrackInfo = {
  // 源轨是 "mic"/"system";二期起成品轨("mixed")也走本结构进播放器
  // (mixed_playback_info 返回,单轨装载,不进 noteAudioInfo 的源轨列表)。
  source: Source | "mixed";
  path: string;
  offset_ms: number;
  duration_ms: number;
  // 真实音频波形(0..255 峰值桶,260 桶等分时长);null/缺失 = 旧笔记未回填,
  // 页面回退按转写段落 rms 聚合的包络。
  waveform?: number[] | null;
};

export interface Mention {
  /** Schema-v1 payloads omit this stable mention id. */
  id?: string;
  entity: string;
  start: number;
  end: number;
}
export interface Entity {
  id: string;
  kind: string;
  name: string;
  aliases?: string[];
}

export interface RefinedParagraph {
  speaker: string;
  name?: string;
  /** 关联的全局声纹库人物 id(P<n>):种子命中或用户在说话人条手动关联时存在。 */
  person_id?: string;
  start_ms: number;
  end_ms: number;
  text: string;
  source_seqs: number[];
  mentions?: Mention[];
}

export interface RefineStages {
  filter: string;
  recluster: string;
  llm: string;
  entities?: string;
  /** Schema-v1 payloads omit the relation extraction stage. */
  relations?: string;
}

export interface RelationPredicate {
  type: string;
  label?: string;
}

export interface RelationEvidence {
  /** Schema-v1-compatible default; populated for schema-v2 writes. */
  id?: string;
  paragraph_index: number;
  start: number;
  end: number;
  quote: string;
  source_seqs?: number[];
  source_hash?: string;
}

export interface RelationFact {
  /** Schema-v1-compatible default; populated for schema-v2 writes. */
  id?: string;
  subject: string;
  predicate: RelationPredicate;
  object: string;
  subject_mentions?: string[];
  object_mentions?: string[];
  confidence: number;
  valid_from?: string;
  valid_to?: string;
  evidence?: RelationEvidence[];
}

export interface GraphExtraction {
  contract_version: number;
  provider: string;
  model: string;
  run_id: string;
  generated_at: string;
  source_hash: string;
  mode: string;
}

export interface RefinedDoc {
  schema_version: number;
  generated_at: string;
  llm_model?: string;
  stages: RefineStages;
  discarded_seqs: number[];
  paragraphs: RefinedParagraph[];
  entities?: Entity[];
  /** Omitted by schema-v1 documents. */
  graph_extraction?: GraphExtraction;
  /** Omitted by schema-v1 documents. */
  relations?: RelationFact[];
  /** 用户编辑保存的乐观并发版本号;历史文档缺省 0(后端 serde default)。 */
  revision?: number;
  /** 仅供旧图谱关系保持结构完整的 mention id;不是 live mention,UI 必须过滤。 */
  graph_support_mentions?: string[];
  /** 文件重转写(三期)后段落已变但本修订稿仍是旧文本;详情页据此提示重新执行 AI。 */
  stale?: boolean;
}

/** Required graph fields for schema-v2 writes; `RefinedDoc` remains permissive for legacy reads. */
export type MentionV2 = Omit<Mention, "id"> & { id: string };
export type RefinedParagraphV2 = Omit<RefinedParagraph, "mentions"> & { mentions: MentionV2[] };
export type RefineStagesV2 = Omit<RefineStages, "relations"> & { relations: string };
export type RelationEvidenceV2 = Omit<RelationEvidence, "id" | "source_seqs" | "source_hash"> & {
  id: string;
  source_seqs: number[];
  source_hash: string;
};
export type RelationFactV2 = Omit<RelationFact, "id" | "subject_mentions" | "object_mentions" | "evidence"> & {
  id: string;
  subject_mentions: string[];
  object_mentions: string[];
  evidence: RelationEvidenceV2[];
};
export type RefinedDocV2 = Omit<
  RefinedDoc,
  "schema_version" | "stages" | "paragraphs" | "graph_extraction" | "relations"
> & {
  schema_version: 2;
  stages: RefineStagesV2;
  paragraphs: RefinedParagraphV2[];
  graph_extraction: GraphExtraction | null;
  relations: RelationFactV2[];
};

/** 按 char 下标把段落文本切成 { 普通片段 | 实体片段 } 序列(实体片段 entityId 非空)。
 *  用 Array.from 按 code point 切分(BMP 中文一致、astral 安全);mentions 排序 + 跳过重叠/越界。 */
export function splitMentions(
  text: string,
  mentions?: Mention[],
): { text: string; entityId: string | null }[] {
  const chars = Array.from(text);
  const valid = (mentions ?? [])
    .filter((m) => Number.isInteger(m.start) && Number.isInteger(m.end) && m.start >= 0 && m.end <= chars.length && m.start < m.end)
    .sort((a, b) => a.start - b.start || b.end - a.end);
  const out: { text: string; entityId: string | null }[] = [];
  let cur = 0;
  for (const m of valid) {
    if (m.start < cur) continue; // 与已产出区间重叠 → 跳过
    if (m.start > cur) out.push({ text: chars.slice(cur, m.start).join(""), entityId: null });
    out.push({ text: chars.slice(m.start, m.end).join(""), entityId: m.entity });
    cur = m.end;
  }
  if (cur < chars.length) out.push({ text: chars.slice(cur).join(""), entityId: null });
  if (out.length === 0) out.push({ text, entityId: null });
  return out;
}

export interface RelatedNote {
  id: string;
  title: string;
  started_at: string;
  shared_entities: number;
}
export const noteRelated = (id: string) => invoke<RelatedNote[]>("note_related", { id });

/** P3 日历改选候选(按重叠降序;零重叠也列出,覆盖延迟开录)。 */
export type CalendarCandidate = {
  event_id: string;
  title: string;
  start_ms: number;
  end_ms: number;
  attendee_n: number;
  overlap_ms: number;
};
export const listCalendarCandidates = (id: string) =>
  invoke<CalendarCandidate[]>("list_calendar_candidates", { id });
/** 改选(eventId)或清除(null,立 tombstone:自动匹配不再复活)。 */
export const setNoteCalendarEvent = (id: string, eventId: string | null) =>
  invoke<void>("set_note_calendar_event", { id, eventId });
export const noteCalendarPermission = () => invoke<string>("calendar_permission");
/** 手动触发/重试说话人身份推断(P2a):完成后 identify_done 事件驱动收件箱刷新。 */
export const identifyNote = (id: string) => invoke<void>("identify_note", { id });

export const listNotes = () => invoke<NoteSummary[]>("list_notes");
/** 笔记音频轨道;无音频(旧笔记/写失败)返回空数组。 */
export const noteAudioInfo = (id: string) => invoke<TrackInfo[]>("note_audio_info", { id });
export const getNote = (id: string) => invoke<Note>("get_note", { id });
export const renameNote = (id: string, title: string) =>
  invoke<void>("rename_note", { id, title });
export const deleteNote = (id: string) => invoke<void>("delete_note", { id });
export const resumeRecording = (noteId: string) => invoke<void>("resume_recording", { noteId });
/** 删除笔记内说话人:表项移除,名下段落回到未标注。只动本笔记,不碰人物库。 */
export const deleteNoteSpeaker = (noteId: string, speakerId: string) =>
  invoke<void>("delete_note_speaker", { noteId, speakerId });
export const renameSpeaker = (noteId: string, speakerId: string, name: string) =>
  invoke<void>("rename_speaker", { noteId, speakerId, name });
/** 导出到用户选定路径(保存对话框),返回落盘绝对路径。preferRefined=真且修订稿
 * 在盘时导修订稿(所见即所得)。 */
export const exportNote = (id: string, format: "md" | "txt", preferRefined: boolean, dest: string) =>
  invoke<string>("export_note", { id, format, preferRefined, dest });

/** 保存对话框的默认文件名:{标题}-{YYYYMMDD-HHmm}.md。
 * 时间直接取 started_at 字符串的墙钟分量(不经 Date):录音的"名义时间"就是写进
 * ISO 串的本地时间,经 Date 换算会随导出机器时区漂移;解析失败则省略时间段。
 * 标题清洗:路径非法字符(/\:*?"<>|)换 '-',控制字符与双向覆盖符(文件名视觉
 * 欺骗)删除,首部点(隐藏文件)与尾部点/空白(Windows 不允许)剥掉;标题截到
 * 160 字节 UTF-8 边界(文件系统文件名上限 255 字节,给时间段+扩展名留量,超长
 * CJK 标题不截会让保存对话框直接拒收);Windows 保留设备名(CON/PRN/…)加尾缀
 * 避让;清洗后无实义字符兜底「未命名」。 */
export function exportFileName(title: string, startedAt: string): string {
  let clean = title
    .replace(/[/\\:*?"<>|]/g, "-")
    .replace(/[\u0000-\u001f\u007f\u200e\u200f\u202a-\u202e\u2066-\u2069]/g, "")
    .trim()
    .replace(/^\.+|[.\s]+$/g, "");
  const enc = new TextEncoder();
  if (enc.encode(clean).length > 160) {
    let bytes = 0;
    let cut = 0;
    for (const ch of clean) {
      const b = enc.encode(ch).length;
      if (bytes + b > 160) break;
      bytes += b;
      cut += ch.length;
    }
    clean = clean.slice(0, cut);
  }
  if (!/[^-\s]/.test(clean)) clean = t("notes.untitled");
  const m = startedAt.match(/^(\d{4})-(\d{2})-(\d{2})[T ](\d{2}):(\d{2})/);
  const time = m ? `-${m[1]}${m[2]}${m[3]}-${m[4]}${m[5]}` : "";
  let stem = `${clean}${time}`;
  if (/^(con|prn|aux|nul|com[1-9]|lpt[1-9])$/i.test(stem)) stem += "_";
  return `${stem}.md`;
}
export const getRefined = (id: string) => invoke<RefinedDoc | null>("get_refined", { id });
export const refineNote = (id: string) => invoke<void>("refine_note", { id });
/** 发起文件重转写(破坏性:覆盖原始逐字稿,后端自动备份)。input: "dual" | "mixed"。 */
export const retranscribeNote = (id: string, input: "dual" | "mixed") =>
  invoke<void>("retranscribe_note", { id, input });
/** 当前重转写任务;空闲 null。挂载时回填(事件只覆盖在页期间)。 */
export const retranscribeStatus = () =>
  invoke<{ note_id: string; stage: string } | null>("retranscribe_status");
/** 成品轨入口可用性:null 可用;字符串为置灰原因。 */
export const mixedInputStatus = (id: string) => invoke<string | null>("mixed_input_status", { id });
/** 回放消费侧一站式读数(二期 A/B 切换):成品轨 + 可信性 + seek 修正表 + 口径告警。 */
export interface MixedPlaybackInfo {
  /** null = 无成品轨(给「生成成品轨」动作)。 */
  track: TrackInfo | null;
  /** 非 null = 有轨但不可信(置灰 + tooltip 原因)。 */
  untrusted: string | null;
  /** 各源段落 seek 到 mixed 的修正量(ms);空表 = 无需修正(regen/旧轨)。 */
  seek_offset_ms: Record<string, number>;
  /** mic 轨经过离线回声清洗:A 侧多一级抑制,A/B 听感不可直比。 */
  ab_caveat: boolean;
}
export const mixedPlaybackInfo = (id: string) =>
  invoke<MixedPlaybackInfo>("mixed_playback_info", { id });
/** 离线补生成成品轨(非破坏:源轨只读,mixed 原子替换)。进度走 "mixed_regen" 事件。 */
export const regenerateMixed = (id: string) => invoke<void>("regenerate_mixed", { id });
/** 正在补生成的 note_id;空闲 null。挂载时回填(事件只覆盖在页期间)。 */
export const mixedRegenStatus = () => invoke<string | null>("mixed_regen_status");
/** 修订稿说话人改名;该说话人已关联库人物时,声纹库(会议搭子)现名一并同步。 */
export const renameRefinedSpeaker = (noteId: string, speakerId: string, name: string) =>
  invoke<void>("rename_refined_speaker", { noteId, speakerId, name });
/** 把修订稿说话人关联到声纹库人物(会议搭子选人),采用库中现名。 */
export const assignRefinedPerson = (noteId: string, speakerId: string, personId: string) =>
  invoke<void>("assign_refined_person", { noteId, speakerId, personId });
/** 原始稿说话人关联声纹库人物:speakers.json 写 person_id 并清本地名(join 显库名)。 */
export const assignNoteSpeakerPerson = (noteId: string, speakerId: string, personId: string) =>
  invoke<void>("assign_note_speaker_person", { noteId, speakerId, personId });

/** speakerLabel/speakerColor 共用的说话人元数据形状(录制态 SpeakerMap 与
    Note.speakers 都满足)。person_id 是全局声纹库人物 id(P<n>)。 */
export type SpeakerMetaLite = { name?: string; person_id?: string | null };

/** 显示名:名字 > 全局编号「说话人 N」(N = 声纹库 P 号,跨笔记恒定) >
    「新说话人 N」(尚未够料入库的过渡态,N = 本场簇号);null → 按来源 我/对方 */
export function speakerLabel(
  speaker: string | null,
  source: Source,
  speakers: Record<string, SpeakerMetaLite>,
): string {
  if (!speaker) return source === "mic" ? t("notes.speaker.me") : t("notes.speaker.other");
  const meta = speakers[speaker];
  if (meta?.name) return meta.name;
  if (meta?.person_id) return t("notes.speaker.n", { n: meta.person_id.replace(/^P/, "") });
  // 修订稿重聚类标签(R1..Rk):终稿命名空间,不叫"新说话人"(它是全场收敛结果而非新面孔)
  if (/^R\d+$/.test(speaker)) return t("notes.speaker.n", { n: speaker.slice(1) });
  return t("notes.speaker.newN", { n: speaker.replace(/^S/, "") });
}
/** 稳定调色板:S1..Sn 循环取色;非 S<n> 形态 id 用字符串散列兜底(哈希逻辑不变)。
    调色板换成 DESIGN.md 粉彩 7 色，返回 CSS 变量引用——随 :root 的亮/暗色定义
    自动换色。 */
const PALETTE = [
  "var(--tint-sky)",
  "var(--tint-mint)",
  "var(--tint-peach)",
  "var(--tint-lavender)",
  "var(--tint-rose)",
  "var(--tint-yellow)",
  "var(--tint-gray)",
];
/** 与 PALETTE 同索引的文字色(soft 底配同色相文字:亮色深文字/暗色亮文字,Raycast soft 公式)。 */
const SPEAKER_INKS = [
  "var(--tint-sky-ink)",
  "var(--tint-mint-ink)",
  "var(--tint-peach-ink)",
  "var(--tint-lavender-ink)",
  "var(--tint-rose-ink)",
  "var(--tint-yellow-ink)",
  "var(--tint-gray-ink)",
];
/** 说话人 id → 调色板索引:S<n>/P<n> 数值循环;其余形态用字符串散列兜底。
    speakerColor/speakerInk 共用,保证背景色与文字色永远同色相。 */
function speakerIndex(speaker: string): number {
  const n = parseInt(speaker.replace(/^[SP]/, ""), 10);
  if (Number.isFinite(n) && n > 0) return (n - 1) % PALETTE.length;
  let h = 0;
  for (const c of speaker) h = (h * 31 + c.charCodeAt(0)) >>> 0;
  return h % PALETTE.length;
}
/** 取色键:已关联全局人物按 P 号取色(同一个人跨笔记同色),否则按本场簇号。 */
function speakerColorKey(
  speaker: string,
  speakers?: Record<string, SpeakerMetaLite>,
): string {
  return speakers?.[speaker]?.person_id || speaker;
}
export function speakerColor(
  speaker: string | null,
  source: Source,
  speakers?: Record<string, SpeakerMetaLite>,
): string {
  if (!speaker) return source === "mic" ? "var(--tint-sky)" : "var(--tint-mint)";
  return PALETTE[speakerIndex(speakerColorKey(speaker, speakers))];
}
/** 徽章文字色:与 speakerColor 同索引(soft 底配同色相文字,Raycast soft 公式)。 */
export function speakerInk(
  speaker: string | null,
  source: Source,
  speakers?: Record<string, SpeakerMetaLite>,
): string {
  if (!speaker) return source === "mic" ? "var(--tint-sky-ink)" : "var(--tint-mint-ink)";
  return SPEAKER_INKS[speakerIndex(speakerColorKey(speaker, speakers))];
}

/** 00:01:23 */
export function formatTs(ms: number): string {
  const s = Math.floor(ms / 1000);
  const pad = (n: number) => String(n).padStart(2, "0");
  return `${pad(Math.floor(s / 3600))}:${pad(Math.floor((s % 3600) / 60))}:${pad(s % 60)}`;
}

/** 1 小时 8 分 / 12 分 3 秒 / 45 秒 */
export function formatDuration(secs: number | null): string {
  if (secs == null) return "—";
  const h = Math.floor(secs / 3600);
  const m = Math.floor((secs % 3600) / 60);
  const s = secs % 60;
  if (h > 0) return t("notes.duration.hm", { h, m });
  if (m > 0) return t("notes.duration.ms", { m, s });
  return t("notes.duration.s", { s });
}

/** RFC3339 → "2026-07-03 15:04"；空串（元数据损坏）→ "—" */
export function formatDate(rfc3339: string): string {
  if (!rfc3339) return "—";
  const d = new Date(rfc3339);
  if (isNaN(d.getTime())) return "—";
  const pad = (n: number) => String(n).padStart(2, "0");
  return `${d.getFullYear()}-${pad(d.getMonth() + 1)}-${pad(d.getDate())} ${pad(d.getHours())}:${pad(d.getMinutes())}`;
}

export const editSegment = (noteId: string, seq: number, expectedText: string, newText: string) =>
  invoke<void>("edit_segment", { noteId, seq, expectedText, newText });
export const deleteSegment = (noteId: string, seq: number, expectedText: string) =>
  invoke<void>("delete_segment", { noteId, seq, expectedText });
/** 返回实际生效的 speaker id（speakerId="new" 时为后端分配的新 id） */
export const setSegmentSpeaker = (noteId: string, seq: number, expectedText: string, speakerId: string) =>
  invoke<string>("set_segment_speaker", { noteId, seq, expectedText, speakerId });

/** 说话人 id 排序：S2 < S10（数值序）；非 S<n> 形态沉底按字典序。 */
export function speakerIdCompare(a: string, b: string): number {
  const num = (id: string) => {
    const n = parseInt(id.replace(/^S/, ""), 10);
    return Number.isFinite(n) && n > 0 ? n : Number.MAX_SAFE_INTEGER;
  };
  return num(a) - num(b) || a.localeCompare(b);
}

/** save_refined 载荷段落:orig_index 指向载入时 doc.paragraphs 下标,null=用户新插入块。 */
export interface ParagraphPayload {
  orig_index: number | null;
  text: string;
  dirty: boolean;
}

/** 整篇保存精修稿(WYSIWYG 编辑),revision 乐观并发,返回新 revision。 */
export const saveRefined = (noteId: string, revision: number, paragraphs: ParagraphPayload[]) =>
  invoke<number>("save_refined", { noteId, revision, paragraphs });
