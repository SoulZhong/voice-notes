import { invoke } from "@tauri-apps/api/core";

export type UpdateInfo = {
  /** 当前应用版本。 */
  current: string;
  /** GitHub 最新 Release 版本(已剥 v 前缀)。 */
  latest: string;
  /** latest 是否严格新于 current。 */
  has_update: boolean;
  /** 发布页 URL(含 changelog + DMG),「查看更新」直接打开。 */
  url: string;
  /** 该版本更新说明(可能为空)。 */
  notes: string;
};

/** 一次性检查(设置页手动「检查更新」用):每次都发新请求,失败向上抛。 */
export function checkUpdate(): Promise<UpdateInfo> {
  return invoke<UpdateInfo>("check_update");
}

// 本会话是否已自动查过(录制页每次挂载都会跑,避免重复请求 GitHub)。
// undefined=未查过;null=查过但无更新/失败;UpdateInfo=有更新。
let sessionResult: UpdateInfo | null | undefined;

/** 启动/进录制页时静默查一次,整个会话缓存结果。失败静默(返回 null)。 */
export async function checkUpdateOncePerSession(): Promise<UpdateInfo | null> {
  if (sessionResult !== undefined) return sessionResult;
  try {
    const u = await checkUpdate();
    sessionResult = u.has_update ? u : null;
  } catch {
    sessionResult = null; // 断网/限流:静默,不打扰
  }
  return sessionResult;
}

const DISMISS_KEY = "vn.updateDismissed";

/** 用户是否已对该版本点过「知道了」(下个新版仍会重新提示)。 */
export function updateDismissed(latest: string): boolean {
  try {
    return localStorage.getItem(DISMISS_KEY) === latest;
  } catch {
    return false;
  }
}

/** 记住忽略了该版本。 */
export function dismissUpdate(latest: string): void {
  try {
    localStorage.setItem(DISMISS_KEY, latest);
  } catch {
    /* localStorage 不可用:本会话内靠组件状态隐藏即可 */
  }
}
