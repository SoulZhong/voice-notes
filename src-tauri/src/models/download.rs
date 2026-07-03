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
    Ok(())
}

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
    let resp = match req.call() {
        Ok(r) => r,
        // ureq 对 4xx/5xx 返回 Err(Status)。416 = 续传偏移越界（上次崩溃残留
        // 满尺寸 .part）：清掉重来，下次重试从头下载。
        Err(ureq::Error::Status(416, _)) => {
            let _ = fs::remove_file(&part);
            anyhow::bail!("续传偏移越界，已清理残留分片，请重试");
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
    fn extract_and_install_replaces_existing_install() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("models");
        std::fs::create_dir_all(root.join("sv-dir")).unwrap();
        std::fs::write(root.join("sv-dir/model.onnx"), b"OLD").unwrap();
        let tarball = make_tarball(tmp.path(), "sv-dir", &[("model.onnx", b"MODEL")]);
        let files = [ff("sv-dir/model.onnx", b"MODEL")];
        extract_and_install(&tarball, &root, "sv-dir", &files).unwrap();
        assert_eq!(std::fs::read(root.join("sv-dir/model.onnx")).unwrap(), b"MODEL", "旧安装被替换");
        assert!(!root.join(".old-sv-dir").exists(), "备份目录成功后清除");
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
