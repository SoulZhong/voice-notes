import { afterEach, describe, expect, it, vi } from "vitest";
import { i18n, localeVariants, resolveLocale, shards, t } from "./index.svelte";

describe("i18n 核心", () => {
  afterEach(() => {
    vi.unstubAllGlobals();
  });

  // 系统语言必须由用例自己钉死,不能依赖运行环境:Node 21 起提供了全局
  // navigator(本机 navigator.language = "zh-CN"),旧用例"node 环境无
  // navigator"的假设随 Node 升级失效,于是在开发机上必挂。
  it("resolveLocale:显式 zh/en 原样,system 按系统语言解析", () => {
    expect(resolveLocale("zh")).toBe("zh");
    expect(resolveLocale("en")).toBe("en");

    vi.stubGlobal("navigator", { language: "zh-CN" });
    expect(resolveLocale("system")).toBe("zh");
    expect(resolveLocale("")).toBe("zh");

    vi.stubGlobal("navigator", { language: "en-US" });
    expect(resolveLocale("system")).toBe("en");

    // 语言缺失(navigator 存在但 language 为空)→ 回落 en
    vi.stubGlobal("navigator", { language: "" });
    expect(resolveLocale("system")).toBe("en");
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

  it("字典分片:en 函数值读取的参数名必须与 zh 占位符一致", () => {
    // 英文用函数值处理复数,参数靠名字取(p.hops)。若与 zh 的 {hops} 对不上,
    // 中文只是占位符原样留着(看得出),英文却会渲染成 "undefined"(看不出是 bug)。
    for (const [name, shard] of Object.entries(shards)) {
      const zh = shard.zh as Record<string, unknown>;
      const en = shard.en as Record<string, unknown>;
      for (const [key, enValue] of Object.entries(en)) {
        if (typeof enValue !== "function") continue;
        const zhValue = zh[key];
        if (typeof zhValue !== "string") continue;
        const zhNames = new Set([...zhValue.matchAll(/\{(\w+)\}/g)].map((m) => m[1]));
        const enNames = new Set(
          [...enValue.toString().matchAll(/\bp\.(\w+)/g)].map((m) => m[1]),
        );
        for (const n of enNames) {
          expect(zhNames.has(n), `分片 ${name} 的 ${key}:en 读了 p.${n},zh 没有对应的 {${n}}`).toBe(true);
        }
        for (const n of zhNames) {
          expect(enNames.has(n), `分片 ${name} 的 ${key}:zh 有 {${n}},en 函数没用到`).toBe(true);
        }
      }
    }
  });

  it("字典分片:en 值不得整段照抄 zh(漏翻的兜底体检)", () => {
    // 语言名按惯例两语言同形(在英文界面也要让中文用户认得出「中文」),白名单放行。
    const sameByDesign = new Set(["common.language.zh", "common.language.en"]);
    for (const [name, shard] of Object.entries(shards)) {
      const zh = shard.zh as Record<string, unknown>;
      const en = shard.en as Record<string, unknown>;
      for (const [key, zhValue] of Object.entries(zh)) {
        if (typeof zhValue !== "string" || !/[一-鿿]/.test(zhValue)) continue;
        if (sameByDesign.has(key)) continue;
        expect(en[key], `分片 ${name} 的 ${key} 未翻译(en 与 zh 同值)`).not.toBe(zhValue);
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
