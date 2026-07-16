# Aing Phase 2b — 图谱页 + 实体点击导航 架构设计

日期:2026-07-17
状态:设计已拍板(brainstorm 完成),待写 plan
上游:`2026-07-16-aing-pipeline-design.md`(整体架构);Phase 1 后端四期 + Phase 2a(笔记页实体高亮 + 相关笔记)已在 master。

## 背景

Aing 全局知识图谱后端(SQLite `entities` + `note_entities` 边)与实体解析已在 master;Phase 2a 让笔记页能看到实体高亮 + 相关笔记。**Phase 2b 让整张图谱可见、可导航**:新增「图谱」页(力导向可视化为主 + 实体浏览/搜索/过滤),并把实体点击导航全局打通(图谱节点、以及 Phase 2a 已铺的笔记页高亮都变可点)。

### 真机数据结论(2026-07-17,建设前已实测)

配好 HTTP 大模型(火山方舟)后重跑 Aing,实测两条真实笔记:

- 抽取可用:一条面试 43 实体 / 7 种 kind(person/org/project/term/product/date/tool),一条评审会 12 实体。图谱累计 55 实体、边已连。
- **实体有噪声**:夹带 ASR 碎渣(`V字`/`fast`/`三轮推演`/`测试接口`)。→ 图谱页必须靠排序/过滤压噪,不能平铺。
- **人实体多半连不到会议搭子**:模型抽出「王子璇」,声纹库那人叫「王紫璇」(子/紫 同音异写)。`resolve_global_id` 是精确规范名匹配 → 不匹配 → 退化为 `e:王子璇` 非人节点。→ 图谱以**非人实体居多**,「人→会议搭子」导航会经常打不中(这是 spec 已声明的「不做模糊合并」限制,不在本期修)。

这两条直接定调了下面的设计:**非人实体的浏览/关联是主价值,噪声治理靠排序+过滤+默认只画有边实体,人物导航是锦上添花不过度投入。**

## 目标 / 非目标

**目标**
- 新左侧页签「图谱」+ `/graph` 路由。
- 力导向**实体共现图**(节点=实体,边=在同一笔记共现)为主视图;可切换为可搜索/按 kind 过滤的实体列表。
- 点节点/列表项 → 右侧详情面板(名/kind/别名 + 出现的笔记 + 共现实体),不跳走。
- 实体点击导航全局:人→ `/speakers/[person_id]`,非人→ 图谱实体详情(深链)。笔记页 Phase 2a 高亮变可点。
- 无大模型 / 无实体时优雅空态引导。

**非目标**
- 实体↔实体**显式关系**抽取(本期只用共现,不建关系表)。
- 图谱的写操作(改名/合并实体)——归后续;人物合并仍走会议搭子。
- 会议搭子页增补「相关实体/相关笔记」——那是 Phase 2c。
- 实时增量(Phase 3)。
- 修复人实体同音异写匹配(模糊/别名归并)——已声明限制,不在本期。

## 关键决策(已拍板)

1. **图谱页 = 力导图为主**,列表为可切换的浏览/过滤视图(非主)。
2. **节点/边模型 = 纯实体图,边 = 共现**(两实体在同一笔记出现即连边,边权 = 共享笔记数)。笔记不作为节点(隐在边里;详情面板列出实体出现的笔记)。
3. **可视化技术 = `d3-force`(新增 npm 依赖,仅 force 模块做物理)+ SVG 渲染**(节点数中等,便于上 DESIGN token / hover / 点击;规模涨到几百节点再考虑换 canvas)。
4. **实体详情 = 图谱页内右侧面板**(不新建实体详情路由);笔记页点非人实体经 `/graph?e=<gid>` 深链打开该面板。
5. 沿用核心红线:所有图谱查询失败/空一律降级返回空,绝不 `Err`、绝不塌页(延续 Phase 2a `note_related` 的姿态)。

## 架构

### 1. 后端(照 `note_related` 模式:graph 查询纯函数 + `ipc` 可序列化镜像 + 命令降级返回空)

`graph::EntityRow` 及 `list_entities` / `entity_notes` / `related_notes` / `resolve_global_id` 已存在(`pub(crate)`,`EntityRow` 无 `Serialize`)。新增:

**graph 查询(`graph/mod.rs`,纯 SQL,带单测)**
- `cooccurrence_edges(data_root) -> Vec<(String, String, i64)>`:`note_entities` 自连接同 `note_id`,配对 `entity_id`(`a < b` 去重),`COUNT(DISTINCT note_id)` 为边权。
- `entity_detail(data_root, gid) -> EntityDetail`:实体行(名/kind/别名/note_count/mention_total)+ 出现笔记(`entity_notes` 联查标题)+ 共现实体 top-N(名/kind/共享笔记数)。
- `resolve_local_ids(data_root, note_id) -> Vec<(String local_id, String global_id, bool is_person)>`:读该笔记 `aing.json` 的 `entities`,逐个 `resolve_global_id`,给笔记页高亮点击用。

**Tauri 命令 + `ipc` 镜像结构(`ipc.rs`,`#[derive(Serialize)]`)**
- `graph_entities() -> Vec<ipc::EntitySummary>`:`list_entities` 镜像(id/kind/name/aliases/is_person/note_count/mention_total),已按 note_count 降序(顺带治「entities.name 首见胜依赖 read_dir 顺序」——图/列表统一走 SQL ORDER BY 稳定序)。
- `graph_data() -> ipc::GraphData { nodes: Vec<EntitySummary>, edges: Vec<EdgeRow{a,b,weight}> }`:力导图数据(`list_entities` + `cooccurrence_edges`)。
- `entity_detail(id: String) -> ipc::EntityDetail`。
- `note_entity_links(id: String) -> Vec<ipc::EntityLink{ local_id, global_id, is_person }>`:笔记页点击导航用。
- 全部命令:图谱失败 → 返回空 `GraphData`/空 `Vec`/`entity_detail` 空态,`Ok(...)` 不 `Err`。

### 2. 前端

**Sidebar(`lib/Sidebar.svelte`)**:在「会议搭子」「钩子」之间加「图谱」竖排页签;`tab` 派生加 `startsWith("/graph") → "graph"` 分支;点击 `goto("/graph")`。

**图谱页(`routes/graph/+page.svelte`)**
- 顶栏:搜索框 + kind 过滤 chips(按 `graph_data` 里实际出现的 kind 动态生成:全部/人/组织/项目/术语/产品/…)+ 图/列表视图切换。
- **图视图**:`d3-force` 力导向 + SVG 自绘节点/边。
  - 节点大小 = note_count(度量重要度);颜色 = kind(DESIGN ink 变体,克制单色系不花);人节点用会议搭子色点体系(`speakerColor`)。
  - **压噪/规模**:默认只画「有共现边」的实体(孤立单提及实体沉到列表不入图);节点按度数 top-N 封顶防糊,超出提示「用搜索查看更多」。hover 节点高亮其邻居、淡化其余。
  - 拖拽节点、点节点触发选中。
- **列表视图**:按 note_count 降序表(名 / kind / 几笔 / 几提及)+ 搜索过滤。
- **右侧详情面板**:选中实体 → 名/kind/别名 + 出现笔记列表(点开 `/notes/[id]`)+ 共现实体(点选中切换)。**人实体节点点击直接 `goto("/speakers/[id]")`**(不进面板)。
- **深链** `/graph?e=<gid>`:进页自动选中并打开该实体面板(图里聚焦该节点)。

**笔记页高亮可点(`routes/notes/[id]/+page.svelte`)**:Phase 2a 的 `<span class="entity-mention">` 变可点。onMount(或数据就绪后)调 `note_entity_links(id)` 得 `local ent_N → {global_id, is_person}` 映射;点击:人→ `/speakers/[id]`,非人→ `/graph?e=<gid>`。无映射(旧笔记/无图谱)时保持纯文本不可点,不报错。

### 3. 空态与降级
- 无实体(没配大模型 / 该库还没 Aing 过):图谱页显示引导空态「配置大模型并重新 Aing 以启用知识图谱」+ 跳 `/ai`。
- `graph_data` 空 → 空态;`entity_detail` 查不到 → 面板空态。
- 笔记页无 mentions / 无图谱 → 高亮不可点(退回 Phase 2a 行为),正文照常。
- 会议搭子、录音、回放一切不受影响。

## 错误处理(红线)
图谱是纯增值派生索引。所有新命令:查询失败/panic 只 `eprintln` + 返回空,`Ok(...)` 不 `Err`,前端拿到空即空态。绝不因图谱问题让图谱页/笔记页/任何页报错或塌。`graph.sqlite` 可从 `aing.json` 整库重建(已有 `rebuild_all`)。

## 测试
- 后端:`cooccurrence_edges`(去重/边权/多笔记)、`entity_detail`(笔记标题联查/共现 top-N)、`resolve_local_ids`(人匹配 person_id / 非人退化 `e:名` / 无 aing.json 空)纯 SQL 单测,沿用 `graph/mod.rs` tempdir 测试范式。
- 前端:视图切换/过滤/搜索纯函数可 vitest;力导向与导航靠截图 + 真机冒烟(真实数据在用户批量 re-Aing 后)。
- gates:`cargo test --lib`、`npm run test`、`npm run check` 0/0、`cargo check`。

## 拆 plan(衔接执行,各自可审可合)
- **Plan A — 后端图谱查询命令 + 全局 id 解析**:`cooccurrence_edges` / `entity_detail` / `resolve_local_ids` + `ipc::{EntitySummary, GraphData, EdgeRow, EntityDetail, EntityLink}` + 四命令接线 + 单测。纯后端零 UI。
- **Plan B — 图谱页浏览/搜索/过滤 + 列表视图 + 详情面板 + 空态**:Sidebar 页签 + `/graph` 路由 + 列表/过滤/搜索 + 详情面板 + 深链 + 空态。**不含力导图**(先交付可用的实体浏览)。
- **Plan C — 力导向可视化 + 实体点击导航**:`d3-force` + SVG 图视图(节点/边/压噪/hover/拖拽)+ 图谱节点导航 + 笔记页高亮可点 + `/graph?e=` 深链聚焦。

每阶段 subagent-driven → 任务级+opus 双审 → push → PR squash 合 master。UI 用 frontend-design 定调、DESIGN.md 是真值源、截图迭代。

## 风险 / 已知限制
- **人实体同音异写连不上会议搭子**(王子璇 vs 王紫璇):本期不修,退化为非人节点。可后续做别名/模糊归并(复用会议搭子合并交互)——独立立项。
- **噪声实体**:靠排序 + 默认只画有边 + kind 过滤缓解;根治要调抽取 prompt(独立于 2b)。
- **规模**:节点涨到几百时 SVG 力导性能;设 top-N 封顶,超出走搜索;必要时后续换 canvas。
- **d3-force 新依赖**:已拍板接受(仅 force 模块)。

## 关联未完项(不属 2b,记账)
- 两条独立 bug 修复已在 master 工作树完成、待走极小 PR:①`llm.rs` 方舟 `thinking:{type:disabled}`(深度思考模型 refine 超 60s 超时);②`lib.rs` `AING_GATE` 全局串行闸(跨笔记 Aing 并发无上限 → 多套 ORT 池抢核/内存爆)。
- 魔法棒 `.reaing`/wand 条目补登 DESIGN.md;内部 `refine→aing` 标识符 cosmetic 改名——均独立于 2b。
