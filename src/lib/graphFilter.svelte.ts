// 图谱浏览的共享筛选态:侧栏的搜索框/kind 过滤药丸、主区力导图三处同源——
// 药丸点的是"术语",画布上就该只剩术语节点,不是列表筛列表、画布看画布两张皮。
class GraphFilterState {
  kind = $state("all");
  query = $state("");
}

export const graphFilter = new GraphFilterState();
