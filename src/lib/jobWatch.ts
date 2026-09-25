// 后台任务状态订阅(AI 整理 / 重新转文字 / 补生成成品轨……)的共用时序。
//
// 页面要知道「这篇笔记的某个后台任务现在在不在跑」:事件只覆盖在页期间,进页前已经
// 开跑的任务要靠补问一次后端快照。这件事的时序很容易写错,笔记页上四处各写一遍、各被
// Codex 审出过一轮竞态(Fix 3 / P2 五到八轮):
//
// 1. **先订阅、后补问**:补问必须等 listen() 的 promise 真正 resolve、监听挂到位之后才发。
//    两头并发的话,快照说「在跑」、终态事件却在监听就位前发出,页面永久卡在「进行中」。
// 2. **迟到的快照让路**:补问结果比某些事件晚到时作废(哪些事件算数由 `invalidates` 定:
//    AI 整理只认终态,中间事件不能作废快照;其余任务任何事件都算)。
// 3. **切走即解绑**:dispose 之后到达的 listen 结果立刻解绑,补问结果与回调一律丢弃。
//
// 调用方只提供「怎么订阅、事件怎么落到页面状态、补问什么」。本模块不碰 Svelte,
// 在 $effect 里调、把返回的 dispose 交还给 effect 即可。

export type Unlisten = () => void;

export interface JobWatchOptions<E, S> {
  /** 订阅事件(events.ts 的 onXxx),resolve 出解绑函数。 */
  subscribe: (cb: (e: E) => void) => Promise<Unlisten>;
  /** 只处理与本页相关的事件(通常是 note_id 比对)。 */
  matches?: (e: E) => boolean;
  /** 相关事件落到页面状态。 */
  onEvent: (e: E) => void;
  /** 该事件是否作废补问快照。缺省:任何相关事件都作废。 */
  invalidates?: (e: E) => boolean;
  /** 补问后端当前状态;监听挂到位之后才发。 */
  snapshot?: () => Promise<S>;
  /**
   * 快照落地(仅在未 dispose、且没有作废事件时调用)。`live()` 在之后任何时刻都可
   * 查询「这份快照还算不算数」——需要轮询复核的调用方(AI 整理)靠它停下。
   */
  onSnapshot?: (s: S, live: () => boolean) => void | Promise<void>;
  /** 补问结束(无论成败,含订阅本身失败);已 dispose 则不调。 */
  onSettled?: () => void;
}

export function watchJob<E, S = unknown>(opts: JobWatchOptions<E, S>): () => void {
  let disposed = false;
  let invalidated = false;
  let unlisten: Unlisten | null = null;
  const live = () => !disposed && !invalidated;

  opts
    .subscribe((e) => {
      if (disposed) return;
      if (opts.matches && !opts.matches(e)) return;
      if (opts.invalidates ? opts.invalidates(e) : true) invalidated = true;
      opts.onEvent(e);
    })
    .then(async (u) => {
      if (disposed) {
        u();
        return;
      }
      unlisten = u;
      if (!opts.snapshot) return;
      const s = await opts.snapshot();
      if (!live()) return;
      await opts.onSnapshot?.(s, live);
    })
    .catch((e) => {
      // 订阅或补问失败:以事件为准,不打断页面;留一行日志供排障。
      console.warn("watchJob:", e);
    })
    .finally(() => {
      if (!disposed && opts.snapshot) opts.onSettled?.();
    });

  return () => {
    disposed = true;
    unlisten?.();
  };
}
