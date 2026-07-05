# 音频压缩 + 系统设置页 + ASR 选型 实施计划

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 笔记音频 WAV→AAC m4a(8 倍压缩,含历史回溯与续录解码);`/settings` 设置页(数据/模型目录自动迁移、模型下载/删除/镜像集中、ASR 选型 SenseVoice/Whisper)。

**Architecture:** 录制仍写 WAV(崩溃安全/对齐不变式原封);停止后经全局串行转码队列调 `/usr/bin/afconvert` 转 m4a,校验后删 WAV,启动扫描回溯历史;续录先解码回 WAV 再走既有逻辑。设置扩展 data_dir/models_dir/asr_model 三字段,路径经 `data_root()`/`models::root()` 统一解析,迁移=复制→校验→写设置→删旧。Whisper 走既有 `Recognizer` trait(P1 已实装),下游语言过滤/段内分离对空标签/空时间戳已有降级路径,零改动。

**Tech Stack:** Rust(std::process 调 afconvert/afinfo,Condvar 队列)+ tauri-plugin-dialog + SvelteKit 新路由。

**Spec:** `docs/superpowers/specs/2026-07-05-voice-notes-audio-compression-settings-design.md`

## Global Constraints

- 特性分支 `audio-compression-settings`(从 master 建),每任务一提交,最终 push→PR→squash。
- 注释中文讲"为什么";cargo test 全过、npm run check 0 错 0 警、双端 build 无新警告。
- 转码/迁移/删除模型全是增值层:任何失败只降级(保留 WAV/保持旧目录/报错横幅),绝不阻塞录制与转写落盘。
- 子进程固定绝对路径 `/usr/bin/afconvert`、`/usr/bin/afinfo`;编码参数 `-f m4af -d aac -b 32000`,解码参数 `-f WAVE -d LEI16@16000 -c 1`(已在本机实测,3s WAV roundtrip 样本数精确保持)。
- 时长校验允差 `DURATION_TOLERANCE_MS = 100`。
- serde 兼容:settings.json 新三字段、audio.json 新两字段均 default(+Option 者 skip_serializing_if),旧文件双向兼容。
- UI 严格按 DESIGN.md(温暖极简 token、hairline、悬停显影、禁 emoji/Unicode 符号图标);设置页复用现有 token,不新增 token。
- TDD:每任务新逻辑先测后码。涉及 afconvert 的测试标 `#[cfg(target_os = "macos")]`(dev/CI 均 mac)。

---

### Task 1: settings 扩展三字段

**Files:**
- Modify: `src-tauri/src/settings.rs`

**Interfaces:**
- Produces:
  - `Settings` 增 `pub data_dir: Option<String>`、`pub models_dir: Option<String>`(均 `#[serde(default, skip_serializing_if = "Option::is_none")]`)、`pub asr_model: String`(`#[serde(default = "default_asr")]`,默认 `"sense_voice"`)。
  - `pub const ASR_SENSE_VOICE: &str = "sense_voice";`、`pub const ASR_WHISPER: &str = "whisper";`
  - `pub fn resolve_data_root(app_data: &Path, s: &Settings) -> PathBuf`:`data_dir` 非空取之,否则 `app_data`(纯函数,供 lib.rs 与测试)。

- [ ] **Step 1: 写失败测试**(settings.rs tests 追加)

```rust
    #[test]
    fn new_fields_default_and_roundtrip() {
        let tmp = tempfile::tempdir().unwrap();
        // 旧文件(仅镜像字段)→ 新字段全默认
        std::fs::write(tmp.path().join("settings.json"), r#"{"mirror_enabled":true,"mirror_prefix":"x"}"#).unwrap();
        let s = load(tmp.path());
        assert_eq!(s.data_dir, None);
        assert_eq!(s.models_dir, None);
        assert_eq!(s.asr_model, ASR_SENSE_VOICE);
        // 新字段 roundtrip
        let s = Settings {
            data_dir: Some("/tmp/d".into()),
            models_dir: Some("/tmp/m".into()),
            asr_model: ASR_WHISPER.into(),
            ..Default::default()
        };
        save(tmp.path(), &s).unwrap();
        let got = load(tmp.path());
        assert_eq!(got.data_dir.as_deref(), Some("/tmp/d"));
        assert_eq!(got.models_dir.as_deref(), Some("/tmp/m"));
        assert_eq!(got.asr_model, "whisper");
    }

    #[test]
    fn resolve_data_root_prefers_configured() {
        let base = Path::new("/base");
        assert_eq!(resolve_data_root(base, &Settings::default()), PathBuf::from("/base"));
        let s = Settings { data_dir: Some("/custom".into()), ..Default::default() };
        assert_eq!(resolve_data_root(base, &s), PathBuf::from("/custom"));
    }
```

- [ ] **Step 2: 跑测确认失败**:`cargo test -p voice-notes settings` → 编译错误(字段不存在)。
- [ ] **Step 3: 实现**:加字段(`Default` impl 同步补)、常量、`default_asr()`、`resolve_data_root`(`use std::path::PathBuf` 补齐)。
- [ ] **Step 4: 跑测通过**:`cargo test -p voice-notes settings`。
- [ ] **Step 5: Commit**:`feat(settings): data_dir/models_dir/asr_model 三字段与 data_root 解析`

---

### Task 2: models 清单加 whisper 工件 + root 可配置 + 按选型判就绪

**Files:**
- Modify: `src-tauri/src/models/mod.rs`

**Interfaces:**
- Produces:
  - `Artifact` 增 `pub prune: &'static [&'static str]`(装好后删除的 root 相对路径,fp32/测试音频不留盘;既有三工件 `&[]`)。
  - `Artifact` 删除 `required_for_recording` 字段(语义改由选型决定,见 `required_now`);`ArtifactState.required_for_recording` 保留(动态计算,前端契约不变)。
  - `ARTIFACTS` 增第四项 whisper(常量见 Step 3,URL/字节数/sha256 已实测钉死)。
  - `pub fn required_now(id: &str, asr_model: &str) -> bool`:vad 恒 true;`"asr"` ⇔ 选型非 whisper;`"whisper"` ⇔ 选型 whisper;其余 false。
  - `pub fn set_models_override(dir: Option<PathBuf>)`(RwLock 静态);`root()` 解析顺序变为 **VN_MODELS env → override → debug dev 目录 → APP_MODELS_ROOT**。
  - `pub fn recording_ready(asr_model: &str) -> bool`、`pub fn status(asr_model: &str) -> ModelsStatus`(签名加参;`recording_ready` 字段 = 所有 `required_now` 工件 present)。

- [ ] **Step 1: 写失败测试**

```rust
    #[test]
    fn manifest_covers_four_artifacts_with_whisper() {
        let ids: Vec<&str> = ARTIFACTS.iter().map(|a| a.id).collect();
        assert_eq!(ids, vec!["vad", "speaker", "asr", "whisper"]);
        let w = ARTIFACTS.iter().find(|a| a.id == "whisper").unwrap();
        assert!(matches!(w.kind, ArtifactKind::TarBz2 { dest_dir: "sherpa-onnx-whisper-base" }));
        assert_eq!(w.files.len(), 3);
        assert!(!w.prune.is_empty(), "fp32 与测试音频装好即删");
        for a in ARTIFACTS {
            for f in a.files { assert_eq!(f.sha256.len(), 64); }
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
```

注意:`root_prefers_env_var` 旧测与本测都动 `VN_MODELS` 进程级环境,若并发互踩则给两测都加 `serial` 语义——本仓惯例是让旧测合并进新测(删旧测,新测已覆盖 env 优先),避免引 serial_test 依赖。

- [ ] **Step 2: 跑测确认失败**。
- [ ] **Step 3: 实现**。whisper 工件常量(实测值,勿改):

```rust
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
```

override 静态:`static MODELS_OVERRIDE: RwLock<Option<PathBuf>> = RwLock::new(None);`(std RwLock,const new)。`status`/`recording_ready` 全部改为按 `required_now`;既有三工件补 `prune: &[]`,`manifest_covers_three_runtime_artifacts` 旧测删除(被新测覆盖)。同文件所有 `required_for_recording` 字段引用同步清理。

- [ ] **Step 4: 跑测通过**(注意 lib.rs 调用点会编译失败——本任务临时把 `lib.rs` 两处 `models::recording_ready()` 改为 `models::recording_ready(settings::ASR_SENSE_VOICE)`、`models_status()` 命令改 `models::status(settings::ASR_SENSE_VOICE)`,Task 8 再接真实设置;`download_models` 中 `ARTIFACTS` 循环暂不动——whisper 会被它下载,Task 8 改按选型过滤)。
- [ ] **Step 5: Commit**:`feat(models): whisper 工件入清单,root 支持设置覆盖,就绪判定按 ASR 选型`

---

### Task 3: 下载器装好即删 prune 项

**Files:**
- Modify: `src-tauri/src/models/download.rs`

**Interfaces:**
- Consumes: `Artifact.prune`(Task 2)。
- Produces: `extract_and_install` 增参 `prune: &[&str]`,安装 rename 成功后对每项 `root.join(p)` 先试 `remove_dir_all` 再试 `remove_file`(失败仅 eprintln——删不掉只是多占盘,不算安装失败)。`finalize_artifact` 透传 `a.prune`。

- [ ] **Step 1: 写失败测试**(download.rs tests)

```rust
    #[test]
    fn extract_and_install_prunes_extras_after_install() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("models");
        std::fs::create_dir_all(&root).unwrap();
        let tarball = make_tarball(tmp.path(), "sv-dir", &[
            ("model.int8.onnx", b"MODEL".as_slice()),
            ("model.onnx", b"BIGFP32".as_slice()),
        ]);
        let files = [ff("sv-dir/model.int8.onnx", b"MODEL")];
        extract_and_install(&tarball, &root, "sv-dir", &files, &["sv-dir/model.onnx"], &AtomicBool::new(false)).unwrap();
        assert!(root.join("sv-dir/model.int8.onnx").exists());
        assert!(!root.join("sv-dir/model.onnx").exists(), "prune 项装好即删");
    }
```

既有 extract 系测试全部补 `&[]` 参数。

- [ ] **Step 2: 跑测确认失败** → **Step 3: 实现** → **Step 4: `cargo test -p voice-notes download` 通过**。
- [ ] **Step 5: Commit**:`feat(models): 安装后清理 prune 项(whisper fp32 不留盘)`

---

### Task 4: audio.json track 扩展 codec/duration_ms,轨道枚举 m4a 优先

**Files:**
- Modify: `src-tauri/src/store/audio.rs`

**Interfaces:**
- Produces:
  - `TrackMeta` 增 `pub codec: Option<String>`、`pub duration_ms: Option<u64>`(default + skip_serializing_if)。
  - `pub fn set_track_compressed(note_dir: &Path, source: &str, duration_ms: u64) -> anyhow::Result<()>`:META_LOCK 内 load→改(codec="aac"+duration)→save。
  - `pub fn clear_track_compressed(note_dir: &Path, source: &str) -> anyhow::Result<()>`:META_LOCK 内清两字段(offset 不动)。
  - `list_tracks`:某源 `<source>.m4a` 存在 → path 指 m4a、`duration_ms` 取 meta 记录值(None 视为损坏,跳过该轨);否则沿既有 WAV 逻辑。
  - `known_sources` 扫描并集补上磁盘 `*.m4a` 对应源(现只并 audio.json∪内建两源,已够——m4a 必经转码产生,必有 audio.json 项;不改)。

- [ ] **Step 1: 写失败测试**

```rust
    #[test]
    fn list_tracks_prefers_m4a_with_recorded_duration() {
        let tmp = tempfile::tempdir().unwrap();
        let mut w = AudioTrackWriter::new(tmp.path(), "mic", 0);
        w.append(&vec![0.1f32; 1600]); // 100ms WAV
        drop(w);
        // 模拟转码完成:m4a 文件(内容不重要,枚举只看存在性)+ meta 标记
        std::fs::write(tmp.path().join("mic.m4a"), b"fake m4a").unwrap();
        set_track_compressed(tmp.path(), "mic", 100).unwrap();
        std::fs::remove_file(tmp.path().join("mic.wav")).unwrap();

        let tracks = list_tracks(tmp.path());
        assert_eq!(tracks.len(), 1);
        assert!(tracks[0].path.ends_with("mic.m4a"));
        assert_eq!(tracks[0].duration_ms, 100, "m4a 时长来自 audio.json 而非字节换算");
        // roundtrip 兼容:文件里真写进了字段
        let meta = load_audio_meta(tmp.path());
        assert_eq!(meta.tracks["mic"].codec.as_deref(), Some("aac"));

        // 清除后回落 WAV 逻辑
        std::fs::remove_file(tmp.path().join("mic.m4a")).unwrap();
        clear_track_compressed(tmp.path(), "mic").unwrap();
        let mut w = AudioTrackWriter::new(tmp.path(), "mic", 0);
        w.append(&vec![0.1f32; 1600]);
        drop(w);
        let tracks = list_tracks(tmp.path());
        assert!(tracks[0].path.ends_with("mic.wav"));
    }

    #[test]
    fn m4a_without_duration_is_skipped_and_old_meta_parses() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("mic.m4a"), b"fake").unwrap();
        // 只有 offset 的旧形状 audio.json(无 codec/duration)→ 可解析;m4a 无 duration 记录 → 跳过
        std::fs::write(tmp.path().join("audio.json"), r#"{"schema_version":1,"tracks":{"mic":{"offset_ms":0}}}"#).unwrap();
        assert!(list_tracks(tmp.path()).is_empty(), "无 duration 记录的 m4a 不上报");
    }
```

- [ ] **Step 2: 跑测确认失败** → **Step 3: 实现**(list_tracks 改造:每源先查 m4a 再查 wav;`repair_stale_tracks` 只对存在的 `.wav` 做,m4a 跳过——现逻辑本就只 open `<source>.wav`,确认 `Ok(md) else continue` 已天然跳过即可,补注释)。
- [ ] **Step 4: `cargo test -p voice-notes store::audio` 通过** → **Step 5: Commit**:`feat(store): audio.json track 记录压缩态,轨道枚举 m4a 优先`

---

### Task 5: transcode 模块——afconvert 封装与单笔记转码/解码

**Files:**
- Create: `src-tauri/src/store/transcode.rs`
- Modify: `src-tauri/src/store/mod.rs`(`pub mod transcode;`)

**Interfaces:**
- Consumes: `audio::{repair_wav_header, set_track_compressed, clear_track_compressed, load_audio_meta, AUDIO_SAMPLE_RATE, HEADER_LEN(改 pub(crate))}`。
- Produces:
  - `pub const DURATION_TOLERANCE_MS: u64 = 100;`
  - `fn afconvert_encode(wav: &Path, m4a_tmp: &Path) -> anyhow::Result<()>`(Command `/usr/bin/afconvert -f m4af -d aac -b 32000 <wav> <tmp>`,非零 exit 带 stderr 报错)
  - `fn afconvert_decode(m4a: &Path, wav_tmp: &Path) -> anyhow::Result<()>`(`-f WAVE -d LEI16@16000 -c 1`)
  - `pub fn probe_duration_ms(path: &Path) -> anyhow::Result<u64>`(`/usr/bin/afinfo` 输出解析 `estimated duration: <f64> sec`,乘 1000 四舍五入)
  - `pub fn transcode_note_dir(note_dir: &Path)`:对目录中每个 `<source>.wav`(文件名去 `.wav` 即 source):先清残留 `*.m4a.tmp`;若 `<source>.m4a` 已存在(上次删 wav 前崩溃)→ 直接删 wav 完成收敛;否则 repair 头 → 空轨(≤44 字节)跳过 → encode 到 `<source>.m4a.tmp` → probe 时长与 `bytes_to_ms(wav_len-44)` 差 ≤ 允差 → `set_track_compressed` → rename tmp→m4a → 删 wav。任何失败:删 tmp、保留 wav、eprintln、continue 下一轨。
  - `pub fn decode_note_to_wav(note_dir: &Path)`:对每个 `<source>.m4a`:decode 到 `<source>.wav.tmp` → probe(对 tmp WAV)与 meta duration 差 ≤ 允差 → rename → 删 m4a → `clear_track_compressed`。失败:删 tmp,m4a rename 为 `<source>.m4a.bad`(退出轨道枚举,本场该源从 base_ms 重新建档,增值层降级)、`clear_track_compressed`、eprintln。
  - `bytes_to_ms` 从 audio.rs 改 `pub(crate)` 复用。

- [ ] **Step 1: 写失败测试**(全部 `#[cfg(target_os = "macos")]`,调真 afconvert)

```rust
    use crate::store::audio::AudioTrackWriter;

    fn make_note_with_wav(ms: u64) -> tempfile::TempDir {
        let tmp = tempfile::tempdir().unwrap();
        let mut w = AudioTrackWriter::new(tmp.path(), "mic", 0);
        w.append(&vec![0.1f32; (16 * ms) as usize]); // 16 样本/ms
        drop(w);
        tmp
    }

    #[test]
    fn transcode_replaces_wav_with_verified_m4a() {
        let tmp = make_note_with_wav(3000);
        transcode_note_dir(tmp.path());
        assert!(!tmp.path().join("mic.wav").exists(), "成功后删 WAV");
        assert!(tmp.path().join("mic.m4a").exists());
        let meta = crate::store::audio::load_audio_meta(tmp.path());
        let d = meta.tracks["mic"].duration_ms.unwrap();
        assert!((d as i64 - 3000).unsigned_abs() <= DURATION_TOLERANCE_MS, "时长记录 {d} ≈ 3000");
        // 幂等:再跑一遍无事发生
        transcode_note_dir(tmp.path());
        assert!(tmp.path().join("mic.m4a").exists());
    }

    #[test]
    fn transcode_converges_when_both_files_exist_and_cleans_tmp() {
        let tmp = make_note_with_wav(500);
        std::fs::write(tmp.path().join("mic.m4a.tmp"), b"junk").unwrap(); // 崩溃残留
        transcode_note_dir(tmp.path());
        assert!(!tmp.path().join("mic.m4a.tmp").exists(), "tmp 残留清掉");
        // 模拟"删 wav 前崩溃":重造 wav,与 m4a 并存
        let mut w = AudioTrackWriter::new(tmp.path(), "mic", 0);
        w.append(&vec![0.1f32; 160]);
        drop(w);
        transcode_note_dir(tmp.path());
        assert!(!tmp.path().join("mic.wav").exists(), "并存收敛为只剩 m4a");
        assert!(tmp.path().join("mic.m4a").exists());
    }

    #[test]
    fn decode_restores_wav_for_resume() {
        let tmp = make_note_with_wav(2000);
        transcode_note_dir(tmp.path());
        decode_note_to_wav(tmp.path());
        assert!(tmp.path().join("mic.wav").exists());
        assert!(!tmp.path().join("mic.m4a").exists());
        let meta = crate::store::audio::load_audio_meta(tmp.path());
        assert!(meta.tracks["mic"].codec.is_none(), "压缩标记清除");
        // 样本数与 2000ms 允差内(afconvert 实测 roundtrip 样本精确,此处放允差防编解码器边界)
        let len = std::fs::metadata(tmp.path().join("mic.wav")).unwrap().len() - 44;
        let ms = len / 2 * 1000 / 16000;
        assert!((ms as i64 - 2000).unsigned_abs() <= DURATION_TOLERANCE_MS);
        // 解码后可直接续录:既有对齐逻辑接手
        let mut w = AudioTrackWriter::new(tmp.path(), "mic", 2000);
        w.append(&vec![0.5f32; 160]);
        drop(w);
    }

    #[test]
    fn corrupt_m4a_degrades_to_bad_rename() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("mic.m4a"), b"not audio").unwrap();
        crate::store::audio::set_track_compressed(tmp.path(), "mic", 1000).unwrap();
        decode_note_to_wav(tmp.path());
        assert!(tmp.path().join("mic.m4a.bad").exists(), "坏 m4a 移出枚举,字节保留");
        assert!(!tmp.path().join("mic.wav").exists());
    }
```

- [ ] **Step 2: 跑测确认失败** → **Step 3: 实现**(Command 用 `.output()` 同步等待;probe 解析找 `"estimated duration:"` 行) → **Step 4: `cargo test -p voice-notes transcode` 通过**。
- [ ] **Step 5: Commit**:`feat(store): afconvert 转码/解码单笔记实现(校验-替换-幂等收敛)`

---

### Task 6: 全局串行转码队列

**Files:**
- Modify: `src-tauri/src/store/transcode.rs`

**Interfaces:**
- Produces:
  - `pub struct TranscodeQueue`(`Mutex<QState> + Condvar`;`QState { queue: VecDeque<PathBuf>, current: Option<PathBuf>, paused: bool }`)
  - `pub fn new() -> Arc<Self>`
  - `pub fn enqueue(&self, note_dir: PathBuf)`(已在队/正在转则去重跳过,notify)
  - `pub fn cancel_and_wait(&self, note_dir: &Path)`(摘队列项;while current==该目录 → cv wait)
  - `pub fn pause_and_wait(&self)` / `pub fn unpause(&self)`(迁移用:置 paused 并等 current 排空)
  - `pub fn spawn_worker(self: &Arc<Self>, running: Arc<Mutex<bool>>, process: fn(&Path))`:常驻线程;录制中(`*running`)只 sleep(2s) 让路不出队;否则 cv 带 2s 超时等待取队头,置 current,**放锁后**调 `process`,完毕清 current 并 notify_all。`process` 参数化 = 生产传 `transcode_note_dir`,测试传桩函数。

- [ ] **Step 1: 写失败测试**

```rust
    use std::sync::atomic::{AtomicUsize, Ordering};

    static PROCESSED: AtomicUsize = AtomicUsize::new(0);
    fn slow_stub(_: &Path) {
        PROCESSED.fetch_add(1, Ordering::SeqCst);
        std::thread::sleep(std::time::Duration::from_millis(200));
    }

    #[test]
    fn queue_dedups_pauses_and_cancel_waits_inflight() {
        PROCESSED.store(0, Ordering::SeqCst);
        let q = TranscodeQueue::new();
        let running = Arc::new(Mutex::new(false));
        q.spawn_worker(running.clone(), slow_stub);

        let a = PathBuf::from("/tmp/note-a");
        q.enqueue(a.clone());
        q.enqueue(a.clone()); // 去重
        // 等 worker 拾起 a
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
        while PROCESSED.load(Ordering::SeqCst) == 0 {
            assert!(std::time::Instant::now() < deadline, "worker 未拾起任务");
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        // cancel_and_wait 必须阻塞到 in-flight 完成
        let t0 = std::time::Instant::now();
        q.cancel_and_wait(&a);
        // 去重生效:只处理了一次
        std::thread::sleep(std::time::Duration::from_millis(300));
        assert_eq!(PROCESSED.load(Ordering::SeqCst), 1, "重复入队被去重");
        assert!(t0.elapsed() >= std::time::Duration::from_millis(50), "等待了 in-flight");

        // paused 期间不出队
        q.pause_and_wait();
        q.enqueue(PathBuf::from("/tmp/note-b"));
        std::thread::sleep(std::time::Duration::from_millis(300));
        assert_eq!(PROCESSED.load(Ordering::SeqCst), 1, "暂停期间不处理");
        q.unpause();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
        while PROCESSED.load(Ordering::SeqCst) < 2 {
            assert!(std::time::Instant::now() < deadline, "恢复后应继续处理");
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
    }
```

(单测串行处理一个静态计数器,cargo test 默认并行下该测试独占自身静态量,无互踩。)

- [ ] **Step 2: 跑测确认失败** → **Step 3: 实现** → **Step 4: 通过**。
- [ ] **Step 5: Commit**:`feat(store): 全局串行转码队列(录制让路/迁移暂停/续录摘单等待)`

---

### Task 7: lib.rs 路径改造 + 转码接线(启动扫描/停止入队/续录解码)

**Files:**
- Modify: `src-tauri/src/lib.rs`

**Interfaces:**
- Consumes: Task 1 `resolve_data_root`、Task 5/6 transcode 模块。
- Produces:
  - `fn data_root(app: &AppHandle) -> anyhow::Result<PathBuf>`:app_data_dir + settings::load → resolve_data_root。
  - `notes_dir(app)` 改挂 data_root 下;`load_voiceprint_seeds`、spawn_session 的 `vp_store_d`、`open_voiceprint_store` 三处 `app_data_dir()` 换 `data_root`(settings 读写两命令不换)。
  - `AppState` 增 `transcode: Arc<store::transcode::TranscodeQueue>`(`Default` 手工实现或 `new()` 构造后 manage)。
  - setup:①settings 载入后 `models::set_models_override`;②data_root 若非默认,`app.asset_protocol_scope().allow_directory(&data_root, true)`;③陈旧头修复扫描改用 data_root,并同一循环里对 `state=="complete"` 且有 `*.wav`(>44 字节)的笔记 `transcode.enqueue`;④`transcode.spawn_worker(st.running.clone(), store::transcode::transcode_note_dir)`。
  - stop_recording:finalize 成功后 `state.transcode.enqueue(note_dir)`(note_dir 从 writer 取,finalize 前克隆)。
  - spawn_session Resume 分支:writer resume 成功取得 note_dir 后、构建 audio_sinks 前:`transcode_queue.cancel_and_wait(&note_dir); store::transcode::decode_note_to_wav(&note_dir);`(queue 的 Arc 经 spawn_session 参数传入,与其余 Arc 同列)。

meta.json state 判定:读 `note_dir/meta.json` 用 `serde_json::from_str::<store::NoteMeta>`,损坏跳过(不入队)。

- [ ] **Step 1: 写失败测试**:路径纯逻辑已在 Task 1 测;本任务为接线,以编译 + 既有 142+ 测试回归为门,另补一个集成测(store 层,模拟启动扫描判定函数——把"该不该入队"抽成纯函数):

```rust
// lib.rs tests 追加
    #[test]
    fn should_enqueue_only_complete_notes_with_wav() {
        use super::should_enqueue_transcode;
        let tmp = tempfile::tempdir().unwrap();
        // 无 meta → 否
        assert!(!should_enqueue_transcode(tmp.path()));
        let meta = |state: &str| format!(
            r#"{{"schema_version":1,"id":"n","title":"t","started_at":"","ended_at":null,"state":"{state}"}}"#);
        std::fs::write(tmp.path().join("meta.json"), meta("recording")).unwrap();
        std::fs::write(tmp.path().join("mic.wav"), vec![0u8; 100]).unwrap();
        assert!(!should_enqueue_transcode(tmp.path()), "已中断可续录,不转码");
        std::fs::write(tmp.path().join("meta.json"), meta("complete")).unwrap();
        assert!(should_enqueue_transcode(tmp.path()));
        std::fs::remove_file(tmp.path().join("mic.wav")).unwrap();
        assert!(!should_enqueue_transcode(tmp.path()), "无 wav 无事可做");
    }
```

`fn should_enqueue_transcode(note_dir: &Path) -> bool`:meta 可解析且 state=="complete" 且目录下存在 >44 字节的 `*.wav`。

- [ ] **Step 2: 跑测确认失败** → **Step 3: 实现**(注意 spawn_session 新增 `transcode: Arc<TranscodeQueue>` 参数,start/resume 两调用点同步)。
- [ ] **Step 4: `cargo test` 全量回归通过**。
- [ ] **Step 5: Commit**:`feat: 存储路径走 data_root,转码队列接线(启动回溯/停止入队/续录解码)`

---

### Task 8: ASR 选型接线 + 模型命令改造

**Files:**
- Modify: `src-tauri/src/lib.rs`

**Interfaces:**
- Produces:
  - `fn whisper_dir() -> PathBuf`(`models::root().join("sherpa-onnx-whisper-base")`)
  - `fn new_recognizer(asr_model: &str) -> anyhow::Result<Box<dyn asr::Recognizer>>`(whisper → `asr::whisper::WhisperRecognizer::new(&whisper_dir())`;否则 SenseVoice。工厂是唯一实例化点:preload、spawn_session 兜底两处全换)
  - `fn current_asr(app: &AppHandle) -> String`(app_data_dir → settings::load → asr_model;失败默认 sense_voice)
  - `preload_models` 增 `app: AppHandle` 参数,线程内 `current_asr` 后走工厂;三个调用点(setup/stop/download 完成)同步。
  - `start_recording`/`resume_recording`:`models::recording_ready(&current_asr(&app))`,错误文案改「模型缺失:请先在设置页下载所选识别模型」。
  - `models_status` 命令:`models::status(&current_asr(&app))`。
  - `download_models` 增参 `ids: Option<Vec<String>>`:Some → 按 id 过滤 ARTIFACTS;None → `required_now(a.id, 当前选型) || a.id == "speaker"`(与旧行为等价:vad+选中 ASR+speaker)。
  - 新命令 `delete_model(app, state, id: String) -> Result<(), String>`:录制中/下载中拒绝;按 kind 删文件(File → files[0].rel_path,TarBz2 → dest_dir 整目录);删 `"asr"|"whisper"` 清 recognizer_cache 槽、删 `"speaker"` 清 embedder_cache 槽。
  - `set_settings` 改造:load 旧值对比——`data_dir`/`models_dir` 变更 → `Err("存储目录变更请使用迁移功能")`;`asr_model` 变更且无活动会话 → 清 recognizer_cache 槽 + save 后 `preload_models`;録制中改 asr_model → `Err("录制中不能切换识别模型")`。save 后若 models_dir 未变无需动 override(迁移命令才动)。

- [ ] **Step 1: 写失败测试**(纯逻辑部分)

```rust
    #[test]
    fn download_selection_defaults_to_required_plus_speaker() {
        use super::default_download_ids;
        let ids = default_download_ids("sense_voice");
        assert_eq!(ids, vec!["vad", "speaker", "asr"]);
        let ids = default_download_ids("whisper");
        assert_eq!(ids, vec!["vad", "speaker", "whisper"]);
    }
```

`fn default_download_ids(asr_model: &str) -> Vec<&'static str>`(遍历 ARTIFACTS 保序过滤),`download_models` None 分支用它。

- [ ] **Step 2: 跑测确认失败** → **Step 3: 实现**(invoke_handler 注册 `delete_model`;Task 2 的临时 `ASR_SENSE_VOICE` 硬编码全部换 `current_asr`)。
- [ ] **Step 4: `cargo test` 全量通过**。
- [ ] **Step 5: Commit**:`feat: ASR 选型工厂接线,download 按选型/按 id,delete_model,set_settings 守卫`

---

### Task 9: 目录迁移引擎与两条迁移命令

**Files:**
- Create: `src-tauri/src/store/migrate.rs`
- Modify: `src-tauri/src/store/mod.rs`(`pub mod migrate;`)、`src-tauri/src/ipc.rs`、`src-tauri/src/lib.rs`

**Interfaces:**
- Produces(migrate.rs,纯文件逻辑,不碰 tauri):
  - `pub fn dir_is_usable_target(dir: &Path) -> anyhow::Result<()>`(不存在→Ok;存在且为空目录→Ok;其余 Err「目标目录非空」/「不是目录」)
  - `pub fn copy_tree(src: &Path, dst: &Path) -> anyhow::Result<(u64, u64)>`(递归复制,返回(文件数,总字节);跳过 symlink)
  - `pub fn verify_tree(src: &Path, dst: &Path) -> anyhow::Result<()>`(双侧 walk 数量+字节一致,不一致 Err)
  - `pub fn migrate_entries(old_root: &Path, new_root: &Path, entries: &[&str]) -> anyhow::Result<()>`:create_dir_all(new_root) → 对每个存在的 entry copy_tree → verify_tree → 全部成功后逐 entry 删旧;任何失败 → 清理 new_root 下已复制内容后 Err(旧数据未动)。
- Produces(ipc.rs):`pub struct MigrateEvent { pub kind: String, pub phase: String, pub message: String }`(Serialize+Clone,kind∈{"data","models"},phase∈{"copying","done","error"})
- Produces(lib.rs):
  - `#[tauri::command] fn migrate_data_dir(app, state, new_dir: String) -> Result<(), String>`:守卫(有会话/下载中 → Err;`dir_is_usable_target`)→ 后台线程:`transcode.pause_and_wait()` → emit copying → `migrate_entries(旧 data_root, 新, &["notes", "voiceprints.json", "voiceprints"])` → settings.data_dir=Some(新) 保存 → `asset_protocol_scope().allow_directory(新, true)` → unpause → emit done;失败 unpause + emit error。线程期间复用 `download_running` 位做全局互斥(迁移与下载/再次迁移互斥;`start_recording` 已有 running 守卫,再加 `download_running` 检查拒绝开录,错误文案「正在迁移或下载,稍后再试」——顺带把下载中开录也守住,原先只靠模型 present 判定)。
  - `#[tauri::command] fn migrate_models_dir(...)`:同构;守卫多一条 `std::env::var("VN_MODELS")` 生效则 Err;entries = 旧 models root 的全部顶层条目(read_dir 收集文件名);成功后 settings.models_dir 保存 + `models::set_models_override(Some(新))`。
  - invoke_handler 注册两命令。

- [ ] **Step 1: 写失败测试**(migrate.rs tests)

```rust
    #[test]
    fn migrate_entries_moves_and_cleans_old() {
        let tmp = tempfile::tempdir().unwrap();
        let old = tmp.path().join("old");
        let new = tmp.path().join("new");
        std::fs::create_dir_all(old.join("notes/n1")).unwrap();
        std::fs::write(old.join("notes/n1/meta.json"), b"{}").unwrap();
        std::fs::write(old.join("voiceprints.json"), b"{}").unwrap();
        // "voiceprints" 目录不存在:缺项跳过不报错
        migrate_entries(&old, &new, &["notes", "voiceprints.json", "voiceprints"]).unwrap();
        assert!(new.join("notes/n1/meta.json").exists());
        assert!(new.join("voiceprints.json").exists());
        assert!(!old.join("notes").exists(), "成功后删旧");
        assert!(!old.join("voiceprints.json").exists());
    }

    #[test]
    fn migrate_failure_keeps_old_and_cleans_new() {
        let tmp = tempfile::tempdir().unwrap();
        let old = tmp.path().join("old");
        let new = tmp.path().join("new");
        std::fs::create_dir_all(old.join("notes")).unwrap();
        std::fs::write(old.join("notes/a.bin"), b"data").unwrap();
        // 让 verify 失败:预先在 new 放同名目录制造复制冲突不可行(copy 会并入),
        // 改为直接测清理助手——复制成功后人为破坏 dst 再走 verify 分支:
        std::fs::create_dir_all(&new).unwrap();
        let (n, _) = copy_tree(&old.join("notes"), &new.join("notes")).unwrap();
        assert_eq!(n, 1);
        std::fs::remove_file(new.join("notes/a.bin")).unwrap();
        assert!(verify_tree(&old.join("notes"), &new.join("notes")).is_err(), "缺文件必被察觉");
        assert!(old.join("notes/a.bin").exists(), "旧数据全程未动");
    }

    #[test]
    fn target_must_be_empty_or_absent() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(dir_is_usable_target(&tmp.path().join("absent")).is_ok());
        assert!(dir_is_usable_target(tmp.path()).is_ok(), "空目录可用");
        std::fs::write(tmp.path().join("x"), b"1").unwrap();
        assert!(dir_is_usable_target(tmp.path()).is_err(), "非空拒绝");
    }
```

- [ ] **Step 2: 跑测确认失败** → **Step 3: 实现 migrate.rs** → **Step 4: 通过** → **Step 5: 实现 ipc/lib.rs 命令与守卫,`cargo test` 全量回归**。
- [ ] **Step 6: Commit**:`feat: 数据/模型目录自动迁移(复制-校验-删旧,失败回退)`

---

### Task 10: dialog 插件接线

**Files:**
- Modify: `src-tauri/Cargo.toml`(`tauri-plugin-dialog = "2"`)
- Modify: `package.json`(dependencies 加 `"@tauri-apps/plugin-dialog": "^2"`,跑 `npm install`)
- Modify: `src-tauri/src/lib.rs`(builder 链加 `.plugin(tauri_plugin_dialog::init())`)
- Modify: `src-tauri/capabilities/default.json`(permissions 加 `"dialog:default"`)

- [ ] **Step 1: 实现以上四处** → **Step 2: `cargo build` + `npm run build` 通过** → **Step 3: Commit**:`chore: 接入 tauri-plugin-dialog(设置页目录选择)`

---

### Task 11: 前端 API 层扩展

**Files:**
- Modify: `src/lib/models.ts`

**Interfaces:**
- Produces(供设置页/下载卡消费):

```ts
export type Settings = {
  mirror_enabled: boolean;
  mirror_prefix: string;
  data_dir?: string | null;
  models_dir?: string | null;
  asr_model: string;
};
export type MigrateEvent = { kind: "data" | "models"; phase: "copying" | "done" | "error"; message: string };

export const downloadModels = (ids?: string[]) => invoke<void>("download_models", { ids: ids ?? null });
export const deleteModel = (id: string) => invoke<void>("delete_model", { id });
export const migrateDataDir = (newDir: string) => invoke<void>("migrate_data_dir", { newDir });
export const migrateModelsDir = (newDir: string) => invoke<void>("migrate_models_dir", { newDir });
export function onMigrate(cb: (e: MigrateEvent) => void) {
  return listen<MigrateEvent>("migrate", (ev) => cb(ev.payload));
}
```

(`modelsStatus`/`getSettings`/`setSettings`/`onModelDownload` 保持;`downloadModels` 旧调用点无参兼容。)

- [ ] **Step 1: 实现** → **Step 2: `npm run check` 0/0** → **Step 3: Commit**:`feat(ui): 模型/迁移前端 API`

---

### Task 12: 设置页 + 侧栏入口

**Files:**
- Create: `src/routes/settings/+page.svelte`
- Modify: `src/lib/Sidebar.svelte`(「声纹库」nav-link 后加同构「设置」入口,齿轮线框 svg:圆心 `circle cx=8 cy=8 r=2.2` + 八向短齿 path,stroke 同现有 nav-icon 规范;`class:current={$page.url.pathname === "/settings"}`)

**行为规格**(视觉全部复用 DESIGN.md token 与 speakers 页排版惯例:h1 + section 块,hairline 分隔):

- **存储区块**:两行(数据存储目录/模型存储目录)。每行:label + 当前生效路径(`ink-secondary`,等宽字体不必)+「更改…」secondary 按钮。当前路径来源:`getSettings()` 的 `data_dir`/`models_dir`,空则显示「默认(应用数据目录)」。点击「更改…」:`open({ directory: true })`(`@tauri-apps/plugin-dialog`)→ 选中后行内出现确认条「将把现有数据完整迁移到 <路径>,期间不能录制。」+「开始迁移」primary/「取消」;开始后调 `migrateDataDir/migrateModelsDir`,`onMigrate` 监听:copying → 行内「迁移中…」禁用两行按钮;done → 刷新 settings 显示新路径;error → danger 横幅显示 message。`recording.isLive` 或下载中(见下)时按钮禁用并带 title 说明。
- **模型区块**:`modelsStatus()` 列全部工件行:label + 约 xxMB + 状态。present → 「已下载」`ink-faint` + 悬停显影「删除」danger 链接钮(`deleteModel(id)` 后刷新;失败 danger 横幅);缺失 → 「下载」secondary 钮(`downloadModels([id])`),下载中该行进度条(复用 ModelDownloadCard 的 bar/fill 样式,`onModelDownload` 过滤本工件 id)。区块底部:镜像开关 checkbox + 前缀输入(逻辑照搬 ModelDownloadCard 现实现:toggle 即 `setSettings`,prefix onblur 保存)。任何下载进行中 → 迁移按钮禁用(以收到 downloading 进度事件未终结为准,页内状态)。
- **语音识别区块**:radio 两项。SenseVoice:「推荐。中英日韩粤,带语言幻觉过滤与段内说话人分离的完整功能。」Whisper:「多语种。段内说话人分离退化为段级标签,语言过滤仅按文本兜底。切换后下一场录制生效。」onchange:`getSettings()` 取新鲜值改 `asr_model` 后 `setSettings`;失败(如录制中)danger 横幅并回弹选项。所选模型未下载时 radio 下方 warning 横幅「所选识别模型未下载,请在上方模型区块下载。」(`modelsStatus().artifacts` 查对应 id present;sense_voice→"asr",whisper→"whisper")。
- 录制中(`recording.isLive`):迁移/删除/切型全禁用;下载允许(与现状一致)。

- [ ] **Step 1: 实现页面与侧栏入口**(单文件 Svelte 5 runes 风格,对照 speakers 页;无单测,验收走 check/build+冒烟)。
- [ ] **Step 2: `npm run check` 0/0、`npm run build` 通过**。
- [ ] **Step 3: Commit**:`feat(ui): 系统设置页(存储迁移/模型管理/ASR 选型)与侧栏入口`

---

### Task 13: ModelDownloadCard 收敛为下载引导

**Files:**
- Modify: `src/lib/ModelDownloadCard.svelte`

**变更:**
- `missing` 过滤加选型条件:`status.artifacts.filter((a) => !a.present && (a.required_for_recording || a.id === "speaker"))`(后端 `required_for_recording` 已按选型动态计算,whisper 未选不再出现)。
- 镜像开关与前缀输入整块删除(`getSettings/setSettings` import 与 `settings` state 一并清);actions 区补一行 `ink-faint` 小字:「下载镜像可在设置页配置。」
- 其余(进度/取消/续传文案)不动;`downloadModels()` 无参调用即默认集,不用改。

- [ ] **Step 1: 实现** → **Step 2: `npm run check` 0/0** → **Step 3: Commit**:`refactor(ui): 下载卡镜像配置移交设置页,缺失列表按选型过滤`

---

### Task 14: 全量验证与 PR

- [ ] **Step 1**: `cargo test`(src-tauri 下)全过;`npm run check` 0 错 0 警;`cargo build` + `npm run build` 无新警告。
- [ ] **Step 2**: 冒烟准备说明(执行者产出文字指引,人工冒烟由用户做):spec「验收冒烟」6 项逐条列出操作路径。
- [ ] **Step 3**: push 分支,`gh pr create`,PR 描述含:变更概览、6 项冒烟清单、已知取舍(spec §已知取舍 照抄)、磁盘效果(115→约 14MB/h/源)。

---

## Self-Review 记录(计划完成后已自查)

- spec 覆盖:压缩(T4-T7)、回溯(T7)、续录解码(T5/T7)、设置字段(T1)、目录解析(T1/T2/T7)、迁移(T9/T10/T12)、模型管理集中(T8/T11/T12/T13)、ASR 选型(T2/T8/T12)、降级文案(T12)、错误姿态(各任务失败分支)、测试清单(各任务 Step 1)、冒烟(T14)。
- Whisper 降级零改动的依据:`session.rs::is_foreign_final` 对空 lang 走文本占比兜底(既有测试「lang 空,兜底不命中」);`diar/split.rs:132` 对空/错长 timestamps 已有回退。计划中无这两处的改动任务,系有意。
- 类型一致性:`TranscodeQueue` 方法名、`set_track_compressed/clear_track_compressed`、`required_now`、`default_download_ids`、`MigrateEvent` 在各任务 Interfaces 与代码块中拼写一致。
- 已知施工顺序约束:T2 会临时硬编码 `ASR_SENSE_VOICE` 保编译,T8 收口——两任务的 Step 里都已写明。
