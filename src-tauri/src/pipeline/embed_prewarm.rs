//! 录音期声纹缓存预热(issue #164):段落定稿即从盘上轨文件切**同一片段**计算嵌入,
//! 写进 embeddings.json——首次 Aing 重聚类/首次拆分不再付全款(80 分钟场实测 724 段
//! 要算数分钟)。
//!
//! 口径统一(设计文档「明确不做」节的解法):不复用实时 registry 的向量(它的窗口是
//! 自有缓冲,与离线切片口径不同,混写会污染缓存语义),而是与 `refine::embed_all`
//! **完全同源重算**——同一 WAV 字节(`read_wav_f32_slice` 与 `read_wav_f32` 逐位同解码)、
//! 同一 offset(`track_offset_ms`)、同一切片算式((ms−offset)×16)、同一模型文件。
//! 由此缓存命中拿到的向量与离线现算逐位一致,零污染。
//!
//! 纯增值旁路:队列有界、满即丢;任何失败只打日志——丢了的段 Aing 时照旧现算。
//! 段落定稿时其音频可能尚未全部落到 WAV(写盘异步),读不满的段**推迟重试**而不是
//! 拿半截音频算出一个与离线不一致的向量。

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::Duration;

use crate::store::embed_cache::EmbedCacheEntry;

pub struct Job {
    pub note_dir: PathBuf,
    pub seq: u64,
    pub source: String,
    pub start_ms: u64,
    pub end_ms: u64,
}

/// 攒批阈值:每满 8 条合并写一次 embeddings.json,避免逐段整写 O(n²) 磁盘量。
const FLUSH_EVERY: usize = 8;
/// 音频未落齐的段最多重试次数。
const MAX_DEFERS: u8 = 10;
/// 推迟重试间隔:每条推迟项挂到期时刻,**每圈**都处理到期项——连续 final 抵达
/// (间隔 < 队列超时)时 recv 永远 Ok,只靠超时拍重试会被饿死(codex P1),
/// 停录即起的自动 Aing 会带着这些未缓存段付全款。
const DEFER_RETRY: Duration = Duration::from_secs(3);

enum Msg {
    Seg(Job),
    /// 排干屏障(codex:自动 Aing 不等 worker 就会重算未落盘的批):FIFO 保证
    /// 在它之前入队的段全部处理完;回执前把该笔记攒着的批与到期推迟项清算落盘。
    Drain(PathBuf, crossbeam_channel::Sender<()>),
}

static TX: Mutex<Option<crossbeam_channel::Sender<Msg>>> = Mutex::new(None);

/// 入队一段(actor 管线 Final 落盘成功后调用)。首次调用惰性起 worker 线程。
/// 队列满即丢:纯缓存,丢了 Aing 时再算,绝不反压录制路径。
pub fn enqueue(app: &tauri::AppHandle, job: Job) {
    let mut g = TX.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let tx = g.get_or_insert_with(|| {
        let (tx, rx) = crossbeam_channel::bounded::<Msg>(1024);
        let app = app.clone();
        std::thread::spawn(move || worker(app, rx));
        tx
    });
    let _ = tx.try_send(Msg::Seg(job));
}

/// 排干一篇笔记的预热在途量(Aing worker 起跑前调用):等 worker 把队列里先于
/// 屏障的段全部算完、该笔记的攒批落盘。有上限等待,超时即放弃——预热是纯增值,
/// 绝不让 Aing 卡在它身上;从未起过 worker 直接返回。
pub fn drain(note_dir: &std::path::Path, timeout: Duration) {
    let tx = {
        let g = TX.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        match g.as_ref() {
            Some(tx) => tx.clone(),
            None => return,
        }
    };
    let (rtx, rrx) = crossbeam_channel::bounded(1);
    if tx.send_timeout(Msg::Drain(note_dir.to_path_buf(), rtx), timeout).is_err() {
        return; // 队列满到塞不进屏障:放弃排干,Aing 照常现算
    }
    if rrx.recv_timeout(timeout).is_err() {
        eprintln!("预热排干超时(不阻塞 Aing,缺的段现算)");
    }
}

fn worker(app: tauri::AppHandle, rx: crossbeam_channel::Receiver<Msg>) {
    // (模型标签, 嵌入器):标签变了重建。模型加载贵,worker 生命周期内复用。
    let mut embedder: Option<(String, crate::diar::SherpaEmbedder)> = None;
    // 待写批:按 (note_dir, model) 攒;正常一场只有一个键。
    let mut pending: HashMap<(PathBuf, String), Vec<EmbedCacheEntry>> = HashMap::new();
    // 音频未落齐推迟重试的段:(job, 已试次数, 到期时刻)。
    let mut deferred: Vec<(Job, u8, std::time::Instant)> = Vec::new();
    loop {
        let msg = match rx.recv_timeout(Duration::from_secs(10)) {
            Ok(m) => Some(m),
            Err(crossbeam_channel::RecvTimeoutError::Timeout) => None,
            Err(crossbeam_channel::RecvTimeoutError::Disconnected) => {
                flush_all(&mut pending);
                return;
            }
        };
        match msg {
            Some(Msg::Seg(job)) => {
                handle(&app, job, 0, &mut embedder, &mut pending, &mut deferred);
            }
            Some(Msg::Drain(dir, reply)) => {
                // 停录后音频已全部落盘:该笔记的推迟项不再等到期,立即清算。
                let (mine, keep): (Vec<_>, Vec<_>) = std::mem::take(&mut deferred)
                    .into_iter()
                    .partition(|(j, _, _)| j.note_dir == dir);
                deferred = keep;
                for (job, tries, _) in mine {
                    handle(&app, job, tries, &mut embedder, &mut pending, &mut deferred);
                }
                // 该笔记二次推迟的(音频真缺)不再等:排干语义是"能算的都算完"。
                deferred.retain(|(j, _, _)| j.note_dir != dir);
                let keys: Vec<(PathBuf, String)> =
                    pending.keys().filter(|(d, _)| *d == dir).cloned().collect();
                for k in keys {
                    if let Some(entries) = pending.remove(&k) {
                        if let Err(e) =
                            crate::store::embed_cache::merge_save(&k.0, &k.1, entries)
                        {
                            eprintln!("预热排干落盘失败(缺的段 Aing 现算): {e}");
                        }
                    }
                }
                let _ = reply.send(());
            }
            None => {
                // 空闲拍:攒着的批落盘(重试不等这一拍,见下)。
                flush_all(&mut pending);
            }
        }
        // 每圈处理到期的推迟项(不管这圈是来活还是超时)。
        let now = std::time::Instant::now();
        let (due, keep): (Vec<_>, Vec<_>) =
            std::mem::take(&mut deferred).into_iter().partition(|(_, _, at)| *at <= now);
        deferred = keep;
        for (job, tries, _) in due {
            handle(&app, job, tries, &mut embedder, &mut pending, &mut deferred);
        }
        // 攒批落盘(逐键判阈值)。
        let full: Vec<(PathBuf, String)> = pending
            .iter()
            .filter(|(_, v)| v.len() >= FLUSH_EVERY)
            .map(|(k, _)| k.clone())
            .collect();
        for k in full {
            if let Some(entries) = pending.remove(&k) {
                if let Err(e) = crate::store::embed_cache::merge_save(&k.0, &k.1, entries) {
                    eprintln!("预热缓存写回失败(丢弃本批,Aing 时再算): {e}");
                }
            }
        }
    }
}

fn flush_all(pending: &mut HashMap<(PathBuf, String), Vec<EmbedCacheEntry>>) {
    for ((dir, model), entries) in pending.drain() {
        if entries.is_empty() {
            continue;
        }
        if let Err(e) = crate::store::embed_cache::merge_save(&dir, &model, entries) {
            eprintln!("预热缓存写回失败(丢弃本批,Aing 时再算): {e}");
        }
    }
}

fn handle(
    app: &tauri::AppHandle,
    job: Job,
    tries: u8,
    embedder: &mut Option<(String, crate::diar::SherpaEmbedder)>,
    pending: &mut HashMap<(PathBuf, String), Vec<EmbedCacheEntry>>,
    deferred: &mut Vec<(Job, u8, std::time::Instant)>,
) {
    let dur = job.end_ms.saturating_sub(job.start_ms);
    if dur < crate::refine::recluster::MIN_EMBED_MS {
        return; // embed_all 对短段也不算,口径一致
    }
    // 模型标签现读现用:场中切模型时新段落按新模型算,与 embed_all 的
    // 「盘上 model 不符整份失效」语义配套(merge_save 弃旧起新)。
    let tag = crate::current_speaker_model(app);
    if tag.is_empty() {
        return;
    }
    if embedder.as_ref().map(|(t, _)| t != &tag).unwrap_or(true) {
        match crate::diar::SherpaEmbedder::new(&crate::speaker_model_path_for(&tag)) {
            Ok(e) => *embedder = Some((tag.clone(), e)),
            Err(e) => {
                eprintln!("预热嵌入器加载失败(本段放弃): {e}");
                return;
            }
        }
    }
    match compute_one(&job, &mut embedder.as_mut().expect("上面刚置 Some").1) {
        Ok(Some(vec)) => {
            pending.entry((job.note_dir.clone(), tag)).or_default().push(EmbedCacheEntry {
                seq: job.seq,
                start_ms: job.start_ms,
                end_ms: job.end_ms,
                source: job.source,
                vec,
            });
        }
        Ok(None) => {
            // 音频尚未落齐:推迟重试。半截音频算出的向量与离线不一致,宁缺毋滥。
            if tries < MAX_DEFERS {
                deferred.push((job, tries + 1, std::time::Instant::now() + DEFER_RETRY));
            }
        }
        Err(e) => eprintln!("预热嵌入失败(本段放弃,Aing 时再算): {e}"),
    }
}

/// 与 `refine::embed_all` 同口径地算一段:同 offset、同切片算式、同 WAV 解码。
/// `Ok(None)` = 该段音频还没全部落盘(定稿早于写盘是常态),调用方推迟重试。
pub(crate) fn compute_one(
    job: &Job,
    embedder: &mut dyn crate::diar::SpeakerEmbedder,
) -> anyhow::Result<Option<Vec<f32>>> {
    let offset_ms = crate::store::audio::track_offset_ms(&job.note_dir, &job.source);
    let from = job.start_ms.saturating_sub(offset_ms) * 16;
    let want = (job.end_ms.saturating_sub(job.start_ms) as usize) * 16;
    let wav = job.note_dir.join(format!("{}.wav", job.source));
    let pcm = crate::store::transcode::read_wav_f32_slice(&wav, from, want)?;
    if pcm.len() < want {
        return Ok(None); // 段尾音频未落盘,等下一轮
    }
    Ok(Some(embedder.embed(&pcm)?))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 确定性假嵌入器:向量 = [均值, 样本数]。两条路径若切出的 PCM 逐位一致,
    /// 向量必然逐位一致;任何切片口径偏差都会立刻反映在这两个数上。
    struct MeanEmbedder;
    impl crate::diar::SpeakerEmbedder for MeanEmbedder {
        fn embed(&mut self, samples: &[f32]) -> anyhow::Result<Vec<f32>> {
            let sum: f32 = samples.iter().sum();
            Ok(vec![sum / samples.len().max(1) as f32, samples.len() as f32])
        }
    }

    fn write_wav(path: &std::path::Path, samples: &[i16]) {
        let spec = hound::WavSpec {
            channels: 1,
            sample_rate: 16000,
            bits_per_sample: 16,
            sample_format: hound::SampleFormat::Int,
        };
        let mut w = hound::WavWriter::create(path, spec).unwrap();
        for s in samples {
            w.write_sample(*s).unwrap();
        }
        w.finalize().unwrap();
    }

    fn seg(seq: u64, start_ms: u64, end_ms: u64) -> crate::store::SegmentRecord {
        crate::store::SegmentRecord {
            seq,
            source: "mic".into(),
            text: "字".into(),
            start_ms,
            end_ms,
            speaker: None,
            rms: None,
        }
    }

    /// 口径统一的核心保证(issue #164):预热算出的向量与离线 embed_all 对同一段
    /// 算出的**逐位一致**——同 WAV、同 offset、同切片、同解码。
    #[test]
    fn prewarm_vector_equals_embed_all_for_same_segment() {
        let dir = tempfile::tempdir().unwrap();
        // 4 秒音频,幅值随位置变化(切错一个样本均值就变)
        let samples: Vec<i16> = (0..64000).map(|i| (i % 3000) as i16).collect();
        write_wav(&dir.path().join("mic.wav"), &samples);
        let s0 = seg(0, 500, 2500);
        let kept = [&s0];
        let offline = crate::refine::embed_all(dir.path(), &kept, &mut MeanEmbedder, "m")
            .unwrap()
            .remove(0)
            .expect("离线路径应算出向量");
        let job = Job {
            note_dir: dir.path().to_path_buf(),
            seq: 0,
            source: "mic".into(),
            start_ms: 500,
            end_ms: 2500,
        };
        let warm = compute_one(&job, &mut MeanEmbedder).unwrap().expect("音频已全部在盘");
        assert_eq!(warm, offline, "两条路径切出的 PCM 必须逐位一致");
    }

    /// 段尾音频未落盘时不许拿半截算(会与离线不一致):返回 None 供推迟重试。
    #[test]
    fn incomplete_audio_defers_instead_of_embedding_half() {
        let dir = tempfile::tempdir().unwrap();
        write_wav(&dir.path().join("mic.wav"), &vec![100i16; 16000]); // 只有 1s
        let job = Job {
            note_dir: dir.path().to_path_buf(),
            seq: 0,
            source: "mic".into(),
            start_ms: 0,
            end_ms: 2000, // 段声称 2s,盘上只有 1s
        };
        assert!(compute_one(&job, &mut MeanEmbedder).unwrap().is_none());
    }

    /// merge_save:upsert 不重复、换模型整份弃旧。
    #[test]
    fn merge_save_upserts_and_resets_on_model_change() {
        let dir = tempfile::tempdir().unwrap();
        let e = |seq: u64, v: f32| crate::store::embed_cache::EmbedCacheEntry {
            seq,
            start_ms: 0,
            end_ms: 2000,
            source: "mic".into(),
            vec: vec![v],
        };
        crate::store::embed_cache::merge_save(dir.path(), "A", vec![e(0, 1.0), e(1, 2.0)])
            .unwrap();
        // 同键 upsert 覆盖,新键追加
        crate::store::embed_cache::merge_save(dir.path(), "A", vec![e(0, 9.0), e(2, 3.0)])
            .unwrap();
        let c = crate::store::embed_cache::load(dir.path()).unwrap();
        assert_eq!(c.entries.len(), 3);
        assert_eq!(c.entries.iter().find(|x| x.seq == 0).unwrap().vec, vec![9.0]);
        // 换模型:弃旧起新
        crate::store::embed_cache::merge_save(dir.path(), "B", vec![e(7, 5.0)]).unwrap();
        let c = crate::store::embed_cache::load(dir.path()).unwrap();
        assert_eq!(c.model, "B");
        assert_eq!(c.entries.len(), 1);
        assert_eq!(c.entries[0].seq, 7);
    }

    /// read_wav_f32_slice 与整读逐位一致(含越界截尾)。
    #[test]
    fn wav_slice_matches_full_read() {
        let dir = tempfile::tempdir().unwrap();
        let wav = dir.path().join("t.wav");
        let samples: Vec<i16> = (0..1000).map(|i| (i * 7 % 501 - 250) as i16).collect();
        write_wav(&wav, &samples);
        let full = crate::store::transcode::read_wav_f32(&wav).unwrap();
        let mid = crate::store::transcode::read_wav_f32_slice(&wav, 100, 300).unwrap();
        assert_eq!(mid, full[100..400].to_vec());
        let tail = crate::store::transcode::read_wav_f32_slice(&wav, 900, 500).unwrap();
        assert_eq!(tail, full[900..].to_vec(), "越过文件尾按实际截");
    }
}
