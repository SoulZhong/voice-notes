import { invoke } from "@tauri-apps/api/core";
import type { NoteSummary } from "$lib/notes";

/** 声纹库人物摘要（对应后端 ipc::PersonSummary）。sources 是该人库里记录过的信道集合
    （"mic"/"system"），不代表"当前在场"。 */
export type PersonSummary = {
  id: string;
  name: string;
  total_ms: number;
  last_seen: string;
  sources: string[];
  /** 录音样本绝对路径列表(按会议逐份累积,合并会带入对方的);空数组 = 无样本,不显示「试听」。 */
  sample_paths: string[];
  /** 与 sample_paths 一一对应的录制日期(文件 mtime,RFC3339;取不到为空串)。 */
  sample_dates: string[];
};

/** 后端已按 last_seen 降序返回。 */
export const listPeople = () => invoke<PersonSummary[]>("list_people");
/** 该人出现过的会议(扫笔记 person_id 引用,经合并重定向归一),按开始时间倒序。 */
export const personNotes = (personId: string) =>
  invoke<NoteSummary[]>("person_notes", { personId });
export const renamePerson = (id: string, name: string) => invoke<void>("rename_person", { id, name });
/** loser 并入 winner,返回合并日志 id(撤销用);录制中后端拒绝(报错文案原样透出)。 */
export const mergePerson = (loser: string, winner: string) =>
  invoke<string>("merge_person", { loser, winner });
export const deletePerson = (id: string) => invoke<void>("delete_person", { id });
/** 删除一份录音样本(试听纠错;样本不参与识别)。path 须取自该人的 sample_paths。 */
export const deletePersonSample = (id: string, path: string) =>
  invoke<void>("delete_person_sample", { id, path });

/** 整理·合并建议:把 loser 并入 winner 的推荐。similarity=共有信道质心余弦最大值;
    salience=S-Norm 显著性 z 分数(相对库内分布的鹤立鸡群程度,库太小为 null)。
    "很可能"判定见 tidy.svelte.ts isStrong;name 空串=未命名(展示按「说话人 N」兜底)。 */
export type PersonMergeSuggestion = {
  loser: string;
  loser_name: string;
  winner: string;
  winner_name: string;
  similarity: number;
  source: string;
  salience: number | null;
};
/** 整理·再辨认:未命名人物与库中其他人比对声纹,可归属者给出合并建议(纯推荐,
    不落修改;确认合并走 mergePerson)。 */
export const suggestPersonMerges = () =>
  invoke<PersonMergeSuggestion[]>("suggest_person_merges");

/** 合并回执(合并日志条目):invalid_reason 非空=不能再撤销(原因文案直接展示)。 */
export type MergeReceipt = {
  journal_id: string;
  time: string;
  origin: "auto" | "manual";
  loser: string;
  loser_name: string;
  winner: string;
  winner_name: string;
  similarity: number | null;
  /** 被并入方合并前的样本快照副本(绝对路径;空=无样本或已被永久失效清理)。 */
  loser_sample_paths: string[];
  /** winner 合并时刻的样本快照副本(绝对路径;核对历史合并要看快照,而非会随
      后续操作漂移的实时状态)。 */
  winner_sample_paths: string[];
  invalid_reason: string | null;
};
/** 整理·自动归并返回:本次合并的回执 + 留给人工的建议。 */
export type ConfidentMergeOutcome = {
  applied: MergeReceipt[];
  remaining: PersonMergeSuggestion[];
};
/** 高置信建议逐条落日志后自动合并;录制中只读算建议不动库。 */
export const applyConfidentMerges = () =>
  invoke<ConfidentMergeOutcome>("apply_confident_merges");
/** 未确认的自动归并回执(重启后仍在,直到「好」/撤销)。 */
export const listMergeReceipts = () => invoke<MergeReceipt[]>("list_merge_receipts");
/** 撤销一次合并(按日志 id);已失效/录制中后端拒绝。 */
export const undoMerge = (journalId: string) => invoke<void>("undo_merge", { journalId });
/** 回执卡「好」:确认自动归并,条目删除。 */
export const acknowledgeMerge = (journalId: string) =>
  invoke<void>("acknowledge_merge", { journalId });
/** 整理条目人工处置(忽略/保留)落盘:重启后不再出现。key 为 tidyItemKey 格式。 */
export const dismissTidyItem = (key: string) => invoke<void>("dismiss_tidy_item", { key });
/** 已落盘的处置键全量(重启后合并进本地已忽略集合)。 */
export const listDismissedTidyItems = () => invoke<string[]>("list_dismissed_tidy_items");
