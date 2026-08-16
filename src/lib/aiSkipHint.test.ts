import { describe, expect, it } from "vitest";
import { aiSkipHint } from "./aiSkipHint";

const base = { llmStage: "off", noteComplete: true, refineEnabled: true, ready: false };

describe("AI 未整理提示", () => {
  it("开关开着但执行体没配全:提示去配置", () => {
    expect(aiSkipHint(base)).toBe("unconfigured");
  });

  it("配置已补齐但这场当时没跑:提示可补跑", () => {
    expect(aiSkipHint({ ...base, ready: true })).toBe("rerunnable");
  });

  it("用户主动关了会后 AI:不提示", () => {
    // 明确选择,不是故障;每篇笔记唠叨一句只会训练用户忽略横幅。
    expect(aiSkipHint({ ...base, refineEnabled: false })).toBe(null);
    expect(aiSkipHint({ ...base, refineEnabled: false, ready: true })).toBe(null);
  });

  it("跑过的场次一律不提示,partial/failed 各有自己的横幅", () => {
    for (const s of ["done", "partial", "failed"]) {
      expect(aiSkipHint({ ...base, llmStage: s })).toBe(null);
    }
  });

  it("没有修订稿(拿不到 stages)时不提示", () => {
    expect(aiSkipHint({ ...base, llmStage: undefined })).toBe(null);
  });

  it("笔记还没完成:还没到该跑 AI 的时候,不提示", () => {
    expect(aiSkipHint({ ...base, noteComplete: false })).toBe(null);
  });
});
