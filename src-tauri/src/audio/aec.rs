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
/// 只开回声消除子模块;AGC/降噪等保持关闭,不引入额外音色变化。
pub fn new_pair(sample_rate: u32) -> anyhow::Result<(AecRender, AecCapture)> {
    let ap = Processor::new(sample_rate).map_err(|e| anyhow::anyhow!("AEC 初始化失败: {e}"))?;
    ap.set_config(Config {
        echo_canceller: Some(config::EchoCanceller::default()),
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
    fn noise(len: usize, seed: &mut u64) -> Vec<f32> {
        (0..len)
            .map(|_| {
                *seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
                ((*seed >> 33) as f32 / (1u64 << 31) as f32) - 0.5
            })
            .collect()
    }

    fn power(s: &[f32]) -> f32 {
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
}
