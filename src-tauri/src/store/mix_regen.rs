//! 离线补生成成品轨:两条源轨(canonical WAV)按 offset_ms 铺进共同时间轴,
//! 走与录制期同一个 `TimelineMixer` 混音后写出 mixed WAV。
//!
//! 与录制期(recording_sink)的三点口径差,消费方必须知道:
//! - **按 offset_ms 定位**,不是首帧偏移 ⇒「文件内毫秒 + offset_ms == 时间轴毫秒」
//!   经典口径成立,段落 seek 不需要任何修正(MixInfo.origin = "regen",seek 表为空);
//! - 历史轨的时钟漂移不在源头消除:调用方可传 `player_align` 的 TimeMap,mic 轨在
//!   **生成期**重采样到 system 时基(spec §离线补生成:对齐烘进产物,回放期零估计);
//! - 喂料按时间轴区间在两源间交替推进,一源在区间内无内容时用空块推进其虚拟位置,
//!   保证水位线前进、混音窗有界(≈2 个区间),不随轨长增长——录制期靠
//!   MAX_MIXER_WINDOW_SAMPLES 自杀守卫,这里靠喂料结构本身。
use std::io::{Seek, SeekFrom, Write};

use crate::audio::timeline_mix::{TimelineMixer, MIC, SYSTEM};
use crate::player_align::TimeMap;
use crate::store::audio::{bytes_to_ms, f32_to_s16, wav_header, HEADER_LEN};

/// 补生成产物的落盘口径:写进 audio.json 的 offset 与 MixInfo.track_ms。
pub struct RegenOutcome {
    pub offset_ms: u64,
    pub track_ms: u64,
}

/// 每轮处理的时间轴区间长度(样本)。1 秒:窗口内存 ≈ 2 秒 f32(128KB),
/// 与逐块 IO 的开销之间取的平衡值,无声学含义。
const CHUNK: u64 = 16_000;

/// 两轨 canonical WAV(44 头、16k 单声道 s16le)字节 + 各自 offset_ms(+可选
/// mic→system 时基映射)→ 把成品轨 WAV 写进 sink。纯函数,无盘面副作用;
/// 调用方负责 tmp + 原子改名与 audio.json 记账。
pub fn regen_mixed_to<W: Write + Seek>(
    mic: &[u8],
    mic_off_ms: u64,
    sys: &[u8],
    sys_off_ms: u64,
    map: Option<&TimeMap>,
    sink: &mut W,
) -> anyhow::Result<RegenOutcome> {
    // 对齐发生在生成期:mic 重采样到 system 时基,offset 用 player 同款公式换算
    // (aligned_track_offset_ms:铺在 system 本地时基上,mic 先开录时起点为负、
    // offset 相应前移)。产物定稿后回放不再做任何估计。
    let (mic_bytes, mic_off_ms) = match map {
        Some(map) => {
            let mut aligned = std::io::Cursor::new(Vec::new());
            crate::player_align::render_aligned_to(mic, map, &mut aligned)?;
            (
                std::borrow::Cow::Owned(aligned.into_inner()),
                crate::player::aligned_track_offset_ms(sys_off_ms, map),
            )
        }
        None => (std::borrow::Cow::Borrowed(mic), mic_off_ms),
    };

    let mic_pcm = wav_body_f32(&mic_bytes)?;
    let sys_pcm = wav_body_f32(sys)?;

    // 时间轴原点 = 较早的 offset;两源起点是相对原点的样本偏移。
    let origin_ms = mic_off_ms.min(sys_off_ms);
    let starts = [(mic_off_ms - origin_ms) * 16, (sys_off_ms - origin_ms) * 16];
    let pcms: [&[f32]; 2] = [&mic_pcm, &sys_pcm];
    debug_assert!(MIC == 0 && SYSTEM == 1, "喂料下标与 TimelineMixer 源号约定一致");

    sink.write_all(&wav_header(0))?;
    let mut data_len: u64 = 0;
    let mut mixer = TimelineMixer::new(0); // 离线到达有序,无需水位余量
    let total_end = (0..2).map(|i| starts[i] + pcms[i].len() as u64).max().unwrap_or(0);
    let mut cursor = 0u64;
    while cursor < total_end {
        let next = (cursor + CHUNK).min(total_end);
        for src in 0..2 {
            let s_start = starts[src];
            let s_end = s_start + pcms[src].len() as u64;
            let lo = cursor.max(s_start);
            let hi = next.min(s_end);
            let out = if lo < hi {
                let (a, b) = ((lo - s_start) as usize, (hi - s_start) as usize);
                mixer.accept_at(src, lo, &pcms[src][a..b])
            } else {
                // 本源在该区间无内容(未开始/已结束):空块推进虚拟位置,水位线
                // 才能越过该区间,窗口不随另一源的独占段增长。
                mixer.accept_at(src, next, &[])
            };
            data_len += write_s16(sink, &out)?;
        }
        cursor = next;
    }
    data_len += write_s16(sink, &mixer.finish())?;

    // 回填真实长度(与 repair_wav_header 同口径:头描述的必须是实际字节)。
    sink.seek(SeekFrom::Start(0))?;
    sink.write_all(&wav_header(u32::try_from(data_len).unwrap_or(u32::MAX)))?;
    sink.seek(SeekFrom::End(0))?;

    Ok(RegenOutcome { offset_ms: origin_ms, track_ms: bytes_to_ms(data_len) })
}

/// canonical WAV 的 data 段 → f32 样本(s16le / 32768,与解码端一致)。
fn wav_body_f32(wav: &[u8]) -> anyhow::Result<Vec<f32>> {
    let body = wav
        .get(HEADER_LEN as usize..)
        .ok_or_else(|| anyhow::anyhow!("WAV 短于 {HEADER_LEN} 字节头,内容损坏"))?;
    Ok(body
        .chunks_exact(2)
        .map(|b| i16::from_le_bytes([b[0], b[1]]) as f32 / 32768.0)
        .collect())
}

fn write_s16<W: Write>(sink: &mut W, samples: &[f32]) -> anyhow::Result<u64> {
    let mut buf = Vec::with_capacity(samples.len() * 2);
    for s in samples {
        buf.extend_from_slice(&f32_to_s16(*s).to_le_bytes());
    }
    sink.write_all(&buf)?;
    Ok(buf.len() as u64)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    /// canonical WAV 构造器:44 头 + s16le@16k 单声道。
    fn wav(samples: &[i16]) -> Vec<u8> {
        let data: Vec<u8> = samples.iter().flat_map(|s| s.to_le_bytes()).collect();
        let mut v = Vec::with_capacity(44 + data.len());
        v.extend_from_slice(&wav_header(data.len() as u32));
        v.extend_from_slice(&data);
        v
    }

    fn pcm_of(out: &[u8]) -> Vec<i16> {
        out[44..].chunks_exact(2).map(|b| i16::from_le_bytes([b[0], b[1]])).collect()
    }

    /// 等 offset 两轨:输出逐样本和(±1 LSB:i16→f32→i16 的一次往返截断),
    /// offset 取公共值,track_ms 按字节量出。
    #[test]
    fn same_offset_tracks_sum_pointwise() {
        let mic = wav(&[1000, 2000, 3000]);
        let sys = wav(&[10, 20, 30]);
        let mut out = Cursor::new(Vec::new());
        let r = regen_mixed_to(&mic, 500, &sys, 500, None, &mut out).unwrap();
        assert_eq!(r.offset_ms, 500);
        let got = pcm_of(out.get_ref());
        for (g, w) in got.iter().zip([1010i16, 2020, 3030]) {
            assert!((*g as i32 - w as i32).abs() <= 1, "got {got:?}");
        }
    }

    /// 不等 offset:后起源在成品轨里带前导独占区,位置按时间轴对齐。
    /// origin = min(offset);system 晚 1ms(16 样本)⇒ 其内容整体右移 16。
    #[test]
    fn offset_gap_becomes_leading_solo_region() {
        let mic = wav(&[100; 32]);
        let sys = wav(&[7; 16]);
        let mut out = Cursor::new(Vec::new());
        let r = regen_mixed_to(&mic, 0, &sys, 1, None, &mut out).unwrap();
        assert_eq!(r.offset_ms, 0);
        let pcm = pcm_of(out.get_ref());
        assert_eq!(pcm.len(), 32);
        assert!(pcm[..16].iter().all(|&s| (s as i32 - 100).abs() <= 1), "前 16 样本 mic 独占: {pcm:?}");
        assert!(pcm[16..].iter().all(|&s| (s as i32 - 107).abs() <= 1), "后 16 样本两源叠加: {pcm:?}");
    }

    /// 和溢出 s16 → clamp,不回绕(f32_to_s16 同口径)。
    #[test]
    fn sum_clamps_instead_of_wrapping() {
        let mic = wav(&[i16::MAX]);
        let sys = wav(&[i16::MAX]);
        let mut out = Cursor::new(Vec::new());
        regen_mixed_to(&mic, 0, &sys, 0, None, &mut out).unwrap();
        assert_eq!(pcm_of(out.get_ref()), vec![i16::MAX]);
    }

    /// 恒等映射(0→0, 10→10)走对齐分支但不改内容——验证 map 管线接通且
    /// offset 修正为 0(= sys_off + map(0))。
    #[test]
    fn identity_map_is_passthrough() {
        let map = crate::player_align::TimeMap::new(vec![(0.0, 0.0), (10.0, 10.0)]).unwrap();
        let mic = wav(&[500; 160]);
        let sys = wav(&[5; 160]);
        let mut out = Cursor::new(Vec::new());
        let r = regen_mixed_to(&mic, 0, &sys, 0, Some(&map), &mut out).unwrap();
        assert_eq!(r.offset_ms, 0);
        let pcm = pcm_of(out.get_ref());
        assert_eq!(pcm.len(), 160);
        // 末样本除外:重采样插值在流末端拿不到下一个样本,边缘落 0 是固有行为
        // (实测 mic 末样本被渲成 0,和只剩 system 的 5)。中段必须保真。
        assert!(pcm[..159].iter().all(|&s| (s as i32 - 505).abs() <= 3), "{pcm:?}");
    }

    /// RIFF/data 头长度字段与实际数据一致(流式写 + Seek 回补的正确性),
    /// track_ms 与数据字节一致。
    #[test]
    fn header_lengths_match_payload() {
        let mic = wav(&[1; 16_000]);
        let sys = wav(&[2; 16_000]);
        let mut out = Cursor::new(Vec::new());
        let r = regen_mixed_to(&mic, 0, &sys, 0, None, &mut out).unwrap();
        let b = out.get_ref();
        let riff = u32::from_le_bytes([b[4], b[5], b[6], b[7]]) as usize;
        let data = u32::from_le_bytes([b[40], b[41], b[42], b[43]]) as usize;
        assert_eq!(riff, b.len() - 8);
        assert_eq!(data, b.len() - 44);
        assert_eq!(r.track_ms, 1000, "16000 样本 = 1s");
    }

    /// 一源远长于另一源(独占尾巴跨多个区间):内容不丢、位置不漂,
    /// 且混音窗不随独占段长度增长(有界喂料的回归锁)。
    #[test]
    fn long_solo_tail_stays_bounded_and_complete() {
        let mic = wav(&vec![50i16; (CHUNK * 5) as usize]); // 5 秒
        let sys = wav(&[10i16; 16]);
        let mut out = Cursor::new(Vec::new());
        regen_mixed_to(&mic, 0, &sys, 0, None, &mut out).unwrap();
        let pcm = pcm_of(out.get_ref());
        assert_eq!(pcm.len(), (CHUNK * 5) as usize);
        assert!(pcm[..16].iter().all(|&s| (s as i32 - 60).abs() <= 1));
        assert!(pcm[16..].iter().all(|&s| (s as i32 - 50).abs() <= 1));
    }
}
