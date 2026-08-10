import { describe, expect, it } from "vitest";
import { refineReady } from "./refineReady";

describe("refineReady(会后 AI 就绪判定,口径对齐 lib.rs refine_llm_ready/refine_agent_ready)", () => {
  it("openai 档:三项均非空才就绪", () => {
    expect(
      refineReady({
        refine_provider: "openai",
        refine_base_url: "https://api.example.com",
        refine_model: "gpt-4o-mini",
        refine_api_key: "sk-xxx",
      }),
    ).toBe(true);
  });
  it("openai 档:任一字段为空 → 未就绪", () => {
    const base = {
      refine_provider: "openai",
      refine_base_url: "https://api.example.com",
      refine_model: "gpt-4o-mini",
      refine_api_key: "sk-xxx",
    };
    expect(refineReady({ ...base, refine_base_url: "" })).toBe(false);
    expect(refineReady({ ...base, refine_model: "" })).toBe(false);
    expect(refineReady({ ...base, refine_api_key: "" })).toBe(false);
  });
  it("agent 档:恒就绪,不看 base_url/model/api_key(后端同口径不查 bin/model)", () => {
    expect(
      refineReady({
        refine_provider: "agent",
        refine_base_url: "",
        refine_model: "",
        refine_api_key: "",
      }),
    ).toBe(true);
  });
  it("provider 坏值(手改 settings.json)按 openai 对待,同后端默认执行体口径", () => {
    expect(
      refineReady({
        refine_provider: "banana",
        refine_base_url: "",
        refine_model: "",
        refine_api_key: "",
      }),
    ).toBe(false);
    expect(
      refineReady({
        refine_provider: "banana",
        refine_base_url: "u",
        refine_model: "m",
        refine_api_key: "k",
      }),
    ).toBe(true);
  });
});
