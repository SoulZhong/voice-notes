/** 显式导航的版本计数:让"等异步结果再跳"的自动重定向能判断——我在等的这段时间里,
 * 有没有人明确要求去别处?有就放弃本次跳转。
 *
 * 为什么不是一个布尔标记:布尔一旦置上就永久生效(第一版就是这么写的),用过一次托盘
 * 「打开设置」之后,再点侧栏回根路由,落地重定向会被永久压住,根页面从此空白
 * (Codex 四轮指出)。计数是**每个调用方各自比对**的:只关心"我 await 期间有没有变",
 * 不留任何长期状态。
 *
 * 为什么不能只比 `window.location.pathname`:`goto()` 是异步的,调用之后到 history
 * 更新之间有一拍,pathname 仍是旧值。冷启动那几百毫秒里同时有三个东西想导航——
 * 托盘「打开设置」(显式)、根路由落地重定向、onboarding 功能引导,后两者都是"等 IPC
 * 回来再跳",谁后回来谁覆盖。计数守卫挡的就是这一拍。
 *
 * 用法:
 * ```ts
 * const v = navVersion();
 * const data = await somethingAsync();
 * if (navVersion() !== v) return; // 期间已有显式导航,让路
 * goto(...);
 * ```
 */
let version = 0;

/** 标记一次显式导航。必须在调用 goto **之前**同步调用。 */
export function markNavigated(): void {
  version += 1;
}

/** 当前版本号。异步流程在 await 前后各取一次比对。 */
export function navVersion(): number {
  return version;
}

/** 仅测试用:复位计数。 */
export function resetNavIntentForTest(): void {
  version = 0;
}
