// 多人混杂:打标(quarantine_only)的 IPC 包装与类型。
// 设计:docs/superpowers/specs/2026-08-20-mixed-speaker-split-design.md
import { invoke } from "@tauri-apps/api/core";

export type SplitOp = {
  op_id: string;
  mode: string;
  note_id: string;
  speaker_ids: string[];
  affected_persons: string[];
  phase: string; // plan|marked|samples_handled|residual_decided|released|done
  residual_choice?: string | null;
  samples_confirm_seen: boolean;
  created_at: string;
  updated_at: string;
};

export type MultiImpactSample = {
  path: string;
  audition_path: string;
  from_marked_cluster: boolean;
};

export type MultiImpactPerson = {
  person_id: string;
  name: string;
  cluster_count_est: number;
  person_count_total: number;
  has_session_centroid: boolean;
  total_ms: number;
  last_seen: string;
  samples: MultiImpactSample[];
};

export type MultiImpactReport = {
  op_id: string;
  phase: string;
  persons: MultiImpactPerson[];
};

/** 打标:speakerIds 是原始稿 S 编号(修订稿 R 由调用方先映射成 S)。返回 op_id。 */
export const markSpeakerMulti = (noteId: string, speakerIds: string[]) =>
  invoke<string>("mark_speaker_multi", { noteId, speakerIds });

export const multiImpact = (opId: string) => invoke<MultiImpactReport>("multi_impact", { opId });

/** 样本处置:可归因的自动删,extraDelete 是用户试听后勾选的相对路径。 */
export const confirmMultiSamples = (opId: string, extraDelete: string[], confirmSeen: boolean) =>
  invoke<number>("confirm_multi_samples", { opId, extraDelete, confirmSeen });

/** 残留二选一:accept=质心不动;baseline=逐人重算(退回样本基线)。
    thenSplit=true 时进入拆分模式(隔离暂不解除,由 commitSplit/cancelSplit 收尾)。 */
export const resolveMultiResidual = (opId: string, choice: "accept" | "baseline", thenSplit = false) =>
  invoke<void>("resolve_multi_residual", { opId, choice, thenSplit });

export type SplitSuggestGroup = {
  seqs: number[];
  total_ms: number;
  suggested: [string, string, number] | null; // (person_id, name, cosine)
};
export type SplitSuggestOut = { groups: SplitSuggestGroup[]; undetermined: number[] };

export type SplitGroupIn = {
  seqs: number[];
  dest_kind: "existing_speaker" | "person" | "new_speaker" | "keep";
  dest_id: string | null;
};

/** 建议分组(拆分专用聚类,纯本地纯读)。重活在后端 spawn_blocking。 */
export const suggestSplitGroups = (opId: string) =>
  invoke<SplitSuggestOut>("suggest_split_groups", { opId });

/** 提交拆分(按阶段续跑;返回回灌的如实结果摘要,空串=全部成功)。 */
export const commitSplit = (opId: string, groups: SplitGroupIn[]) =>
  invoke<string>("commit_split", { opId, groups });

/** 取消拆分(段落改派前可取消;隔离照常解除)。 */
export const cancelSplit = (opId: string) => invoke<void>("cancel_split", { opId });

export const listSplitOps = (noteId: string) => invoke<SplitOp[]>("list_split_ops", { noteId });
