# 段内说话人分离(B)设计

日期:2026-07-05
来源:多轮冒烟实锤的结构性缺陷——两人接话间隙 < VAD 静音阈值(0.6s)时被切进同一段,段级声纹一段一标签,"多人合一";混合段的平均声纹再拖歪质心引发"一人裂多"。圆桌派素材:段时长中位 17s、单段含多人问答。
分支:`intra-segment-diar`(基于 master),单 PR squash 合入。

## 核心思路

**滑窗声纹变更点检测 + 整段一次识别、按 token 时间戳切文本。**
sherpa-rs 0.6.8 `OfflineRecognizerResult` 暴露 `tokens: Vec<String>` + `timestamps: Vec<f32>`(秒,相对段首)——识别只跑一次,文本按变更点时刻分组,不重复 ASR。

## 组件

### 1. 变更点检测(新模块 `src-tauri/src/diar/split.rs`,纯逻辑)

输入:嵌入序列(每窗一条,或 None 表示该窗嵌入失败)+ 窗时间轴参数。输出:变更点时刻列表(ms,相对段首)。

- 相邻有效窗余弦 < `CHANGE_SIM_THRESHOLD`(0.55,待校准)→ 候选变更点,取两窗交界中点;连续多个低谷取最低者(一个说话人切换只产一个点)。
- 变更点将段切成子段;任一子段 < `MIN_SUBSEG_MS`(1200ms)→ 该变更点丢弃(并回,短子段声纹不可靠)。
- 嵌入失败的窗(None)视为"与两侧都相似"(不产生变更点——宁可漏切不误切)。
- 常量集中模块顶,注释标注"待真实会议数据校准"。

### 2. 滑窗嵌入(session.rs worker 内)

- 门槛:段时长 ≥ `SPLIT_MIN_SEGMENT_MS`(3000ms)且 embedder 可用才跑(短段装不下两个人;diarization off 时零开销)。
- 窗 `SPLIT_WIN_MS`(1500ms)、步 `SPLIT_HOP_MS`(500ms);每窗 `embedder.embed(窗样本)`(panic 防护同既有:catch_unwind,失败该窗记 None)。
- 15s 段约 25 窗,CAM++ CPU 每窗毫秒级,ASR worker 串行线程上百毫秒级延迟,可接受。

### 3. 文本切分

- `asr::Transcript` 增 `tokens: Vec<String>`、`timestamps: Vec<f32>`(Default 空;sense_voice 透传,whisper/mock 默认空)。
- 变更点把 token 按时间戳分组:token 时刻 < 变更点 → 归前子段;拼接为子文本(SenseVoice token 即文本片,直接 concat)。
- **回退路径**:timestamps 为空或长度与 tokens 不符(模型异常)→ 对每个子段音频单独 `recognize`(慢一点但正确);回退打日志。
- 子段文本 trim 后为空 → 该子段丢弃(不产 final,与空白段过滤同哲学)。

### 4. 子段接入既有流程(复用最大化)

一个 FinalJob 检测出 N ≥ 2 个子段后,**每个子段等价于一个独立 final**依序走完全既有的处理链:
- 语言过滤:整段判一次(在切分之前,现状不变)即可——子文本源自已放行的整段,不重复判。
- mic 子段:逐个进 ECHO hold/比对(PendingMic per 子段);system 子段:逐个即时 process_final。
- 每子段:embed(子段全音频)→ assign(声纹库种子/阈值照常)→ speaker;`rms_of(子段)`;on_final(source, sub_text, sub_start_ms, sub_end_ms, spk, rms)。
- start/end:段首偏移 + 变更点边界换算,时间轴与母段无缝衔接。
- 无变更点(绝大多数段):原路径原样,零行为变化。
- partial 路径不变。

### 5. 安全失败模式(设计不变式)

- **误切(同人被切开)**:子段声纹相近 → assign 归同簇 → 同一标签,只是段落变多,文本无损。
- **漏切(两人声纹太近/间隙内)**:与现状持平,不更糟。
- 嵌入全部失败/timestamps 缺失且重识别也失败 → 整段按原路径单段处理(占位/正常文本),**不丢内容**。

## 常量(集中 split.rs,全部标注待校准)

`SPLIT_MIN_SEGMENT_MS=3000`、`SPLIT_WIN_MS=1500`、`SPLIT_HOP_MS=500`、`CHANGE_SIM_THRESHOLD=0.55`、`MIN_SUBSEG_MS=1200`。

## 明确不做(backlog)

重叠语音(两人同时说)的分离;子段级质心回写声纹库策略调整(沿既有);变更点的 ASR 语义对齐(按 token 时间戳硬切,不做词边界回退);已落盘历史笔记的回溯切分。

ECHO 去重在两路不对称切分下的漏杀面(system 切分 vs mic 未切,子段 vs 整段相似度上限低于阈值)——带回声素材冒烟观察,必要时 system 子段额外对比 mic 母段全文。
滑窗 hop 750ms 降耗 / 长段处理间隙插 partial 服务(partial 冻结缓解)。

## 测试

- split.rs 纯逻辑:无变更点/单变更点/连续低谷取一/短子段并回/None 窗不产点/边界(空序列、单窗)。
- token 分组:正常分组、时间戳空回退信号、子段空文本丢弃。
- worker 集成:mock embedder 按窗音频内容返回可控向量(如样本均值 <0.5 → e1,否则 e2),mock recognizer 带 tokens/timestamps——双说话人段断言产出 2 个 final、speaker 不同、时间轴衔接;单说话人段断言仍 1 个 final(不乱切)。
- 全量:cargo test 全过、npm check 0/0(前端零改动预期)。

## 验收冒烟

1. 重放圆桌派式快接话素材:此前"两人合一"的长段被切开、各自贴标签(徽章不同)。
2. 单人长段(≥10s 独白)不被乱切成多段(允许偶发切开但标签相同)。
3. 暂停/续录/导出/声纹库自动命名全部照常。
