import type { Dict, Msg } from "../types";

// 跨页面共享的通用文案 + 语言设置项自身。语言名恒用母语呈现(业界惯例:
// 在英文界面里也要让中文用户认得出"中文"),故 zh/en 两边同值。
export const zh = {
  "common.language.label": "语言 / Language",
  "common.language.system": "跟随系统",
  "common.language.zh": "中文",
  "common.language.en": "English",
  "common.language.switchFailed": "切换语言失败: {e}",
  "common.loadFailed": "加载失败: {e}",
  "common.edit": "编辑",
  "common.confirm": "确认",
  "common.notSet": "未填写",
  "common.saveFailed": "保存失败: {e}",
  "common.deleteFailed": "删除失败: {e}",
  "common.none": "—",
  "common.close": "关闭",
} as const satisfies Dict;

export const en = {
  "common.language.label": "Language / 语言",
  "common.language.system": "Follow system",
  "common.language.zh": "中文",
  "common.language.en": "English",
  "common.language.switchFailed": "Failed to switch language: {e}",
  "common.loadFailed": "Failed to load: {e}",
  "common.edit": "Edit",
  "common.confirm": "Confirm",
  "common.notSet": "Not set",
  "common.saveFailed": "Failed to save: {e}",
  "common.deleteFailed": "Failed to delete: {e}",
  "common.none": "—",
  "common.close": "Close",
} satisfies Record<keyof typeof zh, Msg>;
