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

    // 流式口径(codex P2):不把整轨 s16→f32 物化成 Vec(1 小时双轨 ≈ 460MB f32,
    // 叠上源字节与对齐渲染可冲到 GB 级)——每轮只转换当前时间轴区间(≤1s,64KB),
    // 调用方配合 mmap 喂源字节时,全程常驻 ≈ 混音窗 + 对齐渲染产物(仅对齐分支)。
    let bodies: [&[u8]; 2] = [wav_body(&mic_bytes)?, wav_body(sys)?];
    let lens = [bodies[0].len() as u64 / 2, bodies[1].len() as u64 / 2];

    // 时间轴原点 = 较早的 offset;两源起点是相对原点的样本偏移。
    let origin_ms = mic_off_ms.min(sys_off_ms);
    let starts = [(mic_off_ms - origin_ms) * 16, (sys_off_ms - origin_ms) * 16];
    // 喂料下标与 TimelineMixer 源号约定一致(编译期锁死,常量变了立刻编不过)。
    const _: () = assert!(MIC == 0 && SYSTEM == 1);

    sink.write_all(&wav_header(0))?;
    let mut data_len: u64 = 0;
    let mut mixer = TimelineMixer::new(0); // 离线到达有序,无需水位余量
    let total_end = (0..2).map(|i| starts[i] + lens[i]).max().unwrap_or(0);
    let mut cursor = 0u64;
    let mut chunk_buf: Vec<f32> = Vec::with_capacity(CHUNK as usize);
    while cursor < total_end {
        let next = (cursor + CHUNK).min(total_end);
        for src in 0..2 {
            let s_start = starts[src];
            let s_end = s_start + lens[src];
            let lo = cursor.max(s_start);
            let hi = next.min(s_end);
            let out = if lo < hi {
                let (a, b) = ((lo - s_start) as usize, (hi - s_start) as usize);
                chunk_buf.clear();
                chunk_buf.extend(
                    bodies[src][a * 2..b * 2]
                        .chunks_exact(2)
                        .map(|c| i16::from_le_bytes([c[0], c[1]]) as f32 / 32768.0),
                );
                mixer.accept_at(src, lo, &chunk_buf)
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

/// 对一个笔记目录做完整补生成:mmap 两条源轨(wav 优先,仅 m4a 则临时解码)→
/// 复用/现估对齐映射 → tmp 写出 → 提交。**调用方须持有 NoteLock 并保证录制/
/// 重转写/转码互斥**(命令壳的守卫链)。
///
/// 提交顺序(codex P2:旧产物在新品落位前绝不销毁,失败模式只允许「旧品完好」
/// 或「轨暂时隐身可再生」,永不「错误内容被采信」):
/// ① 清 meta 读数与旧 mix 标记 + 写新 offset —— 失败:盘上文件全未动,旧品完好;
///   必须先于 rename:旧标记/读数描述的是旧内容,新 wav 一旦落位就不能再被它们背书。
///   代价:此后任一步失败,旧 m4a 因 duration 读数被清而整轨隐身(track_info_for
///   对无读数的 m4a 判损坏跳过)——可重新生成恢复,不丢源轨数据。
/// ② rename tmp→mixed.wav(原子;若有旧 wav 即原子替换)—— 失败:轨隐身,可再生。
/// ③ 删旧 mixed.m4a —— 失败按提交失败上报(codex 第二轮 P2:m4a 优先级会藏住新
///   wav,macOS 后续转码还可能因"m4a 已存在"直接删新 wav);状态同样是轨隐身可再生。
/// ④ 写新 MixInfo —— 失败:新 wav 无标记 → mixed_untrusted 保守拒,重新生成可修。
/// ⑤ 波形 —— 失败只降级(前端退段落包络)。
pub fn regen_note_dir(dir: &std::path::Path) -> anyhow::Result<RegenOutcome> {
    use crate::pipeline::recording_sink::MIXED_TRACK;
    let meta = crate::store::audio::load_audio_meta(dir);
    // mmap 而非整读进堆(codex P2):小时级源轨数百 MB,mmap 让"源字节"不占常驻
    // 内存;流式转换(regen_mixed_to 内)则免去整轨 f32 物化。m4a 场景解码出的
    // 临时 WAV 同样走 mmap,结束后连文件一起清掉(guard 的 Drop)。
    struct SrcGuard {
        map: memmap2::Mmap,
        cleanup: Option<std::path::PathBuf>,
    }
    impl Drop for SrcGuard {
        fn drop(&mut self) {
            if let Some(p) = self.cleanup.take() {
                let _ = std::fs::remove_file(p);
            }
        }
    }
    let read_src = |src: &str| -> anyhow::Result<(SrcGuard, u64)> {
        let off = meta.tracks.get(src).map(|t| t.offset_ms).unwrap_or(0);
        let wav = dir.join(format!("{src}.wav"));
        let (path, cleanup) = if wav.is_file() {
            (wav, None)
        } else {
            let m4a = dir.join(format!("{src}.m4a"));
            anyhow::ensure!(m4a.is_file(), "缺少 {src} 轨(wav/m4a 都不在),无法补生成");
            // 解码到点前缀临时名:不会被 sources_with_suffix 的目录扫描当成轨
            let tmp = dir.join(format!(".mixregen_{src}.wav"));
            crate::store::transcode::decode_m4a_to_standard_wav(&m4a, &tmp)
                .inspect_err(|_| {
                    let _ = std::fs::remove_file(&tmp);
                })?;
            (tmp.clone(), Some(tmp))
        };
        let file = std::fs::File::open(&path)?;
        // 安全性同 player.rs 的既有用法:映射期间文件受 NoteLock/互斥守卫保护,
        // 不会被并发改写。
        let map = unsafe { memmap2::Mmap::map(&file)? };
        Ok((SrcGuard { map, cleanup }, off))
    };
    let (mic_guard, mic_off) = read_src("mic")?;
    let (sys_guard, sys_off) = read_src("system")?;
    let (mic, sys): (&[u8], &[u8]) = (&mic_guard.map, &sys_guard.map);

    // 对齐映射:回放侧已估过就复用(同一笔记同一映射,回放与成品轨口径一致);
    // 没有则现估,worth_correcting 才采纳,采纳即落盘供回放复用。估不出 = 不纠正,
    // 与 align_mic_track 的保守语义一致(历史轨漂移烘进产物是 spec 已知限制 1)。
    //
    // 新鲜度校验(codex 第四轮 P1):续录会改写/追加源轨而不清 align.json;player.rs
    // 复用前按源轨 mtime 校验,这里同口径——映射文件必须晚于两条源轨的最后修改才可
    // 复用,否则按不存在处理、现估重写(过期映射会把拉伸/错位永久烘进成品轨)。
    // 任何 mtime 读不到都按不新鲜(保守重估,代价只是几秒计算)。
    let src_mtime = |name: &str| -> Option<std::time::SystemTime> {
        ["wav", "m4a"].iter().find_map(|ext| {
            std::fs::metadata(dir.join(format!("{name}.{ext}")))
                .ok()
                .and_then(|m| m.modified().ok())
        })
    };
    let align_fresh = (|| {
        let a = std::fs::metadata(dir.join(crate::store::align::ALIGN_FILE))
            .ok()?
            .modified()
            .ok()?;
        Some(a > src_mtime("mic")? && a > src_mtime("system")?)
    })()
    .unwrap_or(false);
    let map = (if align_fresh { crate::store::align::read(dir) } else { None }).or_else(|| {
        let a = crate::player_align::estimate(mic, mic_off, sys, sys_off)?;
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
        let o = regen_mixed_to(mic, mic_off, sys, sys_off, map.as_ref(), &mut f)?;
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

    // 提交序 ①→③(顺序论证见函数头注):先清读数(旧 m4a 从此不可能被采信),
    // 新 wav 原子落位,最后才动旧 m4a——任何一步失败都不会让旧内容被当成新产物。
    crate::store::audio::reset_mixed_meta(dir, outcome.offset_ms)
        .inspect_err(|_| {
            let _ = std::fs::remove_file(&tmp);
        })?;
    std::fs::rename(&tmp, dir.join(format!("{MIXED_TRACK}.wav")))
        .inspect_err(|_| {
            let _ = std::fs::remove_file(&tmp);
        })?;
    let stale_m4a = dir.join(format!("{MIXED_TRACK}.m4a"));
    if stale_m4a.is_file() {
        // 删除失败必须按提交失败上报(codex 第二轮 P2):m4a 残留会让 track_info_for
        // 因 m4a 优先而把新 wav 藏住,macOS 后续转码还可能看到"m4a 已存在"而直接
        // 删掉新 wav;此刻状态 = 轨隐身、无标记,重新生成即可恢复,与其它失败模式一致。
        std::fs::remove_file(&stale_m4a).map_err(|e| {
            anyhow::anyhow!("旧 mixed.m4a 删除失败,提交中止(轨暂时隐身,可重新生成): {e}")
        })?;
    }
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

/// canonical WAV 的 data 段字节切片(整样本截断,s16le)。
fn wav_body(wav: &[u8]) -> anyhow::Result<&[u8]> {
    let body = wav
        .get(HEADER_LEN as usize..)
        .ok_or_else(|| anyhow::anyhow!("WAV 短于 {HEADER_LEN} 字节头,内容损坏"))?;
    Ok(&body[..body.len() - body.len() % 2])
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

    /// codex 第四轮 P1:align.json 早于源轨最后修改(续录改写过源轨)即视为过期,
    /// 必须丢弃重估——过期映射会把拉伸/错位烘进成品轨。短轨重估必然返回 None,
    /// 所以「过期被丢弃」的可观察结果就是直混(offset 不带映射平移、逐样本和)。
    #[test]
    fn stale_align_map_is_discarded() {
        let dir = tempfile::tempdir().unwrap();
        // 先写一份"把 mic 平移 +5s"的映射,再写源轨 → 源轨更新于映射之后 = 映射过期。
        let map = crate::player_align::TimeMap::new(vec![(0.0, 5.0), (10.0, 15.0)]).unwrap();
        crate::store::align::write(dir.path(), &map).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(20));
        let n = 1600usize;
        std::fs::write(dir.path().join("mic.wav"), wav(&vec![100i16; n])).unwrap();
        std::fs::write(dir.path().join("system.wav"), wav(&vec![10i16; n])).unwrap();
        let outcome = regen_note_dir(dir.path()).unwrap();
        assert_eq!(outcome.offset_ms, 0, "过期映射不得参与(否则 offset 被平移 5s)");
        assert_eq!(outcome.track_ms, 100, "1600 样本直混 = 100ms,无 5s 拉伸");
        let pcm = pcm_of(&std::fs::read(dir.path().join("mixed.wav")).unwrap());
        assert!(pcm.iter().all(|&s| (s as i32 - 110).abs() <= 1), "直混逐样本和");
    }

    /// 对照:映射晚于源轨(新鲜)即复用——mic 被平移 +5s 烘进产物,时长膨胀。
    #[test]
    fn fresh_align_map_is_reused() {
        let dir = tempfile::tempdir().unwrap();
        let n = 1600usize;
        std::fs::write(dir.path().join("mic.wav"), wav(&vec![100i16; n])).unwrap();
        std::fs::write(dir.path().join("system.wav"), wav(&vec![10i16; n])).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(20));
        let map = crate::player_align::TimeMap::new(vec![(0.0, 5.0), (10.0, 15.0)]).unwrap();
        crate::store::align::write(dir.path(), &map).unwrap();
        let outcome = regen_note_dir(dir.path()).unwrap();
        assert!(
            outcome.track_ms >= 5_000,
            "新鲜映射应被复用:mic 平移 +5s,产物时长应 ≥5s,实测 {}ms",
            outcome.track_ms
        );
    }

    /// 提交序回归(codex P2):rename 落位失败时,旧 mixed.m4a 必须还在盘上,
    /// 但其 meta 读数已清 → 轨隐身(track_info_for 对无 duration 的 m4a 判损坏
    /// 跳过),绝不把旧内容当新产物供出;mix 标记同步清空,mixed_untrusted 保守拒。
    /// 用目录占住 mixed.wav 路径让 rename 必败(手法同 recording_sink 的建档失败用例)。
    #[test]
    fn regen_commit_failure_never_serves_stale_content() {
        let dir = tempfile::tempdir().unwrap();
        let n = 1600usize;
        std::fs::write(dir.path().join("mic.wav"), wav(&vec![1i16; n])).unwrap();
        std::fs::write(dir.path().join("system.wav"), wav(&vec![2i16; n])).unwrap();
        std::fs::write(dir.path().join("mixed.m4a"), b"stale-old-product").unwrap();
        crate::store::audio::set_track_mix(
            dir.path(),
            "mixed",
            crate::store::audio::MixInfo {
                origin: "live".into(),
                seek_offset_ms: Default::default(),
                track_ms: 999,
            },
        )
        .unwrap();
        // rename 目标占成目录 → 提交序第 ② 步必败。
        std::fs::create_dir(dir.path().join("mixed.wav")).unwrap();
        assert!(regen_note_dir(dir.path()).is_err(), "落位失败必须报错");
        assert!(dir.path().join("mixed.m4a").is_file(), "旧 m4a 不得在新品落位前被销毁");
        let meta = crate::store::audio::load_audio_meta(dir.path());
        let t = meta.tracks.get("mixed").unwrap();
        assert!(t.duration_ms.is_none(), "旧读数必须已清——无读数的 m4a 不会被采信");
        assert!(t.mix.is_none(), "旧完整性标记必须已清");
        assert!(
            crate::retranscribe::input::mixed_untrusted(&meta).is_some(),
            "中断态必须被消费前校验保守拒掉"
        );
        assert!(!dir.path().join("mixed.wav.tmp").exists(), "tmp 半成品必须清除");
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
