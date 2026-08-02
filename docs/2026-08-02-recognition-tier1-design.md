# 识别准确度第一梯队改造设计(AS-Norm/信道感知/短段分层/ERes2NetV2 收尾)

日期:2026-08-02 · 依据:`2026-08-02-speaker-recognition-accuracy-analysis.md` 第一梯队四项 · 用户已批准

## 目标

压制主症状"跨会议认不出旧人→每场批量造新 P":给实时识别层补上业界标配的分数归一化与信道/时长感知,同时收敛模型切换的边界。**现有行为是新行为的子集(除跨信道快路收紧外只增召回)**;全部新常量标注"待评测集校准的初值"。

## 一、AS-Norm 增益通道(diar/registry.rs)

- 保留同信道裸 `cos ≥ SEED_ASSIGN_THRESHOLD(0.68)` 快路。
- 新增归一化通道:命中 = `z ≥ SEED_ASSIGN_Z(3.0) 且 raw ≥ SEED_ASSIGN_RAW_FLOOR(0.50)`。
- 对称 z 与整理层 suggest 同式:`z = ((s−μa)/σa + (s−μb)/σb)/2`。
  - 测试侧(μa/σa):assign 扫描本就算了测试嵌入对每个种子簇的分数——按 person 聚成"每个其他人物的最高分",对候选人之外的集合取均值/样本标准差(σ 下限 1e-3)。
  - 种子侧(μb/σb):`with_seeds` 注入时预计算——该种子质心对**其他人物**种子质心的每人最高分,均值/标准差,存在 Cluster 上(`Option<(f32,f32)>`;非种子簇恒 None)。
- **小 cohort 关断**:其他人物数 `< SNORM_MIN_COHORT(3,与 suggest 同值)` 时 z 通道整体禁用(小库统计不稳)。
- 每段额外代价:按 person 归组求 max + 一次均值方差,复用扫描期分数,微秒级。

## 二、信道感知(voiceprints.rs seed_clusters + registry)

- `SeedCluster` 加 `pub source: String`(seed_clusters 现在把 centroids 的 BTreeMap 信道键丢了,补上;会话变体同理带其信道)。`Cluster` 记 `seed_source: Option<String>`。
- 判定分路:段的 source == 种子的 source → 快路 + z 通道;**跨信道 → 只走 z 通道**(裸分跨信道不可比;这是唯一收紧点——原先跨信道 raw≥0.68 也放行,现在要求 z 达标,防 mic↔system 系统性错认)。
- 场内普通簇不受影响(同场混信道由既有逻辑处理)。

## 三、短段分层(registry.rs)

- `MIN_CENTROID_UPDATE_SAMPLES: 9600(0.6s) → 24_000(1.5s)`。注释保留 0.6s 首轮校准史,新增依据:<2s 嵌入可靠性跳崖(EER 可翻 3 倍),running-mean 的稀释不足以抵御短段系统性偏移。
- 新增 `SEED_MIN_SAMPLES: usize = 32_000(2s)`:短于 2s 的段**不参与种子命中**(快路与 z 通道都不开;仍可归场内簇 0.62/软归属 0.45)。短段无权拍板"这是谁"。
- 终审 F2 补记:`MIN_CENTROID_UPDATE_SAMPLES` 提到 1.5s 后,0.6~1.5s 的段不再计入 count/total_ms,连带让 `AUTO_ENROLL_MS` 的累计口径变严——碎句说话人攒够登记门槛变慢,方向与本次改造"压制批量造新人"一致,是随本次校准接受的显式决定。

## 四、ERes2NetV2 收尾(models/settings/前端)

链路已内建(设置项+下载+切换触发 `rebuild_for_model`+模型不一致跳种子)。补齐:

1. 核实并显性化**无样本人物**在 rebuild 中的行为:无样本 → 质心无法重建 → 换模型后不可识别。切换入口(设置页)在确认时提示「库内 N 人无录音样本,切换后将无法自动认出(不影响历史笔记)」,N 从后端取(新增或复用轻量查询)。
2. 设置页声纹模型行补一句依据文案:「ERes2NetV2 中文基准更准(CN-Celeb EER 6.14% vs 6.78%),模型更大速度稍慢」。
3. 全局默认仍为 campplus(效率优先理由成立);本机切换由用户在设置页操作,属真机验收项。

## 测试

全部可用合成向量在 registry 单测覆盖:同信道快路命中/跨信道同分不命中/跨信道高 z 命中/cohort<3 关断/1.9s 不认种子/1.4s 不更新质心 1.6s 更新/既有行为回归。seed_clusters 信道贯通配 store 测。第 4 项前端文案走仿真截图,rebuild 边界配 cargo 测。

## 不做

评测集(第三梯队,另立项);z 阈值精调(初值 3.0 与自动归并同档,待评测);OSD/质量门控/回写(第二梯队)。
