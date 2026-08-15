import { describe, expect, it } from "vitest";

/* 迷你播放浮层的底部安全区契约。
 *
 * 浮层是 position:fixed 的圆,不占布局流,自己不会给任何内容让位;唯一的避让手段是
 * +layout.svelte 给主内容区留出的 padding-bottom。这份留白与浮层的尺寸/离边距必须
 * 出自同一组 token,一旦哪边改成硬编码数字,另一边不会跟着动——浮层就重新压住右下角
 * 内容(设置页最后一行的「检查更新」按钮是历史现场)。这类漂移在真机上才看得见,
 * 单测里量不了几何,所以这里守的是"数字只有一份"这个前提。
 *
 * 2026-08-15 用无头 Chromium 复刻 shell/main/graph 的整条类链实测:视口 800 高时,
 * graph 页(height:100% + 自管理滚动)的内容底边落在 696px,浮层顶边 712px,不相交。
 * 也就是说 PR #108/#109 记的"内部自管理滚动的页面父级安全区帮不上忙"并不成立:
 * 百分比高度按 .main 的**内容盒**解析,padding 已经把整页抬到了浮层上方。
 * 保住这个结论的前提同样是下面这几条——所以真出回归,先回来看这里。
 */

type Fs = { readFileSync(path: string, encoding: "utf8"): string };
type Runtime = typeof globalThis & {
  process: { cwd(): string; getBuiltinModule(name: "fs"): Fs };
};

const runtime = globalThis as Runtime;
const fs = runtime.process.getBuiltinModule("fs");
const read = (path: string) => fs.readFileSync(`${runtime.process.cwd()}/${path}`, "utf8");

describe("迷你浮层底部安全区", () => {
  it("尺寸与离边距只在 app.css 定义一次", () => {
    const css = read("src/app.css");
    expect(css).toMatch(/--orb-size:\s*\d+px;/);
    expect(css).toMatch(/--orb-inset:\s*\d+px;/);
  });

  it("浮层自身的定位/尺寸全部取 token,不写死像素", () => {
    const orb = read("src/lib/MiniPlayer.svelte");
    for (const decl of [
      "right: var(--orb-inset)",
      "bottom: var(--orb-inset)",
      "width: var(--orb-size)",
      "height: var(--orb-size)",
    ]) {
      expect(orb, decl).toContain(decl);
    }
  });

  it("安全区高度 = 直径 + 上下各一份留白,按同一组 token 算", () => {
    const layout = read("src/routes/+layout.svelte");
    expect(layout).toContain("padding-bottom: calc(var(--orb-size) + var(--orb-inset) * 2)");
  });

  it("安全区与浮层同源判定,不各判各的", () => {
    // 两边都用 shouldShowMiniPlayer:各造一份判定就会出现"浮层显示了却没留安全区"
    // (压住内容)或"没显示却留了一块空白"。
    const layout = read("src/routes/+layout.svelte");
    const orb = read("src/lib/MiniPlayer.svelte");
    expect(layout).toContain("shouldShowMiniPlayer(playback.session?.noteId ?? null, $page.url.pathname)");
    expect(orb).toContain("shouldShowMiniPlayer(playback.session?.noteId ?? null, $page.url.pathname)");
    expect(layout).toContain("class:with-orb-safearea={showMiniPlayer}");
  });
});
