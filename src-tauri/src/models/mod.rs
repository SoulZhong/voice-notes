//! 模型目录解析与工件清单：运行时定位模型、判定缺失，供下载器（download 子模块）补齐。
//!
//! 目录解析顺序：VN_MODELS 环境变量 → 设置覆盖（set_models_override，settings.models_dir 注入）
//! → debug 构建下的 src-tauri/models（开发机零迁移）→ 生产默认 app_data_dir/models
//! （setup 时经 init_app_root 注入）。env 置顶是为让测试/临时调试能强制覆盖用户设置。

pub mod download;

use serde::Serialize;
use std::path::{Path, PathBuf};
use std::sync::{OnceLock, RwLock};

static APP_MODELS_ROOT: OnceLock<PathBuf> = OnceLock::new();

/// 设置层的模型目录覆盖。用 RwLock 而非 OnceLock：用户可在运行期改 settings.models_dir，
/// 需要可重设（OnceLock 只能设一次）。const new 免运行时初始化。
static MODELS_OVERRIDE: RwLock<Option<PathBuf>> = RwLock::new(None);

/// setup 时注入生产模型根目录（app_data_dir/models）。重复调用无害（首次生效）。
pub fn init_app_root(dir: PathBuf) {
    let _ = APP_MODELS_ROOT.set(dir);
}

/// 设置覆盖模型根目录（None = 清除，回落后续兜底）。settings.models_dir 变更时调用。
pub fn set_models_override(dir: Option<PathBuf>) {
    *MODELS_OVERRIDE.write().unwrap() = dir;
}

/// 模型根目录。见模块注释的解析顺序；多处兜底保证测试进程（未 init）行为与历史一致。
pub fn root() -> PathBuf {
    if let Ok(p) = std::env::var("VN_MODELS") {
        if !p.is_empty() {
            return PathBuf::from(p);
        }
    }
    if let Some(p) = MODELS_OVERRIDE.read().unwrap().clone() {
        return p;
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
    /// 装好后要删除的 root 相对路径：如 whisper 的 fp32 权重与测试音频，present 判定不看它们，
    /// 留盘白占空间。既有三工件无需清理，给 &[]。（清理动作由下载器接入，Task 8。）
    pub prune: &'static [&'static str],
    pub files: &'static [FinalFile],
}

const SV_DIR: &str = "sherpa-onnx-sense-voice-zh-en-ja-ko-yue-2024-07-17";
pub const PF_DIR: &str = "sherpa-onnx-paraformer-zh-2023-09-14";

pub const ARTIFACTS: &[Artifact] = &[
    Artifact {
        id: "vad",
        label: "语句分段（Silero VAD）",
        url: "https://github.com/k2-fsa/sherpa-onnx/releases/download/asr-models/silero_vad.onnx",
        kind: ArtifactKind::File,
        approx_mb: 1,
        prune: &[],
        files: &[FinalFile {
            rel_path: "silero_vad.onnx",
            bytes: 643_854,
            sha256: "9e2449e1087496d8d4caba907f23e0bd3f78d91fa552479bb9c23ac09cbb1fd6",
        }],
    },
    Artifact {
        id: "speaker",
        label: "说话人区分",
        // 注意 URL 里 "recongition" 是上游 release 页的原始拼写，勿"修正"。
        url: "https://github.com/k2-fsa/sherpa-onnx/releases/download/speaker-recongition-models/3dspeaker_speech_campplus_sv_zh-cn_16k-common.onnx",
        kind: ArtifactKind::File,
        approx_mb: 27,
        prune: &[],
        files: &[FinalFile {
            rel_path: "3dspeaker_speech_campplus_sv_zh-cn_16k-common.onnx",
            bytes: 28_281_138,
            sha256: "f682b514c05d947ee3fa91cd6ec6c5c7543479a128373fa29b1faedccd21fd11",
        }],
    },
    Artifact {
        id: "speaker-eres2netv2",
        label: "声纹模型(ERes2NetV2)",
        // 备选声纹嵌入模型(设置页可切换);与 CAM++ 嵌入空间不可混用,切换会触发
        // 声纹库从录音样本重建。URL 里 "recongition" 同上游原始拼写。
        url: "https://github.com/k2-fsa/sherpa-onnx/releases/download/speaker-recongition-models/3dspeaker_speech_eres2netv2_sv_zh-cn_16k-common.onnx",
        kind: ArtifactKind::File,
        approx_mb: 68,
        prune: &[],
        files: &[FinalFile {
            rel_path: "3dspeaker_speech_eres2netv2_sv_zh-cn_16k-common.onnx",
            bytes: 71_441_526,
            sha256: "bf1a75b9930474cf3389ef415e6e5d38ca96fea4a3a00f7e301d080a58ee2239",
        }],
    },
    Artifact {
        id: "asr",
        label: "语音识别（SenseVoice）",
        url: "https://github.com/k2-fsa/sherpa-onnx/releases/download/asr-models/sherpa-onnx-sense-voice-zh-en-ja-ko-yue-2024-07-17.tar.bz2",
        kind: ArtifactKind::TarBz2 { dest_dir: SV_DIR },
        approx_mb: 1000,
        prune: &[],
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
    Artifact {
        id: "whisper",
        label: "语音识别（Whisper base）",
        url: "https://github.com/k2-fsa/sherpa-onnx/releases/download/asr-models/sherpa-onnx-whisper-base.tar.bz2",
        kind: ArtifactKind::TarBz2 { dest_dir: "sherpa-onnx-whisper-base" },
        approx_mb: 198,
        prune: &[
            "sherpa-onnx-whisper-base/base-encoder.onnx",
            "sherpa-onnx-whisper-base/base-decoder.onnx",
            "sherpa-onnx-whisper-base/test_wavs",
        ],
        files: &[
            FinalFile {
                rel_path: "sherpa-onnx-whisper-base/base-encoder.int8.onnx",
                bytes: 29_120_534,
                sha256: "0b8fb1304b6109976038efff5ace81720e00386f3ff6b54ee8c75291ca0a1e11",
            },
            FinalFile {
                rel_path: "sherpa-onnx-whisper-base/base-decoder.int8.onnx",
                bytes: 130_672_026,
                sha256: "9759d217388a01b3a4c7c15533201067b48ae819c4daafc8624e64b9409dc02d",
            },
            FinalFile {
                rel_path: "sherpa-onnx-whisper-base/base-tokens.txt",
                bytes: 816_730,
                sha256: "b34b360dbb493e781e479794586d661700670d65564001f23024971d1f2fa126",
            },
        ],
    },
    Artifact {
        id: "paraformer",
        label: "语音识别（Paraformer 中文大模型）",
        url: "https://github.com/k2-fsa/sherpa-onnx/releases/download/asr-models/sherpa-onnx-paraformer-zh-2023-09-14.tar.bz2",
        kind: ArtifactKind::TarBz2 { dest_dir: PF_DIR },
        approx_mb: 224,
        prune: &["sherpa-onnx-paraformer-zh-2023-09-14/test_wavs"],
        files: &[
            FinalFile {
                rel_path: "sherpa-onnx-paraformer-zh-2023-09-14/model.int8.onnx",
                bytes: 243_371_218,
                sha256: "f36a0433bcf096bd6d6f11b80a3ac8bed110bdca632fe0d731df8d1a84475945",
            },
            FinalFile {
                rel_path: "sherpa-onnx-paraformer-zh-2023-09-14/tokens.txt",
                bytes: 75_756,
                sha256: "59aba8873a2ed1e122c25fee421e25f283b63290efbde85c1f01a853d83cb6e6",
            },
        ],
    },
    // DTLN-aec 256 档：增值层神经残余回声消除。两个裸 onnx 工件（非压缩包），
    // 各自独立 URL/哈希，形状照抄 vad/speaker 的单文件 Artifact（File kind 一 url 一 file，
    // TarBz2 不适用——非压缩包）。not required_for_recording：模型不在场时清洗管线
    // 回落 AEC3-only（见 Task 4），因此不进 required_now。
    // 维护提醒:这两个 onnx 靠手动发布的 public GitHub release(tag models-dtln-aec-v1)
    // 分发,全网无官方 onnx 源。今后更新模型或改 tag,必须同步发布对应 public release 并
    // 上传资产,否则匿名用户下载 404(曾因 release 从未发布导致全体用户下不了)。
    Artifact {
        id: "dtln_aec_256_1",
        label: "神经回声消除（DTLN-aec）· 掩码模型",
        url: "https://github.com/SoulZhong/voice-notes/releases/download/models-dtln-aec-v1/dtln_aec_256_1.onnx",
        kind: ArtifactKind::File,
        approx_mb: 6,
        prune: &[],
        files: &[FinalFile {
            rel_path: "dtln_aec_256_1.onnx",
            bytes: 5_551_837,
            sha256: "61250b397616146e79371b58b34da068ce0adb09f43edfac5421f4faf6990917",
        }],
    },
    Artifact {
        id: "dtln_aec_256_2",
        label: "神经回声消除（DTLN-aec）· 合成模型",
        url: "https://github.com/SoulZhong/voice-notes/releases/download/models-dtln-aec-v1/dtln_aec_256_2.onnx",
        kind: ArtifactKind::File,
        approx_mb: 10,
        prune: &[],
        files: &[FinalFile {
            rel_path: "dtln_aec_256_2.onnx",
            bytes: 10_007_544,
            sha256: "b79a9efca5b7e33e6bbd088acc60fc946250b23e104b103c47a24783a0c0b13a",
        }],
    },
];

/// 某工件在当前 ASR 选型下是否为「录制必需」。取代了静态 required_for_recording 字段：
/// 就绪与否随选型变（三选型互斥：选中哪个就只需要哪个的工件），静态标记表达不了。
/// vad 恒需；asr（SenseVoice）仅 sense_voice 选型需要；whisper 仅 whisper 选型需要；
/// paraformer 仅 paraformer 选型需要；speaker 等不影响录制。
pub fn required_now(id: &str, asr_model: &str) -> bool {
    match id {
        "vad" => true,
        "asr" => {
            asr_model != crate::settings::ASR_WHISPER
                && asr_model != crate::settings::ASR_PARAFORMER
        }
        "whisper" => asr_model == crate::settings::ASR_WHISPER,
        "paraformer" => asr_model == crate::settings::ASR_PARAFORMER,
        _ => false,
    }
}

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
    /// 该工件的原始下载地址(GitHub release 直链),供设置页展示。
    pub url: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct ModelsStatus {
    pub artifacts: Vec<ArtifactState>,
    /// 录制可用 = 录制必需工件（vad+asr）齐。
    pub recording_ready: bool,
    /// 说话人区分可用 = 声纹工件在。
    pub diarization_ready: bool,
}

pub fn status(asr_model: &str) -> ModelsStatus {
    let root = root();
    let artifacts: Vec<ArtifactState> = ARTIFACTS
        .iter()
        .map(|a| ArtifactState {
            id: a.id.into(),
            label: a.label.into(),
            approx_mb: a.approx_mb,
            // required_for_recording 保留为前端契约，但值改为按当前选型动态算。
            required_for_recording: required_now(a.id, asr_model),
            present: artifact_present(&root, a),
            url: a.url.into(),
        })
        .collect();
    ModelsStatus {
        recording_ready: artifacts.iter().filter(|s| s.required_for_recording).all(|s| s.present),
        diarization_ready: artifacts.iter().find(|s| s.id == "speaker").map(|s| s.present).unwrap_or(false),
        artifacts,
    }
}

/// start/resume_recording 入口的防御检查用。按当前 ASR 选型判定必需工件是否齐。
pub fn recording_ready(asr_model: &str) -> bool {
    let root = root();
    ARTIFACTS
        .iter()
        .filter(|a| required_now(a.id, asr_model))
        .all(|a| artifact_present(&root, a))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 测试专用工件（不碰真实 ARTIFACTS，避免依赖本机模型）。
    fn test_artifact() -> Artifact {
        Artifact {
            id: "t", label: "测试", url: "http://example.invalid/t.bin",
            kind: ArtifactKind::File, approx_mb: 1, prune: &[],
            files: &[FinalFile { rel_path: "t.bin", bytes: 4, sha256: "deadbeef" }],
        }
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
    fn manifest_covers_eight_artifacts_with_whisper_paraformer_and_dtln_aec() {
        let ids: Vec<&str> = ARTIFACTS.iter().map(|a| a.id).collect();
        assert_eq!(
            ids,
            vec![
                "vad", "speaker", "speaker-eres2netv2", "asr", "whisper", "paraformer",
                "dtln_aec_256_1", "dtln_aec_256_2",
            ]
        );
        let w = ARTIFACTS.iter().find(|a| a.id == "whisper").unwrap();
        assert!(matches!(w.kind, ArtifactKind::TarBz2 { dest_dir: "sherpa-onnx-whisper-base" }));
        assert_eq!(w.files.len(), 3);
        assert!(!w.prune.is_empty(), "fp32 与测试音频装好即删");
        for a in ARTIFACTS {
            for f in a.files { assert_eq!(f.sha256.len(), 64); }
        }
    }

    #[test]
    fn dtln_aec_artifacts_are_bare_onnx_files_not_required_for_recording() {
        for id in ["dtln_aec_256_1", "dtln_aec_256_2"] {
            let a = ARTIFACTS.iter().find(|a| a.id == id).unwrap_or_else(|| panic!("{id} 工件已注册"));
            assert!(matches!(a.kind, ArtifactKind::File), "裸 onnx，非压缩包");
            assert_eq!(a.files.len(), 1);
            assert!(!required_now(id, crate::settings::ASR_SENSE_VOICE), "增值层，非录制必需");
            assert!(!required_now(id, crate::settings::ASR_WHISPER));
            assert!(!required_now(id, crate::settings::ASR_PARAFORMER));
        }
    }

    #[test]
    fn required_now_follows_selection() {
        assert!(required_now("vad", "sense_voice") && required_now("vad", "whisper"));
        assert!(required_now("asr", "sense_voice") && !required_now("asr", "whisper"));
        assert!(!required_now("whisper", "sense_voice") && required_now("whisper", "whisper"));
        assert!(!required_now("speaker", "sense_voice"));
    }

    #[test]
    fn paraformer_artifact_registered_and_required_semantics() {
        let a = ARTIFACTS.iter().find(|a| a.id == "paraformer").expect("paraformer 工件已注册");
        assert!(matches!(a.kind, ArtifactKind::TarBz2 { dest_dir: PF_DIR }));
        assert!(a.files.iter().any(|f| f.rel_path.ends_with("model.int8.onnx")));
        // 三选型互斥语义
        assert!(required_now("paraformer", crate::settings::ASR_PARAFORMER));
        assert!(!required_now("paraformer", crate::settings::ASR_SENSE_VOICE));
        assert!(!required_now("asr", crate::settings::ASR_PARAFORMER));
        assert!(required_now("asr", crate::settings::ASR_SENSE_VOICE));
        assert!(!required_now("whisper", crate::settings::ASR_PARAFORMER));
    }

    #[test]
    fn root_prefers_env_then_override() {
        let tmp = tempfile::tempdir().unwrap();
        set_models_override(Some(tmp.path().to_path_buf()));
        std::env::set_var("VN_MODELS", "/env-wins");
        assert_eq!(root(), PathBuf::from("/env-wins"));
        std::env::remove_var("VN_MODELS");
        assert_eq!(root(), tmp.path(), "override 次于 env、先于 dev 目录");
        set_models_override(None);
        // 回落 dev 目录(debug 构建、src-tauri/models 存在),与历史一致
        assert_eq!(root(), PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("models"));
    }

    #[test]
    fn status_exposes_artifact_urls() {
        let st = status("sense_voice");
        assert_eq!(st.artifacts.len(), ARTIFACTS.len());
        for s in &st.artifacts {
            let a = ARTIFACTS.iter().find(|a| a.id == s.id).expect("id 应在注册表");
            assert_eq!(s.url, a.url, "DTO url 应等于注册表 url");
            assert!(!s.url.is_empty(), "url 不应为空");
        }
    }
}

/// 声纹模型选型 → 模型文件名(settings.speaker_model 消费;未知值回退 CAM++)。
pub fn speaker_model_file(model: &str) -> &'static str {
    match model {
        "eres2netv2" => "3dspeaker_speech_eres2netv2_sv_zh-cn_16k-common.onnx",
        _ => "3dspeaker_speech_campplus_sv_zh-cn_16k-common.onnx",
    }
}
