use crate::asr::Recognizer;
use crate::audio::{AudioCapture, AudioFrame, Source};
use crate::diar::registry::SpeakerRegistry;
use crate::diar::SpeakerEmbedder;
use crate::pipeline::segment_worker::run_segment_worker;
use crate::pipeline::segmenter::Segmenter;
use crossbeam_channel::Receiver;
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// 完成句识别任务：进 finals 队列，永不丢弃（保证不丢内容）。
#[derive(Debug, Clone)]
pub struct FinalJob {
    pub source: Source,
    pub samples: Vec<f32>,
    /// 相对该源流开始的毫秒（16kHz 样本钟换算）。
    pub start_ms: u64,
    pub end_ms: u64,
}

/// 当前句预览任务：写入每源覆盖式槽，忙时被更新版本覆盖（best-effort）。
#[derive(Debug, Clone)]
pub struct PartialJob {
    pub source: Source,
    pub samples: Vec<f32>,
}

/// diarization 侧事件:说话人表变化 / 簇合并(需回写落盘与 UI)/ worker 结束时的质心快照
/// (仅存入 writer 内存表,不落盘、不 emit,由既有 finalize→persist_speakers 落盘,P4.5 续录铺底)。
#[derive(Debug, Clone)]
pub enum DiarEvent {
    SpeakersChanged(Vec<crate::diar::registry::SpeakerInfo>),
    Merged { loser: String, winner: String },
    Snapshot(Vec<crate::diar::registry::ClusterSnapshot>),
}

/// 单识别 worker：串行消费 finals（不丢、优先），空闲时取每源最新 partial（best-effort）。
/// finals_rx 关闭且排干后返回。识别失败的完成句 emit "[识别失败]" 占位，worker 不退出。
/// 每条 final 定稿时额外提声纹嵌入并归簇（嵌入失败/无 embedder/panic 均降级为 None，绝不影响文本）；
/// 归簇产生的簇合并 / 说话人表变化通过 on_diar 通知（顺序：先 Merged 后 SpeakersChanged）。
pub fn run_asr_worker(
    mut recognizer: Box<dyn Recognizer>,
    mut embedder: Option<Box<dyn SpeakerEmbedder>>,
    finals_rx: Receiver<FinalJob>,
    partial_slots: Vec<(Source, Arc<Mutex<Option<PartialJob>>>)>,
    mut on_final: impl FnMut(Source, String, u64, u64, Option<String>),
    mut on_partial: impl FnMut(Source, String),
    mut on_diar: impl FnMut(DiarEvent),
) -> (Box<dyn Recognizer>, Option<Box<dyn SpeakerEmbedder>>) {
    let mut registry = SpeakerRegistry::new();
    // 与上次发送的完整说话人表比较（非仅 len）：同段内「合并-1+新建+1」净零、
    // 已有簇 sources 增长等变化都能被捕获并同步。
    let mut last_sent: Vec<crate::diar::registry::SpeakerInfo> = Vec::new();
    loop {
        match finals_rx.recv_timeout(Duration::from_millis(100)) {
            Ok(job) => {
                let text = match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    recognizer.recognize(&job.samples)
                })) {
                    Ok(Ok(t)) => t.text,
                    Ok(Err(_)) => "[识别失败]".to_string(),
                    Err(_) => {
                        eprintln!(
                            "run_asr_worker: recognize panicked on a {:?} final; 以占位继续",
                            job.source
                        );
                        "[识别失败]".to_string()
                    }
                };
                // 声纹:嵌入失败/无 embedder → None,绝不影响文本
                let speaker = embedder.as_mut().and_then(|e| {
                    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        e.embed(&job.samples)
                    })) {
                        Ok(Ok(v)) => registry.assign(&v, job.source.as_str(), job.samples.len()),
                        Ok(Err(err)) => {
                            eprintln!("声纹提取失败({:?} 段): {err}", job.source);
                            None
                        }
                        Err(_) => {
                            eprintln!("声纹提取 panic({:?} 段),该段无标签", job.source);
                            None
                        }
                    }
                });
                for (loser, winner) in registry.take_merges() {
                    on_diar(DiarEvent::Merged { loser, winner });
                }
                let speakers = registry.speakers();
                if speakers != last_sent {
                    last_sent = speakers.clone();
                    on_diar(DiarEvent::SpeakersChanged(speakers));
                }
                on_final(job.source, text, job.start_ms, job.end_ms, speaker);
            }
            Err(crossbeam_channel::RecvTimeoutError::Timeout) => {
                // 空闲：服务每源最新 partial（取出即清空，只识别最新一版）。
                for (src, slot) in &partial_slots {
                    let job = slot.lock().unwrap().take();
                    if let Some(job) = job {
                        match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                            recognizer.recognize(&job.samples)
                        })) {
                            Ok(Ok(t)) => on_partial(*src, t.text),
                            Ok(Err(_)) => {}
                            Err(_) => {
                                eprintln!(
                                    "run_asr_worker: recognize panicked on a {:?} partial; 跳过",
                                    src
                                );
                            }
                        }
                    }
                }
            }
            Err(crossbeam_channel::RecvTimeoutError::Disconnected) => {
                on_diar(DiarEvent::Snapshot(registry.snapshot()));
                break;
            }
        }
    }
    (recognizer, embedder)
}

/// 一次录制会话的句柄：持两路 capture + 各 worker 的 join 句柄。
pub struct RecordingHandle {
    captures: Vec<Box<dyn AudioCapture>>,
    workers: Vec<std::thread::JoinHandle<()>>,
    asr: Option<std::thread::JoinHandle<(Box<dyn Recognizer>, Option<Box<dyn SpeakerEmbedder>>)>>,
}

impl RecordingHandle {
    /// 优雅停止：停各 capture（关帧通道）→ 分段 worker flush 尾段后退出并 join
    /// →（其 finals 发送端随之 drop）ASR worker 排干剩余 finals 后退出并 join，
    /// 返还 recognizer / embedder 供复用（asr 线程 panic 时均返 None，调用方现场重载兜底）。
    pub fn stop(mut self) -> (Option<Box<dyn Recognizer>>, Option<Box<dyn SpeakerEmbedder>>) {
        for c in self.captures.iter_mut() {
            c.stop();
        }
        for w in self.workers.drain(..) {
            let _ = w.join();
        }
        match self.asr.take() {
            Some(a) => match a.join() {
                Ok((r, e)) => (Some(r), e),
                Err(_) => {
                    eprintln!("RecordingHandle::stop: asr 线程异常退出（panic），模型不回收");
                    (None, None)
                }
            },
            None => (None, None),
        }
    }
}

/// start_session 的结果：句柄 + 成功启动的源 + 失败的源（含错误串，供降级归类）。
pub struct SessionStart {
    pub handle: RecordingHandle,
    pub active: Vec<Source>,
    pub failed: Vec<(Source, String)>,
}

/// start_session 失败时携带 recognizer / embedder 返还，避免常驻模型在错误路径丢失。
pub struct StartError {
    pub error: anyhow::Error,
    pub recognizer: Box<dyn Recognizer>,
    pub embedder: Option<Box<dyn SpeakerEmbedder>>,
}

impl std::fmt::Debug for StartError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "StartError({})", self.error)
    }
}

/// 起会话：每源一条分段 worker + 单 ASR worker，接好 finals 通道与每源 partial 槽。
/// 某源 capture 启动失败 → 跳过该源并记入 failed（用于降级）；无任何源启动 → Err。
#[allow(clippy::too_many_arguments)]
pub fn start_session(
    sources: Vec<(Source, Box<dyn AudioCapture>, Box<dyn Segmenter>)>,
    recognizer: Box<dyn Recognizer>,
    embedder: Option<Box<dyn SpeakerEmbedder>>,
    target_rate: u32,
    partial_interval_samples: usize,
    on_final: impl FnMut(Source, String, u64, u64, Option<String>) + Send + 'static,
    on_partial: impl FnMut(Source, String) + Send + 'static,
    on_diar: impl FnMut(DiarEvent) + Send + 'static,
) -> Result<SessionStart, StartError> {
    let (finals_tx, finals_rx) = crossbeam_channel::unbounded::<FinalJob>();
    let mut slots: Vec<(Source, Arc<Mutex<Option<PartialJob>>>)> = Vec::new();
    let mut captures: Vec<Box<dyn AudioCapture>> = Vec::new();
    let mut workers: Vec<std::thread::JoinHandle<()>> = Vec::new();
    let mut active: Vec<Source> = Vec::new();
    let mut failed: Vec<(Source, String)> = Vec::new();

    for (source, mut capture, segmenter) in sources {
        let (ftx, frx) = crossbeam_channel::bounded::<AudioFrame>(256);
        let slot = Arc::new(Mutex::new(None));
        let slot_for_worker = slot.clone();
        let final_tx = finals_tx.clone();
        // 先起 worker（消费者），再启动 capture：兼容同步灌帧的 MockCapture，
        // 且若 capture 启动失败，ftx 在 start 内被 drop → frx 关闭 → worker 立即退出。
        let w = std::thread::spawn(move || {
            run_segment_worker(
                source,
                frx,
                target_rate,
                partial_interval_samples,
                final_tx,
                slot_for_worker,
                segmenter,
            );
        });
        match capture.start(ftx) {
            Ok(()) => {
                active.push(source);
                slots.push((source, slot));
                captures.push(capture);
                workers.push(w);
            }
            Err(e) => {
                failed.push((source, e.to_string()));
                let _ = w.join(); // frx 已关闭，worker 已在退出
            }
        }
    }

    drop(finals_tx); // 仅剩各 worker 持有发送端 → 它们结束后 ASR 才断开

    if active.is_empty() {
        return Err(StartError {
            error: anyhow::anyhow!("没有可用音频源可启动: {failed:?}"),
            recognizer,
            embedder,
        });
    }

    let asr = std::thread::spawn(move || {
        run_asr_worker(recognizer, embedder, finals_rx, slots, on_final, on_partial, on_diar)
    });

    Ok(SessionStart {
        handle: RecordingHandle { captures, workers, asr: Some(asr) },
        active,
        failed,
    })
}

#[cfg(test)]
mod asr_worker_tests {
    use super::*;
    use crate::asr::{Recognizer, Transcript};
    use crate::audio::Source;
    use crate::diar::MockEmbedder;
    use std::sync::{Arc, Mutex};

    struct CountingRecognizer;
    impl Recognizer for CountingRecognizer {
        fn recognize(&mut self, s: &[f32]) -> anyhow::Result<Transcript> {
            Ok(Transcript { text: format!("len={}", s.len()) })
        }
    }

    struct FlakyRecognizer { n: usize }
    impl Recognizer for FlakyRecognizer {
        fn recognize(&mut self, s: &[f32]) -> anyhow::Result<Transcript> {
            self.n += 1;
            if self.n == 1 {
                anyhow::bail!("boom");
            }
            Ok(Transcript { text: format!("len={}", s.len()) })
        }
    }

    #[test]
    fn emits_all_finals_tagged_in_order() {
        let (tx, rx) = crossbeam_channel::unbounded::<FinalJob>();
        tx.send(FinalJob { source: Source::Mic, samples: vec![0.0; 10], start_ms: 0, end_ms: 625 }).unwrap();
        tx.send(FinalJob { source: Source::System, samples: vec![0.0; 20], start_ms: 625, end_ms: 1875 }).unwrap();
        drop(tx);

        let finals = Arc::new(Mutex::new(Vec::<(Source, String, u64, u64)>::new()));
        let f2 = finals.clone();
        let _ = run_asr_worker(
            Box::new(CountingRecognizer),
            None,
            rx,
            vec![],
            move |s, t, start_ms, end_ms, _| f2.lock().unwrap().push((s, t, start_ms, end_ms)),
            |_, _| {},
            |_| {},
        );
        assert_eq!(
            *finals.lock().unwrap(),
            vec![
                (Source::Mic, "len=10".into(), 0, 625),
                (Source::System, "len=20".into(), 625, 1875)
            ]
        );
    }

    #[test]
    fn failed_final_becomes_placeholder_and_worker_survives() {
        let (tx, rx) = crossbeam_channel::unbounded::<FinalJob>();
        tx.send(FinalJob { source: Source::Mic, samples: vec![0.0; 3], start_ms: 0, end_ms: 0 }).unwrap();
        tx.send(FinalJob { source: Source::Mic, samples: vec![0.0; 4], start_ms: 0, end_ms: 0 }).unwrap();
        drop(tx);

        let finals = Arc::new(Mutex::new(Vec::<(Source, String)>::new()));
        let f2 = finals.clone();
        let _ = run_asr_worker(
            Box::new(FlakyRecognizer { n: 0 }),
            None,
            rx,
            vec![],
            move |s, t, _, _, _| f2.lock().unwrap().push((s, t)),
            |_, _| {},
            |_| {},
        );
        assert_eq!(
            *finals.lock().unwrap(),
            vec![(Source::Mic, "[识别失败]".into()), (Source::Mic, "len=4".into())]
        );
    }

    struct PanicRecognizer { n: usize }
    impl Recognizer for PanicRecognizer {
        fn recognize(&mut self, s: &[f32]) -> anyhow::Result<Transcript> {
            self.n += 1;
            if self.n == 1 {
                panic!("boom");
            }
            Ok(Transcript { text: format!("len={}", s.len()) })
        }
    }

    #[test]
    fn recognize_panic_becomes_placeholder_worker_survives() {
        let (tx, rx) = crossbeam_channel::unbounded::<FinalJob>();
        tx.send(FinalJob { source: Source::Mic, samples: vec![0.0; 3], start_ms: 0, end_ms: 0 }).unwrap();
        tx.send(FinalJob { source: Source::Mic, samples: vec![0.0; 5], start_ms: 0, end_ms: 0 }).unwrap();
        drop(tx);

        let finals = Arc::new(Mutex::new(Vec::<(Source, String)>::new()));
        let f2 = finals.clone();

        // Suppress "panicked at" output so test stderr stays clean.
        let prev = std::panic::take_hook();
        std::panic::set_hook(Box::new(|_| {}));
        let _ = run_asr_worker(
            Box::new(PanicRecognizer { n: 0 }),
            None,
            rx,
            vec![],
            move |s, t, _, _, _| f2.lock().unwrap().push((s, t)),
            |_, _| {},
            |_| {},
        );
        std::panic::set_hook(prev);

        assert_eq!(
            *finals.lock().unwrap(),
            vec![
                (Source::Mic, "[识别失败]".into()),
                (Source::Mic, "len=5".into()),
            ]
        );
    }

    #[test]
    fn services_latest_partial_when_idle() {
        let (tx, rx) = crossbeam_channel::unbounded::<FinalJob>();
        let slot = Arc::new(Mutex::new(Some(PartialJob { source: Source::System, samples: vec![0.0; 7] })));
        let partials = Arc::new(Mutex::new(Vec::<(Source, String)>::new()));
        let p2 = partials.clone();
        let slot_for_worker = slot.clone();

        let worker = std::thread::spawn(move || {
            let _ = run_asr_worker(
                Box::new(CountingRecognizer),
                None,
                rx,
                vec![(Source::System, slot_for_worker)],
                |_, _, _, _, _| {},
                move |s, t| p2.lock().unwrap().push((s, t)),
                |_| {},
            );
        });

        // 轮询等待 worker 在空闲分支服务了 partial 槽（有界，避免固定 sleep 假设）。
        let mut serviced = false;
        for _ in 0..200 {
            if !partials.lock().unwrap().is_empty() {
                serviced = true;
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        drop(tx); // 结束 worker
        worker.join().unwrap();

        assert!(serviced, "空闲时应服务 partial 槽");
        assert_eq!(*partials.lock().unwrap(), vec![(Source::System, "len=7".into())]);
        assert!(slot.lock().unwrap().is_none(), "partial 取出后槽应清空");
    }

    #[test]
    fn finals_get_speaker_labels_and_diar_events() {
        let (tx, rx) = crossbeam_channel::unbounded::<FinalJob>();
        // 两段长音频:第一段 → S1;第二段正交向量 → S2
        tx.send(FinalJob { source: Source::Mic, samples: vec![0.1; 32000], start_ms: 0, end_ms: 2000 }).unwrap();
        tx.send(FinalJob { source: Source::System, samples: vec![0.1; 32000], start_ms: 2000, end_ms: 4000 }).unwrap();
        drop(tx);

        let embedder = MockEmbedder::new(vec![
            Ok(vec![1.0, 0.0, 0.0]),
            Ok(vec![0.0, 1.0, 0.0]),
        ]);
        let finals = Arc::new(Mutex::new(Vec::<Option<String>>::new()));
        let diar_events = Arc::new(Mutex::new(0usize));
        let (f2, d2) = (finals.clone(), diar_events.clone());
        let (_r, e) = run_asr_worker(
            Box::new(CountingRecognizer),
            Some(Box::new(embedder)),
            rx,
            vec![],
            move |_, _, _, _, spk| f2.lock().unwrap().push(spk),
            |_, _| {},
            move |_ev| *d2.lock().unwrap() += 1,
        );
        assert!(e.is_some(), "embedder 应返还");
        assert_eq!(
            *finals.lock().unwrap(),
            vec![Some("S1".into()), Some("S2".into())]
        );
        assert!(*diar_events.lock().unwrap() >= 2, "每个新说话人应发 SpeakersChanged");
    }

    #[test]
    fn same_speaker_growing_sources_reemits_speakers() {
        let (tx, rx) = crossbeam_channel::unbounded::<FinalJob>();
        // 同一说话人两段，不同 source（两次同向量 → 都归入 S1，sources 从 {mic} 增长到 {mic,system}）。
        tx.send(FinalJob { source: Source::Mic, samples: vec![0.1; 32000], start_ms: 0, end_ms: 2000 }).unwrap();
        tx.send(FinalJob { source: Source::System, samples: vec![0.1; 32000], start_ms: 2000, end_ms: 4000 }).unwrap();
        drop(tx);

        let embedder = MockEmbedder::new(vec![
            Ok(vec![1.0, 0.0, 0.0]),
            Ok(vec![1.0, 0.0, 0.0]),
        ]);
        let finals = Arc::new(Mutex::new(Vec::<Option<String>>::new()));
        let diar_events = Arc::new(Mutex::new(0usize));
        let (f2, d2) = (finals.clone(), diar_events.clone());
        let _ = run_asr_worker(
            Box::new(CountingRecognizer),
            Some(Box::new(embedder)),
            rx,
            vec![],
            move |_, _, _, _, spk| f2.lock().unwrap().push(spk),
            |_, _| {},
            move |_ev| *d2.lock().unwrap() += 1,
        );
        assert_eq!(
            *finals.lock().unwrap(),
            vec![Some("S1".into()), Some("S1".into())],
            "两段同说话人"
        );
        assert!(
            *diar_events.lock().unwrap() >= 2,
            "sources 增长应再发一次 SpeakersChanged（全量比较，非仅 len）"
        );
    }

    #[test]
    fn embed_failure_degrades_to_null_speaker() {
        let (tx, rx) = crossbeam_channel::unbounded::<FinalJob>();
        tx.send(FinalJob { source: Source::Mic, samples: vec![0.1; 32000], start_ms: 0, end_ms: 2000 }).unwrap();
        drop(tx);
        let embedder = MockEmbedder::new(vec![Err(anyhow::anyhow!("boom"))]);
        let finals = Arc::new(Mutex::new(Vec::<Option<String>>::new()));
        let f2 = finals.clone();
        let _ = run_asr_worker(
            Box::new(CountingRecognizer),
            Some(Box::new(embedder)),
            rx,
            vec![],
            move |_, _, _, _, spk| f2.lock().unwrap().push(spk),
            |_, _| {},
            |_| {},
        );
        assert_eq!(*finals.lock().unwrap(), vec![None], "嵌入失败段 speaker 为 null,不影响文本");
    }

    #[test]
    fn no_embedder_all_speakers_null() {
        let (tx, rx) = crossbeam_channel::unbounded::<FinalJob>();
        tx.send(FinalJob { source: Source::Mic, samples: vec![0.1; 32000], start_ms: 0, end_ms: 2000 }).unwrap();
        drop(tx);
        let finals = Arc::new(Mutex::new(Vec::<Option<String>>::new()));
        let f2 = finals.clone();
        let (_r, e) = run_asr_worker(
            Box::new(CountingRecognizer),
            None,
            rx,
            vec![],
            move |_, _, _, _, spk| f2.lock().unwrap().push(spk),
            |_, _| {},
            |_| {},
        );
        assert!(e.is_none());
        assert_eq!(*finals.lock().unwrap(), vec![None]);
    }

    #[test]
    fn worker_emits_snapshot_exactly_once_at_end_after_other_diar_events() {
        let (tx, rx) = crossbeam_channel::unbounded::<FinalJob>();
        tx.send(FinalJob { source: Source::Mic, samples: vec![0.1; 32000], start_ms: 0, end_ms: 2000 }).unwrap();
        drop(tx);

        let embedder = MockEmbedder::new(vec![Ok(vec![1.0, 0.0, 0.0])]);
        let events = Arc::new(Mutex::new(Vec::<DiarEvent>::new()));
        let e2 = events.clone();
        let _ = run_asr_worker(
            Box::new(CountingRecognizer),
            Some(Box::new(embedder)),
            rx,
            vec![],
            |_, _, _, _, _| {},
            |_, _| {},
            move |ev| e2.lock().unwrap().push(ev),
        );
        let evs = events.lock().unwrap();
        let snapshot_count = evs.iter().filter(|e| matches!(e, DiarEvent::Snapshot(_))).count();
        assert_eq!(snapshot_count, 1, "worker 结束时应恰发一次 Snapshot");
        assert!(matches!(evs.last().unwrap(), DiarEvent::Snapshot(_)), "Snapshot 应在末尾(既有 diar 事件之后)");
        match evs.last().unwrap() {
            DiarEvent::Snapshot(snaps) => {
                assert_eq!(snaps.len(), 1);
                assert_eq!(snaps[0].id, "S1");
            }
            _ => unreachable!(),
        }
    }
}

#[cfg(test)]
mod session_tests {
    use super::*;
    use crate::asr::{Recognizer, Transcript};
    use crate::audio::mock::MockCapture;
    use crate::audio::{AudioCapture, AudioFrame, Source};
    use crate::pipeline::segmenter::MockSegmenter;
    use crossbeam_channel::Sender;
    use std::sync::{Arc, Mutex};

    struct CountingRecognizer;
    impl Recognizer for CountingRecognizer {
        fn recognize(&mut self, s: &[f32]) -> anyhow::Result<Transcript> {
            Ok(Transcript { text: format!("len={}", s.len()) })
        }
    }

    /// 发完 fixture 帧后保持通道开启，直到 stop() 被调用——用于测真停止与运行中的会话。
    struct IdlingCapture {
        frames: Vec<AudioFrame>,
        stop_tx: Option<Sender<()>>,
    }
    impl IdlingCapture {
        fn from_fixture() -> Self {
            let mut cap = MockCapture::from_wav(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/tests/fixtures/sample_16k.wav"
            ))
            .expect("fixture");
            // 借 MockCapture 的分帧：把它的帧抽出来（通过一次性 start 到本地通道）。
            let (tx, rx) = crossbeam_channel::unbounded::<AudioFrame>();
            cap.start(tx).unwrap();
            Self { frames: rx.try_iter().collect(), stop_tx: None }
        }
    }
    impl AudioCapture for IdlingCapture {
        fn start(&mut self, sink: Sender<AudioFrame>) -> anyhow::Result<()> {
            let frames = std::mem::take(&mut self.frames);
            let (stx, srx) = crossbeam_channel::bounded::<()>(0);
            self.stop_tx = Some(stx);
            std::thread::spawn(move || {
                for f in frames {
                    let _ = sink.send(f);
                }
                srx.recv().ok(); // 阻塞直到 stop() drop 掉 stx
                // sink 在此 drop → 分段 worker 的 frame_rx 关闭 → flush 退出
            });
            Ok(())
        }
        fn stop(&mut self) {
            self.stop_tx = None;
        }
    }

    #[test]
    fn merges_two_sources_and_stops_cleanly() {
        let finals = Arc::new(Mutex::new(Vec::<(Source, String)>::new()));
        let f2 = finals.clone();

        let sources: Vec<(Source, Box<dyn AudioCapture>, Box<dyn Segmenter>)> = vec![
            (Source::Mic, Box::new(IdlingCapture::from_fixture()), Box::new(MockSegmenter::new(2000))),
            (Source::System, Box::new(IdlingCapture::from_fixture()), Box::new(MockSegmenter::new(2000))),
        ];

        let start = start_session(
            sources,
            Box::new(CountingRecognizer),
            None,
            16000,
            4000,
            move |s, t, _, _, _| f2.lock().unwrap().push((s, t)),
            |_, _| {},
            |_| {},
        )
        .expect("start_session");

        assert_eq!(start.active.len(), 2, "两源都应启动");
        assert!(start.failed.is_empty());

        // 等待两源都产出至少一个 final（有界轮询）。
        let mut ok = false;
        for _ in 0..300 {
            let g = finals.lock().unwrap();
            let has_mic = g.iter().any(|(s, _)| *s == Source::Mic);
            let has_sys = g.iter().any(|(s, _)| *s == Source::System);
            if has_mic && has_sys {
                ok = true;
                break;
            }
            drop(g);
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        let _ = start.handle.stop(); // 真停止：停 capture → join workers → join asr
        assert!(ok, "两源都应产出带标记的 final");
    }

    #[test]
    fn stop_returns_recognizer_for_reuse() {
        let sources: Vec<(Source, Box<dyn AudioCapture>, Box<dyn Segmenter>)> = vec![(
            Source::Mic,
            Box::new(IdlingCapture::from_fixture()),
            Box::new(MockSegmenter::new(2000)),
        )];
        let start = start_session(
            sources,
            Box::new(CountingRecognizer),
            None,
            16000,
            4000,
            |_, _, _, _, _| {},
            |_, _| {},
            |_| {},
        )
        .expect("start_session");
        let (r, _e) = start.handle.stop();
        assert!(r.is_some(), "停止后应返还 recognizer 供复用");
    }

    #[test]
    fn all_sources_fail_returns_recognizer_in_err() {
        struct FailingCapture;
        impl AudioCapture for FailingCapture {
            fn start(&mut self, _sink: Sender<AudioFrame>) -> anyhow::Result<()> {
                anyhow::bail!("unauthorized: nope")
            }
            fn stop(&mut self) {}
        }
        let sources: Vec<(Source, Box<dyn AudioCapture>, Box<dyn Segmenter>)> =
            vec![(Source::System, Box::new(FailingCapture), Box::new(MockSegmenter::new(8000)))];
        let r = start_session(
            sources,
            Box::new(CountingRecognizer),
            None,
            16000,
            4000,
            |_, _, _, _, _| {},
            |_, _| {},
            |_| {},
        );
        let err = match r {
            Ok(_) => panic!("无源可启动应返回 Err"),
            Err(e) => e,
        };
        assert!(err.error.to_string().contains("没有可用音频源"));
        let _reusable: Box<dyn Recognizer> = err.recognizer; // Err 携带 recognizer 返还
    }
}
