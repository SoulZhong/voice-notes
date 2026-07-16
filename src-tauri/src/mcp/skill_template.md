---
name: voice-notes
description: 查询本机 voice-notes 会议笔记(实时转写+说话人识别)。当用户询问会议内容、要会议纪要、写周报/日报需要汇总会议、找会上的决议/待办/承诺/时间点时使用。支持全文检索、读取全文(优先 AI 修订稿)、录制状态查询与(需用户开启)录制控制。
---

<!-- managed-by: voice-notes v{{VERSION}} —— 本文件由 voice-notes 自动安装,应用升级时自动更新;手工修改会被覆盖。如需自定义,请删除本行受管标记(将不再自动更新)。 -->

# voice-notes 会议笔记

## 工具与降级路径

优先用 MCP 工具(server 名 `voice-notes`)。MCP 工具不可用时改用 CLI,输出与 MCP 同一 JSON 形状:

```bash
{{BINARY}} notes list --json
{{BINARY}} notes search "关键词" --json
{{BINARY}} notes get <note-id> --format md
{{BINARY}} speakers list --json
```

控制录制(需 App 运行;`start/stop/pause/resume` 还需用户在左侧「AI」页开启「允许 AI 控制录制」):

    {{BINARY}} record status
    {{BINARY}} record start --title "评审会"
    {{BINARY}} record stop
    {{BINARY}} record live --tail 20

被门控拒绝或 App 未运行时命令会返回指引原文,把它转告用户、不要自行重试。

需要原始逐字稿时加 --raw。

MCP 未注册时,**先征得用户同意**后可代为注册:`{{BINARY}} mcp register --agent auto`。

## 使用策略

- **先定位再取全文**:`search_notes`(大小写不敏感子串,试关键词的多个说法)拿 note_id,再 `get_note`;不要 list 全部后逐个 get。
- `get_note` 默认 prefer_refined=true:有 AI 修订稿(错字修正/段落归并)时返回 修订稿,响应的 `refined` 字段标注来源;需要逐句时间戳或原始逐字稿时用 format="segments"、prefer_refined=false。
- 查询类(list/search/get/speakers)无需 App 运行;`recording_status`/`get_live_transcript` 需要 App 正在运行;`start/stop/pause/resume_recording` 还需用户在左侧「AI」页开启「允许 AI 控制录制」——被拒时把这句指引转告用户,不要自行重试。
- 说话人:人名以响应里的 `speakers` 表(name/person_id)为准;P 号是跨会议一致的人物编号;`speaker_count` 是聚类结果仅供参考。

## 常用工作流

1. **会议纪要**:`get_note(note_id, format="markdown")` → 按「主题 / 结论与决议 / 待办(负责人+时限)/ 遗留问题」归纳;引用原话时带说话人名与时间戳。
2. **周报/日报汇总**:`list_notes(from=<周一日期>)` → 逐条 `get_note` 提取 1-3 个要点合并;标题与时长直接用 list 字段。
3. **找决议/待办/承诺**:`search_notes` 用关键词族(决定/定了/负责/下周/deadline/跟进),命中自带前后一句上下文,必要时 get 全文核对。
4. **代 Aing**(用户明确要求时):`get_note(format="segments")` 拿 修订稿 paragraphs → 只做错字纠正/实体统一/去语气词/中英排版,禁止改写语义 → `apply_refined_texts` 按下标提交有改动的段落(整段全文),确认无需修订则提交空 updates。笔记须已有 修订稿(refined=true),否则请用户先在 App 里 Aing 一次。

## 隐私

会议笔记是用户的本机隐私数据,内容进入你的上下文即离开本机。仅在任务需要时检索;引用大段原文前先确认用户意图。
