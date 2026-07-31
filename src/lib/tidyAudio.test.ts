import { describe, expect, it, vi } from "vitest";
import { createAudition, type PlayerLike } from "./tidyAudio";

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
      play: () => Promise.reject(new Error("NotAllowedError")),
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
    expect(errors).toEqual(["Error: NotAllowedError"]);
    expect(a.key).toBeNull();
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
