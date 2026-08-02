import { describe, it, expect, vi, beforeEach } from "vitest";
import { updateProgressLabel, applyUpdate } from "./update";
import { zh } from "./i18n/dict/settings";
import conf from "../../src-tauri/tauri.conf.json";

// 进度文案已 i18n 化(settings 分片);测试默认 locale=zh,断言从 zh 字典取值,
// 文案改动只需改字典,测试不再钉死字面量。
const updating = zh["settings.update.updating"];
const installing = zh["settings.update.installing"];
const updatingPct = (pct: number) => zh["settings.update.updatingPct"].replace("{pct}", String(pct));

// applyUpdate 内部动态 import 这两个插件;vi.mock 拦截模块解析,测试无需真实 Tauri 环境。
vi.mock("@tauri-apps/plugin-updater", () => ({ check: vi.fn() }));
vi.mock("@tauri-apps/plugin-process", () => ({ relaunch: vi.fn() }));
import { check } from "@tauri-apps/plugin-updater";
import { relaunch } from "@tauri-apps/plugin-process";

// 一键更新的配置契约:updater 插件配置漂移(丢 endpoints/关 createUpdaterArtifacts)
// 会让"一键更新"静默退化成没更新可查,这里用测试钉死。
describe("tauri.conf.json updater 配置", () => {
  it("bundle 开启 createUpdaterArtifacts(否则发布 CI 不产 .sig/.tar.gz)", () => {
    expect(conf.bundle.createUpdaterArtifacts).toBe(true);
  });
  it("bundle targets 钉 Windows 单安装格式(nsis):latest.json 每平台只有一个槽位,\
双格式会让另一格式用户跨格式更新、留下孤儿安装", () => {
    expect(conf.bundle.targets).toEqual(["app", "dmg", "nsis"]);
  });
  it("updater endpoints 钉本仓库 Release 的 latest.json(错仓库/漂移即失联)", () => {
    const eps: string[] = conf.plugins?.updater?.endpoints ?? [];
    expect(eps).toEqual([
      "https://github.com/SoulZhong/voice-notes/releases/latest/download/latest.json",
    ]);
  });
  it("pubkey 是结构合法的 minisign 公钥(截断/错编码的坏 pasting 在这里就地暴露,\
而不是发版后全员签名校验失败)", () => {
    const raw = atob(conf.plugins.updater.pubkey);
    const lines = raw.trim().split("\n");
    expect(lines[0]).toMatch(/^untrusted comment: minisign public key/);
    const key = atob(lines[1]);
    expect(key.length).toBe(42); // 2 字节算法标识 "Ed" + 8 字节 key id + 32 字节 Ed25519 公钥
    expect(key.slice(0, 2)).toBe("Ed");
  });
});

describe("updateProgressLabel", () => {
  it("有总长时显示百分比", () => {
    expect(updateProgressLabel(4200, 10000)).toBe(updatingPct(42));
    expect(updateProgressLabel(10000, 10000)).toBe(updatingPct(100));
  });
  it("无总长/零总长显示省略号", () => {
    expect(updateProgressLabel(4200, undefined)).toBe(updating);
    expect(updateProgressLabel(0, 0)).toBe(updating);
  });
});

describe("applyUpdate", () => {
  beforeEach(() => {
    vi.mocked(check).mockReset();
    vi.mocked(relaunch).mockReset().mockResolvedValue(undefined);
  });

  it("插件查无更新返回 none,不触发 relaunch", async () => {
    vi.mocked(check).mockResolvedValue(null);
    await expect(applyUpdate(() => {})).resolves.toBe("none");
    expect(relaunch).not.toHaveBeenCalled();
  });

  it("进度回调:Progress 报百分比,Finished 切「安装中…」(否则安装期像卡死在 100%)", async () => {
    const labels: string[] = [];
    vi.mocked(check).mockResolvedValue({
      downloadAndInstall: async (cb: (e: unknown) => void) => {
        cb({ event: "Started", data: { contentLength: 100 } });
        cb({ event: "Progress", data: { chunkLength: 50 } });
        cb({ event: "Progress", data: { chunkLength: 50 } });
        cb({ event: "Finished" });
      },
    } as never);
    await applyUpdate((label) => labels.push(label));
    expect(labels).toEqual([updatingPct(50), updatingPct(100), installing]);
    expect(relaunch).toHaveBeenCalledOnce();
  });

  it("单飞:进行中再次调用复用同一 promise,不并发第二趟下载安装", async () => {
    let release!: () => void;
    const gate = new Promise<void>((r) => (release = r));
    vi.mocked(check).mockImplementation(async () => {
      await gate;
      return null;
    });
    const first = applyUpdate(() => {});
    const second = applyUpdate(() => {});
    expect(second).toBe(first);
    release();
    await first;
    expect(check).toHaveBeenCalledOnce();
    // 结束后锁释放,可再次发起。
    vi.mocked(check).mockResolvedValue(null);
    await applyUpdate(() => {});
    expect(check).toHaveBeenCalledTimes(2);
  });

  it("失败向上抛且释放单飞锁(调用方兜底「打开发布页」,下次仍可重试)", async () => {
    vi.mocked(check).mockRejectedValue(new Error("网络断了"));
    await expect(applyUpdate(() => {})).rejects.toThrow("网络断了");
    vi.mocked(check).mockResolvedValue(null);
    await expect(applyUpdate(() => {})).resolves.toBe("none");
  });
});
