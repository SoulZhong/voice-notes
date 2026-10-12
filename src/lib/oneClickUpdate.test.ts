import { describe, it, expect, vi, beforeEach } from "vitest";
import { updateProgressLabel, applyUpdate, toUpdateProgress } from "./update";
import { zh } from "./i18n/dict/settings";
import conf from "../../src-tauri/tauri.conf.json";

// 进度文案已 i18n 化(settings 分片);测试默认 locale=zh,断言从 zh 字典取值,
// 文案改动只需改字典,测试不再钉死字面量。
const updating = zh["settings.update.updating"];
const installing = zh["settings.update.installing"];
const updatingPct = (pct: number) => zh["settings.update.updatingPct"].replace("{pct}", String(pct));
const mirrorSwitched = zh["settings.update.mirrorSwitched"];

// applyUpdate 调后端 apply_update、听 update_progress、装好后 relaunch;vi.mock 拦截模块解析,
// 测试无需真实 Tauri 环境。
vi.mock("@tauri-apps/api/core", () => ({ invoke: vi.fn() }));
vi.mock("@tauri-apps/api/event", () => ({ listen: vi.fn() }));
vi.mock("@tauri-apps/plugin-process", () => ({ relaunch: vi.fn() }));
import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
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
  it("updater endpoints 钉本仓库 Release 的 latest.json,直连在前、加速镜像兜底(错仓库/漂移即失联)", () => {
    const eps: string[] = conf.plugins?.updater?.endpoints ?? [];
    expect(eps).toEqual([
      "https://github.com/SoulZhong/voice-notes/releases/latest/download/latest.json",
      "https://ghfast.top/https://github.com/SoulZhong/voice-notes/releases/latest/download/latest.json",
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

describe("toUpdateProgress", () => {
  it("按收到/总长出百分比;换到加速线路时文案带一句提示", () => {
    expect(toUpdateProgress({ received: 50, total: 100, mirror: false, installing: false })).toEqual({
      pct: 50,
      installing: false,
      label: updatingPct(50),
    });
    expect(toUpdateProgress({ received: 50, total: 100, mirror: true, installing: false }).label).toBe(
      `${updatingPct(50)} · ${mirrorSwitched}`,
    );
  });
  it("总长未知 pct 为 null(横幅退化为扫描态而非假百分比)", () => {
    expect(toUpdateProgress({ received: 50, total: 0, mirror: false, installing: false })).toEqual({
      pct: null,
      installing: false,
      label: updating,
    });
  });
  it("安装阶段切安装态(否则安装期像卡死在 100%)", () => {
    expect(toUpdateProgress({ received: 0, total: 0, mirror: false, installing: true })).toEqual({
      pct: null,
      installing: true,
      label: installing,
    });
  });
});

describe("applyUpdate", () => {
  const unlisten = vi.fn();
  let emit: (p: unknown) => void = () => {};
  beforeEach(() => {
    vi.mocked(invoke).mockReset();
    unlisten.mockReset();
    vi.mocked(listen).mockReset().mockImplementation(async (_ev, cb) => {
      emit = (p) => (cb as (e: { payload: unknown }) => void)({ payload: p });
      return unlisten;
    });
    vi.mocked(relaunch).mockReset().mockResolvedValue(undefined);
  });

  it("后端查无更新返回 none,不触发 relaunch,并解除事件监听", async () => {
    vi.mocked(invoke).mockResolvedValue(false);
    await expect(applyUpdate(() => {})).resolves.toBe("none");
    expect(invoke).toHaveBeenCalledWith("apply_update");
    expect(relaunch).not.toHaveBeenCalled();
    expect(unlisten).toHaveBeenCalledOnce();
  });

  it("后端进度事件转给回调,装好后重启", async () => {
    const events: unknown[] = [];
    vi.mocked(invoke).mockImplementation(async () => {
      emit({ received: 50, total: 100, mirror: false, installing: false });
      emit({ received: 0, total: 0, mirror: false, installing: true });
      return true;
    });
    await applyUpdate((p) => events.push(p));
    expect(events).toEqual([
      { pct: 50, installing: false, label: updatingPct(50) },
      { pct: null, installing: true, label: installing },
    ]);
    expect(relaunch).toHaveBeenCalledOnce();
  });

  it("单飞:进行中再次调用复用同一 promise,不并发第二趟下载安装", async () => {
    let release!: () => void;
    const gate = new Promise<void>((r) => (release = r));
    vi.mocked(invoke).mockImplementation(async () => {
      await gate;
      return false;
    });
    const first = applyUpdate(() => {});
    const second = applyUpdate(() => {});
    expect(second).toBe(first);
    release();
    await first;
    expect(invoke).toHaveBeenCalledOnce();
    // 结束后锁释放,可再次发起。
    vi.mocked(invoke).mockResolvedValue(false);
    await applyUpdate(() => {});
    expect(invoke).toHaveBeenCalledTimes(2);
  });

  it("失败向上抛、解除监听且释放单飞锁(调用方兜底「打开发布页」,下次仍可重试)", async () => {
    vi.mocked(invoke).mockRejectedValue(new Error("网络断了"));
    await expect(applyUpdate(() => {})).rejects.toThrow("网络断了");
    expect(unlisten).toHaveBeenCalledOnce();
    vi.mocked(invoke).mockResolvedValue(false);
    await expect(applyUpdate(() => {})).resolves.toBe("none");
  });
});
