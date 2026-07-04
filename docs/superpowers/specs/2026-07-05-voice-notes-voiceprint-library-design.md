# 全局声纹库(跨会议说话人身份)设计

日期:2026-07-05
来源:用户需求——建立声纹库,匹配上的声纹自动打名字;未匹配的自动入库;用户可合并到已知人或建新人,以后自动识别。
分支:`voiceprint-library`(叠在 lang-filter-rms 之上;PR #7 合并后 rebase 到 master 再开 PR)。

## 用户可见行为

- 录制中:说话人一旦命中声纹库,chips/段落徽章直接显示其名字(而非 S 号)。
- 停止后:本场未命中库、且累计发声 ≥10s 的说话人,自动入库为"未命名人"。
- 管理页(侧栏新入口「说话人」):列出库中所有人(名字或"未命名 · 最近出现 …"),支持改名、合并到另一人、删除。
- 库里改名/合并后:历史笔记打开时,凡有 person 关联且本地无名的说话人显示库中现名。

## 架构:S 号体系不动,person 是叠加层

会话内聚类照旧产出 S1..Sn;簇可携带 `person: Option<String>`(库 person id,如 "P3")。名字传播复用既有通道:种子命中 → SpeakersChanged 事件的 name 字段带库名 → writer 内存表/speakers.json/前端 chips 全部零改动收益。

### 1. 数据模型:`voiceprints.json`(app_data_dir 根,原子写 tmp+rename)

```json
{
  "schema_version": 1,
  "next_person": 3,
  "people": {
    "P1": {
      "name": "张三",              // 空串 = 未命名
      "centroids": { "mic": { "vec": [..], "count": 42 },
                     "system": { "vec": [..], "count": 7 } },
      "total_ms": 132000,          // 累计发声(入库门槛与展示用)
      "last_seen": "2026-07-05T10:00:00+08:00"
    }
  },
  "redirects": { "P2": "P1" }      // 合并后旧引用重定向(读取时解析,链式跟随,防环)
}
```

- 按信道(mic/system)分质心:跨信道声纹差异真实存在,匹配取所有质心的 max 余弦。
- store 层新模块 `store/voiceprints.rs`:`VoiceprintStore { load / save_atomic / resolve(person_id) / rename / merge / delete / upsert_from_session }`。全局单文件,写操作持模块级静态锁(与 notes.rs EDIT_LOCK 同模式、独立锁)。损坏容忍:解析失败按空库处理并 eprintln(不阻塞录制),首次写前备份 `.bak`。

### 2. registry 扩展(diar/registry.rs)

- `Cluster`/`ClusterSnapshot` 增 `person: Option<String>` 与 `total_ms: u64`(assign 长段时累加 num_samples/16;短段不计,与质心策略一致)。
- **种子注入**:`SpeakerRegistry::with_seeds(snaps: &[ClusterSnapshot], seeds: &[SeedCluster])`——先铺会话快照(resume 路径,既有 from_snapshot 语义),再注入库种子:每 (person, source-质心) 一簇,分配新 S 号,`person=Some(..)`,count 从库带(权重大、漂移慢),total_ms 归 0(只统计本场)。**去重**:快照簇已关联的 person 不再注入种子(续录场景笔记快照更贴近本场信道)。
- **种子命中阈值** `SEED_ASSIGN_THRESHOLD: f32 = 0.68`(> ASSIGN 0.62,< MERGE 0.74;跨会议信道差异大,误命名比不命名糟,待真实数据校准):assign 对 `person.is_some()` 的簇用该阈值,普通簇维持 0.62。
- **merge 规则**:两簇 person 均为 Some 且不同 → 禁止自动合并(跳过该对;"库里两人实为一人"只能用户显式操作);一方有 person → winner 继承该 person。
- `SpeakerInfo` 增 `person: Option<String>` 与 `name: Option<String>`(种子簇携带库名,SpeakersChanged 时传出)。

### 3. 会话接线(session.rs / lib.rs)

- 开录/续录:lib.rs 加载 voiceprints.json → 构造 seeds → `with_seeds` 建 registry(替换现有 from_snapshot/new 两个入口)。库加载失败降级为无种子(打日志,不挡录制)。
- SpeakersChanged → lib.rs 既有转 SpeakerEntry 处:种子簇带 name(writer.set_speaker_name 同款写入内存表),person_id 一并存入 writer speakers 表。
- 停止(Snapshot 路径):lib.rs 拿到 ClusterSnapshot 后调 `VoiceprintStore::upsert_from_session`:
  - `person=Some` 的簇:对应 person 按 (source, count) 加权 merge 质心,total_ms 累加,last_seen 更新;
  - `person=None` 且 `total_ms >= AUTO_ENROLL_MS(10_000)` 且质心非空:新建未命名 person(next_person 自增),该会话 speakers 表回填 person_id;
  - 低于门槛的簇:不入库。
  - upsert 在 stop_recording 内、finalize 之前完成(person_id 关联要落进本场 speakers.json)。

### 4. 笔记侧(store)

- `SpeakerMeta` 增 `person_id: Option<String>`(serde default + skip_serializing_if,旧笔记兼容;writer sync/persist 链路透传)。
- `NoteStore::load` 只读 join:speaker 本地 name 为空且 person_id 可解析(经 redirects)且库中该人有名 → 用库名填充返回的 speakers 表(不落盘)。库文件缺失/损坏 → 跳过 join。
- 笔记内既有 rename_speaker 行为不变(本地名优先级最高)。

### 5. 命令与管理页

Tauri commands(全部走 VoiceprintStore,写操作持库锁):
- `list_people() -> Vec<PersonSummary { id, name, total_ms, last_seen, sources }>`(经 redirects 解析后的有效人)
- `rename_person(id, name)`(录制中允许——库名只影响后续会话与只读 join)
- `merge_person(loser, winner)`:centroids 按 source 加权并入、total_ms 相加、redirects 记录 loser→winner(既有指向 loser 的链条保持可解析);录制中拒绝(种子已注入,语义混乱,报错"录制中不能合并说话人")
- `delete_person(id)`:移除 person 与指向它的 redirects;录制中拒绝。

前端 `/speakers` 路由(SvelteKit 页面,侧栏加「说话人」入口):
- 列表:名字(未命名显示"未命名 · 最近 {last_seen 日期}")、累计发声时长、信道标记;
- 行内操作:改名(复用常驻编辑态模式)、合并(下拉选目标人+确认)、删除(二次确认);
- 空库提示:"录一场会议,停止后本场说话人会自动出现在这里"。

## 阈值与常量(标注待校准)

- `SEED_ASSIGN_THRESHOLD = 0.68`、`AUTO_ENROLL_MS = 10_000`,集中在 registry.rs/voiceprints.rs 顶部,注释标注"待真实会议数据校准"。

## 明确不做(backlog)

旧笔记回溯批量匹配;库导入/导出;多设备同步;声纹质量评分;录制中入库(只在停止时);person 级质心多样本集(每信道单质心够 v1)。

## 测试

- registry:种子注入(编号续接/去重/person 继承)、种子阈值分档、person 冲突禁并、total_ms 累计。
- voiceprints store:load/save 往返、损坏容忍、redirects 链式解析防环、merge/delete/rename、upsert(加权合并/门槛入库/回填)。
- notes join:person_id 解析填名、本地名优先、库缺失降级。
- 集成:worker 停止路径 person 关联落盘(mock embedder 走 with_seeds)。

## 验收冒烟

1. 录一场(≥10s 发声)→ 停止 → 管理页出现未命名人
2. 命名"张三" → 再录同人 → chips 自动显示"张三";segments/speakers.json 带 person_id
3. 两个未命名人合并 → 历史笔记该说话人显示合并目标的名字
4. 删除人后再录 → 不再自动命中;旧笔记不崩(redirect/悬空容忍)
5. 库文件手工损坏 → 录制照常(无种子),管理页空列表+不崩
