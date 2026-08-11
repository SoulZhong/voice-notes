import type { Settings } from "./models";

/** 执行体引用是否就绪,口径对齐后端 settings::executor_ready(执行体分层):
 *  - "llm:<id>":档案存在且 base_url/model/api_key 三项均非空;
 *  - "agent:<kind>":引用即就绪——bin 由运行时探测,不计入"是否配置完成";
 *  - 空/悬空引用:未就绪。
 * 与 refine_enabled 无关:调用方按 refineOn && !refineReady(settings) 才决定是否
 * 显示"未配置完成"徽标。 */
export function executorReady(
  s: Pick<Settings, "llm_profiles">,
  executor: string,
): boolean {
  if (executor.startsWith("agent:")) return true;
  if (executor.startsWith("llm:")) {
    const id = executor.slice(4);
    const p = s.llm_profiles.find((x) => x.id === id);
    return !!p && !!p.base_url.trim() && !!p.model.trim() && !!p.api_key.trim();
  }
  return false;
}

/** 会后 AI(整理)配置是否齐备。 */
export function refineReady(s: Pick<Settings, "llm_profiles" | "refine_executor">): boolean {
  return executorReady(s, s.refine_executor.trim());
}
