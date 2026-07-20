// 图谱浏览的共享筛选态:侧栏的搜索框/kind 过滤药丸/视角切换、主区力导图多处同源——
// 药丸点的是"术语",画布上就该只剩术语节点,不是列表筛列表、画布看画布两张皮。
export type GraphMode = "entity" | "note";
class GraphFilterState {
  /** 视角:实体(节点=实体,边=共现)/ 文章(节点=笔记,边=共享实体)。 */
  mode = $state<GraphMode>("entity");
  kind = $state("all");
  query = $state("");
}

export const graphFilter = new GraphFilterState();
