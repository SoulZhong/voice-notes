/** 声音处理方案(spec 2026-08-10)→ 笔记页默认回放:b 档默认成品轨,其余(含未知值)双轨。 */
export function schemeToDefaultPlayback(scheme: string): "dual" | "mixed" {
  return scheme === "b" ? "mixed" : "dual";
}
