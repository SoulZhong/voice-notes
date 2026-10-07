//! 录音时的实时字幕(流式识别)。
//!
//! 旧做法:每 1s 把「当前这句到目前为止的全部音频」整段交给本机识别器重识别一遍——
//! 句子越长重复越多,实测 22.7s 语音单源 CPU 达实时的 52%(两路即约一个核,
//! 2026-10-07)。现在说话期间把音频一路喂给流式 zipformer,每段音频只算一次,
//! 同一段 CPU 降到实时的 10%。每句的定稿仍由用户选的识别器出,笔记准确率不变;
//! 流式结果只是界面上的预览。
//!
//! 一条字幕线程服务所有源(一份模型、逐源一条流),与采集线程之间是有界通道:
//! 满了就丢块(字幕晚一点/缺几个字),绝不反压采集。
//! 出来的文字写进该源的 partial 槽,由 ASR worker 空闲时照旧走 push_partial
//! (预览级回声抑制等不变)。
//!
//! 句界与过期:分段 worker 每定稿一句,持 partial 槽锁把该源的 epoch +1 并清槽,
//! 再发 Reset。字幕线程只在 epoch 仍等于自己这条流的 epoch 时写槽——已定稿句子的
//! 迟到预览不会盖掉新句。

use crate::asr::streaming::{OnlineEngine, OnlineStream};
use crate::audio::Source;
use crate::session::PartialJob;
use crossbeam_channel::{Receiver, Sender, TrySendError};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

/// 字幕线程的解码线程数:只是预览,不和定稿识别抢核。
const THREADS: i32 = 2;
/// 每句开头垫的静音(0.3s @16kHz)。
const LEAD_SILENCE: usize = 4800;
/// 采集侧与字幕线程之间最多积压的块数(一块即分段 worker 的一帧,约 10-20ms)。
const QUEUE: usize = 256;

pub enum CaptionMsg {
    /// 某源一块说话中的音频(16kHz 单声道,与分段器同源)。
    Audio(Source, Vec<f32>),
    /// 某源一句定稿了:换新流,只接受 epoch 不小于此值的结果。
    Reset(Source, u64),
}

/// 分段 worker 手里的那一端。
#[derive(Clone)]
pub struct CaptionFeed {
    tx: Sender<CaptionMsg>,
    /// 本源当前句的编号;分段 worker 定稿时持槽锁 +1。
    pub epoch: Arc<AtomicU64>,
}

impl CaptionFeed {
    pub fn audio(&self, source: Source, samples: &[f32]) {
        match self.tx.try_send(CaptionMsg::Audio(source, samples.to_vec())) {
            Ok(()) | Err(TrySendError::Disconnected(_)) => {}
            Err(TrySendError::Full(_)) => {} // 字幕跟不上就丢:预览可以缺字,采集不能等
        }
    }

    /// 一句定稿后调用(已在槽锁内把 epoch +1)。Reset 必须送达,否则下一句会接在
    /// 上一句的流后面,故用阻塞 send(队列满时等字幕线程腾出位置,极少发生)。
    pub fn reset(&self, source: Source) {
        let _ = self.tx.send(CaptionMsg::Reset(source, self.epoch.load(Ordering::SeqCst)));
    }
}

/// spawn 的产物:每源的喂料端、字幕线程句柄、partial 槽登记处。
pub type Spawned = (Vec<(Source, CaptionFeed)>, std::thread::JoinHandle<()>, CaptionSlots);

/// 起字幕线程。`slots`:每源的 partial 槽。模型加载失败返回 None(调用方退回旧做法)。
/// 线程在所有 CaptionFeed 都被丢弃(各分段 worker 结束)后退出并释放模型。
pub fn spawn(
    stream_dir: PathBuf,
    sources: &[Source],
) -> Option<Spawned> {
    let engine = match OnlineEngine::new(&stream_dir, THREADS) {
        Ok(e) => e,
        Err(e) => {
            eprintln!("实时字幕: 流式模型加载失败,退回整句重识别: {e:#}");
            return None;
        }
    };
    let (tx, rx) = crossbeam_channel::bounded::<CaptionMsg>(QUEUE);
    let slots: CaptionSlots = Arc::new(Mutex::new(Vec::new()));
    let feeds: Vec<(Source, CaptionFeed)> = sources
        .iter()
        .map(|s| (*s, CaptionFeed { tx: tx.clone(), epoch: Arc::new(AtomicU64::new(0)) }))
        .collect();
    drop(tx);
    let epochs: Vec<(Source, Arc<AtomicU64>)> = feeds.iter().map(|(s, f)| (*s, f.epoch.clone())).collect();
    let slots_w = slots.clone();
    let handle = std::thread::Builder::new()
        .name("live-caption".into())
        .spawn(move || run(engine, rx, epochs, slots_w))
        .ok()?;
    Some((feeds, handle, slots))
}

/// 测试用:一端 CaptionFeed、一端直接看发出的消息。
#[cfg(test)]
pub fn test_feed() -> (CaptionFeed, Receiver<CaptionMsg>) {
    let (tx, rx) = crossbeam_channel::bounded(QUEUE);
    (CaptionFeed { tx, epoch: Arc::new(AtomicU64::new(0)) }, rx)
}

/// 每源 partial 槽的登记处:session 在槽建好后登记进来(字幕线程先于槽启动)。
pub type CaptionSlots = Arc<Mutex<Vec<(Source, Arc<Mutex<Option<PartialJob>>>)>>>;

struct Lane {
    source: Source,
    stream: Option<OnlineStream>,
    /// 这条流属于第几句。
    epoch: u64,
    last: String,
}

fn run(
    engine: OnlineEngine,
    rx: Receiver<CaptionMsg>,
    epochs: Vec<(Source, Arc<AtomicU64>)>,
    slots: CaptionSlots,
) {
    let mut lanes: Vec<Lane> = Vec::new();
    for msg in rx {
        let source = match &msg {
            CaptionMsg::Audio(s, _) | CaptionMsg::Reset(s, _) => *s,
        };
        let i = match lanes.iter().position(|l| l.source == source) {
            Some(i) => i,
            None => {
                lanes.push(Lane { source, stream: None, epoch: 0, last: String::new() });
                lanes.len() - 1
            }
        };
        let lane = &mut lanes[i];
        match msg {
            CaptionMsg::Reset(_, epoch) => {
                lane.stream = None;
                lane.epoch = epoch;
                lane.last.clear();
            }
            CaptionMsg::Audio(_, samples) => {
                if lane.stream.is_none() {
                    match engine.stream() {
                        Ok(mut s) => {
                            // 新句先垫一小段静音:流式模型开口即来的语音会吞掉开头几个字
                            // (实测「今天的会议」只出「会议」)。
                            let _ = s.feed(&[0.0; LEAD_SILENCE], false);
                            lane.stream = Some(s);
                        }
                        Err(e) => {
                            eprintln!("实时字幕: {e:#}");
                            continue;
                        }
                    }
                }
                let Some(stream) = lane.stream.as_mut() else { continue };
                let text = match stream.feed(&samples, false) {
                    Ok(t) => t,
                    Err(e) => {
                        eprintln!("实时字幕: 解码失败,本句不再出预览: {e:#}");
                        lane.stream = None;
                        continue;
                    }
                };
                if text.is_empty() || text == lane.last {
                    continue;
                }
                lane.last = text.clone();
                publish(&slots, &epochs, source, lane.epoch, text);
            }
        }
    }
}

/// 写该源的 partial 槽;该句已定稿(epoch 已前进)则丢弃。比较与写入在槽锁内完成,
/// 与分段 worker「持槽锁清槽并 +1」互斥。
fn publish(
    slots: &CaptionSlots,
    epochs: &[(Source, Arc<AtomicU64>)],
    source: Source,
    epoch: u64,
    text: String,
) {
    let slot = slots.lock().unwrap().iter().find(|(s, _)| *s == source).map(|(_, s)| s.clone());
    let current = epochs.iter().find(|(s, _)| *s == source).map(|(_, e)| e.clone());
    let (Some(slot), Some(current)) = (slot, current) else { return };
    let mut g = slot.lock().unwrap();
    if current.load(Ordering::SeqCst) == epoch {
        *g = Some(PartialJob { source, samples: Vec::new(), text: Some(text) });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_late_caption_for_a_finished_sentence_is_dropped() {
        let slot: Arc<Mutex<Option<PartialJob>>> = Arc::new(Mutex::new(None));
        let slots: CaptionSlots = Arc::new(Mutex::new(vec![(Source::Mic, slot.clone())]));
        let epoch = Arc::new(AtomicU64::new(0));
        let epochs = vec![(Source::Mic, epoch.clone())];
        publish(&slots, &epochs, Source::Mic, 0, "把这个".into());
        assert_eq!(slot.lock().unwrap().as_ref().unwrap().text.as_deref(), Some("把这个"));
        // 句子定稿:分段 worker 清槽、句号 +1。
        *slot.lock().unwrap() = None;
        epoch.fetch_add(1, Ordering::SeqCst);
        // 上一句的迟到预览:丢弃。
        publish(&slots, &epochs, Source::Mic, 0, "把这个函数".into());
        assert!(slot.lock().unwrap().is_none());
        // 新一句的预览照常写。
        publish(&slots, &epochs, Source::Mic, 1, "然后".into());
        assert_eq!(slot.lock().unwrap().as_ref().unwrap().text.as_deref(), Some("然后"));
    }
}
