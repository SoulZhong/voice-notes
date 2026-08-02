// 单实例试听控制器:同一时刻至多一个在播,再点同 key 即停。播放器工厂与状态
// 回调可注入——页面传 (src) => new Audio(convertFileSrc(src)) 与 $state 写回,
// 测试传假播放器。三处试听场景(概览已删,审阅流/详情页)语义与旧实现一致。
// onError 可选:play() 的同步异常与 Promise 拒绝都会转交(不传就静默,原语义
// 不变),让调用方能把"点了没声"的真实原因显性化(如错误横幅),而不是吞掉。
import { t } from "$lib/i18n/index.svelte";

export type PlayerLike = { play(): unknown; pause(): void; onended: (() => void) | null };

/** play() 失败转用户可读文案:已知媒体错误给中文(终端用户读不懂 WebKit 英文原文),
    未知错误保留原文——别把真实原因编码没了。 */
export function describePlayError(err: unknown): string {
  const name = (err as { name?: string } | null | undefined)?.name ?? "";
  const text = String(err);
  if (name === "NotSupportedError" || text.includes("no supported source")) {
    return t("speakers.playUnsupported");
  }
  return text;
}

export function createAudition(
  factory: (src: string) => PlayerLike,
  onChange: (key: string | null) => void,
  onError?: (msg: string) => void,
) {
  let player: PlayerLike | null = null;
  let key: string | null = null;
  const stop = () => {
    player?.pause();
    player = null;
    key = null;
    onChange(null);
  };
  const toggle = (k: string, src: string) => {
    if (key === k) {
      stop();
      return;
    }
    if (player) stop();
    const p = factory(src);
    p.onended = () => {
      if (key === k) stop();
    };
    player = p;
    key = k;
    onChange(k);
    try {
      const r = p.play();
      if (r instanceof Promise) r.catch((err) => {
        if (key === k) {
          stop();
          onError?.(describePlayError(err));
        }
      });
    } catch (err) {
      if (key === k) {
        stop();
        onError?.(describePlayError(err));
      }
    }
  };
  return {
    toggle,
    stop,
    get key() {
      return key;
    },
  };
}
