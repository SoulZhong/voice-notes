/// 实时转写回看的纯函数层:搜索命中/说话人过滤/时间轴定位。
/// UI 状态(query、选中集、follow 联动)留在页面,这里只做无状态判定,便于单测锁定口径。

export function searchHits(lines: { text: string }[], query: string): number[] {
  const q = query.trim().toLowerCase();
  if (!q) return [];
  const hits: number[] = [];
  lines.forEach((l, i) => {
    if (l.text.toLowerCase().includes(q)) hits.push(i);
  });
  return hits;
}

export function matchesSpeakerFilter(
  line: { speaker: string | null },
  selected: ReadonlySet<string>,
): boolean {
  return selected.size === 0 || (line.speaker !== null && selected.has(line.speaker));
}

export function nearestIndexByMs(lines: { start_ms: number }[], targetMs: number): number {
  let best = -1;
  let bestD = Infinity;
  lines.forEach((l, i) => {
    const d = Math.abs(l.start_ms - targetMs);
    if (d < bestD) { bestD = d; best = i; }
  });
  return best;
}
