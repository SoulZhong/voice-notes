import { describe, expect, it, vi } from "vitest";
import { createAudition, describePlayError, type PlayerLike } from "./tidyAudio";

function fakePlayer() {
  const p: PlayerLike & { paused: boolean } = {
    paused: false,
    play: vi.fn(),
    pause: vi.fn(() => {
      p.paused = true;
    }),
    onended: null,
  };
  return p;
}

describe("createAudition", () => {
  it("单实例互斥:开新的先停旧的", () => {
    const players: ReturnType<typeof fakePlayer>[] = [];
    const changes: (string | null)[] = [];
    const a = createAudition(
      () => {
        const p = fakePlayer();
        players.push(p);
        return p;
      },
      (k) => changes.push(k),
    );
    a.toggle("k1", "/a.wav");
    a.toggle("k2", "/b.wav");
    expect(players[0].pause).toHaveBeenCalled();
    expect(a.key).toBe("k2");
    expect(changes).toEqual(["k1", null, "k2"]);
  });

  it("再点同 key 停止", () => {
    const a = createAudition(fakePlayer, () => {});
    a.toggle("k1", "/a.wav");
    a.toggle("k1", "/a.wav");
    expect(a.key).toBeNull();
  });

  it("自然播完清态(onended)", () => {
    let last: ReturnType<typeof fakePlayer> | null = null;
    const a = createAudition(
      () => (last = fakePlayer()),
      () => {},
    );
    a.toggle("k1", "/a.wav");
    last!.onended?.();
    expect(a.key).toBeNull();
  });

  it("play() 拒绝时触发 onError,不静默吞掉", async () => {
    const errors: string[] = [];
    const p: PlayerLike = {
      play: () => Promise.reject(new Error("boom")),
      pause: vi.fn(),
      onended: null,
    };
    const a = createAudition(
      () => p,
      () => {},
      (msg) => errors.push(msg),
    );
    a.toggle("k1", "/a.wav");
    await Promise.resolve();
    await Promise.resolve();
    expect(errors).toEqual(["Error: boom"]);
    expect(a.key).toBeNull();
  });

  it("play() 拒绝已知媒体错误时,onError 收到可读中文而非英文原文", async () => {
    const errors: string[] = [];
    const notSupported = Object.assign(new Error("Failed to load because no supported source was found."), {
      name: "NotSupportedError",
    });
    const p: PlayerLike = {
      play: () => Promise.reject(notSupported),
      pause: vi.fn(),
      onended: null,
    };
    const a = createAudition(
      () => p,
      () => {},
      (msg) => errors.push(msg),
    );
    a.toggle("k1", "/a.wav");
    await Promise.resolve();
    await Promise.resolve();
    expect(errors).toEqual(["这份样本无法播放(文件可能已损坏或被移动)"]);
  });

  it("快速切换后过期的拒绝不触发 onError", async () => {
    const errors: string[] = [];
    const p1: PlayerLike = {
      play: () => Promise.reject(new Error("k1-error")),
      pause: vi.fn(),
      onended: null,
    };
    const p2: PlayerLike = {
      play: () => Promise.resolve(),
      pause: vi.fn(),
      onended: null,
    };
    let useP1 = true;
    const a = createAudition(
      () => (useP1 ? p1 : p2),
      () => {},
      (msg) => errors.push(msg),
    );
    a.toggle("k1", "/a.wav");
    useP1 = false;
    a.toggle("k2", "/b.wav");
    await Promise.resolve();
    await Promise.resolve();
    expect(errors).toEqual([]);
    expect(a.key).toBe("k2");
  });
});

describe("describePlayError", () => {
  it("NotSupportedError → 样本损坏/被移动的中文提示", () => {
    const err = Object.assign(new Error("Failed to load because no supported source was found."), {
      name: "NotSupportedError",
    });
    expect(describePlayError(err)).toBe("这份样本无法播放(文件可能已损坏或被移动)");
  });

  it("消息里含 no supported source 也按已知错误归一(无 name 的场景)", () => {
    expect(describePlayError("NotSupportedError: Failed to load because no supported source was found.")).toBe(
      "这份样本无法播放(文件可能已损坏或被移动)",
    );
  });

  it("未知错误保留原文,不硬编码误导文案", () => {
    expect(describePlayError(new Error("boom"))).toBe("Error: boom");
  });
});
