// Critical 1 回归测试:entity_mention 的 toMarkdown runner 必须阻止 milkdown
// SerializerState 对同一文本节点重复调用默认 text runner,否则带实体 mark 的
// 文本会被序列化两次(如 "张三" → "张三张三")。
//
// 用真实序列化管线验证(而非重新实现一份影子逻辑):从 refinedSchema.ts 导出的
// entityMentionToMarkdown 直接塞进一个最小 prosemirror-model Schema 的
// mark.toMarkdown,用 @milkdown/kit/transformer 的 SerializerState 跑真实的
// #runNode/#matchTarget 分发逻辑。SerializerState.build() 返回 mdast 树,
// 不需要 remark-stringify 就能断言文本没有被重复输出——避免引入未在
// package.json 声明的 remark/unified 依赖。
import { describe, expect, it } from "vitest";
import { Schema } from "@milkdown/kit/prose/model";
import { SerializerState } from "@milkdown/kit/transformer";
import type { MarkdownNode } from "@milkdown/kit/transformer";
import { entityMentionToMarkdown } from "./refinedSchema";
import { isBlockTextEmpty } from "./editorDoc";

const schema = new Schema({
  nodes: {
    doc: {
      content: "paragraph+",
      toMarkdown: {
        match: (node: { type: { name: string } }) => node.type.name === "doc",
        runner: (state: SerializerState, node: { content: unknown }) => {
          state.openNode("root");
          state.next(node.content as never);
        },
      },
    },
    paragraph: {
      content: "text*",
      group: "block",
      toMarkdown: {
        match: (node: { type: { name: string } }) => node.type.name === "paragraph",
        runner: (state: SerializerState, node: { content: unknown }) => {
          state.openNode("paragraph");
          state.next(node.content as never);
          state.closeNode();
        },
      },
    },
    text: {
      group: "inline",
      toMarkdown: {
        match: (node: { type: { name: string } }) => node.type.name === "text",
        runner: (state: SerializerState, node: { text?: string | null }) => {
          state.addNode("text", undefined, node.text ?? "");
        },
      },
    },
  },
  marks: {
    // 真实 spec:与 refinedSchema.ts 里 entityMentionSchema 实际使用的对象同一份。
    entity_mention: {
      attrs: { entityId: { default: "" } },
      toMarkdown: entityMentionToMarkdown,
    },
  },
} as never);

function collectText(node: MarkdownNode): string {
  if (typeof node.value === "string") return node.value;
  return (node.children ?? []).map(collectText).join("");
}

describe("entity_mention toMarkdown(真实 SerializerState 管线)", () => {
  it("含 entity_mention mark 的文本只序列化一次,不重复", () => {
    const mark = schema.marks.entity_mention!.create({ entityId: "P1" });
    const doc = schema.node("doc", null, [
      schema.node("paragraph", null, [
        schema.text("我和"),
        schema.text("张三", [mark]),
        schema.text("开会"),
      ]),
    ]);

    const state = new SerializerState(schema);
    state.run(doc);
    const tree = state.build();
    const text = collectText(tree);

    expect(text).toBe("我和张三开会");
    expect(text).not.toContain("张三张三");
  });

  it("entityMentionToMarkdown.runner 返回 true(prevent-default 契约)", () => {
    const mark = schema.marks.entity_mention!.create({ entityId: "P1" });
    const node = schema.text("张三", [mark]);
    const state = new SerializerState(schema);
    const result = entityMentionToMarkdown.runner(state, mark, node);
    expect(result).toBe(true);
  });
});

// Round 2 Critical 1 回归:MarkdownEditor.svelte 的 collectBlocks/markSaved 都
// 靠 blockMarkdown 判断"这个块是不是空的",判定只信 PM 节点的 textContent,
// 绝不信任 serializer 的输出——因为 commonmark 对空 paragraph(非文档末块)的
// 真实序列化行为是吐出 "<br />" 字面量(见
// node_modules/@milkdown/preset-commonmark/src/node/paragraph.ts:
// `state.addNode('html', undefined, '<br />')`),这是个 truthy 字符串。这里
// 用一份复刻该真实行为的最小 schema 跑真实 SerializerState,证明:即使
// serializer 真的输出了 truthy 的 "<br />",空段的 textContent 依然是 ""、
// isBlockTextEmpty 依然判它为空——这正是 blockMarkdown 不能直接信任 serializer
// 输出、必须先用 isBlockTextEmpty 兜底的原因。
describe("空 paragraph 的 serializer 输出不可信(commonmark 会吐 <br />)", () => {
  const brSchema = new Schema({
    nodes: {
      doc: {
        content: "paragraph+",
        toMarkdown: {
          match: (node: { type: { name: string } }) => node.type.name === "doc",
          runner: (state: SerializerState, node: { content: unknown }) => {
            state.openNode("root");
            state.next(node.content as never);
          },
        },
      },
      paragraph: {
        content: "text*",
        group: "block",
        toMarkdown: {
          match: (node: { type: { name: string } }) => node.type.name === "paragraph",
          // 复刻 @milkdown/preset-commonmark 的 paragraph.toMarkdown.runner:
          // 空段(非文档末块)吐 "<br />" 字面量,否则正常序列化文本内容。
          runner: (state: SerializerState, node: { content: { size: number } }) => {
            state.openNode("paragraph");
            if (node.content.size === 0) {
              state.addNode("html", undefined, "<br />");
            } else {
              state.next(node.content as never);
            }
            state.closeNode();
          },
        },
      },
      text: {
        group: "inline",
        toMarkdown: {
          match: (node: { type: { name: string } }) => node.type.name === "text",
          runner: (state: SerializerState, node: { text?: string | null }) => {
            state.addNode("text", undefined, node.text ?? "");
          },
        },
      },
    },
  } as never);

  it("空 paragraph 的真实序列化输出是 truthy 的 '<br />'", () => {
    const emptyPara = brSchema.node("paragraph", null, []);
    const nonEmptyPara = brSchema.node("paragraph", null, [brSchema.text("有内容")]);
    const doc = brSchema.node("doc", null, [emptyPara, nonEmptyPara]);

    const state = new SerializerState(brSchema);
    state.run(doc);
    const tree = state.build();
    const firstParaText = collectText((tree.children as MarkdownNode[])[0]!);

    expect(emptyPara.textContent).toBe("");
    expect(firstParaText).toBe("<br />"); // truthy,若直接拿它判空就会误判成"非空"
  });

  it("即便 serializer 会输出 truthy 的 '<br />',isBlockTextEmpty 仍按 textContent 正确判空", () => {
    const emptyPara = brSchema.node("paragraph", null, []);
    expect(emptyPara.textContent).toBe("");
    expect(isBlockTextEmpty(emptyPara.textContent)).toBe(true);
  });
});
