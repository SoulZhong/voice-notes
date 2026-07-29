import { describe, it, expect } from "vitest";
import { updateProgressLabel } from "./update";
import conf from "../../src-tauri/tauri.conf.json";

// 一键更新的配置契约:updater 插件配置漂移(丢 endpoints/关 createUpdaterArtifacts)
// 会让"一键更新"静默退化成没更新可查,这里用测试钉死。
describe("tauri.conf.json updater 配置", () => {
  it("bundle 开启 createUpdaterArtifacts(否则发布 CI 不产 .sig/.tar.gz)", () => {
    expect(conf.bundle.createUpdaterArtifacts).toBe(true);
  });
  it("updater endpoints 指向 GitHub Release 的 latest.json", () => {
    const eps: string[] = conf.plugins?.updater?.endpoints ?? [];
    expect(eps.length).toBeGreaterThan(0);
    for (const e of eps) expect(e).toMatch(/^https:\/\/.+latest\.json$/);
  });
  it("pubkey 字段存在(占位或真实公钥;签名校验是更新通道的硬要求)", () => {
    expect(typeof conf.plugins?.updater?.pubkey).toBe("string");
    expect(conf.plugins.updater.pubkey.length).toBeGreaterThan(0);
  });
});

describe("updateProgressLabel", () => {
  it("有总长时显示百分比", () => {
    expect(updateProgressLabel(4200, 10000)).toBe("更新中 42%");
    expect(updateProgressLabel(10000, 10000)).toBe("更新中 100%");
  });
  it("无总长/零总长显示省略号", () => {
    expect(updateProgressLabel(4200, undefined)).toBe("更新中…");
    expect(updateProgressLabel(0, 0)).toBe("更新中…");
  });
});
