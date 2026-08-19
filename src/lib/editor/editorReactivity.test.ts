import { describe, expect, it } from "vitest";

// ProseMirror 的生命周期不归 Svelte 管:编辑器实例(及其 Plugin props、NodeView 回调)
// 会在创建它的 effect 销毁之后继续被调用。此时若回调去读 $props / $derived,Svelte 判定
// 「读一个属于已销毁 effect 的 derived」,发 derived_inert 警告**并中断本次渲染**——
// 症状是笔记详情页停在「加载中…」,既不报错也不出内容(有 AI 修订稿时必现)。
//
// 所以 PM 回调只允许读普通(非响应式)镜像变量,由 $effect 负责把 props 同步进镜像。
// 这条约束靠读源码守住:组件运行时行为没法在 node 环境的单测里跑起来。
const source = import.meta.glob(["./MarkdownEditor.svelte", "../../routes/notes/[id]/+page.svelte", "./segmentSchema.ts"], {
  eager: true,
  query: "?raw",
  import: "default",
}) as Record<string, string>;

const editor = source["./MarkdownEditor.svelte"];
const segSchema = source["./segmentSchema.ts"];
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

  it("segmentsrendered 监听器的 tick 自增必须 untrack——同步派发在 effect 追踪上下文内", () => {
    // setSegments 的 dispatchEvent 是同步的:onSegRendered 在数据同步 effect 的
    // 追踪上下文内执行,`segRenderTick++` 先读后写,裸写会把「读」登记成该 effect
    // 的依赖、「写」又立刻使它失效——effect 读写同一状态,每轮自增自触发,
    // effect_update_depth_exceeded 死循环(2026-08-09 定位;自 PR#65 即存在,被
    // hasFocus 守卫常态遮蔽:编辑器无焦点时的离散命令〔改说话人/删段〕才踩中)。
    const handler = detail.slice(detail.indexOf("const onSegRendered"));
    expect(handler).toMatch(/untrack\(\(\) => \(segRenderTick \+= 1\)\)/);
    expect(handler.slice(0, handler.indexOf("};"))).not.toMatch(/^\s*segRenderTick\+\+/m);
  });
});

// 2026-08-19 线上 bug:同一段在逐字稿里显示「新说话人 4」,而顶部说话人胶囊与改派菜单
// 同时显示「刘光浚」——同一份 note.speakers 在同一次渲染里给出两个显示名。
//
// 根因不在 speakerLabel,而在两套生命周期:胶囊/菜单是 Svelte 响应式,数据一变就重算;
// 段落徽章是 ProseMirror NodeView 里的命令式 DOM,只在构造时算一次。而"显示名"来自
// 外部 speakers 映射、不在节点 attrs 里,所以 S4 关联到人物前后,节点在 PM 眼里完全
// 没变(type/attrs/内容全同 → node.eq),它会直接复用旧 NodeView:**即便整份
// replaceWith 重建文档也不会刷新**(这一点尤其反直觉,别再把它归因到 hasFocus 守卫)。
describe("段落徽章必须跟着说话人信息变化刷新", () => {
  it("显示名进 attrs——不进去 PM 就看不见变化", () => {
    expect(segSchema).toBeTruthy();
    expect(segSchema).toMatch(/badgeKey:\s*\{\s*default:/);
  });

  it("setSegments 必须把显示名算进 attrs,而不是只传 speaker id", () => {
    // 只写 {seq, source, speaker, startMs} 的话,attrs 恒定,update() 永不触发。
    expect(editor).toMatch(/badgeKey:\s*`\$\{b\.label\}/);
    // 颜色也得进去:label 相同而配色不同的情形(本地名与新关联人物同名)否则漏更新
    expect(editor).toMatch(/\$\{b\.bg\}/);
    expect(editor).toMatch(/\$\{b\.ink\}/);
  });

  it("NodeView 必须实现 update(),否则只能整只重建——而 PM 连重建都不会做", () => {
    expect(segSchema).toMatch(/update:\s*\(next: PMNode\)\s*=>/);
    // update 里必须真的重算徽章,不能只换引用
    expect(segSchema).toMatch(/badge\.textContent\s*=\s*b\.label/);
  });

  it("update 后事件回调读新节点,不读构造时那个快照", () => {
    // 否则改派菜单会带着旧的 seq/speaker 开出来。
    expect(segSchema).toMatch(/let\s+cur\s*=\s*node/);
    expect(segSchema).toMatch(/cur\.attrs\.seq/);
    expect(segSchema).not.toMatch(/onBadgeClick\(\s*\n?\s*node\.attrs\.seq/);
  });

  it("badgeKey 不得进 docSkeleton——否则改个名字会被段结构锁当成骨架破坏而整条拒绝", () => {
    const skeleton = segSchema.slice(
      segSchema.indexOf("export function docSkeleton"),
      segSchema.indexOf("export const segmentLockPlugin"),
    );
    expect(skeleton).toBeTruthy();
    expect(skeleton).not.toContain("badgeKey");
  });
});
