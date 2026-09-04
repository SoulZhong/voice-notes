use super::{Note, NoteStore, RefinedDoc, SegmentRecord, SpeakerMeta};
use std::collections::BTreeMap;

impl NoteStore {
    /// 导出到用户选定路径(保存对话框流程),渲染与 render/render_refined 同源。
    /// 守卫:dest 须为绝对路径且不落在笔记数据目录内(防误选/被误传毁库);
    /// 父目录不代建——保存对话框保证父目录存在,ENOENT 直接报错。
    /// 写入走同目录临时文件 + rename 原子替换:覆盖已有文件时,磁盘满/拔盘/中途
    /// 被杀不会留下被截断的半成品,原文件要么完好要么已被完整新内容替换。
    /// range:播放器游标圈定的时间段(毫秒,半开区间),只导与之**重叠**的段/段落
    /// (跨界段整段保留,不切句——文字切半句没有意义);None = 导整篇。
    pub fn export_to(
        &self,
        id: &str,
        format: &str,
        refined: Option<&RefinedDoc>,
        dest: &std::path::Path,
        range: Option<(u64, u64)>,
    ) -> anyhow::Result<()> {
        if format != "md" && format != "txt" {
            anyhow::bail!("未知导出格式: {format}");
        }
        if let Some((s, e)) = range {
            if e <= s {
                anyhow::bail!("圈定范围为空");
            }
        }
        self.validate_export_dest(id, dest)?;
        let file_name = dest
            .file_name()
            .ok_or_else(|| anyhow::anyhow!("导出路径缺少文件名"))?;
        let content = match refined {
            Some(doc) => {
                let title = self.load(id)?.meta.title;
                match range {
                    Some((s, e)) => {
                        let mut clipped = doc.clone();
                        clipped.paragraphs.retain(|p| p.end_ms > s && p.start_ms < e);
                        render_refined(&title, &clipped, format == "md")
                    }
                    None => render_refined(&title, doc, format == "md"),
                }
            }
            None => {
                let mut note = self.load(id)?;
                if let Some((s, e)) = range {
                    note.segments.retain(|sg| sg.end_ms > s && sg.start_ms < e);
                }
                render_note(&note, format)?
            }
        };
        let tmp = dest.with_file_name(format!(
            ".{}.tmp-{}",
            file_name.to_string_lossy(),
            std::process::id()
        ));
        if let Err(e) = std::fs::write(&tmp, &content) {
            let _ = std::fs::remove_file(&tmp);
            return Err(e.into());
        }
        if let Err(e) = std::fs::rename(&tmp, dest) {
            let _ = std::fs::remove_file(&tmp);
            return Err(e.into());
        }
        Ok(())
    }

    /// export_to / export_audio_to 共用的 dest 守卫:必须绝对路径,且不得落在
    /// 笔记数据目录内(防误选/被误传毁库——那里放着 meta.json/segments.jsonl 等
    /// 笔记本体,原子替换一旦命中会静默摧毁笔记)。父目录不代建,由调用方各自处理
    /// ENOENT。
    fn validate_export_dest(&self, id: &str, dest: &std::path::Path) -> anyhow::Result<()> {
        if !dest.is_absolute() {
            anyhow::bail!("导出路径必须是绝对路径");
        }
        let note_dir = self.note_dir(id)?;
        let data_root = note_dir
            .parent()
            .map(std::path::Path::to_path_buf)
            .unwrap_or_else(|| note_dir.clone());
        let canon_root = data_root.canonicalize().unwrap_or(data_root);
        let dest_parent = dest
            .parent()
            .ok_or_else(|| anyhow::anyhow!("导出路径缺少父目录"))?;
        let canon_parent = dest_parent
            .canonicalize()
            .unwrap_or_else(|_| dest_parent.to_path_buf());
        if canon_parent.starts_with(&canon_root) {
            anyhow::bail!("导出目标不能位于笔记数据目录内");
        }
        Ok(())
    }

    /// 导出成品轨音频到用户选定路径(保存对话框流程)。守卫与 export_to 同源
    /// (validate_export_dest);源文件由 mixed_track 解析,无成品轨报错。
    /// 拷贝/裁剪走同目录临时文件 + rename 原子替换,与 export_to 同款崩溃纪律。
    /// range:时间轴毫秒半开区间;None = 整轨原样拷贝(不重编码)。圈定时裁出该段:
    /// WAV 纯字节裁剪,m4a 先解回 WAV 裁完再重编码(有损代际,但圈选导出本就是
    /// 「掐一段发出去」的场景,32kbps 语音再压一代听感无碍)。
    pub fn export_audio_to(
        &self,
        id: &str,
        dest: &std::path::Path,
        range: Option<(u64, u64)>,
    ) -> anyhow::Result<()> {
        self.validate_export_dest(id, dest)?;
        let note_dir = self.note_dir(id)?;
        let track = super::audio::mixed_track(&note_dir)
            .ok_or_else(|| anyhow::anyhow!("此笔记没有成品轨"))?;
        // 时间轴 → 轨内时刻:轨 0 点对应时间轴 offset_ms(成品轨通常 0,续录不为 0)。
        let clip = match range {
            Some((s, e)) => {
                if e <= s {
                    anyhow::bail!("圈定范围为空");
                }
                let ls = s.saturating_sub(track.offset_ms);
                let le = e.saturating_sub(track.offset_ms);
                if le <= ls {
                    anyhow::bail!("圈定范围不与成品轨重叠");
                }
                Some((ls, le))
            }
            None => None,
        };
        let file_name = dest
            .file_name()
            .ok_or_else(|| anyhow::anyhow!("导出路径缺少文件名"))?;
        let tmp = dest.with_file_name(format!(
            ".{}.tmp-{}",
            file_name.to_string_lossy(),
            std::process::id()
        ));
        let produced = match clip {
            None => std::fs::copy(&track.path, &tmp).map(|_| ()).map_err(anyhow::Error::from),
            Some((ls, le)) => clip_track_to(std::path::Path::new(&track.path), &tmp, ls, le),
        };
        if let Err(e) = produced {
            let _ = std::fs::remove_file(&tmp);
            return Err(e);
        }
        if let Err(e) = std::fs::rename(&tmp, dest) {
            let _ = std::fs::remove_file(&tmp);
            return Err(e.into());
        }
        Ok(())
    }

    /// 渲染导出内容字符串(不落盘)。MCP get_note 与 export 共用同一渲染,防两处漂移。
    pub fn render(&self, id: &str, format: &str) -> anyhow::Result<String> {
        let note = self.load(id)?;
        render_note(&note, format)
    }

    /// 渲染一个已经在内存里的 `Note`,跳过磁盘 load。给 MCP get_note 用——它
    /// 自己已经 load 过同一笔记,再调 `render(id, ..)` 会对同一笔记二次磁盘读取。
    /// export 模块本身是私有 `mod export;`,外部拿不到 `render_note`,所以在
    /// NoteStore 上开一个转发方法。
    pub fn render_loaded(&self, note: &Note, format: &str) -> anyhow::Result<String> {
        render_note(note, format)
    }
}

/// 渲染逻辑本体,供 `render`(先 load 再渲染)与 `render_loaded`(已有 Note,直接渲染)共用。
pub(crate) fn render_note(note: &Note, format: &str) -> anyhow::Result<String> {
    Ok(match format {
        "md" => render_markdown(note),
        "txt" => render_text(note),
        _ => anyhow::bail!("未知导出格式: {format}"),
    })
}

/// 把成品轨轨内 [start_ms, end_ms) 裁出写到 tmp。WAV 直接按字节裁(录制格式
/// 16k 单声道 s16,毫秒→字节走 audio.rs 的同一换算);m4a 借系统 afconvert 解回
/// 同格式 WAV 再裁、再重编码(afconvert 是 macOS 内建,而 m4a 成品轨本就只在
/// macOS 由它产出,非 macOS 走不到这个分支)。中间产物与 tmp 同目录,收尾必删。
fn clip_track_to(
    src: &std::path::Path,
    tmp: &std::path::Path,
    start_ms: u64,
    end_ms: u64,
) -> anyhow::Result<()> {
    let ext = src.extension().and_then(|e| e.to_str()).unwrap_or("");
    if ext.eq_ignore_ascii_case("wav") {
        return clip_wav(src, tmp, start_ms, end_ms);
    }
    let wav_full = tmp.with_extension("clipsrc.wav");
    let wav_cut = tmp.with_extension("clipcut.wav");
    let result = (|| {
        super::transcode::afconvert_decode(src, &wav_full)?;
        clip_wav(&wav_full, &wav_cut, start_ms, end_ms)?;
        super::transcode::afconvert_encode(&wav_cut, tmp)
    })();
    let _ = std::fs::remove_file(&wav_full);
    let _ = std::fs::remove_file(&wav_cut);
    result
}

/// 录制口径 WAV(16k 单声道 s16)按毫秒裁剪:逐块定位 `data` 块(不可假设 44 字节
/// 标准头——afconvert 解出的 WAV 带 40 字节 fmt 块与 FLLR 填充块,按固定偏移裁会把
/// 元数据当音频,Codex 审出),块内按字节切片、重写标准头,零重编码、流式拷贝。
/// 起点越界报错;终点越界钳到数据末尾(游标停在音频略短于时间轴的尾巴之外属正常)。
fn clip_wav(src: &std::path::Path, dest: &std::path::Path, start_ms: u64, end_ms: u64) -> anyhow::Result<()> {
    use super::audio::{ms_to_bytes, wav_header};
    use std::io::{Read, Seek, SeekFrom, Write};
    let mut f = std::fs::File::open(src)?;
    let mut head = [0u8; 12];
    f.read_exact(&mut head)?;
    if &head[0..4] != b"RIFF" || &head[8..12] != b"WAVE" {
        anyhow::bail!("非 WAV 数据: {}", src.display());
    }
    let file_len = f.metadata()?.len();
    // 块遍历与 transcode::read_wav_f32_slice 同款口径:size 以文件长度为上限,
    // 块尾按 RIFF 规则补齐到偶数,溢出即报错(坏块不死循环)。
    let mut pos = 12u64;
    let (data_start, data_len) = loop {
        if pos + 8 > file_len {
            anyhow::bail!("WAV 无 data 块: {}", src.display());
        }
        f.seek(SeekFrom::Start(pos))?;
        let mut ch = [0u8; 8];
        f.read_exact(&mut ch)?;
        let size = u64::from(u32::from_le_bytes([ch[4], ch[5], ch[6], ch[7]]));
        let start = pos + 8;
        if &ch[0..4] == b"data" {
            break (start, size.min(file_len.saturating_sub(start)));
        }
        pos = start.saturating_add(size).saturating_add(size & 1);
    };
    let from = ms_to_bytes(start_ms);
    let to = ms_to_bytes(end_ms).min(data_len);
    if from >= data_len || to <= from {
        anyhow::bail!("圈定范围在音频末尾之外");
    }
    let n = to - from;
    f.seek(SeekFrom::Start(data_start + from))?;
    let mut out = std::io::BufWriter::new(std::fs::File::create(dest)?);
    out.write_all(&wav_header(u32::try_from(n)?))?;
    let mut remaining = n;
    let mut buf = [0u8; 64 * 1024];
    while remaining > 0 {
        let take = remaining.min(buf.len() as u64) as usize;
        let read = f.read(&mut buf[..take])?;
        if read == 0 {
            anyhow::bail!("音频比头部标称的短(读到 EOF)");
        }
        out.write_all(&buf[..read])?;
        remaining -= read as u64;
    }
    out.flush()?;
    Ok(())
}

/// 修订稿的 md/txt 渲染(原始稿渲染在下方 render_note,Aing 段形状不同单独渲染;
/// GUI 导出与 MCP get_note 共用本函数,防两处漂移)。
/// 段落标签兜底与前端 speakerLabel 同序:名字 > 关联人物全局编号 > R 簇号。
pub fn render_refined(title: &str, doc: &RefinedDoc, md: bool) -> String {
    let mut out = String::new();
    if md {
        out.push_str(&format!("# {title}\n\n"));
    } else {
        out.push_str(&format!("{title}\n\n"));
    }
    for p in &doc.paragraphs {
        // 用户在笔记页插入的自由 markdown 块(空 speaker、无关联人物):只出正文。
        let speakerless =
            p.speaker.is_empty() && p.name.as_deref().unwrap_or("").is_empty() && p.person_id.is_none();
        if speakerless {
            out.push_str(&format!("{}\n\n", p.text));
            continue;
        }
        let label = p
            .name
            .clone()
            .filter(|n| !n.is_empty())
            .or_else(|| {
                p.person_id
                    .as_ref()
                    .map(|pid| format!("说话人 {}", pid.trim_start_matches('P')))
            })
            .unwrap_or_else(|| {
                // 一波说话人后段落是 S 键(与 label() 的段口径一致);旧文档残留 R 键
                // 同样按「说话人 N」兜底。
                let n = p.speaker.trim_start_matches(['R', 'S']);
                if !n.is_empty() && n.chars().all(|c| c.is_ascii_digit()) {
                    format!("说话人 {n}")
                } else {
                    p.speaker.clone()
                }
            });
        let ts = format_ts(p.start_ms);
        if md {
            out.push_str(&format!("**{label}** `[{ts}]`\n\n{}\n\n", p.text));
        } else {
            out.push_str(&format!("{label} [{ts}]\n{}\n\n", p.text));
        }
    }
    out
}

/// 毫秒 → hh:mm:ss。
pub fn format_ts(ms: u64) -> String {
    let s = ms / 1000;
    format!("{:02}:{:02}:{:02}", s / 3600, (s % 3600) / 60, s % 60)
}

/// 秒 → 人读时长："1 小时 8 分" / "12 分 3 秒" / "45 秒"。
pub(super) fn human_duration(secs: u64) -> String {
    let (h, m, s) = (secs / 3600, (secs % 3600) / 60, secs % 60);
    if h > 0 {
        format!("{h} 小时 {m} 分")
    } else if m > 0 {
        format!("{m} 分 {s} 秒")
    } else {
        format!("{s} 秒")
    }
}

/// 段落标签：有说话人 id 且 speakers 表里有非空名字 → 用名字；
/// 有 id 但表里无名（或名为空）→ 「说话人 N」（N 取 id 去掉前导 'S'）；
/// 无 id（未跑声纹/降级）→ 按来源 我/对方。
fn label<'a>(seg: &'a SegmentRecord, speakers: &'a BTreeMap<String, SpeakerMeta>) -> String {
    match &seg.speaker {
        Some(id) => {
            if let Some(name) = speakers.get(id).map(|m| &m.name).filter(|n| !n.is_empty()) {
                name.clone()
            } else {
                format!("说话人 {}", id.trim_start_matches('S'))
            }
        }
        None if seg.source == "mic" => "我".to_string(),
        None => "对方".to_string(),
    }
}

/// 头部第二行："2026-07-03 15:04 – 16:12(1 小时 8 分)"；中断会议结束时间标「中断」。
fn header_line(note: &Note) -> Option<String> {
    let start = chrono::DateTime::parse_from_rfc3339(&note.meta.started_at).ok()?;
    let start_str = start.format("%Y-%m-%d %H:%M").to_string();
    match note
        .meta
        .ended_at
        .as_deref()
        .and_then(|e| chrono::DateTime::parse_from_rfc3339(e).ok())
    {
        Some(end) => {
            let dur = human_duration((end - start).num_seconds().max(0) as u64);
            Some(format!("{start_str} – {}({dur})", end.format("%H:%M")))
        }
        None => Some(format!("{start_str} – 中断")),
    }
}

pub(super) fn render_markdown(note: &Note) -> String {
    let mut out = format!("# {}\n\n", note.meta.title);
    if let Some(h) = header_line(note) {
        out.push_str(&h);
        out.push_str("\n\n");
    }
    for seg in &note.segments {
        out.push_str(&format!(
            "**[{}] {}** {}\n\n",
            label(seg, &note.speakers),
            format_ts(seg.start_ms),
            seg.text
        ));
    }
    out
}

pub(super) fn render_text(note: &Note) -> String {
    let mut out = format!("{}\n\n", note.meta.title);
    if let Some(h) = header_line(note) {
        out.push_str(&h);
        out.push_str("\n\n");
    }
    for seg in &note.segments {
        out.push_str(&format!(
            "[{}] {} {}\n",
            label(seg, &note.speakers),
            format_ts(seg.start_ms),
            seg.text
        ));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::writer::NoteWriter;
    use crate::store::NoteStore;
    use serde_json;

    #[test]
    fn format_ts_is_hhmmss() {
        assert_eq!(format_ts(0), "00:00:00");
        assert_eq!(format_ts(83_000), "00:01:23");
        assert_eq!(format_ts(4_083_000), "01:08:03");
    }

    #[test]
    fn human_duration_formats() {
        assert_eq!(human_duration(4080), "1 小时 8 分");
        assert_eq!(human_duration(723), "12 分 3 秒");
        assert_eq!(human_duration(45), "45 秒");
    }

    #[test]
    fn export_md_and_txt() {
        let tmp = tempfile::tempdir().unwrap();
        let out = tempfile::tempdir().unwrap();
        let mut w = NoteWriter::create(tmp.path(), chrono::Local::now()).unwrap();
        let id = w.note_id().to_string();
        w.append_final("mic", "今天开会讨论项目进度。", 83_000, 86_000, None, None)
            .unwrap();
        w.append_final(
            "system",
            "好的，先看上周的问题。",
            91_000,
            94_000,
            None,
            None,
        )
        .unwrap();
        w.finalize(chrono::Local::now()).unwrap();

        let store = NoteStore::new(tmp.path().to_path_buf());
        let md_path = out.path().join("out.md");
        store.export_to(&id, "md", None, &md_path, None).unwrap();
        let md = std::fs::read_to_string(&md_path).unwrap();
        let title = store.load(&id).unwrap().meta.title;
        assert!(md.starts_with(&format!("# {title}\n")), "首行为标题: {md}");
        assert!(
            md.contains("**[我] 00:01:23** 今天开会讨论项目进度。"),
            "{md}"
        );
        assert!(
            md.contains("**[对方] 00:01:31** 好的，先看上周的问题。"),
            "{md}"
        );

        let txt_path = out.path().join("out.txt");
        store.export_to(&id, "txt", None, &txt_path, None).unwrap();
        let txt = std::fs::read_to_string(&txt_path).unwrap();
        assert!(
            txt.contains("[我] 00:01:23 今天开会讨论项目进度。"),
            "{txt}"
        );
        assert!(!txt.contains("**"), "纯文本无 markdown 记号");

        assert!(
            store
                .export_to(&id, "pdf", None, &out.path().join("out.pdf"), None)
                .is_err(),
            "未知格式报错"
        );
    }

    /// 圈定范围只导重叠段:整段保留不切句;两头都不沾的段被滤掉;空范围报错。
    #[test]
    fn export_range_keeps_overlapping_segments_only() {
        let tmp = tempfile::tempdir().unwrap();
        let out = tempfile::tempdir().unwrap();
        let mut w = NoteWriter::create(tmp.path(), chrono::Local::now()).unwrap();
        let id = w.note_id().to_string();
        w.append_final("mic", "第一段。", 83_000, 86_000, None, None).unwrap();
        w.append_final("system", "第二段。", 91_000, 94_000, None, None).unwrap();
        w.finalize(chrono::Local::now()).unwrap();
        let store = NoteStore::new(tmp.path().to_path_buf());

        // 范围只罩住第二段
        let dest = out.path().join("late.md");
        store.export_to(&id, "md", None, &dest, Some((90_000, 95_000))).unwrap();
        let md = std::fs::read_to_string(&dest).unwrap();
        assert!(!md.contains("第一段。"), "{md}");
        assert!(md.contains("第二段。"), "{md}");

        // 范围压着第一段尾巴(85s):跨界段整段保留
        let dest2 = out.path().join("both.md");
        store.export_to(&id, "md", None, &dest2, Some((85_000, 95_000))).unwrap();
        let md2 = std::fs::read_to_string(&dest2).unwrap();
        assert!(md2.contains("第一段。") && md2.contains("第二段。"), "{md2}");

        // 空范围报错
        assert!(store.export_to(&id, "md", None, &out.path().join("z.md"), Some((5_000, 5_000))).is_err());
    }

    /// 修订稿同样按重叠过滤段落。
    #[test]
    fn export_range_filters_refined_paragraphs() {
        use crate::store::{RefineStages, RefinedDoc, RefinedParagraph};
        let tmp = tempfile::tempdir().unwrap();
        let out = tempfile::tempdir().unwrap();
        let mut w = NoteWriter::create(tmp.path(), chrono::Local::now()).unwrap();
        let id = w.note_id().to_string();
        w.append_final("mic", "占位。", 0, 1_000, None, None).unwrap();
        w.finalize(chrono::Local::now()).unwrap();
        let store = NoteStore::new(tmp.path().to_path_buf());
        let para = |start: u64, end: u64, text: &str| RefinedParagraph {
            speaker: "S1".into(),
            name: Some("甲".into()),
            person_id: None,
            start_ms: start,
            end_ms: end,
            text: text.into(),
            source_seqs: vec![],
            mentions: vec![],
        };
        let doc = RefinedDoc {
            llm_failed_paragraphs: Vec::new(),
            schema_version: 1,
            generated_at: String::new(),
            written_at: String::new(),
            writer_pid: 0,
            finished_at: String::new(),
            writer_run: String::new(),
            llm_model: None,
            stages: RefineStages {
                filter: "done".into(),
                recluster: "done".into(),
                llm: "done".into(),
                entities: "off".into(),
                relations: "off".into(),
            },
            discarded_seqs: vec![],
            entities: vec![],
            graph_extraction: None,
            relations: vec![],
            graph_support_mentions: vec![],
            revision: 0,
            stale: false,
            paragraphs: vec![para(0, 10_000, "开场。"), para(60_000, 70_000, "中段。"), para(120_000, 130_000, "收尾。")],
        };
        let dest = out.path().join("mid.md");
        store.export_to(&id, "md", Some(&doc), &dest, Some((50_000, 80_000))).unwrap();
        let md = std::fs::read_to_string(&dest).unwrap();
        assert!(md.contains("中段。"), "{md}");
        assert!(!md.contains("开场。") && !md.contains("收尾。"), "{md}");
    }

    #[test]
    fn export_refined_renders_paragraphs_with_label_fallbacks() {
        use crate::store::{RefineStages, RefinedDoc, RefinedParagraph};
        let tmp = tempfile::tempdir().unwrap();
        let mut w = NoteWriter::create(tmp.path(), chrono::Local::now()).unwrap();
        let id = w.note_id().to_string();
        w.append_final("mic", "原始句。", 0, 2000, Some("S1"), None)
            .unwrap();
        w.finalize(chrono::Local::now()).unwrap();

        let para = |speaker: &str, name: Option<&str>, person: Option<&str>, text: &str| {
            RefinedParagraph {
                speaker: speaker.into(),
                name: name.map(str::to_string),
                person_id: person.map(str::to_string),
                start_ms: 0,
                end_ms: 2000,
                text: text.into(),
                source_seqs: vec![0],
                mentions: vec![],
            }
        };
        let doc = RefinedDoc {
            llm_failed_paragraphs: Vec::new(),
            schema_version: 1,
            generated_at: "t".into(),
            written_at: String::new(),
            writer_pid: 0,
            finished_at: String::new(),
            writer_run: String::new(),
            llm_model: None,
            stages: RefineStages {
                filter: "done".into(),
                recluster: "done".into(),
                llm: "done".into(),
                entities: "off".into(),
                relations: "off".into(),
            },
            discarded_seqs: vec![],
            entities: vec![],
            graph_extraction: None,
            relations: vec![],
            graph_support_mentions: vec![],
            revision: 0,
            stale: false,
            paragraphs: vec![
                para("R1", Some("张三"), Some("P1"), "有名字用名字。"),
                para("R2", None, Some("P4"), "无名有关联用全局编号。"),
                para("R3", None, None, "全无按 R 簇号兜底。"),
            ],
        };
        let store = NoteStore::new(tmp.path().to_path_buf());
        let out = tempfile::tempdir().unwrap();
        let dest = out.path().join("refined.md");
        store.export_to(&id, "md", Some(&doc), &dest, None).unwrap();
        let md = std::fs::read_to_string(&dest).unwrap();
        assert!(md.contains("**张三** `[00:00:00]`"), "{md}");
        assert!(md.contains("**说话人 4**"), "关联人物按 P 号: {md}");
        assert!(md.contains("**说话人 3**"), "未关联按 R 号: {md}");
        assert!(md.contains("有名字用名字。"), "{md}");
        assert!(!md.contains("原始句。"), "Aing 导出不含原始段: {md}");
        // 同一目标再按原始稿导出:原子替换覆盖为原始内容(所见即所得,后导为准)。
        store.export_to(&id, "md", None, &dest, None).unwrap();
        let md2 = std::fs::read_to_string(&dest).unwrap();
        assert!(md2.contains("原始句。"), "{md2}");
    }

    #[test]
    fn export_uses_speaker_name_when_present() {
        let mut speakers = std::collections::BTreeMap::new();
        speakers.insert(
            "S1".to_string(),
            crate::store::SpeakerMeta {
                name: "张三".into(),
                sources: vec![],
                centroid: None,
                count: 0,
                person_id: None, multi_speaker: false, reserved_by: None, split_born: false, hint_person: None,
            },
        );
        let note = crate::store::Note {
            meta: crate::store::NoteMeta {
                schema_version: 1,
                id: "x".into(),
                title: "t".into(),
                started_at: String::new(),
                ended_at: None,
                state: "complete".into(),
                calendar: None,
                calendar_cleared: false,
            asr_engine: None,
            },
            segments: vec![
                crate::store::SegmentRecord {
                    seq: 0,
                    source: "mic".into(),
                    text: "hi".into(),
                    start_ms: 0,
                    end_ms: 1000,
                    speaker: Some("S1".into()),
                    rms: None,
                },
                crate::store::SegmentRecord {
                    seq: 1,
                    source: "system".into(),
                    text: "yo".into(),
                    start_ms: 1000,
                    end_ms: 2000,
                    speaker: Some("S2".into()), // 表中无此 id
                    rms: None,
                },
                crate::store::SegmentRecord {
                    seq: 2,
                    source: "mic".into(),
                    text: "plain".into(),
                    start_ms: 2000,
                    end_ms: 3000,
                    speaker: None,
                    rms: None,
                },
            ],
            suppressed_segments: vec![],
            skipped_lines: 0,
            speakers,
        };
        let md = render_markdown(&note);
        assert!(md.contains("**[张三] 00:00:00** hi"), "{md}");
        assert!(
            md.contains("**[说话人 2] 00:00:01** yo"),
            "无名兜底为「说话人 N」: {md}"
        );
        assert!(
            md.contains("**[我] 00:00:02** plain"),
            "speaker null 仍走 我/对方: {md}"
        );
    }

    #[test]
    fn header_line_covers_normal_interrupted_and_corrupt() {
        // Test normal case: both started_at and ended_at are valid
        let note_normal = crate::store::Note {
            meta: crate::store::NoteMeta {
                schema_version: 1,
                id: "x".into(),
                title: "t".into(),
                started_at: "2026-07-03T15:04:00+08:00".into(),
                ended_at: Some("2026-07-03T16:12:00+08:00".into()),
                state: "complete".into(),
                calendar: None,
                calendar_cleared: false,
            asr_engine: None,
            },
            segments: vec![],
            suppressed_segments: vec![],
            skipped_lines: 0,
            speakers: Default::default(),
        };
        let md_normal = render_markdown(&note_normal);
        assert!(
            md_normal.contains("2026-07-03 15:04 – 16:12(1 小时 8 分)"),
            "normal case should contain time range with half-width brackets: {md_normal}"
        );

        // Test interrupted case: ended_at is None
        let note_interrupted = crate::store::Note {
            meta: crate::store::NoteMeta {
                schema_version: 1,
                id: "x".into(),
                title: "t".into(),
                started_at: "2026-07-03T15:04:00+08:00".into(),
                ended_at: None,
                state: "complete".into(),
                calendar: None,
                calendar_cleared: false,
            asr_engine: None,
            },
            segments: vec![],
            suppressed_segments: vec![],
            skipped_lines: 0,
            speakers: Default::default(),
        };
        let md_interrupted = render_markdown(&note_interrupted);
        assert!(
            md_interrupted.contains("2026-07-03 15:04 – 中断"),
            "interrupted case should contain 中断: {md_interrupted}"
        );

        // Test corrupt case: started_at is empty
        let note_corrupt = crate::store::Note {
            meta: crate::store::NoteMeta {
                schema_version: 1,
                id: "x".into(),
                title: "t".into(),
                started_at: String::new(),
                ended_at: None,
                state: "complete".into(),
                calendar: None,
                calendar_cleared: false,
            asr_engine: None,
            },
            segments: vec![],
            suppressed_segments: vec![],
            skipped_lines: 0,
            speakers: Default::default(),
        };
        let md_corrupt = render_markdown(&note_corrupt);
        assert!(
            !md_corrupt.contains(" – "),
            "corrupt case should not contain ` – ` (header_line skipped): {md_corrupt}"
        );
        assert!(
            md_corrupt.contains("# t"),
            "corrupt case should still contain title: {md_corrupt}"
        );
    }

    #[test]
    fn export_inherits_display_order_and_blank_filter() {
        let tmp = tempfile::tempdir().unwrap();
        let mut w = NoteWriter::create(tmp.path(), chrono::Local::now()).unwrap();
        let id = w.note_id().to_string();
        w.append_final("mic", "后说的", 5000, 6000, None, None)
            .unwrap();
        w.append_final("system", "  ", 500, 900, None, None)
            .unwrap();
        w.append_final("mic", "先说的", 1000, 1500, None, None)
            .unwrap();
        w.finalize(chrono::Local::now()).unwrap();
        let store = NoteStore::new(tmp.path().to_path_buf());
        let out = tempfile::tempdir().unwrap();
        let dest = out.path().join("order.txt");
        store.export_to(&id, "txt", None, &dest, None).unwrap();
        let txt = std::fs::read_to_string(&dest).unwrap();
        let (i_first, i_later) = (txt.find("先说的").unwrap(), txt.find("后说的").unwrap());
        assert!(i_first < i_later, "导出按 start_ms 序而非落盘序: {txt}");
        assert_eq!(
            txt.lines().filter(|l| l.starts_with('[')).count(),
            2,
            "空白段被过滤,只剩两段: {txt}"
        );
    }

    #[test]
    fn render_refined_skips_prefix_for_speakerless_blocks() {
        let doc: RefinedDoc = serde_json::from_value(serde_json::json!({
            "schema_version": 2,
            "generated_at": "2026-07-30T00:00:00Z",
            "stages": { "filter": "done", "recluster": "done", "llm": "done" },
            "discarded_seqs": [],
            "paragraphs": [
                { "speaker": "", "start_ms": 0, "end_ms": 0, "text": "## 会议纪要", "source_seqs": [] },
                { "speaker": "R1", "start_ms": 0, "end_ms": 1000, "text": "正文", "source_seqs": [1] }
            ]
        }))
        .unwrap();
        let md = render_refined("标题", &doc, true);
        assert!(md.contains("## 会议纪要\n\n"), "无说话人块只出正文: {md}");
        assert!(!md.contains("****"), "不得出现空名加粗前缀: {md}");
        assert!(md.contains("**说话人 1** `[00:00:00]`"), "有说话人的段保持原格式: {md}");
        let txt = render_refined("标题", &doc, false);
        assert!(txt.contains("\n## 会议纪要\n"), "txt 同样跳过前缀: {txt}");
    }
}

#[cfg(test)]
mod export_to_tests {
    use crate::store::writer::NoteWriter;
    use crate::store::NoteStore;

    /// 建一条单段笔记,返回 (notes 根目录, 导出目录, store, id)。
    fn setup() -> (tempfile::TempDir, tempfile::TempDir, NoteStore, String) {
        let tmp = tempfile::tempdir().unwrap();
        let out = tempfile::tempdir().unwrap();
        let mut w = NoteWriter::create(tmp.path(), chrono::Local::now()).unwrap();
        let id = w.note_id().to_string();
        w.append_final("mic", "保存对话框导出。", 1_000, 2_000, None, None).unwrap();
        w.finalize(chrono::Local::now()).unwrap();
        let store = NoteStore::new(tmp.path().to_path_buf());
        (tmp, out, store, id)
    }

    /// 导出到用户选定路径(保存对话框流程):内容与 render 完全一致,写到任意 dest,
    /// 不再往笔记数据目录塞 transcript.md(那是旧"导出后开文件夹"流程的产物)。
    #[test]
    fn export_to_writes_rendered_content_at_dest() {
        let (_tmp, out, store, id) = setup();
        let dest = out.path().join("我的会议-20260729-1630.md");
        store.export_to(&id, "md", None, &dest, None).unwrap();
        let written = std::fs::read_to_string(&dest).unwrap();
        assert_eq!(written, store.render(&id, "md").unwrap(), "内容须与 render 同源");
        assert!(!store.note_dir(&id).unwrap().join("transcript.md").exists(),
            "export_to 不应在笔记目录残留 transcript.md");
        let leftovers: Vec<_> = std::fs::read_dir(out.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().contains(".tmp-"))
            .collect();
        assert!(leftovers.is_empty(), "原子写入不应残留临时文件");
    }

    /// 覆盖已有文件:成功后为完整新内容(临时文件 + rename,不经过截断态)。
    #[test]
    fn export_to_atomically_overwrites_existing_file() {
        let (_tmp, out, store, id) = setup();
        let dest = out.path().join("已有.md");
        std::fs::write(&dest, "用户原有的重要内容").unwrap();
        store.export_to(&id, "md", None, &dest, None).unwrap();
        assert_eq!(std::fs::read_to_string(&dest).unwrap(), store.render(&id, "md").unwrap());
    }

    #[test]
    fn export_to_rejects_unknown_format() {
        let (_tmp, out, store, id) = setup();
        assert!(store.export_to(&id, "pdf", None, &out.path().join("x.pdf"), None).is_err());
    }

    /// 相对路径解析随进程 CWD 漂移,UI 报告的路径会与实际落盘不符 → 直接拒绝。
    #[test]
    fn export_to_rejects_relative_dest() {
        let (_tmp, _out, store, id) = setup();
        let err = store
            .export_to(&id, "md", None, std::path::Path::new("相对.md"), None)
            .unwrap_err();
        assert!(err.to_string().contains("绝对路径"), "{err}");
    }

    /// 兜底守卫:目标落在笔记数据目录内会毁掉笔记本体(segments.jsonl/meta.json),拒绝。
    #[test]
    fn export_to_rejects_dest_inside_notes_dir() {
        let (tmp, _out, store, id) = setup();
        let inside = store.note_dir(&id).unwrap().join("meta.json");
        let err = store.export_to(&id, "md", None, &inside, None).unwrap_err();
        assert!(err.to_string().contains("笔记数据目录"), "{err}");
        let root_level = tmp.path().join("x.md");
        assert!(store.export_to(&id, "md", None, &root_level, None).is_err());
    }

    /// 父目录不存在不再代建(保存对话框保证父目录存在),ENOENT 直接报错。
    #[test]
    fn export_to_errors_on_missing_parent() {
        let (_tmp, out, store, id) = setup();
        let dest = out.path().join("不存在的目录/x.md");
        assert!(store.export_to(&id, "md", None, &dest, None).is_err());
    }
}

#[cfg(test)]
mod export_audio_to_tests {
    use crate::pipeline::recording_sink::MIXED_TRACK;
    use crate::store::audio::AudioTrackWriter;
    use crate::store::writer::NoteWriter;
    use crate::store::NoteStore;

    /// 建一条单段笔记,返回 (notes 根目录, 导出目录, store, id)——与 export_to_tests::setup
    /// 同款建档辅助。
    fn fixture_note() -> (tempfile::TempDir, tempfile::TempDir, NoteStore, String) {
        let tmp = tempfile::tempdir().unwrap();
        let out = tempfile::tempdir().unwrap();
        let mut w = NoteWriter::create(tmp.path(), chrono::Local::now()).unwrap();
        let id = w.note_id().to_string();
        w.append_final("mic", "音频导出。", 1_000, 2_000, None, None).unwrap();
        w.finalize(chrono::Local::now()).unwrap();
        let store = NoteStore::new(tmp.path().to_path_buf());
        (tmp, out, store, id)
    }

    /// 在 note_dir 下落一条真实可被 mixed_track 识别的成品轨(与 audio.rs
    /// mixed_track_returns_the_mixed_source_with_consistent_semantics 同款写法:
    /// 走 AudioTrackWriter 而非手写字节,保证 WAV 头合法、字节数超过 HEADER_LEN,
    /// mixed_track 才认得出这条轨)。
    fn write_mixed_track(note_dir: &std::path::Path) {
        let mut w = AudioTrackWriter::new(note_dir, MIXED_TRACK, 0);
        let _ = w.append(&vec![0.2f32; 8_000]); // 0.5s @ 16kHz，足够超过 44 字节头
        drop(w);
    }

    #[test]
    fn export_audio_to_copies_mixed_track_to_dest() {
        let (_tmp, out, store, id) = fixture_note();
        let note_dir = store.note_dir(&id).unwrap();
        write_mixed_track(&note_dir);
        let source_bytes = std::fs::read(note_dir.join("mixed.wav")).unwrap();
        let dest = out.path().join("a.wav");
        store.export_audio_to(&id, &dest, None).unwrap();
        assert_eq!(std::fs::read(&dest).unwrap(), source_bytes);
    }

    /// 圈定范围裁剪 WAV:字节精确——头重写为切片长,数据区与源文件同位切片逐字节相等;
    /// 终点越过音频末尾按钳到末尾处理(游标停在时间轴尾巴之外属正常)。
    #[test]
    fn export_audio_to_clips_wav_byte_exact_and_clamps_tail() {
        use crate::store::audio::ms_to_bytes;
        const HEADER: usize = 44;
        let (_tmp, out, store, id) = fixture_note();
        let note_dir = store.note_dir(&id).unwrap();
        write_mixed_track(&note_dir); // 0.5s @16k s16 = 16000 字节数据
        let source = std::fs::read(note_dir.join("mixed.wav")).unwrap();

        let dest = out.path().join("clip.wav");
        store.export_audio_to(&id, &dest, Some((100, 300))).unwrap();
        let clipped = std::fs::read(&dest).unwrap();
        let (from, to) = (ms_to_bytes(100) as usize, ms_to_bytes(300) as usize);
        assert_eq!(clipped.len(), HEADER + (to - from));
        assert_eq!(&clipped[HEADER..], &source[HEADER + from..HEADER + to], "数据区同位切片");
        let declared = u32::from_le_bytes(clipped[40..44].try_into().unwrap()) as usize;
        assert_eq!(declared, to - from, "头部 data 尺寸与切片一致");

        // 终点越界:钳到末尾而非报错
        let dest2 = out.path().join("clip-tail.wav");
        store.export_audio_to(&id, &dest2, Some((100, 999_000))).unwrap();
        let tail = std::fs::read(&dest2).unwrap();
        assert_eq!(tail.len(), HEADER + (source.len() - HEADER - from));

        // 起点已在音频末尾之外:报错且不留半成品
        assert!(store.export_audio_to(&id, &out.path().join("x.wav"), Some((600_000, 700_000))).is_err());
        assert!(!out.path().join("x.wav").exists());

        // 空范围:报错
        assert!(store.export_audio_to(&id, &out.path().join("y.wav"), Some((300, 300))).is_err());
    }

    /// afconvert 解出的 WAV 不是 44 字节标准头(40 字节 fmt 块 + FLLR 填充块):
    /// clip_wav 必须逐块定位 data,按固定偏移裁会把元数据当音频(Codex 审出)。
    #[test]
    fn clip_wav_handles_afconvert_style_headers() {
        use crate::store::audio::ms_to_bytes;
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("weird.wav");
        let pcm: Vec<u8> = (0u16..8000).flat_map(|i| (i as i16).to_le_bytes()).collect(); // 0.5s @16k s16
        let mut bytes = Vec::new();
        bytes.extend_from_slice(b"RIFF");
        bytes.extend_from_slice(&0u32.to_le_bytes()); // 总长占位:解析按块走
        bytes.extend_from_slice(b"WAVE");
        bytes.extend_from_slice(b"fmt ");
        bytes.extend_from_slice(&40u32.to_le_bytes());
        bytes.extend_from_slice(&[0u8; 40]);
        bytes.extend_from_slice(b"FLLR");
        bytes.extend_from_slice(&100u32.to_le_bytes());
        bytes.extend_from_slice(&[0u8; 100]);
        bytes.extend_from_slice(b"data");
        bytes.extend_from_slice(&(pcm.len() as u32).to_le_bytes());
        bytes.extend_from_slice(&pcm);
        std::fs::write(&src, &bytes).unwrap();

        let dest = dir.path().join("cut.wav");
        super::clip_wav(&src, &dest, 100, 300).unwrap();
        let cut = std::fs::read(&dest).unwrap();
        let (from, to) = (ms_to_bytes(100) as usize, ms_to_bytes(300) as usize);
        assert_eq!(&cut[44..], &pcm[from..to], "data 从块内正确偏移取出");
        let declared = u32::from_le_bytes(cut[40..44].try_into().unwrap()) as usize;
        assert_eq!(declared, to - from);
    }

    #[test]
    fn export_audio_to_errors_when_no_mixed_track() {
        let (_tmp, out, store, id) = fixture_note();
        assert!(store.export_audio_to(&id, &out.path().join("a.wav"), None).is_err());
    }

    #[test]
    fn export_audio_to_rejects_dest_inside_notes_dir() {
        let (_tmp, _out, store, id) = fixture_note();
        let note_dir = store.note_dir(&id).unwrap();
        write_mixed_track(&note_dir);
        assert!(store.export_audio_to(&id, &note_dir.join("a.wav"), None).is_err());
    }
}
