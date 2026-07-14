# 钩子附带笔记内容（精修稿全文 + 笔记详情）— 设计

日期：2026-07-14
状态：已确认（接续 hooks-tab 分支 / PR #40）
前置：docs/superpowers/specs/2026-07-14-hooks-tab-design.md（钩子自动化已落地）

## 背景与目标

钩子目前只带事件名/笔记 id/标题。用户希望钩子能拿到**精修后的笔记全文**与**笔记详情**，
以便「精修完成后把纪要发出去」这类用例开箱即用。

用户拍板：shell 与 webhook **都内嵌文本**（不走文件路径）；**每条钩子一个开关**决定是否附带
（不需要内容的钩子零成本）。

## 设计

### 1. 配置

`HookCfg` 增 `include_note: bool`：

```rust
/// 附带笔记内容:开启时执行注入笔记详情与全文(精修稿优先)。默认关,
/// serde default 兼容老 hooks.json。
#[serde(default)]
pub include_note: bool,
```

前端 `HookCfg` 类型同步；`newHook()` 默认 false。编辑页「启用」行之上加
「附带笔记内容」switch 行（row-desc：把笔记详情与全文交给命令/接口，精修稿优先）。

### 2. 内容构建（`hooks_external` 内新逻辑）

```rust
pub struct NoteContent {
    pub started_at: String,
    pub ended_at: String,        // 空串 = 未结束
    pub duration_secs: u64,
    pub speakers: Vec<String>,   // 显示名:名字 > 「说话人 N」
    pub text: String,            // markdown
    pub truncated: bool,
}
```

- 文本来源：`store::load_refined` 命中时 `render_refined`（与 `export_note` 同款：
  段落含 person_id 时只读 join 声纹库现名），否则 `NoteStore::render(id, "md")` 原始稿。
- 详情来源：`NoteStore::load` 的 meta 与 speakers（与 get_note 同源）。
- **截断上限 `NOTE_TEXT_MAX = 200_000` 字节**：macOS execve 的 env+argv 总预算约 1MB，
  内嵌全文必须留余量，否则长会议 spawn 直接 E2BIG 失败。按 UTF-8 字符边界安全截断，
  置 `truncated = true`。webhook 同上限（形态间行为一致）。
- 截断的纯函数 `truncate_utf8(s, max) -> (String, bool)` 单测覆盖多字节边界。

### 3. 注入

**shell** 追加环境变量（在既有 VN_EVENT/VN_NOTE_ID/VN_NOTE_TITLE 之上）：

| 变量 | 值 |
|---|---|
| `VN_NOTE_TEXT` | 全文 markdown（可能截断） |
| `VN_NOTE_STARTED_AT` | RFC3339 |
| `VN_NOTE_ENDED_AT` | RFC3339，未结束为空串 |
| `VN_NOTE_DURATION_SECS` | 整数秒 |
| `VN_NOTE_SPEAKERS` | 显示名顿号（、）分隔 |
| `VN_NOTE_TEXT_TRUNCATED` | 仅截断时注入，值 `1` |

**webhook** payload 追加 `note` 字段：

```json
{
  "event": "refine_finished",
  "note_id": "…",
  "note_title": "…",
  "occurred_at": "…",
  "note": {
    "started_at": "…",
    "ended_at": "…",
    "duration_secs": 3600,
    "speakers": ["张三", "说话人 2"],
    "text": "…markdown…",
    "text_truncated": false
  }
}
```

未开启 `include_note` 的钩子：env 与 payload 与现状完全一致（`note` 字段不出现）。

### 4. 派发与失败语义

- `run_fires` 中，本批匹配钩子里任一 `include_note` 才构建，**按 note_id 缓存一次**
  （停录+自动精修同帧多事件不重复构建）。
- 构建失败（笔记刚删、读盘失败）：记日志，**跳过附带照常执行钩子**（env/payload 无
  note 部分）——内容是增值信息，不挡触发。
- 事件与内容的时点语义：附带的是**执行时刻盘上最新内容**。`recording_stopped` 时通常
  是原始稿（自动精修未完），`refine_finished` 时是精修稿——想要精修全文就挂
  refine_finished，概览页文档写明这一点。

### 5. test_hook

`include_note` 开启时注入固定假内容（text=「测试正文」，详情用假值，speakers=
「测试说话人」），不依赖库里有真笔记；未开启行为不变。

### 6. 文档

- `/hooks` 概览页：环境变量表补六个新变量（标注「仅勾选附带笔记内容时注入」）；
  webhook payload 示例补 `note` 字段；补一句时点语义（精修全文挂 refine_finished）。
- DESIGN.md「钩子配置」条目：编辑页行清单补「附带笔记内容」switch 行。

## 错误处理

- 内容构建任何失败只记日志、照常触发钩子（无 note 部分）。
- 老 hooks.json 缺 `include_note` → false，行为与现状逐位一致。

## 测试

- `truncate_utf8`：多字节字符边界、恰好等长、空串。
- NoteContent 构建：精修稿优先、无精修回落原始稿、speakers 显示名兜底（名字>说话人 N）。
- env/payload 形状：开启时六变量/`note` 字段齐全，未开启时与现状逐字一致（回归防漂移）。
- 老配置缺字段默认 false。

## 非目标（YAGNI）

- 附带音频文件/路径。
- 按钩子自定义截断上限或导出格式（txt/json）。
- recording_started 等无内容意义事件的特殊处理——开关是用户的选择，不做事件级限制。
