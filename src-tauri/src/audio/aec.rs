//! 软件回声消除(WebRTC AEC3,bundled 构建)。
//!
//! capture_path=aec(默认路径)下 VPIO(苹果通话模式)不启动,外放回声会原样串进麦克风;
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

/// 最新 AEC3 erle(毫 dB;i32::MIN=尚无读数)。场景传感器跨线程读取。
static LATEST_ERLE_MILLI_DB: std::sync::atomic::AtomicI32 =
    std::sync::atomic::AtomicI32::new(i32::MIN);
fn publish_erle(erle: Option<f64>) {
    let v = erle
        .filter(|x| x.is_finite())
        .map(|x| (x * 1000.0) as i32)
        .unwrap_or(i32::MIN);
    LATEST_ERLE_MILLI_DB.store(v, std::sync::atomic::Ordering::Relaxed);
}
pub fn latest_erle_db() -> Option<f32> {
    let v = LATEST_ERLE_MILLI_DB.load(std::sync::atomic::Ordering::Relaxed);
    (v != i32::MIN).then(|| v as f32 / 1000.0)
}


/// 10ms @ 16kHz。Processor::new(16000) 的帧长与之一致(num_samples_per_frame)。
const FRAME: usize = 160;

/// 生产 AGC2 自适应档参数。抽成一处而不是在 new_pair/new_aligned_pair 各写一遍:
/// 两处本来就必须一致(实时链路只是多开了 NS 和预对齐,增益策略是同一套),
/// 分开写迟早会漂。同时也让测试能直接打这组数——它是听感与削波风险的平衡点,
/// 改动必须有实验支撑。
///
/// `headroom_db = 6`(2026-08-17 从 3 提上来):3dB 余量不够,AGC2 抬完增益后
/// 响峰会顶到 limiter 硬削,产生单样本爆音。真语音实测(system 轨 3 段 × 5 档
/// 输入电平,见 `agc_experiments::headroom_robustness_across_levels_and_segments`):
/// headroom=3 共 87 处削波,headroom=6 共 0 处,而耳语灵敏度未观察到损失
/// (输出 RMS 每一档打印到 5 位小数都相同)。换句话说这 3dB 在这份材料上是白捡的。
///
/// 结论范围(Codex review P2):这是同信号、同参数、只改 headroom 的 A/B,材料是
/// 一场真实会议的 system 轨。它支持"6dB 在该材料上显著降低削波且未见耳语损失",
/// 不宜外推成"任何材料上都零削波"。
///
/// 反面教训记在这里,免得再走一遍:先用**噪声**做的同一组扫描得出「headroom 与
/// max_gain 都无效、只有 initial_gain 有用」的结论,全错——AGC2 的自适应增益由
/// 语音置信度门控,噪声进去 VAD 不认,自适应档根本不启动,量到的永远是
/// initial_gain 的直读(158.5x 恰好=22dB)。调这组参数必须用真语音。
fn production_adaptive_digital() -> config::AdaptiveDigital {
    config::AdaptiveDigital {
        headroom_db: 6.0,
        max_gain_db: 60.0,
        initial_gain_db: 22.0,
        max_gain_change_db_per_second: 12.0,
        max_output_noise_level_dbfs: -44.0,
    }
}

/// 建一对 AEC 句柄(render 给 system worker,capture 给 mic worker)。
/// 开两个子模块:回声消除 + AGC2 自适应数字增益;降噪等保持关闭。
pub fn new_pair(sample_rate: u32) -> anyhow::Result<(AecRender, AecCapture)> {
    let ap = Processor::new(sample_rate).map_err(|e| anyhow::anyhow!("AEC 初始化失败: {e}"))?;
    ap.set_config(Config {
        echo_canceller: Some(config::EchoCanceller::default()),
        // 自动增益(AGC2 自适应数字):普通麦克风模式没有 VPIO 的增益管理,系统输入
        // 音量被会议软件拉低/说话偏轻时波形过小——VAD 概率过不了阈,句子根本不切段
        // (观感"声音小就不转写"),录音回放也听不见。自适应数字增益作用在回声消除
        // 之后。**警示(2026-08-07 实测)**:它的语音检测分不清"残余回声"与"本人轻声
        // 说话",纯远端回声场景会把残余抬回语音电平——理想对齐下消除量从 48.6dB 掉到
        // 20.2dB(diag_agc2_on_residual_echo_level)。因果性修复(签名预对齐)后此
        // 残余是否仍可被 VAD/ASR 捕获,待真机数据评估;若是,AGC2 需要远端活动门控。
        // input_volume_controller 关死:
        // 绝不碰系统输入音量旋钮——那个旋钮会被会议软件抢,抢完不还(2026-07-06
        // 排障实锤),我们只做进程内数字增益,不参与系统层拉锯。
        gain_controller: Some(config::GainController::GainController2(config::GainController2 {
            input_volume_controller_enabled: false,
            // 平衡档:轻声说话要能抬进 VAD 可识别区间,响亮人声不该被推到削波。
            // 默认参数的噪声地板 -50dBFS 会把真耳语当噪声拒掉,故放宽到 -44 并提高
            // max_gain/爬坡速度;底噪随之抬到约 -44dBFS(轻微可闻),VAD/语言过滤/
            // 会后 Aing 三层兜幻觉段。具体数值与依据见 production_adaptive_digital()。
            //
            // 更正(2026-08-17):此处原先写着"真耳语抬 158x 功率"等四电平实验结论,
            // 那组数是用**噪声**测的,而 AGC2 自适应增益由语音置信度门控——噪声进去
            // 自适应档不启动,158x 恰好就是 initial_gain 22dB 的直读,不是自适应行为。
            // 该结论已作废,勿再引用;真语音重测见 agc_experiments 里的 real_speech_*。
            adaptive_digital: Some(production_adaptive_digital()),
            fixed_digital: config::FixedDigital::default(),
        })),
        ..Default::default()
    });
    let ap = Arc::new(ap);
    Ok((
        AecRender { ap: ap.clone(), buf: Vec::new(), align: None },
        AecCapture { ap, buf: Vec::new(), align: None, frames_since_stats: 0 },
    ))
}

/// 二期生产实时对:AEC3 + AGC2(同 new_pair 参数) + NS(Moderate,实时流同时供
/// ASR/声纹,取温和档;离线清洗的 High 档见 new_clean_pair) + 实时预对齐。
/// initial_predelay_ms 由调用方按输出设备给(蓝牙 ≈450,其他 0);之后由
/// AlignState 滑窗实测接管。
pub fn new_aligned_pair(
    sample_rate: u32,
    initial_predelay_ms: u32,
) -> anyhow::Result<(AecRender, AecCapture, Arc<crate::audio::aec_align::AlignState>)> {
    let ap = Processor::new(sample_rate).map_err(|e| anyhow::anyhow!("AEC 初始化失败: {e}"))?;
    ap.set_config(Config {
        echo_canceller: Some(config::EchoCanceller::default()),
        noise_suppression: Some(config::NoiseSuppression {
            level: config::NoiseSuppressionLevel::Moderate,
            ..Default::default()
        }),
        gain_controller: Some(config::GainController::GainController2(config::GainController2 {
            input_volume_controller_enabled: false,
            adaptive_digital: Some(production_adaptive_digital()),
            fixed_digital: config::FixedDigital::default(),
        })),
        ..Default::default()
    });
    let ap = Arc::new(ap);
    let align = crate::audio::aec_align::new(initial_predelay_ms);
    Ok((
        AecRender { ap: ap.clone(), buf: Vec::new(), align: Some(align.clone()) },
        AecCapture { ap, buf: Vec::new(), align: Some(align.clone()), frames_since_stats: 0 },
        align,
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
        AecRender { ap: ap.clone(), buf: Vec::new(), align: None },
        AecCapture { ap, buf: Vec::new(), align: None, frames_since_stats: 0 },
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
    align: Option<Arc<crate::audio::aec_align::AlignState>>,
}

impl AecRender {
    /// 喂入 system 路重采样后的 16k 单声道样本。逐 10ms 帧 analyze;
    /// 失败只打日志(远端分析失败不该影响 system 路自身的转写)。
    /// align 为 Some 时先经预对齐(扣压/放行按实测延迟调节)再分帧喂入。
    pub fn push(&mut self, input: &[f32]) {
        let aligned: Vec<f32>;
        let samples: &[f32] = if let Some(a) = &self.align {
            aligned = a.on_render(input);
            &aligned
        } else {
            input
        };
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
    align: Option<Arc<crate::audio::aec_align::AlignState>>,
    frames_since_stats: usize,
}

/// 每 1000 个 10ms 帧(10s)打一次 AEC3 内部延迟估计观测日志。
const STATS_EVERY_FRAMES: usize = 1000;

impl AecCapture {
    /// 处理 mic 路 16k 单声道样本,返回消回声后的样本(10ms 整帧倍数;不足一帧的
    /// 余量滞留到下次调用)。单帧处理失败原样透传——宁可留回声也不丢波形。
    /// align 为 Some 时样本先过签名预对齐:参考迟到场景下 capture 被扣压若干
    /// 十毫秒(交付推迟、样本域不变),让参考先于回声就位;流结束须调 flush()
    /// 排空扣压尾部,否则丢真实尾音。
    pub fn process(&mut self, samples: &[f32]) -> Vec<f32> {
        let delayed: Vec<f32>;
        let samples: &[f32] = if let Some(a) = &self.align {
            delayed = a.on_capture(samples);
            &delayed
        } else {
            samples
        };
        self.buf.extend_from_slice(samples);
        let full = (self.buf.len() / FRAME) * FRAME;
        let mut out: Vec<f32> = self.buf.drain(..full).collect();
        for chunk in out.chunks_mut(FRAME) {
            if let Err(e) = self.ap.process_capture_frame([&mut chunk[..]]) {
                eprintln!("AEC capture 处理失败(该帧原样透传): {e}");
            }
            if self.align.is_some() {
                self.frames_since_stats += 1;
                if self.frames_since_stats >= STATS_EVERY_FRAMES {
                    self.frames_since_stats = 0;
                    // [诊断插桩 2026-08-07] 加 ERL/ERLE:ERLE 是消除量的直接读数,
                    // 区分"对齐了但没消掉"与"根本没对齐"。
                    let s = self.ap.get_stats();
                    eprintln!(
                        "AEC3 stats: delay {:?}ms erl {:?} erle {:?} (预对齐扣压不计入)",
                        s.delay_ms, s.echo_return_loss, s.echo_return_loss_enhancement
                    );
                    // 场景传感器共享(2026-08-23 一期):最新 erle 以毫 dB 原子量导出,
                    // session 侧判「外放(收敛)vs 同源双路(收不敛)」用。
                    publish_erle(s.echo_return_loss_enhancement);
                }
            }
        }
        out
    }

    /// 流结束:排空签名预对齐在 capture 侧扣压的尾部样本(最多 CAPTURE_MAX_MS),
    /// 过 AEC3 后返回;不足 10ms 整帧的残余以原样附带(≤10ms,与既有"至多丢一帧
    /// 余量"的口径一致但不再丢)。align 为 None 时无扣压,仅返回既有余量语义下
    /// 的空结果(行为不变)。调用方(segment_worker)须把返回样本继续喂给
    /// sink/segmenter——这些是真实 mic 音频,不是可弃的处理残渣。
    pub fn flush(&mut self) -> Vec<f32> {
        let Some(a) = &self.align else {
            return Vec::new();
        };
        let tail = a.flush_capture();
        self.buf.extend_from_slice(&tail);
        let full = (self.buf.len() / FRAME) * FRAME;
        let mut out: Vec<f32> = self.buf.drain(..).collect();
        for chunk in out[..full].chunks_mut(FRAME) {
            if let Err(e) = self.ap.process_capture_frame([&mut chunk[..]]) {
                eprintln!("AEC capture 收尾处理失败(该帧原样透传): {e}");
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

    /// 生产 AGC2 必须留够余量,否则响峰顶到 limiter 硬削 → 单样本爆音。
    ///
    /// 由来(2026-08-17):用户报「偶有尖刺的声响」。实录五场 mic 轨全部有削波
    /// (3.8~137 处/分钟),而同链路未过 AGC 的 system 轨零削波。真语音 A/B 实测
    /// (3 段 × 5 档输入电平)headroom=3 共 87 处削波、headroom=6 共 0 处,
    /// 且耳语灵敏度未观察到损失——这 3dB 是白捡的,不许被调回去。
    ///
    /// 本用例两条腿:①锁住参数值;②用仓库内真语音 fixture 验证它确实换来了
    /// 更低的输出峰值(4.1s 太短,复现不出稀疏的削波事件,但峰值差稳定可测,
    /// 而峰值余量正是"离硬削还有多远"本身)。
    #[test]
    fn production_agc2_keeps_enough_headroom_against_limiter_clipping() {
        let ad = production_adaptive_digital();
        assert!(
            ad.headroom_db >= 6.0,
            "余量被调回 {}dB 会重新出削波爆音(真语音实测 3dB→87 处、6dB→0 处)",
            ad.headroom_db
        );

        let mut rd = hound::WavReader::open(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/sample_zh_16k.wav"
        ))
        .expect("语音 fixture");
        let one: Vec<f32> =
            rd.samples::<i16>().map(|s| s.unwrap() as f32 / 32768.0).collect();
        let src_peak = one.iter().fold(0.0f32, |m, x| m.max(x.abs())).max(1e-9);
        // 重复到约 60s:自适应增益要跑够时间才进稳态,单遍 4.1s 看不出差别。
        let mut sig: Vec<f32> = Vec::new();
        for _ in 0..15 {
            sig.extend(one.iter().map(|x| x * 0.05 / src_peak));
        }
        let peak_with = |ad: config::AdaptiveDigital| -> f32 {
            let ap = Processor::new(16_000).unwrap();
            ap.set_config(Config {
                gain_controller: Some(config::GainController::GainController2(
                    config::GainController2 {
                        input_volume_controller_enabled: false,
                        adaptive_digital: Some(ad),
                        fixed_digital: config::FixedDigital::default(),
                    },
                )),
                ..Default::default()
            });
            let mut buf = sig.clone();
            let mut peak = 0.0f32;
            for chunk in buf.chunks_mut(FRAME) {
                if chunk.len() < FRAME {
                    break;
                }
                let _ = ap.process_capture_frame([&mut chunk[..]]);
                for v in chunk.iter() {
                    peak = peak.max(v.abs());
                }
            }
            peak
        };
        let tight = config::AdaptiveDigital { headroom_db: 3.0, ..production_adaptive_digital() };
        let prod = peak_with(production_adaptive_digital());
        let old = peak_with(tight);
        assert!(
            prod < old * 0.9,
            "生产余量必须换来明显更低的输出峰:生产 {prod:.3} vs 3dB 余量 {old:.3}"
        );
        assert!(prod < 1.0, "生产配置在这段真语音上不得触顶,实测 {prod:.3}");
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

    /// 清洗对无增益判别测试:静音参考下,低电平近端过 clean pair 不得被放大
    /// (对照 diag_tests::quiet_near_end_gets_boosted_by_agc —— 同形输入过
    /// new_pair 会被 AGC2 抬升 >1.5x;本测试若换用 new_pair 必失败,故能
    /// 真正区分两构造器)。NS 只减不增,上限留 1.2x 容差。
    #[test]
    fn clean_pair_applies_no_gain_on_quiet_near_end() {
        let (mut r, mut c) = new_clean_pair(16_000).unwrap();
        let mut seed = 11u64;
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
        assert!(out_p <= in_p * 1.2, "清洗对不得放大近端: in={in_p:.8} out={out_p:.8}");
    }

    /// 二期端到端判别测试:600ms 回声(蓝牙量级,远超 AEC3 内置 ~250ms 估计范围),
    /// aligned pair 从预延迟 0 起步,应在滑窗重估+预对齐后把回声消下去。
    /// 这正是 new_pair 做不到的场景(一期背景:蓝牙外放软件消回声完全失效)。
    ///
    /// 与一期"单抽头冻结"实锤的关系(echo_clean.rs 同名注释):那次冻结在离线
    /// 双 pass 重喂+无 AGC 构型下复现,探针集未含 AGC2;本测试为单次流式+AGC2,
    /// 单抽头实测未冻结(尾段 ~78dB 衰减),机理未定论(AGC2 持续增益扰动或
    /// 单次流式无重喂,皆为候选解释)。刻意保留单抽头:若未来配置变更让冻结在
    /// 此构型复现,本断言当场红——这正是要的哨兵行为,届时按一期方法换 4 抽头
    /// 并记录构型差异。
    #[test]
    fn aligned_pair_cancels_600ms_echo_after_adjustment() {
        use crate::audio::delay_estimate::tests::block_modulated_noise;

        let (mut r, mut c, align) = new_aligned_pair(16_000, 0).unwrap();
        let mut seed = 77u64;
        let far = block_modulated_noise(16_000 * 45, &mut seed); // 45s
        let delay = 9600; // 600ms
        let echo_gain = 0.5f32;
        let mut near = vec![0.0f32; far.len()];
        for i in delay..far.len() {
            near[i] = far[i - delay] * echo_gain;
        }
        let tail_from = far.len() - 16_000 * 5; // 只评估最后 5s(调整+重收敛之后)
        let mut out_tail = Vec::new();
        for (i, (f, n)) in far.chunks(FRAME).zip(near.chunks(FRAME)).enumerate() {
            r.push(f); // align 为 Some 时 push 内部先过 AlignState 再分帧喂入
            let cleaned = c.process(n);
            if i * FRAME >= tail_from {
                out_tail.extend_from_slice(&cleaned);
            }
        }
        // 预对齐应已发生(600-100=500ms 附近)。
        let p = align.predelay_ms() as i64;
        assert!((p - 500).unsigned_abs() <= 60, "预延迟应≈500ms,实际 {p}ms");
        let echo_power = power(&near[tail_from..]);
        let out_power = power(&out_tail);
        assert!(
            out_power < echo_power / 4.0,
            "预对齐后 600ms 回声应至少衰减 6dB: {echo_power:.6} -> {out_power:.6}"
        );
    }
}

#[cfg(test)]
mod diag_tests {
    use super::*;
    use tests::{noise, power};

    /// 修复验证(2026-08-07 根因):参考恒定迟到 200ms(SCK 交付滞后的稳态近似),
    /// 回声声学延迟 30ms——即 mic 回声比参考早 170ms 到达。修复前因果破坏,消除
    /// ≈0(见 diag_reference_arriving_after_capture 的 -9.5dB);修复后 capture 侧
    /// 扣压应恢复因果,收敛尾段至少 6dB。
    #[test]
    fn aligned_pair_cancels_echo_with_late_reference() {
        use crate::audio::delay_estimate::tests::block_modulated_noise;
        let (mut r, mut c, align) = new_aligned_pair(16_000, 0).unwrap();
        let mut seed = 77u64;
        let content = block_modulated_noise(16_000 * 45, &mut seed);
        let echo_delay = 480; // 30ms 声学
        let late = 3200; // 参考交付滞后 200ms
        let echo_gain = 0.5f32;
        // near[i] = content[i-480]*0.5;喂给 render 的流在位置 i 是 content[i-3200]
        let mut near = vec![0.0f32; content.len()];
        for i in echo_delay..content.len() {
            near[i] = content[i - echo_delay] * echo_gain;
        }
        let mut ref_fed = vec![0.0f32; content.len()];
        for i in late..content.len() {
            ref_fed[i] = content[i - late];
        }
        let tail_from = content.len() - 16_000 * 5;
        let mut out_tail = Vec::new();
        for (i, (f, n)) in ref_fed.chunks(FRAME).zip(near.chunks(FRAME)).enumerate() {
            r.push(f);
            let cleaned = c.process(n);
            if i * FRAME >= tail_from {
                out_tail.extend_from_slice(&cleaned);
            }
        }
        // capture 侧应已扣压 ~100+170=270ms
        let p = align.capture_predelay_ms() as i64;
        assert!((p - 270).unsigned_abs() <= 80, "capture 扣压应≈270ms,实际 {p}ms");
        let echo_power = power(&near[tail_from..]);
        let out_power = power(&out_tail);
        assert!(
            out_power < echo_power / 4.0,
            "参考迟到场景修复后应至少衰减 6dB: {echo_power:.6} -> {out_power:.6}"
        );
    }

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

    /// 诊断 H1(2026-08-07 转写重复调查):参考**迟到**——回声延迟 30ms(内置扬声器
    /// 量级),但 render 按 500ms 批次在对应 capture 处理完之后才喂入,模拟"SCK 批量
    /// 交付晚于回声到达"。若消除崩塌 → 因果性破坏假说成立。
    /// 生产一贯失效背景:约 40 场离线清洗 conf 数万~87万,实时 AEC 从未真正工作过。
    #[test]
    fn diag_reference_arriving_after_capture() {
        let (mut r, mut c) = new_pair(16_000).unwrap();
        let mut seed = 42u64;
        let far = noise(16_000 * 8, &mut seed);
        let delay = 480; // 30ms
        let echo_gain = 0.5f32;
        let mut near = vec![0.0f32; far.len()];
        for i in delay..far.len() {
            near[i] = far[i - delay] * echo_gain;
        }
        let burst = 16_000 / 2; // 500ms 批
        let tail_from = far.len() - 16_000; // 评估最后 1s
        let mut out_tail = Vec::new();
        let mut pos = 0usize;
        while pos < far.len() {
            let end = (pos + burst).min(far.len());
            // capture 先处理(回声已在 mic 流里)……
            for (j, n) in near[pos..end].chunks(FRAME).enumerate() {
                let cleaned = c.process(n);
                if pos + j * FRAME >= tail_from {
                    out_tail.extend_from_slice(&cleaned);
                }
            }
            // ……参考随后才批量到达
            for f in far[pos..end].chunks(FRAME) {
                r.push(f);
            }
            pos = end;
        }
        let echo_p = power(&near[tail_from..]);
        let out_p = power(&out_tail);
        let att_db = 10.0 * (echo_p / out_p.max(1e-12)).log10();
        eprintln!("[诊断A] 参考迟到500ms批: 回声功率 {echo_p:.6} 输出 {out_p:.6} 衰减 {att_db:.1}dB");
    }

    /// 诊断 H1 对照:同样 500ms 批量,但参考**先到**(每批先喂 render 再喂 capture)。
    /// 若这组衰减正常而上一组崩塌 → 破坏因素是"迟到",不是"批量"本身。
    #[test]
    fn diag_reference_arriving_before_capture_in_bursts() {
        let (mut r, mut c) = new_pair(16_000).unwrap();
        let mut seed = 42u64;
        let far = noise(16_000 * 8, &mut seed);
        let delay = 480;
        let echo_gain = 0.5f32;
        let mut near = vec![0.0f32; far.len()];
        for i in delay..far.len() {
            near[i] = far[i - delay] * echo_gain;
        }
        let burst = 16_000 / 2;
        let tail_from = far.len() - 16_000;
        let mut out_tail = Vec::new();
        let mut pos = 0usize;
        while pos < far.len() {
            let end = (pos + burst).min(far.len());
            for f in far[pos..end].chunks(FRAME) {
                r.push(f);
            }
            for (j, n) in near[pos..end].chunks(FRAME).enumerate() {
                let cleaned = c.process(n);
                if pos + j * FRAME >= tail_from {
                    out_tail.extend_from_slice(&cleaned);
                }
            }
            pos = end;
        }
        let echo_p = power(&near[tail_from..]);
        let out_p = power(&out_tail);
        let att_db = 10.0 * (echo_p / out_p.max(1e-12)).log10();
        eprintln!("[诊断A对照] 参考先到500ms批: 回声功率 {echo_p:.6} 输出 {out_p:.6} 衰减 {att_db:.1}dB");
    }

    /// 诊断 H2:AGC2 是否把残余回声重新抬回语音电平。纯远端回声(无本地人声),
    /// 严格 10ms 交替喂入(AEC 最佳条件),对比 aligned(带 AGC2)与 clean(无 AGC)
    /// 的输出绝对电平。new_pair 头注声称"AGC2 作用在回声消除之后(不放大回声)"
    /// ——本测试验证这句话。
    #[test]
    fn diag_agc2_on_residual_echo_level() {
        let mut run = |with_agc: bool| -> (f32, f32) {
            let (mut r, mut c) = if with_agc {
                let (r, c, _a) = new_aligned_pair(16_000, 0).unwrap();
                (r, c)
            } else {
                new_clean_pair(16_000).unwrap()
            };
            let mut seed = 42u64;
            let far = noise(16_000 * 10, &mut seed);
            let delay = 480;
            let mut near = vec![0.0f32; far.len()];
            for i in delay..far.len() {
                near[i] = far[i - delay] * 0.5;
            }
            let tail_from = far.len() - 16_000;
            let mut out_tail = Vec::new();
            for (i, (f, n)) in far.chunks(FRAME).zip(near.chunks(FRAME)).enumerate() {
                r.push(f);
                let cleaned = c.process(n);
                if i * FRAME >= tail_from {
                    out_tail.extend_from_slice(&cleaned);
                }
            }
            (power(&near[tail_from..]), power(&out_tail))
        };
        let (echo_p, out_agc) = run(true);
        let (_, out_clean) = run(false);
        let att_agc = 10.0 * (echo_p / out_agc.max(1e-12)).log10();
        let att_clean = 10.0 * (echo_p / out_clean.max(1e-12)).log10();
        eprintln!(
            "[诊断B] 纯回声输入 | 带AGC2: 残余功率 {out_agc:.8} 衰减 {att_agc:.1}dB | 无AGC: 残余功率 {out_clean:.8} 衰减 {att_clean:.1}dB"
        );
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


    /// 故障形态实验(2026-08-17,Codex 复核 P1 指定):现有 agc_experiments 全是
    /// **稳态**电平——恒定振幅跑 8 秒。可 2026-08-17 那场蓝牙录音的形态是
    /// 「长静音 → 突然开口」:49% 的帧低于 −90dBFS(含 14% 是我们补的零),
    /// 语音一来就出现满量程冲激。稳态实验永远看不到这个。
    ///
    /// 本函数造出那个形态并量输出削波:静音段(可选纯零/真实底噪)接一段语音。
    /// 返回(语音段峰值, |x|>=1.0 的样本数)。**只量语音段**——静音段本来就没东西。
    fn peak_and_clips_after_silence(
        cfg: Config,
        silence_secs: f32,
        silence_floor: f32,
        amp: f32,
    ) -> (f32, usize) {
        let ap = Processor::new(16_000).unwrap();
        ap.set_config(cfg);
        let mut seed = 7u64;
        let sil_n = (16_000.0 * silence_secs) as usize / FRAME * FRAME;
        // 静音段:floor==0.0 就是我们补的绝对零;否则是设备真实底噪。
        let mut sig: Vec<f32> = if silence_floor == 0.0 {
            vec![0.0; sil_n]
        } else {
            noise(sil_n, &mut seed).iter().map(|x| x * silence_floor).collect()
        };
        // 语音段:2 秒,带音节包络的噪声(与既有实验同形,便于横向对读)。
        let speech_n = 16_000 * 2;
        let speech: Vec<f32> = noise(speech_n, &mut seed)
            .iter()
            .enumerate()
            .map(|(i, x)| {
                let t = i as f32 / 16_000.0;
                x * amp * (0.6 + 0.4 * (t * 4.0 * std::f32::consts::TAU).sin())
            })
            .collect();
        sig.extend_from_slice(&speech);
        let mut out_speech = Vec::new();
        for (i, chunk) in sig.chunks_mut(FRAME).enumerate() {
            let _ = ap.process_capture_frame([&mut chunk[..]]);
            if i * FRAME >= sil_n {
                out_speech.extend_from_slice(chunk);
            }
        }
        let peak = out_speech.iter().fold(0.0f32, |m, x| m.max(x.abs()));
        let clips = out_speech.iter().filter(|x| x.abs() >= 1.0).count();
        (peak, clips)
    }

    /// 直接引用生产参数,不手抄——手抄的副本迟早和生产漂开,那时实验量的就
    /// 不是生产行为了。
    fn agc2_production() -> Config {
        Config {
            gain_controller: Some(config::GainController::GainController2(config::GainController2 {
                input_volume_controller_enabled: false,
                adaptive_digital: Some(super::production_adaptive_digital()),
                fixed_digital: config::FixedDigital::default(),
            })),
            ..Default::default()
        }
    }

    /// 定论实验:尖刺到底是不是 AGC2 这一级造成的。
    /// 三组对照 × 多个输入电平 × 多个静音时长,每组量语音段的峰值与削波数。
    ///  A 生产 AGC2 + 补零式绝对零静音   ← 被告
    ///  B 生产 AGC2 + 真实底噪静音(−54dBFS) ← 静音"性质"的对照
    ///  C 完全不开 AGC          ← 增益级本身的对照
    /// 判据:若只有 A/B 削波而 C 不削,尖刺就出在 AGC2;若 A 显著重于 B,
    /// 「补零把增益喂上去」这个具体机制才成立(Codex 认为它不成立)。
    #[test]
    #[ignore] // 实验用:cargo test agc_experiments::silence_then_onset -- --ignored --nocapture
    fn silence_then_onset_clipping_ab() {
        let no_agc = || Config { ..Default::default() };
        eprintln!("输入振幅 | 静音时长 | A 生产AGC2+补零零 | B 生产AGC2+底噪 | C 无AGC");
        eprintln!("---------|----------|-------------------|-----------------|--------");
        for amp in [0.01f32, 0.03, 0.05, 0.1, 0.2] {
            for sil in [1.0f32, 5.0, 30.0] {
                let a = peak_and_clips_after_silence(agc2_production(), sil, 0.0, amp);
                let b = peak_and_clips_after_silence(agc2_production(), sil, 0.002, amp);
                let c = peak_and_clips_after_silence(no_agc(), sil, 0.0, amp);
                eprintln!(
                    "{amp:>8.2} | {sil:>7.0}s | 峰{:.3} 削{:>5} | 峰{:.3} 削{:>5} | 峰{:.3} 削{:>5}",
                    a.0, a.1, b.0, b.1, c.0, c.1
                );
            }
        }
    }

    /// 承上:既然削波不是静音驱动的、而是「稳态增益 × 响峰」顶到满量程,
    /// 那修法就该在增益参数上找,而且必须同时守住当初调这组参数的目的——真耳语。
    /// 本实验对每个候选配置同时量两件事:
    ///   1. 响段削波(输入峰 0.1 量级,即实录里出爆音的那档);
    ///   2. 耳语增益(0.002≈−54dBFS 与 0.005≈−46dBFS 的功率增益比)。
    /// 只降削波、把耳语一起降没了的配置不算解。
    #[test]
    #[ignore] // 实验用:cargo test agc_experiments::gain_sweep -- --ignored --nocapture
    fn gain_sweep_clipping_vs_whisper_sensitivity() {
        let mk = |headroom: f32, max_gain: f32, initial: f32, rate: f32| Config {
            gain_controller: Some(config::GainController::GainController2(config::GainController2 {
                input_volume_controller_enabled: false,
                adaptive_digital: Some(config::AdaptiveDigital {
                    headroom_db: headroom,
                    max_gain_db: max_gain,
                    initial_gain_db: initial,
                    max_gain_change_db_per_second: rate,
                    max_output_noise_level_dbfs: -44.0,
                }),
                fixed_digital: config::FixedDigital::default(),
            })),
            ..Default::default()
        };
        let cands: Vec<(&str, Config)> = vec![
            ("生产(3/60/22/12)", mk(3.0, 60.0, 22.0, 12.0)),
            ("headroom 6    ", mk(6.0, 60.0, 22.0, 12.0)),
            ("headroom 9    ", mk(9.0, 60.0, 22.0, 12.0)),
            ("initial 12    ", mk(3.0, 60.0, 12.0, 12.0)),
            ("initial 12+hr6", mk(6.0, 60.0, 12.0, 12.0)),
            ("max_gain 40   ", mk(3.0, 40.0, 22.0, 12.0)),
            ("hr6+init12+40 ", mk(6.0, 40.0, 12.0, 12.0)),
        ];
        eprintln!("配置             | 响段(amp0.2) 峰/削 | 耳语0.002 增益 | 轻声0.005 增益");
        eprintln!("-----------------|--------------------|----------------|---------------");
        for (name, cfg) in cands {
            let loud = peak_and_clips_after_silence(cfg.clone(), 5.0, 0.0, 0.2);
            let w1 = run_with(cfg.clone(), 0.002);
            let w2 = run_with(cfg, 0.005);
            eprintln!(
                "{name} | 峰{:.3} 削{:>5}    | {:>10.1}x   | {:>10.1}x",
                loud.0, loud.1, w1, w2
            );
        }
    }

    /// 上面两个实验有一个共同的硬伤,必须记下来:输入是**噪声**,而 WebRTC AGC2 的
    /// 自适应增益由语音置信度门控——噪声进去 VAD 不认,自适应档根本不启动,量到的
    /// 增益恒等于 `initial_gain_db`(158.5x=22dB、15.8x=12dB,精确到小数点)。
    /// 既有的 compare_on_true_whisper_level 也踩了同一个坑:那个"耳语抬 158x"
    /// 的结论其实只是 initial_gain 的直读,不是自适应行为。
    ///
    /// 本实验换真语音:从 `VN_AGC_WAV` 读一段 16k 单声道 WAV(取 system 轨——真实
    /// 会议人声,且从未经过我们的 AGC),缩放到设备级输入电平后跑三组:
    ///   A 生产 AGC2,信号原样
    ///   B 生产 AGC2,按实录形态挖洞补零(每 2.7s 挖 380ms,≈14% 补零率)
    ///   C 不开 AGC
    /// A vs C 定「削波是不是 AGC2 造成的」;A vs B 定「补零是不是驱动因素」。
    #[test]
    #[ignore] // 实验用:VN_AGC_WAV=/path/speech16k.wav cargo test agc_experiments::real_speech -- --ignored --nocapture
    fn real_speech_agc2_clipping_ab() {
        let Ok(path) = std::env::var("VN_AGC_WAV") else {
            eprintln!("跳过:未设 VN_AGC_WAV(16k 单声道 WAV 路径)");
            return;
        };
        let mut rd = hound::WavReader::open(&path).expect("打不开 WAV");
        assert_eq!(rd.spec().sample_rate, 16_000, "需 16k");
        assert_eq!(rd.spec().channels, 1, "需单声道");
        let raw: Vec<f32> = rd
            .samples::<i16>()
            .map(|s| s.unwrap() as f32 / 32768.0)
            .collect();
        // 只取前 5 分钟,够跑出自适应稳态又不至于太慢。
        let raw: Vec<f32> = raw.into_iter().take(16_000 * 300).collect();
        let src_peak = raw.iter().fold(0.0f32, |m, x| m.max(x.abs()));

        // 把峰缩放到若干档"设备侧输入电平",逐档看什么时候开始削。
        for target_peak in [0.05f32, 0.1, 0.2, 0.4] {
            let k = target_peak / src_peak.max(1e-9);
            let base: Vec<f32> = raw.iter().map(|x| x * k).collect();
            // B:按实录形态挖洞补零——每 2.7 秒挖掉 380ms(实录 514 次洞/1374s、
            // 平均 378ms、合计 14.2%)。挖的是内容,补的是零,与生产链路同形。
            let mut holed = base.clone();
            let period = (16_000.0 * 2.7) as usize;
            let hole = (16_000.0 * 0.38) as usize;
            let mut p = period;
            while p + hole < holed.len() {
                for v in &mut holed[p..p + hole] {
                    *v = 0.0;
                }
                p += period;
            }
            let run = |cfg: Config, sig: &[f32]| -> (f32, usize) {
                let ap = Processor::new(16_000).unwrap();
                ap.set_config(cfg);
                let mut buf = sig.to_vec();
                let mut peak = 0.0f32;
                let mut clips = 0usize;
                for chunk in buf.chunks_mut(FRAME) {
                    if chunk.len() < FRAME {
                        break;
                    }
                    let _ = ap.process_capture_frame([&mut chunk[..]]);
                    for v in chunk.iter() {
                        peak = peak.max(v.abs());
                        if v.abs() >= 1.0 {
                            clips += 1;
                        }
                    }
                }
                (peak, clips)
            };
            let a = run(agc2_production(), &base);
            let b = run(agc2_production(), &holed);
            let c = run(Config { ..Default::default() }, &base);
            eprintln!(
                "输入峰 {target_peak:.2} | A 生产AGC2 峰{:.3} 削{:>6} | B +补零洞 峰{:.3} 削{:>6} | C 无AGC 峰{:.3} 削{:>6}",
                a.0, a.1, b.0, b.1, c.0, c.1
            );
        }
    }

    /// 真语音下的参数扫描。噪声版的 gain_sweep 结论作废(自适应档没启动),
    /// 这里用真人声重做,并同时守住耳语灵敏度——只降削波、把耳语一起降没的不算解。
    /// 耳语指标改用「把真语音缩到 −54dBFS 峰值后的输出 RMS」,比噪声版可信。
    #[test]
    #[ignore] // 实验用:VN_AGC_WAV=... cargo test agc_experiments::real_speech_gain_sweep -- --ignored --nocapture
    fn real_speech_gain_sweep() {
        let Ok(path) = std::env::var("VN_AGC_WAV") else {
            eprintln!("跳过:未设 VN_AGC_WAV");
            return;
        };
        let mut rd = hound::WavReader::open(&path).unwrap();
        // 同 real_speech_agc2_clipping_ab:固定按 16k 单声道解释就必须先验
        // (Codex 二轮 P2:这一处上轮漏了)。
        assert_eq!(rd.spec().sample_rate, 16_000, "VN_AGC_WAV 必须是 16kHz");
        assert_eq!(rd.spec().channels, 1, "VN_AGC_WAV 必须是单声道");
        let raw: Vec<f32> = rd
            .samples::<i16>()
            .map(|s| s.unwrap() as f32 / 32768.0)
            // 300s:削波事件很稀疏且集中在个别响段,180s 那截根本不出削波,
            // 拿它做扫描会得出"所有配置都不削"的假结论。
            .take(16_000 * 300)
            .collect();
        let src_peak = raw.iter().fold(0.0f32, |m, x| m.max(x.abs()));
        let scaled = |peak: f32| -> Vec<f32> {
            let k = peak / src_peak.max(1e-9);
            raw.iter().map(|x| x * k).collect()
        };
        let run = |cfg: Config, sig: &[f32]| -> (f32, usize, f32) {
            let ap = Processor::new(16_000).unwrap();
            ap.set_config(cfg);
            let mut buf = sig.to_vec();
            let (mut peak, mut clips, mut sq, mut n) = (0.0f32, 0usize, 0.0f64, 0usize);
            for chunk in buf.chunks_mut(FRAME) {
                if chunk.len() < FRAME {
                    break;
                }
                let _ = ap.process_capture_frame([&mut chunk[..]]);
                for v in chunk.iter() {
                    peak = peak.max(v.abs());
                    if v.abs() >= 1.0 {
                        clips += 1;
                    }
                    sq += (*v as f64) * (*v as f64);
                    n += 1;
                }
            }
            (peak, clips, (sq / n.max(1) as f64).sqrt() as f32)
        };
        let mk = |hr: f32, mg: f32, ig: f32, rate: f32| Config {
            gain_controller: Some(config::GainController::GainController2(config::GainController2 {
                input_volume_controller_enabled: false,
                adaptive_digital: Some(config::AdaptiveDigital {
                    headroom_db: hr,
                    max_gain_db: mg,
                    initial_gain_db: ig,
                    max_gain_change_db_per_second: rate,
                    max_output_noise_level_dbfs: -44.0,
                }),
                fixed_digital: config::FixedDigital::default(),
            })),
            ..Default::default()
        };
        let normal = scaled(0.05); // 实录里出爆音的那档输入电平(见 real_speech_agc2_clipping_ab)
        let whisper = scaled(0.002); // 真耳语 ≈ −54dBFS 峰
        eprintln!("配置              | 正常(峰0.15) 峰/削 | 耳语(峰0.002) 输出RMS");
        eprintln!("------------------|--------------------|----------------------");
        for (name, cfg) in [
            ("生产 3/60/22/12  ", mk(3.0, 60.0, 22.0, 12.0)),
            ("headroom 6       ", mk(6.0, 60.0, 22.0, 12.0)),
            ("headroom 12      ", mk(12.0, 60.0, 22.0, 12.0)),
            ("max_gain 30      ", mk(3.0, 30.0, 22.0, 12.0)),
            ("max_gain 20      ", mk(3.0, 20.0, 22.0, 12.0)),
            ("max_gain 12      ", mk(3.0, 12.0, 22.0, 12.0)),
            ("initial 12       ", mk(3.0, 60.0, 12.0, 12.0)),
            ("hr12+max20       ", mk(12.0, 20.0, 22.0, 12.0)),
            ("无 AGC(对照)     ", Config { ..Default::default() }),
        ] {
            let a = run(cfg.clone(), &normal);
            let w = run(cfg, &whisper);
            eprintln!(
                "{name} | 峰{:.3} 削{:>6}    | {:.5}",
                a.0, a.1, w.2
            );
        }
    }

    /// 稳健性验证:headroom 3→6 消掉削波这件事,不能只在某一个输入电平/某一段音频
    /// 上成立。这里跨 5 档输入电平 × 3 段不同音频(前/中/后各 4 分钟)复核,并同时
    /// 盯住耳语灵敏度不掉。改生产参数之前必须过这一关。
    #[test]
    #[ignore] // 实验用:VN_AGC_WAV=... cargo test agc_experiments::headroom_robustness -- --ignored --nocapture
    fn headroom_robustness_across_levels_and_segments() {
        let Ok(path) = std::env::var("VN_AGC_WAV") else {
            eprintln!("跳过:未设 VN_AGC_WAV");
            return;
        };
        let mut rd = hound::WavReader::open(&path).unwrap();
        // 固定按 16k 单声道解释,就必须先验(Codex review P2):喂错格式会静默算出
        // 一组看着像样、实则无意义的数,而这组数是要拿来定生产参数的。
        assert_eq!(rd.spec().sample_rate, 16_000, "VN_AGC_WAV 必须是 16kHz");
        assert_eq!(rd.spec().channels, 1, "VN_AGC_WAV 必须是单声道");
        let all: Vec<f32> = rd
            .samples::<i16>()
            .map(|s| s.unwrap() as f32 / 32768.0)
            .collect();
        let seg_len = 16_000 * 240;
        let segs: Vec<(&str, &[f32])> = vec![
            ("前段", &all[..seg_len.min(all.len())]),
            ("中段", &all[all.len() / 2..(all.len() / 2 + seg_len).min(all.len())]),
            ("后段", &all[all.len().saturating_sub(seg_len)..]),
        ];
        let mk = |hr: f32| Config {
            gain_controller: Some(config::GainController::GainController2(config::GainController2 {
                input_volume_controller_enabled: false,
                adaptive_digital: Some(config::AdaptiveDigital {
                    headroom_db: hr,
                    max_gain_db: 60.0,
                    initial_gain_db: 22.0,
                    max_gain_change_db_per_second: 12.0,
                    max_output_noise_level_dbfs: -44.0,
                }),
                fixed_digital: config::FixedDigital::default(),
            })),
            ..Default::default()
        };
        let run = |cfg: Config, sig: &[f32], peak_to: f32| -> (f32, usize, f32) {
            let sp = sig.iter().fold(0.0f32, |m, x| m.max(x.abs())).max(1e-9);
            let k = peak_to / sp;
            let ap = Processor::new(16_000).unwrap();
            ap.set_config(cfg);
            let mut buf: Vec<f32> = sig.iter().map(|x| x * k).collect();
            let (mut peak, mut clips, mut sq, mut n) = (0.0f32, 0usize, 0.0f64, 0usize);
            for chunk in buf.chunks_mut(FRAME) {
                if chunk.len() < FRAME {
                    break;
                }
                let _ = ap.process_capture_frame([&mut chunk[..]]);
                for v in chunk.iter() {
                    peak = peak.max(v.abs());
                    if v.abs() >= 1.0 {
                        clips += 1;
                    }
                    sq += (*v as f64) * (*v as f64);
                    n += 1;
                }
            }
            (peak, clips, (sq / n.max(1) as f64).sqrt() as f32)
        };
        eprintln!("段   输入峰 | hr=3 峰/削      | hr=6 峰/削      | 耳语RMS hr3 → hr6");
        eprintln!("-----------|-----------------|-----------------|-------------------");
        let (mut c3, mut c6) = (0usize, 0usize);
        for (sname, sig) in &segs {
            for lvl in [0.02f32, 0.05, 0.1, 0.2, 0.4] {
                let a = run(mk(3.0), sig, lvl);
                let b = run(mk(6.0), sig, lvl);
                c3 += a.1;
                c6 += b.1;
                let w3 = run(mk(3.0), sig, 0.002).2;
                let w6 = run(mk(6.0), sig, 0.002).2;
                eprintln!(
                    "{sname} {lvl:>6.2} | 峰{:.3} 削{:>5} | 峰{:.3} 削{:>5} | {:.5} → {:.5}",
                    a.0, a.1, b.0, b.1, w3, w6
                );
            }
        }
        eprintln!("合计削波: hr=3 共 {c3},hr=6 共 {c6}");
        // 只打印不断言的话,这个实验将来跑出相反结论也没人会发现(Codex review P2)。
        assert!(c3 > 0, "前提:这段材料必须能在 3dB 余量下复现削波,否则实验无效");
        assert_eq!(c6, 0, "6dB 余量在这段材料上应当零削波,实测 {c6}");
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
        // 历史基线(headroom 3),保持原值以免这个老实验的读数变了意思。
        // 注意:本实验用噪声驱动,自适应档不启动,结论已作废——见
        // production_adaptive_digital() 的说明,勿再引用其数字。
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
