// 批量改说话人的纯选择逻辑(2026-08-22,甲乙双模式)。
export type SegLite = { seq: number; speaker?: string | null };

/** 含 seq 的「连续同说话人」段落 seq 列表(按展示序,向两侧扩到说话人变化为止)。 */
export function contiguousRun(segs: SegLite[], seq: number): number[] {
  const i = segs.findIndex((s) => s.seq === seq);
  if (i < 0) return [];
  const sp = segs[i].speaker ?? null;
  let a = i;
  while (a > 0 && (segs[a - 1].speaker ?? null) === sp) a--;
  let b = i;
  while (b + 1 < segs.length && (segs[b + 1].speaker ?? null) === sp) b++;
  return segs.slice(a, b + 1).map((s) => s.seq);
}

/** 展示序上 a、b 两段之间(含端点)的全部 seq(Shift 选区间用;a/b 顺序无关)。 */
export function seqRange(segs: SegLite[], a: number, b: number): number[] {
  const ia = segs.findIndex((s) => s.seq === a);
  const ib = segs.findIndex((s) => s.seq === b);
  if (ia < 0 || ib < 0) return [];
  const [lo, hi] = ia <= ib ? [ia, ib] : [ib, ia];
  return segs.slice(lo, hi + 1).map((s) => s.seq);
}

/** 同源双路清洗(2026-08-23):与 system 活动重叠 ≥80% 的 mic 段 seq(高危回声段,
    与后端断喂同一判据)。 */
export function overlappedMicSeqs(
  segs: { seq: number; source: string; start_ms: number; end_ms: number }[],
): number[] {
  const sys = segs.filter((s) => s.source === "system").map((s) => [s.start_ms, s.end_ms] as const);
  const out: number[] = [];
  for (const s of segs) {
    if (s.source !== "mic") continue;
    const dur = Math.max(s.end_ms - s.start_ms, 1);
    let ov = 0;
    for (const [a, b] of sys) ov += Math.max(0, Math.min(s.end_ms, b) - Math.max(s.start_ms, a));
    if (ov / dur >= 0.8) out.push(s.seq);
  }
  return out;
}
