import { invoke } from "@tauri-apps/api/core";

/** 声纹库人物摘要（对应后端 ipc::PersonSummary）。sources 是该人库里记录过的信道集合
    （"mic"/"system"），不代表"当前在场"。 */
export type PersonSummary = {
  id: string;
  name: string;
  total_ms: number;
  last_seen: string;
  sources: string[];
};

export const listPeople = () => invoke<PersonSummary[]>("list_people");
export const renamePerson = (id: string, name: string) => invoke<void>("rename_person", { id, name });
/** loser 并入 winner；录制中后端拒绝(报错文案原样透出)。 */
export const mergePerson = (loser: string, winner: string) =>
  invoke<void>("merge_person", { loser, winner });
export const deletePerson = (id: string) => invoke<void>("delete_person", { id });
