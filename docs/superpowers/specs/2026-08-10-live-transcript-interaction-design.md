# 实时转写页交互升级(控制条重设计 + 回看辅助 + 当场纠正)

日期:2026-08-10
状态:已批准(用户逐项确认:纠正范围=说话人+文本、回看=搜索+过滤+时间轴、控制条四项全要、一次性大改单 PR)

## 背景

录制页(`src/routes/record/+page.svelte`)的实时转写区目前是纯只读:每行只有说话人徽章+文字,唯一交互是打断跟随后的「回到最新」。所有整理能力(改说话人/命名/编辑文本)只在停止录制后的笔记页可用。冒烟反馈三类不足:①暂停状态不醒目(列表徽标甚至仍显示「录制中」);②长会议无法回看/查找前文;③录制中发现转写错误无法当场纠正。

三个决定性技术事实:

1. **录制中冷编辑路径被 flock 明确拒绝**(`store/notes.rs`:笔记目录录制期独占)。当场纠正必须新开 live IPC,路由到持锁的录制会话内执行,冷路径拒绝不变式**保持不动**。
2. **实时段无编辑锚点**:`FinalEvent`/`Line` 只有 `source/text/speaker/start_ms`,磁盘段的 `seq` 未透传。编辑寻址先补 seq。
3. **电平只有 mic 单通道**:`ipc.rs` 的 `LevelEvent { rms }`(~10Hz,事件名 "level")仅 mic;系统声通道需补。

## 设计

### 1. 控制条重设计(纯前端 + 一个小后端补充)

- **状态醒目化**:录制中标题旁红点呼吸动画(reduced-motion 静止)+「录制中」文字;暂停时整条控制带背景换 warning-tint 色调,状态文字「已暂停」——整页一眼可辨,不再只靠右上角小灰点。
- **按钮体系化**:暂停/恢复/停止套 #84 控件体系——幽灵按钮族+16px 线性图标;停止是破坏性动作,改为**警示胶囊二段确认**(#84 同款,行内淡入不跳版),根治误触;暂停与停止拉开间距。
- **双通道电平**:mic/系统声两条实时电平条(窄条,控制带右侧)。后端 `LevelEvent` 增加 `source: "mic" | "system"` 字段,SCK 采集侧按 mic 同款(闸前 RMS,~10Hz)补发系统声电平;前端旧监听按缺省 mic 兼容。电平条同时回答「是否真在收音」的焦虑(冒烟多次遇到 mic 坏轨事后才发现)。
- **侧栏暂停徽标**(已实现,随本 PR 交付):`noteBadgeKind` 纯函数合成持久化 state × `recording.paused`,active+paused → 灰底「已暂停」徽标;红底只留给真在录。

### 2. 回看辅助(前端只读,录制中可用)

- **页内搜索**:控制带下搜索框;命中行高亮+计数(n/m),上/下跳转按钮与 Enter/Shift+Enter;进入搜索即暂停跟随最新,清空(或 Esc)恢复跟随并回到底部。匹配为大小写不敏感子串,纯函数 `searchTranscript(finals, query)` 配单测。
- **说话人过滤**:搜索框旁 chips(数据源 `recording.speakers`),点选后只显示该说话人的行(多选并集);过滤激活时也暂停跟随。过滤谓词纯函数配单测。
- **迷你时间轴**:转写区右缘细轨,按 `start_ms` 映射分钟刻度;点击跳转到对应时间最近的行(平滑滚动)。映射 `timelineIndex(finals, targetMs)` 纯函数配单测。三者组合的可见性判定(搜索∧过滤)统一在一个派生层,避免口径分叉。

### 3. 当场纠正(前后端)

- **seq 透传**:定稿落盘时后端已知 `seq`,`FinalEvent` 增加 `seq: number`;`Line` 同步增加;retract 匹配逻辑不变(仍按 source+start_ms+text)。续录/冷刷新回灌路径从磁盘段直接带 seq。
- **live 编辑 IPC 三条**:`live_set_segment_speaker` / `live_edit_segment` / `live_rename_speaker`,参数与冷路径同构(seq + expectedText 乐观校验 / speaker_id / name)。实现:命令投递到录制会话 actor,由 actor 串行执行(与 ASR 定稿追加天然互斥,无并发写 segments.jsonl),改写后通过既有事件回发前端(speaker 改名走 speakers 事件,段变更新增 `segment_edited` 事件,前端按 seq 更新 finals)。会话不在录(状态不符/note_id 不匹配)时返回明确错误。
- **UI**:已定稿行 hover 浮现操作钮(幽灵形态):改说话人(菜单:现有说话人列表+新说话人,复用笔记页 speakerPick 逻辑)/编辑文本(行内 input,Enter 提交、Esc 取消,expectedText 冲突时提示刷新);partial 行不可编辑;暂停与录制中均可操作。
- **不做**(YAGNI,与用户确认的范围一致):录制中删段、撤销栈、批量改。停录后笔记页能力不变。

### 4. 测试与验收

- vitest:searchTranscript / 过滤谓词 / timelineIndex / noteBadgeKind(已有)各配单测;i18n parity 与 noHardcodedCjk 哨兵自动覆盖新文案。
- cargo:live 编辑经 actor 的三条命令各配测试(在录成功/不在录拒绝/expectedText 冲突);LevelEvent 双通道序列化契约。
- 真机冒烟清单(PR 描述):暂停整条变色、电平双条随说话跳动、停止二段确认、搜索跳转+恢复跟随、过滤、时间轴点击、录制中改说话人/编辑文本并停录后在笔记页核对落盘。

## 风险与边界

- live 编辑与定稿追加共享 actor 串行队列,编辑延迟受当前转写负载影响(可接受:人手速≪段间隔)。
- `expectedText` 乐观校验在「编辑期间该行恰被 retract 撤回」时失败——按冲突处理(提示已变化),不做合并。
- 电平事件频率 ~10Hz×2 通道,渲染用 CSS transform 更新,不进 Svelte 大状态。
