# 钩子（Hook）自动化：侧栏页签 + 完整执行闭环 — 设计

日期：2026-07-14
状态：已确认（本期做完整闭环：配置 UI + 持久化 + 后端执行体）

## 背景与目标

用户希望在笔记生命周期的各个状态变化时触发自定义动作（shell 命令 / webhook），
并在左侧边栏以独立页签「钩子」配置——与「录音」「会议搭子」并列。

现状：
- 后端 `lifecycle/hooks.rs` 的 `HookBus` 只有内部消费者（托盘），
  `ExternalHookCfg`/`register_external` 是无执行体的占位。
- `TransitionCtx` 只覆盖 session 主时间轴迁移，精修（refine）维度变化不经过总线。
- 前端无任何 hook 配置 UI。

目标：用户在「钩子」页签里配置若干 hook（事件 + shell 命令或 webhook URL），
对应事件发生时后端真实执行；失败只记日志，绝不影响录制/精修主流程。

## 架构（已选方案 A）

在 lifecycle actor 提交状态处（`actor.rs` 现有 `bus.notify` 旁），对比提交前后
**完整内核状态（session + refine 两维）**，经纯函数映射出业务事件白名单，
派发给新的独立执行模块 `hooks_external`。不复用 HookBus 消费者路径
（`TransitionCtx` 够不着精修事件，且原始迁移暴露内部状态机、重构即破坏用户配置）。

### 事件白名单

| 事件 | 判定（提交前 → 提交后） | note_id |
|---|---|---|
| `recording_started` | 非 Recording → Recording（含续录） | 有 |
| `recording_stopped` | Recording \| Stopping → Idle | 有 |
| `recording_paused` | Recording(paused=false) → Recording(paused=true) | 有 |
| `recording_resumed` | Recording(paused=true) → Recording(paused=false) | 有 |
| `refine_started` | refine 集合新增 id（逐 id） | 有 |
| `refine_finished` | refine 集合移除 id（逐 id） | 有 |

映射为纯函数 `hook_events(before, after) -> Vec<(HookEvent, note_id)>`，可单测。
一次提交可能产出多个事件（如停录同时触发自动精修）。

### 数据模型与持久化

新文件 `app_data_dir/hooks.json`（原子写，模式同 `settings.rs`；独立于
settings.json，避免与设置页写同一文件互踩）：

```json
{
  "hooks": [
    {
      "id": "h_...",
      "name": "停录后归档",
      "event": "recording_stopped",
      "kind": "shell",          // "shell" | "webhook"
      "command": "…",           // kind=shell 时使用
      "url": "…",               // kind=webhook 时使用
      "enabled": true
    }
  ]
}
```

serde 全字段 default；坏文件/缺文件回落空表并记日志。后端每次事件读快照，
无内存状态同步问题。

### 执行体（新模块 `hooks_external.rs`）

事件发生 → 读 hooks.json 快照 → 过滤 `enabled && event` 匹配 → 逐个派发到
后台线程执行（不阻塞 actor）：

- **shell**：`/bin/sh -c <command>`，事件信息经环境变量传入：
  `VN_EVENT`、`VN_NOTE_ID`、`VN_NOTE_TITLE`。等待退出，30s 超时，退出码记日志。
- **webhook**：POST JSON `{event, note_id, note_title, occurred_at}`，
  10s 超时，非 2xx 记日志。HTTP 客户端复用项目现有依赖。

契约（与 HookBus 一致）：任何执行失败/超时/panic 只记日志，不影响主流程，
不影响其余 hook。

### Tauri 命令

- `list_hooks() -> Vec<HookCfg>`
- `save_hooks(Vec<HookCfg>)`（整表覆盖，原子写）
- `test_hook(cfg: HookCfg) -> TestResult`：以假载荷（`VN_EVENT=test` 等）
  立即执行一次，返回退出码/stdout 摘要或 HTTP 状态，供配置页「测试」按钮。

## 前端 UI（遵循 DESIGN.md，完成后同步 DESIGN.md）

- **侧栏**：`tab-rail` 增加第三个竖排页签「钩子」，tab 由路由派生（`/hooks` 域），
  沿用「点当前页签回根」模式；录制按钮照旧常驻面板顶部。
- **侧栏面板**（钩子页签选中时）：「新建钩子」按钮 + 按事件分组的 hook 列表
  （行 = 名称 + 事件标签；禁用的淡显），点击行进主区编辑页。
- **主区 `/hooks`**：空态介绍（钩子是什么、能做什么）+ 环境变量/payload 文档；
  有配置时显示概览列表。
- **主区 `/hooks/[id]`**（`new` 为新建）：名称、触发事件下拉、shell/webhook
  分段选择、命令（多行）或 URL、启用开关、测试按钮及结果展示、保存/删除。

## 错误处理

- hooks.json 读失败：回落空表 + 日志，UI 显示加载失败横幅（与现有 banner 规范一致）。
- 保存失败：编辑页就地报错，不丢用户输入。
- 执行失败：只记日志（含 hook 名、事件、错误）；`test_hook` 把错误如实返回给 UI。

## 测试

- `hook_events` 纯函数单测：全部白名单事件（含续录、暂停/恢复、停录+自动精修
  同帧多事件、精修 diff 按 id）。
- hooks.json load/save 回归：缺字段默认、坏文件回落空表、原子写。
- shell 执行体单测：echo 命令、退出码、环境变量注入。
- `test_hook` 命令冒烟。

## 非目标（YAGNI）

- 按笔记过滤 hook（`TransitionCtx.note_id` 预留的方向）——本期全局配置。
- 执行历史/日志查看 UI——只记后端日志。
- hook 排序/优先级、条件表达式、模板变量插值。
