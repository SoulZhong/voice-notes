import { onPlayerPos } from "$lib/events";

/** 活动播放会话。由用户**真正开始播放**建立,不是装载建立——播放器进笔记页就会
    自动装载,若用装载判定,只看过没播过的笔记离页后也会冒出迷你条。 */
export type PlaybackSession = {
  /** 后端装载代次:位置事件靠它辨认归属。 */
  gen: number;
  noteId: string;
  title: string;
  totalMs: number;
};

/** 迷你条显示判定:有会话,且当前不在这篇笔记自己的详情页上。
    路径比较只看 pathname 的第一段笔记 id,尾斜杠/查询串/hash 不参与。 */
export function shouldShowMiniPlayer(noteId: string | null, pathname: string): boolean {
  if (!noteId) return false;
  const path = pathname.split(/[?#]/)[0].replace(/\/+$/, "");
  return path !== `/notes/${noteId}`;
}

/** 组件卸载时要不要停内核。所有权模型:内核归**会话**所有,不再归组件所有。
    - 从未成功装载 → 无核可收
    - 本组件装的核正是活动会话 → 不停(会话接管)
    - 其余 → 停,与本功能引入前语义一致 */
export function shouldStopOnCleanup(
  lastBackendGen: number | null,
  sessionGen: number | null,
): boolean {
  if (lastBackendGen === null) return false;
  return lastBackendGen !== sessionGen;
}

class Playback {
  session = $state<PlaybackSession | null>(null);
  currentMs = $state(0);
  playing = $state(false);

  begin(s: PlaybackSession) {
    this.session = s;
    this.currentMs = 0;
    this.playing = true;
  }

  clear() {
    this.session = null;
    this.currentMs = 0;
    this.playing = false;
  }

  /** 位置事件入口:只接受属于当前会话代次的事件。丢弃的是"停 A 装 B"窗口里
      A 的排队事件——它们若被采纳,A 的位置会写进 B 的界面。 */
  applyPos(e: { pos_ms: number; playing: boolean; gen: number }) {
    if (!this.session || e.gen !== this.session.gen) return;
    this.currentMs = Math.min(e.pos_ms, this.session.totalMs);
    this.playing = e.playing;
  }

  /** 后台播放期间笔记被改名 → 迷你条标题跟着更新,否则会一直显示旧名。 */
  rename(noteId: string, title: string) {
    if (this.session?.noteId === noteId) this.session = { ...this.session, title };
  }

  /** 同篇重装后恢复会话:代次换新,位置与播放态沿用重装前的现场。
      不能用 begin ——它把 currentMs 归零、playing 置真,那是「新开一段播放」的语义,
      重装场景下会把迷你条进度打回 0。 */
  restore(s: PlaybackSession, atMs: number, playing: boolean) {
    this.session = s;
    this.currentMs = atMs;
    this.playing = playing;
  }
}

export const playback = new Playback();

/** 全局位置订阅:必须放在 store 而不是 AudioPlayer 组件里——组件一卸载订阅就没了,
    迷你条的进度会僵住。应用启动时由 +layout.svelte 调一次。 */
export function startPlaybackSubscriptions(): () => void {
  const un = onPlayerPos((e) => playback.applyPos(e));
  return () => {
    un.then((f) => f());
  };
}
