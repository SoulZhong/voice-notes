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

  it("含空格的笔记文件名整条脱掉,尾部措辞保留", () => {
    // 这条向量此前只在 Rust 侧有。两边行为其实不同:TS 那条正则会一路吃到行尾,
    // 把 "failed" 也吞掉——号称共用的一组向量,漏掉的恰恰是能暴露漂移的那条。
    const out = redact("write /Users/Alice/Library/voice-notes/notes/Q3 roadmap.json failed");
    expect(out).not.toContain("roadmap");
    expect(out).not.toContain("Alice");
    expect(out).toContain("failed");
  });

  it("家目录路径两种形态都收敛", () => {
    expect(redact("open /Users/zhangwei/notes/x.json failed")).not.toContain("zhangwei");
    expect(redact("open /home/lisi/notes/y.json failed")).not.toContain("lisi");
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

  it("栈帧里的路径一起脱掉——Rust 侧修过,TS 侧此前没跟上", () => {
    const ev = {
      event: "$exception",
      properties: {
        $exception_list: [
          {
            type: "Error",
            value: "boom",
            stacktrace: { frames: [{ filename: "/Users/张伟/voice-notes/src/x.ts" }] },
          },
        ],
        $exception_panic_file: "/Users/张伟/voice-notes/src/y.rs",
      },
    };
    const dumped = JSON.stringify(redactEvent(ev).properties);
    expect(dumped).not.toContain("张伟");
  });

  it("认不出的 stacktrace 结构原样放过而不是丢事件", () => {
    const ev = {
      event: "$exception",
      properties: { $exception_list: [{ type: "Error", value: "boom", stacktrace: "字符串形态" }] },
    };
    expect(redactEvent(ev)).not.toBeNull();
  });

  it("非异常事件原样通过", () => {
    const ev = { event: "vn_page_view", properties: { path: "/notes" } };
    expect(redactEvent(ev).properties.path).toBe("/notes");
  });
});
