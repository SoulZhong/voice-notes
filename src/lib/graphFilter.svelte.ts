// 图谱只共享日常需要的搜索与实体类型筛选。高级关系查询留在索引层，不占用浏览界面。
export class GraphFilterState {
  kind = $state("all");
  query = $state("");
}

export const graphFilter = new GraphFilterState();
