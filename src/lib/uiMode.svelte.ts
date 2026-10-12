// 界面模式(简洁/完整,见 CONTEXT.md「界面模式」)与左侧菜单显示哪些入口。
// 侧栏读它,设置页改设置时写它——设置本身没有变更事件,不经这里侧栏就要等重启才刷新。
import { getSettings, type Settings } from "$lib/models";

export const uiMode = $state({
  /** 完整模式(settings.show_all_settings)。 */
  full: true,
  /** 「设备」总开关(settings.device_enabled)。 */
  deviceEnabled: true,
  /** 连接过设备(settings.device_name 有值)。 */
  devicePaired: false,
});

/** 「钩子」面向开发者:只在完整模式显示。 */
export const showHooksTab = () => uiMode.full;

/** 「设备」:总开关开着,且完整模式或连接过设备(没有硬件的人看到只会困惑)。 */
export const showDeviceTab = () => uiMode.deviceEnabled && (uiMode.full || uiMode.devicePaired);

export function syncUiMode(s: Settings) {
  uiMode.full = s.show_all_settings;
  uiMode.deviceEnabled = s.device_enabled !== false;
  uiMode.devicePaired = !!s.device_name;
}

/** 启动时按磁盘上的设置对齐一次;读失败保持默认(完整)。 */
export async function loadUiMode() {
  try {
    syncUiMode(await getSettings());
  } catch {
    // 保持默认
  }
}
