//! 每源时钟漂移监视器:包一个 `DriftDll` + 状态记录,供 frame_tap(Task 5)按
//! 真实帧喂入、停录(Task 6)取 snapshot 落盘。纯状态与出口层,不做采集/不做 I/O。
//!
//! 依据:`docs/2026-08-12-clock-drift-sensor-design.md` 第四节。

use crate::audio::drift_dll::{DllConfig, DriftDll};
use crate::audio::{AudioFrame, Source};
use std::sync::Mutex;

/// 每 10s 记一个 (自首帧秒, ppm) 采样点,供报告画趋势线。
const SERIES_BUCKET_SECS: f64 = 10.0;
/// 报告阈值:|rate_ppm| 超过此值判异常(设计文档第四节)。
const ANOMALY_RATE_PPM: f64 = 500.0;
/// 报告阈值:单源重锚次数超过此值判异常。
const ANOMALY_REANCHORS: u32 = 3;
/// mixed 时间轴换算:1ppm 相对速率偏差,一小时累计的错位(毫秒)。
/// 3600s × 1e-6 × 1000ms/s = 3.6ms/h/ppm。
const MS_PER_HOUR_PER_PPM: f64 = 3.6;

#[derive(Debug, Clone, serde::Serialize)]
pub struct DriftSourceReport {
    pub nominal_hz: u32,
    /// "hw" | "degraded"(见过任一无时间戳帧即降级,含"从未见过任何帧"的保守情形)。
    pub quality: String,
    pub rate_ppm: f64,
    pub converged: bool,
    pub converge_secs: Option<f64>,
    pub reanchors: u32,
    /// (自首帧秒, ppm),每 10s 一点。
    pub rate_ppm_series: Vec<(f64, f64)>,
    pub events: Vec<DriftEvent>,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct DriftEvent {
    pub t_s: f64,
    pub kind: String,
    pub why: String,
}

struct Inner {
    nominal_hz: u32,
    dll: DriftDll,
    /// 该源自会话起累计的单声道样本数(与 dll 的 nominal_hz 口径一致)。
    total_samples: u64,
    /// 见过任一无时间戳帧(降级用 now_ns 顶替) → true,quality 据此判 "degraded"。
    saw_untimed: bool,
    saw_any: bool,
    first_feed_ns: Option<u64>,
    /// 最近一次 feed 换算出的 t_s(自首帧秒),mark_reanchor 无 feed 可依时退化用它
    /// (初值 0.0,与 brief"无 feed 记 0.0"一致)。
    last_t_s: f64,
    /// 已记录到第几个 10s 桶(避免同一桶内重复 push)。
    last_series_bucket: Option<u64>,
    series: Vec<(f64, f64)>,
    events: Vec<DriftEvent>,
    converge_secs: Option<f64>,
}

/// 每源时钟漂移监视器。内部 `Mutex<Inner>`:frame_tap 线程串行调用 `feed`,
/// 停录时可能从另一线程 `snapshot`,故需要跨线程共享 + 内部可变。
///
/// Mutex 纪律:本模块所有对锁的访问都是短临界区、纯内存运算,不做 I/O、不递归加锁、
/// 不跨 await 持锁(同步代码),因此不会主动 panic 持锁——唯一能让锁中毒的路径是本
/// 文件自身的 bug(越界之类)。故用 `lock().unwrap()`:这是"要么锁健康、要么已有
/// 别的 bug 需要暴出来"的场景,吞掉毒锁反而会把真实 bug 悄悄丢数据掩盖掉。
pub struct DriftMonitor {
    inner: Mutex<Inner>,
}

impl DriftMonitor {
    pub fn new(nominal_hz: u32) -> Self {
        Self {
            inner: Mutex::new(Inner {
                nominal_hz,
                dll: DriftDll::new(nominal_hz as f64, DllConfig::default()),
                total_samples: 0,
                saw_untimed: false,
                saw_any: false,
                first_feed_ns: None,
                last_t_s: 0.0,
                last_series_bucket: None,
                series: Vec::new(),
                events: Vec::new(),
                converge_secs: None,
            }),
        }
    }

    /// frame_tap 对每个【真实】帧调用(补零帧不喂)。带时间戳→hw 路径;无→降级用
    /// now_ns(该帧不再计入 hw quality,整源随之降级为 "degraded")。
    ///
    /// 陷阱:`AudioFrame` 到 tap 时是 `to_mono` **之前**的交错多声道原始帧,
    /// `samples.len()` 是"样本数 × 声道数",必须除以 `channels` 才是与
    /// `dll`(单声道 nominal_hz)口径一致的样本推进量。
    pub fn feed(&self, frame: &AudioFrame) {
        let mut g = self.inner.lock().unwrap();
        // 设备中途换了声明率(拔插耳机/切设备后 frame_tap 转发的帧 sample_rate 变了):
        // 旧 nominal_hz 下积累的频率状态在新标称率下全盘失真,ppm/quality 却仍会照旧
        // 标 "hw" ——必须重锁标称率,这是 quality 语义成立的前提。重建的新 DLL 从零
        // 锚定,本帧(改口径后的第一帧)就是它的首个测量点,故只重置 total_samples
        // 让 push 的样本基准与新 DLL 对齐;first_feed_ns/系列点等报告口径不动
        // (那些以本源会话起点为准,与 DLL 内部锚点是两回事)。
        if frame.sample_rate > 0 && frame.sample_rate != g.nominal_hz {
            let old = g.nominal_hz;
            g.dll = DriftDll::new(frame.sample_rate as f64, DllConfig::default());
            g.nominal_hz = frame.sample_rate;
            g.total_samples = 0;
            let t_s = g.last_t_s;
            g.events.push(DriftEvent {
                t_s,
                kind: "nominal_relock".into(),
                why: format!("rate {old}->{}", frame.sample_rate),
            });
        }
        let ns = match frame.host_time_ns {
            Some(ns) => ns,
            None => {
                g.saw_untimed = true;
                crate::audio::host_time::now_ns()
            }
        };
        let first = *g.first_feed_ns.get_or_insert(ns);
        let total = g.total_samples;
        g.dll.push(total, ns);
        let per_channel = frame.samples.len() / frame.channels.max(1) as usize;
        g.total_samples += per_channel as u64;
        g.saw_any = true;
        let t_s = ns.saturating_sub(first) as f64 / 1e9;
        g.last_t_s = t_s;
        let est = g.dll.estimate();
        if g.converge_secs.is_none() && est.converged {
            g.converge_secs = Some(t_s);
        }
        let bucket = (t_s / SERIES_BUCKET_SECS) as u64;
        if g.last_series_bucket != Some(bucket) {
            g.last_series_bucket = Some(bucket);
            g.series.push((t_s, est.rate_ppm));
        }
    }

    /// 设备切换(rate 变)→ full=true(dll 全清,含频率状态);
    /// 补零段结束/暂停恢复 → full=false(dll 保留频率状态,只清相位)。
    /// 记一条事件;`t_s` 取最近一次 `feed` 换算的自首帧秒,没有 feed 过则记 0.0。
    pub fn mark_reanchor(&self, why: &'static str, full: bool) {
        let mut g = self.inner.lock().unwrap();
        g.dll.reanchor(!full);
        let t_s = g.last_t_s;
        g.events.push(DriftEvent {
            t_s,
            kind: if full {
                "reanchor_full".into()
            } else {
                "reanchor_soft".into()
            },
            why: why.into(),
        });
    }

    pub fn snapshot(&self) -> DriftSourceReport {
        let g = self.inner.lock().unwrap();
        let est = g.dll.estimate();
        DriftSourceReport {
            nominal_hz: g.nominal_hz,
            // 未见过任何帧也保守判 degraded:没有真实测量,不能声称 "hw"。
            quality: if g.saw_untimed || !g.saw_any {
                "degraded".into()
            } else {
                "hw".into()
            },
            rate_ppm: est.rate_ppm,
            converged: est.converged,
            converge_secs: g.converge_secs,
            reanchors: est.reanchors,
            rate_ppm_series: g.series.clone(),
            events: g.events.clone(),
        }
    }
}

/// 汇总各源报告成 drift_report.json 的顶层结构(纯函数,好测)。
/// schema:`{"schema":1,"sources":{...},"inter_track":{...},"anomalies":[...]}`;
/// `inter_track` 仅双源(mic + system 都在)时给值,否则为 `null`;
/// anomalies 每条 `{"source","kind","value"}`,按三条设计阈值生成。
pub fn build_report(sources: &[(Source, DriftSourceReport)]) -> serde_json::Value {
    let mut sources_obj = serde_json::Map::new();
    let mut anomalies = Vec::new();

    for (src, report) in sources {
        let key = src.as_str();
        if report.rate_ppm.abs() > ANOMALY_RATE_PPM {
            anomalies.push(serde_json::json!({
                "source": key,
                "kind": "rate_ppm_high",
                "value": report.rate_ppm,
            }));
        }
        if report.quality == "degraded" {
            anomalies.push(serde_json::json!({
                "source": key,
                "kind": "quality_degraded",
                "value": report.quality,
            }));
        }
        if report.reanchors > ANOMALY_REANCHORS {
            anomalies.push(serde_json::json!({
                "source": key,
                "kind": "reanchors_high",
                "value": report.reanchors,
            }));
        }
        sources_obj.insert(key.to_string(), serde_json::to_value(report).unwrap());
    }

    let mic = sources
        .iter()
        .find(|(s, _)| *s == Source::Mic)
        .map(|(_, r)| r);
    let system = sources
        .iter()
        .find(|(s, _)| *s == Source::System)
        .map(|(_, r)| r);
    let inter_track = match (mic, system) {
        (Some(mic), Some(system)) => {
            let rel_ppm = mic.rate_ppm - system.rate_ppm;
            serde_json::json!({
                "rel_ppm": rel_ppm,
                "est_misalign_ms_per_hour": rel_ppm * MS_PER_HOUR_PER_PPM,
            })
        }
        _ => serde_json::Value::Null,
    };

    serde_json::json!({
        "schema": 1,
        "sources": sources_obj,
        "inter_track": inter_track,
        "anomalies": anomalies,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audio::{AudioFrame, Source};

    fn frame(n: usize, rate: u32, ns: Option<u64>) -> AudioFrame {
        AudioFrame {
            samples: vec![0.1; n],
            sample_rate: rate,
            channels: 1,
            host_time_ns: ns,
        }
    }

    #[test]
    fn hw_frames_yield_hw_quality_and_ppm() {
        let m = DriftMonitor::new(48_000);
        // 500ppm 慢设备,60 秒(6000 帧 × 10ms)
        for k in 0..6000u64 {
            let ns = (k as f64 * 0.01 * (1.0 + 500.0 / 1e6) * 1e9) as u64;
            m.feed(&frame(480, 48_000, Some(ns)));
        }
        let r = m.snapshot();
        assert_eq!(r.quality, "hw");
        assert!((r.rate_ppm + 500.0).abs() < 5.0, "got {}", r.rate_ppm);
        assert!(r.rate_ppm_series.len() >= 5, "每 10s 一点,60s 应有 ≥5 点");
        assert!(r.converge_secs.is_some());
    }

    #[test]
    fn any_untimed_frame_degrades_quality() {
        let m = DriftMonitor::new(48_000);
        m.feed(&frame(480, 48_000, Some(0)));
        m.feed(&frame(480, 48_000, None));
        assert_eq!(m.snapshot().quality, "degraded");
    }

    #[test]
    fn reanchor_is_recorded_as_event() {
        let m = DriftMonitor::new(48_000);
        m.feed(&frame(480, 48_000, Some(0)));
        m.mark_reanchor("device_switch", true);
        let r = m.snapshot();
        assert_eq!(r.events.len(), 1);
        assert_eq!(r.events[0].why, "device_switch");
    }

    #[test]
    fn nominal_rate_change_relocks_and_records_event() {
        let m = DriftMonitor::new(48_000);
        for k in 0..100u64 {
            let ns = k * 10_000_000; // 10ms/帧
            m.feed(&frame(480, 48_000, Some(ns)));
        }
        // 设备中途换率(如声明改写成 44.1kHz):标称率必须跟着重锁。
        let base_ns = 100 * 10_000_000;
        for k in 0..10u64 {
            let ns = base_ns + k * 10_000_000;
            m.feed(&frame(441, 44_100, Some(ns)));
        }
        let r = m.snapshot();
        assert_eq!(r.nominal_hz, 44_100);
        assert!(
            r.events.iter().any(|e| e.kind == "nominal_relock"),
            "events: {:?}",
            r.events
        );
    }

    #[test]
    fn report_computes_inter_track_and_anomalies() {
        let mic = DriftSourceReport {
            nominal_hz: 48_000,
            quality: "hw".into(),
            rate_ppm: 100.0,
            converged: true,
            converge_secs: Some(14.0),
            reanchors: 0,
            rate_ppm_series: vec![],
            events: vec![],
        };
        let sys = DriftSourceReport {
            rate_ppm: -600.0,
            reanchors: 5,
            quality: "degraded".into(),
            ..mic.clone()
        };
        let v = build_report(&[(Source::Mic, mic), (Source::System, sys)]);
        assert_eq!(v["schema"], 1);
        let rel = v["inter_track"]["rel_ppm"].as_f64().unwrap();
        assert!((rel - 700.0).abs() < 1e-6);
        assert!(
            (v["inter_track"]["est_misalign_ms_per_hour"]
                .as_f64()
                .unwrap()
                - 2520.0)
                .abs()
                < 1e-6
        );
        let anomalies = v["anomalies"].as_array().unwrap();
        // system 触发三条:|ppm|>500、degraded、reanchors>3
        assert_eq!(
            anomalies.iter().filter(|a| a["source"] == "system").count(),
            3
        );
    }
}
