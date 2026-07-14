//! DTLN-aec 神经残余级：双阶段模型加载与单块前向（P3b 可行性硬闸,T1）。
//!
//! **路线定版：ONNX + tract-onnx（纯 Rust 运行时）。** 原计划的两条 tflite 路线硬闸双败
//! （tract-tflite 0.22/0.23 反序列化负轴 `reduce_mean(axis=-1)` 时未做回绕直接 `as usize`
//! 越界 panic;tflitec 需 bazel 对 TensorFlow 全量源码整编,本机被 GitHub 429 限流且该
//! 构建链路对桌面应用不可维护），详见 `.superpowers/sdd/p3b-task-1-report.md` 偏差记录。
//! 官方 tflite 模型已离线转换为 ONNX（tf2onnx --opset 13,随机输入数值最大差 ~1e-6），
//! 工件哈希与下载 URL 钉死在计划 Global Constraints。
//!
//! 单块算法（全文见 `docs/superpowers/plans/2026-07-14-voice-notes-neural-residual-aec.md`
//! 「官方块算法」一节，T2 会在此基础上补外层 512 滑窗缓冲）：
//! ```text
//! mag_mic = |rfft(mic_block)|; mag_lpb = |rfft(lpb_block)|
//! (mask, states_1') = model1(mag_mic, states_1, mag_lpb)
//! est = irfft(rfft(mic_block) * mask) / 512   ← realfft 的 C2R 不含 1/N，需手工补
//! (out_block, states_2') = model2(est, states_2, lpb_block)
//! ```
//! 张量绑定**按名字映射,不赌下标**（tf2onnx 保留 tflite 输入名，且名字顺序与语义顺序
//! 不一致）：模型1 输入 input_3/input_5/input_4 = mag/states/lpb_mag；模型2 输入
//! input_6/input_8/input_7 = est/states/lpb。加载时 eprintln 双模型全部输入/输出的
//! name/shape（T2 依赖这份 dump）。

use std::path::Path;

use anyhow::{ensure, Context, Result};
use realfft::RealFftPlanner;
use tract_onnx::prelude::*;

const BLOCK_LEN: usize = 512;
const FREQ_BINS: usize = BLOCK_LEN / 2 + 1;

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
    /// 按插槽下标组装 (primary, state, secondary) 三输入并前向，返回主输出。
    fn run(&self, primary: &[f32], state: &[f32], secondary: &[f32]) -> Result<Vec<f32>> {
        let mut slots: Vec<Option<TValue>> = vec![None, None, None];
        slots[self.in_primary] = Some(Tensor::from_shape(&self.primary_shape, primary)?.into());
        slots[self.in_state] = Some(Tensor::from_shape(&self.state_shape, state)?.into());
        slots[self.in_secondary] =
            Some(Tensor::from_shape(&self.secondary_shape, secondary)?.into());
        let mut inputs: TVec<TValue> = tvec!();
        for v in slots {
            inputs.push(v.expect("三个输入插槽应全部填充"));
        }
        let outputs = self.plan.run(inputs).context("模型前向推理失败")?;
        Ok(outputs[self.out_primary]
            .as_slice::<f32>()
            .context("主输出读取失败")?
            .to_vec())
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
        let mask = self
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
        let out_block = self
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
}
