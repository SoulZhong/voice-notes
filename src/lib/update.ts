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

/** 检查更新:每次都发新请求。设置页手动「检查更新」向上抛错误;录制页启动静默查(catch)。
    不做会话缓存——录制页极少重复挂载,直接新查更简单也更稳(缓存失败会拖成永远不提示)。 */
export function checkUpdate(): Promise<UpdateInfo> {
  return invoke<UpdateInfo>("check_update");
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
