/**
 * 功能引导使用稳定 ID 独立记账，不能复用全局布尔值。
 * 新功能或引导内容发生重大变化时新增/升级 ID；旧引导不会因此重播。
 */
export const AI_TOOLS_GUIDE_ID = "ai-tools-v1";

export type ProductGuideStep = {
  selector: string;
  /** i18n 键(shell 分片);展示时由 ContextGuide 经 t() 解析,模块常量不锁死语言。 */
  eyebrow: string;
  title: string;
  body: string;
};

export type ProductGuide = {
  id: string;
  matches: (pathname: string) => boolean;
  steps: ProductGuideStep[];
};

/** 生成一步的三个 i18n 键(文案本体在 src/lib/i18n/dict/shell.ts)。 */
const step = (selector: string, key: string): ProductGuideStep => ({
  selector,
  eyebrow: `shell.guide.${key}.eyebrow`,
  title: `shell.guide.${key}.title`,
  body: `shell.guide.${key}.body`,
});

/** 主功能各自独立记账；进入对应真实页面时才触发。 */
export const PRODUCT_GUIDES: ProductGuide[] = [
  {
    id: "recording-entry-v1",
    // 录音入口常驻在笔记侧栏；应在用户开始录音之前解释，而不是等进入 /record。
    matches: (p) => p === "/" || /^\/notes\/[^/]+$/.test(p),
    steps: [step(".record-btn", "recordingEntry.s1")],
  },
  {
    id: "recording-basics-v1",
    matches: (p) => p === "/record",
    steps: [step(".controls", "recordingBasics.s1"), step(".transcript", "recordingBasics.s2")],
  },
  {
    id: "note-reading-v1",
    matches: (p) => /^\/notes\/[^/]+$/.test(p),
    steps: [
      step(".topbar", "noteReading.s1"),
      step(".transcript", "noteReading.s2"),
      step(".related", "noteReading.s3"),
    ],
  },
  {
    id: "people-library-v1",
    matches: (p) => p === "/speakers",
    steps: [step(".container > .desc", "peopleLibrary.s1"), step(".stats, .empty", "peopleLibrary.s2")],
  },
  {
    id: "knowledge-graph-v1",
    matches: (p) => p === "/graph",
    steps: [
      step(".graph-main", "knowledgeGraph.s1"),
      step(".canvas-shell, .map-column", "knowledgeGraph.s2"),
    ],
  },
  {
    id: "hooks-automation-v1",
    matches: (p) => p === "/hooks",
    steps: [step(".flow-card", "hooksAutomation.s1"), step(".rows", "hooksAutomation.s2")],
  },
  {
    id: "settings-basics-v1",
    matches: (p) => p === "/settings",
    steps: [
      step(".container section:nth-of-type(1)", "settingsBasics.s1"),
      step(".container section:nth-of-type(3)", "settingsBasics.s2"),
      step(".container section:nth-of-type(4)", "settingsBasics.s3"),
    ],
  },
];
