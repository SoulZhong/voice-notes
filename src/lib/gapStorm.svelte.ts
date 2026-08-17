import { onSourceHealth, type SourceHealthEvent } from "$lib/events";
import { recording } from "$lib/recording.svelte";
import { nextGapStorm, type GapStormState } from "$lib/gapStorm";

export type { GapStormState };

class GapStorm {
  mic = $state<number | null>(null);
  system = $state<number | null>(null);

  apply(ev: SourceHealthEvent) {
    const next = nextGapStorm({ mic: this.mic, system: this.system }, ev, recording.isLive);
    this.mic = next.mic;
    this.system = next.system;
  }

  clear() {
    this.mic = null;
    this.system = null;
  }
}

/**
 * 模块级单例,**不是页面局部状态**(Codex 二轮 P2)。
 *
 * 录制中离开 /record 再回来,页面组件会重新挂载、局部状态归零;而后端此时
 * `armed=false`,持续的风暴不会重发上升沿,横幅就在风暴仍在继续时永久消失了。
 * 状态活得比页面长,才谈得上"显示到风暴平息"。
 */
export const gapStorm = new GapStorm();

/**
 * 全局订阅。放在 layout 里起,不放页面里——组件一卸载订阅就没了
 * (与 startPlaybackSubscriptions 同一条教训)。返回值供 `$effect` 清理。
 */
export function startGapStormSubscription(): () => void {
  const un = onSourceHealth((e) => gapStorm.apply(e));
  return () => {
    void un.then((f) => f());
  };
}
