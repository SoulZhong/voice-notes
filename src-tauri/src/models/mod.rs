//! 模型目录解析与工件清单：运行时定位模型、判定缺失，供下载器（download 子模块）补齐。
//!
//! 目录解析顺序：VN_MODELS 环境变量 → debug 构建下的 src-tauri/models（开发机零迁移）
//! → 生产默认 app_data_dir/models（setup 时经 init_app_root 注入）。

pub mod download;

use serde::Serialize;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

static APP_MODELS_ROOT: OnceLock<PathBuf> = OnceLock::new();

/// setup 时注入生产模型根目录（app_data_dir/models）。重复调用无害（首次生效）。
pub fn init_app_root(dir: PathBuf) {
    let _ = APP_MODELS_ROOT.set(dir);
}

/// 模型根目录。见模块注释的解析顺序；三处兜底保证测试进程（未 init）行为与历史一致。
pub fn root() -> PathBuf {
    if let Ok(p) = std::env::var("VN_MODELS") {
        if !p.is_empty() {
            return PathBuf::from(p);
        }
    }
    #[cfg(debug_assertions)]
    {
        let dev = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("models");
        if dev.is_dir() {
            return dev;
        }
    }
    APP_MODELS_ROOT
        .get()
        .cloned()
        .unwrap_or_else(|| PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("models"))
}

/// 工件的一个最终落位文件。present 判定看「存在 + 字节数精确匹配」（启动全量哈希
/// 1GB 不划算）；sha256 仅下载后校验用。
pub struct FinalFile {
    pub rel_path: &'static str,
    pub bytes: u64,
    pub sha256: &'static str,
}

pub enum ArtifactKind {
    /// 单文件直下：下载完校验后 rename 到 files[0].rel_path。
    File,
    /// tar.bz2：解压出 dest_dir 目录后整体 rename 进位。
    TarBz2 { dest_dir: &'static str },
}

pub struct Artifact {
    /// 稳定标识（进度事件/前端用）。
    pub id: &'static str,
    /// 中文显示名。
    pub label: &'static str,
    pub url: &'static str,
    pub kind: ArtifactKind,
    /// 下载体积（约数，仅展示）。
    pub approx_mb: u64,
    /// true = 录制必需（ASR/VAD）；false = 仅说话人区分（缺失只降级）。
    pub required_for_recording: bool,
    pub files: &'static [FinalFile],
}

const SV_DIR: &str = "sherpa-onnx-sense-voice-zh-en-ja-ko-yue-2024-07-17";

pub const ARTIFACTS: &[Artifact] = &[
    Artifact {
        id: "vad",
        label: "语句分段（Silero VAD）",
        url: "https://github.com/k2-fsa/sherpa-onnx/releases/download/asr-models/silero_vad.onnx",
        kind: ArtifactKind::File,
        approx_mb: 1,
        required_for_recording: true,
        files: &[FinalFile {
            rel_path: "silero_vad.onnx",
            bytes: 643_854,
            sha256: "9e2449e1087496d8d4caba907f23e0bd3f78d91fa552479bb9c23ac09cbb1fd6",
        }],
    },
    Artifact {
        id: "speaker",
        label: "声纹（说话人区分）",
        // 注意 URL 里 "recongition" 是上游 release 页的原始拼写，勿"修正"。
        url: "https://github.com/k2-fsa/sherpa-onnx/releases/download/speaker-recongition-models/3dspeaker_speech_campplus_sv_zh-cn_16k-common.onnx",
        kind: ArtifactKind::File,
        approx_mb: 27,
        required_for_recording: false,
        files: &[FinalFile {
            rel_path: "3dspeaker_speech_campplus_sv_zh-cn_16k-common.onnx",
            bytes: 28_281_138,
            sha256: "f682b514c05d947ee3fa91cd6ec6c5c7543479a128373fa29b1faedccd21fd11",
        }],
    },
    Artifact {
        id: "asr",
        label: "语音识别（SenseVoice）",
        url: "https://github.com/k2-fsa/sherpa-onnx/releases/download/asr-models/sherpa-onnx-sense-voice-zh-en-ja-ko-yue-2024-07-17.tar.bz2",
        kind: ArtifactKind::TarBz2 { dest_dir: SV_DIR },
        approx_mb: 1000,
        required_for_recording: true,
        files: &[
            FinalFile {
                rel_path: "sherpa-onnx-sense-voice-zh-en-ja-ko-yue-2024-07-17/model.onnx",
                bytes: 937_617_178,
                sha256: "977016bd9c79f9eb343430b5cc305e07ab64d5212dff41b0dcfa1694bee9a8cb",
            },
            FinalFile {
                rel_path: "sherpa-onnx-sense-voice-zh-en-ja-ko-yue-2024-07-17/tokens.txt",
                bytes: 315_894,
                sha256: "f449eb28dc567533d7fa59be34e2abca8784f771850c78a47fb731a31429a1dc",
            },
        ],
    },
];

pub fn artifact_present(root: &Path, a: &Artifact) -> bool {
    a.files.iter().all(|f| {
        root.join(f.rel_path)
            .metadata()
            .map(|m| m.is_file() && m.len() == f.bytes)
            .unwrap_or(false)
    })
}

#[derive(Debug, Clone, Serialize)]
pub struct ArtifactState {
    pub id: String,
    pub label: String,
    pub approx_mb: u64,
    pub required_for_recording: bool,
    pub present: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct ModelsStatus {
    pub artifacts: Vec<ArtifactState>,
    /// 录制可用 = 录制必需工件（vad+asr）齐。
    pub recording_ready: bool,
    /// 说话人区分可用 = 声纹工件在。
    pub diarization_ready: bool,
}

pub fn status() -> ModelsStatus {
    let root = root();
    let artifacts: Vec<ArtifactState> = ARTIFACTS
        .iter()
        .map(|a| ArtifactState {
            id: a.id.into(),
            label: a.label.into(),
            approx_mb: a.approx_mb,
            required_for_recording: a.required_for_recording,
            present: artifact_present(&root, a),
        })
        .collect();
    ModelsStatus {
        recording_ready: artifacts.iter().filter(|s| s.required_for_recording).all(|s| s.present),
        diarization_ready: artifacts.iter().find(|s| s.id == "speaker").map(|s| s.present).unwrap_or(false),
        artifacts,
    }
}

/// start/resume_recording 入口的防御检查用。
pub fn recording_ready() -> bool {
    let root = root();
    ARTIFACTS
        .iter()
        .filter(|a| a.required_for_recording)
        .all(|a| artifact_present(&root, a))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 测试专用工件（不碰真实 ARTIFACTS，避免依赖本机模型）。
    fn test_artifact() -> Artifact {
        Artifact {
            id: "t", label: "测试", url: "http://example.invalid/t.bin",
            kind: ArtifactKind::File, approx_mb: 1, required_for_recording: true,
            files: &[FinalFile { rel_path: "t.bin", bytes: 4, sha256: "deadbeef" }],
        }
    }

    #[test]
    fn root_prefers_env_var() {
        let tmp = tempfile::tempdir().unwrap();
        std::env::set_var("VN_MODELS", tmp.path());
        assert_eq!(root(), tmp.path());
        std::env::remove_var("VN_MODELS");
        // env 清掉后回落 dev 目录（debug 构建、src-tauri/models 存在）
        assert_eq!(root(), std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("models"));
    }

    #[test]
    fn artifact_present_requires_existence_and_exact_size() {
        let tmp = tempfile::tempdir().unwrap();
        let a = test_artifact();
        assert!(!artifact_present(tmp.path(), &a), "文件缺失 → 不 present");
        std::fs::write(tmp.path().join("t.bin"), b"abc").unwrap(); // 3 字节 ≠ 4
        assert!(!artifact_present(tmp.path(), &a), "大小不符 → 不 present");
        std::fs::write(tmp.path().join("t.bin"), b"abcd").unwrap();
        assert!(artifact_present(tmp.path(), &a));
    }

    #[test]
    fn manifest_covers_three_runtime_artifacts() {
        let ids: Vec<&str> = ARTIFACTS.iter().map(|a| a.id).collect();
        assert_eq!(ids, vec!["vad", "speaker", "asr"]);
        assert!(ARTIFACTS.iter().filter(|a| a.required_for_recording).count() == 2, "vad+asr 录制必需");
        for a in ARTIFACTS {
            assert!(!a.files.is_empty());
            for f in a.files { assert_eq!(f.sha256.len(), 64, "sha256 应为 64 位 hex"); }
        }
    }
}
