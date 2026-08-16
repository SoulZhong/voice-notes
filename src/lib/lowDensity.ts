/** 「疑似识别失败」的段落判定与统计。
 *
 * 由来(2026-08-16 实测):一场 7 分钟录音里,sense_voice 把一个 14.6 秒的段解成 "."、
 * 一个 10.7 秒的段解成「应该我觉得。」;全场 4 段这样的,共 27 秒说话内容基本没进笔记。
 * 更长的那场 98 分钟里有 39 段、共 286 秒。这些段的音频**是好的**——同一段
 * (RMS 0.10、有声帧 58%,与转写正常的对照段一致)FireRed 解出的是连贯中文,
 * 而把同一段切成 3~5 秒小块再喂 sense_voice 只会多吐跨语种噪声("Any you."、
 * "F question."、"我新 제。"),字数上去了内容没上去——所以救回它们的办法是**换引擎**,
 * 不是切块,更不是把它们当垃圾抑制掉。
 *
 * 判据刻意保守:段长 ≥ 3 秒(短句本就可能只有两三个字)且有效字符 ≤ 2。有效字符 =
 * 去掉标点、空白之后剩下的字。历史上有过按"低字/秒"抑制这类段的规则,后来撤了——
 * 它们不是幻觉,是内容丢失,抑制等于掩盖。这里只做"看见并可修"。
 */

/** 判定用的最小段长(毫秒)。 */
export const LOW_DENSITY_MIN_MS = 3000;
/** 判定用的有效字符上限(含)。 */
export const LOW_DENSITY_MAX_CHARS = 2;

/** 去掉标点/空白后的有效字符数。中英日韩与数字都算,`.`「。」`?` 之类不算。 */
export function effectiveChars(text: string): number {
  return (text.match(/[\p{L}\p{N}]/gu) ?? []).length;
}

export type SegmentLike = { start_ms: number; end_ms: number; text: string };

export type LowDensityStat = {
  /** 命中段数。 */
  count: number;
  /** 命中段的总时长(秒,向下取整)。 */
  seconds: number;
};

/** 统计一篇笔记里疑似识别失败的段。 */
export function lowDensityStat(segments: SegmentLike[]): LowDensityStat {
  let count = 0;
  let ms = 0;
  for (const s of segments) {
    const span = s.end_ms - s.start_ms;
    if (span < LOW_DENSITY_MIN_MS) continue;
    if (effectiveChars(s.text ?? "") > LOW_DENSITY_MAX_CHARS) continue;
    count += 1;
    ms += span;
  }
  return { count, seconds: Math.floor(ms / 1000) };
}

/** 值不值得向用户提这件事。单独一两段可能只是有人清嗓子,不打扰;
 *  三段起才提——真出问题时(实测 4 段 / 39 段)一定过线。 */
export const LOW_DENSITY_HINT_MIN_COUNT = 3;

export function shouldOfferBetterEngine(
  stat: LowDensityStat,
  opts: {
    /** **这篇**当初实际用的引擎(note.meta.asr_engine);老笔记没记就传 undefined。 */
    noteEngine: string | undefined;
    /** FireRed 与重转写所需的其它模型(VAD)都在本地。 */
    ready: boolean;
    /** 现在能不能开始一次重转写(没在录制/精修/已有重转写在跑,且本篇已完成)。 */
    actionable: boolean;
  },
): boolean {
  if (stat.count < LOW_DENSITY_HINT_MIN_COUNT) return false;
  // 判据必须看**这篇当初用的**引擎,不是当前全局设置(Codex P2):设置改过之后,
  // sense_voice 转出来的老笔记会莫名其妙不再提示,而 FireRed 转的笔记反被建议
  // "换 FireRed"。老笔记没记引擎(字段是后加的)时按"值得一试"处理。
  if (opts.noteEngine === "firered") return false;
  // 云端转的场次不在此列:那是另一套质量问题,换本地引擎不构成"更强"。
  if (opts.noteEngine?.startsWith("cloud:")) return false;
  return opts.ready && opts.actionable;
}
