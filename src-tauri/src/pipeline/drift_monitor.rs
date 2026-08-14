//! 每源时钟漂移监视器:包一个 `DriftDll` + 状态记录,供 frame_tap(Task 5)按
//! 真实帧喂入、停录(Task 6)取 snapshot 落盘。纯状态与出口层,不做采集/不做 I/O。
//!
//! 依据:`docs/2026-08-12-clock-drift-sensor-design.md` 第四节。

use crate::audio::drift_dll::{DllConfig, DriftDll};
use crate::audio::{AudioFrame, Source};
use std::path::Path;
use std::sync::{Arc, Mutex};

/// 每 10s 记一个 (自首帧秒, ppm) 采样点,供报告画趋势线。
///
/// 桶号非单调(issue #100 条 7):t_s 取自帧的硬件时间戳,设备缓冲回吐、时戳跳变
/// 都可能让相邻两帧的 t_s 回退,于是桶号会倒退,同一个桶号也可能被跨越两次。
/// `last_series_bucket` 只做"同桶去重",不保证序列点严格等距、严格递增——读
/// rate_ppm_series 时要按点自带的 t_s 定位,不能拿下标当时间。
const SERIES_BUCKET_SECS: f64 = 10.0;
/// 报告阈值:|rate_ppm| 超过此值判异常(设计文档第四节)。
const ANOMALY_RATE_PPM: f64 = 500.0;
/// 报告阈值:单源重锚次数超过此值判异常。
const ANOMALY_REANCHORS: u32 = 3;
/// 重锚风暴判据(issue #100 条 1):每分钟重锚次数的峰值超过此值 → 声明率不可信。
/// 用密度而非总数:长会话自然会攒出几次零星重锚(补零段结束、暂停恢复),而
/// 风暴是短时间内连打——冒烟③单场 4420 次,2026-08-14 两场故障录音是 12.9 与
/// 31.9 次/分钟,健康会话则是 0。取 10 次/分钟,三个真实案例全覆盖且远离健康值。
const ANOMALY_REANCHOR_BURST_PER_MIN: u32 = 10;
/// 风暴密度的统计窗(秒)。
const REANCHOR_BURST_WINDOW_SECS: f64 = 60.0;
/// drift_report 保留的历史场次上限(issue #100 条 4)。续录一场笔记可能停录几十次,
/// 不设上限会让报告无限膨胀;保留最近若干场足够回溯。
const HISTORY_MAX: usize = 20;
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
    /// 任一 60s 窗口内的重锚次数峰值(issue #100 条 1)。风暴看密度不看总数:
    /// 长会话零星重锚是正常的,短时间连打才说明声明率不可信。
    #[serde(default)]
    pub reanchor_burst_per_min: u32,
    /// (自首帧秒, ppm),每 10s 一点。
    pub rate_ppm_series: Vec<(f64, f64)>,
    pub events: Vec<DriftEvent>,
    /// 系统实测采样率旁证(Task 7,`kAudioDevicePropertyActualSampleRate`)相对
    /// nominal_hz 的偏差(ppm),与 DLL 估计的 `rate_ppm` 互为旁证。None = 尚未
    /// 收到过一次成功查询(非 macOS/查询失败/nominal 未锁定)。
    pub actual_rate_ppm: Option<f64>,
    /// 录音设备名(issue #100 条 6):按设备归类才能看出"最差设备组合"——
    /// 同一台机器上内置麦、蓝牙 HFP、USB 声卡的漂移特性完全不同,混在一起统计
    /// 出来的分布没有意义。会话起点取一次,查询失败为 None(纯诊断,不影响行为)。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub device_name: Option<String>,
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
    /// DLL 实例被 nominal 重锁替换时,旧实例的重锚计数结转于此,保证全会话单调
    /// (不因换实例而清零)。
    reanchor_carry: u32,
    /// 最近一次 `set_actual_rate_hz` 换算出的 ppm(Task 7 旁证)。None = 尚未成功
    /// 查询过一次。
    actual_rate_ppm: Option<f64>,
    /// 重锚发生时刻(自首帧秒),用于算密度峰值。只留最近 REANCHOR_BURST_WINDOW_SECS
    /// 窗口内的,不随会话长度增长。
    reanchor_times: std::collections::VecDeque<f64>,
    /// 任一 60s 窗口内的重锚次数峰值。
    reanchor_burst_peak: u32,
    /// 会话起点(单调墙钟)。重锚密度以真实时间为分母,不能用可回退的帧时间戳。
    session_start: std::time::Instant,
    /// 录音设备名(首次成功解析时取一次)。
    device_name: Option<String>,
    /// 设备归属已作废(会话中途换过设备):此后拒收实测率,报告里 device_name/
    /// actual_rate_ppm 均为 None。
    device_evidence_void: bool,
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
                reanchor_carry: 0,
                actual_rate_ppm: None,
                device_name: None,
                device_evidence_void: false,
                reanchor_times: std::collections::VecDeque::new(),
                reanchor_burst_peak: 0,
                session_start: std::time::Instant::now(),
            }),
        }
    }

    /// 录音设备名(issue #100 条 6),由装配层在首次成功解析设备时给一次。
    /// 归属一旦作废就不再接受新名字——报告里只留一个孤零零的设备名而没有旁证,
    /// 反而会让读者以为这就是本场录音的设备。
    pub fn set_device_name(&self, name: String) {
        let mut g = self.inner.lock().unwrap();
        if g.device_evidence_void {
            return;
        }
        g.device_name = Some(name);
    }

    /// 设备中途变了(默认输入被改 / 断连自愈重开了另一只麦):此前测得的实测率
    /// 与设备名都归属不明,一并作废并锁死,不再接受新值。宁可报 None,也不发布
    /// 一个张冠李戴的旁证(Codex review P1)。
    pub fn invalidate_device_evidence(&self) {
        let mut g = self.inner.lock().unwrap();
        g.actual_rate_ppm = None;
        g.device_name = None;
        g.device_evidence_void = true;
    }

    /// 系统实测采样率旁证(Task 7):`hz` 来自
    /// `audio::actual_rate::actual_hz_of(dev)`(CoreAudio 查询**本场录音所绑定设备**的
    /// `kAudioDevicePropertyActualSampleRate`;设备在会话起点解析一次并固定,
    /// 见 issue #100 条 2),按当前 nominal_hz 换算成 ppm 存入报告,与 DLL 频率估计
    /// 互为旁证。
    ///
    /// 除零防御:nominal_hz==0 是 `DriftMonitor::new(0)` 的惰性初始化未锁定态(首帧尚
    /// 未到达,见 `feed` 顶部注释)——此时没有基准可换算,静默跳过不存值,等首帧锁定
    /// 标称率后,下一次轮询自然回到有效路径。
    pub fn set_actual_rate_hz(&self, hz: f64) {
        let mut g = self.inner.lock().unwrap();
        if g.nominal_hz == 0 || g.device_evidence_void {
            return;
        }
        g.actual_rate_ppm = Some((hz / g.nominal_hz as f64 - 1.0) * 1e6);
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
            // old==0 是 `DriftMonitor::new(0)` 的惰性初始化:装配处不知道设备声明率
            // (mic 声明率要 start 后才可知),约定 0 表示"以首帧声明率为准"。这不是
            // 真正的"换率"——没有旧状态可言,不记 nominal_relock 事件、不结转重锚数,
            // 静默锁定即可。真正的换率(old!=0)才是运行期异常,照旧记事件。
            let is_initial_lock = old == 0;
            if !is_initial_lock {
                // 旧 DLL 实例即将被丢弃,其累计重锚数结转到 reanchor_carry,否则
                // snapshot() 直接读新实例的 reanchors 会把切换前的重锚历史静默清零。
                g.reanchor_carry += g.dll.estimate().reanchors;
                // Codex review Fix 2(P2):旧标称率下测得的 actual_rate_ppm 是旧设备/旧
                // 标称率的旁证,换标称率后继续挂在新设备名下会误导——不结转,清空等
                // 下一次轮询(Task 7,10s 一次)重新对新标称率打一个新值。
                g.actual_rate_ppm = None;
            }
            g.dll = DriftDll::new(frame.sample_rate as f64, DllConfig::default());
            g.nominal_hz = frame.sample_rate;
            g.total_samples = 0;
            // 换标称率后频率状态全部作废,收敛需重新计时,故清空 converge_secs。
            g.converge_secs = None;
            if !is_initial_lock {
                let t_s = g.last_t_s;
                g.events.push(DriftEvent {
                    t_s,
                    kind: "nominal_relock".into(),
                    why: format!("rate {old}->{}", frame.sample_rate),
                });
            }
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
        // 密度按**真实时间**算,故用会话单调墙钟,不用帧的 t_s:`feed` 明确容忍
        // 硬件时间戳回退(设备吐旧缓冲/时戳跳变),拿它当基准时滑窗会乱序,前端
        // 淘汰再也推不动,稀疏重锚被累积成假风暴(Codex review P1)。墙钟单调,
        // 且"每分钟多少次"本就该以真实时间为分母。
        let now_s = self.inner.lock().unwrap().session_start.elapsed().as_secs_f64();
        self.mark_reanchor_at(why, full, now_s);
    }

    /// 时钟可注入版:生产走 `mark_reanchor`(会话单调墙钟),测试直接给时刻——
    /// 否则"每 90 秒一次不算风暴"这类用例只能靠真睡 90 秒来表达。
    pub(crate) fn mark_reanchor_at(&self, why: &'static str, full: bool, now_s: f64) {
        let mut g = self.inner.lock().unwrap();
        g.dll.reanchor(!full);
        let t_s = g.last_t_s;
        // 风暴密度(issue #100 条 1):滑窗内计数,记峰值。窗口只留 60s 内的时刻,
        // 内存不随会话长度增长;now_s 单调 ⇒ 队列有序 ⇒ 前端淘汰必定推进。
        g.reanchor_times.push_back(now_s);
        while g
            .reanchor_times
            .front()
            .is_some_and(|&t| now_s - t > REANCHOR_BURST_WINDOW_SECS)
        {
            g.reanchor_times.pop_front();
        }
        let burst = g.reanchor_times.len() as u32;
        g.reanchor_burst_peak = g.reanchor_burst_peak.max(burst);
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
            reanchors: g.reanchor_carry + est.reanchors,
            reanchor_burst_per_min: g.reanchor_burst_peak,
            rate_ppm_series: g.series.clone(),
            events: g.events.clone(),
            actual_rate_ppm: g.actual_rate_ppm,
            device_name: g.device_name.clone(),
        }
    }
}

/// 汇总各源报告成 drift_report.json 的顶层结构(纯函数,好测)。
/// schema:`{"schema":1,"sources":{...},"inter_track":{...},"anomalies":[...]}`;
/// `inter_track` 仅双源(mic + system 都在)**且两源都已收敛**时给值,否则为
/// `null`(Codex review Fix 3,P2:未收敛的 rate_ppm 噪声大,发布出去是假精度);
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
        // 重锚风暴 = 声明率不可信(issue #100 条 1)。CoreAudio 在默认设备下透明换
        // 硬件时声明率不变、流不重启,nominal_relock/capture_restart 都不触发,
        // 只有重锚密度会飙——这条把间接线索升格为直说的诊断。
        //
        // 刻意**不**联动"提前触发 rate_fix":issue 原文提出过这个设想,但 PR #103
        // 查明,那两场风暴的重锚全部来自 rate_fix 本身,而 rate_fix 是被"用到达间隔
        // 测采样率"这个错误测量骗出来的(已改用硬件时间戳根治,同机同麦复测 0 次)。
        // 把执行器调得更激进,等于把刚修好的音调失真重新放出来。
        if report.reanchor_burst_per_min >= ANOMALY_REANCHOR_BURST_PER_MIN {
            anomalies.push(serde_json::json!({
                "source": key,
                "kind": "declared_rate_untrusted",
                "value": report.reanchor_burst_per_min,
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
    // Codex review Fix 3(P2):双源都收敛才发布 inter_track——未收敛的 rate_ppm 估计
    // 噪声大,拿它算出的 rel_ppm/est_misalign 是假精度,读者拿着当结论用会被误导。
    // 未双收敛时按"没有可用的双源结论"处理,与单源情形(只有一个源时)同为 null。
    let inter_track = match (mic, system) {
        (Some(mic), Some(system)) if mic.converged && system.converged => {
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

/// 停录单点调用:各源 snapshot → `build_report` → 覆盖写 `<note_dir>/drift_report.json`。
/// 一期铁律:传感器只测不动数据,本函数只负责落盘,不影响停录主流程——调用方
/// (lib.rs `do_stop_teardown`)对本函数的 `Err` 只 eprintln,绝不中断/回滚停录。
/// 报告是终值(会话已结束、各源不会再 feed),故直接覆盖写,无需加锁保护文件。
/// 返回写入的 anomalies 数组,供调用方逐条打日志(不强制——测试只关心文件内容)。
pub fn persist_report(
    note_dir: &Path,
    sources: &[(Source, Arc<DriftMonitor>)],
) -> std::io::Result<Vec<serde_json::Value>> {
    let reports: Vec<(Source, DriftSourceReport)> =
        sources.iter().map(|(src, m)| (*src, m.snapshot())).collect();
    let mut report = build_report(&reports);
    let anomalies = report["anomalies"].as_array().cloned().unwrap_or_default();
    let path = note_dir.join("drift_report.json");
    // 续录/多次停录不再丢证据(issue #100 条 4):旧报告收进 history,顶层仍是本场
    // ——drift_stats 等既有读者的口径完全不变,不必跟着改。history 内的条目自身
    // 先剥掉 history 再存,否则每停录一次就把整棵历史再套一层,指数膨胀。
    let mut history: Vec<serde_json::Value> = Vec::new();
    if let Ok(raw) = std::fs::read_to_string(&path) {
        if let Ok(mut prev) = serde_json::from_str::<serde_json::Value>(&raw) {
            if let Some(old) = prev.get_mut("history").and_then(|h| h.as_array_mut()) {
                history.append(old);
            }
            if let Some(obj) = prev.as_object_mut() {
                obj.remove("history");
            }
            history.push(prev);
        }
    }
    if history.len() > HISTORY_MAX {
        history.drain(..history.len() - HISTORY_MAX);
    }
    if !history.is_empty() {
        report["history"] = serde_json::Value::Array(history);
    }
    let json = serde_json::to_string_pretty(&report)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))?;
    std::fs::write(path, json)?;
    Ok(anomalies)
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
    fn nominal_relock_carries_over_reanchor_count_and_resets_converge() {
        let m = DriftMonitor::new(48_000);
        for k in 0..100u64 {
            let ns = k * 10_000_000; // 10ms/帧
            m.feed(&frame(480, 48_000, Some(ns)));
        }
        // 换率前先制造一次重锚,验证其计数不会被 relock 换实例清零。
        m.mark_reanchor("gap_end", false);
        // Codex review Fix 2(P2):relock 前先打一个 actual_rate_ppm 旁证值,验证
        // relock 会把它清掉(旧标称率/旧设备的旁证不得挂在新标称率名下)。
        m.set_actual_rate_hz(48_000.48); // +10ppm(相对旧 nominal_hz=48000)
        assert!(m.snapshot().actual_rate_ppm.is_some(), "预置旁证应先生效");
        // 设备中途换率:标称率重锁,内部 DriftDll 被整体替换成新实例。
        let base_ns = 100 * 10_000_000;
        for k in 0..10u64 {
            let ns = base_ns + k * 10_000_000;
            m.feed(&frame(441, 44_100, Some(ns)));
        }
        let r = m.snapshot();
        assert_eq!(r.nominal_hz, 44_100);
        assert!(
            r.reanchors >= 1,
            "换实例前的重锚计数应结转,不应被清零: {}",
            r.reanchors
        );
        assert_eq!(
            r.converge_secs, None,
            "重锁后应重新计收敛,converge_secs 应被重置"
        );
        assert_eq!(
            r.actual_rate_ppm, None,
            "relock 后旧标称率的 actual_rate_ppm 旁证不得沿用到新设备名下"
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
            reanchor_burst_per_min: 0,
            rate_ppm_series: vec![],
            events: vec![],
            actual_rate_ppm: None,
            device_name: None,
        };
        let sys = DriftSourceReport {
            rate_ppm: -600.0,
            reanchors: 5,
            reanchor_burst_per_min: 0,
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

    /// Codex review Fix 3(P2):一源未收敛时 inter_track 必须为 null——未收敛的
    /// rate_ppm 估计噪声大,发布出去是假精度。双源都在但只有一个 converged=false
    /// 就该按"没有可用的双源结论"处理,与只有单源时同为 null。
    #[test]
    fn inter_track_is_null_when_either_source_has_not_converged() {
        let mic = DriftSourceReport {
            nominal_hz: 48_000,
            quality: "hw".into(),
            rate_ppm: 100.0,
            converged: true,
            converge_secs: Some(14.0),
            reanchors: 0,
            reanchor_burst_per_min: 0,
            rate_ppm_series: vec![],
            events: vec![],
            actual_rate_ppm: None,
            device_name: None,
        };
        let sys_not_converged = DriftSourceReport {
            rate_ppm: -600.0,
            converged: false,
            converge_secs: None,
            ..mic.clone()
        };
        let v = build_report(&[(Source::Mic, mic), (Source::System, sys_not_converged)]);
        assert!(
            v["inter_track"].is_null(),
            "一源未收敛时 inter_track 应为 null,实得: {:?}",
            v["inter_track"]
        );
    }

    #[test]
    fn lazy_nominal_rate_locks_to_first_frame() {
        let m = DriftMonitor::new(0);
        m.feed(&frame(441, 44_100, Some(0)));
        assert_eq!(m.snapshot().nominal_hz, 44_100);
    }

    #[test]
    fn lazy_nominal_rate_lock_is_not_recorded_as_relock_event() {
        // 惰性初始化(new(0))的首帧锁定不是"运行期换率",不该产生 nominal_relock
        // 事件,也不该有重锚计数结转——那是留给真实换率(设备中途切率)的语义。
        let m = DriftMonitor::new(0);
        m.feed(&frame(441, 44_100, Some(0)));
        let r = m.snapshot();
        assert!(
            r.events.is_empty(),
            "惰性首帧锁定不应记事件: {:?}",
            r.events
        );
        assert_eq!(r.reanchors, 0);
    }

    #[test]
    fn actual_rate_ppm_is_relative_to_nominal() {
        let m = DriftMonitor::new(48_000);
        m.feed(&frame(480, 48_000, Some(0)));
        m.set_actual_rate_hz(48_000.48); // +10ppm
        let r = m.snapshot();
        assert!((r.actual_rate_ppm.unwrap() - 10.0).abs() < 0.01);
    }

    /// issue #100 条 1:重锚风暴升格为显式诊断。CoreAudio 在默认设备下透明换硬件
    /// 时声明率不变、流不重启,nominal_relock/capture_restart 都不触发,只能靠
    /// DLL 重锚风暴间接暴露(冒烟③单场 4420 次)。总数阈值不够用——长会话自然会
    /// 攒出几次;真正的判据是**密度**(每分钟多少次)。
    #[test]
    fn reanchor_storm_is_reported_as_declared_rate_untrusted() {
        let m = DriftMonitor::new(48_000);
        // 推进到 t≈30s,然后在 1 秒内连打 20 次重锚 = 远超每分钟阈值。
        for _ in 0..3000 {
            m.feed(&frame(480, 48_000, Some(0)));
        }
        for i in 0..20 {
            m.mark_reanchor_at("rate_fix", false, i as f64 * 0.05); // 1 秒内连打 20 次
        }
        let r = m.snapshot();
        assert!(r.reanchor_burst_per_min >= 20, "峰值密度应记满: {}", r.reanchor_burst_per_min);
        let v = build_report(&[(Source::Mic, r)]);
        let kinds: Vec<&str> = v["anomalies"]
            .as_array()
            .unwrap()
            .iter()
            .map(|a| a["kind"].as_str().unwrap())
            .collect();
        assert!(kinds.contains(&"declared_rate_untrusted"), "应升格为显式诊断: {kinds:?}");
    }

    /// 零星重锚(正常会话:补零段结束、暂停恢复)不得被误报成风暴。
    /// 每次重锚之间真推进 90 秒——早先这里只把时间戳从 0 加到 4(纳秒),
    /// 等于根本没推进时间,测试是靠"总数不够"蒙混过关的(Codex review P1 指出)。
    #[test]
    fn occasional_reanchors_are_not_a_storm() {
        let m = DriftMonitor::new(48_000);
        for i in 0..20u64 {
            m.feed(&frame(480, 48_000, Some(i * 90_000_000_000)));
            m.mark_reanchor_at("gap_end", false, i as f64 * 90.0); // 真实时间每步 +90s
        }
        let r = m.snapshot();
        assert_eq!(r.reanchors, 20, "总数确实攒到了 20(超过 reanchors_high 阈值)");
        assert!(
            r.reanchor_burst_per_min <= 1,
            "每 90 秒一次 = 密度 1/分钟,不是风暴: {}",
            r.reanchor_burst_per_min
        );
        let v = build_report(&[(Source::Mic, r)]);
        let kinds: Vec<&str> = v["anomalies"]
            .as_array()
            .unwrap()
            .iter()
            .map(|a| a["kind"].as_str().unwrap())
            .collect();
        assert!(!kinds.contains(&"declared_rate_untrusted"), "稀疏重锚不是风暴: {kinds:?}");
    }

    /// Codex review P1:`feed` 明确容忍硬件时间戳回退(设备缓冲回吐/时戳跳变),
    /// 于是 t_s 会倒退。滑窗若直接用"当前 t_s − 队首"淘汰,队首一旦位于"未来"
    /// 差值就恒为负,旧重锚永远淘汰不掉,稀疏重锚累积成假风暴。
    #[test]
    fn burst_window_survives_timestamp_regression() {
        let m = DriftMonitor::new(48_000);
        // 真实时间每 70 秒一次(稀疏),但帧的硬件时间戳来回跳:先递增再整体回退。
        // 密度以真实时间为准,不该被时戳回退带偏。
        for i in 0..10u64 {
            m.feed(&frame(480, 48_000, Some(i * 60_000_000_000)));
            m.mark_reanchor_at("gap_end", false, i as f64 * 70.0);
        }
        for i in 0..10u64 {
            m.feed(&frame(480, 48_000, Some(i * 60_000_000_000))); // 时戳回退
            m.mark_reanchor_at("gap_end", false, 700.0 + i as f64 * 70.0);
        }
        let r = m.snapshot();
        assert!(
            r.reanchor_burst_per_min < ANOMALY_REANCHOR_BURST_PER_MIN,
            "时戳回退不得把稀疏重锚累积成假风暴: {}",
            r.reanchor_burst_per_min
        );
    }

    /// Codex review P1:会话中途换设备(用户改默认输入,或断连自愈重开了另一只麦)
    /// 后,此前测得的实测率属于另一只设备。cpal 不暴露流绑定的 AudioDeviceID,
    /// 归属无法确定时就不发布——作废并锁死,而不是发一个张冠李戴的数字。
    #[test]
    fn device_change_voids_evidence_permanently() {
        let m = DriftMonitor::new(48_000);
        m.set_device_name("MacBook Pro麦克风".into());
        m.set_actual_rate_hz(48_000.48);
        assert!(m.snapshot().actual_rate_ppm.is_some(), "换设备前正常发布");

        m.invalidate_device_evidence();
        let r = m.snapshot();
        assert_eq!(r.actual_rate_ppm, None, "旁证作废");
        assert_eq!(r.device_name, None, "设备名一并作废");

        // 作废后即便再来新值也不收:归属已经不明,发什么都是猜的。
        m.set_actual_rate_hz(48_000.48);
        m.set_device_name("OpenRun Pro by Shokz".into());
        let r = m.snapshot();
        assert_eq!(r.actual_rate_ppm, None, "作废是锁死的");
        assert_eq!(r.device_name, None, "设备名同样不再接受——只留名字没有旁证更误导");
    }

    /// issue #100 条 4:续录/多次停录时 drift_report 被覆盖写,只剩最后一场,
    /// 前面场次的证据全丢。改为把旧报告收进 history(顶层仍是最新场次,
    /// drift_stats 等既有读者零改动)。
    #[test]
    fn persist_report_keeps_previous_sessions_in_history() {
        let dir = tempfile::tempdir().unwrap();
        let mk = |untimed: bool| {
            let m = std::sync::Arc::new(DriftMonitor::new(48_000));
            m.feed(&frame(480, 48_000, if untimed { None } else { Some(0) }));
            m
        };
        persist_report(dir.path(), &[(Source::Mic, mk(true))]).unwrap();
        persist_report(dir.path(), &[(Source::Mic, mk(false))]).unwrap();
        persist_report(dir.path(), &[(Source::Mic, mk(false))]).unwrap();

        let raw = std::fs::read_to_string(dir.path().join("drift_report.json")).unwrap();
        let v: serde_json::Value = serde_json::from_str(&raw).unwrap();
        // 顶层 = 最新一场(既有读者的口径不变)
        assert_eq!(v["schema"], 1);
        assert_eq!(v["sources"]["mic"]["quality"], "hw");
        // 历史按时间顺序保留前两场,且 history 自身不嵌套 history(不指数膨胀)
        let hist = v["history"].as_array().expect("应有 history");
        assert_eq!(hist.len(), 2, "两场旧报告都要在");
        assert_eq!(hist[0]["sources"]["mic"]["quality"], "degraded", "最早那场在最前");
        assert!(hist[0].get("history").is_none(), "history 内不得再嵌套 history");
    }

    #[test]
    fn persist_report_writes_json_with_anomalies() {
        let dir = tempfile::tempdir().unwrap();
        let m = std::sync::Arc::new(DriftMonitor::new(48_000));
        m.feed(&frame(480, 48_000, None)); // degraded → 必然产生一条 anomaly
        persist_report(dir.path(), &[(Source::Mic, m)]).unwrap();
        let raw = std::fs::read_to_string(dir.path().join("drift_report.json")).unwrap();
        let v: serde_json::Value = serde_json::from_str(&raw).unwrap();
        assert_eq!(v["schema"], 1);
        assert_eq!(v["sources"]["mic"]["quality"], "degraded");
        assert!(!v["anomalies"].as_array().unwrap().is_empty());
    }
}
