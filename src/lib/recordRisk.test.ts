import { describe, expect, it, vi } from "vitest";
import { createRecordRiskGate, unresolvedRisks, type RecordRisk } from "./recordRisk.svelte";
// 文案契约:钉住 t(键) + 字典里这个键仍是那句中文(与 relationBackfill.test.ts 同法)。
import { zh as recordZh } from "./i18n/dict/record";

const VOICE: RecordRisk = { kind: "voice_isolation", detail: "" };
const BT: RecordRisk = { kind: "bluetooth_mic", detail: "" };

describe("unresolvedRisks", () => {
  it("无风险时返回空", () => {
    expect(unresolvedRisks([], new Set())).toEqual([]);
  });

  it("已在本次会话放行过的 kind 不再拦", () => {
    expect(unresolvedRisks([VOICE, BT], new Set(["voice_isolation"]))).toEqual([BT]);
  });

  it("全部放行过就不拦", () => {
    expect(unresolvedRisks([VOICE, BT], new Set(["voice_isolation", "bluetooth_mic"]))).toEqual([]);
  });
});

describe("开录守卫", () => {
  it("没有风险时直接放行,不弹窗", async () => {
    const gate = createRecordRiskGate(async () => []);
    const go = await gate.guard();
    expect(go).toBe(true);
    expect(gate.risks).toEqual([]);
  });

  it("有风险时先挂起等用户决定,期间不放行", async () => {
    const gate = createRecordRiskGate(async () => [VOICE]);
    const pending = gate.guard();
    // 等 probe 的微任务跑完
    await Promise.resolve();
    await Promise.resolve();
    expect(gate.risks).toEqual([VOICE]);
    gate.cancel();
    expect(await pending).toBe(false);
    expect(gate.risks).toEqual([]);
  });

  it("选「仍然录制」放行,且同一 kind 本次会话内不再拦", async () => {
    const probe = vi.fn(async () => [VOICE]);
    const gate = createRecordRiskGate(probe);
    const first = gate.guard();
    await Promise.resolve();
    await Promise.resolve();
    gate.proceed();
    expect(await first).toBe(true);

    // 第二次开录:同样探到风险,但已放行过 → 不再弹
    const second = await gate.guard();
    expect(second).toBe(true);
    expect(gate.risks).toEqual([]);
    expect(probe).toHaveBeenCalledTimes(2);
  });

  it("放行只对已放行的那条生效,新出现的风险照拦", async () => {
    let found: RecordRisk[] = [VOICE];
    const gate = createRecordRiskGate(async () => found);
    const first = gate.guard();
    await Promise.resolve();
    await Promise.resolve();
    gate.proceed();
    await first;

    found = [VOICE, BT]; // 用户中途插了蓝牙耳机
    const second = gate.guard();
    await Promise.resolve();
    await Promise.resolve();
    expect(gate.risks).toEqual([BT]);
    gate.cancel();
    expect(await second).toBe(false);
  });

  /// Codex review P1:原实现每次 guard() 都直接覆盖 #resolve,并发调用(双击按钮、
  /// 或侧栏与录制页先后触发)会把前一个 Promise 永久挂起——按钮在 guard() 期间
  /// 没有 pending 态,这条竞态实际可达。
  it("并发调用共用同一次确认,两个调用都要 settle", async () => {
    const probe = vi.fn(async () => [VOICE]);
    const gate = createRecordRiskGate(probe);
    const a = gate.guard();
    const b = gate.guard();
    await Promise.resolve();
    await Promise.resolve();
    expect(gate.risks).toEqual([VOICE]);
    expect(probe).toHaveBeenCalledTimes(1);
    gate.proceed();
    expect(await a).toBe(true);
    expect(await b).toBe(true);
  });

  it("并发调用被取消时两个调用都收到 false", async () => {
    const gate = createRecordRiskGate(async () => [VOICE]);
    const a = gate.guard();
    const b = gate.guard();
    await Promise.resolve();
    await Promise.resolve();
    gate.cancel();
    expect(await a).toBe(false);
    expect(await b).toBe(false);
  });

  it("一次确认结束后,下一次 guard 重新探测", async () => {
    const probe = vi.fn(async () => [VOICE]);
    const gate = createRecordRiskGate(probe);
    const first = gate.guard();
    await Promise.resolve();
    await Promise.resolve();
    gate.cancel();
    await first;
    // 「去改设置」不进免打扰名单,所以第二次仍会探到风险并再次挂起等确认。
    // 不能 await 它——挂起是预期行为,await 会让用例自己卡死。
    const second = gate.guard();
    await Promise.resolve();
    await Promise.resolve();
    expect(probe).toHaveBeenCalledTimes(2);
    expect(gate.risks).toEqual([VOICE]);
    gate.cancel();
    expect(await second).toBe(false);
  });

  it("探测失败不挡开录:宁可漏提示,不能因为查不到系统状态就让人录不了", async () => {
    const gate = createRecordRiskGate(async () => {
      throw new Error("命令不可用");
    });
    expect(await gate.guard()).toBe(true);
    expect(gate.risks).toEqual([]);
  });
});

describe("文案契约", () => {
  it("每条风险都有标题、后果、改法三段文案", () => {
    // 对话框按 `record.risk.<kind>.<段>` 拼键取文案,少一段就是空白行。
    // 这里按字面键断言而不是模板拼接:字典是 const 字面量类型,拼出来的键索引不进去,
    // 而"能不能索引到"恰恰就是这个用例要保的东西。
    expect(recordZh["record.risk.voice_isolation.title"]).toBeTruthy();
    expect(recordZh["record.risk.voice_isolation.impact"]).toBeTruthy();
    expect(recordZh["record.risk.voice_isolation.how"]).toBeTruthy();
    expect(recordZh["record.risk.bluetooth_mic.title"]).toBeTruthy();
    expect(recordZh["record.risk.bluetooth_mic.impact"]).toBeTruthy();
    expect(recordZh["record.risk.bluetooth_mic.how"]).toBeTruthy();
  });

  it("蓝牙麦克风那条要说清丢的是内容,不是音质", () => {
    expect(recordZh["record.risk.bluetooth_mic.impact"]).toContain("丢");
  });
});
