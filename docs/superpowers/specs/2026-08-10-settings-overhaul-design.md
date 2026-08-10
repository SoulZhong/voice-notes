# 配置项大梳理(默认成品轨 + 清理 + 交互统一)

日期:2026-08-10(v2,经 Codex 审查后重写——首版 10 P1/8 P2,见文末审查记录)
状态:已批准(用户逐项拍板,含 Codex 轮后三项新决策:硬承诺双轨 / AEC json 逃生舱 / 四项扩展全收)

## 背景

PR#85 落地声音处理方案三档(a/ab/b,默认 a)。本单:B 升为旗舰默认 + 设置面大梳理(删过时项/统一控件/低频收折叠) + Codex 审查追加的正确性修复与扩展项。

## 设计

### 1. 声音处理方案默认翻转(a → b)

`AudioScheme` 默认 `B`。**迁移判定改为原始 JSON 键存在性判断**(Codex P1#1:"值==默认值⇒未显式设置"在默认翻转后必错——`{"audio_scheme":"b","mix_track":true}` 会被误改 ab)。实现:`audio_scheme` 反序列化为 `Option<AudioScheme>`(内部字段),load 后解析:

| 旧文件状态 | 结果 |
|---|---|
| `audio_scheme` 键在场(任意值) | 照旧(旧键即便在场也忽略) |
| 键缺失 + `mix_track: true` | **ab** |
| 键缺失 + `mix_track: false`/缺失 | **b**(新默认) |

对外(get_settings/前端类型)仍是具体值非 Option。`Settings::default()` 解析后语义为 b(unwrap_or_default 兜底口径单独断言)。

### 2. 设置文件安全网(Codex P1#2/#9)

- **升级备份**:启动迁移首次改写前,若 settings.json 存在且无本版本备份,先拷贝为 `settings.json.bak-<旧schema标记>`(一次性,不覆盖已有备份)——"删除字段可回滚"从此成立(save 会立刻抹掉旧键,备份是唯一回退路径)。
- **解析容错**:整文件 JSON 解析失败或字段级类型错时,不再整对象静默重置——先备份坏文件为 `settings.json.corrupt-<ts>`,再逐字段从 `serde_json::Value` 抢救可读字段(读不出的字段才用默认值),并记 log。默认翻转放大了整体重置的伤害面(云凭证/目录/隐私开关全丢),必须堵住。

### 3. 三删一藏(消费者全清单,Codex P1#3/4/7、P2#11)

- **删 `keep_audio`**(固定保留):lib.rs 快照与 sink 装配分支;**lib.rs:7140 续录同步测试的 keep_audio=false 契约改写为"writer 未写入"一般性场景**;设置行+i18n。
- **删 `record_system_only`**(固定双轨):lib.rs 两个录制入口的 `RecordSource` 计算与 `RecordSource::from_settings`(telemetry.rs:21)——**遥测枚举改为常量 Both 或删除该维度**;`required_sources_follow_system_only` 测试重写;设置行+i18n。
- **删 `keep_output_volume`**(固定普通输入+AEC):lib.rs 采集路径分支;**录音页两处消费者改常态检测**——蓝牙回声风险提示(record/+page.svelte:62)与低输入音量提示(:78)不再依赖开关、始终按设备状态判定;**record.ts 蓝牙提示文案重写**(原文案指向已删除的设置开关,改为提示蓝牙设备可能击穿回声消除+指引换设备/逃生舱)。
- **AEC 逃生舱**(Codex P1#5,用户拍板):新后端字段 `capture_path: "aec"|"vpio"`(默认 `"aec"`,**无 UI**,手改 json,同 `asr_provider` 先例)。蓝牙击穿/设备格式不兼容(microphone.rs 仅收默认设备 F32)等翻车场景的运行时退路。
- **藏 `language_filter`**:行为不动,UI 入高级折叠区。

### 4. 硬承诺双轨(Codex P1#6,用户拍板)

System 源升为**必备源**:屏幕录制权限缺失或 system 采集启动失败时**拒绝开录**,给授权引导(说明卡 + 打开系统设置按钮,沿用日历授权说明卡先例);不再静默降级单麦克风。涉及:lib.rs 必备源判定、录制入口错误 UX、record 页文案。b/ab 档由此保证有成品轨产物。

### 5. 容量治理(Codex P1#8,用户拍板收编)

新增「音频自动保留期」设置:`audio_retention: "forever"|"90d"|"30d"`(默认 forever,Segmented 三段)。到期笔记仅清音频轨(转写/精修稿永留),复用现有手动清理的删除路径,启动时后台执行一次。磁盘占用展示与手动清理入口**留在可见位置(存储不再整体进高级区——修订 v1 决定:高级区只收语言过滤+identify_auto_apply;数据/模型目录仍留可见「存储」组,与容量治理同区)**。

### 6. 文案/状态如实化(Codex P2#12/13,用户拍板收编)

- lockHint 按真实生效时机分层:识别方式录制中锁定;主题/托盘/语言即时生效;其余下一场生效——文案如实分句,不再一刀切。
- 「会后 AI」开关显示就绪状态:refine 四项配置不齐时开关旁给"未配置完成"提示 + 跳转 AI 页链接;开关仍可切(行为不变),但状态不再撒谎。

### 7. 其余清理(用户拍板收编)

- `identify_auto_apply` 移入高级折叠区(实验开关,前置条件普通用户无法完成)。
- `mirror_prefix` 简化:删自定义前缀字段及其迁移代码,只留 `mirror_enabled` + 编译期 URL 常量(serde 忽略旧键,升级备份兜底)。

### 8. 设置页重排与控件统一

- **分区**:通用 / 录制 / 存储(目录+磁盘占用+清理+保留期) / 语音模型 / 高级(折叠:语言过滤、identify_auto_apply) / 关于。
- **高级折叠区可执行定义**(Codex P2#18):disclosure 行为 `<button aria-expanded>` + chevron,键盘 Enter/Space 可开合,展开态为**页面挂载内记忆**(组件 $state,离开路由即复位——如实描述,不承诺"会话内");120ms 展开,reduced-motion 直切。
- **控件统一**:≤3 互斥选项一律 Segmented——主题/界面语言/识别方式/声纹模型/云厂商/声音处理方案/音频保留期。
- 录制区最终行:声音处理方案、日历匹配;快捷键/托盘在通用区不动。

### 9. 测试(Codex P1#10 扩表)

Rust:迁移矩阵(键在场任意值×旧键组合、键缺失×true/false/缺失、未知枚举值、截断 JSON 抢救、备份只写一次、set_settings 后旧键不复活)、capture_path 默认与消费、retention 判定纯函数、三删字段旧 json 解析+启动重写后内容断言。前端:如实化文案条件渲染逻辑(可测部分)、i18n parity。真机冒烟清单在计划收尾节展开(含:三种旧文件升级、AEC 常态回声、蓝牙设备提示、无屏幕录制权限拒录引导、保留期清理、逃生舱手改生效)。

## 范围与非目标

**不动**:AudioScheme 三档语义/笔记页回放逻辑、AI 精修组配置本体、快捷键/托盘/主题行为、MCP 设置。
**另记后续项(不入本单)**:麦克风设备选择器(Codex P2#16)、API key 迁 Keychain(P2#17)。
**分支**:`settings-overhaul`,叠在 `audio-scheme-setting` 上;下层合入后依次改 base。

## Codex 审查记录(2026-08-10)

首版 spec 经 `/codex review`(consult 模式,760k tokens)得 10 P1 + 8 P2,GATE FAIL。P1 全部吸收:#1 迁移判定改键存在性;#2 升级备份;#3/4 录音页消费者与文案;#5 逃生舱;#6 硬承诺双轨;#7 遥测/测试清单;#8 容量治理;#9 解析容错;#10 测试扩表。P2:#11/12/13/14/15/18 收编,#16/17 记后续项。Codex 结论:三删字段无 MCP/CLI 直接消费者。
