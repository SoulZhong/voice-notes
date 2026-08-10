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
/// 按 ASR 选型返回模型目录。lib.rs 识别器装配与 asr_bench 评测工具共用,
/// 防两处各拼一份路径漂移。未知值按 SenseVoice 兜底,与 new_recognizer 分支一致。
pub fn asr_model_dir(asr_model: &str) -> PathBuf {
    let dir = match asr_model {
        "whisper" => "sherpa-onnx-whisper-base",
        "paraformer" => PF_DIR,
        "qwen3" => QWEN3_DIR,
        _ => SV_DIR,
    };
    root().join(dir)
}

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
pub const QWEN3_DIR: &str = "sherpa-onnx-qwen3-asr-0.6B-int8-2026-03-25";

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
    Artifact {
        id: "qwen3",
        label: "语音识别（Qwen3-ASR 0.6B）",
        url: "https://github.com/k2-fsa/sherpa-onnx/releases/download/asr-models/sherpa-onnx-qwen3-asr-0.6B-int8-2026-03-25.tar.bz2",
        kind: ArtifactKind::TarBz2 { dest_dir: QWEN3_DIR },
        approx_mb: 879,
        prune: &["sherpa-onnx-qwen3-asr-0.6B-int8-2026-03-25/test_wavs"],
        files: &[
            FinalFile {
                rel_path: "sherpa-onnx-qwen3-asr-0.6B-int8-2026-03-25/conv_frontend.onnx",
                bytes: 44_148_281,
                sha256: "d22dc4423e0940e49884e903d2ea2f7e5567c14fc1aed97e4e26d6b8f208ef9e",
            },
            FinalFile {
                rel_path: "sherpa-onnx-qwen3-asr-0.6B-int8-2026-03-25/encoder.int8.onnx",
                bytes: 182_491_662,
                sha256: "60748d3e6744a57c9c91e1b17424a6c2990567e8adceb0783940c03ed98fa9d9",
            },
            FinalFile {
                rel_path: "sherpa-onnx-qwen3-asr-0.6B-int8-2026-03-25/decoder.int8.onnx",
                bytes: 755_914_231,
                sha256: "4f6885be5959ae26af3089d38ee7972c5fafbeeb1cf8d5e76eab6d8b61ca5771",
            },
            FinalFile {
                rel_path: "sherpa-onnx-qwen3-asr-0.6B-int8-2026-03-25/tokenizer/vocab.json",
                bytes: 2_776_833,
                sha256: "ca10d7e9fb3ed18575dd1e277a2579c16d108e32f27439684afa0e10b1440910",
            },
            FinalFile {
                rel_path: "sherpa-onnx-qwen3-asr-0.6B-int8-2026-03-25/tokenizer/merges.txt",
                bytes: 1_671_853,
                sha256: "8831e4f1a044471340f7c0a83d7bd71306a5b867e95fd870f74d0c5308a904d5",
            },
            FinalFile {
                rel_path: "sherpa-onnx-qwen3-asr-0.6B-int8-2026-03-25/tokenizer/tokenizer_config.json",
                bytes: 12_487,
                sha256: "4942d005604266809309cabc9f4e9cb89ce855d59b14681fdc0e1cc62ea26c4c",
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
/// 就绪与否随选型变（四选型互斥：选中哪个就只需要哪个的工件），静态标记表达不了。
/// vad 恒需；asr（SenseVoice）仅 sense_voice 选型需要；whisper/paraformer/qwen3
/// 各仅对应选型需要；speaker 等不影响录制。
pub fn required_now(id: &str, asr_model: &str) -> bool {
    match id {
        "vad" => true,
        "asr" => {
            asr_model != crate::settings::ASR_WHISPER
                && asr_model != crate::settings::ASR_PARAFORMER
                && asr_model != crate::settings::ASR_QWEN3
        }
        "whisper" => asr_model == crate::settings::ASR_WHISPER,
        "paraformer" => asr_model == crate::settings::ASR_PARAFORMER,
        "qwen3" => asr_model == crate::settings::ASR_QWEN3,
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
    /// 模型存储目录(root() 的展示形式)。设置页展示并支持点击打开。
    pub root: String,
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
        root: root.display().to_string(),
    }
}

/// 就绪判定的模式感知入口。cloud_mode=false 时与 status() 完全等价(本地现状);
/// cloud_mode=true:识别在云端,本地 ASR 大模型全不必需,vad 保留必需(spec §5:
/// 避免 required_now 分叉、回切本地时不缺件);录制就绪还要求凭证齐(creds_ok)。
pub fn status_for(asr_model: &str, cloud_mode: bool, creds_ok: bool) -> ModelsStatus {
    let mut s = status(asr_model);
    if cloud_mode {
        for a in &mut s.artifacts {
            a.required_for_recording = a.id == "vad";
        }
        let vad_ok = s.artifacts.iter().find(|a| a.id == "vad").map(|a| a.present).unwrap_or(false);
        s.recording_ready = vad_ok && creds_ok;
    }
    s
}

// 曾有一个 recording_ready(asr_model) 供开录守卫/托盘单独判定,已删除:它只认本地
// 选型,云端模式下会把"本机大模型没下全"当成不可开录,而云端根本不需要那些件。
// 就绪判定现在只有 status_for 一条真源,调用方经 lib.rs::current_models_status 取用。

#[cfg(test)]
mod tests {
    use super::*;

    /// root() 读 MODELS_OVERRIDE/VN_MODELS 两个进程级全局态；cargo test 默认多线程并行，
    /// 凡是读或写它们的测试都要共用这把锁串行化，否则 root_prefers_env_then_override
    /// 的写会和别的测试的读交叉，读到过渡态（曾致 status_for_cloud_mode_needs_only_vad_plus_creds
    /// 间歇失败）。同一线程内可重入判定不适用——std Mutex 非重入，故各测试仅在自身体内取一次。
    static ROOT_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

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
    fn status_exposes_models_root_path() {
        let _guard = ROOT_LOCK.lock().unwrap();
        // 前端「语音模型」区展示存储目录并支持点击打开,root 必须随 status 下发。
        let s = status(crate::settings::ASR_SENSE_VOICE);
        assert!(!s.root.is_empty());
        assert_eq!(s.root, root().display().to_string());
    }

    #[test]
    fn manifest_covers_nine_artifacts_with_qwen3_whisper_paraformer_and_dtln_aec() {
        let ids: Vec<&str> = ARTIFACTS.iter().map(|a| a.id).collect();
        assert_eq!(
            ids,
            vec![
                "vad", "speaker", "speaker-eres2netv2", "asr", "whisper", "paraformer",
                "qwen3", "dtln_aec_256_1", "dtln_aec_256_2",
            ]
        );
        let w = ARTIFACTS.iter().find(|a| a.id == "whisper").unwrap();
        assert!(matches!(w.kind, ArtifactKind::TarBz2 { dest_dir: "sherpa-onnx-whisper-base" }));
        assert_eq!(w.files.len(), 3);
        assert!(!w.prune.is_empty(), "fp32 与测试音频装好即删");
        let q = ARTIFACTS.iter().find(|a| a.id == "qwen3").unwrap();
        assert!(matches!(q.kind, ArtifactKind::TarBz2 { dest_dir: QWEN3_DIR }));
        assert_eq!(q.files.len(), 6, "三个 onnx + tokenizer 三件");
        assert!(!q.prune.is_empty(), "test_wavs 装好即删");
        for a in ARTIFACTS {
            for f in a.files { assert_eq!(f.sha256.len(), 64); }
        }
    }

    #[test]
    fn qwen3_required_only_when_selected() {
        assert!(required_now("qwen3", crate::settings::ASR_QWEN3));
        assert!(!required_now("qwen3", crate::settings::ASR_SENSE_VOICE));
        assert!(!required_now("asr", crate::settings::ASR_QWEN3), "选 qwen3 时不需要 SenseVoice 工件");
        assert!(required_now("vad", crate::settings::ASR_QWEN3), "vad 恒需");
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
        let _guard = ROOT_LOCK.lock().unwrap();
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
        let _guard = ROOT_LOCK.lock().unwrap();
        let st = status("sense_voice");
        assert_eq!(st.artifacts.len(), ARTIFACTS.len());
        for s in &st.artifacts {
            let a = ARTIFACTS.iter().find(|a| a.id == s.id).expect("id 应在注册表");
            assert_eq!(s.url, a.url, "DTO url 应等于注册表 url");
            assert!(!s.url.is_empty(), "url 不应为空");
        }
    }

    #[test]
    fn status_for_cloud_mode_needs_only_vad_plus_creds() {
        let _guard = ROOT_LOCK.lock().unwrap();
        // 本地模式 = 现状(回归锚)。
        let local = status_for(crate::settings::ASR_SENSE_VOICE, false, false);
        assert_eq!(local.recording_ready, status(crate::settings::ASR_SENSE_VOICE).recording_ready);
        // 云端模式:ASR 工件全不必需,vad 仍必需。
        let cloud = status_for(crate::settings::ASR_SENSE_VOICE, true, true);
        for a in &cloud.artifacts {
            let want = a.id == "vad";
            assert_eq!(a.required_for_recording, want, "云端模式 {} 必需性", a.id);
        }
        // 凭证缺失 → 即使 vad 在也不就绪。
        let no_creds = status_for(crate::settings::ASR_SENSE_VOICE, true, false);
        assert!(!no_creds.recording_ready, "云端无凭证不可开录");
    }
}

/// 声纹模型选型 → 模型文件名(settings.speaker_model 消费;未知值回退 CAM++)。
pub fn speaker_model_file(model: &str) -> &'static str {
    match model {
        "eres2netv2" => "3dspeaker_speech_eres2netv2_sv_zh-cn_16k-common.onnx",
        _ => "3dspeaker_speech_campplus_sv_zh-cn_16k-common.onnx",
    }
}
