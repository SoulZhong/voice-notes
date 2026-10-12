//! 模型下载器：断点续传 + SHA256 校验 + tar.bz2 解压进位。
//! 本文件的纯逻辑（镜像拼接/校验/解压）由单测覆盖；网络路径（download_artifact）
//! 靠人工冒烟，不做单测。

use super::FinalFile;
use sha2::{Digest, Sha256};
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

/// 稳定错误码:以 "[E_*]" 前缀嵌入错误串,重试判定只认码不认人类可读文案——
/// 文案可随 UI 语言(tr!)翻译,控制流不受影响。"cancelled" 字面量是取消协议,不在此列。
pub const CODE_SIZE_MISMATCH: &str = "[E_SIZE_MISMATCH]";
pub const CODE_SHA256_MISMATCH: &str = "[E_SHA256_MISMATCH]";
pub const CODE_ARCHIVE_MISSING_DIR: &str = "[E_ARCHIVE_MISSING_DIR]";

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
        anyhow::bail!(
            "{CODE_SIZE_MISMATCH} {}",
            crate::tr!(
                "{rel} 大小不符: {len} != {want}",
                "{rel}: size mismatch: {len} != {want}",
                rel = expected.rel_path,
                len = len,
                want = expected.bytes
            )
        );
    }
    let got = sha256_file(path)?;
    if got != expected.sha256 {
        anyhow::bail!(
            "{CODE_SHA256_MISMATCH} {}",
            crate::tr!("{rel} SHA256 校验失败", "{rel}: SHA256 checksum failed", rel = expected.rel_path)
        );
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

/// 包装 Reader:每次 read 前查取消标志,置位即返回 Err——把取消响应性带进
/// 解压这类长同步调用(unpack 内部逐块拉取,取消在块级——下一次 read——响应)。
/// ErrorKind 用 Other 而非 Interrupted:Interrupted 会被多数 Read 消费者自动重试,
/// 永远断不掉。
struct CancelReader<'a, R: Read> {
    inner: R,
    cancel: &'a AtomicBool,
}

impl<R: Read> Read for CancelReader<'_, R> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        if self.cancel.load(Ordering::Relaxed) {
            return Err(std::io::Error::new(std::io::ErrorKind::Other, "cancelled"));
        }
        self.inner.read(buf)
    }
}

/// 解压 tar.bz2 到 root/.tmp-extract，校验 files 后把 dest_dir 整体 rename 进位。
/// 任何一步失败都不触碰 root 下的既有安装。
/// prune: 安装成功后即删的 root 相对路径（如 whisper fp32/test_wavs）——
/// 只是省盘的增值清理，删不掉也不算安装失败。
pub fn extract_and_install(
    tarball: &Path,
    root: &Path,
    dest_dir: &str,
    files: &[FinalFile],
    prune: &[&str],
    cancel: &AtomicBool,
) -> anyhow::Result<()> {
    let tmp = tmp_extract_dir(root);
    let _ = fs::remove_dir_all(&tmp);
    fs::create_dir_all(&tmp)?;
    let f = fs::File::open(tarball)?;
    let reader = CancelReader { inner: f, cancel };
    if let Err(e) = tar::Archive::new(bzip2::read::BzDecoder::new(reader)).unpack(&tmp) {
        let _ = fs::remove_dir_all(&tmp);
        // 归一取消错误:上层(download_artifact/lib.rs)以 msg=="cancelled" 识别,
        // 走保留 .part 的取消路径而非删分片的失败路径。
        if cancel.load(Ordering::Relaxed) {
            anyhow::bail!("cancelled");
        }
        return Err(e.into());
    }
    let src = tmp.join(dest_dir);
    if !src.is_dir() {
        anyhow::bail!(
            "{CODE_ARCHIVE_MISSING_DIR} {}",
            crate::tr!("压缩包内缺少目录 {dest_dir}", "Archive is missing directory {dest_dir}", dest_dir = dest_dir)
        );
    }
    // FinalFile.rel_path 相对 models root，而 tmp 镜像 root 布局，直接拼即可。
    for ff in files {
        verify_file(&tmp.join(ff.rel_path), ff)?;
    }
    // 换位安装：旧安装先挪到备份位，新目录 rename 失败时可回滚——任何失败不触碰既有安装。
    let dst = root.join(dest_dir);
    let backup = root.join(format!(".old-{dest_dir}"));
    let _ = fs::remove_dir_all(&backup);
    let had_old = dst.exists();
    if had_old {
        fs::rename(&dst, &backup)?;
    }
    if let Err(e) = fs::rename(&src, &dst) {
        if had_old {
            let _ = fs::rename(&backup, &dst); // 回滚旧安装
        }
        return Err(e.into());
    }
    let _ = fs::remove_dir_all(&backup);
    let _ = fs::remove_dir_all(&tmp);
    // 装好即删:fp32/test_wavs 这类只在装包校验期有用的大件，装完不留盘。
    for p in prune {
        let target = root.join(p);
        if fs::remove_dir_all(&target).is_err() {
            if let Err(e) = fs::remove_file(&target) {
                eprintln!("prune {p} 失败(不影响安装): {e}");
            }
        }
    }
    Ok(())
}

/// 进度回调：(artifact_id, phase, received_bytes, total_bytes, message)。
pub type Progress = dyn Fn(&str, &str, u64, u64, &str);

/// 下载完成(或 416 判定本地已有全量 .part)后的收尾:校验/解压安装,成功清 .part。
/// 失败删 .part(脏数据不值得续)——唯 "cancelled" 例外:tarball 完好,保留供复装。
fn finalize_artifact(
    a: &super::Artifact,
    root: &Path,
    part: &Path,
    cancel: &AtomicBool,
    progress: &Progress,
    received: u64,
    total: u64,
) -> anyhow::Result<()> {
    match &a.kind {
        super::ArtifactKind::File => {
            progress(a.id, "verifying", received, total, "");
            if let Err(e) = verify_file(part, &a.files[0]) {
                let _ = fs::remove_file(part);
                return Err(e);
            }
            fs::rename(part, root.join(a.files[0].rel_path))?;
        }
        super::ArtifactKind::TarBz2 { dest_dir } => {
            progress(a.id, "extracting", received, total, "");
            if let Err(e) = extract_and_install(part, root, dest_dir, a.files, a.prune, cancel) {
                if e.to_string() != "cancelled" {
                    let _ = fs::remove_file(part);
                }
                return Err(e);
            }
            let _ = fs::remove_file(part);
        }
    }
    progress(a.id, "done", received, total, "");
    Ok(())
}

/// 下载并安装单个工件。断点：root/<id>.part（HTTP Range 续传；服务端不支持则重下）。
/// 线路(直连/镜像)由 [`crate::netroute::fetch`] 自动选择与切换。
/// cancel 置位 → Err 且消息恰为 "cancelled"（保留 .part 供续传）；
/// 校验/解压失败 → 删 .part（脏数据不值得续）并 Err。
/// 进度回调的 message 为 "mirror" 表示已换到加速线路(界面据此提示)。
pub fn download_artifact(
    a: &super::Artifact,
    root: &Path,
    memory: &crate::netroute::RouteMemory,
    cancel: &AtomicBool,
    progress: &Progress,
) -> anyhow::Result<()> {
    fs::create_dir_all(root)?;
    let part = root.join(format!("{}.part", a.id));
    let on_progress = |p: crate::netroute::Progress| {
        let note = if p.switched && p.mirror { "mirror" } else { "" };
        progress(a.id, "downloading", p.received, p.total, note);
    };
    let received = crate::netroute::fetch(a.url, &part, memory, cancel, &on_progress).map_err(|e| anyhow::anyhow!(e))?;
    finalize_artifact(a, root, &part, cancel, progress, received, received)
}

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
    fn verify_file_checks_size_then_hash() {
        let tmp = tempfile::tempdir().unwrap();
        let p = tmp.path().join("m.bin");
        std::fs::write(&p, b"hello").unwrap();
        assert!(verify_file(&p, &ff("m.bin", b"hello")).is_ok());
        let size_err = verify_file(&p, &ff("m.bin", b"hell")).unwrap_err().to_string();
        assert!(size_err.contains(CODE_SIZE_MISMATCH), "大小不符应带稳定码: {size_err}");
        let mut wrong = ff("m.bin", b"hello");
        wrong.sha256 = Box::leak("0".repeat(64).into_boxed_str());
        let hash_err = verify_file(&p, &wrong).unwrap_err().to_string();
        assert!(hash_err.contains(CODE_SHA256_MISMATCH), "哈希不符应带稳定码: {hash_err}");
    }

    #[test]
    fn extract_and_install_happy_path_installs_and_cleans_tmp() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("models");
        std::fs::create_dir_all(&root).unwrap();
        let tarball = make_tarball(tmp.path(), "sv-dir", &[("model.onnx", b"MODEL"), ("tokens.txt", b"TOK")]);
        let files = [ff("sv-dir/model.onnx", b"MODEL"), ff("sv-dir/tokens.txt", b"TOK")];
        extract_and_install(&tarball, &root, "sv-dir", &files, &[], &AtomicBool::new(false)).unwrap();
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
        assert!(extract_and_install(&tarball, &root, "sv-dir", &files, &[], &AtomicBool::new(false)).is_err());
        assert!(!root.join("sv-dir").exists(), "校验失败不得半安装");
    }

    #[test]
    fn extract_and_install_replaces_existing_install() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("models");
        std::fs::create_dir_all(root.join("sv-dir")).unwrap();
        std::fs::write(root.join("sv-dir/model.onnx"), b"OLD").unwrap();
        let tarball = make_tarball(tmp.path(), "sv-dir", &[("model.onnx", b"MODEL")]);
        let files = [ff("sv-dir/model.onnx", b"MODEL")];
        extract_and_install(&tarball, &root, "sv-dir", &files, &[], &AtomicBool::new(false)).unwrap();
        assert_eq!(std::fs::read(root.join("sv-dir/model.onnx")).unwrap(), b"MODEL", "旧安装被替换");
        assert!(!root.join(".old-sv-dir").exists(), "备份目录成功后清除");
    }

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

    #[test]
    fn sweep_tmp_removes_residue() {
        let tmp = tempfile::tempdir().unwrap();
        let d = tmp_extract_dir(tmp.path());
        std::fs::create_dir_all(&d).unwrap();
        std::fs::File::create(d.join("junk")).unwrap().write_all(b"x").unwrap();
        sweep_tmp(tmp.path());
        assert!(!d.exists());
    }

    #[test]
    fn extract_cancel_is_prompt_and_preserves_tarball() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("models");
        std::fs::create_dir_all(&root).unwrap();
        let tarball = make_tarball(tmp.path(), "sv-dir", &[("model.onnx", b"MODEL")]);
        let files = [ff("sv-dir/model.onnx", b"MODEL")];
        let cancel = AtomicBool::new(true); // 预先置位:首次 read 即断
        let err = extract_and_install(&tarball, &root, "sv-dir", &files, &[], &cancel).unwrap_err();
        assert_eq!(err.to_string(), "cancelled", "取消错误归一,供上层按消息识别");
        assert!(!root.join("sv-dir").exists(), "取消不得半安装");
        assert!(tarball.exists(), "tarball 由调用方保留(供免重下复装)");
        assert!(!tmp_extract_dir(&root).exists(), "解压残留即时清理");
    }

    #[test]
    #[ignore]
    fn real_download_installs_vad_with_verified_hash() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let a = crate::models::ARTIFACTS
            .iter()
            .find(|a| a.id == "vad")
            .expect("vad 已注册");
        let noop: &Progress = &|id, phase, received, total, msg| {
            eprintln!("[{id}] {phase} {received}/{total} {msg}");
        };
        let memory = crate::netroute::RouteMemory::new(root);
        download_artifact(a, root, &memory, &AtomicBool::new(false), noop)
            .expect("真机下载应成功");
        let installed = root.join(a.files[0].rel_path);
        assert!(installed.is_file(), "应落位 models 根目录");
        assert_eq!(std::fs::metadata(&installed).unwrap().len(), a.files[0].bytes);
        verify_file(&installed, &a.files[0]).expect("SHA256 应与钉死值一致");
    }

    /// 联网:注册表里每个模型的下载地址都还在(发版前跑)。自托管工件的 Release 被删过
    /// 一次(models-dtln-aec-v1,疑随 2026-08-08 重写历史清理远端标签而丢),全体用户
    /// 下载 404 两个月无人发现。只取 1 字节;单文件工件顺带核对服务端总长。
    #[test]
    #[ignore]
    fn every_registered_model_url_is_downloadable() {
        let mut broken = Vec::new();
        for a in crate::models::ARTIFACTS {
            let agent = crate::netproxy::agent_for(a.url);
            match agent.get(a.url).timeout(std::time::Duration::from_secs(30)).set("Range", "bytes=0-0").call() {
                Ok(r) => {
                    let total = r
                        .header("Content-Range")
                        .and_then(|v| v.rsplit('/').next())
                        .and_then(|n| n.parse::<u64>().ok());
                    if let (super::super::ArtifactKind::File, Some(n)) = (&a.kind, total) {
                        if n != a.files[0].bytes {
                            broken.push(format!("{}: 服务端 {n} 字节,登记 {}", a.id, a.files[0].bytes));
                        }
                    }
                }
                Err(e) => broken.push(format!("{}: {e}", a.id)),
            }
        }
        assert!(broken.is_empty(), "下载地址失效:\n{}", broken.join("\n"));
    }

    /// 416 免重下复装的核心路径:全量有效 .part 直接 finalize 完成安装(无网络)。
    #[test]
    fn finalize_artifact_installs_valid_full_part_without_network() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("models");
        std::fs::create_dir_all(&root).unwrap();
        let tarball = make_tarball(tmp.path(), "sv-dir", &[("model.onnx", b"MODEL")]);
        let part = root.join("sv.part");
        std::fs::copy(&tarball, &part).unwrap();
        let a = crate::models::Artifact {
            id: "sv",
            label: "测试工件",
            url: "http://unused.invalid/pkg.tar.bz2",
            approx_mb: 1,
            prune: &[],
            kind: crate::models::ArtifactKind::TarBz2 { dest_dir: "sv-dir" },
            files: Box::leak(vec![ff("sv-dir/model.onnx", b"MODEL")].into_boxed_slice()),
        };
        let noop: &Progress = &|_, _, _, _, _| {};
        finalize_artifact(&a, &root, &part, &AtomicBool::new(false), noop, 0, 0).unwrap();
        assert_eq!(std::fs::read(root.join("sv-dir/model.onnx")).unwrap(), b"MODEL");
        assert!(!part.exists(), "复装成功清 .part");
    }
}
