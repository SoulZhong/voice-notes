import { invoke } from "@tauri-apps/api/core";
import { t } from "$lib/i18n/index.svelte";

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

/** 下载进度 → 按钮文案。总长未知(部分 CDN 不给 Content-Length)退化省略号。 */
export function updateProgressLabel(downloaded: number, total: number | undefined): string {
  if (!total) return t("settings.update.updating");
  return t("settings.update.updatingPct", { pct: Math.min(100, Math.round((downloaded / total) * 100)) });
}

/** 结构化更新进度:横幅要画进度条,光有文案不够。 */
export type UpdateProgress = {
  /** 0-100;总长未知(部分 CDN 不给 Content-Length)为 null,渲染为往复扫描。 */
  pct: number | null;
  /** 下载完毕后的签名校验+解包安装阶段(可能几十秒,进度不可知)。 */
  installing: boolean;
  /** 现成文案(设置页按钮直接用)。 */
  label: string;
};

/** 网络请求超时(查询与下载各自适用):连接悬死不能让按钮永远停在「更新中」。 */
const UPDATE_TIMEOUT_MS = 30_000;

// 单飞锁:横幅(+layout)与设置页两个入口同时可见,各自组件态互不知情,
// 不加锁会并发两趟 downloadAndInstall(双下载、安装互踩)。进行中再调用
// 直接复用同一 promise(后来者的进度回调不接线,其按钮停在初始「更新中…」,
// 与真实进度共享同一次安装)。
let inFlight: Promise<"none"> | null = null;

/** 一键更新:updater 插件查 → 下载(回调进度文案)→ 安装 → 重启。
 * 返回 "none" = 端上认为没有更新(与 check_update 的 GitHub API 判断可能短暂不一致,
 * 如 Release 刚发但 latest.json 未就绪);安装成功后 relaunch 不返回
 * (Windows 上安装器会先行结束应用,relaunch 根本执行不到——由安装器负责收尾)。
 * 任何失败向上抛:调用方兜底到「打开发布页」手动路径——一键更新是增强,
 * 不能因签名/网络问题把用户堵死在无法更新的状态。 */
export function applyUpdate(onProgress: (p: UpdateProgress) => void): Promise<"none"> {
  if (!inFlight) {
    inFlight = doApplyUpdate(onProgress).finally(() => (inFlight = null));
  }
  return inFlight;
}

async function doApplyUpdate(onProgress: (p: UpdateProgress) => void): Promise<"none"> {
  const { check } = await import("@tauri-apps/plugin-updater");
  const { relaunch } = await import("@tauri-apps/plugin-process");
  const update = await check({ timeout: UPDATE_TIMEOUT_MS });
  if (!update) return "none";
  let downloaded = 0;
  let total: number | undefined;
  await update.downloadAndInstall(
    (event) => {
      if (event.event === "Started") {
        total = event.data.contentLength ?? undefined;
      } else if (event.event === "Progress") {
        downloaded += event.data.chunkLength;
        onProgress({
          pct: total ? Math.min(100, Math.round((downloaded / total) * 100)) : null,
          installing: false,
          label: updateProgressLabel(downloaded, total),
        });
      } else if (event.event === "Finished") {
        // 下载完成后还有签名校验+解包安装,可能要几十秒;不处理会停在
        // 「更新中 100%」像卡死。
        onProgress({ pct: null, installing: true, label: t("settings.update.installing") });
      }
    },
    { timeout: UPDATE_TIMEOUT_MS },
  );
  await relaunch();
  return "none"; // relaunch 后通常到不了这里,类型完整性兜底
}
