# AI 导航响应与标签对齐设计

## 问题

进入 AI 页面时会同时读取设置、AI 日志、Agent 注册状态、Skill 状态、能力清单和
本机 CLI 路径。相关 Tauri 命令目前为同步命令，其中 CLI 探测会启动多次
`where.exe`。Windows WebView/IPC 执行路径因此被阻塞，用户紧接着点击其他标签时，
窗口表现为无响应。

侧栏标签同时使用 `button` 和 `a`，但 `.vtab` 没有统一宽度、盒模型和 flex 居中。
横排 `AI` 按链接内容宽度布局，竖排中文则按纵向内容布局，造成选中态边框和文字轴线
不一致。

## 后端设计

- AI 页面启动阶段涉及磁盘、进程或路径探测的只读命令改为 `async` Tauri 命令。
- 命令内部使用 `tauri::async_runtime::spawn_blocking` 执行原有同步逻辑。
- 返回载荷、错误文本和前端并发加载方式保持不变。
- 不缓存探测结果，确保页面仍展示当前磁盘和 CLI 状态。
- 写操作不在本次范围内；用户点击安装、注册或保存时的现有语义保持不变。

## 标签布局

- `.vtab` 使用 `box-sizing: border-box`、`display: flex`、`align-items: center`、
  `justify-content: center` 和 `width: 100%`。
- `button` 与 `a` 继承同一字体并清除浏览器默认盒模型差异。
- 中文标签继续 `writing-mode: vertical-rl`。
- `AI` 使用 `writing-mode: horizontal-tb`，在统一标签盒内水平、垂直居中。
- 选中态仍以 `margin-right: -1px` 与内容面板连接，不改变现有视觉语言。

## 验证

1. 回归测试确认 AI 启动探测命令为 async，并通过 `spawn_blocking` 隔离。
2. 回归测试确认全部 `.vtab` 占满轨道并使用统一 flex 居中。
3. 运行完整 Vitest、Svelte 检查、Rust 编译和 Windows Release 构建。
4. 安装后冒烟测试：反复从 AI 切换到录音、图谱、钩子和设置，窗口持续响应；
   检查 `AI` 与中文标签中心轴和选中态边框对齐。

