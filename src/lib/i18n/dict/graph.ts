import type { Dict, Msg } from "../types";

// graph 领域文案分片。键一律以 "graph." 前缀命名,分片之间不得重键(有测试哨兵)。
// 覆盖:/graph 路由页、ForceGraph 画布、KnowledgeGraphToolbar、KnowledgePathPanel、
// GraphEdgeInspector,以及 graph.ts(kind 标签)与 knowledgeView.ts(谓词标签/降级文案)。
export const zh = {
  // graph.ts:实体 kind→展示标签(未知 kind 原样返回,不进字典)
  "graph.kind.person": "人",
  "graph.kind.org": "组织",
  "graph.kind.project": "项目",
  "graph.kind.product": "产品",
  "graph.kind.term": "术语",
  "graph.kind.decision": "决议",
  "graph.kind.task": "任务",
  "graph.kind.place": "地点",
  "graph.kind.date": "日期",

  // knowledgeView.ts:核心谓词→展示标签
  "graph.predicate.participates_in": "参与",
  "graph.predicate.responsible_for": "负责",
  "graph.predicate.belongs_to": "属于",
  "graph.predicate.uses": "使用",
  "graph.predicate.depends_on": "依赖",
  "graph.predicate.produces": "产生",
  "graph.predicate.assigned_to": "指派给",
  "graph.predicate.occurs_at": "发生于",

  // 共现边标签(实体图=共享笔记数,文章图=共用实体数)
  "graph.edge.sharedNotes": "{n} 篇笔记同时提到",
  "graph.edge.sharedEntities": "{n} 个共用实体",

  // knowledgeView.ts:语义请求失败降级文案
  "graph.semantic.failedWithLegacy": "语义关系暂时无法读取，已显示可用的共现关系。请稍后重试。",
  "graph.semantic.failedNoLegacy": "语义关系暂时无法读取，当前没有可用的备用关系图。请稍后重试。",

  // /graph 路由页:状态条/横幅
  "graph.semantic.degraded": "语义关系服务暂时降级，当前显示可用结果。",
  "graph.retryRead": "重新读取",
  "graph.backfill.analyze": "分析笔记关系",
  "graph.fallback.notice": "还没有分析笔记之间的具体关系，目前先按共同提到的内容连接。",
  "graph.filteredEmpty.notice": "当前筛选下没有语义关系，图谱没有切换为旧版共现结果。",
  "graph.filteredEmpty.reset": "重置图谱筛选",
  "graph.debug.banner": "隔离调试模式 · 仅创建并读取临时夹具，不会读取或修改真实资料库",
  "graph.debug.fixtureSize": "1,000 个实体 / 5,000 条语义关系",
  "graph.debug.loadFailed": "隔离调试夹具加载失败。请重新打开调试地址。",

  // /graph 路由页:占位(文章视角)
  "graph.notePh.noEdgesTitle": "笔记之间还没有连接",
  "graph.notePh.noEdgesDesc": "当两篇笔记提到同一个实体时，它们会在这里连成一条边。",
  "graph.notePh.loadingTitle": "文章视角",
  "graph.notePh.loadingDesc": "正在按共享实体加载笔记关系。",
  "graph.notePh.errorTitle": "文章图谱加载失败",
  "graph.notePh.errorDesc": "图谱索引失败不会影响笔记内容。",
  "graph.notePh.reload": "重新加载文章图谱",
  "graph.notePh.emptyTitle": "还没有进入图谱的笔记",
  "graph.notePh.emptyDesc": "对笔记重新执行 AI 后会按共享实体建立连接。",

  // /graph 路由页:占位(实体视角)
  "graph.emptyPh.title": "还没有知识图谱",
  "graph.emptyPh.desc": "配置大模型并对笔记重新执行 AI 后，人物、组织、项目等实体会汇入这里。",
  "graph.emptyPh.goAi": "前往 AI 设置",
  "graph.noMatchPh.title": "没有匹配的实体关系",
  "graph.noMatchPh.desc": "当前类型下没有可连接的关系。选择全部类型，或从侧栏搜索其他实体。",
  "graph.noMatchPh.showAll": "显示全部类型",
  "graph.introPh.title": "知识图谱",
  "graph.introPh.desc": "从实体名称开始，沿完整关系逐层探索会议上下文。",

  // /graph 路由页:检查器与实体详情
  "graph.canvasAria": "知识图谱画布",
  "graph.noteInspectorAria": "文章连接详情",
  "graph.governanceInspectorAria": "知识治理检查器",
  "graph.closeEntityAria": "关闭实体治理检查器",
  "graph.detail.loading": "正在读取实体治理信息",
  "graph.detail.unavailable": "实体治理信息暂不可用。",
  "graph.detail.retry": "重试读取实体信息",
  "graph.detail.notIndexed": "该实体还未进入当前语义索引，请稍后刷新。",
  "graph.detail.loadFailed": "实体治理信息读取失败：{e}",

  // /graph 路由页:节点右键菜单
  "graph.ctx.aria": "治理 {name}",
  "graph.ctx.viewDetail": "查看实体详情",
  "graph.ctx.manage": "管理实体",

  // ForceGraph:画布与节点/边
  "graph.svgAria": "知识图谱力导向图",
  "graph.unnamed": "未命名",
  "graph.nodeAria": "{name}，{kind}",
  "graph.edgeAria.semantic": "{label}，点击查看关系证据",
  "graph.edgeAria.sharedEntities": "{label}，点击查看共用实体",
  "graph.edgeAria.sharedNotes": "{label}，点击查看共同出现的笔记",
  "graph.zoomHint": "点击节点逐层展开 · 滚轮放大显示更多名称",

  // ForceGraph:关系线说明(edge key)与 kind 图例
  "graph.edgeKey.aria": "关系线说明",
  "graph.edgeKey.semantic": "明确关系",
  "graph.edgeKey.semanticDesc": "箭头表示方向 · 点击线查看依据",
  "graph.edgeKey.sharedEntities": "共用实体",
  "graph.edgeKey.sharedEntitiesDesc": "细线表示两篇文章含有相同实体",
  "graph.edgeKey.cooccur": "同时提及",
  "graph.edgeKey.cooccurDesc": "细线表示一篇笔记提到两个实体",
  "graph.legend.person": "人 · 按个人上色",

  // ForceGraph:规模控制条(收起/展开/显示全部)
  "graph.trunc.all": "已显示全部 {n} 个实体",
  "graph.expanded": "已展开 {hops} 层 · 共 {n} 个实体",
  "graph.trunc.top": "显示连接最紧的 {n} 个实体",
  "graph.trunc.collapse": "收起",
  "graph.trunc.expandOne": "展开一层",
  "graph.trunc.expandMore": "继续展开",
  "graph.trunc.showAll": "显示全部",

  // KnowledgeGraphToolbar
  "graph.toolbar.aria": "知识图谱筛选与视图控制",
  "graph.toolbar.entityKinds": "实体类型",
  "graph.toolbar.predicates": "关系类型",
  "graph.toolbar.noPredicates": "还没有可筛选的具体关系。完成笔记关系分析后会显示在这里。",
  "graph.toolbar.more": "更多",
  "graph.toolbar.enabled": "已启用",
  "graph.toolbar.from": "开始日期",
  "graph.toolbar.to": "结束日期",
  "graph.toolbar.includeHistory": "包含历史关系",
  "graph.toolbar.includeCooccur": "显示共同出现的弱连接",
  "graph.toolbar.updating": "正在更新关系",
  "graph.toolbar.count": "{visible} / {total} 个实体",
  "graph.toolbar.reset": "重置 {n} 项筛选",
  "graph.toolbar.collapse": "收起到主干",

  // KnowledgePathPanel
  "graph.path.aria": "两点关系路径",
  "graph.path.notChosen": "尚未选择",
  "graph.path.startMark": "起",
  "graph.path.endMark": "终",
  "graph.path.clear": "清除路径",
  "graph.path.includeCooccur": "包含共现弱连接",
  "graph.path.choosing": "再选择一个实体作为终点，画布不会隐藏其他关系。",
  "graph.path.loading": "正在沿当前筛选寻找可解释路径",
  "graph.path.errorFallback": "路径读取失败。请检查筛选条件后重试。",
  "graph.path.empty": "未找到可连接两点的路径。可以放宽关系类型或启用共现弱连接。",
  "graph.path.cooccurStep": "共同出现（{n} 篇）",
  "graph.path.forward": "正向",
  "graph.path.reverse": "逆向",
  "graph.path.provenance": "{origin} · 置信度 {pct}% · {n} 条证据",
  "graph.path.origin.model": "模型提取",
  "graph.path.origin.confirmed": "人工确认",
  "graph.path.origin.manual": "人工建立",
  "graph.path.origin.user_assertion": "用户声明",
  "graph.path.origin.cooccurrence": "共现弱连接",
  "graph.path.viewEvidence": "查看关系证据",

  // GraphEdgeInspector(共现边连接详情)
  "graph.edgeDetail.eyebrow": "连接详情",
  "graph.edgeDetail.sharedEntities": "共用实体",
  "graph.edgeDetail.sharedNotes": "共同出现的笔记",
  "graph.edgeDetail.closeAria": "关闭连接详情",
  "graph.edgeDetail.endpointsAria": "连接两端",
  "graph.edgeDetail.loading": "正在读取连接内容",
  "graph.edgeDetail.countEntities": "{n} 个实体",
  "graph.edgeDetail.countNotes": "{n} 篇笔记",
  "graph.edgeDetail.headingNote": "两篇笔记都提到了",
  "graph.edgeDetail.headingEntity": "两个实体都出现在",
  "graph.edgeDetail.changed": "这条连接已变化，请刷新图谱后再试。",
  "graph.edgeDetail.loadFailed": "连接内容读取失败：{e}",
} as const satisfies Dict;

export const en = {
  "graph.kind.person": "Person",
  "graph.kind.org": "Organization",
  "graph.kind.project": "Project",
  "graph.kind.product": "Product",
  "graph.kind.term": "Term",
  "graph.kind.decision": "Decision",
  "graph.kind.task": "Task",
  "graph.kind.place": "Place",
  "graph.kind.date": "Date",

  "graph.predicate.participates_in": "Participates in",
  "graph.predicate.responsible_for": "Responsible for",
  "graph.predicate.belongs_to": "Belongs to",
  "graph.predicate.uses": "Uses",
  "graph.predicate.depends_on": "Depends on",
  "graph.predicate.produces": "Produces",
  "graph.predicate.assigned_to": "Assigned to",
  "graph.predicate.occurs_at": "Occurs at",

  "graph.edge.sharedNotes": (p) => `Mentioned together in ${p.n} note${p.n === 1 ? "" : "s"}`,
  "graph.edge.sharedEntities": (p) => `${p.n} shared ${p.n === 1 ? "entity" : "entities"}`,

  "graph.semantic.failedWithLegacy":
    "Semantic relations are temporarily unavailable; showing available co-occurrence relations. Please try again later.",
  "graph.semantic.failedNoLegacy":
    "Semantic relations are temporarily unavailable and no fallback graph is available. Please try again later.",

  "graph.semantic.degraded": "The semantic relation service is temporarily degraded; showing available results.",
  "graph.retryRead": "Retry",
  "graph.backfill.analyze": "Analyze note relations",
  "graph.fallback.notice":
    "Note relations haven't been analyzed yet; for now, connections come from shared mentions.",
  "graph.filteredEmpty.notice":
    "No semantic relations match the current filters; the graph did not fall back to legacy co-occurrence.",
  "graph.filteredEmpty.reset": "Reset graph filters",
  "graph.debug.banner":
    "Isolated debug mode · only creates and reads a temporary fixture; your real library is never read or modified",
  "graph.debug.fixtureSize": "1,000 entities / 5,000 semantic relations",
  "graph.debug.loadFailed": "Failed to load the isolated debug fixture. Please reopen the debug URL.",

  "graph.notePh.noEdgesTitle": "No connections between notes yet",
  "graph.notePh.noEdgesDesc": "When two notes mention the same entity, an edge will connect them here.",
  "graph.notePh.loadingTitle": "Note view",
  "graph.notePh.loadingDesc": "Loading note relations by shared entities.",
  "graph.notePh.errorTitle": "Failed to load the note graph",
  "graph.notePh.errorDesc": "A graph index failure does not affect your notes.",
  "graph.notePh.reload": "Reload note graph",
  "graph.notePh.emptyTitle": "No notes in the graph yet",
  "graph.notePh.emptyDesc": "Re-run AI on your notes to build connections from shared entities.",

  "graph.emptyPh.title": "No knowledge graph yet",
  "graph.emptyPh.desc":
    "Configure an LLM and re-run AI on your notes; people, organizations, projects, and other entities will gather here.",
  "graph.emptyPh.goAi": "Open AI settings",
  "graph.noMatchPh.title": "No matching entity relations",
  "graph.noMatchPh.desc":
    "No connectable relations for the current types. Select all types, or search for other entities in the sidebar.",
  "graph.noMatchPh.showAll": "Show all types",
  "graph.introPh.title": "Knowledge graph",
  "graph.introPh.desc": "Start from an entity name and explore meeting context hop by hop along full relations.",

  "graph.canvasAria": "Knowledge graph canvas",
  "graph.noteInspectorAria": "Note connection details",
  "graph.governanceInspectorAria": "Knowledge governance inspector",
  "graph.closeEntityAria": "Close entity governance inspector",
  "graph.detail.loading": "Loading entity governance info",
  "graph.detail.unavailable": "Entity governance info is currently unavailable.",
  "graph.detail.retry": "Retry loading entity info",
  "graph.detail.notIndexed": "This entity is not in the semantic index yet. Please refresh later.",
  "graph.detail.loadFailed": "Failed to load entity governance info: {e}",

  "graph.ctx.aria": "Manage {name}",
  "graph.ctx.viewDetail": "View entity details",
  "graph.ctx.manage": "Manage entity",

  "graph.svgAria": "Knowledge graph force layout",
  "graph.unnamed": "Unnamed",
  "graph.nodeAria": "{name}, {kind}",
  "graph.edgeAria.semantic": "{label}, click to view relation evidence",
  "graph.edgeAria.sharedEntities": "{label}, click to view shared entities",
  "graph.edgeAria.sharedNotes": "{label}, click to view co-occurring notes",
  "graph.zoomHint": "Click a node to expand step by step · zoom in to reveal more names",

  "graph.edgeKey.aria": "Relation line legend",
  "graph.edgeKey.semantic": "Explicit relation",
  "graph.edgeKey.semanticDesc": "Arrow shows direction · click a line for evidence",
  "graph.edgeKey.sharedEntities": "Shared entities",
  "graph.edgeKey.sharedEntitiesDesc": "Thin lines link notes that mention the same entities",
  "graph.edgeKey.cooccur": "Co-mentioned",
  "graph.edgeKey.cooccurDesc": "Thin lines mean one note mentions both entities",
  "graph.legend.person": "People · colored per person",

  "graph.trunc.all": (p) => `Showing all ${p.n} entities`,
  "graph.expanded": (p) => `Expanded ${p.hops} hop${p.hops === 1 ? "" : "s"} · ${p.n} entities`,
  "graph.trunc.top": (p) => `Showing the ${p.n} most connected entities`,
  "graph.trunc.collapse": "Collapse",
  "graph.trunc.expandOne": "Expand one hop",
  "graph.trunc.expandMore": "Expand further",
  "graph.trunc.showAll": "Show all",

  "graph.toolbar.aria": "Knowledge graph filters and view controls",
  "graph.toolbar.entityKinds": "Entity types",
  "graph.toolbar.predicates": "Relation types",
  "graph.toolbar.noPredicates":
    "No specific relations to filter yet. They will appear here once note relation analysis completes.",
  "graph.toolbar.more": "More",
  "graph.toolbar.enabled": "on",
  "graph.toolbar.from": "Start date",
  "graph.toolbar.to": "End date",
  "graph.toolbar.includeHistory": "Include historical relations",
  "graph.toolbar.includeCooccur": "Show weak co-occurrence links",
  "graph.toolbar.updating": "Updating relations",
  "graph.toolbar.count": "{visible} / {total} entities",
  "graph.toolbar.reset": (p) => `Reset ${p.n} filter${p.n === 1 ? "" : "s"}`,
  "graph.toolbar.collapse": "Collapse to backbone",

  "graph.path.aria": "Path between two entities",
  "graph.path.notChosen": "Not selected",
  "graph.path.startMark": "A",
  "graph.path.endMark": "B",
  "graph.path.clear": "Clear path",
  "graph.path.includeCooccur": "Include weak co-occurrence",
  "graph.path.choosing": "Pick another entity as the endpoint; the canvas keeps other relations visible.",
  "graph.path.loading": "Finding an explainable path under the current filters",
  "graph.path.errorFallback": "Failed to load the path. Check your filters and try again.",
  "graph.path.empty":
    "No path connects these two entities. Try relaxing relation types or enabling weak co-occurrence.",
  "graph.path.cooccurStep": (p) => `Co-occur in ${p.n} note${p.n === 1 ? "" : "s"}`,
  "graph.path.forward": "forward",
  "graph.path.reverse": "reverse",
  "graph.path.provenance": (p) =>
    `${p.origin} · confidence ${p.pct}% · ${p.n} evidence item${p.n === 1 ? "" : "s"}`,
  "graph.path.origin.model": "Model extracted",
  "graph.path.origin.confirmed": "Manually confirmed",
  "graph.path.origin.manual": "Manually created",
  "graph.path.origin.user_assertion": "User asserted",
  "graph.path.origin.cooccurrence": "Weak co-occurrence",
  "graph.path.viewEvidence": "View relation evidence",

  "graph.edgeDetail.eyebrow": "Connection details",
  "graph.edgeDetail.sharedEntities": "Shared entities",
  "graph.edgeDetail.sharedNotes": "Co-occurring notes",
  "graph.edgeDetail.closeAria": "Close connection details",
  "graph.edgeDetail.endpointsAria": "Connection endpoints",
  "graph.edgeDetail.loading": "Loading connection contents",
  "graph.edgeDetail.countEntities": (p) => `${p.n} ${p.n === 1 ? "entity" : "entities"}`,
  "graph.edgeDetail.countNotes": (p) => `${p.n} note${p.n === 1 ? "" : "s"}`,
  "graph.edgeDetail.headingNote": "Both notes mention",
  "graph.edgeDetail.headingEntity": "Both entities appear in",
  "graph.edgeDetail.changed": "This connection has changed. Refresh the graph and try again.",
  "graph.edgeDetail.loadFailed": "Failed to load connection contents: {e}",
} satisfies Record<keyof typeof zh, Msg>;
