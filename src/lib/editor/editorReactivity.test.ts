import { describe, expect, it } from "vitest";

// ProseMirror 的生命周期不归 Svelte 管:编辑器实例(及其 Plugin props、NodeView 回调)
// 会在创建它的 effect 销毁之后继续被调用。此时若回调去读 $props / $derived,Svelte 判定
// 「读一个属于已销毁 effect 的 derived」,发 derived_inert 警告**并中断本次渲染**——
// 症状是笔记详情页停在「加载中…」,既不报错也不出内容(有 AI 修订稿时必现)。
//
// 所以 PM 回调只允许读普通(非响应式)镜像变量,由 $effect 负责把 props 同步进镜像。
// 这条约束靠读源码守住:组件运行时行为没法在 node 环境的单测里跑起来。
const source = import.meta.glob(["./MarkdownEditor.svelte", "../../routes/notes/[id]/+page.svelte"], {
  eager: true,
  query: "?raw",
  import: "default",
}) as Record<string, string>;

const editor = source["./MarkdownEditor.svelte"];
const detail = source["../../routes/notes/[id]/+page.svelte"];

describe("编辑器与 Svelte 响应式的边界", () => {
  it("ProseMirror 的 editable / canEdit 回调不得直接读响应式 prop", () => {
    expect(editor).toBeTruthy();
    // 直接读 prop 的写法(`() => editable`)正是 derived_inert 的来源。
    expect(editor).not.toMatch(/editable:\s*\(\)\s*=>\s*editable\b/);
    expect(editor).not.toMatch(/canEdit:\s*\(\)\s*=>\s*editable\b/);
  });

  it("存在非响应式镜像变量,且 PM 回调读的是它", () => {
    // 镜像必须是普通 let(不能是 $state/$derived,否则等于没脱钩)。
    expect(editor).toMatch(/let\s+editableNow\s*=/);
    expect(editor).not.toMatch(/let\s+editableNow\s*=\s*\$(state|derived)/);
    expect(editor).toMatch(/editable:\s*\(\)\s*=>\s*editableNow/);
    expect(editor).toMatch(/canEdit:\s*\(\)\s*=>\s*editableNow/);
  });

  it("详情页的 id 复位 effect 只依赖 id,读 refinedEditor 必须 untrack", () => {
    // 这个 effect 会把 note/refined 清空以显示加载态。若它裸读 refinedEditor
    // (bind:this 的 $state),有修订稿的笔记就进入自毁循环:编辑器挂载 → refinedEditor
    // 由 null 变实例 → effect 重跑 → 清空 note/refined → 编辑器卸载 → refinedEditor
    // 回到 null → effect 再跑……页面永远停在「加载中…」,全程不报错。
    expect(detail).toBeTruthy();
    // 用注释锚点圈出这个复位 effect(onDestroy 里也有一处 flushRefined,那里是裸读且
    // 合法——onDestroy 不是 effect,不建立响应式依赖,不能被这条断言误伤)。
    const start = detail.indexOf("// 只在 id 变化时清");
    expect(start, "复位 effect 的注释锚点不见了,测试需同步更新").toBeGreaterThan(-1);
    const effect = detail.slice(start, detail.indexOf("refining = false;", start));
    expect(effect).toContain("untrack(() => refinedEditor?.flushRefined(true))");
    expect(effect).not.toMatch(/^\s*refinedEditor\?\./m);
  });

  it("建编辑器前先对齐一次镜像,首帧不会用错默认值", () => {
    // 宿主传 editable={false} 而 $effect 尚未跑时,首帧必须已是 false,
    // 否则编辑器会以可编辑状态挂载(录制中的当前笔记本应只读)。
    const mount = editor.slice(editor.indexOf("onMount(async () => {"));
    expect(mount).toContain("editableNow = editable");
    expect(mount.indexOf("editableNow = editable")).toBeLessThan(mount.indexOf("Editor.make()"));
  });

  it("镜像由 $effect 同步,保证 editable 变化仍能即时生效", () => {
    // 原有 effect 已经在追踪 editable 并派发空事务让 PM 重读;镜像赋值必须在它里面,
    // 且要在 dispatch 之前——否则 PM 重读到的还是旧值。
    const effect = editor.slice(editor.indexOf("$effect(() => {\n    void editable;"));
    expect(effect).toContain("editableNow = editable");
    expect(effect.indexOf("editableNow = editable")).toBeLessThan(effect.indexOf("view.dispatch"));
  });
});
