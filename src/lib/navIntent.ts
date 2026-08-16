/** 显式导航意图标记:一旦发生过"用户/托盘明确要求去某页",所有**自动**重定向都让路。
 *
 * 为什么需要它,而不是各处去比 `window.location.pathname`:
 * `goto()` 是异步的——调用之后到浏览器 history 真正更新之间有一个窗口,这段时间里
 * pathname 仍是旧值。冷启动那几百毫秒里同时有三个东西想导航:
 *   ① 托盘「打开设置」(显式,应当赢)
 *   ② 根路由的落地重定向(listNotes 回来后跳 /record 或最近笔记)
 *   ③ onboarding 的功能引导(settings/models IPC 回来后跳 /ai?guide=...)
 * ②③ 都是"等一个异步结果再跳",谁后回来谁覆盖前面的人。只比 pathname 挡不住
 * 「①已 goto 但还没落地」这一拍(Codex 审查连开三轮指出的正是这一族竞态)。
 *
 * 语义刻意做成**单向、进程内一次性**:置了就不再复位——自动重定向本来就只服务
 * "刚启动、用户还没表达意图"这一小段时间,之后由用户自己掌舵。
 */
let navigated = false;

/** 标记"已经发生显式导航"。必须在调用 goto **之前**同步调用。 */
export function markNavigated(): void {
  navigated = true;
}

/** 自动重定向的守卫:返回 true 说明已有显式导航,应当放弃本次自动跳转。 */
export function hasNavigated(): boolean {
  return navigated;
}

/** 仅测试用:重置标记(生产路径不复位,见文件头)。 */
export function resetNavIntentForTest(): void {
  navigated = false;
}
