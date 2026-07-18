//! stderr/stdout 黑匣子：GUI 方式启动的 App 没有终端，eprintln 与 ONNX Runtime
//! 的错误输出全部丢失——2026-07-07 两次 VAD 内 ORT 抛异常闪退（SIGABRT），崩溃报告
//! 只有调用栈拿不到错误文本，就是因为没有这份日志。启动即把 2 个 fd 重定向到
//! app_data_dir/logs/stderr.log，事后排障直接看文件。
//!
//! dev 场景（stderr 是 tty）跳过重定向，保留终端实时输出。

use std::path::Path;

/// 日志超过该字节数时轮转为 .old（只留一代，上限约 10MB 总量，不需要更复杂的策略）。
const ROTATE_BYTES: u64 = 5 * 1024 * 1024;

pub fn redirect_stdio_to_file(app_data: &Path) {
    // Windows 首版无 fd 级重定向:dup2/STDERR_FILENO 是 unix 概念,Windows GUI 子系统
    // 的 stdout/stderr 需 SetStdHandle + CRT fd 双重接管,留作后续(计划文档已记);
    // dev 构建带控制台,输出仍可见,不至于两眼一抹黑。
    #[cfg(not(unix))]
    {
        let _ = app_data;
        return;
    }
    #[cfg(unix)]
    redirect_stdio_to_file_unix(app_data);
}

#[cfg(unix)]
fn redirect_stdio_to_file_unix(app_data: &Path) {
    // dev（cargo/终端启动）不重定向:is_terminal 判定,比 debug_assertions 更准
    //（release 二进制手动从终端跑时也能看到输出）。
    use std::io::IsTerminal;
    if std::io::stderr().is_terminal() {
        return;
    }
    let logs = app_data.join("logs");
    if std::fs::create_dir_all(&logs).is_err() {
        return; // 建目录都失败(磁盘满/权限),放弃重定向,绝不挡启动
    }
    let path = logs.join("stderr.log");
    if let Ok(md) = std::fs::metadata(&path) {
        if md.len() > ROTATE_BYTES {
            let _ = std::fs::rename(&path, logs.join("stderr.old.log"));
        }
    }
    let Ok(f) = std::fs::OpenOptions::new().create(true).append(true).open(&path) else {
        return;
    };
    use std::os::fd::AsRawFd;
    let fd = f.as_raw_fd();
    unsafe {
        libc::dup2(fd, libc::STDOUT_FILENO);
        libc::dup2(fd, libc::STDERR_FILENO);
    }
    // fd 必须活到进程结束:关闭 f 会使 dup2 后的 1/2 变悬空。
    std::mem::forget(f);
    eprintln!(
        "\n===== voice-notes {} 启动 {} =====",
        env!("CARGO_PKG_VERSION"),
        chrono::Local::now().format("%Y-%m-%d %H:%M:%S")
    );
}
