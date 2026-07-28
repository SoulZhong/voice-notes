/**
 * 功能引导使用稳定 ID 独立记账，不能复用全局布尔值。
 * 新功能或引导内容发生重大变化时新增/升级 ID；旧引导不会因此重播。
 */
export const AI_TOOLS_GUIDE_ID = "ai-tools-v1";

export type ProductGuideStep = {
  selector: string;
  eyebrow: string;
  title: string;
  body: string;
};

export type ProductGuide = {
  id: string;
  matches: (pathname: string) => boolean;
  steps: ProductGuideStep[];
};

/** 主功能各自独立记账；进入对应真实页面时才触发。 */
export const PRODUCT_GUIDES: ProductGuide[] = [
  {
    id: "recording-entry-v1",
    // 录音入口常驻在笔记侧栏；应在用户开始录音之前解释，而不是等进入 /record。
    matches: (p) => p === "/" || /^\/notes\/[^/]+$/.test(p),
    steps: [
      {
        selector: ".record-btn",
        eyebrow: "开始使用 · 录音入口",
        title: "从这里开始一场新录音",
        body: "点击后立即进入实时转写。录制期间同一个按钮会变成“停止录制”，暂停和状态控制会出现在录音页。",
      },
    ],
  },
  {
    id: "recording-basics-v1",
    matches: (p) => p === "/record",
    steps: [
      { selector: ".controls", eyebrow: "录音 · 1 / 2", title: "从这里控制整场录音", body: "开始后可暂停或停止；计时、输入波形和当前状态都会在同一行显示。" },
      { selector: ".transcript", eyebrow: "录音 · 2 / 2", title: "转写会实时落在这里", body: "内容边说边保存。停止后会生成笔记，并按你的会后 AI 设置继续整理。" },
    ],
  },
  {
    id: "note-reading-v1",
    matches: (p) => /^\/notes\/[^/]+$/.test(p),
    steps: [
      { selector: ".topbar", eyebrow: "笔记 · 1 / 3", title: "标题、导出和播放都在这里", body: "点击标题可改名；播放器支持按时间回听，也可以继续录制同一场会议。" },
      { selector: ".transcript", eyebrow: "笔记 · 2 / 3", title: "边听边校对转写", body: "点击时间戳跳到对应音频；说话人和文字都可以在原稿中修正。" },
      { selector: ".related", eyebrow: "笔记 · 3 / 3", title: "沿关系继续找上下文", body: "相关笔记会把同一人物、项目或主题串起来，便于回溯决定的来龙去脉。" },
    ],
  },
  {
    id: "people-library-v1",
    matches: (p) => p === "/speakers",
    steps: [
      { selector: ".container > .desc", eyebrow: "会议搭子 · 1 / 2", title: "这里是本地说话人资料库", body: "命名一次后，后续录音会尝试自动认出同一个人。" },
      { selector: ".stats, .empty", eyebrow: "会议搭子 · 2 / 2", title: "优先处理待命名与重复项", body: "录音样本足够时可以试听、命名、合并；整理操作不会修改原始音频。" },
    ],
  },
  {
    id: "knowledge-graph-v1",
    matches: (p) => p === "/graph",
    steps: [
      { selector: ".graph-main", eyebrow: "关系图谱 · 1 / 2", title: "在真实关系地图中探索", body: "拖动、缩放并点击节点查看上下文；文章视角和实体视角回答不同问题。" },
      { selector: ".canvas-shell, .map-column", eyebrow: "关系图谱 · 2 / 2", title: "筛选后再深入具体关系", body: "工具栏可按实体类型和关系类型收窄范围，点击连线可以查看关系证据。" },
    ],
  },
  {
    id: "hooks-automation-v1",
    matches: (p) => p === "/hooks",
    steps: [
      { selector: ".flow-card", eyebrow: "钩子 · 1 / 2", title: "先选择工作流触发时机", body: "录制开始、停止和 AI 完成等状态都能触发自动化。" },
      { selector: ".rows", eyebrow: "钩子 · 2 / 2", title: "把会议数据交给现有工具", body: "Shell 读取环境变量，Webhook 接收 JSON；从左侧“新建钩子”进入真实配置。" },
    ],
  },
  {
    id: "settings-basics-v1",
    matches: (p) => p === "/settings",
    steps: [
      { selector: ".container section:nth-of-type(1)", eyebrow: "设置 · 1 / 3", title: "先调整日常使用偏好", body: "主题、快捷键和托盘行为会立即保存并生效。" },
      { selector: ".container section:nth-of-type(3)", eyebrow: "设置 · 2 / 3", title: "录制策略都在这里", body: "系统声音、原始音频保留和语言过滤会影响之后的新录音。" },
      { selector: ".container section:nth-of-type(4)", eyebrow: "设置 · 3 / 3", title: "模型完全由本机管理", body: "可以查看、下载或删除语音模型；操作不会影响已经保存的笔记文本。" },
    ],
  },
];
