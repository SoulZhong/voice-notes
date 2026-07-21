export type GraphMode = "entity" | "note";

// 侧栏与画布共享视角、搜索和实体类型，切换视角时两边始终同步。
export class GraphFilterState {
  mode = $state<GraphMode>("entity");
  kind = $state("all");
  query = $state("");
}

export const graphFilter = new GraphFilterState();
