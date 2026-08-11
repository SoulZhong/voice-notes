import { describe, expect, it } from "vitest";
import { executorReady, refineReady } from "./refineReady";

const profile = (over: Partial<{ id: string; base_url: string; model: string; api_key: string }> = {}) => ({
  id: "p1",
  label: "T",
  base_url: "https://api.example.com",
  model: "gpt-4o-mini",
  api_key: "sk-xxx",
  ...over,
});

describe("executorReady(执行体就绪判定,口径对齐 settings::executor_ready)", () => {
  it("llm 引用:档案存在且三项均非空才就绪", () => {
    expect(executorReady({ llm_profiles: [profile()] }, "llm:p1")).toBe(true);
    expect(executorReady({ llm_profiles: [profile({ base_url: "" })] }, "llm:p1")).toBe(false);
    expect(executorReady({ llm_profiles: [profile({ model: "" })] }, "llm:p1")).toBe(false);
    expect(executorReady({ llm_profiles: [profile({ api_key: "" })] }, "llm:p1")).toBe(false);
  });
  it("悬空引用(档案已删)→ 未就绪", () => {
    expect(executorReady({ llm_profiles: [] }, "llm:ghost")).toBe(false);
  });
  it("agent 引用:恒就绪(bin 探测留运行时,后端同口径)", () => {
    expect(executorReady({ llm_profiles: [] }, "agent:claude")).toBe(true);
  });
  it("空引用/坏值 → 未就绪", () => {
    expect(executorReady({ llm_profiles: [profile()] }, "")).toBe(false);
    expect(executorReady({ llm_profiles: [profile()] }, "banana")).toBe(false);
  });
});

describe("refineReady(会后 AI 就绪 = refine_executor 的执行体就绪)", () => {
  it("引用就绪的档案 → 就绪;未配置 → 未就绪", () => {
    expect(refineReady({ llm_profiles: [profile()], refine_executor: "llm:p1" })).toBe(true);
    expect(refineReady({ llm_profiles: [profile()], refine_executor: "" })).toBe(false);
  });
});
