import type { Settings } from "./models";

/** 会后 AI 配置是否齐备,口径对齐后端 readiness 判定(src-tauri/src/lib.rs
 * refine_llm_ready/refine_agent_ready,约 266-278 行):
 *  - openai 档(provider != "agent"):base_url/model/api_key 三项均非空;
 *  - agent 档:只看 provider == "agent" 本身——bin 由运行时探测(未装 CLI 时 refine
 *    阶段自行落 failed + 日志),探测结果不计入"是否配置完成",后端同样不检查。
 * 与 refine_enabled 无关:调用方按 refineOn && !refineReady(settings) 才决定是否
 * 显示"未配置完成"徽标。 */
export function refineReady(
  s: Pick<Settings, "refine_provider" | "refine_base_url" | "refine_model" | "refine_api_key">,
): boolean {
  if (s.refine_provider === "agent") return true;
  return !!s.refine_base_url && !!s.refine_model && !!s.refine_api_key;
}
