import { describe, expect, it } from "vitest";
import { describeActionError } from "./speakerAction";

/** 调用方实际传 t("speakers.actionFailed");测试里用哨兵串,避免依赖 i18n。 */
const FB = "<fallback>";

/** 背景:2026-08-17 「确认删除」点了无效——后端返回 Err("该笔记正在 Aing 中,稍后再删"),
 *  但 SpeakerChips 的 commitDelete 直接 `await onDelete(id)` 无 try/catch,错误被吞,
 *  用户只看到面板关闭、什么都没发生。这里钉死「任何失败都必须产出非空可读文案」。 */
describe("describeActionError(说话人条操作失败文案)", () => {
  it("后端 invoke reject 的字符串原样透出(tr! 已按界面语言本地化,不该再包装)", () => {
    expect(describeActionError("该笔记正在 Aing 中，稍后再删", FB)).toBe("该笔记正在 Aing 中，稍后再删");
    expect(describeActionError("录制中的笔记不能编辑", FB)).toBe("录制中的笔记不能编辑");
  });

  it("Error 取 message", () => {
    expect(describeActionError(new Error("未知说话人: S9"), FB)).toBe("未知说话人: S9");
  });

  it("空/无信息的失败也必须有非空文案(否则又变成静默失败)", () => {
    for (const v of [null, undefined, "", "   ", new Error("")]) {
      const s = describeActionError(v, FB);
      expect(s.trim()).not.toBe("");
    }
  });

  it("非字符串对象兜底成可读文本,不出现 [object Object]", () => {
    expect(describeActionError({ code: 500 }, FB)).not.toContain("[object Object]");
    expect(describeActionError({ code: 500 }, FB).trim()).not.toBe("");
  });

  it("前后空白裁掉(后端文案偶带换行,直接塞进单行元素会撑版)", () => {
    expect(describeActionError("  该笔记正在 Aing 中  \n", FB)).toBe("该笔记正在 Aing 中");
  });
});
