import { svelte } from "@sveltejs/vite-plugin-svelte";
import { defineConfig } from "vitest/config";

// node 环境单测,无需 jsdom。挂 svelte 插件仅为编译 .svelte.ts rune 模块
// (i18n 单例被 notes.ts 等普通模块引用,测试导入链会穿过它);$state 信号在 node 可用。
export default defineConfig({
  plugins: [svelte()],
  // $lib 是 SvelteKit 的别名;单测不走 kit 插件链,这里手动对齐,让被测模块可用 $lib 导入。
  resolve: {
    alias: { $lib: new URL("./src/lib", import.meta.url).pathname },
  },
  test: {
    include: ["src/**/*.test.ts"],
    environment: "node",
  },
});
