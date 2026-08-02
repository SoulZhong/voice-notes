import { describe, expect, it } from "vitest";
import { i18n, localeVariants, resolveLocale, shards, t } from "./index.svelte";

describe("i18n 核心", () => {
  it("resolveLocale:显式 zh/en 原样,system 按系统语言,node 无 navigator 回落 en", () => {
    expect(resolveLocale("zh")).toBe("zh");
    expect(resolveLocale("en")).toBe("en");
    // node 环境无 navigator → 非中文系统语义,回落 en
    expect(resolveLocale("system")).toBe("en");
    expect(resolveLocale("")).toBe("en");
  });

  it("t:静态文案按 locale 取值,切换即生效", () => {
    i18n.setChoice("zh");
    expect(t("common.language.system")).toBe("跟随系统");
    i18n.setChoice("en");
    expect(t("common.language.system")).toBe("Follow system");
    i18n.setChoice("zh");
  });

  it("t:{name} 占位插值;参数缺失时占位原样保留", () => {
    i18n.setChoice("zh");
    expect(t("common.saveFailed", { e: "disk full" })).toBe("保存失败: disk full");
    expect(t("common.saveFailed")).toBe("保存失败: {e}");
  });

  it("t:缺键返回 key 本身(界面永远有字可显)", () => {
    expect(t("no.such.key")).toBe("no.such.key");
  });

  it("localeVariants:给出该键的全部语言写法(写入用户数据的文案要跨语言判等)", () => {
    const me = localeVariants("notes.speaker.me");
    expect(me).toContain("我");
    expect(me).toContain("Me");
    // 两语言同值时去重;缺键给空数组(调用方 includes 判定自然为 false)。
    expect(localeVariants("common.none")).toEqual(["—"]);
    expect(localeVariants("no.such.key")).toEqual([]);
  });

  it("字典分片:zh/en 键集一致(漏翻在 CI 现形,不等运行时)", () => {
    for (const [name, shard] of Object.entries(shards)) {
      const zhKeys = Object.keys(shard.zh).sort();
      const enKeys = Object.keys(shard.en).sort();
      expect(enKeys, `分片 ${name} 的 en 键集必须与 zh 一致`).toEqual(zhKeys);
    }
  });

  it("字典分片:分片之间不得重键(合并会静默互吞)", () => {
    const seen = new Map<string, string>();
    for (const [name, shard] of Object.entries(shards)) {
      for (const key of Object.keys(shard.zh)) {
        const prev = seen.get(key);
        expect(prev, `键 ${key} 同时出现在分片 ${prev} 与 ${name}`).toBeUndefined();
        seen.set(key, name);
      }
    }
  });

  it("字典分片:键必须以所在分片名为前缀(防未来撞键)", () => {
    for (const [name, shard] of Object.entries(shards)) {
      for (const key of Object.keys(shard.zh)) {
        expect(key.startsWith(`${name}.`), `分片 ${name} 中键 ${key} 未带 "${name}." 前缀`).toBe(true);
      }
    }
  });
});
