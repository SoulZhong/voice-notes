use crate::asr::Recognizer;
use crate::audio::{AudioCapture, AudioFrame, Source};
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

/// 单识别 worker：串行消费 finals（不丢、优先），空闲时取每源最新 partial（best-effort）。
/// finals_rx 关闭且排干后返回。识别失败的完成句 emit "[识别失败]" 占位，worker 不退出。
pub fn run_asr_worker(
    mut recognizer: Box<dyn Recognizer>,
    finals_rx: Receiver<FinalJob>,
    partial_slots: Vec<(Source, Arc<Mutex<Option<PartialJob>>>)>,
    mut on_final: impl FnMut(Source, String, u64, u64),
    mut on_partial: impl FnMut(Source, String),
) -> Box<dyn Recognizer> {
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
                on_final(job.source, text, job.start_ms, job.end_ms);
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
            Err(crossbeam_channel::RecvTimeoutError::Disconnected) => break,
        }
    }
    recognizer
}

/// 一次录制会话的句柄：持两路 capture + 各 worker 的 join 句柄。
pub struct RecordingHandle {
    captures: Vec<Box<dyn AudioCapture>>,
    workers: Vec<std::thread::JoinHandle<()>>,
    asr: Option<std::thread::JoinHandle<Box<dyn Recognizer>>>,
}

impl RecordingHandle {
    /// 优雅停止：停各 capture（关帧通道）→ 分段 worker flush 尾段后退出并 join
    /// →（其 finals 发送端随之 drop）ASR worker 排干剩余 finals 后退出并 join，
    /// 返还 recognizer 供复用（asr 线程 panic 时返 None，调用方现场重载兜底）。
    pub fn stop(mut self) -> Option<Box<dyn Recognizer>> {
        for c in self.captures.iter_mut() {
            c.stop();
        }
        for w in self.workers.drain(..) {
            let _ = w.join();
        }
        match self.asr.take() {
            Some(a) => match a.join() {
                Ok(r) => Some(r),
                Err(_) => {
                    eprintln!("RecordingHandle::stop: asr 线程异常退出（panic），识别器不回收");
                    None
                }
            },
            None => None,
        }
    }
}

/// start_session 的结果：句柄 + 成功启动的源 + 失败的源（含错误串，供降级归类）。
pub struct SessionStart {
    pub handle: RecordingHandle,
    pub active: Vec<Source>,
    pub failed: Vec<(Source, String)>,
}

/// start_session 失败时携带 recognizer 返还，避免常驻识别器在错误路径丢失。
pub struct StartError {
    pub error: anyhow::Error,
    pub recognizer: Box<dyn Recognizer>,
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
    target_rate: u32,
    partial_interval_samples: usize,
    on_final: impl FnMut(Source, String, u64, u64) + Send + 'static,
    on_partial: impl FnMut(Source, String) + Send + 'static,
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
        });
    }

    let asr = std::thread::spawn(move || {
        run_asr_worker(recognizer, finals_rx, slots, on_final, on_partial)
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
            rx,
            vec![],
            move |s, t, start_ms, end_ms| f2.lock().unwrap().push((s, t, start_ms, end_ms)),
            |_, _| {},
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
            rx,
            vec![],
            move |s, t, _, _| f2.lock().unwrap().push((s, t)),
            |_, _| {},
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
            rx,
            vec![],
            move |s, t, _, _| f2.lock().unwrap().push((s, t)),
            |_, _| {},
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
                rx,
                vec![(Source::System, slot_for_worker)],
                |_, _, _, _| {},
                move |s, t| p2.lock().unwrap().push((s, t)),
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
            16000,
            4000,
            move |s, t, _, _| f2.lock().unwrap().push((s, t)),
            |_, _| {},
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
        let start = start_session(sources, Box::new(CountingRecognizer), 16000, 4000, |_, _, _, _| {}, |_, _| {})
            .expect("start_session");
        let returned = start.handle.stop();
        assert!(returned.is_some(), "停止后应返还 recognizer 供复用");
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
        let r = start_session(sources, Box::new(CountingRecognizer), 16000, 4000, |_, _, _, _| {}, |_, _| {});
        let err = match r {
            Ok(_) => panic!("无源可启动应返回 Err"),
            Err(e) => e,
        };
        assert!(err.error.to_string().contains("没有可用音频源"));
        let _reusable: Box<dyn Recognizer> = err.recognizer; // Err 携带 recognizer 返还
    }
}
