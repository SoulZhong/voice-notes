/** 声音处理方案(spec 2026-08-10)→ 笔记页默认回放:b 档默认成品轨,其余(含未知值)双轨。 */
export function schemeToDefaultPlayback(scheme: string): "dual" | "mixed" {
  return scheme === "b" ? "mixed" : "dual";
}

/** 成品轨回放的回落判定(笔记页回落 effect 的纯逻辑):pending 期间不判——
    mixedInfo=null 是"未知"不是"确无",抢跑会把 b 档默认成品轨秒降回双轨(终审回归实锤)。
    落定后:无轨/不可信 → 回落双轨。 */
export function shouldFallbackToDual(
  pending: boolean,
  scheme: "dual" | "mixed",
  info: { track: unknown | null; untrusted: string | null } | null,
): boolean {
  if (pending || scheme !== "mixed") return false;
  return !info?.track || info.untrusted !== null;
}
