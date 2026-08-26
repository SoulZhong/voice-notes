//! Aing 产物 refined.json:原始三文件之外的独立终稿,损坏/缺失时 UI 回落原始逐字稿。

use serde::{Deserialize, Serialize};
use std::fs::{File, OpenOptions};
use std::io::{BufRead, Read, Write};
use std::path::Path;
#[cfg(not(unix))]
use std::path::PathBuf;
#[cfg(not(unix))]
use std::sync::atomic::{AtomicU64, Ordering};

use super::notelock::NoteLock;

pub const REFINED_SCHEMA_VERSION: u32 = 2;

/// 每笔记修订稿产物文件名(人读真值)。
pub const AING_DOC_FILE: &str = "aing.json";
/// 旧文件名:一次性迁移到 `AING_DOC_FILE`,迁移后保留供回滚。
pub const LEGACY_REFINED_FILE: &str = "refined.json";

fn stage_off() -> String {
    "off".into()
}

/// 实体在段落正文中的一次提及(笔记页高亮 + 图谱建边用)。`start`/`end` 是本段
/// `text` 的字符(char)下标,半开区间 [start, end);`entity` 引用本篇
/// `RefinedDoc.entities[].id`。Plan 3 由大模型产出,本 plan 恒为空。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Mention {
    #[serde(default)]
    pub id: String,
    pub entity: String,
    pub start: usize,
    pub end: usize,
}

/// 本篇出现的一个实体(人读真值;全局知识图谱由所有 aing.json 派生、可整库重建)。
/// `id`:人实体复用全局 `person_id`(P<n>),非人实体为新分配 `ent_id`。
/// `kind`:person/org/project/term/decision/task/place/date… 用字符串免枚举迁移。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Entity {
    pub id: String,
    pub kind: String,
    pub name: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub aliases: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RefinedParagraph {
    pub speaker: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// 关联的全局声纹库人物 id(P<n>):重聚类种子命中时写入,或用户在说话人条
    /// 手动关联。有它才能把修订稿改名同步进声纹库(会议搭子)。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub person_id: Option<String>,
    pub start_ms: u64,
    pub end_ms: u64,
    pub text: String,
    pub source_seqs: Vec<u64>,
    /// 本段实体提及区间(Plan 3 填,本 plan 恒空)。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub mentions: Vec<Mention>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RefineStages {
    pub filter: String,
    pub recluster: String,
    pub llm: String,
    /// 实体抽取阶段:off/running/done/partial/failed(Plan 3 用,本 plan 恒 off)。
    #[serde(default = "stage_off")]
    pub entities: String,
    #[serde(default = "stage_off")]
    pub relations: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RefinedDoc {
    pub schema_version: u32,
    pub generated_at: String,
    /// 本份文档**落盘时刻**(每次整写在 write_refined_atomic_locked 内自动盖戳)。
    /// generated_at 是"开跑时刻",事后无法回答"这稿是几点写出的/哪一轮写的"——
    /// 2026-08-26 排障为此误判过一整晚(issue #173)。旧文件缺字段 serde default 兼容。
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub written_at: String,
    /// 写出本份文档的进程 pid(运行代次标识):重跑覆盖旧稿时,新旧稿归属哪一轮
    /// 从此可查。0 不落盘。
    #[serde(default, skip_serializing_if = "is_zero_u32")]
    pub writer_pid: u32,
    /// 整轮 worker 有序收工时刻(issue #173 十轮):llm 终态只说明 llm 阶段写过盘,
    /// identify/标题等尾段可能还在跑;此戳由 worker 终态上报前落盘。空 = 未收工。
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub finished_at: String,
    /// 本次写盘所属的运行标识 "pid-代次"(codex 三十轮):writer_pid 在同进程多轮
    /// 重跑间不变,无法把稿与 aing_runs.jsonl 里的某一轮对上;此值从心跳表取在跑
    /// worker 的代次,空 = 非 worker 写(编辑器保存/维护工具)。
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub writer_run: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub llm_model: Option<String>,
    pub stages: RefineStages,
    #[serde(default)]
    pub discarded_seqs: Vec<u64>,
    /// 本篇实体清单(Plan 3 填,本 plan 恒空)。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub entities: Vec<Entity>,
    #[serde(default)]
    pub graph_extraction: Option<super::aing_graph::GraphExtraction>,
    #[serde(default)]
    pub relations: Vec<super::aing_graph::RelationFact>,
    /// 仅供旧关系保持端点归属/证据拆分的 mention ids。它们仍存在于段落 mentions
    /// 以通过图谱结构校验，但不是当前正文的 live mentions，不得进入 UI/搜索索引。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub graph_support_mentions: Vec<String>,
    /// 用户编辑保存的乐观并发版本号:每次锁内编辑落盘 +1(见 update_refined),
    /// 管线整写永不回退(never-regress 后备在 write_refined_atomic_locked,所有
    /// writer 的收敛点)。历史文档缺省 0。
    #[serde(default)]
    pub revision: u64,
    /// LLM 精修失败块覆盖的段落下标(升序去重)。「只重试失败段落」的输入;整写
    /// (重新 Aing)重算,WYSIWYG 保存随 index_map 重映射(增删段时下标会漂——
    /// 2026-08-21 自查发现原设计假设有误)。旧文件无此键 = 无部分重跑入口。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub llm_failed_paragraphs: Vec<usize>,
    /// 段落已被重转写整体替换:本稿引用的 source_seqs/文本基于旧段,内容过期。
    /// UI 据此提示「重新 Aing」;下一次 run_local 整写新稿时自然回 false。
    #[serde(default)]
    pub stale: bool,
    pub paragraphs: Vec<RefinedParagraph>,
}

fn prepared_doc_bytes(note_id: &str, doc: &RefinedDoc) -> anyhow::Result<Vec<u8>> {
    let mut doc = doc.clone();
    crate::store::aing_graph::ensure_graph_ids(note_id, &mut doc);
    // 落盘戳单一咽喉(issue #173):write_refined_atomic_locked 与锚定 writer
    // (Agent 图谱写回/关系回填)全都在此序列化,谁写盘谁盖戳,不会漏路径。
    doc.written_at = chrono::Local::now().to_rfc3339();
    doc.writer_pid = std::process::id();
    doc.writer_run = crate::refine_beat_run_of(note_id).unwrap_or_default();
    Ok(serde_json::to_vec_pretty(&doc)?)
}

fn parse_doc(note_id: &str, bytes: &[u8]) -> anyhow::Result<RefinedDoc> {
    let mut doc: RefinedDoc = serde_json::from_slice(bytes)?;
    crate::store::aing_graph::ensure_graph_ids(note_id, &mut doc);
    Ok(doc)
}

/// 一旦构造成功，后续 Aing 真值/旧稿/锁/临时文件操作都绑定到同一个已打开的
/// 笔记目录。这样外部进程即使在检查后替换 `notes/<id>`，读写也不会重新解析父路径。
pub(crate) struct AnchoredRefinedDir {
    note_id: String,
    #[cfg(unix)]
    dir: File,
    #[cfg(windows)]
    _notes_root: File,
    #[cfg(windows)]
    dir: File,
    #[cfg(windows)]
    path: PathBuf,
    #[cfg(not(any(unix, windows)))]
    path: PathBuf,
}

impl AnchoredRefinedDir {
    pub(crate) fn open(notes_root: &Path, note_id: &str) -> anyhow::Result<Self> {
        crate::store::validate_note_id(note_id)?;

        #[cfg(unix)]
        {
            let notes = open_unix_directory(notes_root)?;
            let dir = open_unix_directory_at(&notes, note_id)?;
            return Ok(Self {
                note_id: note_id.into(),
                dir,
            });
        }

        #[cfg(windows)]
        {
            let notes = open_windows_directory(notes_root)?;
            let path = notes_root.join(note_id);
            let dir = open_windows_directory(&path)?;
            return Ok(Self {
                note_id: note_id.into(),
                _notes_root: notes,
                dir,
                path,
            });
        }

        #[cfg(not(any(unix, windows)))]
        {
            let root = std::fs::canonicalize(notes_root)?;
            let path = notes_root.join(note_id);
            let canonical = std::fs::canonicalize(&path)?;
            anyhow::ensure!(
                canonical.parent() == Some(root.as_path()),
                "笔记目录越出 notes 根"
            );
            return Ok(Self {
                note_id: note_id.into(),
                path,
            });
        }
    }

    pub(crate) fn acquire_lock(&self) -> std::io::Result<Option<NoteLock>> {
        NoteLock::acquire_opened(|| self.open_lock_file())
    }

    /// 只读当前 aing.json；缺失不回退旧稿，也不触发迁移。
    pub(crate) fn load_current(&self) -> anyhow::Result<Option<RefinedDoc>> {
        let Some(bytes) = self.read_optional(AING_DOC_FILE)? else {
            return Ok(None);
        };
        Ok(Some(parse_doc(&self.note_id, &bytes)?))
    }

    /// 只读当前 identify.json(refine::identify 的身份推断产物;文件名与
    /// identify::IDENTIFY_FILE 一致,store 不反向依赖 refine 故用字面量)。
    /// 缺失返回 None;与 load_current 同一锚定防护(mcp::tools 消费)。
    pub(crate) fn load_identify_value(&self) -> anyhow::Result<Option<serde_json::Value>> {
        let Some(bytes) = self.read_optional("identify.json")? else {
            return Ok(None);
        };
        Ok(Some(serde_json::from_slice(&bytes)?))
    }

    /// 已持锁加载当前真值；只有 aing.json 确实不存在才读旧稿并尝试迁移。
    pub(crate) fn load_locked(&self, lock: &NoteLock) -> anyhow::Result<Option<RefinedDoc>> {
        if let Some(bytes) = self.read_optional(AING_DOC_FILE)? {
            return Ok(Some(parse_doc(&self.note_id, &bytes)?));
        }
        let Some(bytes) = self.read_optional(LEGACY_REFINED_FILE)? else {
            return Ok(None);
        };
        let doc = parse_doc(&self.note_id, &bytes)?;
        // 与旧路径 loader 一致：迁移写失败不影响本次读取，旧文件永远保留。
        let _ = self.write_locked(&doc, lock);
        Ok(Some(doc))
    }

    pub(crate) fn write_locked(&self, doc: &RefinedDoc, _lock: &NoteLock) -> anyhow::Result<()> {
        let bytes = prepared_doc_bytes(&self.note_id, doc)?;
        self.write_bytes_atomic(&bytes)
    }

    #[cfg(unix)]
    fn read_optional(&self, name: &str) -> anyhow::Result<Option<Vec<u8>>> {
        let Some(mut file) = open_unix_child(&self.dir, name, libc::O_RDONLY, 0)? else {
            return Ok(None);
        };
        anyhow::ensure!(file.metadata()?.is_file(), "{name} 不是普通文件");
        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes)?;
        Ok(Some(bytes))
    }

    #[cfg(windows)]
    fn read_optional(&self, name: &str) -> anyhow::Result<Option<Vec<u8>>> {
        use std::os::windows::fs::{MetadataExt, OpenOptionsExt};
        use windows_sys::Win32::Storage::FileSystem::{
            FILE_ATTRIBUTE_REPARSE_POINT, FILE_FLAG_OPEN_REPARSE_POINT, FILE_SHARE_READ,
            FILE_SHARE_WRITE,
        };

        let path = self.path.join(name);
        let mut file = match OpenOptions::new()
            .read(true)
            .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE)
            .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
            .open(&path)
        {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error.into()),
        };
        let metadata = file.metadata()?;
        anyhow::ensure!(
            metadata.is_file() && metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT == 0,
            "{name} 不是无 reparse 的普通文件"
        );
        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes)?;
        Ok(Some(bytes))
    }

    #[cfg(not(any(unix, windows)))]
    fn read_optional(&self, name: &str) -> anyhow::Result<Option<Vec<u8>>> {
        let path = self.path.join(name);
        let metadata = match std::fs::symlink_metadata(&path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error.into()),
        };
        anyhow::ensure!(
            metadata.is_file() && !metadata.file_type().is_symlink(),
            "{name} 不是普通文件"
        );
        Ok(Some(std::fs::read(path)?))
    }

    #[cfg(unix)]
    fn open_lock_file(&self) -> std::io::Result<File> {
        open_unix_child(
            &self.dir,
            super::notelock::LOCK_FILE,
            libc::O_RDWR | libc::O_CREAT,
            0o600,
        )?
        .ok_or_else(|| std::io::Error::from(std::io::ErrorKind::NotFound))
        .and_then(|file| {
            if file.metadata()?.is_file() {
                Ok(file)
            } else {
                Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    ".note.lock 不是普通文件",
                ))
            }
        })
    }

    #[cfg(windows)]
    fn open_lock_file(&self) -> std::io::Result<File> {
        use std::os::windows::fs::{MetadataExt, OpenOptionsExt};
        use windows_sys::Win32::Storage::FileSystem::{
            FILE_ATTRIBUTE_REPARSE_POINT, FILE_FLAG_OPEN_REPARSE_POINT, FILE_SHARE_READ,
            FILE_SHARE_WRITE,
        };

        let file = OpenOptions::new()
            .create(true)
            .write(true)
            .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE)
            .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
            .open(self.path.join(super::notelock::LOCK_FILE))?;
        let metadata = file.metadata()?;
        if metadata.is_file() && metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT == 0 {
            Ok(file)
        } else {
            Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                ".note.lock 不是无 reparse 的普通文件",
            ))
        }
    }

    #[cfg(not(any(unix, windows)))]
    fn open_lock_file(&self) -> std::io::Result<File> {
        OpenOptions::new()
            .create(true)
            .write(true)
            .open(self.path.join(super::notelock::LOCK_FILE))
    }

    #[cfg(unix)]
    fn write_bytes_atomic(&self, bytes: &[u8]) -> anyhow::Result<()> {
        let temp_name = unique_temp_name()?;
        let mut temp = open_unix_child(
            &self.dir,
            &temp_name,
            libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL,
            0o600,
        )?
        .ok_or_else(|| anyhow::anyhow!("无法创建唯一 Aing 临时文件"))?;
        let result = (|| -> anyhow::Result<()> {
            temp.write_all(bytes)?;
            temp.sync_all()?;
            ensure_unix_entry_is_open_file(&self.dir, &temp_name, &temp)?;
            ensure_unix_optional_regular(&self.dir, AING_DOC_FILE)?;
            rename_unix_child(&self.dir, &temp_name, AING_DOC_FILE)?;
            self.dir.sync_all()?;
            Ok(())
        })();
        if result.is_err() {
            let _ = unlink_unix_child(&self.dir, &temp_name);
        }
        result
    }

    #[cfg(windows)]
    fn write_bytes_atomic(&self, bytes: &[u8]) -> anyhow::Result<()> {
        use std::os::windows::fs::{MetadataExt, OpenOptionsExt};
        use windows_sys::Win32::Foundation::GENERIC_WRITE;
        use windows_sys::Win32::Storage::FileSystem::{
            DELETE, FILE_ATTRIBUTE_REPARSE_POINT, FILE_FLAG_OPEN_REPARSE_POINT, FILE_SHARE_DELETE,
            FILE_SHARE_READ, FILE_SHARE_WRITE,
        };

        // Existing final symlinks/reparse points fail closed. The commit below is a path-based
        // MoveFileExW (the handle-relative rename was dropped for Windows share-mode reasons):
        // it replaces the destination directory entry without following it, but unlike the Unix
        // renameat path it re-resolves parent components, so a parent swapped for a junction
        // after this check is a knowingly accepted residual race in this single-user data dir.
        if let Some(metadata) = symlink_metadata_optional(&self.path.join(AING_DOC_FILE))? {
            anyhow::ensure!(
                metadata.is_file()
                    && metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT == 0,
                "aing.json 不是无 reparse 的普通文件"
            );
        }

        let (temp_name, mut temp) = loop {
            let name = unique_temp_name()?;
            match OpenOptions::new()
                .write(true)
                .create_new(true)
                .access_mode(GENERIC_WRITE | DELETE)
                .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
                .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
                .open(self.path.join(&name))
            {
                Ok(file) => break (name, file),
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(error) => return Err(error.into()),
            }
        };
        let result = (|| -> anyhow::Result<()> {
            temp.write_all(bytes)?;
            temp.sync_all()?;
            drop(temp);
            rename_windows_file(
                &self.path.join(&temp_name),
                &self.path.join(AING_DOC_FILE),
            )?;
            Ok(())
        })();
        if result.is_err() {
            let _ = std::fs::remove_file(self.path.join(temp_name));
        }
        result
    }

    #[cfg(not(any(unix, windows)))]
    fn write_bytes_atomic(&self, bytes: &[u8]) -> anyhow::Result<()> {
        let temp = self.path.join(unique_temp_name()?);
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temp)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        std::fs::rename(temp, self.path.join(AING_DOC_FILE))?;
        Ok(())
    }
}

#[cfg(not(unix))]
static AING_TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(0);

fn unique_temp_name() -> anyhow::Result<String> {
    #[cfg(unix)]
    {
        let mut random = [0u8; 16];
        File::open("/dev/urandom")?.read_exact(&mut random)?;
        let nonce = random
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>();
        return Ok(format!(".aing.json.tmp.{nonce}"));
    }

    #[cfg(not(unix))]
    {
        let sequence = AING_TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)?
            .as_nanos();
        Ok(format!(
            ".aing.json.tmp.{}.{}.{sequence}",
            std::process::id(),
            nanos
        ))
    }
}

#[cfg(unix)]
fn c_name(name: &str) -> std::io::Result<std::ffi::CString> {
    std::ffi::CString::new(name)
        .map_err(|_| std::io::Error::new(std::io::ErrorKind::InvalidInput, "文件名包含 NUL"))
}

#[cfg(unix)]
fn open_unix_directory(path: &Path) -> anyhow::Result<File> {
    use std::os::unix::fs::OpenOptionsExt;

    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_CLOEXEC | libc::O_DIRECTORY | libc::O_NOFOLLOW)
        .open(path)?;
    anyhow::ensure!(file.metadata()?.is_dir(), "不是目录: {}", path.display());
    Ok(file)
}

#[cfg(unix)]
fn open_unix_directory_at(parent: &File, name: &str) -> anyhow::Result<File> {
    use std::os::fd::{AsRawFd, FromRawFd};

    let name = c_name(name)?;
    let fd = unsafe {
        libc::openat(
            parent.as_raw_fd(),
            name.as_ptr(),
            libc::O_RDONLY | libc::O_CLOEXEC | libc::O_DIRECTORY | libc::O_NOFOLLOW,
        )
    };
    if fd < 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    let file = unsafe { File::from_raw_fd(fd) };
    anyhow::ensure!(file.metadata()?.is_dir(), "笔记节点不是目录");
    Ok(file)
}

#[cfg(unix)]
fn open_unix_child(
    dir: &File,
    name: &str,
    flags: libc::c_int,
    mode: libc::mode_t,
) -> std::io::Result<Option<File>> {
    use std::os::fd::{AsRawFd, FromRawFd};

    let name = c_name(name)?;
    let fd = unsafe {
        libc::openat(
            dir.as_raw_fd(),
            name.as_ptr(),
            flags | libc::O_CLOEXEC | libc::O_NOFOLLOW,
            mode as libc::c_uint,
        )
    };
    if fd < 0 {
        let error = std::io::Error::last_os_error();
        return if error.kind() == std::io::ErrorKind::NotFound {
            Ok(None)
        } else {
            Err(error)
        };
    }
    Ok(Some(unsafe { File::from_raw_fd(fd) }))
}

#[cfg(unix)]
fn ensure_unix_optional_regular(dir: &File, name: &str) -> anyhow::Result<()> {
    if let Some(file) = open_unix_child(dir, name, libc::O_RDONLY, 0)? {
        anyhow::ensure!(file.metadata()?.is_file(), "{name} 不是普通文件");
    }
    Ok(())
}

#[cfg(unix)]
fn ensure_unix_entry_is_open_file(dir: &File, name: &str, open: &File) -> anyhow::Result<()> {
    use std::os::unix::fs::MetadataExt;

    let current = open_unix_child(dir, name, libc::O_RDONLY, 0)?
        .ok_or_else(|| anyhow::anyhow!("Aing 临时文件在提交前消失"))?;
    let open_metadata = open.metadata()?;
    let current_metadata = current.metadata()?;
    anyhow::ensure!(
        current_metadata.is_file()
            && current_metadata.dev() == open_metadata.dev()
            && current_metadata.ino() == open_metadata.ino(),
        "Aing 临时文件在提交前被替换"
    );
    Ok(())
}

#[cfg(unix)]
fn rename_unix_child(dir: &File, from: &str, to: &str) -> std::io::Result<()> {
    use std::os::fd::AsRawFd;

    let from = c_name(from)?;
    let to = c_name(to)?;
    let result =
        unsafe { libc::renameat(dir.as_raw_fd(), from.as_ptr(), dir.as_raw_fd(), to.as_ptr()) };
    if result == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

#[cfg(unix)]
fn unlink_unix_child(dir: &File, name: &str) -> std::io::Result<()> {
    use std::os::fd::AsRawFd;

    let name = c_name(name)?;
    let result = unsafe { libc::unlinkat(dir.as_raw_fd(), name.as_ptr(), 0) };
    if result == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

#[cfg(windows)]
fn open_windows_directory(path: &Path) -> anyhow::Result<File> {
    use std::os::windows::fs::{MetadataExt, OpenOptionsExt};
    use windows_sys::Win32::Storage::FileSystem::{
        FILE_ATTRIBUTE_REPARSE_POINT, FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT,
        FILE_SHARE_READ, FILE_SHARE_WRITE,
    };

    let file = OpenOptions::new()
        .read(true)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)
        .open(path)?;
    let metadata = file.metadata()?;
    anyhow::ensure!(
        metadata.is_dir() && metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT == 0,
        "不是无 reparse 的真实目录: {}",
        path.display()
    );
    Ok(file)
}

#[cfg(windows)]
fn rename_windows_file(temp: &Path, target: &Path) -> anyhow::Result<()> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::{
        MoveFileExW, MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH,
    };

    let temp = temp
        .as_os_str()
        .encode_wide()
        .chain(Some(0))
        .collect::<Vec<_>>();
    let target = target
        .as_os_str()
        .encode_wide()
        .chain(Some(0))
        .collect::<Vec<_>>();
    let moved = unsafe {
        MoveFileExW(
            temp.as_ptr(),
            target.as_ptr(),
            MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
        )
    };
    if moved == 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    Ok(())
}

#[cfg(windows)]
fn symlink_metadata_optional(path: &Path) -> std::io::Result<Option<std::fs::Metadata>> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) => Ok(Some(metadata)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}

/// 已持有该笔记 `NoteLock` 时使用的底层原子写。把锁凭据作为参数，避免调用者
/// 意外绕开跨进程互斥，也避免 Aing 管线在锁内重入公共 writer。
///
/// 这是所有 aing.json writer 的收敛点，但只兜「拿旧内存态整写会让 revision 倒退」
/// 这一条底——内容整替换的 writer(乐观并发校验、精修管线)自己负责算出正确的
/// 目标 revision 并进位；这里只做单调性后备，严格大于才纠正:`existing.revision >
/// doc.revision` 时把待写副本的 revision 拉高到 `existing.revision + 1`。相等时
/// 原样透传，不额外进位——这是留给「载入-改-写回」型 writer(如迁移写、
/// mark_graph_failed 这类同锁内先读后写、revision 本就与盘面一致的调用点)的
/// 契约：它们的内存态与盘面在写入前后应保持一致，不能被这里的后备规则悄悄推高。
/// 若 aing.json 存在但已损坏(`load_aing_file` 返回 `None`),此处不进位——损坏时
/// 旧值本就不可读，无法判断该不该让步，保持当前 doc 的 revision 原样落盘是有意
/// 取舍。
fn is_zero_u32(v: &u32) -> bool {
    *v == 0
}

impl RefinedDoc {
    /// 停摆自愈用的最小失败稿(issue #173):worker 无声消失且盘上无稿时落一份
    /// llm=failed,让 UI 显示「失败可重跑」而不是「这场没做 AI 整理」的幻觉。
    /// 段落为空——展示端 load 缺段回落原始 segments,不影响正文。
    pub fn minimal_failed() -> Self {
        Self {
            schema_version: 1,
            generated_at: String::new(), // 调用方盖当前时刻
            written_at: String::new(),   // 落盘咽喉自动盖
            writer_pid: 0,
            finished_at: String::new(),
            writer_run: String::new(),
            llm_model: None,
            stages: RefineStages {
                filter: "off".into(),
                recluster: "off".into(),
                llm: "failed".into(),
                entities: "off".into(),
                relations: "off".into(),
            },
            discarded_seqs: Vec::new(),
            entities: Vec::new(),
            graph_extraction: None,
            relations: Vec::new(),
            graph_support_mentions: Vec::new(),
            revision: 0,
            paragraphs: Vec::new(),
            llm_failed_paragraphs: Vec::new(),
            stale: false,
        }
    }
}

pub(crate) fn write_refined_atomic_locked(
    note_dir: &Path,
    doc: &RefinedDoc,
    _lock: &NoteLock,
) -> anyhow::Result<()> {
    let mut doc = doc.clone();
    // 落盘戳已下沉到 prepared_doc_bytes(锚定 writer 同样经过那里),此处只管 revision。
    if let Some(Some(existing)) = load_aing_file(note_dir) {
        if existing.revision > doc.revision {
            doc.revision = existing.revision.saturating_add(1);
        }
    }
    let note_id = note_dir
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| anyhow::anyhow!("修订稿目录缺少有效笔记 id"))?;
    let tmp = note_dir.join("aing.json.tmp");
    std::fs::write(&tmp, prepared_doc_bytes(note_id, &doc)?)?;
    std::fs::rename(&tmp, note_dir.join(AING_DOC_FILE))?;
    Ok(())
}

/// 公共整份写入同样服从笔记级跨进程锁；所有 aing.json writer 因而共享一条
/// 串行化边界，固定的 `.tmp` 文件也不会被并发写者竞写。revision never-regress
/// 规则已下沉到 `write_refined_atomic_locked`，此处自动获得该保证。
pub fn write_refined_atomic(note_dir: &Path, doc: &RefinedDoc) -> anyhow::Result<()> {
    let lock = NoteLock::acquire(note_dir)?
        .ok_or_else(|| anyhow::anyhow!("笔记正在被另一进程修改，请稍后重试"))?;
    write_refined_atomic_locked(note_dir, doc, &lock)
}

fn ensure_ids(note_dir: &Path, mut doc: RefinedDoc) -> Option<RefinedDoc> {
    let note_id = note_dir.file_name()?.to_str()?;
    crate::store::aing_graph::ensure_graph_ids(note_id, &mut doc);
    Some(doc)
}

/// `Some(None)` 表示文件不存在；`None` 表示文件存在但读/解析失败。后者不能回退
/// 旧文件，否则损坏的新真值会被旧快照静默覆盖。
fn load_aing_file(note_dir: &Path) -> Option<Option<RefinedDoc>> {
    match std::fs::read(note_dir.join(AING_DOC_FILE)) {
        Ok(bytes) => {
            let doc: RefinedDoc = serde_json::from_slice(&bytes).ok()?;
            Some(Some(ensure_ids(note_dir, doc)?))
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Some(None),
        Err(_) => None,
    }
}

fn load_legacy_file(note_dir: &Path) -> Option<RefinedDoc> {
    let bytes = std::fs::read(note_dir.join(LEGACY_REFINED_FILE)).ok()?;
    let doc: RefinedDoc = serde_json::from_slice(&bytes).ok()?;
    ensure_ids(note_dir, doc)
}

/// 已持有 NoteLock 的加载路径。旧稿迁移也复用同一锁内 writer，避免重入。
pub(crate) fn load_refined_locked(note_dir: &Path, lock: &NoteLock) -> Option<RefinedDoc> {
    match load_aing_file(note_dir)? {
        Some(doc) => Some(doc),
        None => {
            let doc = load_legacy_file(note_dir)?;
            // 迁移落盘失败不致命(下次加载再试),旧文件不删。
            let _ = write_refined_atomic_locked(note_dir, &doc, lock);
            Some(doc)
        }
    }
}

/// 读修订稿:优先 `aing.json`;缺失时从旧 `refined.json` 一次性迁移(读旧格式→写
/// aing.json,旧文件保留供回滚)。两者皆无或损坏 → None(UI 回落原始逐字稿)。
pub fn load_refined(note_dir: &Path) -> Option<RefinedDoc> {
    match load_aing_file(note_dir)? {
        Some(doc) => Some(doc),
        None => match NoteLock::acquire(note_dir) {
            Ok(Some(lock)) => load_refined_locked(note_dir, &lock),
            // 另一 writer 正持锁时仍可读已有的完整原子快照；若它尚未产生
            // aing.json，则暂时返回旧稿但绝不在无锁状态迁移。
            _ => match load_aing_file(note_dir)? {
                Some(doc) => Some(doc),
                None => load_legacy_file(note_dir),
            },
        },
    }
}

/// 读修订稿并套上跨轨时基投影(见 `realign_paragraphs`)。**只给纯展示面用**:
/// 笔记页、导出这类"读出来给人看"的地方。
///
/// 为什么单开一个函数而不是把投影塞进 `load_refined`:投影后的时间戳一旦被写回磁盘,
/// 下次读取会再投影一次,每写一轮漂一次。而写路径遍布仓库(Agent Aing 失败时就有一处
/// `load_refined` → 改 stages → `write_refined_atomic`),靠"记得别写回"是守不住的。
/// 把默认值定成**未投影**,写路径拿到的天然是磁盘真值,只有明确要展示的两处才升级
/// 到本函数——错误方向从"默认危险"翻成"默认安全"。
pub fn load_refined_for_display(note_dir: &Path) -> Option<RefinedDoc> {
    load_refined(note_dir).map(|d| realign_paragraphs(note_dir, d))
}

/// 跨轨时基纠正:与 `NoteStore::load` 同一套(见 `store::align` 模块头)。修订段落
/// 的时间戳继承自它的源段,mic 轨漂移过就同样是错的,高亮/点击跳转都会偏。
///
/// 只映射「源段全是 mic」的段落:段落跨两轨时(同一说话人被两条链路各录一份的边界
/// 情形)映射哪一条都不对,宁可不动——它的时间戳本来就是两条时基混出来的,已不可修。
/// align.json 不存在时整条路径零开销(一次 read 失败),不拖慢图谱重建那种全库遍历。
///
/// **只改时间戳,绝不重排**。段落数组的下标就是保存契约:笔记页整篇保存按
/// `ParagraphPayload::orig_index` 指回基线数组,而基线取自 `load_refined_locked`
/// (不经本函数,是磁盘原序)。这里一重排,前端按新序发下来的下标就会落到磁盘上的
/// 另一段——编辑会写错段落。段那边可以排序是因为段编辑按 `seq` 定位,不是下标。
/// 修订稿的段落顺序本就是 Aing 按(当时错误的)时序分好的,要真正理顺得重跑 Aing。
fn realign_paragraphs(note_dir: &Path, mut doc: RefinedDoc) -> RefinedDoc {
    let Some(map) = crate::store::align::read(note_dir) else { return doc };
    let mic_seqs: std::collections::HashSet<u64> = match std::fs::File::open(note_dir.join("segments.jsonl")) {
        Ok(f) => std::io::BufReader::new(f)
            .lines()
            .map_while(Result::ok)
            .filter_map(|l| serde_json::from_str::<crate::store::SegmentRecord>(&l).ok())
            .filter(|s| s.source == "mic")
            .map(|s| s.seq)
            .collect(),
        Err(_) => return doc,
    };
    for p in doc.paragraphs.iter_mut() {
        if !p.source_seqs.is_empty() && p.source_seqs.iter().all(|q| mic_seqs.contains(q)) {
            p.start_ms = crate::player_align::map_ms(&map, p.start_ms);
            p.end_ms = crate::player_align::map_ms(&map, p.end_ms);
        }
    }
    doc
}

/// aing.json 或旧 refined.json 是否存在(供「是否有修订稿」判断,迁移感知)。
pub fn aing_exists(note_dir: &Path) -> bool {
    note_dir.join(AING_DOC_FILE).exists() || note_dir.join(LEGACY_REFINED_FILE).exists()
}

/// 同进程 read-modify-write 串行锁。跨进程边界始终是 NoteLock；两把锁同时需要
/// 时固定按 NoteLock → REFINED_EDIT_LOCK 获取，杜绝反序死锁。
static REFINED_EDIT_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// 锁内 read-modify-write 骨架:加载 → 就地修改 → 原子落盘。缺失/损坏 → Err
/// (编辑必须以「盘上有可编辑的修订稿」为前提,不能凭空造一份)。
/// 部分重试的写回入口:泛化 update(NoteLock + revision 递增)对 lib.rs 收窄暴露。
pub fn update_refined_for_retry(
    note_dir: &Path,
    f: impl FnOnce(&mut RefinedDoc) -> anyhow::Result<()>,
) -> anyhow::Result<()> {
    update_refined(note_dir, f)
}

fn update_refined(
    note_dir: &Path,
    f: impl FnOnce(&mut RefinedDoc) -> anyhow::Result<()>,
) -> anyhow::Result<()> {
    let note_lock = NoteLock::acquire(note_dir)?
        .ok_or_else(|| anyhow::anyhow!("笔记正在被另一进程修改，请稍后重试"))?;
    let _process_guard = REFINED_EDIT_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let mut doc = load_refined_locked(note_dir, &note_lock)
        .ok_or_else(|| anyhow::anyhow!("修订稿不存在或已损坏"))?;
    f(&mut doc)?;
    // 任何锁内编辑落盘都推进 revision:所有基于旧 revision 的未保存编辑器会话随之
    // 失效,防止笔记页保存悄悄盖掉改名/Agent 修订等其他 writer 的成果。
    doc.revision = doc.revision.saturating_add(1);
    write_refined_atomic_locked(note_dir, &doc, &note_lock)
}

/// 停摆自愈(issue #173,codex P1a/P1b):一把 NoteLock 内完成「查-判-写」,
/// 消灭"检查后 worker 诈尸写完稿、自愈再拿空失败稿盖掉"的窗口。
/// 返回动作描述供日志;拿不到锁(别的进程正在写=有人活着)让路即成功。
pub fn heal_stale_refined(
    note_dir: &Path,
    still_stale: impl Fn() -> bool,
) -> anyhow::Result<&'static str> {
    let Some(lock) = NoteLock::acquire(note_dir)? else {
        return Ok("另一进程持锁,让路");
    };
    match load_aing_file(note_dir) {
        // 文件在但读不出:是证据,不能拿失败稿盖掉
        None => Ok("盘上稿损坏,保留原样"),
        Some(Some(mut doc)) => {
            // 进门先复验接管(codex 十二轮):替补在首查之后才占槽时,盘上可能还是
            // 上一轮的终态稿——此时定性/广播都属于替补的剧本,这里不能抢戏。
            if !still_stale() {
                return Ok("新一轮已接手(占槽),让路");
            }
            if matches!(doc.stages.llm.as_str(), "done" | "failed" | "partial") {
                // llm 已终态 ≠ 整个 worker 收工(codex 九/十轮):看收工戳定性。
                // 稿子本身可用,两种情形都不动稿;定性字串供调用方选终态事件。
                return Ok(if doc.finished_at.is_empty() {
                    // identify/标题尾段吊死:worker 没有序退场过
                    "盘上稿 llm 已终态但收工戳缺失(尾段停摆),不动"
                } else {
                    "盘上稿已收工(收工戳在),不动"
                });
            }
            // 接管识别只看生死簿不看写盘戳(codex 八轮定稿):真替补从起跑到收工
            // 全程占着 lifecycle 槽,still_stale 必能探到;替补已收工则稿子是终态,
            // 上面的检查已放行。写盘戳比对反而会被普通编辑(WYSIWYG/改名,停摆
            // 标记一摘编辑立即放行)误触发,把该标失败的中间稿漏掉。
            // 提交时复验(codex 七轮):替补 worker 可能在前面查完之后才占上
            // lifecycle 槽、且还没写出自己的 aing.json——写盘戳守卫探不到它,
            // 落笔前再问一次生死簿。
            if !still_stale() {
                return Ok("新一轮已接手(占槽未写盘),让路");
            }
            // run_local 之后 llm 阶段停摆:中间稿改标 failed,UI 出「失败可重跑」
            doc.stages.llm = "failed".into();
            doc.revision = doc.revision.saturating_add(1);
            write_refined_atomic_locked(note_dir, &doc, &lock)?;
            Ok("中间稿已改标 llm=failed")
        }
        Some(None) => {
            // 盘上无 aing.json(run_local 前就停摆)。两个坑(codex 二轮):
            // ① 旧世界只有 refined.json 的笔记,写空稿会把旧稿整个挡在读取
            //   优先级后面——先认旧稿,改标 failed 迁移过来,正文原样保留;
            // ② 全新笔记落空段稿会让「修订视图/get_note(prefer_refined)」变
            //   白板——从 segments.jsonl 物化原始段当正文,失败横幅照出。
            if !still_stale() {
                return Ok("新一轮已接手(占槽未写盘),让路");
            }
            if let Some(mut doc) = load_legacy_file(note_dir) {
                doc.stages.llm = "failed".into();
                doc.revision = doc.revision.saturating_add(1);
                write_refined_atomic_locked(note_dir, &doc, &lock)?;
                return Ok("旧稿已迁移并改标 llm=failed");
            }
            let mut doc = RefinedDoc::minimal_failed();
            doc.generated_at = chrono::Local::now().to_rfc3339();
            // 正文取自规范加载器(codex 五轮):抑制侧车/空段剔除/稳定排序/align
            // 时基修正全套语义与原始稿视图一致,失败稿不另造一套口径。
            if let (Some(parent), Some(id)) =
                (note_dir.parent(), note_dir.file_name().and_then(|n| n.to_str()))
            {
                if let Ok(note) = crate::store::NoteStore::new(parent.to_path_buf()).load(id) {
                    // aing.json 只能存未投影时基(codex 六轮):展示端
                    // load_refined_for_display 会再按 align.json 投影一次,直接存
                    // 规范加载器给的已投影值等于二次投影。段序/抑制/空段语义照用
                    // 规范视图,时间戳按 seq 回查磁盘原始行。
                    let mut raw_ms = std::collections::HashMap::new();
                    if let Ok(raw) = std::fs::read_to_string(note_dir.join("segments.jsonl")) {
                        for line in raw.lines() {
                            if let Ok(r) =
                                serde_json::from_str::<crate::store::SegmentRecord>(line)
                            {
                                raw_ms.insert(r.seq, (r.start_ms, r.end_ms));
                            }
                        }
                    }
                    for seg in note.segments {
                        let (start_ms, end_ms) =
                            raw_ms.get(&seg.seq).copied().unwrap_or((seg.start_ms, seg.end_ms));
                        doc.paragraphs.push(RefinedParagraph {
                            speaker: seg.speaker.unwrap_or_default(),
                            name: None,
                            person_id: None,
                            text: seg.text,
                            start_ms,
                            end_ms,
                            source_seqs: vec![seg.seq],
                            mentions: Vec::new(),
                        });
                    }
                }
            }
            write_refined_atomic_locked(note_dir, &doc, &lock)?;
            Ok("已落 llm=failed 失败稿(正文取原始段)")
        }
    }
}

/// 修订稿说话人改名:该 speaker 的全部段落 name 置为新名。
/// 把修订稿整份标 stale(拆分同步失败的硬兜底:原始段已改派,修订稿再不标脏,
/// 用户在默认视图看到的就是旧归属还以为拆完了)。
pub fn mark_refined_stale(note_dir: &Path) -> anyhow::Result<()> {
    update_refined(note_dir, |doc| {
        doc.stale = true;
        Ok(())
    })
}

/// 拆分后的修订稿同步(一期边界,codex 设计轮三 P1⑤):
/// - 某段落的**全部**非空 source_seqs 都被改派到同一目标 → 原位只改
///   speaker/person/name,段界与文本一字不动
/// - 同一段落的源段被拆到多个目标 → 整份标 stale(横幅提示重新 Aing),不拆文本
///   (source_seqs 没有字符级映射,拆文本只能复制全文/瞎猜/丢字)
/// moved: seq → 新 speaker id;person_name: 新 speaker → (person_id, name)(仅 person 去向)。
/// 返回是否被标了 stale。
pub fn sync_refined_after_split(
    note_dir: &Path,
    moved: &std::collections::BTreeMap<u64, String>,
) -> anyhow::Result<bool> {
    let mut went_stale = false;
    update_refined(note_dir, |doc| {
        let mut stale = false;
        for p in doc.paragraphs.iter_mut() {
            let touched: Vec<&u64> =
                p.source_seqs.iter().filter(|q| moved.contains_key(q)).collect();
            if touched.is_empty() {
                continue;
            }
            let dests: std::collections::BTreeSet<&String> =
                touched.iter().map(|q| &moved[q]).collect();
            if touched.len() == p.source_seqs.len() && dests.len() == 1 {
                let dest = (*dests.iter().next().expect("len==1")).clone();
                // 一波说话人(2026-08-21):段落不携带身份,只改归属;显示端现查
                // note.speakers,人物随 speakers.json 自然更新。
                p.person_id = None;
                p.name = None;
                p.speaker = dest;
            } else {
                stale = true; // 跨组/部分改派:不可映射,整份作废重新 Aing
            }
        }
        if stale {
            doc.stale = true;
        }
        went_stale = stale;
        Ok(())
    })?;
    Ok(went_stale)
}

/// Agent Aing 写回:按段落下标批量替换 text,并把 stages.llm 置 "done"、记录 llm_model。
/// 约束式写入——只能改文本,说话人/时间戳/段落数一概不可动,这是把「外部 Agent 可写」
/// 的面收到最小的关键:哪怕 Agent 行为失常,最坏也只是文本变差,结构不会被破坏。
/// 任一下标越界或文本为空即整体拒绝(不落盘半份结果)。updates 为空是合法输入,
/// 语义为「已审阅,确无需要修订之处」——同样把 llm 置 done,否则干净稿会被误报失败。
pub fn apply_refined_texts(
    note_dir: &Path,
    updates: &[(usize, String)],
    llm_model: &str,
) -> anyhow::Result<usize> {
    update_refined(note_dir, |doc| {
        for (i, text) in updates {
            anyhow::ensure!(
                *i < doc.paragraphs.len(),
                "段落下标越界: {i}(共 {} 段)",
                doc.paragraphs.len()
            );
            anyhow::ensure!(!text.trim().is_empty(), "第 {i} 段修订文本为空");
        }
        for (i, text) in updates {
            doc.paragraphs[*i].text = text.clone();
        }
        doc.stages.llm = "done".into();
        doc.llm_model = Some(llm_model.to_string());
        Ok(())
    })?;
    Ok(updates.len())
}

/// save_refined 载荷段落:orig_index 指向保存基线 doc.paragraphs 的下标(None=用户
/// 新插入块);dirty=文本相对载入基线有变(mention 偏移随之失效)。
#[derive(Debug, Clone, serde::Deserialize)]
pub struct ParagraphPayload {
    pub orig_index: Option<usize>,
    pub text: String,
    pub dirty: bool,
}

/// 笔记页 WYSIWYG 整篇保存。与 apply_refined_texts(Agent 只改文本)不同,这里允许
/// 增删段与插入无说话人块,因此要自己维护图谱一致性:
/// - 干净段(dirty=false):整段原样保留,连 text 也不替换——载荷文本被忽略。
///   dirty=false 的语义是「相对载入基线没变」,而编辑器的 markdown 序列化会给正文
///   加转义符(`1. ` → `1\. `),据此替换会污染用户没编辑过的段并让 mention 偏移错位;
/// - 脏段:替换 text(speaker/时间戳/source_seqs 仍原样保留),但 mention 偏移失效
///   → mention id 移入 graph_support_mentions(mention 本体留在段上,图谱关系端点
///   不悬空;UI/搜索按 support 过滤);
/// - 被删段:mentions 随段消失 → 引用这些 mention 的关系整条剪掉;
/// - 证据:paragraph_index 按新布局重定位,落在被删/脏段上的证据丢弃(偏移无效);
/// - 新块:空 speaker + 零时间戳 + 空 source_seqs(导出侧对空 speaker 不加前缀)。
/// revision 乐观并发:不匹配即拒绝;成功后经 update_refined 统一 +1,返回新值。
pub fn save_refined_paragraphs(
    note_dir: &Path,
    expected_revision: u64,
    payload: &[ParagraphPayload],
) -> anyhow::Result<u64> {
    update_refined(note_dir, |doc| {
        // 前端 +page.svelte 的 doSaveRefined 冲突判别靠字符串匹配"已在别处更新"这个
        // 子串来触发重载分支,改这句文案要同步改那边的 String(err).includes(...)。
        anyhow::ensure!(
            doc.revision == expected_revision,
            "修订稿已在别处更新(盘上 revision {} ≠ 期望 {})",
            doc.revision,
            expected_revision
        );
        let old = std::mem::take(&mut doc.paragraphs);
        // old 下标 → (新下标, 是否脏);None = 该段被删
        let mut index_map: Vec<Option<(usize, bool)>> = vec![None; old.len()];
        let mut new_paras = Vec::with_capacity(payload.len());
        for (new_i, p) in payload.iter().enumerate() {
            anyhow::ensure!(!p.text.trim().is_empty(), "第 {new_i} 段文本为空");
            match p.orig_index {
                Some(i) => {
                    anyhow::ensure!(i < old.len(), "orig_index 越界: {i}(共 {} 段)", old.len());
                    anyhow::ensure!(index_map[i].is_none(), "orig_index 重复: {i}");
                    index_map[i] = Some((new_i, p.dirty));
                    let mut para = old[i].clone();
                    // dirty=false 语义即「相对载入基线没变」→ 一律保留盘上原文,忽略载荷
                    // 文本。编辑器把带 live mention 的干净段按字面载入、再经 commonmark
                    // 序列化回载荷,markdown 转义会静默改写正文(`1. 议题`→`1\. 议题`、
                    // `预算[初稿]`→`预算\[初稿]`);基线同样是转义结果所以判不出 dirty,
                    // 若在这里替换,用户从未碰过的段会被写进转义符,mentions 的
                    // start/end 字符偏移随之错位。脏段行为不变(偏移本就失效,见下方
                    // support 降级)。
                    if p.dirty {
                        para.text = p.text.clone();
                    }
                    new_paras.push(para);
                }
                None => new_paras.push(RefinedParagraph {
                    speaker: String::new(),
                    name: None,
                    person_id: None,
                    start_ms: 0,
                    end_ms: 0,
                    text: p.text.clone(),
                    source_seqs: Vec::new(),
                    mentions: Vec::new(),
                }),
            }
        }
        // 失败段下标随本次增删/重排一并重映射:不映射的话,部分重跑会把润色写到错的段
        // (被删的段直接移出列表)。与段落数组同一次写盘,原子。
        doc.llm_failed_paragraphs = {
            let mut v: Vec<usize> = doc
                .llm_failed_paragraphs
                .iter()
                .filter_map(|&i| index_map.get(i).copied().flatten().map(|(new_i, _)| new_i))
                .collect();
            v.sort_unstable();
            v.dedup();
            v
        };
        doc.paragraphs = new_paras;

        // 脏段 mention 降级为 support-only
        for slot in index_map.iter() {
            let Some((new_i, true)) = slot else { continue };
            for m in &doc.paragraphs[*new_i].mentions {
                if !m.id.is_empty() && !doc.graph_support_mentions.contains(&m.id) {
                    doc.graph_support_mentions.push(m.id.clone());
                }
            }
        }
        // 被删段的 mention 彻底消失:剪掉引用它们的关系与 support 残留
        let alive: std::collections::HashSet<&str> = doc
            .paragraphs
            .iter()
            .flat_map(|p| p.mentions.iter().map(|m| m.id.as_str()))
            .collect();
        doc.relations.retain(|r| {
            r.subject_mentions
                .iter()
                .chain(r.object_mentions.iter())
                .all(|id| alive.contains(id.as_str()))
        });
        doc.graph_support_mentions.retain(|id| alive.contains(id.as_str()));
        // 证据重定位:落在被删/脏段上的丢弃,其余 paragraph_index 重映射
        for rel in doc.relations.iter_mut() {
            rel.evidence.retain_mut(|ev| match index_map.get(ev.paragraph_index).copied().flatten() {
                Some((new_i, false)) => {
                    ev.paragraph_index = new_i;
                    true
                }
                _ => false,
            });
        }
        Ok(())
    })?;
    // expected_revision + 1 与 update_refined 锁内那次 +1 是同一个值:能走到这里说明
    // 上面的 ensure 已确认盘上 doc.revision == expected_revision,而 update_refined 在
    // 同一把锁内对同一个 doc 做 revision += 1 后才落盘,中途没有别的 writer 能插入。
    // (saturating 同样一致:u64::MAX 在两边都饱和到同一值。)
    Ok(expected_revision.saturating_add(1))
}

/// 只读 join:关联了库人物的段落,展示名跟随库中现名(会议搭子改名 → 历史修订稿
/// 跟着变),person_id 经 redirects 归一到 winner。只改返回值,不落盘——与
/// notes.rs join_person_names 同一哲学。库中无名/人已删除时保留段落原 name。
/// 一波说话人显示 join(2026-08-21-one-speaker-set-design.md §2/§7):
/// 段落身份一律现查 note.speakers——person 经 redirects 归一、名字跟库中现名,
/// speakers.json 本地名兜底;旧版 R 键文档按 source_seqs 的多数源段落说话人映射回
/// S(display 副本连 speaker 一并改写,前端胸牌/操作全走 S 域)。只影响返回值,
/// 不落盘。查不到映射的段落(用户手插块/空 source_seqs 的遗留段)保留原样。
pub fn join_note_identities(
    doc: &mut RefinedDoc,
    speakers: &std::collections::BTreeMap<String, super::SpeakerMeta>,
    segments: &[super::SegmentRecord],
    vp: &super::voiceprints::Voiceprints,
) {
    let seg_speaker: std::collections::BTreeMap<u64, &str> = segments
        .iter()
        .filter_map(|s| s.speaker.as_deref().map(|sp| (s.seq, sp)))
        .collect();
    for p in doc.paragraphs.iter_mut() {
        if !speakers.contains_key(&p.speaker) {
            // 旧 R 键(或已被删的 S):多数票映射。平票取 BTreeMap 序首者,稳定可复现。
            let mut counts: std::collections::BTreeMap<&str, usize> = Default::default();
            for q in &p.source_seqs {
                if let Some(sp) = seg_speaker.get(q) {
                    *counts.entry(sp).or_default() += 1;
                }
            }
            let Some(best) = counts.iter().max_by_key(|(_, n)| **n).map(|(sp, _)| sp.to_string())
            else {
                // 无源段可映射:保留遗留身份字段,但仍归一 redirects、跟库中现名
                // (旧 join_library_names 语义),不至于显示已合并人物的旧名。
                if let Some(rid) =
                    p.person_id.as_deref().and_then(|pid| super::VoiceprintStore::resolve(vp, pid))
                {
                    if let Some(person) = vp.people.get(rid) {
                        if !person.name.is_empty() {
                            p.name = Some(person.name.clone());
                        }
                    }
                    p.person_id = Some(rid.to_string());
                }
                continue;
            };
            p.speaker = best;
        }
        let Some(meta) = speakers.get(&p.speaker) else { continue };
        match meta.person_id.as_deref().and_then(|pid| super::VoiceprintStore::resolve(vp, pid)) {
            Some(rid) => {
                let lib_name = vp.people.get(rid).map(|per| per.name.clone()).unwrap_or_default();
                p.person_id = Some(rid.to_string());
                p.name = if !lib_name.is_empty() {
                    Some(lib_name)
                } else if !meta.name.is_empty() {
                    Some(meta.name.clone())
                } else {
                    None
                };
            }
            None => {
                p.person_id = None;
                p.name = if meta.name.is_empty() { None } else { Some(meta.name.clone()) };
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::{ensure_graph_ids, evidence_id};

    #[test]
    fn heal_stale_covers_missing_intermediate_and_finished_docs() {
        let dir = tempfile::tempdir().unwrap();
        let dir = dir.path().join("20260101-000000");
        std::fs::create_dir_all(&dir).unwrap();
        // ① 盘上无稿:落失败稿,正文从 segments.jsonl 物化(不落白板稿)
        std::fs::write(
            dir.join("segments.jsonl"),
            concat!(
                r#"{"seq":0,"source":"mic","text":"原文甲","start_ms":0,"end_ms":1000,"speaker":"S1"}"#,
                "
",
                r#"{"seq":1,"source":"mic","text":"回声段","start_ms":1000,"end_ms":2000,"speaker":"S1"}"#,
            ),
        )
        .unwrap();
        std::fs::write(dir.join("segment-suppressions.jsonl"), r#"{"seq":1,"reason":"echo_match"}"#)
            .unwrap();
        let act = heal_stale_refined(&dir, || true).unwrap();
        assert!(act.contains("原始段"), "{act}");
        let doc = load_refined(&dir).unwrap();
        assert_eq!(doc.stages.llm, "failed");
        assert_eq!(doc.paragraphs[0].text, "原文甲", "修订视图不是白板");
        assert_eq!(doc.paragraphs[0].speaker, "S1");
        assert_eq!(doc.paragraphs.len(), 1, "被抑制的回声段不得借失败稿还魂");
        assert!(!doc.written_at.is_empty(), "咽喉盖了写盘戳");
        // ② 中间稿(run_local 后 llm=off):改标 failed,revision 进位,正文保留
        let mut mid = RefinedDoc::minimal_failed();
        mid.stages.llm = "off".into();
        mid.paragraphs.push(RefinedParagraph {
            speaker: "S1".into(),
            name: None,
            person_id: None,
            text: "正文在".into(),
            start_ms: 0,
            end_ms: 1,
            source_seqs: vec![0],
            mentions: Vec::new(),
        });
        write_refined_atomic(&dir, &mid).unwrap();
        let rev0 = load_refined(&dir).unwrap().revision;
        // ②a 接管守卫:生死簿上有人(still_stale=false) ⇒ 替补在跑,让路
        let act = heal_stale_refined(&dir, || false).unwrap();
        assert!(act.contains("让路"), "{act}");
        assert_eq!(load_refined(&dir).unwrap().stages.llm, "off", "替补的稿不被扣帽");
        let act = heal_stale_refined(&dir, || true).unwrap();
        assert!(act.contains("改标"), "{act}");
        let doc = load_refined(&dir).unwrap();
        assert_eq!(doc.stages.llm, "failed");
        assert_eq!(doc.paragraphs[0].text, "正文在", "正文不丢");
        assert!(doc.revision > rev0, "进位挡住过期编辑器");
        // ③ 已收工的稿(llm=done):原样不动
        let mut done = load_refined(&dir).unwrap();
        done.stages.llm = "done".into();
        write_refined_atomic(&dir, &done).unwrap();
        let before = std::fs::read(dir.join(AING_DOC_FILE)).unwrap();
        let act = heal_stale_refined(&dir, || true).unwrap();
        assert!(act.contains("尾段停摆"), "llm 终态但无收工戳 ⇒ 尾段停摆: {act}");
        assert_eq!(std::fs::read(dir.join(AING_DOC_FILE)).unwrap(), before);
        // ③b 收工戳在:定性为已收工(调用方广播 done)
        let mut done2 = load_refined(&dir).unwrap();
        done2.finished_at = "2026-01-01T00:00:00+08:00".into();
        write_refined_atomic(&dir, &done2).unwrap();
        let act = heal_stale_refined(&dir, || true).unwrap();
        assert!(act.contains("已收工"), "{act}");
        // ④ 只有旧世界 refined.json 的笔记:迁移旧稿改标 failed,正文保留
        let legacy_dir = dir.parent().unwrap().join("20260101-000001");
        std::fs::create_dir_all(&legacy_dir).unwrap();
        let mut legacy = load_refined(&dir).unwrap();
        legacy.stages.llm = "done".into();
        legacy.paragraphs[0].text = "旧稿正文".into();
        std::fs::write(
            legacy_dir.join(LEGACY_REFINED_FILE),
            serde_json::to_vec(&legacy).unwrap(),
        )
        .unwrap();
        let act = heal_stale_refined(&legacy_dir, || true).unwrap();
        assert!(act.contains("迁移"), "{act}");
        let doc = load_refined(&legacy_dir).unwrap();
        assert_eq!(doc.stages.llm, "failed");
        assert_eq!(doc.paragraphs[0].text, "旧稿正文", "旧稿正文不被空稿挡住");
    }

    #[test]
    fn v2_writes_synthesize_stable_mention_ids_without_a_repair_read() {
        let dir = tempfile::tempdir().unwrap();
        let doc = RefinedDoc {
            llm_failed_paragraphs: Vec::new(),
            schema_version: REFINED_SCHEMA_VERSION,
            generated_at: "2026-07-21T00:00:00+08:00".into(),
            written_at: String::new(),
            writer_pid: 0,
            finished_at: String::new(),
            writer_run: String::new(),
            llm_model: None,
            stages: RefineStages { filter: "done".into(), recluster: "done".into(), llm: "done".into(), entities: "done".into(), relations: "off".into() },
            discarded_seqs: vec![],
            entities: vec![],
            graph_extraction: None,
            relations: vec![],
            graph_support_mentions: vec![],
            revision: 0,
            stale: false,
            paragraphs: vec![RefinedParagraph {
                speaker: "S1".into(), name: None, person_id: None,
                start_ms: 0, end_ms: 1000, text: "灯塔计划启动".into(), source_seqs: vec![7],
                mentions: vec![Mention { id: String::new(), entity: "ent_1".into(), start: 0, end: 4 }],
            }],
        };

        write_refined_atomic(dir.path(), &doc).unwrap();
        let first: serde_json::Value = serde_json::from_slice(&std::fs::read(dir.path().join(AING_DOC_FILE)).unwrap()).unwrap();
        let first_id = first["paragraphs"][0]["mentions"][0]["id"].as_str().unwrap().to_string();
        write_refined_atomic(dir.path(), &doc).unwrap();
        let second: serde_json::Value = serde_json::from_slice(&std::fs::read(dir.path().join(AING_DOC_FILE)).unwrap()).unwrap();

        assert!(first_id.starts_with("mn_"));
        assert_eq!(first_id.len(), 27);
        assert_eq!(second["paragraphs"][0]["mentions"][0]["id"].as_str(), Some(first_id.as_str()));
        assert_eq!(first.get("graph_extraction"), Some(&serde_json::Value::Null));
        assert_eq!(first.get("relations"), Some(&serde_json::json!([])));
        assert!(first.get("graph_support_mentions").is_none());
    }

    #[test]
    fn schema_v1_defaults_graph_fields() {
        let legacy = r#"{
            "schema_version": 1,
            "generated_at": "2026-07-01T09:00:00+08:00",
            "stages": { "filter": "done", "recluster": "done", "llm": "done" },
            "discarded_seqs": [],
            "paragraphs": [{
                "speaker": "S1", "start_ms": 0, "end_ms": 500,
                "text": "灯塔计划启动", "source_seqs": [7],
                "mentions": [{ "entity": "ent_1", "start": 0, "end": 4 }]
            }]
        }"#;
        let mut doc: RefinedDoc = serde_json::from_str(legacy).unwrap();

        ensure_graph_ids("note-1", &mut doc);
        let first_id = doc.paragraphs[0].mentions[0].id.clone();
        ensure_graph_ids("note-1", &mut doc);

        assert_eq!(doc.stages.relations, "off");
        assert!(doc.graph_extraction.is_none());
        assert!(doc.relations.is_empty());
        assert!(doc.graph_support_mentions.is_empty());
        assert!(doc.paragraphs[0].mentions[0].id.starts_with("mn_"));
        assert_eq!(doc.paragraphs[0].mentions[0].id, first_id);
        assert_eq!(doc.paragraphs[0].mentions[0].id.len(), 27);
    }

    #[test]
    fn evidence_ids_include_normalized_quote() {
        let first = evidence_id("note-1", &[7], 1, 3, "  灯塔   计划 ");
        let same_normalized = evidence_id("note-1", &[7], 1, 3, "灯塔 计划");
        let changed_quote = evidence_id("note-1", &[7], 1, 3, "灯塔项目");

        assert_eq!(first, same_normalized);
        assert_ne!(first, changed_quote);
    }

    #[test]
    fn roundtrip_and_corrupt_returns_none() {
        let dir = tempfile::tempdir().unwrap();
        assert!(load_refined(dir.path()).is_none(), "缺失返回 None");
        let doc = RefinedDoc {
            llm_failed_paragraphs: Vec::new(),
            schema_version: 1,
            generated_at: "2026-07-06T15:00:00+08:00".into(),
            written_at: String::new(),
            writer_pid: 0,
            finished_at: String::new(),
            writer_run: String::new(),
            llm_model: Some("deepseek-chat".into()),
            stages: RefineStages { filter: "done".into(), recluster: "done".into(), llm: "off".into(), entities: "off".into(), relations: "off".into() },
            discarded_seqs: vec![1, 2],
            entities: vec![],
            graph_extraction: None,
            relations: vec![],
            graph_support_mentions: vec![],
            revision: 0,
            stale: false,
            paragraphs: vec![RefinedParagraph {
                speaker: "R1".into(), name: Some("张三".into()), person_id: Some("P1".into()),
                start_ms: 0, end_ms: 5000, text: "你好。".into(), source_seqs: vec![0, 3],
                mentions: vec![],
            }],
        };
        write_refined_atomic(dir.path(), &doc).unwrap();
        let got = load_refined(dir.path()).expect("写后可读");
        assert_eq!(got.paragraphs.len(), 1);
        assert_eq!(got.discarded_seqs, vec![1, 2]);
        assert_eq!(got.paragraphs[0].name.as_deref(), Some("张三"));
        assert_eq!(got.paragraphs[0].person_id.as_deref(), Some("P1"));
        std::fs::write(dir.path().join(AING_DOC_FILE), "{broken").unwrap();
        assert!(load_refined(dir.path()).is_none(), "损坏返回 None 不 panic");
    }

    #[test]
    fn legacy_refined_json_migrates_to_aing_json_on_load() {
        let dir = tempfile::tempdir().unwrap();
        // 只有旧 refined.json,没有 aing.json
        let legacy = r#"{
            "schema_version": 1,
            "generated_at": "2026-07-01T09:00:00+08:00",
            "stages": { "filter": "done", "recluster": "done", "llm": "done" },
            "discarded_seqs": [],
            "paragraphs": [
                { "speaker": "S1", "start_ms": 0, "end_ms": 500, "text": "旧稿", "source_seqs": [0] }
            ]
        }"#;
        std::fs::write(dir.path().join("refined.json"), legacy).unwrap();
        assert!(!dir.path().join("aing.json").exists());

        let doc = load_refined(dir.path()).expect("应从旧 refined.json 迁移出");
        assert_eq!(doc.paragraphs[0].text, "旧稿");
        // 迁移把 aing.json 落盘,旧文件保留供回滚
        assert!(dir.path().join("aing.json").exists(), "迁移应写出 aing.json");
        assert!(dir.path().join("refined.json").exists(), "旧文件保留");
    }

    #[test]
    fn aing_json_takes_precedence_over_legacy() {
        let dir = tempfile::tempdir().unwrap();
        let mk = |text: &str| format!(
            r#"{{"schema_version":1,"generated_at":"t","stages":{{"filter":"done","recluster":"done","llm":"done"}},"discarded_seqs":[],"paragraphs":[{{"speaker":"S1","start_ms":0,"end_ms":1,"text":"{text}","source_seqs":[0]}}]}}"#
        );
        std::fs::write(dir.path().join("aing.json"), mk("新稿")).unwrap();
        std::fs::write(dir.path().join("refined.json"), mk("旧稿")).unwrap();
        assert_eq!(load_refined(dir.path()).unwrap().paragraphs[0].text, "新稿");
    }

    #[test]
    fn aing_exists_considers_both_filenames() {
        let dir = tempfile::tempdir().unwrap();
        assert!(!aing_exists(dir.path()));
        std::fs::write(dir.path().join("refined.json"), "{}").unwrap();
        assert!(aing_exists(dir.path()), "只有旧文件也算有");
        std::fs::remove_file(dir.path().join("refined.json")).unwrap();
        std::fs::write(dir.path().join("aing.json"), "{}").unwrap();
        assert!(aing_exists(dir.path()));
    }

    #[test]
    fn aing_fields_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let doc = RefinedDoc {
            llm_failed_paragraphs: Vec::new(),
            schema_version: REFINED_SCHEMA_VERSION,
            generated_at: "2026-07-16T10:00:00+08:00".into(),
            written_at: String::new(),
            writer_pid: 0,
            finished_at: String::new(),
            writer_run: String::new(),
            llm_model: None,
            stages: RefineStages {
                filter: "done".into(),
                recluster: "done".into(),
                llm: "off".into(),
                entities: "off".into(),
                relations: "off".into(),
            },
            discarded_seqs: vec![],
            entities: vec![Entity {
                id: "ent_1".into(),
                kind: "project".into(),
                name: "灯塔计划".into(),
                aliases: vec!["Lighthouse".into()],
            }],
            paragraphs: vec![RefinedParagraph {
                speaker: "S1".into(),
                name: None,
                person_id: None,
                start_ms: 0,
                end_ms: 1000,
                text: "灯塔计划下周启动".into(),
                source_seqs: vec![0],
                mentions: vec![Mention { id: "mn_000000000000000000000000".into(), entity: "ent_1".into(), start: 0, end: 4 }],
            }],
            graph_extraction: None,
            relations: vec![],
            graph_support_mentions: vec![],
            revision: 0,
            stale: false,
        };
        write_refined_atomic(dir.path(), &doc).unwrap();
        let back = load_refined(dir.path()).unwrap();
        assert_eq!(back.entities, doc.entities);
        assert_eq!(back.paragraphs[0].mentions, doc.paragraphs[0].mentions);
        assert_eq!(back.stages.entities, "off");
    }

    #[test]
    fn old_doc_without_aing_fields_still_loads_with_empty_defaults() {
        // 旧 refined.json:没有 entities / mentions / stages.entities 键
        let dir = tempfile::tempdir().unwrap();
        let old = r#"{
            "schema_version": 1,
            "generated_at": "2026-07-01T09:00:00+08:00",
            "stages": { "filter": "done", "recluster": "done", "llm": "done" },
            "discarded_seqs": [],
            "paragraphs": [
                { "speaker": "S1", "start_ms": 0, "end_ms": 500, "text": "你好", "source_seqs": [0] }
            ]
        }"#;
        std::fs::write(dir.path().join("refined.json"), old).unwrap();
        let doc = load_refined(dir.path()).expect("旧结构应能加载");
        assert!(doc.entities.is_empty());
        assert!(doc.paragraphs[0].mentions.is_empty());
        assert!(doc.graph_support_mentions.is_empty());
        assert_eq!(doc.stages.entities, "off", "缺 stages.entities 键默认 off");
    }

    /// 旧版 refined.json(无 person_id 字段)必须照常解析——字段缺省为 None。
    #[test]
    fn old_schema_without_person_id_still_loads() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("refined.json"),
            r#"{"schema_version":1,"generated_at":"t",
                "stages":{"filter":"done","recluster":"done","llm":"off"},
                "paragraphs":[{"speaker":"R1","start_ms":0,"end_ms":1000,"text":"嗯。","source_seqs":[0]}]}"#,
        )
        .unwrap();
        let doc = load_refined(dir.path()).expect("旧 schema 可读");
        assert!(doc.paragraphs[0].person_id.is_none());
        assert!(doc.paragraphs[0].name.is_none());
    }

    fn para(speaker: &str, name: Option<&str>, person: Option<&str>, start: u64) -> RefinedParagraph {
        RefinedParagraph {
            speaker: speaker.into(),
            name: name.map(str::to_string),
            person_id: person.map(str::to_string),
            start_ms: start,
            end_ms: start + 1000,
            text: "内容。".into(),
            source_seqs: vec![start / 1000],
            mentions: vec![],
        }
    }

    /// 修订段落的时间戳继承自源段,mic 轨漂移过就同样是错的。存在 align.json 时
    /// 「源段全是 mic」的段落须换到新时基并重排;跨两轨的段落不动(它的时间戳本来
    /// 就是两条时基混出来的,映射哪一条都不对)。
    #[test]
    fn align_map_shifts_mic_only_paragraphs_and_leaves_mixed_ones() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        // seq 0=mic, 1=system。段落 A 只含 mic(seq 0),B 只含 system(seq 1),C 跨两轨。
        let segs = concat!(
            r#"{"seq":0,"source":"mic","text":"甲","start_ms":0,"end_ms":900}"#,
            "\n",
            r#"{"seq":1,"source":"system","text":"乙","start_ms":1000,"end_ms":1900}"#,
            "\n"
        );
        std::fs::write(dir.join("segments.jsonl"), segs).unwrap();
        let mut a = para("S1", None, None, 0);
        a.source_seqs = vec![0];
        let mut b = para("S2", None, None, 1000);
        b.source_seqs = vec![1];
        let mut c = para("S3", None, None, 1500);
        c.source_seqs = vec![0, 1];
        write_doc(dir, vec![a, b, c]);

        let before = load_refined_for_display(dir).unwrap();
        assert_eq!(before.paragraphs[0].start_ms, 0, "未纠正时 mic 段落在最前");

        let map = crate::player_align::TimeMap::new(vec![(0.0, 2.0), (100.0, 102.0)]).unwrap();
        crate::store::align::write(dir, &map).unwrap();

        let after = load_refined_for_display(dir).unwrap();
        let by_speaker = |s: &str| {
            after.paragraphs.iter().find(|p| p.speaker == s).unwrap().start_ms
        };
        assert_eq!(by_speaker("S1"), 2000, "纯 mic 段落后移 2s");
        assert_eq!(by_speaker("S2"), 1000, "纯 system 段落不动");
        assert_eq!(by_speaker("S3"), 1500, "跨两轨的段落不动");
        // 顺序必须原样保持:数组下标就是保存契约(见 realign_paragraphs 文档注释)。
        assert_eq!(
            after.paragraphs.iter().map(|p| p.speaker.as_str()).collect::<Vec<_>>(),
            ["S1", "S2", "S3"],
            "投影只改时间戳,绝不重排——重排会让整篇保存的 orig_index 落到别的段上"
        );
    }

    /// 默认加载器**不得**带投影。这条是 P1 回归锁:仓库里遍布
    /// `load_refined → 改几个字段 → write_refined_atomic` 的写路径(Agent Aing 失败
    /// 落 stages 就是一处),一旦默认加载器带上投影,投影后的时间戳会被写回磁盘,
    /// 下次读取再投影一次,每失败一轮多漂一次,而且不可逆。
    #[test]
    fn default_loader_returns_raw_timestamps_so_write_paths_cannot_persist_the_projection() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        std::fs::write(
            dir.join("segments.jsonl"),
            concat!(r#"{"seq":0,"source":"mic","text":"甲","start_ms":0,"end_ms":900}"#, "\n"),
        )
        .unwrap();
        let mut a = para("S1", None, None, 0);
        a.source_seqs = vec![0];
        write_doc(dir, vec![a]);
        let map = crate::player_align::TimeMap::new(vec![(0.0, 9.0), (100.0, 109.0)]).unwrap();
        crate::store::align::write(dir, &map).unwrap();

        assert_eq!(load_refined(dir).unwrap().paragraphs[0].start_ms, 0, "默认加载器必须是磁盘真值");
        assert_eq!(
            load_refined_for_display(dir).unwrap().paragraphs[0].start_ms,
            9000,
            "投影只在明确要展示时发生"
        );

        // 模拟一次"读→改→写"的写路径:回写后再读,时间戳不得被推着走。
        let mut doc = load_refined(dir).unwrap();
        doc.stages.llm = "failed".into();
        write_refined_atomic(dir, &doc).unwrap();
        assert_eq!(
            load_refined(dir).unwrap().paragraphs[0].start_ms,
            0,
            "写路径回写后磁盘仍是原始时基"
        );
        assert_eq!(
            load_refined_for_display(dir).unwrap().paragraphs[0].start_ms,
            9000,
            "展示投影仍只映射一次(没有叠加)"
        );
    }

    /// 保存契约的回归锁:`load_refined_for_display`(展示投影)与 `load_refined_locked`
    /// (保存基线,磁盘原序)必须逐段一一对应。任何在读侧重排/增删段落的改动都会在此失败。
    #[test]
    fn display_projection_keeps_save_baseline_index_alignment() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        std::fs::write(
            dir.join("segments.jsonl"),
            concat!(
                r#"{"seq":0,"source":"mic","text":"甲","start_ms":0,"end_ms":900}"#,
                "\n",
                r#"{"seq":1,"source":"system","text":"乙","start_ms":1000,"end_ms":1900}"#,
                "\n"
            ),
        )
        .unwrap();
        let mut a = para("S1", None, None, 0);
        a.source_seqs = vec![0];
        let mut b = para("S2", None, None, 1000);
        b.source_seqs = vec![1];
        write_doc(dir, vec![a, b]);
        let map = crate::player_align::TimeMap::new(vec![(0.0, 9.0), (100.0, 109.0)]).unwrap();
        crate::store::align::write(dir, &map).unwrap();

        let shown = load_refined_for_display(dir).unwrap();
        let lock = NoteLock::acquire(dir).unwrap().unwrap();
        let baseline = load_refined_locked(dir, &lock).unwrap();
        assert_eq!(shown.paragraphs.len(), baseline.paragraphs.len());
        for (i, (s, b)) in shown.paragraphs.iter().zip(baseline.paragraphs.iter()).enumerate() {
            assert_eq!(s.speaker, b.speaker, "第 {i} 段在展示投影与保存基线上必须是同一段");
            assert_eq!(s.source_seqs, b.source_seqs);
        }
        assert_eq!(shown.paragraphs[0].start_ms, 9000, "投影确实改了时间戳(否则本测试退化)");
        assert_eq!(baseline.paragraphs[0].start_ms, 0, "保存基线保持磁盘原值");
    }

    fn write_doc(dir: &Path, paragraphs: Vec<RefinedParagraph>) {
        let doc = RefinedDoc {
            llm_failed_paragraphs: Vec::new(),
            schema_version: REFINED_SCHEMA_VERSION,
            generated_at: "t".into(),
            written_at: String::new(),
            writer_pid: 0,
            finished_at: String::new(),
            writer_run: String::new(),
            llm_model: None,
            stages: RefineStages { filter: "done".into(), recluster: "done".into(), llm: "off".into(), entities: "off".into(), relations: "off".into() },
            discarded_seqs: vec![],
            entities: vec![],
            graph_extraction: None,
            relations: vec![],
            graph_support_mentions: vec![],
            revision: 0,
            stale: false,
            paragraphs,
        };
        write_refined_atomic(dir, &doc).unwrap();
    }

    #[test]
    fn every_writer_respects_note_lock_and_locked_writer_can_commit() {
        let dir = tempfile::tempdir().unwrap();
        write_doc(dir.path(), vec![para("R1", None, None, 0)]);
        let original = std::fs::read(dir.path().join(AING_DOC_FILE)).unwrap();
        let guard = crate::store::notelock::NoteLock::try_exclusive(dir.path())
            .unwrap()
            .unwrap();

        let mut replacement = load_refined(dir.path()).unwrap();
        replacement.paragraphs[0].text = "整份替换。".into();
        assert!(
            write_refined_atomic(dir.path(), &replacement).is_err(),
            "公共整写也必须服从 NoteLock"
        );
        assert!(
            apply_refined_texts(dir.path(), &[(0, "局部替换。".into())], "m").is_err(),
            "公共 read-modify-write 必须服从同一把 NoteLock"
        );
        assert_eq!(
            std::fs::read(dir.path().join(AING_DOC_FILE)).unwrap(),
            original,
            "抢锁失败的写入不能触碰盘上真值"
        );

        write_refined_atomic_locked(dir.path(), &replacement, &guard).unwrap();
        drop(guard);
        assert_eq!(
            load_refined(dir.path()).unwrap().paragraphs[0].text,
            "整份替换。"
        );
    }

    #[test]
    fn apply_refined_texts_updates_and_marks_llm_done() {
        let dir = tempfile::tempdir().unwrap();
        write_doc(dir.path(), vec![para("R1", None, None, 0), para("R2", None, None, 1000)]);
        let n = apply_refined_texts(dir.path(), &[(1, "修订后。".into())], "claude-agent").unwrap();
        assert_eq!(n, 1);
        let doc = load_refined(dir.path()).unwrap();
        assert_eq!(doc.paragraphs[0].text, "内容。", "未提交的段落不动");
        assert_eq!(doc.paragraphs[1].text, "修订后。");
        assert_eq!(doc.stages.llm, "done");
        assert_eq!(doc.llm_model.as_deref(), Some("claude-agent"));
        assert_eq!(doc.paragraphs.len(), 2, "段落数不可变");
    }

    #[test]
    fn apply_refined_texts_empty_updates_means_reviewed_clean() {
        let dir = tempfile::tempdir().unwrap();
        write_doc(dir.path(), vec![para("R1", None, None, 0)]);
        assert_eq!(apply_refined_texts(dir.path(), &[], "m").unwrap(), 0);
        let doc = load_refined(dir.path()).unwrap();
        assert_eq!(doc.stages.llm, "done", "空 updates = 已审阅无需修订,同样算完成");
        assert_eq!(doc.paragraphs[0].text, "内容。");
    }

    #[test]
    fn apply_refined_texts_rejects_bad_input_without_writing() {
        let dir = tempfile::tempdir().unwrap();
        write_doc(dir.path(), vec![para("R1", None, None, 0)]);
        assert!(apply_refined_texts(dir.path(), &[(9, "x".into())], "m").is_err(), "下标越界");
        assert!(apply_refined_texts(dir.path(), &[(0, "  ".into())], "m").is_err(), "空文本");
        // 混合提交里带一个坏项:整体拒绝,好项也不落盘
        assert!(apply_refined_texts(dir.path(), &[(0, "好的。".into()), (5, "x".into())], "m").is_err());
        let doc = load_refined(dir.path()).unwrap();
        assert_eq!(doc.paragraphs[0].text, "内容。", "整体拒绝,未落盘任何修改");
        assert_eq!(doc.stages.llm, "off");
        // 无修订稿时报错,不凭空造文件
        let empty = tempfile::tempdir().unwrap();
        assert!(apply_refined_texts(empty.path(), &[(0, "x".into())], "m").is_err());
    }

    #[test]
    fn join_note_identities_resolves_s_and_maps_legacy_r() {
        use crate::store::voiceprints::{Person, Voiceprints};
        let mut vp = Voiceprints::default();
        vp.people.insert("P1".into(), Person { name: "张三".into(), ..Default::default() });
        vp.redirects.insert("P2".into(), "P1".into());
        let mut speakers: std::collections::BTreeMap<String, crate::store::SpeakerMeta> =
            Default::default();
        let meta = |name: &str, person: Option<&str>| crate::store::SpeakerMeta {
            name: name.into(),
            sources: vec!["mic".into()],
            centroid: None,
            count: 1,
            person_id: person.map(str::to_string),
            multi_speaker: false,
            reserved_by: None,
            split_born: false,
            hint_person: None,
        };
        speakers.insert("S1".into(), meta("", Some("P2"))); // 关联(经 redirect)
        speakers.insert("S2".into(), meta("现场名", None)); // 只有本地名
        let segments = vec![
            crate::store::SegmentRecord { seq: 0, source: "mic".into(), text: "a".into(), start_ms: 0, end_ms: 1000, speaker: Some("S1".into()), rms: None },
            crate::store::SegmentRecord { seq: 1, source: "mic".into(), text: "b".into(), start_ms: 1000, end_ms: 2000, speaker: Some("S2".into()), rms: None },
            crate::store::SegmentRecord { seq: 2, source: "mic".into(), text: "c".into(), start_ms: 2000, end_ms: 3000, speaker: Some("S2".into()), rms: None },
        ];
        let mut doc = RefinedDoc {
            llm_failed_paragraphs: Vec::new(),
            schema_version: REFINED_SCHEMA_VERSION,
            generated_at: "t".into(),
            written_at: String::new(),
            writer_pid: 0,
            finished_at: String::new(),
            writer_run: String::new(),
            llm_model: None,
            stages: RefineStages { filter: "done".into(), recluster: "done".into(), llm: "off".into(), entities: "off".into(), relations: "off".into() },
            discarded_seqs: vec![],
            entities: vec![],
            graph_extraction: None,
            relations: vec![],
            graph_support_mentions: vec![],
            revision: 0,
            stale: false,
            paragraphs: vec![
                // 新格式:S 键,身份现查表(经 redirect 归一、名字跟库)。
                { let mut p = para("S1", None, None, 0); p.source_seqs = vec![0]; p },
                // 旧 R 键:source_seqs 多数票映射到 S2,采用其本地名。
                { let mut p = para("R7", Some("旧名"), None, 1000); p.source_seqs = vec![1, 2]; p },
                // 无源段的遗留段:保留字段,但 person 归一 redirects + 跟库名。
                { let mut p = para("R8", Some("旧快照名"), Some("P2"), 3000); p.source_seqs = vec![]; p },
            ],
        };
        join_note_identities(&mut doc, &speakers, &segments, &vp);
        assert_eq!(doc.paragraphs[0].person_id.as_deref(), Some("P1"));
        assert_eq!(doc.paragraphs[0].name.as_deref(), Some("张三"));
        assert_eq!(doc.paragraphs[1].speaker, "S2", "旧 R 键按源段多数票映射回 S");
        assert!(doc.paragraphs[1].person_id.is_none());
        assert_eq!(doc.paragraphs[1].name.as_deref(), Some("现场名"));
        assert_eq!(doc.paragraphs[2].speaker, "R8", "无源段:不硬造映射");
        assert_eq!(doc.paragraphs[2].person_id.as_deref(), Some("P1"), "遗留引用仍归一 redirects");
        assert_eq!(doc.paragraphs[2].name.as_deref(), Some("张三"));
    }

    fn editable_doc() -> RefinedDoc {
        serde_json::from_value(serde_json::json!({
            "schema_version": 2,
            "generated_at": "2026-07-30T00:00:00Z",
            "stages": { "filter": "done", "recluster": "done", "llm": "done", "entities": "done", "relations": "done" },
            "discarded_seqs": [],
            "revision": 3,
            "entities": [{ "id": "P1", "kind": "person", "name": "张三" }],
            "relations": [{
                "id": "rel1",
                "subject": "P1",
                "predicate": { "type": "mentions" },
                "object": "P1",
                "subject_mentions": ["m1"],
                "object_mentions": ["m1"],
                "confidence": 0.9,
                "evidence": [{ "id": "ev1", "paragraph_index": 0, "start": 0, "end": 2, "quote": "张三",
                               "source_seqs": [1], "source_hash": "h" }]
            }],
            "paragraphs": [
                { "speaker": "R1", "start_ms": 0, "end_ms": 1000, "text": "张三在发言", "source_seqs": [1],
                  "mentions": [{ "id": "m1", "entity": "P1", "start": 0, "end": 2 }] },
                { "speaker": "R2", "start_ms": 1000, "end_ms": 2000, "text": "第二段", "source_seqs": [2] }
            ]
        }))
        .expect("fixture 反序列化失败")
    }

    fn payload(items: &[(Option<usize>, &str, bool)]) -> Vec<ParagraphPayload> {
        items
            .iter()
            .map(|(i, t, d)| ParagraphPayload { orig_index: *i, text: t.to_string(), dirty: *d })
            .collect()
    }

    #[test]
    fn save_refined_rejects_revision_mismatch() {
        let dir = tempfile::tempdir().unwrap();
        let note = dir.path().join("n1");
        std::fs::create_dir_all(&note).unwrap();
        write_refined_atomic(&note, &editable_doc()).unwrap();
        let err = save_refined_paragraphs(&note, 999, &payload(&[(Some(0), "x", true)])).unwrap_err();
        assert!(err.to_string().contains("revision"), "错误应指明版本冲突: {err}");
    }

    #[test]
    fn save_refined_replaces_texts_and_bumps_revision() {
        let dir = tempfile::tempdir().unwrap();
        let note = dir.path().join("n1");
        std::fs::create_dir_all(&note).unwrap();
        write_refined_atomic(&note, &editable_doc()).unwrap();
        let rev = load_refined(&note).unwrap().revision; // 整写进位后的实际值
        let new_rev = save_refined_paragraphs(
            &note,
            rev,
            &payload(&[(Some(0), "张三在发言", false), (Some(1), "改过的第二段", true)]),
        )
        .unwrap();
        assert_eq!(new_rev, rev + 1);
        let doc = load_refined(&note).unwrap();
        assert_eq!(doc.revision, new_rev);
        assert_eq!(doc.paragraphs[1].text, "改过的第二段");
        // 干净段保留 speaker/时间戳/mentions
        assert_eq!(doc.paragraphs[0].speaker, "R1");
        assert_eq!(doc.paragraphs[0].mentions.len(), 1);
        assert!(doc.graph_support_mentions.is_empty());
    }

    /// Critical 1 回归:干净段(dirty=false)的正文一律保留盘上原文,载荷文本被忽略。
    /// 编辑器把带 live mention 的段按字面载入,再经 commonmark 序列化成载荷时会加
    /// markdown 转义(`1. 议题` → `1\. 议题`、`预算[初稿]` → `预算\[初稿]`、
    /// `2*3` → `2\*3`);载入基线同样是转义结果,所以 dirty 判不出来。若后端照载荷
    /// 替换,用户只编辑了第二段,第一段也会被写进转义符,mentions 的字符偏移错位。
    #[test]
    fn save_refined_keeps_clean_paragraph_text_byte_for_byte() {
        let dir = tempfile::tempdir().unwrap();
        let note = dir.path().join("n1");
        std::fs::create_dir_all(&note).unwrap();
        let mut doc = editable_doc();
        let prefix = "1. 议题:预算[初稿] ";
        let clean_text = format!("{prefix}张三 主讲,2*3 个方案");
        let start = prefix.chars().count();
        doc.paragraphs[0].text = clean_text.clone();
        doc.paragraphs[0].mentions =
            vec![Mention { id: "m1".into(), entity: "P1".into(), start, end: start + 2 }];
        write_refined_atomic(&note, &doc).unwrap();
        let rev = load_refined(&note).unwrap().revision;

        // 编辑器载荷:第一段是序列化后的转义文本 + dirty=false;第二段是真实编辑。
        let escaped = "1\\. 议题:预算\\[初稿] 张三 主讲,2\\*3 个方案";
        let new_rev = save_refined_paragraphs(
            &note,
            rev,
            &payload(&[(Some(0), escaped, false), (Some(1), "用户改过的第二段", true)]),
        )
        .unwrap();

        let back = load_refined(&note).unwrap();
        assert_eq!(back.revision, new_rev);
        assert_eq!(back.paragraphs[0].text, clean_text, "干净段正文必须逐字节不变");
        assert_eq!(back.paragraphs[0].mentions, doc.paragraphs[0].mentions, "mention 偏移不得漂移");
        let chars: Vec<char> = back.paragraphs[0].text.chars().collect();
        let m = &back.paragraphs[0].mentions[0];
        assert_eq!(chars[m.start..m.end].iter().collect::<String>(), "张三", "偏移仍落在实体原文上");
        assert!(back.graph_support_mentions.is_empty(), "干净段 mention 仍是 live");
        assert_eq!(back.paragraphs[1].text, "用户改过的第二段", "脏段照常替换");
    }

    #[test]
    fn save_refined_dirty_paragraph_moves_mentions_to_support() {
        let dir = tempfile::tempdir().unwrap();
        let note = dir.path().join("n1");
        std::fs::create_dir_all(&note).unwrap();
        write_refined_atomic(&note, &editable_doc()).unwrap();
        let rev = load_refined(&note).unwrap().revision;
        save_refined_paragraphs(&note, rev, &payload(&[(Some(0), "改写了第一段", true), (Some(1), "第二段", false)]))
            .unwrap();
        let doc = load_refined(&note).unwrap();
        // mention 仍在段上(图谱结构完整),但 id 进了 support 列表(UI/搜索不再当 live)
        assert_eq!(doc.paragraphs[0].mentions.len(), 1);
        assert!(doc.graph_support_mentions.contains(&"m1".to_string()));
        // 证据偏移随脏段失效,但关系端点仍有 mention 支撑,关系保留
        assert!(doc.relations[0].evidence.is_empty());
        assert_eq!(doc.relations.len(), 1);
    }

    #[test]
    fn save_refined_removed_paragraph_prunes_relations() {
        let dir = tempfile::tempdir().unwrap();
        let note = dir.path().join("n1");
        std::fs::create_dir_all(&note).unwrap();
        write_refined_atomic(&note, &editable_doc()).unwrap();
        let rev = load_refined(&note).unwrap().revision;
        // 只保留第二段:第一段(含 m1)被删 → 引用 m1 的关系整条剪掉
        save_refined_paragraphs(&note, rev, &payload(&[(Some(1), "第二段", false)])).unwrap();
        let doc = load_refined(&note).unwrap();
        assert_eq!(doc.paragraphs.len(), 1);
        assert!(doc.relations.is_empty());
        assert!(doc.graph_support_mentions.is_empty());
    }

    #[test]
    fn save_refined_new_block_gets_empty_speaker() {
        let dir = tempfile::tempdir().unwrap();
        let note = dir.path().join("n1");
        std::fs::create_dir_all(&note).unwrap();
        write_refined_atomic(&note, &editable_doc()).unwrap();
        let rev = load_refined(&note).unwrap().revision;
        save_refined_paragraphs(
            &note,
            rev,
            &payload(&[(None, "## 会议纪要", true), (Some(0), "张三在发言", false), (Some(1), "第二段", false)]),
        )
        .unwrap();
        let doc = load_refined(&note).unwrap();
        assert_eq!(doc.paragraphs.len(), 3);
        assert_eq!(doc.paragraphs[0].speaker, "");
        assert_eq!(doc.paragraphs[0].text, "## 会议纪要");
        assert!(doc.paragraphs[0].source_seqs.is_empty());
        // 证据 paragraph_index 随原第 0 段后移一位
        assert_eq!(doc.relations[0].evidence[0].paragraph_index, 1);
    }

    #[test]
    fn save_refined_rejects_empty_text_and_bad_index() {
        let dir = tempfile::tempdir().unwrap();
        let note = dir.path().join("n1");
        std::fs::create_dir_all(&note).unwrap();
        write_refined_atomic(&note, &editable_doc()).unwrap();
        let rev = load_refined(&note).unwrap().revision;
        assert!(save_refined_paragraphs(&note, rev, &payload(&[(Some(0), "  ", true)])).is_err());
        assert!(save_refined_paragraphs(&note, rev, &payload(&[(Some(9), "x", true)])).is_err());
        assert!(save_refined_paragraphs(&note, rev, &payload(&[(Some(0), "a", true), (Some(0), "b", true)])).is_err());
        // 三次失败都不应落盘:revision 与段落数保持不变
        let doc = load_refined(&note).unwrap();
        assert_eq!(doc.revision, rev);
        assert_eq!(doc.paragraphs.len(), 2);
    }

    #[test]
    fn write_refined_atomic_never_regresses_revision() {
        let dir = tempfile::tempdir().unwrap();
        let note = dir.path().join("n1");
        std::fs::create_dir_all(&note).unwrap();
        let mut doc = editable_doc();
        doc.revision = 5;
        write_refined_atomic(&note, &doc).unwrap();
        // 管线重跑拿着旧内存 doc(revision 0)整写 → 落盘必须进位到 6,而不是回到 0
        let mut stale = editable_doc();
        stale.revision = 0;
        write_refined_atomic(&note, &stale).unwrap();
        assert_eq!(load_refined(&note).unwrap().revision, 6);
    }

    #[test]
    fn write_refined_atomic_locked_never_regresses_revision() {
        // 模拟精修管线主路径:mod.rs 直接持锁调 write_refined_atomic_locked,
        // 新 doc 硬编码 revision: 0。never-regress 规则必须在这条路径上同样生效,
        // 而不仅仅在公共 write_refined_atomic 入口生效。
        let dir = tempfile::tempdir().unwrap();
        let note = dir.path().join("n1");
        std::fs::create_dir_all(&note).unwrap();
        let mut doc = editable_doc();
        doc.revision = 5;
        write_refined_atomic(&note, &doc).unwrap();
        assert_eq!(load_refined(&note).unwrap().revision, 5);

        let lock = NoteLock::acquire(&note).unwrap().expect("应能取得笔记锁");
        let mut pipeline_doc = editable_doc();
        pipeline_doc.revision = 0;
        write_refined_atomic_locked(&note, &pipeline_doc, &lock).unwrap();
        drop(lock);

        assert_eq!(load_refined(&note).unwrap().revision, 6);
    }

    #[test]
    fn write_refined_atomic_locked_passes_through_equal_revision() {
        // 「载入-改-写回」型 writer(迁移写、mark_graph_failed 这类同锁内先读后写)
        // 传入的 doc.revision 本就等于盘上现值——收敛点的单调性后备只在严格大于时
        // 才纠正，相等必须原样透传，否则这类合法 writer 的内存态会与盘面永久漂移。
        let dir = tempfile::tempdir().unwrap();
        let note = dir.path().join("n1");
        std::fs::create_dir_all(&note).unwrap();
        let mut doc = editable_doc();
        doc.revision = 3;
        write_refined_atomic(&note, &doc).unwrap();
        let on_disk_revision = load_refined(&note).unwrap().revision;

        let lock = NoteLock::acquire(&note).unwrap().expect("应能取得笔记锁");
        let mut same_revision_doc = editable_doc();
        same_revision_doc.revision = on_disk_revision;
        write_refined_atomic_locked(&note, &same_revision_doc, &lock).unwrap();
        drop(lock);

        assert_eq!(load_refined(&note).unwrap().revision, on_disk_revision);
    }
    // ── 失败段下标的重映射与序列化(部分重跑,2026-08-21) ──

    #[test]
    fn wysiwyg_save_remaps_failed_paragraph_indices() {
        // 失败列表 [0,2];保存删掉段 0、保留段 1/2 → 列表应重映射为 [1](原段 2 的新位置)。
        // 不映射的话部分重跑会把润色写错段(设计自查修正项)。
        let dir = tempfile::tempdir().unwrap();
        let mut doc = RefinedDoc {
            llm_failed_paragraphs: vec![0, 2],
            schema_version: REFINED_SCHEMA_VERSION,
            generated_at: "t".into(),
            written_at: String::new(),
            writer_pid: 0,
            finished_at: String::new(),
            writer_run: String::new(),
            llm_model: None,
            stages: RefineStages { filter: "done".into(), recluster: "done".into(), llm: "partial".into(), entities: "off".into(), relations: "off".into() },
            discarded_seqs: vec![],
            entities: vec![],
            graph_extraction: None,
            relations: vec![],
            graph_support_mentions: vec![],
            revision: 5,
            stale: false,
            paragraphs: (0..3)
                .map(|i| RefinedParagraph {
                    speaker: "R1".into(), name: None, person_id: None,
                    start_ms: i * 1000, end_ms: i * 1000 + 900,
                    text: format!("段{i}"), source_seqs: vec![i], mentions: vec![],
                })
                .collect(),
        };
        write_refined_atomic(dir.path(), &doc).unwrap();
        doc = load_refined(dir.path()).unwrap(); // 取落盘 revision 基线
        let payload = vec![
            ParagraphPayload { orig_index: Some(1), text: "段1".into(), dirty: false },
            ParagraphPayload { orig_index: Some(2), text: "段2".into(), dirty: false },
        ];
        save_refined_paragraphs(dir.path(), doc.revision, &payload).unwrap();
        let saved = load_refined(dir.path()).unwrap();
        assert_eq!(saved.llm_failed_paragraphs, vec![1], "原段2 → 新下标1;被删的段0移出");
    }

    #[test]
    fn empty_failed_list_is_not_serialized() {
        // 空列表不落键:旧文件形状不变,与一切按字节/JSON 比对的既有快照兼容。
        let doc = RefinedDoc {
            llm_failed_paragraphs: vec![],
            schema_version: REFINED_SCHEMA_VERSION,
            generated_at: "t".into(),
            written_at: String::new(),
            writer_pid: 0,
            finished_at: String::new(),
            writer_run: String::new(),
            llm_model: None,
            stages: RefineStages { filter: "done".into(), recluster: "done".into(), llm: "done".into(), entities: "off".into(), relations: "off".into() },
            discarded_seqs: vec![],
            entities: vec![],
            graph_extraction: None,
            relations: vec![],
            graph_support_mentions: vec![],
            revision: 0,
            stale: false,
            paragraphs: vec![],
        };
        let json = serde_json::to_string(&doc).unwrap();
        assert!(!json.contains("llm_failed_paragraphs"), "{json}");
        let back: RefinedDoc = serde_json::from_str(&json).unwrap();
        assert!(back.llm_failed_paragraphs.is_empty());
    }

    // ── 拆分后的修订稿同步(混杂说话人 Phase D) ──

    fn split_fixture(dir: &std::path::Path) {
        let doc = RefinedDoc {
            llm_failed_paragraphs: Vec::new(),
            schema_version: REFINED_SCHEMA_VERSION,
            generated_at: "2026-08-20T00:00:00+08:00".into(),
            written_at: String::new(),
            writer_pid: 0,
            finished_at: String::new(),
            writer_run: String::new(),
            llm_model: None,
            stages: RefineStages { filter: "done".into(), recluster: "done".into(), llm: "done".into(), entities: "off".into(), relations: "off".into() },
            discarded_seqs: vec![],
            entities: vec![],
            graph_extraction: None,
            relations: vec![],
            graph_support_mentions: vec![],
            revision: 3,
            stale: false,
            paragraphs: vec![
                RefinedParagraph {
                    speaker: "R5".into(), name: None, person_id: None,
                    start_ms: 0, end_ms: 1000, text: "整段同去向".into(), source_seqs: vec![1, 2],
                    mentions: vec![],
                },
                RefinedParagraph {
                    speaker: "R5".into(), name: None, person_id: None,
                    start_ms: 1000, end_ms: 2000, text: "跨组段落".into(), source_seqs: vec![3, 4],
                    mentions: vec![],
                },
            ],
        };
        write_refined_atomic(dir, &doc).unwrap();
    }

    #[test]
    fn split_sync_updates_whole_paragraph_in_place() {
        let dir = tempfile::tempdir().unwrap();
        split_fixture(dir.path());
        let moved: std::collections::BTreeMap<u64, String> =
            [(1u64, "S9".to_string()), (2u64, "S9".to_string())].into();
        let stale = sync_refined_after_split(dir.path(), &moved).unwrap();
        assert!(!stale);
        let doc = load_refined(dir.path()).unwrap();
        assert_eq!(doc.paragraphs[0].speaker, "S9");
        // 一波说话人:段落不携带身份,归属改写后 person/name 恒空,显示端现查表。
        assert!(doc.paragraphs[0].person_id.is_none() && doc.paragraphs[0].name.is_none());
        assert_eq!(doc.paragraphs[0].text, "整段同去向", "文本一字不动");
        assert_eq!(doc.paragraphs[1].speaker, "R5", "未触及的段落不动");
        assert!(!doc.stale);
    }

    #[test]
    fn split_sync_marks_stale_when_paragraph_crosses_groups() {
        // source_seqs 没有字符级映射:段落被拆到多组时不拆文本,整份标 stale。
        let dir = tempfile::tempdir().unwrap();
        split_fixture(dir.path());
        let moved: std::collections::BTreeMap<u64, String> =
            [(3u64, "S9".to_string()), (4u64, "S10".to_string())].into();
        let stale = sync_refined_after_split(dir.path(), &moved).unwrap();
        assert!(stale);
        let doc = load_refined(dir.path()).unwrap();
        assert!(doc.stale, "跨组 → 整份标 stale 等重新 Aing");
        assert_eq!(doc.paragraphs[1].speaker, "R5", "不尝试拆文本/改归属");
    }
}
