//! Windows 的软件 AEC 桩(与 aec.rs 同形 API)。
//!
//! webrtc-audio-processing(AEC3)官方不支持 Windows/MSVC(其 CI 无 windows,
//! issue #34 维护者明言无测试机;build.rs 走 meson + GCC 语法编译旗标,MSVC 必挂,
//! 无可用 fork)——依赖在 Cargo.toml 圈进 cfg(not(windows)),本文件经 audio/mod.rs
//! 的 #[path] 顶替模块位,平台无关代码(session/segment_worker 的 AecRole 签名、
//! echo_clean 的构造调用)零改动照常编译。
//!
//! 行为:三个构造一律返回 Err → lib.rs 装配层走既有「无 AEC 降级」路径(每场一行
//! 日志,不挡录制);文本级回声去重链(session.rs,平台无关)成为 Windows 的回声
//! 兜底。Render/Capture 仍给出 no-op/透传实现——它们只会在测试或未来代码里被
//! 直接构造,不会经 Err 的构造函数流出。
//!
//! 后续增强(计划文档已记):DTLN-aec 走 tract-onnx 纯 Rust,可平台无关地做成
//! Windows 实时声学 AEC,需真机验证延迟预算后再上。

use std::sync::Arc;

pub fn new_pair(_sample_rate: u32) -> anyhow::Result<(AecRender, AecCapture)> {
    anyhow::bail!("Windows 暂无声学 AEC(webrtc-audio-processing 不支持 MSVC),文本级回声去重兜底")
}

pub fn new_aligned_pair(
    _sample_rate: u32,
    _initial_predelay_ms: u32,
) -> anyhow::Result<(AecRender, AecCapture, Arc<crate::audio::aec_align::AlignState>)> {
    anyhow::bail!("Windows 暂无声学 AEC(webrtc-audio-processing 不支持 MSVC),文本级回声去重兜底")
}

pub fn new_clean_pair(_sample_rate: u32) -> anyhow::Result<(AecRender, AecCapture)> {
    anyhow::bail!("Windows 暂无离线清洗 APM(webrtc-audio-processing 不支持 MSVC)")
}

/// 分段 worker 的 AEC 角色:随源分发(与真实实现同形)。
pub enum AecRole {
    Render(AecRender),
    Capture(AecCapture),
}

/// 远端参考句柄:桩实现丢弃输入。
pub struct AecRender {}

impl AecRender {
    pub fn push(&mut self, _input: &[f32]) {}
}

/// 近端消回声句柄:桩实现原样透传。
pub struct AecCapture {}

impl AecCapture {
    pub fn process(&mut self, samples: &[f32]) -> Vec<f32> {
        samples.to_vec()
    }
}
