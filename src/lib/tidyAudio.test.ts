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
});
