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
