/** 按 key 只执行一次的懒加载去重:响应式 effect 反复重跑时,同一 key 不再重复发请求
    (会议上下文懒加载曾放大成 N² 次 person_notes)。失败不重试,吞掉拒绝——调用方
    自己写空态兜底,与 notesCache 的页面会话语义一致。 */
export function keyedOnce<K>(fn: (key: K) => Promise<void>): (key: K) => void {
  const seen = new Set<K>();
  return (key) => {
    if (seen.has(key)) return;
    seen.add(key);
    void fn(key).catch(() => {});
  };
}
