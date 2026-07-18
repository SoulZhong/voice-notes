import { invoke } from "@tauri-apps/api/core";

/** 图谱实体摘要(列表 / 力导图节点)。镜像 ipc::EntitySummary。 */
export interface EntitySummary {
  id: string;
  kind: string;
  name: string;
  aliases: string[];
  is_person: boolean;
  note_count: number;
  mention_total: number;
}

/** 一条共现边(a<b,weight=共享笔记数)。镜像 ipc::EdgeRow。 */
export interface EdgeRow {
  a: string;
  b: string;
  weight: number;
}

/** 力导图数据(Plan C 用)。镜像 ipc::GraphData。 */
export interface GraphData {
  nodes: EntitySummary[];
  edges: EdgeRow[];
}

/** 详情面板「出现的笔记」一项(已联查标题)。镜像 ipc::EntityNoteRef。 */
export interface EntityNoteRef {
  id: string;
  title: string;
  started_at: string;
  mention_count: number;
}

/** 详情面板「共现实体」一项。镜像 ipc::RelatedEntity。 */
export interface RelatedEntity {
  id: string;
  kind: string;
  name: string;
  shared_notes: number;
}

/** 实体详情(右侧面板)。镜像 ipc::EntityDetail。 */
export interface EntityDetail {
  id: string;
  kind: string;
  name: string;
  aliases: string[];
  is_person: boolean;
  note_count: number;
  mention_total: number;
  notes: EntityNoteRef[];
  related: RelatedEntity[];
}

/** 笔记页高亮点击导航:局部实体→全局 id。镜像 ipc::EntityLink(Plan C 笔记页消费)。 */
export interface EntityLink {
  local_id: string;
  global_id: string;
  is_person: boolean;
}

const KIND_LABELS: Record<string, string> = {
  person: "人",
  org: "组织",
  project: "项目",
  product: "产品",
  term: "术语",
  decision: "决议",
  task: "任务",
  place: "地点",
  date: "日期",
};

/** kind→中文标签;未知 kind 原样返回(不吞大模型新造的类型)。 */
export function kindLabel(kind: string): string {
  return KIND_LABELS[kind] ?? kind;
}

/** 全部实体(列表),note_count 降序。图谱失败/空 → []。 */
export const graphEntities = () => invoke<EntitySummary[]>("graph_entities");
/** 力导图数据(Plan C)。 */
export const graphData = () => invoke<GraphData>("graph_data");
/** 单实体详情;不存在/失败 → null。 */
export const entityDetail = (id: string) => invoke<EntityDetail | null>("entity_detail", { id });
/** 笔记局部实体→全局 id(Plan C 笔记页)。 */
export const noteEntityLinks = (id: string) => invoke<EntityLink[]>("note_entity_links", { id });

/** 改实体名结果:new_id=改后的规范 id(人实体不变);merged=撞已存在实体自动合并了。 */
export interface RenameEntityResult {
  new_id: string;
  merged: boolean;
}
/** 改实体显示名(纠 ASR 提取错的名字)。人实体委托声纹库、id 不变;非人实体 id 随名字
    重算,撞已存在实体自动合并。 */
export const renameEntity = (id: string, newName: string) =>
  invoke<RenameEntityResult>("rename_entity", { id, newName });
