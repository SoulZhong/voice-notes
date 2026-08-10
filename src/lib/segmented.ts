// Segmented 控件的数据模型与键盘导航纯函数(组件运行时行为在 node 单测里
// 跑不起来——仓库既有约定,可测逻辑抽到这里)。

export interface SegmentedItem {
  id: string;
  label: string;
  disabled?: boolean;
  /** 悬停提示;disabled 时用来解释原因 */
  title?: string;
  /** 动作段:点击触发 onAction 而非切换,滑块不落位(如 mix-switch 的「生成成品轨」) */
  momentary?: boolean;
}

/** 方向键导航:从 current 沿 dir 环绕找下一个可选段(跳过 disabled 与 momentary);
    全都不可选时原样返回 current。 */
export function nextEnabledIndex(
  items: Pick<SegmentedItem, "disabled" | "momentary">[],
  current: number,
  dir: 1 | -1,
): number {
  const n = items.length;
  if (n === 0) return current;
  for (let step = 1; step <= n; step++) {
    const i = (((current + dir * step) % n) + n) % n;
    const it = items[i]!;
    if (!it.disabled && !it.momentary) return i;
  }
  return current;
}
