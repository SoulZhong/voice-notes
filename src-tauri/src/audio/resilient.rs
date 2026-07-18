//! 采集断连自愈(借鉴 meetily device_monitor 的问题意识,按本仓惯用法重设计)。
//!
//! 问题:cpal 流错误(设备拔出/蓝牙断电/被系统回收)此前只落一行日志,采集
//! 线程不退出、帧通道不关闭、分段 worker 永远等帧——**单路静默丢失**,用户录完
//! 才发现半场会议只有一路。会议场景里"AirPods 没电了"是常态不是异常。
//!
//! 设计:ResilientCapture 包装真实采集,持 sink 的克隆;监控线程吃两路互补信号:
//!  1. `CaptureEvent::Error`(cpal 系后端的流错误回调,快路径);
//!  2. 外部 kick(FrameTap 帧荒失联通知,慢路径——覆盖 VPIO/SCK 等未接错误
//!     回调的后端,它们的运行期死亡表现就是"不再出帧")。
//! 触发后:停内层 → 有界退避重试(工厂重建采集,复用同一 sink 克隆)→ 成功则
//! 上报恢复(重启计数由装配层回调递增),一轮耗尽则上报放弃。worker/时间轴全程
//! 无感:断流窗口 FrameTap 在按墙钟补零,恢复后帧续上,双轨对齐不断裂。
//!
//! 层叠:TappedCapture(ResilientCapture(真实采集))。首次 start 失败仍同步返回
//! Err——启动期降级归类(unauthorized/unavailable)语义不变,自愈只管运行期。

use super::{AudioCapture, AudioFrame, CaptureEvent};
use crossbeam_channel::{Receiver, Sender};
use std::time::Duration;

/// 工厂:每次(重)建一个采集实例 + 其事件接收端(无事件后端给个空通道即可)。
pub type CaptureFactory =
    Box<dyn FnMut() -> (Box<dyn AudioCapture>, Receiver<CaptureEvent>) + Send>;

/// 自愈结局回调(装配层注入:发 ipc 事件/递增健康计数/落日志)。
pub struct ResilientNotify {
    /// 一次重启成功(每成功一次调一次)。
    pub on_recovered: Option<Box<dyn Fn() + Send>>,
    /// 一轮重试耗尽,该源放弃(本场不再尝试;tap 继续补零维持时间轴)。
    pub on_lost: Option<Box<dyn Fn() + Send>>,
}

impl ResilientNotify {
    pub fn none() -> Self {
        Self { on_recovered: None, on_lost: None }
    }
}

pub struct ResilientCapture {
    factory: Option<CaptureFactory>,
    notify: Option<ResilientNotify>,
    /// 一轮重试的退避表(默认 1s/2s/4s;测试注入毫秒级)。成功后计数清零,
    /// 下次断连重新从头一轮——"每次故障最多打扰设备 3 次"而非全场共 3 次。
    backoff: Vec<Duration>,
    /// 外部 kick 的发送端(FrameTap on_stall 持有克隆);start 前创建,句柄可先取。
    kick_tx: Sender<()>,
    kick_rx: Option<Receiver<()>>,
    /// drop = 通知监控线程停止(本仓 stop-channel 惯用法)。
    ctrl_tx: Option<Sender<()>>,
    monitor: Option<std::thread::JoinHandle<()>>,
}

impl ResilientCapture {
    pub fn new(factory: CaptureFactory, notify: ResilientNotify) -> Self {
        Self::with_backoff(
            factory,
            notify,
            vec![Duration::from_secs(1), Duration::from_secs(2), Duration::from_secs(4)],
        )
    }

    pub fn with_backoff(
        factory: CaptureFactory,
        notify: ResilientNotify,
        backoff: Vec<Duration>,
    ) -> Self {
        // kick 用容量 1 的通道:重试进行中再多的 kick 也只留一个待处理,天然去抖。
        let (kick_tx, kick_rx) = crossbeam_channel::bounded::<()>(1);
        Self {
            factory: Some(factory),
            notify: Some(notify),
            backoff,
            kick_tx,
            kick_rx: Some(kick_rx),
            ctrl_tx: None,
            monitor: None,
        }
    }

    /// 外部失联踢一脚的句柄(FrameTap on_stall 用)。满了就丢——说明已有一脚在处理。
    pub fn kicker(&self) -> Sender<()> {
        self.kick_tx.clone()
    }
}

impl AudioCapture for ResilientCapture {
    fn start(&mut self, sink: Sender<AudioFrame>) -> anyhow::Result<()> {
        let mut factory = self
            .factory
            .take()
            .ok_or_else(|| anyhow::anyhow!("ResilientCapture 不支持重复 start"))?;
        let notify = self.notify.take().unwrap_or_else(ResilientNotify::none);
        let kick_rx = self
            .kick_rx
            .take()
            .ok_or_else(|| anyhow::anyhow!("ResilientCapture 不支持重复 start"))?;

        // 首启同步:失败直接冒泡,启动期降级归类语义(unauthorized/unavailable)不变。
        let (mut inner, mut events_rx) = factory();
        inner.start(sink.clone())?;

        let (ctrl_tx, ctrl_rx) = crossbeam_channel::bounded::<()>(0);
        let backoff = self.backoff.clone();
        let monitor = std::thread::spawn(move || {
            loop {
                crossbeam_channel::select! {
                    recv(ctrl_rx) -> _ => {
                        // stop() drop 发送端(或显式发)→ 停内层退出。
                        inner.stop();
                        return;
                    }
                    recv(events_rx) -> ev => {
                        match ev {
                            Ok(CaptureEvent::Error(msg)) => {
                                eprintln!("采集流错误,进入自愈重试: {msg}");
                            }
                            // 事件通道关闭 = 内层已被替换/停止,非故障信号:
                            // 换一个永不来消息的通道占位,避免 select 空转。
                            Err(_) => {
                                events_rx = crossbeam_channel::never();
                                continue;
                            }
                        }
                        if !attempt_restart(
                            &mut factory, &mut inner, &mut events_rx,
                            &sink, &backoff, &ctrl_rx, &notify,
                        ) {
                            // 放弃或停录:若是放弃,守在 ctrl 上等会话拆除。
                            ctrl_rx.recv().ok();
                            inner.stop();
                            return;
                        }
                    }
                    recv(kick_rx) -> k => {
                        if k.is_err() {
                            // kick 发送端全部消失(理论不可达:self 持有一份)——忽略。
                            continue;
                        }
                        eprintln!("采集帧荒失联,进入自愈重试(FrameTap 触发)");
                        if !attempt_restart(
                            &mut factory, &mut inner, &mut events_rx,
                            &sink, &backoff, &ctrl_rx, &notify,
                        ) {
                            ctrl_rx.recv().ok();
                            inner.stop();
                            return;
                        }
                    }
                }
            }
        });

        self.ctrl_tx = Some(ctrl_tx);
        self.monitor = Some(monitor);
        Ok(())
    }

    fn stop(&mut self) {
        // drop ctrl 发送端 → 监控线程停内层并退出;join 保证设备释放后才返回
        // (退避等待用 ctrl 超时轮询,停录最多等一个轮询步长,不久等)。
        self.ctrl_tx = None;
        if let Some(m) = self.monitor.take() {
            let _ = m.join();
        }
    }
}

/// 一轮有界退避重试。返回 true = 恢复成功(inner/events_rx 已替换);
/// false = 停录信号到达或一轮耗尽(耗尽时已调 on_lost)。
fn attempt_restart(
    factory: &mut CaptureFactory,
    inner: &mut Box<dyn AudioCapture>,
    events_rx: &mut Receiver<CaptureEvent>,
    sink: &Sender<AudioFrame>,
    backoff: &[Duration],
    ctrl_rx: &Receiver<()>,
    notify: &ResilientNotify,
) -> bool {
    inner.stop();
    for delay in backoff {
        // 退避等待兼听停录:停录信号(发或断开)到达就立刻放弃,不拖停止流程。
        match ctrl_rx.recv_timeout(*delay) {
            Ok(_) | Err(crossbeam_channel::RecvTimeoutError::Disconnected) => return false,
            Err(crossbeam_channel::RecvTimeoutError::Timeout) => {}
        }
        let (mut candidate, rx) = factory();
        match candidate.start(sink.clone()) {
            Ok(()) => {
                *inner = candidate;
                *events_rx = rx;
                eprintln!("采集自愈成功,恢复出帧");
                if let Some(cb) = &notify.on_recovered {
                    cb();
                }
                return true;
            }
            Err(e) => {
                eprintln!("采集重启失败(退避 {delay:?} 后重试): {e}");
            }
        }
    }
    eprintln!("采集自愈一轮耗尽,本场放弃该源(时间轴由静音填充维持)");
    if let Some(cb) = &notify.on_lost {
        cb();
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::sync::Arc;

    /// 脚本化采集:start 时同步发 N 帧;可选在 start 后触发一次流错误事件。
    struct Scripted {
        frames: usize,
        fail_start: bool,
        error_after_start: bool,
        events_tx: Sender<CaptureEvent>,
    }
    impl AudioCapture for Scripted {
        fn start(&mut self, sink: Sender<AudioFrame>) -> anyhow::Result<()> {
            if self.fail_start {
                anyhow::bail!("scripted start failure");
            }
            for _ in 0..self.frames {
                let _ = sink.send(AudioFrame {
                    samples: vec![0.1; 160],
                    sample_rate: 16000,
                    channels: 1,
                });
            }
            if self.error_after_start {
                let _ = self.events_tx.send(CaptureEvent::Error("device gone".into()));
            }
            Ok(())
        }
        fn stop(&mut self) {}
    }

    fn fast_backoff() -> Vec<Duration> {
        vec![Duration::from_millis(10), Duration::from_millis(20)]
    }

    /// 第一实例出错 → 自动重建第二实例,帧续到同一 sink;恢复回调恰一次。
    #[test]
    fn restarts_on_error_and_keeps_sink() {
        let built = Arc::new(AtomicU32::new(0));
        let recovered = Arc::new(AtomicU32::new(0));
        let b2 = built.clone();
        let factory: CaptureFactory = Box::new(move || {
            let n = b2.fetch_add(1, Ordering::SeqCst);
            let (etx, erx) = crossbeam_channel::unbounded();
            let cap = Scripted {
                frames: 2,
                fail_start: false,
                error_after_start: n == 0, // 仅第一实例演故障
                events_tx: etx,
            };
            (Box::new(cap) as Box<dyn AudioCapture>, erx)
        });
        let r2 = recovered.clone();
        let notify = ResilientNotify {
            on_recovered: Some(Box::new(move || {
                r2.fetch_add(1, Ordering::SeqCst);
            })),
            on_lost: Some(Box::new(|| panic!("不应放弃"))),
        };
        let mut cap = ResilientCapture::with_backoff(factory, notify, fast_backoff());
        let (sink_tx, sink_rx) = crossbeam_channel::unbounded();
        cap.start(sink_tx).expect("首启成功");

        // 有界等待:第二实例的帧到达同一 sink(共 4 帧)。
        let deadline = std::time::Instant::now() + Duration::from_secs(3);
        let mut got = 0;
        while got < 4 && std::time::Instant::now() < deadline {
            if sink_rx.recv_timeout(Duration::from_millis(50)).is_ok() {
                got += 1;
            }
        }
        assert_eq!(got, 4, "两个实例各 2 帧都应到达同一 sink");
        assert_eq!(built.load(Ordering::SeqCst), 2, "恰重建一次");
        assert_eq!(recovered.load(Ordering::SeqCst), 1, "恢复回调恰一次");
        cap.stop();
    }

    /// 重试全败 → on_lost 恰一次;之后 stop() 不悬挂。
    #[test]
    fn gives_up_after_backoff_round_and_stop_does_not_hang() {
        let lost = Arc::new(AtomicU32::new(0));
        let built = Arc::new(AtomicU32::new(0));
        let b2 = built.clone();
        let factory: CaptureFactory = Box::new(move || {
            let n = b2.fetch_add(1, Ordering::SeqCst);
            let (etx, erx) = crossbeam_channel::unbounded();
            let cap = Scripted {
                frames: 1,
                fail_start: n > 0, // 首启成功,之后全败
                error_after_start: n == 0,
                events_tx: etx,
            };
            (Box::new(cap) as Box<dyn AudioCapture>, erx)
        });
        let l2 = lost.clone();
        let notify = ResilientNotify {
            on_recovered: Some(Box::new(|| panic!("不应恢复"))),
            on_lost: Some(Box::new(move || {
                l2.fetch_add(1, Ordering::SeqCst);
            })),
        };
        let mut cap = ResilientCapture::with_backoff(factory, notify, fast_backoff());
        let (sink_tx, _sink_rx) = crossbeam_channel::unbounded();
        cap.start(sink_tx).expect("首启成功");

        let deadline = std::time::Instant::now() + Duration::from_secs(3);
        while lost.load(Ordering::SeqCst) == 0 && std::time::Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(lost.load(Ordering::SeqCst), 1, "一轮耗尽报一次 lost");
        assert_eq!(built.load(Ordering::SeqCst), 3, "首建 + 两次退避重试");
        cap.stop(); // 放弃态守在 ctrl 上,stop 应立即返回
    }

    /// 外部 kick(FrameTap 失联)同样触发重启——覆盖无错误回调的后端。
    #[test]
    fn external_kick_triggers_restart() {
        let built = Arc::new(AtomicU32::new(0));
        let b2 = built.clone();
        let factory: CaptureFactory = Box::new(move || {
            b2.fetch_add(1, Ordering::SeqCst);
            let (etx, erx) = crossbeam_channel::unbounded();
            // 永不主动报错:只能靠 kick 触发重启(模拟 VPIO/SCK)。
            let cap =
                Scripted { frames: 1, fail_start: false, error_after_start: false, events_tx: etx };
            (Box::new(cap) as Box<dyn AudioCapture>, erx)
        });
        let mut cap =
            ResilientCapture::with_backoff(factory, ResilientNotify::none(), fast_backoff());
        let kicker = cap.kicker();
        let (sink_tx, sink_rx) = crossbeam_channel::unbounded();
        cap.start(sink_tx).expect("首启成功");
        let _ = sink_rx.recv_timeout(Duration::from_secs(1)).expect("首实例帧");

        kicker.try_send(()).expect("kick 应可投递");
        // 重启后第二实例再出 1 帧。
        let _ = sink_rx.recv_timeout(Duration::from_secs(3)).expect("重启后帧");
        assert_eq!(built.load(Ordering::SeqCst), 2, "kick 触发恰一次重建");
        cap.stop();
    }

    /// 停录发生在退避等待中:立即放弃重试,stop 不拖延。
    #[test]
    fn stop_during_backoff_returns_promptly() {
        let factory: CaptureFactory = Box::new(move || {
            let (etx, erx) = crossbeam_channel::unbounded();
            let cap =
                Scripted { frames: 0, fail_start: false, error_after_start: true, events_tx: etx };
            (Box::new(cap) as Box<dyn AudioCapture>, erx)
        });
        let mut cap = ResilientCapture::with_backoff(
            factory,
            ResilientNotify::none(),
            vec![Duration::from_secs(30)], // 故意超长退避:靠 stop 打断
        );
        let (sink_tx, _sink_rx) = crossbeam_channel::unbounded();
        cap.start(sink_tx).expect("首启成功");
        std::thread::sleep(Duration::from_millis(50)); // 让错误事件进入退避等待
        let t0 = std::time::Instant::now();
        cap.stop();
        assert!(
            t0.elapsed() < Duration::from_secs(2),
            "stop 应打断退避立即返回,实际 {:?}",
            t0.elapsed()
        );
    }
}
