# 归属建议卡「换个人」设计(切换合并目标)

日期:2026-08-02 · 分支:feature/speaker-tidy-redesign · 前置:2026-08-01-speaker-tidy-followups-design.md

背景:建议卡右侧是系统猜的合并目标。猜错人时(如"说话人 281 → 仲维建"其实该并给别人),现在只能忽略后绕去详情页「合并到…」,丢掉卡片左右对照的拍板上下文。

## 交互

- **入口**:winner 侧面板头部行尾一个低调按钮「换个人」(ink-faint 小字,hover 显色;title「不是他?换成另一个已有的人」)。录制中不置灰(纯本地选择)。
- **选人 popover**:与详情页「合并到…」同款——检索输入(filterPeople,含拼音)+ `PersonPickList` 复用;候选=全库 − 左侧说话人 − 当前显示目标;`menu-scrim` 点外部关闭 + `svelte:window` Esc(概览页补 scrim/Esc,模式抄详情页刚落地的实现)。
- **切换后的卡**:右侧面板换成所选人(`personPane` 复用,选中即 `loadNotes(id)` 拉会议上下文);标题的「相似度 N% · 很可能」替换为「手动改选」小标(相似度只对系统建议对成立);旁边「还原建议」小链接回到系统目标(还原后相似度标恢复)。
- **合并**:把左侧并入**当前显示目标**(`mergePerson(s.loser, 当前目标)`);撤销条标签用当前目标名;卡片级错误/act() 对账/录制中置灰沿用。**忽略**仍按原建议对键 `s:loser>winner` 落盘(忽略的是这条建议)。

## 状态与兜底

- 会话级覆盖表:`sugOverride: Record<string /* sugKey */, string /* person id */>`,不落盘。
- 目标解析纯函数(tidyQueue.ts,TDD):
  `resolveSugTarget(s, overrides, byId): { id, name, overridden }` — 覆盖 id 存在于库 → 用覆盖;不存在(其间被合并/删除)→ 回落系统建议目标(overridden=false)。s.winner 本身不在库的建议已被 buildTidyQueue 过滤,无需兜底。
- 弹层同屏互斥:打开选人 popover 前收起一键清理确认(复用 act() 的清场习惯即可,不引入全局浮层管理)。

## 不做

方向互换;同名组/无样本卡换人;改选状态落盘;后端改动(零)。

## 测试

- vitest:resolveSugTarget 三态(覆盖生效/覆盖失效回落/无覆盖)。
- IPC 仿真回归:换人→卡片右侧更新+「手动改选」标→合并并入所选人→撤销复原;还原建议;Esc/外点关闭;覆盖人被删后回落。
