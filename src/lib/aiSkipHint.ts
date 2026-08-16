/** 「这场没做 AI 整理」的可见提示判定。
 *
 * 为什么需要:执行体没配全时后端是**完全静默**降级的——`active_refine_executor`
 * 返回 None,精修、主题标题、身份推断一起跳过,笔记照常生成,界面上看不出任何异样,
 * 只有设置页角落一个徽标。2026-08-13 至 08-16 因此连断四天六场没被发现(profile 重建后
 * 接入点 ID 与密钥没填完),六场全留着默认标题。
 *
 * 判定落在 `stages.llm === "off"` 上:后端已经如实记进 aing.json 了(跑过的是
 * done/partial/failed),UI 此前只是对 "off" 什么都不说。
 */
export type AiSkipHint = "unconfigured" | "rerunnable" | null;

export function aiSkipHint(opts: {
  /** refined.stages.llm;拿不到修订稿时传 undefined。 */
  llmStage: string | undefined;
  /** 笔记已完成(录制中/收尾中还没到该跑 AI 的时候)。 */
  noteComplete: boolean;
  /** 会后 AI 功能开关。 */
  refineEnabled: boolean;
  /** 执行体配置是否齐备(口径见 refineReady)。 */
  ready: boolean;
}): AiSkipHint {
  const { llmStage, noteComplete, refineEnabled, ready } = opts;
  // 跑过就有各自的提示(partial/failed 已有 banner),不归这里管。
  if (llmStage !== "off") return null;
  if (!noteComplete) return null;
  // 用户主动关掉了会后 AI:这是明确选择,每篇笔记都唠叨一句是噪音。
  if (!refineEnabled) return null;
  // 开关开着却没跑成:要么现在仍没配全(去配置),要么已经配好了(可以补跑)。
  return ready ? "rerunnable" : "unconfigured";
}
