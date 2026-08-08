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

/// 对一个笔记目录做完整补生成:读两条源轨(wav 优先,仅 m4a 则临时解码)→
/// 复用/现估对齐映射 → tmp 写出 → 清过期读数 → 原子改名 → 记账(offset + MixInfo)
/// → 波形。**调用方须持有 NoteLock 并保证录制/重转写/转码互斥**(命令壳的守卫链);
/// 本函数只管盘面正确性:任何失败都不留下可被误认的半成品(tmp 删除,正品要么
/// 完整落位要么根本不存在)。
pub fn regen_note_dir(dir: &std::path::Path) -> anyhow::Result<RegenOutcome> {
    use crate::pipeline::recording_sink::MIXED_TRACK;
    let meta = crate::store::audio::load_audio_meta(dir);
    let read_src = |src: &str| -> anyhow::Result<(Vec<u8>, u64)> {
        let off = meta.tracks.get(src).map(|t| t.offset_ms).unwrap_or(0);
        let wav = dir.join(format!("{src}.wav"));
        if wav.is_file() {
            return Ok((std::fs::read(&wav)?, off));
        }
        let m4a = dir.join(format!("{src}.m4a"));
        anyhow::ensure!(m4a.is_file(), "缺少 {src} 轨(wav/m4a 都不在),无法补生成");
        // 解码到点前缀临时名:不会被 sources_with_suffix 的目录扫描当成轨
        let tmp = dir.join(format!(".mixregen_{src}.wav"));
        let out = crate::store::transcode::decode_m4a_to_standard_wav(&m4a, &tmp)
            .and_then(|_| Ok(std::fs::read(&tmp)?));
        let _ = std::fs::remove_file(&tmp);
        Ok((out?, off))
    };
    let (mic, mic_off) = read_src("mic")?;
    let (sys, sys_off) = read_src("system")?;

    // 对齐映射:回放侧已估过就复用(同一笔记同一映射,回放与成品轨口径一致);
    // 没有则现估,worth_correcting 才采纳,采纳即落盘供回放复用。估不出 = 不纠正,
    // 与 align_mic_track 的保守语义一致(历史轨漂移烘进产物是 spec 已知限制 1)。
    let map = crate::store::align::read(dir).or_else(|| {
        let a = crate::player_align::estimate(&mic, mic_off, &sys, sys_off)?;
        if !crate::player_align::worth_correcting(&a) {
            return None;
        }
        if let Err(e) = crate::store::align::write(dir, &a.map) {
            eprintln!("补生成:对齐映射落盘失败(不影响本次产物): {e}");
        }
        Some(a.map)
    });

    let tmp = dir.join(format!("{MIXED_TRACK}.wav.tmp"));
    let outcome = (|| -> anyhow::Result<RegenOutcome> {
        let mut f = std::io::BufWriter::new(std::fs::File::create(&tmp)?);
        let o = regen_mixed_to(&mic, mic_off, &sys, sys_off, map.as_ref(), &mut f)?;
        f.flush()?;
        Ok(o)
    })()
    .inspect_err(|_| {
        let _ = std::fs::remove_file(&tmp);
    })?;
    anyhow::ensure!(
        std::fs::metadata(&tmp)?.len() > HEADER_LEN,
        "补生成产物为空,已放弃"
    );

    // 清旧形态必须在 rename 之前:mixed.m4a 若残留,track_info_for 会优先采信
    // m4a + 旧 duration_ms,新 wav 永远读不到;旧 waveform/duration 同理是过期读数。
    let _ = std::fs::remove_file(dir.join(format!("{MIXED_TRACK}.m4a")));
    crate::store::audio::reset_mixed_meta(dir, outcome.offset_ms)?;
    std::fs::rename(&tmp, dir.join(format!("{MIXED_TRACK}.wav")))?;
    crate::store::audio::set_track_mix(
        dir,
        MIXED_TRACK,
        crate::store::audio::MixInfo {
            origin: "regen".into(),
            // regen 按 offset_ms 定位,段落 seek 无需修正 ⇒ 空表(见模块头注)
            seek_offset_ms: Default::default(),
            track_ms: outcome.track_ms,
        },
    )?;
    match crate::store::audio::waveform_from_wav(&dir.join(format!("{MIXED_TRACK}.wav"))) {
        Ok(w) => {
            if let Err(e) = crate::store::audio::set_track_waveform(dir, MIXED_TRACK, w) {
                eprintln!("补生成:波形写入失败(只降级,前端退段落包络): {e}");
            }
        }
        Err(e) => eprintln!("补生成:波形计算失败(只降级): {e}"),
    }
    Ok(outcome)
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

    /// 目录级闭环:两条源轨(带 sync 对账)→ regen_note_dir → mixed.wav 落位、
    /// audio.json 记账(offset/MixInfo/波形)、mixed_untrusted 判可信、seek 表为空。
    /// 短轨(<30s)估不出对齐映射 → map=None 直混,正好覆盖"无 align.json"分支。
    #[test]
    fn regen_note_dir_produces_trusted_marked_track() {
        let dir = tempfile::tempdir().unwrap();
        let n = 16_000usize; // 1s
        std::fs::write(dir.path().join("mic.wav"), wav(&vec![100i16; n])).unwrap();
        std::fs::write(dir.path().join("system.wav"), wav(&vec![10i16; n])).unwrap();
        for src in ["mic", "system"] {
            crate::store::audio::set_track_sync(
                dir.path(),
                src,
                crate::store::audio::SyncInfo {
                    wall_ms: 1000, samples: 48_000, track_ms: 1000, drift_ms: 0,
                    silence_ms: 0, gaps: 0, rate_fixes: 0, first_frame_offset_ms: Some(0),
                },
            )
            .unwrap();
        }
        let outcome = regen_note_dir(dir.path()).unwrap();
        assert_eq!(outcome.offset_ms, 0);
        assert_eq!(outcome.track_ms, 1000);
        let meta = crate::store::audio::load_audio_meta(dir.path());
        let t = meta.tracks.get("mixed").expect("mixed 条目");
        let mix = t.mix.as_ref().expect("regen 定稿必须写 MixInfo");
        assert_eq!(mix.origin, "regen");
        assert!(mix.seek_offset_ms.is_empty(), "regen 按 offset 定位,无 seek 修正");
        assert!(t.waveform.is_some(), "波形应同步写出");
        assert_eq!(
            crate::retranscribe::input::mixed_untrusted(&meta),
            None,
            "补生成产物必须能过消费前校验"
        );
        let pcm = pcm_of(&std::fs::read(dir.path().join("mixed.wav")).unwrap());
        assert_eq!(pcm.len(), n);
        assert!(pcm.iter().all(|&s| (s as i32 - 110).abs() <= 1));
    }

    /// 覆盖旧产物:残留的 mixed.m4a 与过期 duration/waveform 必须被清掉,
    /// 只留新 wav 一个形态(track_info_for 是 m4a 优先,不清会读到旧产物)。
    #[test]
    fn regen_note_dir_clears_stale_m4a_and_readings() {
        let dir = tempfile::tempdir().unwrap();
        let n = 1600usize;
        std::fs::write(dir.path().join("mic.wav"), wav(&vec![1i16; n])).unwrap();
        std::fs::write(dir.path().join("system.wav"), wav(&vec![2i16; n])).unwrap();
        std::fs::write(dir.path().join("mixed.m4a"), b"stale").unwrap();
        regen_note_dir(dir.path()).unwrap();
        assert!(!dir.path().join("mixed.m4a").exists(), "旧 m4a 必须清除");
        let meta = crate::store::audio::load_audio_meta(dir.path());
        let t = meta.tracks.get("mixed").unwrap();
        assert!(t.codec.is_none() && t.duration_ms.is_none(), "过期读数必须清零");
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
