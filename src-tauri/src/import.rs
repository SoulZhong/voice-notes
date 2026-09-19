//! 导入本地音频文件建笔记(2026-09-19)。
//! spec: docs/superpowers/specs/2026-09-19-audio-import-design.md
//!
//! 一句话:**导入 = 建一篇只有 mic 轨的笔记,再对它跑一遍离线转写**。本模块只管
//! 前半句(解码落轨 + 建档),后半句原样复用重转写链路(`run_retranscribe_once`),
//! 不另起管线——转写、段内切分、声纹归属、Aing、日历、identify、拟题全部走录音
//! 笔记已经走熟的那条路。
//!
//! # 为什么写成 `mic` 轨而不是 `mixed`
//!
//! 看上去导入文件更像"成品轨"(一条轨里所有人都在),但 `mixed` 在本仓有专门语义:
//! 它是 mic+system **混出来的派生物**,因此 ①`store::audio::list_tracks` 刻意把它
//! 排除在播放器轨列表之外(源轨与成品轨叠播会音量翻倍),导入笔记若只有 mixed 轨,
//! 笔记页将一条可播的轨都没有;②`retranscribe::input::mixed_untrusted` 要拿源轨
//! 读数对账成品轨完整性,导入笔记没有源轨可对,恒判"不可信"、成品轨重转写入口永久
//! 置灰。而"只有 mic、没有 system"本就是合法笔记形态(只录麦克风的场次),播放、
//! 波形、转码、重转写、裁剪导出全链路都已支持,导入直接落在这条既有形态上。
//!
//! # 平台
//!
//! macOS 走系统内建 afconvert(与本仓其余音频转换同一条路,格式面等于 CoreAudio);
//! 其余平台只解 WAV(纯 Rust,hound + 与实时采集同一条 `resample_linear`)。这与本仓
//! 既有的平台落差一致(转码/离线回声清洗同样只在 macOS)。

use crate::store::audio::AUDIO_SAMPLE_RATE;
use crate::store::{NoteMeta, SCHEMA_VERSION};
use chrono::{DateTime, Local};
use std::path::{Path, PathBuf};

/// 可导入的扩展名(小写,不含点)。口径 = macOS CoreAudio 能解的常见音频容器;
/// 非 macOS 只支持其中的 wav(见 `decode_to_canonical_wav`)。这张表同时是文件
/// 选择器的过滤器与命令层的入口校验——两边同源,不各写一份。
pub const SUPPORTED_EXTS: &[&str] =
    &["mp3", "m4a", "m4b", "aac", "wav", "aif", "aiff", "caf", "mp4", "flac"];

/// 非 macOS 平台唯一能解的扩展名(纯 Rust WAV 解码器)。
pub const WAV_EXT: &str = "wav";

/// 标题最长字符数(不是字节)。meta.title 会进侧栏列表、托盘、导出文件名,
/// 一个 200 字的文件名拿来当标题会把这些面全撑爆。
const MAX_TITLE_CHARS: usize = 60;

/// 解码中转文件的前缀。落在 notes 根下(与笔记目录同一文件系统,rename 才是原子的),
/// 点号开头 + 非目录,`NoteStore::list` 只枚举目录,天然不会把它当成一篇坏笔记。
const TMP_PREFIX: &str = ".import-";

/// 文件名里那些"设备自动生成、不含任何主题信息"的词。见 `title_from_filename`。
const GENERIC_STEMS: &[&str] = &[
    "录音", "新录音", "录音文件", "语音", "语音备忘录", "未命名", "新建录音", "会议", "会议录音",
    "audio", "recording", "recordings", "new recording", "voice", "voice memo", "memo", "untitled",
    "sound", "track", "rec", "call", "meeting", "note",
];

/// 扩展名(小写)。无扩展名 → None。
pub fn ext_of(path: &Path) -> Option<String> {
    path.extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_ascii_lowercase())
}

/// 扩展名是否在导入白名单内。
pub fn is_supported(path: &Path) -> bool {
    ext_of(path).is_some_and(|e| SUPPORTED_EXTS.contains(&e.as_str()))
}

/// 本平台能否解这个扩展名(白名单之内再过一道平台闸)。
pub fn platform_supports(ext: &str) -> bool {
    cfg!(target_os = "macos") || ext == WAV_EXT
}

/// 按字符截断(不是字节:中文标题按字节截会切出半个字)。
fn truncate_chars(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    s.chars().take(max).collect()
}

/// 由文件名(不含扩展名)推笔记标题。`None` = 用默认标题,把拟题让给 Aing。
///
/// 判据:折叠内部空白后,把数字与常见分隔符从**首尾**剥掉;剩下的若为空、或(ASCII
/// 忽略大小写后)整体落在 `GENERIC_STEMS` 里,就是设备自动起的名(「录音 3」
/// 「New Recording 12」「20260918_143022」),没有主题信息可用;反之(「客户访谈-张三」)
/// 是人自己起的名,比 LLM 拟的题更权威,直接采用。
///
/// 为什么这条判据必须存在:`store::writer::is_default_title` 是 Aing 自动拟题
/// (`NoteStore::rename_if_default`)的唯一闸门,标题一旦不是默认样式,自动拟题整篇
/// 让路。把「20260918_143022」写进标题,等于用一串没有信息的数字永久占掉那个位置。
pub fn title_from_filename(stem: &str) -> Option<String> {
    let cleaned = stem.split_whitespace().collect::<Vec<_>>().join(" ");
    if cleaned.is_empty() {
        return None;
    }
    let core = cleaned.trim_matches(|c: char| c.is_ascii_digit() || " -_.#()[]【】".contains(c));
    if core.is_empty() || GENERIC_STEMS.iter().any(|g| core.eq_ignore_ascii_case(g)) {
        return None;
    }
    Some(truncate_chars(&cleaned, MAX_TITLE_CHARS))
}

/// 任意受支持音频 → 本仓标准轨(16k / 单声道 / s16le / 标准 44 字节头)落到 `dest`。
///
/// macOS 先试 afconvert;它解 WAV 失败时(非标准块序、奇异位深等变体)降级纯 Rust
/// 路径——一条我们自己就能读的轨,不该因为子进程挑食而导入失败。非 WAV 的失败直接
/// 上报:那是真的解不了,降级也无能为力。
pub fn decode_to_canonical_wav(src: &Path, dest: &Path) -> anyhow::Result<()> {
    let ext = ext_of(src).unwrap_or_default();
    if cfg!(target_os = "macos") {
        match crate::store::transcode::decode_to_standard_wav(src, dest) {
            Ok(()) => return Ok(()),
            Err(e) if ext == WAV_EXT => {
                eprintln!("导入: afconvert 解 WAV 失败,降级纯 Rust 解码: {e}");
            }
            Err(e) => return Err(e),
        }
    }
    if ext != WAV_EXT {
        anyhow::bail!("当前平台只能导入 WAV(其余格式依赖 macOS 音频转换工具): {}", src.display());
    }
    wav_to_canonical(src, dest)
}

/// 纯 Rust WAV → 标准轨。多声道取算术平均下混;采样率用与实时采集**同一条**
/// `resample_linear` 降到 16k(录制链路 48k→16k 走的就是它),导入不另立一套口径。
pub fn wav_to_canonical(src: &Path, dest: &Path) -> anyhow::Result<()> {
    let mut reader = hound::WavReader::open(src)?;
    let spec = reader.spec();
    if spec.channels == 0 {
        anyhow::bail!("WAV 声道数为 0: {}", src.display());
    }
    let interleaved: Vec<f32> = match spec.sample_format {
        hound::SampleFormat::Float => reader.samples::<f32>().collect::<Result<_, _>>()?,
        hound::SampleFormat::Int => {
            // 位深归一化:i32 读出的是原位深的有符号整数,除以该位深的满量程。
            let full = (1i64 << (spec.bits_per_sample.max(1) - 1)) as f32;
            reader
                .samples::<i32>()
                .map(|s| s.map(|v| v as f32 / full))
                .collect::<Result<_, _>>()?
        }
    };
    let ch = spec.channels as usize;
    let mono: Vec<f32> = if ch == 1 {
        interleaved
    } else {
        interleaved
            .chunks(ch)
            .map(|frame| frame.iter().sum::<f32>() / frame.len() as f32)
            .collect()
    };
    let pcm = crate::audio::resample::resample_linear(&mono, spec.sample_rate, AUDIO_SAMPLE_RATE);
    write_canonical_wav(dest, &pcm)
}

/// f32 样本 → 标准 44 头 16k 单声道 s16le WAV,tmp 同目录写 + rename 原子落位。
fn write_canonical_wav(dest: &Path, pcm: &[f32]) -> anyhow::Result<()> {
    if pcm.is_empty() {
        anyhow::bail!("解码得到空音频: {}", dest.display());
    }
    let data_len = pcm.len().saturating_mul(2);
    // WAV 的 data 块长度字段是 u32:超 4GiB(16k 单声道约 37 小时)写下去会悄悄截断,
    // 落成一条头尾对不上的轨。宁可当场拒绝,不留一条"看起来正常"的坏轨。
    if data_len > u32::MAX as usize {
        anyhow::bail!("音频过长(超出 WAV 单文件上限),请先切分后导入");
    }
    let mut bytes = Vec::with_capacity(44 + data_len);
    bytes.extend_from_slice(&crate::store::audio::wav_header(data_len as u32));
    for s in pcm {
        bytes.extend_from_slice(&crate::store::audio::f32_to_s16(*s).to_le_bytes());
    }
    let tmp = dest.with_extension("wav.import.tmp");
    std::fs::write(&tmp, &bytes)?;
    std::fs::rename(&tmp, dest)?;
    Ok(())
}

/// 一次导入的产物。
pub struct Imported {
    pub note_id: String,
    pub dir: PathBuf,
    /// 音轨时长(毫秒,按落盘字节实算)。
    pub duration_ms: u64,
}

/// 建一篇导入笔记:解码 → 分配目录 → 落 `mic.wav` → 写 meta(state=complete)。
///
/// 返回后盘上即是一篇**有音频、没转写**的完整笔记——转写由调用方另起(复用重转写
/// 链路)。这个中间态是刻意的:转写失败时笔记与音频仍在,用户可以听、可以换引擎
/// 重新分析,而不是连音频一起丢掉。
///
/// 失败不留半成品:解码在 notes 根下的中转文件里做(同一文件系统,rename 才原子),
/// 解码失败时笔记目录根本还没建;建档阶段任何一步失败则整个目录删掉。
pub fn create_note(
    notes_dir: &Path,
    src: &Path,
    now: DateTime<Local>,
) -> anyhow::Result<Imported> {
    if !src.is_file() {
        anyhow::bail!("文件不存在: {}", src.display());
    }
    std::fs::create_dir_all(notes_dir)?;
    // 中转文件名带进程 id + 时间戳:两个实例同时导入不会互相覆盖中转件。
    let tmp = notes_dir.join(format!(
        "{TMP_PREFIX}{}-{}.wav",
        std::process::id(),
        now.format("%Y%m%d%H%M%S%3f")
    ));
    let cleanup_tmp = || {
        let _ = std::fs::remove_file(&tmp);
    };
    decode_to_canonical_wav(src, &tmp).inspect_err(|_| cleanup_tmp())?;

    let duration_ms = {
        let len = std::fs::metadata(&tmp).map(|m| m.len()).unwrap_or(0);
        crate::store::audio::bytes_to_ms(len.saturating_sub(44))
    };
    if duration_ms == 0 {
        cleanup_tmp();
        anyhow::bail!("音频时长为 0,没有可转写的内容: {}", src.display());
    }

    let stem = src.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
    let (note_id, dir) = crate::store::writer::alloc_note_dir(notes_dir, &now).inspect_err(|_| cleanup_tmp())?;
    let build = || -> anyhow::Result<()> {
        std::fs::rename(&tmp, dir.join("mic.wav"))?;
        let meta = NoteMeta {
            schema_version: SCHEMA_VERSION,
            id: note_id.clone(),
            title: title_from_filename(&stem)
                .unwrap_or_else(|| crate::store::writer::unique_default_title(notes_dir, &now)),
            // started_at 取"导入这一刻"而不是文件的修改时间:后者常年被复制/转存
            // 抹平,一个 2020 年的 mtime 会把新导入的笔记直接沉到列表最底下,用户
            // 会以为导入没成功。ended_at 顺延音轨时长,列表副标题的时长读数
            // (summarize 的 duration_from_meta 回退)在转写落段之前就是对的。
            started_at: now.to_rfc3339(),
            ended_at: Some((now + chrono::Duration::milliseconds(duration_ms as i64)).to_rfc3339()),
            state: "complete".into(),
            calendar: None,
            calendar_cleared: false,
            attendees: vec![],
            attendees_removed: vec![],
            asr_engine: None,
            imported_from: Some(
                src.file_name().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default(),
            ),
        };
        crate::store::write_meta_atomic(&dir, &meta)?;
        Ok(())
    };
    if let Err(e) = build() {
        cleanup_tmp();
        let _ = std::fs::remove_dir_all(&dir);
        return Err(e);
    }
    Ok(Imported { note_id, dir, duration_ms })
}

/// 清理上次崩溃遗留的解码中转件(导入入口每次调用前跑一遍)。中转件可能有数百 MB,
/// 留着白占盘;它不在任何笔记目录里,删掉不影响任何一篇笔记。
pub fn sweep_stale_tmp(notes_dir: &Path) {
    let Ok(rd) = std::fs::read_dir(notes_dir) else { return };
    for e in rd.flatten() {
        let name = e.file_name();
        let Some(name) = name.to_str() else { continue };
        if name.starts_with(TMP_PREFIX) && e.path().is_file() {
            if let Err(err) = std::fs::remove_file(e.path()) {
                eprintln!("导入: 清理残留中转件失败({name}): {err}");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 设备自动起的名一律让位给 Aing 拟题(返回 None),人起的名直接采用。
    /// 这条判据错一边的代价不对称:误判成"人起的名"会让一串数字永久占住标题,
    /// 误判成"自动名"只是多一次 LLM 拟题——所以表里宁可多收几个常见词。
    #[test]
    fn title_from_filename_skips_device_generated_names() {
        for generic in [
            "20260918_143022",
            "录音 3",
            "录音",
            "New Recording 12",
            "new recording",
            "voice 001",
            "Untitled",
            "  ",
            "2026-09-18 14-30-22",
            "rec_0007",
        ] {
            assert_eq!(title_from_filename(generic), None, "应判为自动名: {generic:?}");
        }
        assert_eq!(title_from_filename("客户访谈-张三"), Some("客户访谈-张三".into()));
        assert_eq!(title_from_filename("Q3 Planning"), Some("Q3 Planning".into()));
        // 内部空白折叠,首尾修剪。
        assert_eq!(title_from_filename("  发布   计划  "), Some("发布 计划".into()));
    }

    /// 超长文件名按**字符**截断:按字节截会把中文切出半个字,写进 meta 就是乱码。
    #[test]
    fn title_truncates_by_chars_not_bytes() {
        let long = "标".repeat(MAX_TITLE_CHARS + 20);
        let t = title_from_filename(&long).expect("非自动名");
        assert_eq!(t.chars().count(), MAX_TITLE_CHARS);
        assert!(t.chars().all(|c| c == '标'), "不得切出半个字");
    }

    #[test]
    fn supported_ext_is_case_insensitive_and_bounded() {
        assert!(is_supported(Path::new("/a/b/录音.M4A")));
        assert!(is_supported(Path::new("/a/b/x.mp3")));
        assert!(!is_supported(Path::new("/a/b/x.txt")));
        assert!(!is_supported(Path::new("/a/b/noext")));
    }

    /// 平台闸:非 macOS 只放行 wav,白名单里的其余格式一律拒(没有解码器)。
    #[test]
    fn platform_gate_matches_decoder_availability() {
        assert!(platform_supports("wav"), "wav 全平台可解");
        assert_eq!(platform_supports("mp3"), cfg!(target_os = "macos"));
    }

    /// 纯 Rust 路径端到端:44.1kHz 立体声 16bit → 16k 单声道标准轨。
    /// 断言落盘字节(44 头 + 样本数)与下混:左右反相的一帧下混后必须归零。
    #[test]
    fn wav_to_canonical_downmixes_and_resamples() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("in.wav");
        let spec = hound::WavSpec {
            channels: 2,
            sample_rate: 44_100,
            bits_per_sample: 16,
            sample_format: hound::SampleFormat::Int,
        };
        let mut w = hound::WavWriter::create(&src, spec).unwrap();
        // 1 秒:前半段左右反相(下混应归零),后半段同相(下混保留)。
        for i in 0..44_100 {
            if i < 22_050 {
                w.write_sample(8_000i16).unwrap();
                w.write_sample(-8_000i16).unwrap();
            } else {
                w.write_sample(8_000i16).unwrap();
                w.write_sample(8_000i16).unwrap();
            }
        }
        w.finalize().unwrap();

        let dest = dir.path().join("out.wav");
        wav_to_canonical(&src, &dest).unwrap();
        let bytes = std::fs::read(&dest).unwrap();
        assert_eq!(&bytes[0..4], b"RIFF");
        let samples = (bytes.len() - 44) / 2;
        // 1 秒 @16k;线性重采样按全局比值取整,允 1 样本边界差。
        assert!(samples.abs_diff(16_000) <= 1, "重采样后应约 16000 样本,实际 {samples}");
        let pcm = crate::store::transcode::read_wav_f32(&dest).unwrap();
        let (head, tail) = pcm.split_at(pcm.len() / 2);
        let peak = |xs: &[f32]| xs.iter().fold(0.0f32, |m, x| m.max(x.abs()));
        // 反相段留一点重采样边界余量(过渡点插值),用中段取样避开接缝。
        assert!(peak(&head[100..head.len() - 100]) < 0.01, "反相段下混后应接近静音");
        assert!(peak(&tail[100..tail.len() - 100]) > 0.2, "同相段下混后应保留电平");
    }

    /// 建档端到端:目录建出、mic.wav 在位、meta 是 complete + 带来源文件名,
    /// 且时长按落盘字节算进了 ended_at(转写还没跑,列表时长就已经对)。
    #[test]
    fn create_note_lands_track_and_meta() {
        let root = tempfile::tempdir().unwrap();
        let notes = root.path().join("notes");
        let src = root.path().join("客户访谈-张三.wav");
        let spec = hound::WavSpec {
            channels: 1,
            sample_rate: 16_000,
            bits_per_sample: 16,
            sample_format: hound::SampleFormat::Int,
        };
        let mut w = hound::WavWriter::create(&src, spec).unwrap();
        for i in 0..32_000 {
            w.write_sample(((i % 100) as i16 - 50) * 100).unwrap();
        }
        w.finalize().unwrap();

        let now = chrono::Local::now();
        let out = super::create_note(&notes, &src, now).unwrap();
        assert!(out.dir.join("mic.wav").is_file(), "mic.wav 必须在位");
        assert!(out.duration_ms.abs_diff(2_000) <= 50, "2 秒音频,实际 {}ms", out.duration_ms);
        let meta: NoteMeta =
            serde_json::from_str(&std::fs::read_to_string(out.dir.join("meta.json")).unwrap()).unwrap();
        assert_eq!(meta.state, "complete");
        assert_eq!(meta.title, "客户访谈-张三");
        assert_eq!(meta.imported_from.as_deref(), Some("客户访谈-张三.wav"));
        assert!(meta.ended_at.is_some(), "ended_at 必须写,否则列表读不到时长");
        // 中转件不得残留。
        assert!(!std::fs::read_dir(&notes).unwrap().flatten()
            .any(|e| e.file_name().to_string_lossy().starts_with(TMP_PREFIX)));
    }

    /// **非 WAV** 端到端(macOS):自己编一个 m4a(复用生产同款 afconvert_encode 参数),
    /// 再把它导进来——覆盖"压缩容器 → afconvert 解码 → 标准 44 头"这条主路径。
    /// 上面那个 create_note 用例喂的是 WAV,走的也是 afconvert,但 WAV 不能证明容器
    /// 解码这一段(它本来就是 PCM);压缩容器才是导入的真实主场景(手机/会议软件导出)。
    #[cfg(target_os = "macos")]
    #[test]
    fn create_note_imports_compressed_container() {
        let root = tempfile::tempdir().unwrap();
        let notes = root.path().join("notes");
        // 3 秒 440Hz 正弦的标准轨 → 编成 m4a。
        let src_wav = root.path().join("tone.wav");
        let pcm: Vec<f32> = (0..48_000)
            .map(|i| (i as f32 * 440.0 * std::f32::consts::TAU / 16_000.0).sin() * 0.5)
            .collect();
        super::write_canonical_wav(&src_wav, &pcm).unwrap();
        let m4a = root.path().join("季度评审.m4a");
        crate::store::transcode::afconvert_encode(&src_wav, &m4a).unwrap();

        let out = super::create_note(&notes, &m4a, chrono::Local::now()).unwrap();
        let track = out.dir.join("mic.wav");
        assert!(track.is_file(), "m4a 导入后也必须落成 mic.wav");
        // 编解码边界允几十毫秒差(与 transcode_one 的时长核对同量级容限)。
        assert!(out.duration_ms.abs_diff(3_000) <= 200, "3 秒音频,实际 {}ms", out.duration_ms);
        // 标准 44 头(播放器 mmap 按 44+2i 索引,胖头会整轨错位)。
        let head = std::fs::read(&track).unwrap();
        assert_eq!(&head[0..4], b"RIFF");
        assert_eq!(&head[36..40], b"data", "必须是标准 44 字节头,不能留 afconvert 的胖头");
        let back = crate::store::transcode::read_wav_f32(&track).unwrap();
        let peak = back.iter().fold(0.0f32, |m, x| m.max(x.abs()));
        assert!(peak > 0.2, "解出来得有信号(实际峰值 {peak})");
        let meta: NoteMeta =
            serde_json::from_str(&std::fs::read_to_string(out.dir.join("meta.json")).unwrap()).unwrap();
        assert_eq!(meta.title, "季度评审");
        assert_eq!(meta.imported_from.as_deref(), Some("季度评审.m4a"));
    }

    /// 解不了的输入:目录一个都不许建(失败不留空壳笔记),中转件也不留。
    #[test]
    fn create_note_leaves_nothing_behind_on_decode_failure() {
        let root = tempfile::tempdir().unwrap();
        let notes = root.path().join("notes");
        let src = root.path().join("坏文件.wav");
        std::fs::write(&src, b"not a wav at all").unwrap();
        assert!(super::create_note(&notes, &src, chrono::Local::now()).is_err());
        let left: Vec<_> = std::fs::read_dir(&notes).unwrap().flatten().collect();
        assert!(left.is_empty(), "失败后 notes 根下不得有任何残留: {left:?}");
    }

    /// 残留中转件清扫:只删自己的中转件,笔记目录与别的文件一概不碰。
    #[test]
    fn sweep_removes_only_import_tmp_files() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(format!("{TMP_PREFIX}1-2.wav")), b"x").unwrap();
        std::fs::write(dir.path().join("voiceprints.json"), b"{}").unwrap();
        std::fs::create_dir(dir.path().join("20260919-101010")).unwrap();
        sweep_stale_tmp(dir.path());
        assert!(!dir.path().join(format!("{TMP_PREFIX}1-2.wav")).exists());
        assert!(dir.path().join("voiceprints.json").exists());
        assert!(dir.path().join("20260919-101010").is_dir());
    }
}
