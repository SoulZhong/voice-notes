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
/** loser 并入 winner；录制中后端拒绝(报错文案原样透出)。 */
export const mergePerson = (loser: string, winner: string) =>
  invoke<void>("merge_person", { loser, winner });
export const deletePerson = (id: string) => invoke<void>("delete_person", { id });
/** 删除一份录音样本(试听纠错;样本不参与识别)。path 须取自该人的 sample_paths。 */
export const deletePersonSample = (id: string, path: string) =>
  invoke<void>("delete_person_sample", { id, path });
