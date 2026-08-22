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
