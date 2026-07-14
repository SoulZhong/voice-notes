//! 软件回声消除(WebRTC AEC3,bundled 构建)。
//!
//! 「保持外放音量」模式下 VPIO(苹果通话模式)不启动,外放回声会原样串进麦克风;
//! 本模块以 system 采集流为远端参考,把回声从 mic 波形里减掉——mic 路只剩本人
//! 声音,转写不再依赖文本级回声去重(那套链路保留为兜底)。
//!
//! 结构:`new_pair` 造一对句柄,共享同一个线程安全的 `Processor`(&self API):
//! - `AecRender` 给 system 分段 worker:重采样后的 16k 单声道样本喂 render 侧
//!   (analyze,不修改样本,system 路转写零影响);
//! - `AecCapture` 给 mic 分段 worker:样本原地消回声后再进录音落盘与 VAD。
//!
//! 两端各自做 10ms(160 样本 @16k)分帧,不足一帧的余量滞留到下一次——只带来
//! <10ms 的处理粒度,样本总数守恒(流结束时至多丢一帧余量,≤10ms 尾音)。
//! 外放延迟(输出缓冲+声学路径,数十到数百 ms)由 AEC3 内置延迟估计吸收,
//! 无需显式对齐两路时间轴。

use std::sync::Arc;
use webrtc_audio_processing::{config, Config, Processor};

/// 10ms @ 16kHz。Processor::new(16000) 的帧长与之一致(num_samples_per_frame)。
const FRAME: usize = 160;

/// 建一对 AEC 句柄(render 给 system worker,capture 给 mic worker)。
/// 开两个子模块:回声消除 + AGC2 自适应数字增益;降噪等保持关闭。
pub fn new_pair(sample_rate: u32) -> anyhow::Result<(AecRender, AecCapture)> {
    let ap = Processor::new(sample_rate).map_err(|e| anyhow::anyhow!("AEC 初始化失败: {e}"))?;
    ap.set_config(Config {
        echo_canceller: Some(config::EchoCanceller::default()),
        // 自动增益(AGC2 自适应数字):普通麦克风模式没有 VPIO 的增益管理,系统输入
        // 音量被会议软件拉低/说话偏轻时波形过小——VAD 概率过不了阈,句子根本不切段
        // (观感"声音小就不转写"),录音回放也听不见。自适应数字增益作用在回声消除
        // 之后(不放大回声),把人声抬到正常电平。input_volume_controller 关死:
        // 绝不碰系统输入音量旋钮——那个旋钮会被会议软件抢,抢完不还(2026-07-06
        // 排障实锤),我们只做进程内数字增益,不参与系统层拉锯。
        gain_controller: Some(config::GainController::GainController2(config::GainController2 {
            input_volume_controller_enabled: false,
            // 平衡档(0.002/0.005/0.05/0.15 四电平实验选定,见 agc_experiments):
            // 真耳语(0.002≈-54dBFS)抬 158x 功率进 VAD 可识别区间;正常人声(0.05)
            // 温和 5x 改善回放响度;响亮人声(0.15)不动。默认参数的噪声地板 -50dBFS
            // 会把真耳语当噪声拒掉,故放宽到 -44 并提高 max_gain/爬坡速度。
            // 底噪会被抬到约 -44dBFS(轻微可闻),VAD/语言过滤/会后精修三层兜幻觉段。
            adaptive_digital: Some(config::AdaptiveDigital {
                headroom_db: 3.0,
                max_gain_db: 60.0,
                initial_gain_db: 22.0,
                max_gain_change_db_per_second: 12.0,
                max_output_noise_level_dbfs: -44.0,
            }),
            fixed_digital: config::FixedDigital::default(),
        })),
        ..Default::default()
    });
    let ap = Arc::new(ap);
    Ok((
        AecRender { ap: ap.clone(), buf: Vec::new() },
        AecCapture { ap, buf: Vec::new() },
    ))
}

/// 离线清洗用的一对句柄:AEC3 + 降噪(NS High),不开 AGC。
/// 与实时录制的 new_pair 区别:清洗输入是录制时已经 AGC 过的波形,再增益会把
/// 底噪二次抬升;降噪在这里开(实时链路二期再评估),清掉普通麦克风路径的底噪。
pub fn new_clean_pair(sample_rate: u32) -> anyhow::Result<(AecRender, AecCapture)> {
    let ap = Processor::new(sample_rate).map_err(|e| anyhow::anyhow!("清洗 APM 初始化失败: {e}"))?;
    ap.set_config(Config {
        echo_canceller: Some(config::EchoCanceller::default()),
        noise_suppression: Some(config::NoiseSuppression {
            level: config::NoiseSuppressionLevel::High,
            ..Default::default()
        }),
        ..Default::default()
    });
    let ap = Arc::new(ap);
    Ok((
        AecRender { ap: ap.clone(), buf: Vec::new() },
        AecCapture { ap, buf: Vec::new() },
    ))
}

/// 分段 worker 的 AEC 角色:随源分发(system=Render 喂参考,mic=Capture 消回声)。
pub enum AecRole {
    Render(AecRender),
    Capture(AecCapture),
}

/// 远端(system 路)句柄:喂参考信号,不修改样本。
pub struct AecRender {
    ap: Arc<Processor>,
    buf: Vec<f32>,
}

impl AecRender {
    /// 喂入 system 路重采样后的 16k 单声道样本。逐 10ms 帧 analyze;
    /// 失败只打日志(远端分析失败不该影响 system 路自身的转写)。
    pub fn push(&mut self, samples: &[f32]) {
        self.buf.extend_from_slice(samples);
        let full = (self.buf.len() / FRAME) * FRAME;
        for chunk in self.buf[..full].chunks(FRAME) {
            if let Err(e) = self.ap.analyze_render_frame([chunk]) {
                eprintln!("AEC render 分析失败(跳过该帧): {e}");
            }
        }
        self.buf.drain(..full);
    }
}

/// 近端(mic 路)句柄:消回声。
pub struct AecCapture {
    ap: Arc<Processor>,
    buf: Vec<f32>,
}

impl AecCapture {
    /// 处理 mic 路 16k 单声道样本,返回消回声后的样本(10ms 整帧倍数;不足一帧的
    /// 余量滞留到下次调用)。单帧处理失败原样透传——宁可留回声也不丢波形。
    pub fn process(&mut self, samples: &[f32]) -> Vec<f32> {
        self.buf.extend_from_slice(samples);
        let full = (self.buf.len() / FRAME) * FRAME;
        let mut out: Vec<f32> = self.buf.drain(..full).collect();
        for chunk in out.chunks_mut(FRAME) {
            if let Err(e) = self.ap.process_capture_frame([&mut chunk[..]]) {
                eprintln!("AEC capture 处理失败(该帧原样透传): {e}");
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 确定性伪随机噪声(LCG),噪声类信号让自适应滤波快速收敛,且测试可复现。
    pub(crate) fn noise(len: usize, seed: &mut u64) -> Vec<f32> {
        (0..len)
            .map(|_| {
                *seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
                ((*seed >> 33) as f32 / (1u64 << 31) as f32) - 0.5
            })
            .collect()
    }

    pub(crate) fn power(s: &[f32]) -> f32 {
        s.iter().map(|x| x * x).sum::<f32>() / s.len() as f32
    }

    #[test]
    fn framing_conserves_samples_in_10ms_multiples() {
        let (_r, mut c) = new_pair(16_000).unwrap();
        // 零散尺寸推入:输出恒为 160 的倍数,余量滞留,总数守恒。
        let mut total_out = 0;
        for n in [100usize, 100, 120, 7, 33] {
            total_out += c.process(&vec![0.0; n]).len();
        }
        let total_in: usize = 100 + 100 + 120 + 7 + 33; // 360
        assert_eq!(total_out % FRAME, 0, "输出恒为整帧倍数");
        assert_eq!(total_out, (total_in / FRAME) * FRAME, "除余量外全部吐出");
    }

    /// 端到端收敛冒烟:近端 = 远端的衰减延迟拷贝(纯回声,无本地人声),
    /// 收敛后输出能量应显著低于原始回声能量。阈值取 6dB(4 倍功率)——AEC3 对
    /// 噪声类信号实测远优于此,宽松阈值防跨平台/版本波动误报。
    #[test]
    fn cancels_delayed_echo_of_render_stream() {
        let (mut r, mut c) = new_pair(16_000).unwrap();
        let mut seed = 42u64;
        let far = noise(16_000 * 4, &mut seed); // 4s 参考噪声
        let delay = 960; // 60ms 外放延迟(AEC3 延迟估计范围内)
        let echo_gain = 0.5f32;

        // 近端 = 远端延迟 60ms × 0.5(纯回声)。
        let mut near = vec![0.0f32; far.len()];
        for i in delay..far.len() {
            near[i] = far[i - delay] * echo_gain;
        }

        // 按 10ms 步进交替喂 render/capture(模拟两路实时到达)。
        let mut out_tail = Vec::new();
        let tail_from = far.len() - 16_000 / 2; // 只评估最后 0.5s(收敛后)
        for (i, (f, n)) in far.chunks(FRAME).zip(near.chunks(FRAME)).enumerate() {
            r.push(f);
            let cleaned = c.process(n);
            if i * FRAME >= tail_from {
                out_tail.extend_from_slice(&cleaned);
            }
        }

        let echo_power = power(&near[tail_from..]);
        let out_power = power(&out_tail);
        assert!(
            out_power < echo_power / 4.0,
            "收敛后回声应至少衰减 6dB: 回声功率 {echo_power:.6}, 输出功率 {out_power:.6}"
        );
    }

    /// 清洗对(AEC3+NS,无AGC):回声照样消,且不做增益(输出功率不该高于输入)。
    #[test]
    fn clean_pair_cancels_echo_without_gain() {
        let (mut r, mut c) = new_clean_pair(16_000).unwrap();
        let mut seed = 42u64;
        let far = noise(16_000 * 4, &mut seed);
        let delay = 960;
        let mut near = vec![0.0f32; far.len()];
        for i in delay..far.len() {
            near[i] = far[i - delay] * 0.5;
        }
        let tail_from = far.len() - 16_000 / 2;
        let mut out_tail = Vec::new();
        for (i, (f, n)) in far.chunks(FRAME).zip(near.chunks(FRAME)).enumerate() {
            r.push(f);
            let cleaned = c.process(n);
            if i * FRAME >= tail_from {
                out_tail.extend_from_slice(&cleaned);
            }
        }
        let echo_power = power(&near[tail_from..]);
        let out_power = power(&out_tail);
        assert!(out_power < echo_power / 4.0, "回声至少衰减 6dB: {echo_power:.6} -> {out_power:.6}");
    }
}

#[cfg(test)]
mod diag_tests {
    use super::*;
    use tests::{noise, power};

    /// 诊断:render 全程静音(远端没人说话)时,近端人声应该原样通过,不得被压制。
    #[test]
    fn near_end_passes_through_when_render_is_silent() {
        let (mut r, mut c) = new_pair(16_000).unwrap();
        let mut seed = 7u64;
        let near = noise(16_000 * 4, &mut seed);
        let silence = vec![0.0f32; near.len()];
        let tail_from = near.len() - 16_000 / 2;
        let mut out_tail = Vec::new();
        for (i, (f, n)) in silence.chunks(FRAME).zip(near.chunks(FRAME)).enumerate() {
            r.push(f);
            let cleaned = c.process(n);
            if i * FRAME >= tail_from {
                out_tail.extend_from_slice(&cleaned);
            }
        }
        let in_p = power(&near[tail_from..]);
        let out_p = power(&out_tail);
        eprintln!("静音参考: 输入功率 {in_p:.6} 输出功率 {out_p:.6} 比值 {:.3}", out_p / in_p);
        assert!(out_p > in_p * 0.25, "近端不应被压超过 6dB: in={in_p:.6} out={out_p:.6}");
    }

    /// AGC2:低电平近端(模拟系统输入音量被拉低)不得被进一步衰减,且应获得增益抬升。
    /// 阈值宽松(≥1.5x 功率)防跨版本波动;实际自适应增益远高于此。
    #[test]
    fn quiet_near_end_gets_boosted_by_agc() {
        let (mut r, mut c) = new_pair(16_000).unwrap();
        let mut seed = 11u64;
        // 0.02 振幅 ≈ 被拉低的近场人声电平;带 4Hz 包络调制,更接近语音的时变特性。
        let near: Vec<f32> = noise(16_000 * 6, &mut seed)
            .iter()
            .enumerate()
            .map(|(i, x)| {
                let t = i as f32 / 16_000.0;
                x * 0.04 * (0.6 + 0.4 * (t * 4.0 * std::f32::consts::TAU).sin())
            })
            .collect();
        let silence = vec![0.0f32; near.len()];
        let tail_from = near.len() - 16_000;
        let mut out_tail = Vec::new();
        for (i, (f, n)) in silence.chunks(FRAME).zip(near.chunks(FRAME)).enumerate() {
            r.push(f);
            let cleaned = c.process(n);
            if i * FRAME >= tail_from {
                out_tail.extend_from_slice(&cleaned);
            }
        }
        let in_p = power(&near[tail_from..]);
        let out_p = power(&out_tail);
        eprintln!("AGC: 输入功率 {in_p:.8} 输出功率 {out_p:.8} 比值 {:.2}", out_p / in_p);
        assert!(out_p > in_p * 1.5, "低电平近端应被 AGC 抬升: in={in_p:.8} out={out_p:.8}");
    }

    /// 诊断:完全不喂 render(系统源无帧)时近端表现。
    #[test]
    fn near_end_without_any_render_frames() {
        let (_r, mut c) = new_pair(16_000).unwrap();
        let mut seed = 9u64;
        let near = noise(16_000 * 4, &mut seed);
        let tail_from = near.len() - 16_000 / 2;
        let mut out_tail = Vec::new();
        for (i, n) in near.chunks(FRAME).enumerate() {
            let cleaned = c.process(n);
            if i * FRAME >= tail_from {
                out_tail.extend_from_slice(&cleaned);
            }
        }
        let in_p = power(&near[tail_from..]);
        let out_p = power(&out_tail);
        eprintln!("无参考帧: 输入功率 {in_p:.6} 输出功率 {out_p:.6} 比值 {:.3}", out_p / in_p);
        assert!(out_p > in_p * 0.25, "近端不应被压超过 6dB: in={in_p:.6} out={out_p:.6}");
    }
}

#[cfg(test)]
mod agc_experiments {
    use super::*;
    use tests::{noise, power};
    use webrtc_audio_processing::{config, Config, Processor};

    fn run_with(cfg: Config, amp: f32) -> f32 {
        let ap = Processor::new(16_000).unwrap();
        ap.set_config(cfg);
        let mut seed = 5u64;
        let near: Vec<f32> = noise(16_000 * 8, &mut seed)
            .iter()
            .enumerate()
            .map(|(i, x)| {
                let t = i as f32 / 16_000.0;
                x * amp * (0.6 + 0.4 * (t * 4.0 * std::f32::consts::TAU).sin())
            })
            .collect();
        let tail = near.len() - 16_000;
        let mut out_tail = Vec::new();
        let mut buf = near.clone();
        for (i, chunk) in buf.chunks_mut(FRAME).enumerate() {
            let _ = ap.process_capture_frame([&mut chunk[..]]);
            if i * FRAME >= tail {
                out_tail.extend_from_slice(chunk);
            }
        }
        power(&out_tail) / power(&near[tail..])
    }


    #[test]
    #[ignore] // 实验用
    fn compare_on_true_whisper_level() {
        let mk2 = |ad: config::AdaptiveDigital| Config {
            gain_controller: Some(config::GainController::GainController2(config::GainController2 {
                input_volume_controller_enabled: false,
                adaptive_digital: Some(ad),
                fixed_digital: config::FixedDigital::default(),
            })),
            ..Default::default()
        };
        let default_ad = config::AdaptiveDigital::default();
        let balanced = config::AdaptiveDigital {
            headroom_db: 3.0,
            max_gain_db: 60.0,
            initial_gain_db: 22.0,
            max_gain_change_db_per_second: 12.0,
            max_output_noise_level_dbfs: -44.0,
        };
        for amp in [0.002f32, 0.005, 0.05, 0.15] {
            eprintln!("amp {amp}: 默认 {:.1}x | balanced {:.1}x", run_with(mk2(default_ad), amp), run_with(mk2(balanced), amp));
        }
    }
    #[test]
    #[ignore] // 实验用:cargo test agc_experiments -- --ignored --nocapture
    fn compare_agc_configs_on_whisper_level() {
        let amp = 0.005f32; // ≈ -46dBFS,实测耳语段电平
        let agc2_default = Config {
            gain_controller: Some(config::GainController::GainController2(config::GainController2 {
                input_volume_controller_enabled: false,
                adaptive_digital: Some(config::AdaptiveDigital::default()),
                fixed_digital: config::FixedDigital::default(),
            })),
            ..Default::default()
        };
        let agc2_aggressive = Config {
            gain_controller: Some(config::GainController::GainController2(config::GainController2 {
                input_volume_controller_enabled: false,
                adaptive_digital: Some(config::AdaptiveDigital {
                    headroom_db: 1.0,
                    max_gain_db: 60.0,
                    initial_gain_db: 30.0,
                    max_gain_change_db_per_second: 20.0,
                    max_output_noise_level_dbfs: -40.0,
                }),
                fixed_digital: config::FixedDigital::default(),
            })),
            ..Default::default()
        };
        let agc1_adaptive = Config {
            gain_controller: Some(config::GainController::GainController1(config::GainController1 {
                mode: config::GainControllerMode::AdaptiveDigital,
                target_level_dbfs: 3,
                compression_gain_db: 20,
                enable_limiter: true,
                analog_gain_controller: None,
            })),
            ..Default::default()
        };
        eprintln!("耳语电平 0.005 增益比(功率):");
        eprintln!("  AGC2 默认   : {:.2}x", run_with(agc2_default, amp));
        eprintln!("  AGC2 激进   : {:.2}x", run_with(agc2_aggressive, amp));
        eprintln!("  AGC1 自适应 : {:.2}x", run_with(agc1_adaptive, amp));
        eprintln!("正常电平 0.05 增益比(不应过度放大):");
        let agc2_aggressive2 = agc2_aggressive.clone();
        eprintln!("  AGC2 激进   : {:.2}x", run_with(agc2_aggressive2, 0.05));
        let agc1_2 = agc1_adaptive.clone();
        eprintln!("  AGC1 自适应 : {:.2}x", run_with(agc1_2, 0.05));
    }
}
