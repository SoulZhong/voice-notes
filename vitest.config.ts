import { defineConfig } from "vitest/config";

// 纯函数单测:node 环境即可,无需 jsdom / SvelteKit 插件链。
export default defineConfig({
  test: {
    include: ["src/**/*.test.ts"],
    environment: "node",
  },
});
