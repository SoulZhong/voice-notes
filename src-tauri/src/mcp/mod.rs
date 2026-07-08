//! MCP 子系统入口:argv `voice-notes mcp ...` 的 CLI 分发,与无 tauri 环境下的
//! app_data 解析(stdio 服务进程/注册 CLI 都不经过 tauri Builder)。

use std::path::PathBuf;

pub mod registry;

/// 无 tauri 环境下的 app_data_dir。identifier 与 tauri.conf.json 保持一致——
/// GUI 侧 `app.path().app_data_dir()` 解析到的正是这个目录。VN_APP_DATA 供
/// 测试与 e2e 注入 tempdir(生产不设)。
pub fn app_data_dir() -> PathBuf {
    if let Ok(p) = std::env::var("VN_APP_DATA") {
        if !p.is_empty() {
            return PathBuf::from(p);
        }
    }
    let home = std::env::var("HOME").unwrap_or_default();
    PathBuf::from(home).join("Library/Application Support/com.teemo.voice-notes")
}

/// `voice-notes mcp <sub> ...` 的分发。返回进程退出码。
pub fn cli_main(args: &[String]) -> i32 {
    let sub = args.first().map(String::as_str).unwrap_or("");
    match sub {
        "serve" | "register" | "unregister" | "status" => {
            eprintln!("mcp {sub}: 尚未实现");
            1
        }
        _ => {
            eprintln!(
                "用法: voice-notes mcp <serve|register|unregister|status>\n\
                 serve                 以 stdio MCP 服务运行(供 Agent spawn)\n\
                 register [--agent X]  注册到本机 Agent(X: claude-code|claude-desktop|cursor|codex|gemini|auto)\n\
                 unregister [--agent X]\n\
                 status [--json]       各 Agent 检测/注册状态"
            );
            2
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn app_data_respects_env_override() {
        std::env::set_var("VN_APP_DATA", "/tmp/vn-test-app-data");
        assert_eq!(app_data_dir(), PathBuf::from("/tmp/vn-test-app-data"));
        std::env::remove_var("VN_APP_DATA");
        let p = app_data_dir();
        assert!(p.ends_with("Library/Application Support/com.teemo.voice-notes"), "{p:?}");
    }

    #[test]
    fn cli_unknown_subcommand_exits_2() {
        assert_eq!(cli_main(&["nope".into()]), 2);
        assert_eq!(cli_main(&[]), 2);
    }
}
