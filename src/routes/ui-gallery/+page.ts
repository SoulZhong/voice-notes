// UI 改版(#730)的主题与组件预览台:只在开发构建里可达,生产构建直接 404。
import { error } from "@sveltejs/kit";

export function load() {
  if (!import.meta.env.DEV) error(404);
}
