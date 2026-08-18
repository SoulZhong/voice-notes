import { describe, expect, it } from "vitest";
import { redact, redactEvent } from "./redact";

/** 与 Rust 侧 src-tauri/src/redact.rs 共用同一组测试向量。
 *  任一侧规则漂移,两边测试会一起红。 */
describe("redact(与 Rust 侧同规则)", () => {
  it("spike 实测泄漏样本被脱干净", () => {
    const leaked =
      "refine(note-140751): 写入 /Users/张伟/Library/Application Support/voice-notes/notes/季度复盘会.json 失败";
    const out = redact(leaked);
    expect(out).not.toContain("张伟");
    expect(out).not.toContain("季度复盘会");
    expect(out).toContain("note-140751"); // note-id 不是内容,保留以便定位
  });

  it("Windows 家目录路径同样收敛", () => {
    const out = redact("write C:\\Users\\Alice\\AppData\\voice-notes\\notes\\周会.json failed");
    expect(out).not.toContain("Alice");
    expect(out).not.toContain("周会");
  });

  it("长中文串整段丢弃而非截断", () => {
    const out = redact("parse failed: 这段话是会议逐字稿的一部分不应该被上报出去");
    expect(out).not.toContain("会议逐字稿");
    expect(out).toContain("<TEXT>");
    expect(out).toContain("parse failed");
  });

  it("短中文措辞保留便于排查", () => {
    expect(redact("写入失败")).toBe("写入失败");
  });

  it("密钥形态被抹掉(含带前缀的形态)", () => {
    expect(redact("key=sk-abcdefghijklmnop failed")).not.toContain("sk-abcdefghijklmnop");
  });

  it("无敏感内容时原样通过", () => {
    const clean = "asr engine returned empty result (code 3)";
    expect(redact(clean)).toBe(clean);
  });
});

describe("redactEvent(before_send 钩子)", () => {
  it("脱 $exception_list 里的 value——现代 PostHog 错误追踪用的就是它", () => {
    const ev = {
      event: "$exception",
      properties: {
        $exception_list: [{ type: "Error", value: "写入 /Users/张伟/notes/季度复盘会.json 失败" }],
      },
    };
    const out = redactEvent(ev);
    const v = (out.properties.$exception_list as Array<{ value: string }>)[0].value;
    expect(v).not.toContain("张伟");
    expect(v).not.toContain("季度复盘会");
  });

  it("旧的标量字段一并覆盖", () => {
    const ev = { event: "$exception", properties: { $exception_message: "/Users/李四/x.json" } };
    expect((redactEvent(ev).properties.$exception_message as string)).not.toContain("李四");
  });

  it("事件本身绝不丢弃——丢了就看不见异常", () => {
    const ev = { event: "$exception", properties: { $exception_message: "boom" } };
    expect(redactEvent(ev)).not.toBeNull();
    expect(redactEvent(null)).toBeNull();
  });

  it("非异常事件原样通过", () => {
    const ev = { event: "vn_page_view", properties: { path: "/notes" } };
    expect(redactEvent(ev).properties.path).toBe("/notes");
  });
});
