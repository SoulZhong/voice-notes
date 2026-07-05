# 语言幻觉过滤 + 段级 RMS 埋点 设计

日期:2026-07-04
来源:外放视频冒烟(20260704-195129):SenseVoice 短段语言误判产出假名幻觉段(`でかし`/`それもし`/`美国のポ調スパ`),漏过文本回声去重、污染说话人表;AEC 残渣的能量门槛(A1)因段落无能量数据无法校准。
分支:`lang-filter-rms`,单 PR squash 合入。

## 范围

1. **语言白名单过滤**:会议场景仅中英混合;识别为日/韩的 final 段整段丢弃(不 embed、不 assign、不 emit、不落盘),打日志留痕。
2. **段级 RMS 埋点**:每个落盘段追加 `rms` 字段(f32,段音频均方根),为 A1 能量门槛攒真实会议数据。纯诊断,不参与任何行为。

明确不做:能量门槛丢弃(A1,等 rms 数据校准);段内说话人分离(v2);partial 的语言过滤(转瞬即逝,不值得);聚类阈值调整。

## 1. 语言白名单过滤

**判据(双保险,任一命中即丢)**:
1. **模型语言标签**:sherpa-rs 0.6.8 `OfflineRecognizerResult.lang`(SenseVoice 输出如 `<|zh|>`);`Transcript` 增 `lang: String` 字段透传。lang 含 "ja" 或 "ko" → 丢。
2. **字符占比兜底**(lang 为空/格式意外时仍有效):文本中假名(U+3040-30FF、U+31F0-31FF)+ 谚文(U+AC00-D7AF、U+1100-11FF、U+3130-318F)占"字母类字符"(Alphabetic)比例 > 0.3 → 丢。纯汉字的日语幻觉读作中文,不拦(无损)。

**落点**:`session.rs::run_asr_worker`,final 段 recognize 成功后、回声去重 hold 与 `process_final` 之前——与既有 ECHO 命中丢弃同层(不 embed/不 assign/不 emit/不落盘),从源头杜绝垃圾段开新说话人。丢弃打 `eprintln!` 含 source/时间戳/lang/文本前缀。

**双路适用**:mic 与 system 段同判(system 路理论上也可能误判,今天未见,但判据无副作用)。

**测试**:字符占比函数纯逻辑单测(假名段/谚文段/中英混合/纯汉字/空串);过滤判定函数(lang 标签命中、占比命中、中英正常放行)单测。worker 集成路径靠既有门控测试 + 冒烟。

## 2. 段级 RMS 埋点

**计算**:final 段 `job.samples`(16k f32)的 RMS(`sqrt(mean(x²))`),在 run_asr_worker 内识别同点计算,随 final 透传至 `NoteWriter::append_final` 落盘。

**Schema**:`SegmentRecord` 增 `rms: Option<f32>`,`#[serde(default, skip_serializing_if = "Option::is_none")]`——旧笔记行反序列化为 None,不破坏兼容;新段始终写入。前端 TS 类型加可选字段,UI 不消费。export 不输出 rms。

**波及**:`append_final` 签名增参(生产 1 处调用 + writer/notes/export 测试若干调用点补 None 或值);`process_final`/`FinalJob` 链路透传。

## 验收

- cargo test 全过(新增语言判定单测);npm check 0/0;build OK。
- 人工冒烟:重放同类外放视频,假名段不再出现在笔记/说话人表;segments.jsonl 新段带 rms;老笔记打开无异常。
