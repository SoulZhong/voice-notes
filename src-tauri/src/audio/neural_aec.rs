//! DTLN-aec 神经残余级：双阶段模型加载 + 完整块循环推理（P3b T1 可行性硬闸 + T2 推理引擎）。
//!
//! **路线定版：ONNX + tract-onnx（纯 Rust 运行时）。** 原计划的两条 tflite 路线硬闸双败
//! （tract-tflite 0.22/0.23 反序列化负轴 `reduce_mean(axis=-1)` 时未做回绕直接 `as usize`
//! 越界 panic;tflitec 需 bazel 对 TensorFlow 全量源码整编,本机被 GitHub 429 限流且该
//! 构建链路对桌面应用不可维护），详见 `.superpowers/sdd/p3b-task-1-report.md` 偏差记录。
//! 官方 tflite 模型已离线转换为 ONNX（tf2onnx --opset 13,随机输入数值最大差 ~1e-6），
//! 工件哈希与下载 URL 钉死在计划 Global Constraints。
//!
//! 公开入口 [`suppress_residual`]：内部 `load` 一次并复用于整段音频的完整块循环
//! （全文见 `docs/superpowers/plans/2026-07-14-voice-notes-neural-residual-aec.md`
//! 「官方块算法」一节，T2 在此基础上补外层 pad/512 滑窗缓冲/overlap-add/状态回喂）：
//! ```text
//! 两端各 pad 384(=block_len-block_shift)个零;128 采样滑窗逐块:
//!   mag_mic = |rfft(in_buffer)|; mag_lpb = |rfft(in_buffer_lpb)|
//!   (mask, states_1') = model1(mag_mic, states_1, mag_lpb)
//!   est = irfft(rfft(in_buffer) * mask) / 512   ← realfft 的 C2R 不含 1/N，需手工补
//!   (out_block, states_2') = model2(est, states_2, in_buffer_lpb)
//!   out_buffer 左移 128 尾部清零、+= out_block;收集 out_buffer[..128]
//! 去 pad 后截到原长;states_1/states_2 逐块回喂(不复位)。
//! ```
//! 张量绑定**按名字映射,不赌下标**（tf2onnx 保留 tflite 输入名，且名字顺序与语义顺序
//! 不一致）：模型1 输入 input_3/input_5/input_4 = mag/states/lpb_mag；模型2 输入
//! input_6/input_8/input_7 = est/states/lpb。加载时 eprintln 双模型全部输入/输出的
//! name/shape（T1 遗留调试输出，T2 沿用同一映射）。
//!
//! **负发现（T2 实测,勿再踩）**：预训练模型对宽带调制噪声"回声"零响应——用
//! `block_modulated_noise` 构造 mic=reference×0.3 的纯回声,任意延迟（含 0,即逐样本
//! 严格成比例）抑制均仅 -0.01~-0.73dB。这是语音域模型的特性而非实现缺陷：同一实现对
//! 官方仓库的真实远端单讲回声样本压 -49.8dB,与官方 python+tflite 参考实现（-49.7dB）
//! 一致到 0.1dB 内。因此**合成噪声不可作本模块的回声测试信号**,回声衰减测试改用真实
//! 样本交叉验证（见 `suppresses_real_echo_sample`）;真实录音上的端到端效力由
//! spike（残余互相关 0.289→0.116）与 T5 验收兜底。

use std::path::Path;

use anyhow::{ensure, Context, Result};
use realfft::RealFftPlanner;
use tract_onnx::prelude::*;

const BLOCK_LEN: usize = 512;
const FREQ_BINS: usize = BLOCK_LEN / 2 + 1;
const BLOCK_SHIFT: usize = 128;
/// 两端 pad 长度：block_len - block_shift。
const PAD: usize = BLOCK_LEN - BLOCK_SHIFT;

/// 单个 ONNX 模型的可运行 Plan + 输入插槽映射（按名字定位一次，块间复用）。
struct StageModel {
    plan: TypedRunnableModel<TypedModel>,
    /// 主输入插槽下标（模型1: mag_mic；模型2: est）。
    in_primary: usize,
    /// 状态输入插槽下标。
    in_state: usize,
    /// 副输入插槽下标（模型1: mag_lpb；模型2: lpb_time）。
    in_secondary: usize,
    /// 主输出下标（模型1: mask；模型2: out_block）；状态输出为另一个（rank=4）。
    out_primary: usize,
    /// 主输入张量形状（取自模型声明）。
    primary_shape: Vec<usize>,
    /// 副输入张量形状。
    secondary_shape: Vec<usize>,
    /// 状态张量形状，全零初始化用。
    state_shape: Vec<usize>,
}

pub struct DtlnAec {
    model1: StageModel,
    model2: StageModel,
}

/// TDim 形状转定长 usize。注意 tract 0.21 的 TDim 没有 to_usize,走 to_i64。
fn shape_to_usize(shape: &ShapeFact) -> Result<Vec<usize>> {
    shape
        .iter()
        .map(|d| {
            d.to_i64()
                .map(|v| v as usize)
                .with_context(|| format!("张量维度含符号,无法定为定长: {d:?}"))
        })
        .collect()
}

/// 打印一个已优化 TypedModel 全部输入/输出的 name/shape/dtype（T2 依赖这份 dump 做名字映射）。
fn dump_io(label: &str, model: &TypedModel) -> Result<()> {
    for (i, outlet) in model.input_outlets()?.iter().enumerate() {
        let name = &model.node(outlet.node).name;
        let fact = model.outlet_fact(*outlet)?;
        eprintln!(
            "[neural_aec] {label} in[{i}] name={name:?} shape={:?} dt={:?}",
            fact.shape, fact.datum_type
        );
    }
    for (i, outlet) in model.output_outlets()?.iter().enumerate() {
        let name = model
            .outlet_label(*outlet)
            .map(|s| s.to_string())
            .unwrap_or_else(|| model.node(outlet.node).name.clone());
        let fact = model.outlet_fact(*outlet)?;
        eprintln!(
            "[neural_aec] {label} out[{i}] name={name:?} shape={:?} dt={:?}",
            fact.shape, fact.datum_type
        );
    }
    Ok(())
}

/// 按输入节点名找插槽下标；找不到时报错并列出实际名字（防上游重导出换名后静默错绑）。
fn input_slot_by_name(label: &str, model: &TypedModel, name: &str) -> Result<usize> {
    let outlets = model.input_outlets()?;
    outlets
        .iter()
        .position(|o| model.node(o.node).name == name)
        .with_context(|| {
            let actual: Vec<String> =
                outlets.iter().map(|o| model.node(o.node).name.clone()).collect();
            format!("{label}: 未找到输入张量 {name:?},实际输入名: {actual:?}")
        })
}

/// 加载一个阶段模型，输入按名字三元组 (primary, state, secondary) 绑定插槽。
fn load_stage(path: &Path, label: &str, names: (&str, &str, &str)) -> Result<StageModel> {
    ensure!(
        path.is_file(),
        "模型文件缺失: {path:?}（期望 DTLN-aec 256 档 ONNX 转换工件）"
    );
    let typed = tract_onnx::onnx()
        .model_for_path(path)
        .with_context(|| format!("加载 ONNX 模型失败: {path:?}"))?
        .into_optimized()
        .with_context(|| format!("优化 ONNX 模型失败: {path:?}"))?;

    dump_io(label, &typed)?;

    let (primary_name, state_name, secondary_name) = names;
    let in_primary = input_slot_by_name(label, &typed, primary_name)?;
    let in_state = input_slot_by_name(label, &typed, state_name)?;
    let in_secondary = input_slot_by_name(label, &typed, secondary_name)?;

    let in_outlets = typed.input_outlets()?.to_vec();
    ensure!(in_outlets.len() == 3, "{label}: 期望 3 个输入,实际 {}", in_outlets.len());
    let primary_shape = shape_to_usize(&typed.outlet_fact(in_outlets[in_primary])?.shape)?;
    let state_shape = shape_to_usize(&typed.outlet_fact(in_outlets[in_state])?.shape)?;
    let secondary_shape = shape_to_usize(&typed.outlet_fact(in_outlets[in_secondary])?.shape)?;
    ensure!(
        state_shape.len() == 4,
        "{label}: 状态输入 {state_name:?} 应为 rank=4,实际形状 {state_shape:?}"
    );

    // 输出：状态输出 rank=4，另一个为主输出（mask / out_block）。
    let out_outlets = typed.output_outlets()?.to_vec();
    ensure!(out_outlets.len() == 2, "{label}: 期望 2 个输出,实际 {}", out_outlets.len());
    let mut out_primary = None;
    for (i, outlet) in out_outlets.iter().enumerate() {
        if typed.outlet_fact(*outlet)?.shape.rank() != 4 {
            ensure!(out_primary.is_none(), "{label}: 发现多个非状态输出");
            out_primary = Some(i);
        }
    }
    let out_primary = out_primary.with_context(|| format!("{label}: 未找到主输出(非 rank=4)"))?;

    ensure!(
        in_primary != in_state && in_primary != in_secondary && in_state != in_secondary,
        "{label}: 三个输入名字应互不相同,实际映射到相同插槽(primary={in_primary}, state={in_state}, secondary={in_secondary})"
    );

    let plan = typed
        .into_runnable()
        .with_context(|| format!("构建可运行 Plan 失败: {path:?}"))?;

    Ok(StageModel {
        plan,
        in_primary,
        in_state,
        in_secondary,
        out_primary,
        primary_shape,
        secondary_shape,
        state_shape,
    })
}

/// 加载 DTLN-aec 256 档双模型（`dtln_aec_256_1.onnx` / `_2.onnx`）。缺文件时报错带路径，不 panic。
pub fn load(models_dir: &Path) -> Result<DtlnAec> {
    let p1 = models_dir.join("dtln_aec_256_1.onnx");
    let p2 = models_dir.join("dtln_aec_256_2.onnx");
    // 名字映射见模块头注释:tf2onnx 保留的 tflite 输入名,名字序与语义序不一致,勿赌下标。
    let model1 = load_stage(&p1, "model1", ("input_3", "input_5", "input_4"))?;
    let model2 = load_stage(&p2, "model2", ("input_6", "input_8", "input_7"))?;
    Ok(DtlnAec { model1, model2 })
}

impl StageModel {
    /// 按插槽下标组装 (primary, state, secondary) 三输入并前向，返回 (主输出, 回填状态)。
    /// 状态输出需由调用方逐块带回下一次调用的 `state` 参数（overlap-add 状态回喂）。
    fn run(&self, primary: &[f32], state: &[f32], secondary: &[f32]) -> Result<(Vec<f32>, Vec<f32>)> {
        let mut slots: [Option<TValue>; 3] = [None, None, None];
        slots[self.in_primary] = Some(Tensor::from_shape(&self.primary_shape, primary)?.into());
        slots[self.in_state] = Some(Tensor::from_shape(&self.state_shape, state)?.into());
        slots[self.in_secondary] =
            Some(Tensor::from_shape(&self.secondary_shape, secondary)?.into());
        let mut inputs: TVec<TValue> = tvec!();
        for (i, v) in slots.into_iter().enumerate() {
            inputs.push(v.with_context(|| {
                format!(
                    "输入插槽 {i} 未填充(load 阶段应已保证 in_primary/in_state/in_secondary 互不相同)"
                )
            })?);
        }
        let outputs = self.plan.run(inputs).context("模型前向推理失败")?;
        let primary_out = outputs[self.out_primary]
            .as_slice::<f32>()
            .context("主输出读取失败")?
            .to_vec();
        // 只有两个输出(load 阶段已 ensure),状态输出即另一个下标。
        let state_out_idx = 1 - self.out_primary;
        let state_out = outputs[state_out_idx]
            .as_slice::<f32>()
            .context("状态输出读取失败")?
            .to_vec();
        Ok((primary_out, state_out))
    }

    fn zero_state(&self) -> Vec<f32> {
        vec![0.0f32; self.state_shape.iter().product()]
    }
}

impl DtlnAec {
    /// T1/T2 共用的单块前向探针：对齐「官方块算法」一节。输入/输出均为定长 512 样本块。
    /// T1 阶段：mic_block/lpb_block 直接视为待变换的 512 长缓冲（多块滑窗与状态回喂留给 T2）。
    pub(crate) fn run_block_for_test(
        &self,
        mic_block: &[f32; BLOCK_LEN],
        lpb_block: &[f32; BLOCK_LEN],
    ) -> Result<Vec<f32>> {
        let mut planner = RealFftPlanner::<f32>::new();
        let r2c = planner.plan_fft_forward(BLOCK_LEN);

        let mut mic_in = mic_block.to_vec();
        let mut fft_mic = r2c.make_output_vec();
        r2c.process(&mut mic_in, &mut fft_mic).context("mic rfft 失败")?;
        let mag_mic: Vec<f32> = fft_mic.iter().map(|c| c.norm()).collect();

        let mut lpb_in = lpb_block.to_vec();
        let mut fft_lpb = r2c.make_output_vec();
        r2c.process(&mut lpb_in, &mut fft_lpb).context("lpb rfft 失败")?;
        let mag_lpb: Vec<f32> = fft_lpb.iter().map(|c| c.norm()).collect();
        ensure!(mag_mic.len() == FREQ_BINS, "rfft 输出 bin 数不符预期");

        // 模型1:mag_mic + states_1 + mag_lpb -> mask
        let (mask, _states_1) = self
            .model1
            .run(&mag_mic, &self.model1.zero_state(), &mag_lpb)
            .context("模型1(mask 估计)失败")?;
        ensure!(mask.len() == FREQ_BINS, "mask 长度应为 {FREQ_BINS},实际 {}", mask.len());

        // est = irfft(fft_mic * mask) / 512（realfft C2R 不含 1/N 归一）
        let mut masked: Vec<realfft::num_complex::Complex<f32>> =
            fft_mic.iter().zip(mask.iter()).map(|(c, m)| c * m).collect();
        let c2r = planner.plan_fft_inverse(BLOCK_LEN);
        let mut est = c2r.make_output_vec();
        c2r.process(&mut masked, &mut est).context("irfft 逆变换失败")?;
        for v in est.iter_mut() {
            *v /= BLOCK_LEN as f32;
        }

        // 模型2:est + states_2 + lpb_time -> out_block
        let (out_block, _states_2) = self
            .model2
            .run(&est, &self.model2.zero_state(), lpb_block)
            .context("模型2(时域增强)失败")?;
        ensure!(
            out_block.len() == BLOCK_LEN,
            "输出块长度应为 {BLOCK_LEN},实际 {}",
            out_block.len()
        );

        Ok(out_block)
    }

    /// 完整块循环:两端各 pad `PAD`(384)零,128 采样滑窗,逐块 rfft→模型1→mask→irfft/512→
    /// 模型2→overlap-add,状态张量逐块回喂。见模块头注释「官方块算法」。
    /// 输出长度恒等于 `mic.len()`;`reference` 短则零垫齐、长则截断到 `mic.len()`。
    fn process(&self, mic: &[f32], reference: &[f32]) -> Result<Vec<f32>> {
        let len_audio = mic.len();

        // reference 对齐 mic 长度:短则零垫齐,长则截断。
        let mut lpb = reference.to_vec();
        lpb.resize(len_audio, 0.0);

        // 右侧额外补零到 BLOCK_SHIFT 整数倍,保证滑窗块数足以覆盖 PAD+len_audio,
        // 否则末尾非 128 整数倍的样本会在整除取块数时被悄悄丢弃(样本守恒要求见 brief)。
        let tail_extra = (BLOCK_SHIFT - (len_audio % BLOCK_SHIFT)) % BLOCK_SHIFT;
        let padded_len = PAD + len_audio + PAD + tail_extra;

        let mut mic_padded = vec![0.0f32; padded_len];
        mic_padded[PAD..PAD + len_audio].copy_from_slice(mic);
        let mut lpb_padded = vec![0.0f32; padded_len];
        lpb_padded[PAD..PAD + len_audio].copy_from_slice(&lpb);

        let num_blocks = (padded_len - PAD) / BLOCK_SHIFT;

        let mut in_buffer = vec![0.0f32; BLOCK_LEN];
        let mut in_buffer_lpb = vec![0.0f32; BLOCK_LEN];
        let mut out_buffer = vec![0.0f32; BLOCK_LEN];
        let mut states_1 = self.model1.zero_state();
        let mut states_2 = self.model2.zero_state();

        let mut planner = RealFftPlanner::<f32>::new();
        let r2c = planner.plan_fft_forward(BLOCK_LEN);
        let c2r = planner.plan_fft_inverse(BLOCK_LEN);

        let mut collected = Vec::with_capacity(num_blocks * BLOCK_SHIFT);

        for idx in 0..num_blocks {
            let start = idx * BLOCK_SHIFT;

            in_buffer.copy_within(BLOCK_SHIFT.., 0);
            in_buffer[BLOCK_LEN - BLOCK_SHIFT..]
                .copy_from_slice(&mic_padded[start..start + BLOCK_SHIFT]);
            in_buffer_lpb.copy_within(BLOCK_SHIFT.., 0);
            in_buffer_lpb[BLOCK_LEN - BLOCK_SHIFT..]
                .copy_from_slice(&lpb_padded[start..start + BLOCK_SHIFT]);

            let mut mic_in = in_buffer.clone();
            let mut fft_mic = r2c.make_output_vec();
            r2c.process(&mut mic_in, &mut fft_mic)
                .with_context(|| format!("mic rfft 失败(块 {idx})"))?;
            let mag_mic: Vec<f32> = fft_mic.iter().map(|c| c.norm()).collect();

            let mut lpb_in = in_buffer_lpb.clone();
            let mut fft_lpb = r2c.make_output_vec();
            r2c.process(&mut lpb_in, &mut fft_lpb)
                .with_context(|| format!("lpb rfft 失败(块 {idx})"))?;
            let mag_lpb: Vec<f32> = fft_lpb.iter().map(|c| c.norm()).collect();

            let (mask, new_states_1) = self
                .model1
                .run(&mag_mic, &states_1, &mag_lpb)
                .with_context(|| format!("模型1(mask 估计)失败(块 {idx})"))?;
            states_1 = new_states_1;

            // est = irfft(fft_mic * mask) / 512(realfft C2R 不含 1/N 归一)
            let mut masked: Vec<realfft::num_complex::Complex<f32>> =
                fft_mic.iter().zip(mask.iter()).map(|(c, m)| c * m).collect();
            let mut est = c2r.make_output_vec();
            c2r.process(&mut masked, &mut est)
                .with_context(|| format!("irfft 逆变换失败(块 {idx})"))?;
            for v in est.iter_mut() {
                *v /= BLOCK_LEN as f32;
            }

            let (out_block, new_states_2) = self
                .model2
                .run(&est, &states_2, &in_buffer_lpb)
                .with_context(|| format!("模型2(时域增强)失败(块 {idx})"))?;
            states_2 = new_states_2;

            out_buffer.copy_within(BLOCK_SHIFT.., 0);
            for v in out_buffer[BLOCK_LEN - BLOCK_SHIFT..].iter_mut() {
                *v = 0.0;
            }
            for (o, b) in out_buffer.iter_mut().zip(out_block.iter()) {
                *o += b;
            }

            collected.extend_from_slice(&out_buffer[..BLOCK_SHIFT]);
        }

        ensure!(
            collected.len() >= PAD + len_audio,
            "内部一致性错误:收集样本数 {} 不足以覆盖 PAD+len_audio {}",
            collected.len(),
            PAD + len_audio
        );
        Ok(collected[PAD..PAD + len_audio].to_vec())
    }
}

/// 神经残余级完整入口:每次调用内部 `load` 一次并复用于整段音频的块循环
/// (45 分钟音频约 34 万块,plan/模型只建一次)。输出长度恒等于 `mic.len()`;
/// `reference` 短则零垫齐、长则截断到 `mic.len()`。
pub fn suppress_residual(mic: &[f32], reference: &[f32], models_dir: &Path) -> Result<Vec<f32>> {
    let aec = load(models_dir)?;
    aec.process(mic, reference)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 可行性硬闸：tract-onnx 能加载转换后双模型并对全零输入出数。
    /// 运行: VN_DTLN_DIR=<模型目录> cargo test neural_aec -- --ignored --nocapture
    #[test]
    #[ignore]
    fn models_load_and_run_one_block() {
        let dir = std::path::PathBuf::from(std::env::var("VN_DTLN_DIR").expect("需设 VN_DTLN_DIR"));
        let aec = load(&dir).expect("双模型应可加载");
        let out = aec
            .run_block_for_test(&[0.0f32; 512], &[0.0f32; 512])
            .expect("全零单块应出数");
        assert_eq!(out.len(), 512, "块输出长度");
        assert!(out.iter().all(|v| v.is_finite()), "输出应全有限");
    }

    /// 无模型目录：load 报错带路径，不 panic。
    #[test]
    fn load_missing_models_errs_gracefully() {
        let e = match load(std::path::Path::new("/nonexistent-vn-dtln")) {
            Ok(_) => panic!("缺模型目录应报错"),
            Err(e) => e,
        };
        assert!(format!("{e:#}").contains("dtln_aec_256"), "错误应点名缺失文件");
    }

    /// 从 16-bit PCM wav 读单声道 f32 样本(测试样本均为 16kHz/mono/int16)。
    fn read_wav_mono_f32(path: &Path) -> Vec<f32> {
        let mut r = hound::WavReader::open(path)
            .unwrap_or_else(|e| panic!("打开 wav 失败 {path:?}: {e}"));
        let spec = r.spec();
        assert_eq!(spec.channels, 1, "测试样本应为单声道: {path:?}");
        assert_eq!(spec.sample_rate, 16_000, "测试样本应为 16kHz: {path:?}");
        match spec.sample_format {
            hound::SampleFormat::Float => r.samples::<f32>().map(|s| s.unwrap()).collect(),
            hound::SampleFormat::Int => {
                let max = (1i64 << (spec.bits_per_sample - 1)) as f32;
                r.samples::<i32>().map(|s| s.unwrap() as f32 / max).collect()
            }
        }
    }

    /// 真实回声样本交叉验证:与官方 python 实现同口径(-49.7dB),Rust 应 ≥20dB。
    /// 样本不入库(AEC-Challenge 数据许可保守处理),经 VN_DTLN_SAMPLE_DIR 指向
    /// 本地克隆的 DTLN-aec 仓样本目录运行。
    /// (原合成噪声回声测试已删:预训练模型对宽带调制噪声"回声"零响应,见模块头「负发现」。)
    #[test]
    #[ignore]
    fn suppresses_real_echo_sample() {
        let dir = std::path::PathBuf::from(std::env::var("VN_DTLN_DIR").unwrap());
        let base = std::path::PathBuf::from(std::env::var("VN_DTLN_SAMPLE_DIR").unwrap());
        let mic = read_wav_mono_f32(&base.join("9mkQhVtzTEy2hDk-6u2Sww_farend_singletalk_mic.wav"));
        let lpb = read_wav_mono_f32(&base.join("9mkQhVtzTEy2hDk-6u2Sww_farend_singletalk_lpb.wav"));
        let out = suppress_residual(&mic, &lpb, &dir).unwrap();
        assert_eq!(out.len(), mic.len(), "样本守恒");
        let p = |s: &[f32]| {
            s.iter().map(|x| (*x as f64) * (*x as f64)).sum::<f64>() / s.len() as f64
        };
        let (pm, po) = (p(&mic), p(&out));
        let db = 10.0 * (po / pm).log10();
        eprintln!("[real_echo] p_mic={pm:.8} p_out={po:.10} 抑制 {db:.2} dB");
        assert!(db <= -20.0, "纯远端回声应至少压 20dB,实测 {db:.2} dB");
    }

    /// 参考全零:近端直通,能量保持在 ±3dB(神经模型对无回声输入不该乱削)。
    #[test]
    #[ignore]
    fn near_end_passes_when_reference_silent() {
        let dir = std::path::PathBuf::from(std::env::var("VN_DTLN_DIR").unwrap());
        let mut seed = 21u64;
        let mic = crate::audio::delay_estimate::tests::block_modulated_noise(16_000 * 10, &mut seed);
        let zeros = vec![0.0f32; mic.len()];
        let out = suppress_residual(&mic, &zeros, &dir).unwrap();
        assert_eq!(out.len(), mic.len());
        let p = |s: &[f32]| s.iter().map(|x| x * x).sum::<f32>() / s.len() as f32;
        let tail = mic.len() / 2;
        let ratio = p(&out[tail..]) / p(&mic[tail..]);
        assert!(ratio > 0.5 && ratio < 2.0, "近端应保持 ±3dB: {ratio}");
    }

    /// 零散长度守恒:非 128 倍数输入,输出长度仍严格相等。
    #[test]
    #[ignore]
    fn conserves_arbitrary_length() {
        let dir = std::path::PathBuf::from(std::env::var("VN_DTLN_DIR").unwrap());
        let mic = vec![0.01f32; 16_000 + 77];
        let reference = vec![0.0f32; 16_000 + 77];
        let out = suppress_residual(&mic, &reference, &dir).unwrap();
        assert_eq!(out.len(), mic.len());
    }
}
