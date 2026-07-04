# P5 v1 收尾 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 模型自助下载（目录迁移 + 断点续传下载器 + 录制页引导卡片）、录制暂停/恢复/计时/电平表、详情页段落编辑（改文本/删除/改说话人），并入 7 项终审遗留小修。

**Architecture:** 新 `models` 模块承担目录解析（VN_MODELS → dev 目录 → app_data_dir/models）与工件清单+下载器（ureq 流式 + Range 续传 + SHA256 校验 + tar.bz2 解压进位）；暂停用 `Arc<AtomicBool>` 在 segment_worker 入口闸帧（VPIO 持续运行，暂停瞬间 flush 在途语句）；电平在闸前算 RMS 经新回调上抛；段落编辑走 NoteStore 整文件读改+原子重写，seq 为主键 + expected_text 乐观校验。

**Tech Stack:** Rust (Tauri 2, ureq, sha2, hex, bzip2, tar, crossbeam-channel), SvelteKit (Svelte 5 runes), sherpa-rs。

**Spec:** docs/superpowers/specs/2026-07-04-voice-notes-p5-v1-polish-design.md

## Global Constraints

- 分支 `p5-v1-polish`（自 master），squash 合入；每任务一提交，提交信息中文、带 `Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>` trailer。
- 动手前基线必须绿：`cargo test`（在 `src-tauri/`，81 tests）与 `npm run check`（0 errors）。
- 锁序约定（lib.rs 顶部注释）：`running → generation → session_slot`；recognizer/embedder cache 为叶子锁。新代码不得引入新的嵌套持锁。
- 原子写模式：一律 `*.tmp` 写完 `fs::rename` 进位（meta.json/speakers.json 现状如此）。
- 注释与用户可见文案用中文；错误信息风格如「录制中的笔记不能删除」。
- 模型门控测试用 `#[ignore]` 标注（需真实模型时）；常规 `cargo test` 不得依赖网络。
- 前端 Svelte 5 runes（`$state`/`$derived`/`$effect`/`$props`），事件监听集中在 `src/lib/events.ts` 风格。
- 模型三工件真值（本机实测，钉进 manifest）：
  - `silero_vad.onnx`：643,854 B，sha256 `9e2449e1087496d8d4caba907f23e0bd3f78d91fa552479bb9c23ac09cbb1fd6`
  - `3dspeaker_speech_campplus_sv_zh-cn_16k-common.onnx`：28,281,138 B，sha256 `f682b514c05d947ee3fa91cd6ec6c5c7543479a128373fa29b1faedccd21fd11`
  - `sherpa-onnx-sense-voice-zh-en-ja-ko-yue-2024-07-17/model.onnx`：937,617,178 B，sha256 `977016bd9c79f9eb343430b5cc305e07ab64d5212dff41b0dcfa1694bee9a8cb`
  - `sherpa-onnx-sense-voice-zh-en-ja-ko-yue-2024-07-17/tokens.txt`：315,894 B，sha256 `f449eb28dc567533d7fa59be34e2abca8784f771850c78a47fb731a31429a1dc`

---

### Task 0: 分支与基线

**Files:** 无代码改动。

- [ ] **Step 1: 建分支**

```bash
git checkout master && git pull && git checkout -b p5-v1-polish
```

- [ ] **Step 2: 基线绿**

```bash
cd src-tauri && cargo test 2>&1 | tail -5   # 期望: 81 passed (另有 ignored)
cd .. && npm run check 2>&1 | tail -3       # 期望: 0 errors
```

若基线不绿，停下修基线（单独提交），不得带病开工。

---

### Task 1: models 模块——目录解析 + 工件清单 + 状态判定 + lib.rs 收敛

**Files:**
- Create: `src-tauri/src/models/mod.rs`
- Modify: `src-tauri/src/lib.rs`（models_dir/sense_voice_dir/speaker_model_path/vad_path 收敛；setup 注入 app root；预载抽函数；models_status command；录制入口 guard）

**Interfaces:**
- Produces: `models::root() -> PathBuf`；`models::init_app_root(PathBuf)`；`models::ARTIFACTS: &[Artifact]`（含 `FinalFile { rel_path, bytes, sha256 }`、`ArtifactKind::{File, TarBz2{dest_dir}}`、字段 `id/label/url/kind/approx_mb/required_for_recording/files`）；`models::artifact_present(&Path, &Artifact) -> bool`；`models::status() -> ModelsStatus`（Serialize：`artifacts: Vec<ArtifactState{id,label,approx_mb,required_for_recording,present}>, recording_ready, diarization_ready`）；`models::recording_ready() -> bool`；lib.rs `fn preload_models(recognizer_cache, embedder_cache)`；command `models_status`。
- Consumes: 无（首任务）。

- [ ] **Step 1: 写失败测试（models/mod.rs 内嵌 tests）**

创建 `src-tauri/src/models/mod.rs`，先只放测试骨架与 `use`（实现留空会编译失败，故本任务采用「实现+测试同文件一次写齐、先跑红再补实现」不可行——Rust 单文件模块内测试必须编译。改为：先写完整实现骨架但 `todo!()` 留空函数体？`todo!()` 会 panic 使测试红）。**执行顺序**：写下面 Step 3 的完整文件但把 `root()`、`artifact_present()`、`status()` 的函数体换成 `todo!()`，跑测试确认 3 个测试 panic（红），再填实现（绿）。

测试代码（文件 tests 模块，Step 3 的文件已包含，此处为其内容）：

```rust
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
```

注意 `root_prefers_env_var` 独占改动进程级 env：本模块只有这一个测试碰 `VN_MODELS`，且集成测试是独立进程，不冲突。

- [ ] **Step 2: 跑红**

```bash
cd src-tauri && cargo test models:: 2>&1 | tail -8
```
Expected: 3 个测试 FAIL（`todo!()` panic）。

- [ ] **Step 3: 完整实现 models/mod.rs**

```rust
//! 模型目录解析与工件清单：运行时定位模型、判定缺失，供下载器（download 子模块）补齐。
//!
//! 目录解析顺序：VN_MODELS 环境变量 → debug 构建下的 src-tauri/models（开发机零迁移）
//! → 生产默认 app_data_dir/models（setup 时经 init_app_root 注入）。

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
```

（tests 模块见 Step 1，同文件。）

- [ ] **Step 4: lib.rs 收敛调用点 + setup 注入 + 预载抽函数 + guard + command**

`src-tauri/src/lib.rs` 改动清单：

1. 模块声明区加 `pub mod models;`。
2. **删除** `fn models_dir()`（第 53-55 行），三个路径函数改为：

```rust
fn sense_voice_dir() -> PathBuf {
    models::root().join("sherpa-onnx-sense-voice-zh-en-ja-ko-yue-2024-07-17")
}

fn speaker_model_path() -> PathBuf {
    models::root().join("3dspeaker_speech_campplus_sv_zh-cn_16k-common.onnx")
}
```

spawn_session 内（原第 185 行）：`let vad_path = models::root().join("silero_vad.onnx");`

3. 预载逻辑从 setup 闭包抽成自由函数（setup 与后续下载完成后共用）：

```rust
/// 后台预载识别器与声纹嵌入器进常驻槽（幂等：槽已有则跳过）。
/// 锁序：预载是唯一嵌套持两槽者——持 recognizer 槽锁期间嵌套获取 embedder 槽锁，
/// 消除间隙内开录线程 take 到空 embedder 的静默降级（详见原 setup 注释）。
fn preload_models(
    cache: Arc<Mutex<Option<Box<dyn asr::Recognizer>>>>,
    embedder_cache: Arc<Mutex<Option<Box<dyn diar::SpeakerEmbedder>>>>,
) {
    std::thread::spawn(move || {
        let mut slot = cache.lock().unwrap();
        if slot.is_none() {
            match asr::sense_voice::SenseVoiceRecognizer::new(&sense_voice_dir()) {
                Ok(r) => *slot = Some(Box::new(r) as Box<dyn asr::Recognizer>),
                Err(e) => eprintln!("识别器预载失败（将在开录时现场加载）: {e}"),
            }
        }
        let mut eslot = embedder_cache.lock().unwrap();
        if eslot.is_none() {
            match diar::SherpaEmbedder::new(&speaker_model_path()) {
                Ok(e) => *eslot = Some(Box::new(e) as Box<dyn diar::SpeakerEmbedder>),
                Err(e) => eprintln!("声纹模型预载失败（说话人区分将不可用）: {e}"),
            }
        }
        drop(eslot);
        drop(slot);
    });
}
```

setup 改为：

```rust
.setup(|app| {
    // 生产模型根目录注入（VN_MODELS / dev 目录优先级更高，见 models::root）。
    if let Ok(dir) = app.path().app_data_dir() {
        let models_dir = dir.join("models");
        let _ = std::fs::create_dir_all(&models_dir);
        models::init_app_root(models_dir);
    }
    let cache = app.state::<AppState>().recognizer_cache.clone();
    let embedder_cache = app.state::<AppState>().embedder_cache.clone();
    preload_models(cache, embedder_cache);
    Ok(())
})
```

4. 录制入口 guard——`start_recording` 与 `resume_recording` 函数体开头（spawn_session 调用前）都加：

```rust
    if !models::recording_ready() {
        return Err("模型缺失：请先在录制页下载模型".into());
    }
```

5. 新 command 并注册进 `generate_handler!`：

```rust
#[tauri::command]
fn models_status() -> models::ModelsStatus {
    models::status()
}
```

- [ ] **Step 5: 跑绿 + 全量回归**

```bash
cd src-tauri && cargo test 2>&1 | tail -5
```
Expected: 原 81 + 新 3 = 84 passed。注意：现有模型门控 `#[ignore]` 测试路径经由 VN_MODELS 或 dev 目录不受影响。

- [ ] **Step 6: Commit**

```bash
git add src-tauri/src/models src-tauri/src/lib.rs
git commit -m "feat(models): 模型目录运行时解析 + 工件清单与状态判定

models::root() 解析 VN_MODELS → dev src-tauri/models → app_data_dir/models,
替代编译期烙死的 CARGO_MANIFEST_DIR;三工件 manifest(URL/字节数/SHA256 钉死);
models_status command;录制入口缺模型 guard;预载抽 preload_models 供下载后复用。

Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>"
```

---

### Task 2: 下载器纯逻辑——镜像拼接 / SHA256 校验 / 解压进位

**Files:**
- Modify: `src-tauri/Cargo.toml`（加依赖）
- Create: `src-tauri/src/models/download.rs`
- Modify: `src-tauri/src/models/mod.rs`（首行下加 `pub mod download;`）

**Interfaces:**
- Consumes: Task 1 的 `FinalFile`。
- Produces: `download::apply_mirror(url, enabled, prefix) -> String`；`download::sha256_file(&Path) -> Result<String>`；`download::verify_file(&Path, &FinalFile) -> Result<()>`；`download::tmp_extract_dir(&Path) -> PathBuf`；`download::sweep_tmp(&Path)`；`download::extract_and_install(tarball, root, dest_dir, files) -> Result<()>`。

- [ ] **Step 1: 加依赖**

`src-tauri/Cargo.toml` `[dependencies]` 追加：

```toml
ureq = "2"
sha2 = "0.10"
hex = "0.4"
bzip2 = "0.5"
tar = "0.4"
```

`cargo build` 确认可解析（ureq 本任务未用到，Task 3 用；一次加齐避免两次锁文件变更）。

- [ ] **Step 2: 写测试（download.rs 同文件 tests，先 `todo!()` 骨架跑红再实现）**

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::FinalFile;
    use std::io::Write;

    /// 造小 tar.bz2 fixture：dest_dir/ 下若干小文件。
    fn make_tarball(dir: &std::path::Path, dest_dir: &str, files: &[(&str, &[u8])]) -> std::path::PathBuf {
        let tar_path = dir.join("pkg.tar.bz2");
        let f = std::fs::File::create(&tar_path).unwrap();
        let enc = bzip2::write::BzEncoder::new(f, bzip2::Compression::default());
        let mut b = tar::Builder::new(enc);
        for (name, content) in files {
            let mut h = tar::Header::new_gnu();
            h.set_size(content.len() as u64);
            h.set_mode(0o644);
            h.set_cksum();
            b.append_data(&mut h, format!("{dest_dir}/{name}"), *content).unwrap();
        }
        b.into_inner().unwrap().finish().unwrap();
        tar_path
    }

    /// 测试用 FinalFile：内容哈希现算（&'static 经 Box::leak）。
    fn ff(rel: &str, content: &[u8]) -> FinalFile {
        use sha2::{Digest, Sha256};
        FinalFile {
            rel_path: Box::leak(rel.to_string().into_boxed_str()),
            bytes: content.len() as u64,
            sha256: Box::leak(hex::encode(Sha256::digest(content)).into_boxed_str()),
        }
    }

    #[test]
    fn apply_mirror_prefixes_only_when_enabled() {
        let u = "https://github.com/a/b.onnx";
        assert_eq!(apply_mirror(u, false, "https://ghproxy.net/"), u);
        assert_eq!(apply_mirror(u, true, ""), u, "空前缀视同关闭");
        assert_eq!(apply_mirror(u, true, "https://ghproxy.net/"), format!("https://ghproxy.net/{u}"));
        assert_eq!(apply_mirror(u, true, "https://ghproxy.net"), format!("https://ghproxy.net/{u}"), "自动补尾斜杠");
    }

    #[test]
    fn verify_file_checks_size_then_hash() {
        let tmp = tempfile::tempdir().unwrap();
        let p = tmp.path().join("m.bin");
        std::fs::write(&p, b"hello").unwrap();
        assert!(verify_file(&p, &ff("m.bin", b"hello")).is_ok());
        assert!(verify_file(&p, &ff("m.bin", b"hell")).is_err(), "大小不符");
        let mut wrong = ff("m.bin", b"hello");
        wrong.sha256 = Box::leak("0".repeat(64).into_boxed_str());
        assert!(verify_file(&p, &wrong).is_err(), "哈希不符");
    }

    #[test]
    fn extract_and_install_happy_path_installs_and_cleans_tmp() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("models");
        std::fs::create_dir_all(&root).unwrap();
        let tarball = make_tarball(tmp.path(), "sv-dir", &[("model.onnx", b"MODEL"), ("tokens.txt", b"TOK")]);
        let files = [ff("sv-dir/model.onnx", b"MODEL"), ff("sv-dir/tokens.txt", b"TOK")];
        extract_and_install(&tarball, &root, "sv-dir", &files).unwrap();
        assert_eq!(std::fs::read(root.join("sv-dir/model.onnx")).unwrap(), b"MODEL");
        assert!(!tmp_extract_dir(&root).exists(), "临时解压目录应清掉");
    }

    #[test]
    fn extract_and_install_bad_hash_leaves_no_install() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("models");
        std::fs::create_dir_all(&root).unwrap();
        let tarball = make_tarball(tmp.path(), "sv-dir", &[("model.onnx", b"CORRUPT")]);
        let files = [ff("sv-dir/model.onnx", b"MODEL")]; // 期望哈希对不上
        assert!(extract_and_install(&tarball, &root, "sv-dir", &files).is_err());
        assert!(!root.join("sv-dir").exists(), "校验失败不得半安装");
    }

    #[test]
    fn sweep_tmp_removes_residue() {
        let tmp = tempfile::tempdir().unwrap();
        let d = tmp_extract_dir(tmp.path());
        std::fs::create_dir_all(&d).unwrap();
        std::fs::File::create(d.join("junk")).unwrap().write_all(b"x").unwrap();
        sweep_tmp(tmp.path());
        assert!(!d.exists());
    }
}
```

- [ ] **Step 3: 跑红**

```bash
cd src-tauri && cargo test models::download 2>&1 | tail -8
```
Expected: 5 FAIL（todo! panic）。

- [ ] **Step 4: 实现**

`src-tauri/src/models/download.rs`：

```rust
//! 模型下载器：断点续传 + SHA256 校验 + tar.bz2 解压进位。
//! 本文件的纯逻辑（镜像拼接/校验/解压）由单测覆盖；网络路径（download_artifact，
//! Task 3 添加）靠人工冒烟。

use super::FinalFile;
use sha2::{Digest, Sha256};
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};

/// 镜像前缀拼接：启用且前缀非空时 = prefix + 原完整 URL（ghproxy 风格），自动补尾 '/'。
pub fn apply_mirror(url: &str, enabled: bool, prefix: &str) -> String {
    let p = prefix.trim();
    if !enabled || p.is_empty() {
        return url.to_string();
    }
    if p.ends_with('/') {
        format!("{p}{url}")
    } else {
        format!("{p}/{url}")
    }
}

/// 流式计算文件 SHA256（hex 小写）。
pub fn sha256_file(path: &Path) -> anyhow::Result<String> {
    let mut f = fs::File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buf = [0u8; 64 * 1024];
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(hex::encode(hasher.finalize()))
}

/// 校验最终文件：先字节数（快）再 SHA256（慢），全对才 Ok。
pub fn verify_file(path: &Path, expected: &FinalFile) -> anyhow::Result<()> {
    let len = fs::metadata(path)?.len();
    if len != expected.bytes {
        anyhow::bail!("{} 大小不符: {len} != {}", expected.rel_path, expected.bytes);
    }
    let got = sha256_file(path)?;
    if got != expected.sha256 {
        anyhow::bail!("{} SHA256 校验失败", expected.rel_path);
    }
    Ok(())
}

/// 临时解压目录（root/.tmp-extract）。启动与每次下载前清扫残留。
pub fn tmp_extract_dir(root: &Path) -> PathBuf {
    root.join(".tmp-extract")
}

pub fn sweep_tmp(root: &Path) {
    let _ = fs::remove_dir_all(tmp_extract_dir(root));
}

/// 解压 tar.bz2 到 root/.tmp-extract，校验 files 后把 dest_dir 整体 rename 进位。
/// 任何一步失败都不触碰 root 下的既有安装。
pub fn extract_and_install(
    tarball: &Path,
    root: &Path,
    dest_dir: &str,
    files: &[FinalFile],
) -> anyhow::Result<()> {
    let tmp = tmp_extract_dir(root);
    let _ = fs::remove_dir_all(&tmp);
    fs::create_dir_all(&tmp)?;
    let f = fs::File::open(tarball)?;
    tar::Archive::new(bzip2::read::BzDecoder::new(f)).unpack(&tmp)?;
    let src = tmp.join(dest_dir);
    if !src.is_dir() {
        anyhow::bail!("压缩包内缺少目录 {dest_dir}");
    }
    // FinalFile.rel_path 相对 models root，而 tmp 镜像 root 布局，直接拼即可。
    for ff in files {
        verify_file(&tmp.join(ff.rel_path), ff)?;
    }
    let dst = root.join(dest_dir);
    let _ = fs::remove_dir_all(&dst);
    fs::rename(&src, &dst)?;
    let _ = fs::remove_dir_all(&tmp);
    Ok(())
}
```

`models/mod.rs` 文档注释后加 `pub mod download;`。

- [ ] **Step 5: 跑绿**

```bash
cd src-tauri && cargo test 2>&1 | tail -5
```
Expected: 84 + 5 = 89 passed。

- [ ] **Step 6: Commit**

```bash
git add src-tauri/Cargo.toml src-tauri/Cargo.lock src-tauri/src/models
git commit -m "feat(models): 下载器纯逻辑——镜像前缀/SHA256 校验/tar.bz2 解压原子进位

Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>"
```

---

### Task 3: 下载引擎 + 进度事件 + settings.json + commands

**Files:**
- Modify: `src-tauri/src/models/download.rs`（download_artifact 网络引擎）
- Create: `src-tauri/src/settings.rs`
- Modify: `src-tauri/src/ipc.rs`（ModelDownloadEvent）
- Modify: `src-tauri/src/lib.rs`（AppState 下载控制位、download_models/cancel_models_download/get_settings/set_settings commands、setup 清扫）

**Interfaces:**
- Consumes: Task 1 `models::{ARTIFACTS, artifact_present, root}`、`preload_models`；Task 2 `download::{apply_mirror, verify_file, extract_and_install, sweep_tmp}`。
- Produces: `download::download_artifact(a, root, url, cancel: &AtomicBool, progress: &dyn Fn(&str,&str,u64,u64,&str)) -> Result<()>`（取消时 Err 消息恰为 `"cancelled"`）；`settings::Settings { mirror_enabled, mirror_prefix }`（Serialize+Deserialize）、`settings::{load, save, DEFAULT_MIRROR_PREFIX}`；事件 `model_download`（`ipc::ModelDownloadEvent { artifact, phase, received_bytes, total_bytes, message }`，phase ∈ downloading/verifying/extracting/done/error/cancelled，artifact="all"+done=整体完成）；commands `download_models`/`cancel_models_download`/`get_settings`/`set_settings`。

- [ ] **Step 1: settings.rs 测试先行（同文件 tests，todo! 跑红）**

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn load_missing_or_corrupt_falls_back_to_default() {
        let tmp = tempfile::tempdir().unwrap();
        let s = load(tmp.path());
        assert!(!s.mirror_enabled);
        assert_eq!(s.mirror_prefix, DEFAULT_MIRROR_PREFIX);
        std::fs::write(tmp.path().join("settings.json"), "not json").unwrap();
        assert!(!load(tmp.path()).mirror_enabled, "损坏 → 默认值");
    }

    #[test]
    fn save_then_load_roundtrip() {
        let tmp = tempfile::tempdir().unwrap();
        let s = Settings { mirror_enabled: true, mirror_prefix: "https://mirror.example/".into() };
        save(tmp.path(), &s).unwrap();
        let got = load(tmp.path());
        assert!(got.mirror_enabled);
        assert_eq!(got.mirror_prefix, "https://mirror.example/");
        assert!(!tmp.path().join("settings.json.tmp").exists(), "原子写不留 tmp");
    }
}
```

- [ ] **Step 2: settings.rs 实现**

```rust
//! 轻量应用设置（app_data_dir/settings.json，原子写）。目前仅镜像加速配置。

use serde::{Deserialize, Serialize};
use std::path::Path;

pub const DEFAULT_MIRROR_PREFIX: &str = "https://ghproxy.net/";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Settings {
    #[serde(default)]
    pub mirror_enabled: bool,
    #[serde(default = "default_prefix")]
    pub mirror_prefix: String,
}

fn default_prefix() -> String {
    DEFAULT_MIRROR_PREFIX.into()
}

impl Default for Settings {
    fn default() -> Self {
        Self { mirror_enabled: false, mirror_prefix: default_prefix() }
    }
}

/// 缺失/损坏 → 默认值（容忍，不报错）。
pub fn load(app_data: &Path) -> Settings {
    std::fs::read_to_string(app_data.join("settings.json"))
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

pub fn save(app_data: &Path, s: &Settings) -> anyhow::Result<()> {
    std::fs::create_dir_all(app_data)?;
    let tmp = app_data.join("settings.json.tmp");
    std::fs::write(&tmp, serde_json::to_string_pretty(s)?)?;
    std::fs::rename(&tmp, app_data.join("settings.json"))?;
    Ok(())
}
```

lib.rs 模块声明区加 `mod settings;`。

- [ ] **Step 3: download_artifact 引擎（网络路径，无单测——纯逻辑已在 Task 2 覆盖，端到端走人工冒烟）**

追加到 `models/download.rs`：

```rust
use std::io::Write;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

/// 进度回调：(artifact_id, phase, received_bytes, total_bytes, message)。
pub type Progress = dyn Fn(&str, &str, u64, u64, &str);

/// 下载并安装单个工件。断点：root/<id>.part（HTTP Range 续传；服务端不支持则重下）。
/// cancel 置位 → Err 且消息恰为 "cancelled"（保留 .part 供续传）；
/// 校验/解压失败 → 删 .part（脏数据不值得续）并 Err。
pub fn download_artifact(
    a: &super::Artifact,
    root: &Path,
    url: &str,
    cancel: &AtomicBool,
    progress: &Progress,
) -> anyhow::Result<()> {
    fs::create_dir_all(root)?;
    let part = root.join(format!("{}.part", a.id));
    let mut offset = part.metadata().map(|m| m.len()).unwrap_or(0);

    let req = ureq::get(url).timeout(Duration::from_secs(600 * 60)); // 大文件慢链路：整体超时放极宽，靠取消兜底
    let req = if offset > 0 { req.set("Range", &format!("bytes={offset}-")) } else { req };
    let resp = req.call().map_err(|e| anyhow::anyhow!("请求失败: {e}"))?;
    let status = resp.status();
    let out: fs::File;
    if status == 206 {
        out = fs::OpenOptions::new().append(true).open(&part)?;
    } else if status == 200 {
        offset = 0; // 服务端不支持 Range（或首次下载）：从头来
        out = fs::File::create(&part)?;
    } else {
        anyhow::bail!("HTTP {status}");
    }
    let total = offset
        + resp
            .header("Content-Length")
            .and_then(|v| v.parse::<u64>().ok())
            .unwrap_or(0);
    let mut reader = resp.into_reader();
    let mut out = std::io::BufWriter::new(out);
    let mut received = offset;
    let mut buf = [0u8; 64 * 1024];
    let mut last_emit = Instant::now() - Duration::from_secs(1);
    loop {
        if cancel.load(Ordering::Relaxed) {
            drop(out); // 落盘已写字节，保留 .part
            anyhow::bail!("cancelled");
        }
        let n = reader.read(&mut buf)?;
        if n == 0 {
            break;
        }
        out.write_all(&buf[..n])?;
        received += n as u64;
        if last_emit.elapsed() >= Duration::from_millis(250) {
            last_emit = Instant::now();
            progress(a.id, "downloading", received, total, "");
        }
    }
    out.flush()?;
    drop(out);

    match &a.kind {
        super::ArtifactKind::File => {
            progress(a.id, "verifying", received, total, "");
            if let Err(e) = verify_file(&part, &a.files[0]) {
                let _ = fs::remove_file(&part);
                return Err(e);
            }
            fs::rename(&part, root.join(a.files[0].rel_path))?;
        }
        super::ArtifactKind::TarBz2 { dest_dir } => {
            progress(a.id, "extracting", received, total, "");
            if let Err(e) = extract_and_install(&part, root, dest_dir, a.files) {
                let _ = fs::remove_file(&part);
                return Err(e);
            }
            let _ = fs::remove_file(&part);
        }
    }
    progress(a.id, "done", received, total, "");
    Ok(())
}
```

- [ ] **Step 4: ipc.rs 事件 + lib.rs 接线**

`ipc.rs` 追加：

```rust
/// 模型下载进度，事件名 "model_download"。artifact="all" + phase="done" 表示整体完成。
/// phase: downloading | verifying | extracting | done | error | cancelled。
#[derive(Debug, Clone, Serialize)]
pub struct ModelDownloadEvent {
    pub artifact: String,
    pub phase: String,
    pub received_bytes: u64,
    pub total_bytes: u64,
    /// error 时的原因说明，其余为空串。
    pub message: String,
}
```

`lib.rs`：

1. `use std::sync::atomic::{AtomicBool, Ordering};`（文件头 use 区）。
2. `AppState` 追加两个字段（`#[derive(Default)]` 仍然成立——`Arc<AtomicBool>` 有 Default）：

```rust
    /// 模型下载互斥位（true = 下载线程在跑）与取消信号。
    download_running: Arc<AtomicBool>,
    download_cancel: Arc<AtomicBool>,
```

3. setup 里 `models::init_app_root(...)` 之后加一行清扫：`models::download::sweep_tmp(&models::root());`
4. 新 commands：

```rust
#[tauri::command]
fn download_models(app: AppHandle, state: State<AppState>) -> Result<(), String> {
    if state.download_running.swap(true, Ordering::SeqCst) {
        return Err("下载已在进行中".into());
    }
    state.download_cancel.store(false, Ordering::SeqCst);
    let running = state.download_running.clone();
    let cancel = state.download_cancel.clone();
    let recognizer_cache = state.recognizer_cache.clone();
    let embedder_cache = state.embedder_cache.clone();
    std::thread::spawn(move || {
        let root = models::root();
        models::download::sweep_tmp(&root);
        let s = app
            .path()
            .app_data_dir()
            .map(|d| settings::load(&d))
            .unwrap_or_default();
        let emit = |id: &str, phase: &str, received: u64, total: u64, message: &str| {
            let _ = app.emit(
                "model_download",
                ipc::ModelDownloadEvent {
                    artifact: id.into(),
                    phase: phase.into(),
                    received_bytes: received,
                    total_bytes: total,
                    message: message.into(),
                },
            );
        };
        let mut all_ok = true;
        for a in models::ARTIFACTS {
            if models::artifact_present(&root, a) {
                continue;
            }
            let url = models::download::apply_mirror(a.url, s.mirror_enabled, &s.mirror_prefix);
            if let Err(e) = models::download::download_artifact(a, &root, &url, &cancel, &emit) {
                all_ok = false;
                let msg = e.to_string();
                let phase = if msg == "cancelled" { "cancelled" } else { "error" };
                emit(a.id, phase, 0, 0, &msg);
                break;
            }
        }
        running.store(false, Ordering::SeqCst);
        if all_ok {
            emit("all", "done", 0, 0, "");
            // 补齐后立即预载，无需重启即可开录。
            preload_models(recognizer_cache, embedder_cache);
        }
    });
    Ok(())
}

#[tauri::command]
fn cancel_models_download(state: State<AppState>) {
    state.download_cancel.store(true, Ordering::SeqCst);
}

#[tauri::command]
fn get_settings(app: AppHandle) -> Result<settings::Settings, String> {
    app.path().app_data_dir().map(|d| settings::load(&d)).map_err(|e| e.to_string())
}

#[tauri::command]
fn set_settings(app: AppHandle, new_settings: settings::Settings) -> Result<(), String> {
    let d = app.path().app_data_dir().map_err(|e| e.to_string())?;
    settings::save(&d, &new_settings).map_err(|e| e.to_string())
}
```

注意 `emit` 闭包借用 `app`，而 `settings::load` 在闭包创建**前**用 `app`——顺序如上即可（load 先执行完）。四个 command 注册进 `generate_handler!`。

- [ ] **Step 5: 跑绿**

```bash
cd src-tauri && cargo test 2>&1 | tail -5   # 期望 89 + 2(settings) = 91 passed
cargo build 2>&1 | tail -3                   # 无 warning（download_artifact 已被 lib.rs 使用）
```

- [ ] **Step 6: Commit**

```bash
git add src-tauri/src/models/download.rs src-tauri/src/settings.rs src-tauri/src/ipc.rs src-tauri/src/lib.rs
git commit -m "feat(models): 下载引擎(Range 续传/取消/进度事件) + 镜像设置 + 四个 command

download_models 后台线程逐工件补齐,完成即预载;cancel 保留 .part 续传;
settings.json 存镜像开关与前缀。

Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>"
```

---

### Task 4: 前端——模型下载卡片 + 录制页集成

**Files:**
- Create: `src/lib/models.ts`
- Create: `src/lib/ModelDownloadCard.svelte`
- Modify: `src/routes/record/+page.svelte`

**Interfaces:**
- Consumes: Task 1/3 的 commands 与 `model_download` 事件（payload 字段见 Task 3 Produces）。
- Produces: `models.ts` 导出 `modelsStatus()/downloadModels()/cancelModelsDownload()/getSettings()/setSettings()/onModelDownload()` 与类型 `ModelsStatus/ArtifactState/Settings/ModelDownloadEvent`；`ModelDownloadCard` props `{ status: ModelsStatus, compact?: boolean, onComplete: () => void }`。

- [ ] **Step 1: models.ts**

```ts
import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";

export type ArtifactState = {
  id: string;
  label: string;
  approx_mb: number;
  required_for_recording: boolean;
  present: boolean;
};
export type ModelsStatus = {
  artifacts: ArtifactState[];
  recording_ready: boolean;
  diarization_ready: boolean;
};
export type Settings = { mirror_enabled: boolean; mirror_prefix: string };
export type ModelDownloadEvent = {
  artifact: string;
  phase: "downloading" | "verifying" | "extracting" | "done" | "error" | "cancelled";
  received_bytes: number;
  total_bytes: number;
  message: string;
};

export const modelsStatus = () => invoke<ModelsStatus>("models_status");
export const downloadModels = () => invoke<void>("download_models");
export const cancelModelsDownload = () => invoke<void>("cancel_models_download");
export const getSettings = () => invoke<Settings>("get_settings");
export const setSettings = (s: Settings) => invoke<void>("set_settings", { newSettings: s });
export function onModelDownload(cb: (e: ModelDownloadEvent) => void) {
  return listen<ModelDownloadEvent>("model_download", (ev) => cb(ev.payload));
}
```

- [ ] **Step 2: ModelDownloadCard.svelte**

```svelte
<script lang="ts">
  import { onMount } from "svelte";
  import {
    downloadModels,
    cancelModelsDownload,
    getSettings,
    setSettings,
    onModelDownload,
    type ModelsStatus,
    type Settings,
    type ModelDownloadEvent,
  } from "$lib/models";

  let {
    status,
    compact = false,
    onComplete,
  }: { status: ModelsStatus; compact?: boolean; onComplete: () => void } = $props();

  const missing = $derived(status.artifacts.filter((a) => !a.present));
  const totalMb = $derived(missing.reduce((s, a) => s + a.approx_mb, 0));

  let downloading = $state(false);
  let error = $state("");
  let cancelled = $state(false);
  /** 各工件进度：received/total 字节 + phase。 */
  let prog = $state<Record<string, { received: number; total: number; phase: string }>>({});
  let settings = $state<Settings | null>(null);

  onMount(() => {
    getSettings().then((s) => (settings = s)).catch(() => {});
    // 事件监听随组件生命周期注册/解绑（下载跨页面继续，回到本页重新拿到进度流）。
    const un = onModelDownload(handle);
    return () => {
      un.then((f) => f());
    };
  });

  function handle(e: ModelDownloadEvent) {
    if (e.artifact === "all" && e.phase === "done") {
      downloading = false;
      onComplete();
      return;
    }
    if (e.phase === "error") {
      downloading = false;
      error = e.message;
      return;
    }
    if (e.phase === "cancelled") {
      downloading = false;
      cancelled = true;
      return;
    }
    prog = { ...prog, [e.artifact]: { received: e.received_bytes, total: e.total_bytes, phase: e.phase } };
    if (e.phase === "done") onComplete(); // 单工件完成：刷新 present 态
  }

  async function start() {
    error = "";
    cancelled = false;
    downloading = true;
    try {
      await downloadModels();
    } catch (e) {
      // "下载已在进行中" 不算错：保持 downloading 态继续收进度事件。
      if (!String(e).includes("已在进行中")) {
        downloading = false;
        error = String(e);
      }
    }
  }

  async function toggleMirror() {
    if (!settings) return;
    settings = { ...settings, mirror_enabled: !settings.mirror_enabled };
    await setSettings(settings);
  }
  async function savePrefix() {
    if (settings) await setSettings(settings);
  }

  const pct = (p: { received: number; total: number }) =>
    p.total > 0 ? Math.min(100, Math.floor((p.received / p.total) * 100)) : 0;
  const mb = (n: number) => (n / 1024 / 1024).toFixed(0);
  const phaseText: Record<string, string> = {
    downloading: "下载中",
    verifying: "校验中",
    extracting: "解压中",
    done: "完成",
  };
</script>

<div class="card" class:compact>
  {#if compact}
    <span>说话人区分需补下声纹模型（约 {totalMb}MB）。</span>
  {:else}
    <h2>下载语音模型</h2>
    <p class="desc">首次使用需下载识别模型（共约 {totalMb}MB），全程本地运行、不上传任何音频。</p>
  {/if}

  {#each missing as a (a.id)}
    <div class="row">
      <span class="label">{a.label} · 约 {a.approx_mb}MB</span>
      {#if prog[a.id]}
        <span class="phase">
          {phaseText[prog[a.id].phase] ?? prog[a.id].phase}
          {#if prog[a.id].phase === "downloading" && prog[a.id].total > 0}
            {mb(prog[a.id].received)}/{mb(prog[a.id].total)}MB
          {/if}
        </span>
        <div class="bar"><div class="fill" style="width:{pct(prog[a.id])}%"></div></div>
      {/if}
    </div>
  {/each}

  {#if error}
    <div class="error">下载失败：{error}（已下载部分已保留，重试将续传）</div>
  {/if}
  {#if cancelled}
    <div class="hint">已暂停下载，已下载部分保留，可随时继续。</div>
  {/if}

  <div class="actions">
    {#if downloading}
      <button onclick={() => cancelModelsDownload()}>暂停下载</button>
    {:else}
      <button class="primary" onclick={start}>{error || cancelled ? "继续下载" : "下载模型"}</button>
    {/if}
    {#if settings && !compact}
      <label class="mirror">
        <input type="checkbox" checked={settings.mirror_enabled} onchange={toggleMirror} />
        使用镜像加速（国内网络推荐）
      </label>
      {#if settings.mirror_enabled}
        <input class="prefix" bind:value={settings.mirror_prefix} onblur={savePrefix} placeholder="镜像前缀，如 https://ghproxy.net/" />
      {/if}
    {/if}
  </div>
</div>

<style>
  .card {
    background: #f5f5f7;
    border-radius: 10px;
    padding: 1rem 1.2rem;
    margin: 0.5rem 0 1rem;
  }
  .card.compact {
    display: flex;
    flex-wrap: wrap;
    align-items: center;
    gap: 0.6rem;
    background: #fff4e5;
    border: 1px solid #f0c98a;
    color: #8a5a00;
    font-size: 0.95rem;
  }
  h2 { margin: 0 0 0.25rem; font-size: 1.1rem; }
  .desc { color: #666; margin: 0 0 0.75rem; font-size: 0.9rem; }
  .row { margin: 0.4rem 0; }
  .label { font-size: 0.9rem; }
  .phase { color: #666; font-size: 0.8rem; margin-left: 0.5em; }
  .bar { height: 6px; background: #e0e0e3; border-radius: 3px; margin-top: 0.25rem; overflow: hidden; }
  .fill { height: 100%; background: #396cd8; transition: width 0.3s; }
  .actions { display: flex; align-items: center; gap: 0.8rem; margin-top: 0.8rem; flex-wrap: wrap; }
  button { border-radius: 8px; border: 1px solid #ccc; padding: 0.45em 1.1em; cursor: pointer; background: #fff; }
  button.primary { background: #396cd8; color: #fff; border-color: transparent; font-weight: 600; }
  .mirror { font-size: 0.85rem; display: flex; align-items: center; gap: 0.3em; }
  .prefix { flex: 1; min-width: 14rem; padding: 0.3em 0.5em; border-radius: 6px; border: 1px solid #ccc; font-size: 0.85rem; }
  .error { color: #c0392b; font-size: 0.9rem; margin-top: 0.5rem; }
  .hint { color: #8a5a00; font-size: 0.9rem; margin-top: 0.5rem; }
  @media (prefers-color-scheme: dark) {
    .card { background: #2a2a2a; }
    .card.compact { background: #3a2e18; border-color: #6b5426; color: #e8c88a; }
    .desc, .phase { color: #aaa; }
    .bar { background: #444; }
    button { background: #0f0f0f98; color: #fff; border-color: #555; }
    .prefix { background: #2a2a2a; color: #f0f0f0; border-color: #555; }
  }
</style>
```

- [ ] **Step 3: record/+page.svelte 集成**

script 区追加：

```ts
import { onMount } from "svelte";
import { modelsStatus, type ModelsStatus } from "$lib/models";
import ModelDownloadCard from "$lib/ModelDownloadCard.svelte";

let models = $state<ModelsStatus | null>(null);
async function refreshModels() {
  try {
    models = await modelsStatus();
  } catch {
    /* 查询失败按就绪处理，不挡老用户 */
  }
}
onMount(refreshModels);
```

markup：`<h1>实时转写</h1>` 之后、状态行之前插入：

```svelte
{#if models && !models.recording_ready}
  <ModelDownloadCard status={models} onComplete={refreshModels} />
{:else if models && !models.diarization_ready}
  <ModelDownloadCard status={models} compact onComplete={refreshModels} />
{/if}
```

并把原「状态 + 横幅 + chips + transcript」整块包进 `{#if !models || models.recording_ready} ... {/if}`（录制必需模型缺失时只显示下载卡片；`models` 尚未加载完为 null 时按就绪渲染避免闪烁）。

- [ ] **Step 4: 检查 + 提交**

```bash
npm run check 2>&1 | tail -3   # 0 errors
npm run build 2>&1 | tail -3   # 构建成功
git add src/lib/models.ts src/lib/ModelDownloadCard.svelte src/routes/record/+page.svelte
git commit -m "feat(ui): 录制页模型下载卡片(进度/续传/镜像开关) + 缺声纹小提示条

Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>"
```

---

### Task 5: 管线暂停闸 + 麦克风电平回调（segment_worker / session）

**Files:**
- Modify: `src-tauri/src/pipeline/segment_worker.rs`
- Modify: `src-tauri/src/session.rs`（start_session 签名 + RecordingHandle）
- Modify: `src-tauri/src/lib.rs`（start_session 调用点补 None，本任务不接真回调）
- Modify: `src-tauri/src/pipeline/silero.rs`（加模型门控 flush 时间轴测试）

**Interfaces:**
- Consumes: 既有 `Segmenter::{accept, take_finished, current_partial, flush}`、`FinalJob/PartialJob`。
- Produces: `run_segment_worker(..., paused: Arc<AtomicBool>, on_level: Option<Box<dyn Fn(f32) + Send>>)`（新增末两参）；`start_session(..., on_mic_level: Option<Box<dyn Fn(f32) + Send>>)`（新增末参，仅 Mic worker 收到）；`RecordingHandle::set_paused(&self, bool)`；常量 `pipeline::segment_worker::LEVEL_INTERVAL_SAMPLES = 1600`（100ms @16k）。

- [ ] **Step 1: 写失败测试（segment_worker.rs tests 追加）**

```rust
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::Duration;

    #[test]
    fn pause_flushes_inflight_drops_frames_and_unpause_resumes_monotonic() {
        let (ftx, frx) = crossbeam_channel::bounded::<AudioFrame>(256);
        let (final_tx, final_rx) = crossbeam_channel::unbounded::<FinalJob>();
        let slot = Arc::new(Mutex::new(None));
        let paused = Arc::new(AtomicBool::new(false));
        let (p2, s2) = (paused.clone(), slot.clone());
        let worker = std::thread::spawn(move || {
            run_segment_worker(
                Source::Mic, frx, 16000, 4000, final_tx, s2,
                Box::new(MockSegmenter::new(2000)), p2, None,
            );
        });
        let frame = |n: usize| AudioFrame { samples: vec![0.1; n], sample_rate: 16000, channels: 1 };

        // 1) 2500 样本 → 1 段定稿(2000)，在途 500。
        ftx.send(frame(2500)).unwrap();
        let first = final_rx.recv_timeout(Duration::from_secs(2)).expect("首段");
        assert_eq!(first.samples.len(), 2000);

        // 2) 置暂停，下一帧触发跳变 → 在途 500 被 flush 定稿；该帧本身被丢。
        paused.store(true, Ordering::Relaxed);
        ftx.send(frame(100)).unwrap();
        let flushed = final_rx.recv_timeout(Duration::from_secs(2)).expect("暂停跳变 flush");
        assert_eq!(flushed.samples.len(), 500, "在途语句在暂停瞬间定稿，不丢已说的话");
        assert!(slot.lock().unwrap().is_none(), "暂停后 partial 槽清空");

        // 3) 暂停期灌 4000 样本（本可切 2 段）→ 不得产段。
        ftx.send(frame(4000)).unwrap();
        assert!(
            final_rx.recv_timeout(Duration::from_millis(300)).is_err(),
            "暂停期丢帧，不产段"
        );

        // 4) 恢复后 2000 样本 → 恢复产段，且时间轴单调（暂停期不前进）。
        paused.store(false, Ordering::Relaxed);
        ftx.send(frame(2000)).unwrap();
        let resumed = final_rx.recv_timeout(Duration::from_secs(2)).expect("恢复产段");
        assert_eq!(resumed.samples.len(), 2000);
        assert!(resumed.start_ms >= flushed.end_ms, "恢复后时间戳接续，不回退不重叠");

        drop(ftx);
        worker.join().unwrap();
    }

    #[test]
    fn level_callback_throttles_and_survives_pause() {
        let calls = Arc::new(Mutex::new(Vec::<f32>::new()));
        let c2 = calls.clone();
        let (ftx, frx) = crossbeam_channel::bounded::<AudioFrame>(16);
        let (final_tx, _final_rx) = crossbeam_channel::unbounded::<FinalJob>();
        let slot = Arc::new(Mutex::new(None));
        let paused = Arc::new(AtomicBool::new(true)); // 全程暂停：电平仍须上报
        let worker = std::thread::spawn(move || {
            run_segment_worker(
                Source::Mic, frx, 16000, 4000, final_tx, slot,
                Box::new(MockSegmenter::new(2000)), paused,
                Some(Box::new(move |v| c2.lock().unwrap().push(v))),
            );
        });
        // 两帧、每帧恰好 LEVEL_INTERVAL_SAMPLES(1600) 个 0.5 → 各触发一次回调，RMS≈0.5。
        let frame = AudioFrame { samples: vec![0.5; LEVEL_INTERVAL_SAMPLES], sample_rate: 16000, channels: 1 };
        ftx.send(frame.clone()).unwrap();
        ftx.send(frame).unwrap();
        drop(ftx);
        worker.join().unwrap();
        let got = calls.lock().unwrap();
        assert_eq!(got.len(), 2, "按 1600 样本节流：两帧两次");
        assert!((got[0] - 0.5).abs() < 1e-3, "RMS 计算正确: {}", got[0]);
    }
```

- [ ] **Step 2: 跑红**

```bash
cd src-tauri && cargo test pipeline::segment_worker 2>&1 | tail -8
```
Expected: 编译失败（run_segment_worker 参数个数不符）——即为红。

- [ ] **Step 3: 实现 run_segment_worker**

替换 `segment_worker.rs` 的实现部分（tests 之外）：

```rust
use crate::audio::{resample::resample_linear, to_mono, AudioFrame, Source};
use crate::pipeline::segmenter::Segmenter;
use crate::session::{FinalJob, PartialJob};
use crossbeam_channel::{Receiver, Sender};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

/// 电平上报节流窗口：1600 样本 = 100ms @16kHz。
pub const LEVEL_INTERVAL_SAMPLES: usize = 1600;

/// 把 segmenter 里已完成的段全部定稿发出，返回段数。定稿即清过时 partial 预览。
fn emit_finished(
    segmenter: &mut Box<dyn Segmenter>,
    partial_slot: &Arc<Mutex<Option<PartialJob>>>,
    finals_tx: &Sender<FinalJob>,
    source: Source,
    target_rate: u32,
) -> usize {
    let ms = |samples: usize| samples as u64 * 1000 / target_rate as u64;
    let mut n = 0;
    for seg in segmenter.take_finished() {
        *partial_slot.lock().unwrap() = None;
        let (start_ms, end_ms) = (ms(seg.start), ms(seg.start + seg.samples.len()));
        if finals_tx
            .send(FinalJob { source, samples: seg.samples, start_ms, end_ms })
            .is_err()
        {
            eprintln!("segment_worker: finals 通道已关闭，一段完成句被丢弃 ({source:?})");
        }
        n += 1;
    }
    n
}

/// 单源分段 worker：frame_rx 取原生帧 → 归一 16kHz 单声道 → VAD 分段。
/// 完成句 → finals_tx.send(FinalJob)；当前句按采样节流 → 覆盖 partial_slot。
/// frame_rx 关闭（采集停止/结束）后 flush 尾段并返回。
///
/// paused 置位期间丢帧（时间轴冻结）；false→true 跳变瞬间把在途语句 flush 定稿。
/// on_level（仅 mic 路传入）在闸前对归一后样本算 RMS、按 LEVEL_INTERVAL_SAMPLES
/// 节流上报——暂停期间持续，供 UI 确认麦克风存活。
pub fn run_segment_worker(
    source: Source,
    frame_rx: Receiver<AudioFrame>,
    target_rate: u32,
    partial_interval_samples: usize,
    finals_tx: Sender<FinalJob>,
    partial_slot: Arc<Mutex<Option<PartialJob>>>,
    mut segmenter: Box<dyn Segmenter>,
    paused: Arc<AtomicBool>,
    on_level: Option<Box<dyn Fn(f32) + Send>>,
) {
    let mut since_partial: usize = 0;
    let mut was_paused = false;
    let mut level_sumsq: f64 = 0.0;
    let mut level_count: usize = 0;
    for frame in frame_rx.iter() {
        let mono = to_mono(&frame.samples, frame.channels);
        let resampled = resample_linear(&mono, frame.sample_rate, target_rate);

        if let Some(cb) = &on_level {
            level_sumsq += resampled.iter().map(|s| (*s as f64) * (*s as f64)).sum::<f64>();
            level_count += resampled.len();
            if level_count >= LEVEL_INTERVAL_SAMPLES {
                cb((level_sumsq / level_count as f64).sqrt() as f32);
                level_sumsq = 0.0;
                level_count = 0;
            }
        }

        if paused.load(Ordering::Relaxed) {
            if !was_paused {
                was_paused = true;
                // 暂停跳变：在途语句立刻定稿（不丢已说的话），清预览。
                segmenter.flush();
                emit_finished(&mut segmenter, &partial_slot, &finals_tx, source, target_rate);
                *partial_slot.lock().unwrap() = None;
                since_partial = 0;
            }
            continue; // 丢帧：暂停期时间轴冻结
        }
        was_paused = false;

        since_partial += resampled.len();
        segmenter.accept(&resampled);
        if emit_finished(&mut segmenter, &partial_slot, &finals_tx, source, target_rate) > 0 {
            since_partial = 0;
        }
        if since_partial >= partial_interval_samples {
            since_partial = 0;
            *partial_slot.lock().unwrap() =
                segmenter.current_partial().map(|cur| PartialJob { source, samples: cur });
        }
    }

    // 采集结束：尾段定稿
    segmenter.flush();
    emit_finished(&mut segmenter, &partial_slot, &finals_tx, source, target_rate);
}
```

既有两个测试的调用点补 `Arc::new(AtomicBool::new(false)), None` 两参（`stale_partial_cleared_when_throttle_returns_none` 与 `segment_worker_tags_finals_with_source`）。

- [ ] **Step 4: session.rs——RecordingHandle 暂停位 + start_session 签名**

`RecordingHandle` 定义与 stop 后追加：

```rust
pub struct RecordingHandle {
    captures: Vec<Box<dyn AudioCapture>>,
    workers: Vec<std::thread::JoinHandle<()>>,
    asr: Option<std::thread::JoinHandle<(Box<dyn Recognizer>, Option<Box<dyn SpeakerEmbedder>>)>>,
    /// 各 segment_worker 共享的暂停闸（true = 丢帧，时间轴冻结）。
    paused: Arc<std::sync::atomic::AtomicBool>,
}

impl RecordingHandle {
    /// 置暂停闸。跳变瞬间的在途语句 flush 由 worker 侧完成（见 run_segment_worker）。
    pub fn set_paused(&self, v: bool) {
        self.paused.store(v, std::sync::atomic::Ordering::Relaxed);
    }
    // ... 既有 stop 不变
}
```

`start_session` 签名追加末参 `on_mic_level: Option<Box<dyn Fn(f32) + Send>>`；函数体开头 `let paused = Arc::new(std::sync::atomic::AtomicBool::new(false)); let mut mic_level = on_mic_level;`；sources 循环里 worker spawn 改为：

```rust
        let level_cb = if source == Source::Mic { mic_level.take() } else { None };
        let paused_w = paused.clone();
        let w = std::thread::spawn(move || {
            run_segment_worker(
                source,
                frx,
                target_rate,
                partial_interval_samples,
                final_tx,
                slot_for_worker,
                segmenter,
                paused_w,
                level_cb,
            );
        });
```

返回值 `RecordingHandle { captures, workers, asr: Some(asr), paused }`。

调用点全部补末参：lib.rs `session::start_session(..., None)`（Task 6 换真回调）；session.rs 测试里所有 `start_session(` 调用补 `None`。

- [ ] **Step 5: silero.rs 模型门控测试——flush 中途调用时间轴延续**

暂停依赖「sherpa VAD flush ≠ reset、样本索引继续」这一未验证假设，加门控测试钉死。`silero.rs` 文件尾（若无 tests 模块则新建）：

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::pipeline::segmenter::Segmenter;

    /// 暂停功能依赖：flush 之后继续 accept，段的 start 样本偏移必须延续而非归零。
    /// 需要真实模型：cargo test -- --ignored（或 VN_MODELS 指向模型目录）。
    #[test]
    #[ignore]
    fn flush_midstream_keeps_timeline_monotonic() {
        let model = crate::models::root().join("silero_vad.onnx");
        let mut seg = SileroSegmenter::new(&model).expect("加载 VAD");
        let wav = {
            let mut r = hound::WavReader::open(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/tests/fixtures/sample_16k.wav"
            ))
            .expect("fixture");
            r.samples::<i16>().map(|s| s.unwrap() as f32 / 32768.0).collect::<Vec<f32>>()
        };
        seg.accept(&wav);
        seg.flush();
        let a = seg.take_finished();
        assert!(!a.is_empty(), "fixture 是真实语音，flush 应产段");
        seg.accept(&wav);
        seg.flush();
        let b = seg.take_finished();
        assert!(!b.is_empty());
        let last_a = a.last().unwrap();
        assert!(
            b[0].start >= last_a.start + last_a.samples.len(),
            "flush 后时间轴延续不重叠: b.start={} vs a.end={}",
            b[0].start,
            last_a.start + last_a.samples.len()
        );
    }
}
```

- [ ] **Step 6: 跑绿（含门控）**

```bash
cd src-tauri && cargo test 2>&1 | tail -5              # 91 + 2 = 93 passed
cargo test -- --ignored flush_midstream 2>&1 | tail -5 # 门控测试 1 passed（本机有模型）
```
**若门控测试失败**（sherpa flush 会重置索引）：停下，把发现记进 progress.md，改用 worker 侧维护时间轴偏移校正（`pause_offset += 跳变时刻已产出的最大 end 样本 - flush 后首段 start`）再继续——这是计划内的已知风险点。

- [ ] **Step 7: Commit**

```bash
git add src-tauri/src/pipeline/segment_worker.rs src-tauri/src/session.rs src-tauri/src/lib.rs src-tauri/src/pipeline/silero.rs
git commit -m "feat(pipeline): 暂停闸(跳变 flush 在途语句/丢帧冻结时间轴) + 麦克风电平回调

RecordingHandle::set_paused;start_session 增 on_mic_level(仅 mic worker);
电平在闸前算 RMS 按 1600 样本节流,暂停期持续上报;
silero 门控测试钉死 flush 中途时间轴延续假设。

Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>"
```

---

### Task 6: 暂停/恢复 commands + 计时器 + 电平事件（lib.rs / ipc.rs）

**Files:**
- Modify: `src-tauri/src/ipc.rs`（StatusEvent 加 elapsed_ms；LevelEvent）
- Modify: `src-tauri/src/lib.rs`（ActiveSession 计时字段、pause/unpause commands、recording_status、level 接线、全部 StatusEvent 构造点补字段）

**Interfaces:**
- Consumes: Task 5 `RecordingHandle::set_paused`、`start_session(..., on_mic_level)`。
- Produces: `ipc::StatusEvent` 增 `elapsed_ms: u64`（活跃毫秒，含续录 base_ms；非录制态为 0）；status 事件 state 新增 `"paused"`；`ipc::LevelEvent { rms: f32 }` 事件名 `"level"`；commands `pause_recording`/`unpause_recording`（非录制中 → Err「没有正在进行的录制」，重复调用幂等 Ok）；`recording_status` 在暂停时返回 state="paused" 且总带 elapsed_ms。

- [ ] **Step 1: 写失败测试（lib.rs tests 模块；lib.rs 目前无内嵌 tests，新建在文件尾）**

计时算法抽纯函数测试：

```rust
#[cfg(test)]
mod tests {
    use super::active_elapsed_ms;
    use std::time::Duration;

    #[test]
    fn active_elapsed_subtracts_pauses_and_adds_base() {
        let s = Duration::from_secs;
        assert_eq!(active_elapsed_ms(s(10), s(0), None, 0), 10_000, "无暂停");
        assert_eq!(active_elapsed_ms(s(10), s(3), None, 0), 7_000, "扣已累计暂停");
        assert_eq!(active_elapsed_ms(s(10), s(3), Some(s(2)), 0), 5_000, "再扣当前暂停");
        assert_eq!(active_elapsed_ms(s(10), s(0), None, 60_000), 70_000, "续录加 base_ms");
        assert_eq!(active_elapsed_ms(s(1), s(5), None, 0), 0, "异常倒挂饱和为 0 不 panic");
    }
}
```

- [ ] **Step 2: 跑红**

```bash
cd src-tauri && cargo test active_elapsed 2>&1 | tail -5
```
Expected: 编译失败（函数不存在）。

- [ ] **Step 3: 实现**

**ipc.rs**：`StatusEvent` 加字段（注释一并更新）：

```rust
    /// 活跃录制毫秒数（不含暂停期；续录含历史 base_ms）。仅 recording/paused 状态
    /// 有意义，其余为 0。
    pub elapsed_ms: u64,
```

并新增：

```rust
/// 麦克风电平（闸前 RMS，0..1 量级），事件名 "level"，约 10Hz。
#[derive(Debug, Clone, Serialize)]
pub struct LevelEvent {
    pub rms: f32,
}
```

**lib.rs**：

1. 纯函数 + ActiveSession 字段：

```rust
/// 活跃时长 = 总 wall 时长 - 已累计暂停 - 当前暂停中时长，再加续录基线 base_ms。
/// checked_sub 兜底：时钟异常倒挂时饱和为 0 而非 panic。
fn active_elapsed_ms(
    total: std::time::Duration,
    paused_accum: std::time::Duration,
    current_pause: Option<std::time::Duration>,
    base_ms: u64,
) -> u64 {
    let active = total
        .checked_sub(paused_accum + current_pause.unwrap_or_default())
        .unwrap_or_default();
    base_ms + active.as_millis() as u64
}
```

`ActiveSession` 追加：

```rust
    /// 计时：会话入槽时刻、续录基线、暂停起点（Some=暂停中）、已累计暂停时长。
    started: std::time::Instant,
    base_ms: u64,
    paused_at: Option<std::time::Instant>,
    paused_accum: std::time::Duration,
```

```rust
impl ActiveSession {
    fn elapsed_ms(&self) -> u64 {
        active_elapsed_ms(
            self.started.elapsed(),
            self.paused_accum,
            self.paused_at.map(|p| p.elapsed()),
            self.base_ms,
        )
    }
}
```

2. spawn_session 存 session 处（`*session_slot.lock().unwrap() = Some(ActiveSession {...})`）补：`base_ms, started: std::time::Instant::now(), paused_at: None, paused_accum: std::time::Duration::ZERO,`（`base_ms` 变量已有）。随后的 "recording" emit 补 `elapsed_ms: base_ms`。

3. 全部 StatusEvent 构造点补 `elapsed_ms: 0`：fail 闭包（1 处）、stop_recording（1 处）、recording_status idle 分支（1 处）。recording_status 的 recording 分支改为：

```rust
        Some(s) => ipc::StatusEvent {
            state: if s.paused_at.is_some() { "paused".into() } else { "recording".into() },
            system_audio: s.system_audio.clone(),
            note_id: s.note_id.clone(),
            diarization: s.diarization.clone(),
            elapsed_ms: s.elapsed_ms(),
        },
```

4. 新 commands（emit 全部在锁外）：

```rust
#[tauri::command]
fn pause_recording(app: AppHandle, state: State<AppState>) -> Result<(), String> {
    let ev = {
        let mut slot = state.session.lock().unwrap();
        let Some(s) = slot.as_mut() else { return Err("没有正在进行的录制".into()) };
        if s.paused_at.is_some() {
            return Ok(()); // 已暂停：幂等
        }
        s.handle.set_paused(true);
        s.paused_at = Some(std::time::Instant::now());
        ipc::StatusEvent {
            state: "paused".into(),
            system_audio: s.system_audio.clone(),
            note_id: s.note_id.clone(),
            diarization: s.diarization.clone(),
            elapsed_ms: s.elapsed_ms(),
        }
    };
    let _ = app.emit("status", ev);
    Ok(())
}

#[tauri::command]
fn unpause_recording(app: AppHandle, state: State<AppState>) -> Result<(), String> {
    let ev = {
        let mut slot = state.session.lock().unwrap();
        let Some(s) = slot.as_mut() else { return Err("没有正在进行的录制".into()) };
        let Some(p) = s.paused_at.take() else { return Ok(()) }; // 未暂停：幂等
        s.paused_accum += p.elapsed();
        s.handle.set_paused(false);
        ipc::StatusEvent {
            state: "recording".into(),
            system_audio: s.system_audio.clone(),
            note_id: s.note_id.clone(),
            diarization: s.diarization.clone(),
            elapsed_ms: s.elapsed_ms(),
        }
    };
    let _ = app.emit("status", ev);
    Ok(())
}
```

注册进 `generate_handler!`。

5. 电平接线——spawn_session 里 start_session 调用的末参 `None` 换成：

```rust
            {
                let app_l = app.clone();
                Some(Box::new(move |rms: f32| {
                    let _ = app_l.emit("level", ipc::LevelEvent { rms });
                }) as Box<dyn Fn(f32) + Send>)
            },
```

- [ ] **Step 4: 跑绿**

```bash
cd src-tauri && cargo test 2>&1 | tail -5   # 93 + 1 = 94 passed
```

- [ ] **Step 5: Commit**

```bash
git add src-tauri/src/ipc.rs src-tauri/src/lib.rs
git commit -m "feat(record): pause/unpause commands + 后端计时(活跃时长真值源) + level 事件

StatusEvent 增 elapsed_ms 与 paused 态;recording_status 暂停返回 paused;
计时纯函数 active_elapsed_ms 单测覆盖(扣暂停/加续录基线/饱和防倒挂)。

Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>"
```

---

### Task 7: 前端——录制控制条（暂停/恢复/计时/电平）+ 状态水合

**Files:**
- Modify: `src/lib/events.ts`（StatusEvent.elapsed_ms、LevelEvent、onLevel）
- Modify: `src/lib/recording.svelte.ts`（paused/计时/电平/pause()/unpause()/水合，遗留#7）
- Modify: `src/routes/record/+page.svelte`（控制条）
- Modify: `src/lib/Sidebar.svelte`（isLive 联动）
- Modify: `src/routes/notes/[id]/+page.svelte`（「继续录制」disabled 改 isLive）

**Interfaces:**
- Consumes: Task 6 的 `elapsed_ms`/`"paused"` 状态/`level` 事件、`pause_recording`/`unpause_recording` commands。
- Produces: `recording` store 新增 getters `paused: boolean`、`isLive: boolean`（recording ∥ paused）、`elapsedMs: number`（1s tick 刷新）、`level: number`（0..1 原始 rms）；方法 `pause()`/`unpause()`。**既有 `isRecording` 语义不变**（仅 status==="recording"）。

- [ ] **Step 1: events.ts**

`StatusEvent` 类型加 `elapsed_ms: number;`。文件尾追加：

```ts
export type LevelEvent = { rms: number };

export function onLevel(cb: (e: LevelEvent) => void) {
  return listen<LevelEvent>("level", (ev) => cb(ev.payload));
}
```

- [ ] **Step 2: recording.svelte.ts**

state 区追加：

```ts
let paused = $state(false);
/** 计时基线（后端 elapsed_ms 快照）+ 本地锚点（recording 态才走表）。 */
let elapsedBaseMs = $state(0);
let tickAnchor = $state<number | null>(null);
let nowTick = $state(Date.now());
let level = $state(0);
```

`import { onLevel } from "./events";`（并入既有 import）。

对象追加 getters/方法：

```ts
  get paused() { return paused; },
  get isLive() { return status === "recording" || status === "paused"; },
  get level() { return level; },
  /** 活跃录制毫秒：后端快照 + 本地走表（暂停/停止时不走）。 */
  get elapsedMs() { return elapsedBaseMs + (tickAnchor !== null ? nowTick - tickAnchor : 0); },

  async pause() {
    if (pending || status !== "recording") return;
    pending = true;
    try { await invoke("pause_recording"); } finally { pending = false; }
  },
  async unpause() {
    if (pending || status !== "paused") return;
    pending = true;
    try { await invoke("unpause_recording"); } finally { pending = false; }
  },
```

`init()` 里：

1. 注册电平与秒表（onStatus 之后）：

```ts
    onLevel((e) => {
      level = e.rms;
    });
    setInterval(() => {
      nowTick = Date.now();
    }, 1000);
```

2. onStatus 回调整体替换为（新增 paused 分支与「恢复/对账不清屏」守卫，即遗留#7 的事件侧）：

```ts
    onStatus((e) => {
      if (e.state === "recording") {
        // 同一笔记且此前已是 live（暂停恢复/重复对账）：只更新计时与暂停位，不清屏。
        const isUnpause = e.note_id === noteId && (status === "recording" || status === "paused");
        status = e.state;
        systemAudio = e.system_audio;
        diarization = e.diarization;
        paused = false;
        elapsedBaseMs = e.elapsed_ms;
        tickAnchor = Date.now();
        if (isUnpause) return;
        noteId = e.note_id;
        if (resuming) {
          resuming = false;
          partialMic = "";
          partialSystem = "";
          storageDegraded = false;
          statusVersion++;
        } else {
          finals = [];
          partialMic = "";
          partialSystem = "";
          storageDegraded = false;
          speakers = {};
          statusVersion++;
        }
      } else if (e.state === "paused") {
        status = "paused";
        paused = true;
        elapsedBaseMs = e.elapsed_ms;
        tickAnchor = null;
        partialMic = "";
        partialSystem = "";
      } else if (e.state === "stopped" || e.state.startsWith("error:")) {
        status = e.state;
        systemAudio = e.system_audio;
        diarization = e.diarization;
        resuming = false;
        paused = false;
        elapsedBaseMs = 0;
        tickAnchor = null;
        level = 0;
        partialMic = "";
        partialSystem = "";
        storageDegraded = false;
        statusVersion++;
        if (e.state === "stopped" && e.note_id) {
          goto(`/notes/${e.note_id}`);
        }
      } else {
        status = e.state;
      }
    });
```

（注意原实现在函数体首行统一 `status = e.state` 等赋值；替换后各分支自行赋值，保持既有语义。）

3. 冷启动对账分支改为（含遗留#7 的水合）：

```ts
    const s = await invoke<StatusEvent>("recording_status");
    if (s.state === "recording" || s.state === "paused") {
      status = s.state;
      systemAudio = s.system_audio;
      diarization = s.diarization;
      noteId = s.note_id;
      paused = s.state === "paused";
      elapsedBaseMs = s.elapsed_ms;
      tickAnchor = s.state === "recording" ? Date.now() : null;
      await hydrateFromDisk(s.note_id);
    }
```

4. 模块级水合函数（`recording` 对象外、文件内）：

```ts
/** 冷刷新/对账时用磁盘内容回灌 finals+speakers（录制中笔记边录边落盘，直接可读）。 */
async function hydrateFromDisk(id: string) {
  if (!id) return;
  try {
    const note = await getNote(id);
    finals = note.segments
      .filter((s) => s.text.trim())
      .map((s) => ({ source: s.source, text: s.text, speaker: s.speaker }));
    speakers = { ...note.speakers };
  } catch {
    // 水合失败仅影响历史段回显，不阻塞录制状态重建。
  }
}
```

5. `start()` 与 `resume()` 的「已在录制」对账分支：在 `noteId = s.note_id;` 之后各加一行 `await hydrateFromDisk(s.note_id);`；两处 `if (pending || status === "recording") return false;` 改为 `if (pending || this.isLive) return false;`——注意对象字面量方法里 `this` 可用（调用方式恒为 `recording.start()`）；`stop()` 不变。

- [ ] **Step 3: record/+page.svelte 控制条**

script 追加：

```ts
import { formatTs } from "$lib/notes";

async function startRecording() {
  await recording.start(); // 已在录制页，无需跳转
}
const levelPct = $derived.by(() => {
  if (!recording.isLive || recording.level <= 0) return 0;
  const db = 20 * Math.log10(recording.level);
  return Math.max(0, Math.min(100, ((db + 50) / 50) * 100)); // -50dBFS..0dBFS → 0..100%
});
```

markup：状态行（`<p class="status">`）之前插入控制条（模型就绪块内）：

```svelte
  <div class="controls">
    {#if !recording.isLive}
      <button class="ctl primary" disabled={recording.pending} onclick={startRecording}>● 开始录制</button>
    {:else}
      {#if recording.paused}
        <button class="ctl" disabled={recording.pending} onclick={() => recording.unpause()}>▶ 恢复</button>
      {:else}
        <button class="ctl" disabled={recording.pending} onclick={() => recording.pause()}>⏸ 暂停</button>
      {/if}
      <button class="ctl danger" disabled={recording.pending} onclick={() => recording.stop()}>■ 停止</button>
    {/if}
    <span class="timer" class:pausedTimer={recording.paused}>{formatTs(recording.elapsedMs)}</span>
    <div class="meter" title="麦克风电平"><div class="meter-fill" style="width:{levelPct}%"></div></div>
    {#if recording.paused}<span class="paused-tag">已暂停</span>{/if}
  </div>
```

两处 banner 条件 `recording.isRecording &&` 改为 `recording.isLive &&`（系统声音横幅、声纹横幅）。

style 追加：

```css
  .controls {
    display: flex;
    align-items: center;
    gap: 0.75rem;
    margin: 0 0 0.75rem;
  }
  .ctl {
    border-radius: 8px;
    border: 1px solid #ccc;
    padding: 0.45em 1.1em;
    font-weight: 600;
    cursor: pointer;
    background: #fff;
  }
  .ctl.primary { background: #396cd8; color: #fff; border-color: transparent; }
  .ctl.danger { background: #c0392b; color: #fff; border-color: transparent; }
  .timer {
    font-variant-numeric: tabular-nums;
    font-weight: 600;
    color: #444;
  }
  .timer.pausedTimer { color: #d88a39; }
  .meter {
    width: 120px;
    height: 8px;
    background: #e0e0e3;
    border-radius: 4px;
    overflow: hidden;
  }
  .meter-fill {
    height: 100%;
    background: #2e9e5b;
    transition: width 0.1s linear;
  }
  .paused-tag {
    background: #d88a39;
    color: #fff;
    font-size: 0.75em;
    font-weight: 600;
    border-radius: 6px;
    padding: 0.1em 0.5em;
  }
  @media (prefers-color-scheme: dark) {
    .ctl { background: #0f0f0f98; color: #fff; border-color: #555; }
    .ctl.primary { background: #396cd8; }
    .ctl.danger { background: #c0392b; }
    .timer { color: #ccc; }
    .meter { background: #444; }
  }
```

- [ ] **Step 4: Sidebar.svelte / 详情页联动**

Sidebar：`toggleRecording` 里 `recording.isRecording` → `recording.isLive`；按钮文案/样式行改：

```svelte
  <button
    class="record-btn"
    class:recording={recording.isLive}
    onclick={toggleRecording}
    disabled={recording.pending}
  >
    {recording.isLive ? (recording.paused ? "⏸ 已暂停 · 停止" : "■ 停止") : "● 开始录制"}
  </button>
```

`notes/[id]/+page.svelte`：「继续录制」按钮 `disabled={recording.isRecording}` → `disabled={recording.isLive}`；`doResume` 的兜底文案保持。

- [ ] **Step 5: 检查 + 提交**

```bash
npm run check 2>&1 | tail -3   # 0 errors
npm run build 2>&1 | tail -3
git add src/lib/events.ts src/lib/recording.svelte.ts src/routes/record/+page.svelte src/lib/Sidebar.svelte "src/routes/notes/[id]/+page.svelte"
git commit -m "feat(ui): 录制控制条(暂停/恢复/停止/计时/电平表) + 冷刷新与对账水合 finals

paused 态贯穿 store/录制页/侧栏;计时以后端 elapsed_ms 为基线本地走表;
电平 dBFS 映射;hydrateFromDisk 清掉 P3.5/P4.5 遗留的不回灌问题。

Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>"
```

---

### Task 8: 后端段落编辑——改文本 / 删除 / 改说话人

**Files:**
- Modify: `src-tauri/src/store/notes.rs`（编辑原语 + 单测）
- Modify: `src-tauri/src/lib.rs`（3 个 command + 活动笔记 guard）

**Interfaces:**
- Consumes: 既有 `SegmentRecord`、`read_speakers`、`write_speakers_atomic`、`NoteStore::note_dir`。
- Produces: `NoteStore::edit_segment_text(id, seq, expected_text, new_text) -> Result<()>`；`NoteStore::delete_segment(id, seq, expected_text) -> Result<()>`；`NoteStore::set_segment_speaker(id, seq, expected_text, speaker_id) -> Result<String>`（`speaker_id="new"` → 分配 `S<max+1>` 并返回实际 id）；commands `edit_segment`/`delete_segment`/`set_segment_speaker`（参数 camelCase：noteId/seq/expectedText/newText/speakerId）。乐观冲突错误文案含「请刷新后重试」。

- [ ] **Step 1: 写失败测试（notes.rs tests 追加）**

```rust
    /// 造带说话人的笔记：segs = (text, speaker)；known = 写入 speakers.json 的说话人表
    /// （与段内 speaker 解耦——测试需要「段里有、表里没有」的孤儿 id）。
    fn make_spk_note(dir: &std::path::Path, segs: &[(&str, Option<&str>)], known: &[&str]) -> String {
        let mut w = NoteWriter::create(dir, now()).unwrap();
        for (i, (t, spk)) in segs.iter().enumerate() {
            let s = i as u64 * 1000;
            w.append_final("mic", t, s, s + 900, *spk).unwrap();
        }
        if !known.is_empty() {
            let pairs: Vec<(String, Vec<String>)> =
                known.iter().map(|s| (s.to_string(), vec!["mic".to_string()])).collect();
            w.sync_speakers(&pairs).unwrap();
        }
        w.finalize(now()).unwrap();
        w.note_id().to_string()
    }

    #[test]
    fn edit_segment_text_rewrites_only_target() {
        let tmp = tempfile::tempdir().unwrap();
        let id = make_spk_note(tmp.path(), &[("原文一", None), ("原文二", None)], &[]);
        let store = NoteStore::new(tmp.path().to_path_buf());
        store.edit_segment_text(&id, 1, "原文二", "改后二").unwrap();
        let n = store.load(&id).unwrap();
        assert_eq!(n.segments[0].text, "原文一", "非目标段不动");
        assert_eq!(n.segments[1].text, "改后二");
        assert_eq!(n.segments[1].seq, 1, "seq/时间戳等其余字段保留");
        assert_eq!(n.segments[1].start_ms, 1000);
    }

    #[test]
    fn edit_rejects_stale_expected_and_blank_text() {
        let tmp = tempfile::tempdir().unwrap();
        let id = make_spk_note(tmp.path(), &[("原文", None)], &[]);
        let store = NoteStore::new(tmp.path().to_path_buf());
        let e = store.edit_segment_text(&id, 0, "别人已改过", "x").unwrap_err();
        assert!(e.to_string().contains("请刷新后重试"), "乐观冲突提示: {e}");
        assert!(store.edit_segment_text(&id, 0, "原文", "   ").is_err(), "空文本拒绝");
        assert!(store.edit_segment_text(&id, 99, "原文", "x").is_err(), "seq 不存在");
        assert_eq!(store.load(&id).unwrap().segments[0].text, "原文", "拒绝路径不落盘");
    }

    #[test]
    fn delete_segment_removes_line_and_preserves_corrupt_raw() {
        let tmp = tempfile::tempdir().unwrap();
        let id = make_spk_note(tmp.path(), &[("一", None), ("二", None)], &[]);
        // 人为插入损坏行：编辑重写后必须原样保留（不丢数据）。
        use std::io::Write;
        let mut f = std::fs::OpenOptions::new()
            .append(true)
            .open(tmp.path().join(&id).join("segments.jsonl"))
            .unwrap();
        f.write_all(b"{corrupt-line\n").unwrap();
        drop(f);
        let store = NoteStore::new(tmp.path().to_path_buf());
        store.delete_segment(&id, 0, "一").unwrap();
        let n = store.load(&id).unwrap();
        assert_eq!(n.segments.len(), 1);
        assert_eq!(n.segments[0].text, "二");
        assert_eq!(n.skipped_lines, 1, "损坏行经重写仍在（原样保留）");
    }

    #[test]
    fn set_segment_speaker_existing_new_and_unknown() {
        let tmp = tempfile::tempdir().unwrap();
        // speakers.json 有 S1、S3；另有孤儿段 speaker=S5（表里没有）。
        let id = make_spk_note(tmp.path(), &[("甲", Some("S1")), ("乙", Some("S3")), ("丙", Some("S5"))], &["S1", "S3"]);
        let store = NoteStore::new(tmp.path().to_path_buf());
        // 改为既有说话人
        assert_eq!(store.set_segment_speaker(&id, 0, "甲", "S3").unwrap(), "S3");
        assert_eq!(store.load(&id).unwrap().segments[0].speaker.as_deref(), Some("S3"));
        // 未知说话人拒绝
        assert!(store.set_segment_speaker(&id, 0, "甲", "S99").is_err());
        // 新建：max 取 speakers 表(S1,S3,S5) 与段内(S5) 的并集 → S6
        let got = store.set_segment_speaker(&id, 1, "乙", "new").unwrap();
        assert_eq!(got, "S6");
        let n = store.load(&id).unwrap();
        assert_eq!(n.segments[1].speaker.as_deref(), Some("S6"));
        assert!(n.speakers.contains_key("S6"), "新说话人已入表(空名,无质心)");
        assert_eq!(n.speakers["S6"].name, "");
        assert!(n.speakers["S6"].centroid.is_none());
    }
```

- [ ] **Step 2: 跑红**

```bash
cd src-tauri && cargo test store::notes 2>&1 | tail -8
```
Expected: 编译失败（方法不存在）。

- [ ] **Step 3: 实现（notes.rs impl NoteStore 内追加 + 文件级私有辅助）**

```rust
/// segments.jsonl 的一行：可解析段或损坏原文。编辑重写时损坏行原样保留（不丢数据）。
enum JsonlLine {
    Seg(SegmentRecord),
    Raw(String),
}

fn read_jsonl_lines(path: &Path) -> Vec<JsonlLine> {
    let Ok(f) = fs::File::open(path) else { return Vec::new() };
    std::io::BufReader::new(f)
        .lines()
        .filter_map(|l| l.ok())
        .filter(|l| !l.trim().is_empty())
        .map(|l| match serde_json::from_str::<SegmentRecord>(&l) {
            Ok(r) => JsonlLine::Seg(r),
            Err(_) => JsonlLine::Raw(l),
        })
        .collect()
}

/// 原子重写 segments.jsonl（tmp+rename，与 meta/speakers 同哲学）。
fn write_jsonl_atomic(dir: &Path, lines: &[JsonlLine]) -> anyhow::Result<()> {
    let tmp = dir.join("segments.jsonl.tmp");
    let mut out = String::new();
    for l in lines {
        match l {
            JsonlLine::Seg(r) => out.push_str(&serde_json::to_string(r)?),
            JsonlLine::Raw(s) => out.push_str(s),
        }
        out.push('\n');
    }
    fs::write(&tmp, out)?;
    fs::rename(&tmp, dir.join("segments.jsonl"))?;
    Ok(())
}

/// 按 seq 定位段并做乐观校验（seq 跨续录单调唯一，见 writer.rs resume 测试）。
fn find_seg<'a>(
    lines: &'a mut [JsonlLine],
    seq: u64,
    expected_text: &str,
) -> anyhow::Result<&'a mut SegmentRecord> {
    for l in lines.iter_mut() {
        if let JsonlLine::Seg(r) = l {
            if r.seq == seq {
                if r.text != expected_text {
                    anyhow::bail!("段落内容已变化，请刷新后重试");
                }
                return Ok(r);
            }
        }
    }
    anyhow::bail!("段落不存在（seq={seq}）")
}
```

impl NoteStore 追加：

```rust
    /// 改段落文本。空文本拒绝（如需去段请用 delete_segment）。
    pub fn edit_segment_text(
        &self,
        id: &str,
        seq: u64,
        expected_text: &str,
        new_text: &str,
    ) -> anyhow::Result<()> {
        let new_text = new_text.trim();
        if new_text.is_empty() {
            anyhow::bail!("文本不能为空（如需去掉这段请用删除）");
        }
        let dir = self.note_dir(id)?;
        let mut lines = read_jsonl_lines(&dir.join("segments.jsonl"));
        find_seg(&mut lines, seq, expected_text)?.text = new_text.to_string();
        write_jsonl_atomic(&dir, &lines)
    }

    /// 物理删除段落行。speakers.json 不清孤儿说话人（无害，chips 仍可改名）。
    pub fn delete_segment(&self, id: &str, seq: u64, expected_text: &str) -> anyhow::Result<()> {
        let dir = self.note_dir(id)?;
        let mut lines = read_jsonl_lines(&dir.join("segments.jsonl"));
        find_seg(&mut lines, seq, expected_text)?;
        lines.retain(|l| !matches!(l, JsonlLine::Seg(r) if r.seq == seq));
        write_jsonl_atomic(&dir, &lines)
    }

    /// 改段落说话人归属。speaker_id="new" → 分配 S<max+1>（max 跨 speakers.json 键与
    /// 段内既有 speaker id，防与孤儿 id 撞号）先入表再改段（中间崩溃只留无害孤儿）。
    /// 只改 segment.speaker 字段，不回灌声纹质心（离线编辑不影响聚类）。
    pub fn set_segment_speaker(
        &self,
        id: &str,
        seq: u64,
        expected_text: &str,
        speaker_id: &str,
    ) -> anyhow::Result<String> {
        let dir = self.note_dir(id)?;
        let mut lines = read_jsonl_lines(&dir.join("segments.jsonl"));
        find_seg(&mut lines, seq, expected_text)?;
        let mut speakers = read_speakers(&dir);
        let target = if speaker_id == "new" {
            let num = |s: &str| s.strip_prefix('S').and_then(|n| n.parse::<u64>().ok()).unwrap_or(0);
            let max_known = speakers
                .keys()
                .map(|k| num(k))
                .chain(lines.iter().filter_map(|l| match l {
                    JsonlLine::Seg(r) => r.speaker.as_deref().map(num),
                    _ => None,
                }))
                .max()
                .unwrap_or(0);
            let new_id = format!("S{}", max_known + 1);
            speakers.insert(
                new_id.clone(),
                SpeakerMeta { name: String::new(), sources: Vec::new(), centroid: None, count: 0 },
            );
            write_speakers_atomic(&dir, &speakers)?;
            new_id
        } else {
            if !speakers.contains_key(speaker_id) {
                anyhow::bail!("未知说话人: {speaker_id}");
            }
            speaker_id.to_string()
        };
        find_seg(&mut lines, seq, expected_text)?.speaker = Some(target.clone());
        write_jsonl_atomic(&dir, &lines)?;
        Ok(target)
    }
```

- [ ] **Step 4: lib.rs commands**

```rust
/// 段落编辑共用 guard：活动会话笔记一律拒绝（与 rename_note 同模式）。
fn reject_if_active(state: &State<AppState>, note_id: &str) -> Result<(), String> {
    if state.session.lock().unwrap().as_ref().map(|s| s.note_id == note_id).unwrap_or(false) {
        return Err("录制中的笔记不能编辑".into());
    }
    Ok(())
}

#[tauri::command]
fn edit_segment(
    app: AppHandle,
    state: State<AppState>,
    note_id: String,
    seq: u64,
    expected_text: String,
    new_text: String,
) -> Result<(), String> {
    reject_if_active(&state, &note_id)?;
    let dir = notes_dir(&app).map_err(|e| e.to_string())?;
    store::NoteStore::new(dir)
        .edit_segment_text(&note_id, seq, &expected_text, &new_text)
        .map_err(|e| e.to_string())
}

#[tauri::command]
fn delete_segment(
    app: AppHandle,
    state: State<AppState>,
    note_id: String,
    seq: u64,
    expected_text: String,
) -> Result<(), String> {
    reject_if_active(&state, &note_id)?;
    let dir = notes_dir(&app).map_err(|e| e.to_string())?;
    store::NoteStore::new(dir)
        .delete_segment(&note_id, seq, &expected_text)
        .map_err(|e| e.to_string())
}

#[tauri::command]
fn set_segment_speaker(
    app: AppHandle,
    state: State<AppState>,
    note_id: String,
    seq: u64,
    expected_text: String,
    speaker_id: String,
) -> Result<String, String> {
    reject_if_active(&state, &note_id)?;
    let dir = notes_dir(&app).map_err(|e| e.to_string())?;
    store::NoteStore::new(dir)
        .set_segment_speaker(&note_id, seq, &expected_text, &speaker_id)
        .map_err(|e| e.to_string())
}
```

三个注册进 `generate_handler!`。

- [ ] **Step 5: 跑绿 + 提交**

```bash
cd src-tauri && cargo test 2>&1 | tail -5   # 94 + 4 = 98 passed
git add src-tauri/src/store/notes.rs src-tauri/src/lib.rs
git commit -m "feat(store): 段落编辑原语——改文本/删除/改说话人(seq 主键+乐观校验+原子重写)

损坏行经重写原样保留;new 说话人跨表与段内取 max+1 防撞号;活动笔记 command 层拒绝。

Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>"
```

---

### Task 9: 详情页编辑 UI + 遗留 #1/#2/#6

**Files:**
- Modify: `src/lib/notes.ts`（3 个 wrapper + speakerIdCompare）
- Modify: `src/routes/notes/[id]/+page.svelte`

**Interfaces:**
- Consumes: Task 8 三 commands；`recording.isLive`/`recording.noteId`（Task 7）。
- Produces: `notes.ts` 导出 `editSegment/deleteSegment/setSegmentSpeaker/speakerIdCompare`（Task 10 chips 复用 speakerIdCompare）。

- [ ] **Step 1: notes.ts 追加**

```ts
export const editSegment = (noteId: string, seq: number, expectedText: string, newText: string) =>
  invoke<void>("edit_segment", { noteId, seq, expectedText, newText });
export const deleteSegment = (noteId: string, seq: number, expectedText: string) =>
  invoke<void>("delete_segment", { noteId, seq, expectedText });
/** 返回实际生效的 speaker id（speakerId="new" 时为后端分配的新 id） */
export const setSegmentSpeaker = (noteId: string, seq: number, expectedText: string, speakerId: string) =>
  invoke<string>("set_segment_speaker", { noteId, seq, expectedText, speakerId });

/** 说话人 id 排序：S2 < S10（数值序）；非 S<n> 形态沉底按字典序。 */
export function speakerIdCompare(a: string, b: string): number {
  const num = (id: string) => {
    const n = parseInt(id.replace(/^S/, ""), 10);
    return Number.isFinite(n) && n > 0 ? n : Number.MAX_SAFE_INTEGER;
  };
  return num(a) - num(b) || a.localeCompare(b);
}
```

- [ ] **Step 2: 详情页改造**

script 区改动：

```ts
import {
  getNote, renameNote, exportNote, formatTs, formatDate, formatDuration,
  speakerLabel, speakerColor, speakerIdCompare,
  editSegment, deleteSegment, setSegmentSpeaker,
  type Note, type SegmentRecord,
} from "$lib/notes";

// 段落编辑状态
let editingSeq = $state<number | null>(null);
let editingText = $state("");
let confirmSeq = $state<number | null>(null);
let speakerMenuSeq = $state<number | null>(null);

/** 展示序：过滤空白段(遗留#1) + 按 start_ms 稳定排序消除 ECHO hold 交错(遗留#2)。 */
const displaySegments = $derived(
  note
    ? [...note.segments]
        .filter((s) => s.text.trim())
        .sort((a, b) => a.start_ms - b.start_ms || a.seq - b.seq)
    : [],
);
/** 本笔记正在录制（含暂停）时禁用一切编辑入口（后端另有 guard 兜底）。 */
const canEdit = $derived(!(recording.isLive && recording.noteId === id));
const speakerIds = $derived(note ? Object.keys(note.speakers).sort(speakerIdCompare) : []);

function beginEditSeg(s: SegmentRecord) {
  editingSeq = s.seq;
  editingText = s.text;
  speakerMenuSeq = null;
  confirmSeq = null;
}

async function commitEditSeg(s: SegmentRecord) {
  if (editingSeq !== s.seq) return;
  const text = editingText.trim();
  editingSeq = null;
  if (!text || text === s.text) return;
  try {
    await editSegment(id, s.seq, s.text, text);
    await refresh();
  } catch (e) {
    error = `编辑失败: ${e}`;
    await refresh(); // 乐观冲突：重载最新内容
  }
}

async function doDeleteSeg(s: SegmentRecord) {
  confirmSeq = null;
  try {
    await deleteSegment(id, s.seq, s.text);
    await refresh();
  } catch (e) {
    error = `删除失败: ${e}`;
    await refresh();
  }
}

async function doSetSpeaker(s: SegmentRecord, speakerId: string) {
  speakerMenuSeq = null;
  try {
    await setSegmentSpeaker(id, s.seq, s.text, speakerId);
    await refresh();
  } catch (e) {
    error = `修改说话人失败: ${e}`;
    await refresh();
  }
}
```

`$effect` 拆两个（遗留#6：编辑中不吹掉编辑态；编辑结束后 effect 重跑自然补上被跳过的刷新）：

```ts
  // id 切换：无条件复位一切编辑态。
  $effect(() => {
    void id;
    editing = false;
    editingSeq = null;
    speakerMenuSeq = null;
    confirmSeq = null;
  });
  // 刷新：任何编辑进行中都跳过（编辑态是 effect 依赖，编辑结束会自动重跑并刷新）。
  $effect(() => {
    void id;
    void recording.notesVersion;
    if (editing || editingSeq !== null || speakerMenuSeq !== null) return;
    exportMsg = "";
    refresh();
  });
```

transcript 段落循环替换（`{#each note.segments as seg (seg.seq)}` → displaySegments；加编辑操作）：

```svelte
    <div class="transcript">
      {#each displaySegments as seg (seg.seq)}
        <div class="seg">
          {#if canEdit && speakerMenuSeq === seg.seq}
            <span class="badge-menu">
              {#each speakerIds as sid (sid)}
                <button class="menu-item" onclick={() => doSetSpeaker(seg, sid)}>
                  {speakerLabel(sid, seg.source, note.speakers)}
                </button>
              {/each}
              <button class="menu-item new" onclick={() => doSetSpeaker(seg, "new")}>＋ 新说话人</button>
              <button class="menu-item" onclick={() => (speakerMenuSeq = null)}>取消</button>
            </span>
          {:else}
            <button
              class="badge as-btn"
              style="background: {speakerColor(seg.speaker, seg.source)}"
              disabled={!canEdit}
              title={canEdit ? "点击改说话人" : ""}
              onclick={() => (speakerMenuSeq = seg.seq)}
            >
              {speakerLabel(seg.speaker, seg.source, note.speakers)}
            </button>
          {/if}
          <span class="ts">{formatTs(seg.start_ms)}</span>
          {#if editingSeq === seg.seq}
            <!-- svelte-ignore a11y_autofocus -->
            <textarea
              class="seg-edit"
              autofocus
              bind:value={editingText}
              onkeydown={(e) => {
                if (e.key === "Enter" && !e.shiftKey) {
                  e.preventDefault();
                  commitEditSeg(seg);
                }
                if (e.key === "Escape") editingSeq = null;
              }}
              onblur={() => commitEditSeg(seg)}
            ></textarea>
          {:else}
            <span class="seg-text">{seg.text}</span>
            {#if canEdit}
              <span class="seg-actions">
                <button class="link" onclick={() => beginEditSeg(seg)}>编辑</button>
                {#if confirmSeq === seg.seq}
                  <button class="link danger" onclick={() => doDeleteSeg(seg)}>确认删除</button>
                  <button class="link" onclick={() => (confirmSeq = null)}>取消</button>
                {:else}
                  <button class="link" onclick={() => (confirmSeq = seg.seq)}>删除</button>
                {/if}
              </span>
            {/if}
          {/if}
        </div>
      {/each}
      {#if displaySegments.length === 0}
        <p class="hint">（这场会议没有转写内容）</p>
      {/if}
    </div>
```

style 追加：

```css
  .seg {
    margin: 0 0 0.35rem;
    line-height: 1.6;
  }
  .badge.as-btn {
    border: none;
    cursor: pointer;
    font-family: inherit;
  }
  .badge.as-btn:disabled {
    cursor: default;
  }
  .seg-actions {
    visibility: hidden;
    margin-left: 0.4em;
  }
  .seg:hover .seg-actions {
    visibility: visible;
  }
  .link {
    background: none;
    border: none;
    color: #396cd8;
    cursor: pointer;
    padding: 0.1em 0.25em;
    font-size: 0.8em;
    box-shadow: none;
  }
  .link.danger {
    color: #c0392b;
    font-weight: 600;
  }
  .seg-edit {
    width: 100%;
    box-sizing: border-box;
    font: inherit;
    line-height: 1.5;
    border-radius: 6px;
    border: 1px solid #396cd8;
    padding: 0.3em 0.5em;
    margin-top: 0.2em;
    resize: vertical;
    min-height: 2.4em;
  }
  .badge-menu {
    display: inline-flex;
    flex-wrap: wrap;
    gap: 0.25em;
    background: #fff;
    border: 1px solid #ccc;
    border-radius: 8px;
    padding: 0.2em 0.4em;
    margin-right: 0.4em;
  }
  .menu-item {
    background: none;
    border: none;
    color: #396cd8;
    cursor: pointer;
    font-size: 0.8em;
    padding: 0.15em 0.4em;
  }
  .menu-item.new {
    font-weight: 600;
  }
  @media (prefers-color-scheme: dark) {
    .seg-edit {
      background: #2a2a2a;
      color: #f0f0f0;
    }
    .badge-menu {
      background: #2a2a2a;
      border-color: #555;
    }
  }
```

- [ ] **Step 3: 检查 + 提交**

```bash
npm run check 2>&1 | tail -3   # 0 errors（新增 a11y warning 数不得超过既有 2 条）
npm run build 2>&1 | tail -3
git add src/lib/notes.ts "src/routes/notes/[id]/+page.svelte"
git commit -m "feat(ui): 详情页段落编辑(内联改文本/删除/改说话人菜单)

并修遗留:空白段过滤、按 start_ms 稳定排序、编辑中 notesVersion 刷新不吹掉编辑态。

Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>"
```

---

### Task 10: 遗留小修 #3/#4/#5 + 全量验证 + 冒烟清单

**Files:**
- Modify: `src/lib/notes.ts`（speakerColor 兜底）
- Modify: `src/lib/SpeakerChips.svelte`（数值排序）
- Modify: `src/routes/notes/[id]/+page.svelte`（h1 键盘入口）

**Interfaces:**
- Consumes: Task 9 的 `speakerIdCompare`。

- [ ] **Step 1: speakerColor 非 S\<n\> 兜底（遗留#4）**

当前 `parseInt` 失败给 0 → `PALETTE[-1]` 为 undefined（CSS 失效）。替换 `speakerColor`：

```ts
/** 稳定调色板:S1..Sn 循环取色;非 S<n> 形态 id 用字符串散列兜底(亮/暗色下均可读) */
export function speakerColor(speaker: string | null, source: Source): string {
  if (!speaker) return source === "mic" ? "#396cd8" : "#2e9e5b";
  const n = parseInt(speaker.replace(/^S/, ""), 10);
  if (Number.isFinite(n) && n > 0) return PALETTE[(n - 1) % PALETTE.length];
  let h = 0;
  for (const c of speaker) h = (h * 31 + c.charCodeAt(0)) >>> 0;
  return PALETTE[h % PALETTE.length];
}
```

- [ ] **Step 2: chips 数值排序（遗留#3）**

`SpeakerChips.svelte`：`import { speakerColor, speakerLabel, renameSpeaker } from "$lib/notes";` 加 `speakerIdCompare`；排序行改：

```ts
  const ids = $derived(Object.keys(speakers).sort(speakerIdCompare));
```

- [ ] **Step 3: h1 改名键盘入口（遗留#5）**

`notes/[id]/+page.svelte` 标题行改：

```svelte
      <h1
        class="title"
        title="点击改名"
        role="button"
        tabindex="0"
        onclick={beginRename}
        onkeydown={(e) => {
          if (e.key === "Enter") beginRename();
        }}
      >
        {note.meta.title}
      </h1>
```

（这同时消掉既有的 a11y click 告警之一；`npm run check` 告警数应下降或持平。）

- [ ] **Step 4: 全量验证**

```bash
cd src-tauri && cargo test 2>&1 | tail -5                 # 98 passed
cargo test -- --ignored 2>&1 | tail -5                    # 门控测试全过(本机有模型)
cd .. && npm run check 2>&1 | tail -3 && npm run build 2>&1 | tail -3
```

- [ ] **Step 5: Commit**

```bash
git add src/lib/notes.ts src/lib/SpeakerChips.svelte "src/routes/notes/[id]/+page.svelte"
git commit -m "fix(ui): chips 数值排序(S2<S10)/speakerColor 非 S<n> 散列兜底/h1 改名键盘入口

Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>"
```

- [ ] **Step 6: 人工冒烟清单（合并前必过；记录到 progress.md）**

1. **模型下载全流程**：`VN_MODELS=$(mktemp -d) npm run tauri dev` 模拟裸机 → 录制页出大卡片、笔记浏览不受影响 → 点下载（可开镜像）→ 三工件进度推进 → 中途「暂停下载」→ 「继续下载」从断点续（received 不清零）→ 全部完成后卡片消失、**不重启**直接开录成功。
2. **缺声纹小提示条**：临时目录里只放 vad+sense-voice（从 src-tauri/models 拷贝/软链）→ 录制页出小提示条不挡录制 → 补下后提示条消失。
3. **暂停/恢复**：开录说话 → 暂停：在途句立即定稿上屏、计时停走、电平条仍随说话摆动、说话不再产段 → 恢复：计时续走、转写恢复 → 暂停态直接停止 → 详情页时间轴无重叠（暂停期不占时长）。
4. **计时**：录制中刷新页面（dev 下 R 或路由离开再回）计时不清零；续录既有笔记计时从历史时长起算。
5. **段落编辑**：详情页改文本（Enter 存/Esc 取消/空文本拒绝）、删除（确认）、改说话人（既有+新说话人,chips 出现新 chip 可改名）；导出 md/txt 反映编辑；录制中打开该笔记详情无编辑入口。
6. **遗留项抽查**：10+ 说话人会议 chips 序 S1..S10;冷刷新录制中笔记 finals 回灌;侧栏改名时详情页编辑不丢。

冒烟通过后按 finishing-a-development-branch 流程：全分支终审 → 推 origin → PR → squash 合入。

---

## Self-Review 记录

- **Spec 覆盖**：§1.1→T1；§1.2→T1；§1.3→T2/T3；§1.4→T3；§1.5→T4；§2.1→T5/T6/T7；§2.2→T6/T7；§2.3→T5/T6/T7；§2.4→T7；§3.1→T8;§3.2→T9;§4 七项→#7=T7、#1/#2/#6=T9、#3/#4/#5=T10;§5 测试→各任务内嵌;§6 非目标未越界。
- **类型一致性**：`FinalFile/Artifact/ArtifactKind`（T1 定义,T2/T3 消费）;`start_session(..., on_mic_level)` 末参（T5 定义,T6 传实参）;`StatusEvent.elapsed_ms`（T6 后端,T7 前端）;`speakerIdCompare`（T9 定义,T10 消费）——已核对同名同形。
- **已知风险**：sherpa VAD flush 中途调用的时间轴延续性（T5 Step 5 门控测试钉死,失败有 B 计划）;ureq 对 GitHub 30x 重定向默认跟随（ureq 2 默认 redirects=5,够用）;ghproxy 镜像可用性不保证（前缀可改,默认关）。
