//! 模型下载器：断点续传 + SHA256 校验 + tar.bz2 解压进位。
//! 本文件的纯逻辑（镜像拼接/校验/解压）由单测覆盖；网络路径（download_artifact）
//! 靠人工冒烟，不做单测。

use super::FinalFile;
use sha2::{Digest, Sha256};
use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

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

pub fn download_urls(url: &str, mirror_enabled: bool, mirror_prefix: &str) -> Vec<String> {
    let mirrored = apply_mirror(url, mirror_enabled, mirror_prefix);
    if mirrored == url {
        vec![url.to_string()]
    } else {
        vec![mirrored, url.to_string()]
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
        anyhow::bail!("压缩包内缺少目录 {dest_dir}");
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
    let resp = match req.call() {
        Ok(r) => r,
        // 416 = 偏移 ≥ 服务端全量。两种来源:上次解压被取消(.part 是完好全量
        // tarball,直接收尾复装,免整包重下)或崩溃残留脏分片(收尾校验失败 →
        // finalize 已删分片,报错引导重试,同旧行为)。
        Err(ureq::Error::Status(416, _)) => {
            return finalize_artifact(a, root, &part, cancel, progress, offset, offset).map_err(|e| {
                if e.to_string() == "cancelled" {
                    e
                } else {
                    anyhow::anyhow!("续传偏移越界,残留分片无法直接安装,请重试({e})")
                }
            });
        }
        Err(e) => anyhow::bail!("请求失败: {e}"),
    };
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

    finalize_artifact(a, root, &part, cancel, progress, received, total)
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
    fn apply_mirror_prefixes_only_when_enabled() {
        let u = "https://github.com/a/b.onnx";
        assert_eq!(apply_mirror(u, false, "https://ghproxy.net/"), u);
        assert_eq!(apply_mirror(u, true, ""), u, "空前缀视同关闭");
        assert_eq!(apply_mirror(u, true, "https://ghproxy.net/"), format!("https://ghproxy.net/{u}"));
        assert_eq!(apply_mirror(u, true, "https://ghproxy.net"), format!("https://ghproxy.net/{u}"), "自动补尾斜杠");
    }

    #[test]
    fn download_urls_try_mirror_first_then_origin_when_enabled() {
        let u = "https://github.com/a/b.onnx";
        assert_eq!(download_urls(u, false, "https://ghproxy.net/"), vec![u.to_string()]);
        assert_eq!(download_urls(u, true, ""), vec![u.to_string()]);
        assert_eq!(
            download_urls(u, true, "https://ghproxy.net/"),
            vec![format!("https://ghproxy.net/{u}"), u.to_string()],
        );
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
