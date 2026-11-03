# UI 改版:页面迁移约定(#730)

调研:`docs/2026-11-03-ui-component-library-research.md`(主工作区,选型 shadcn-svelte)。
一期已落地:Tailwind 4 无 Preflight、组件在 `src/lib/ui/components/*`、皮肤 × 明暗分离
(Classic / Paper / Nord)、首帧外观、开发预览台 `/ui-gallery`。

## 迁什么、不迁什么

- **迁**:原生 `<button>` → `Button`;文本框 → `Input` / `Textarea`;`input.switch` 开关 → `Switch`;
  `<select>` → `Select`;手写下拉/右键菜单 → `DropdownMenu` / `ContextMenu`;手写弹层 → `Popover`;
  手写模态 → `Dialog` / `AlertDialog`;手写分段/页签 → `Tabs` 或 `ToggleGroup`;复选 → `Checkbox`;
  进度条 → `Progress`;滑块 → `Slider`。
- **不迁**(只换 token,不换实现):Milkdown/ProseMirror 编辑器内部、实时波形、音轨播放条的
  波形绘制、ForceGraph(d3/SVG)、说话人徽章配色、录制红点、系统原生确认框(Tauri dialog)。
- **业务逻辑一行不改**:状态、事件处理、IPC 调用、i18n 键都保持原样;只换控件外壳与样式。

## 写法

- 组件从 `$lib/ui/components/<name>` 导入;类名合并用 `cn` (`$lib/ui/utils`)。
- 颜色只用语义:Tailwind 侧 `bg-background / bg-card / bg-muted / text-muted-foreground /
  border-border / bg-primary / text-destructive`;或仍用 `var(--canvas)` 等 app.css token。
  **禁止硬编码色值**。shadcn 的 `accent` 是悬停底色;本应用的链接/焦点蓝是 `var(--accent)`(Tailwind 里是 `ring`)。
- **禁用四个裸类名**:`container`、`hidden`、`inline`、`ring`(与旧页面同名,已从生成里排除;
  用 `w-full`、`sr-only`/条件渲染、`inline-block`、`ring-1`)。
- 密度按 800×600 窗口:按钮默认 `size="sm"` 或 `default`(h-8);别照搬文档站的大尺寸。
- 中文输入法:`Enter` 提交的输入框一律先判 `e.isComposing`。
- 弹层经 portal 挂到 body:组件已补字体;依赖页面局部 CSS 变量的样式不要指望进弹层。
- 无障碍:保留原有 `aria-*` 与键盘路径;图标按钮必须有 `aria-label`。
- 文案:不得新增裸中文(`noHardcodedCjk` 护栏);新文案进对应 dict 分片,中英各一份。

## 验收

每迁一块:`npm run check` 0 错、`npx vitest run` 不新增失败、在 `/ui-gallery` 或真机看三套皮肤 ×
亮暗。e2e 见 `e2e/`。
