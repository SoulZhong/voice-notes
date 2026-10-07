// 「设备」一级菜单显不显示:跟设置里的「设备」开关(settings.device_enabled)走,默认开。
// 侧栏读它,设置页拨开关时写它——设置本身没有变更事件,不经这里侧栏就要等重启才刷新。
import { getSettings } from "$lib/models";

export const deviceMenu = $state({ visible: true });

/** 启动时按磁盘上的设置对齐一次;读失败保持默认(显示)。 */
export async function loadDeviceMenu() {
  try {
    deviceMenu.visible = (await getSettings()).device_enabled !== false;
  } catch {
    // 保持默认
  }
}
