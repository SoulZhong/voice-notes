//! MCP 子系统入口:argv `voice-notes mcp ...` 的 CLI 分发,与无 tauri 环境下的
//! app_data 解析(stdio 服务进程/注册 CLI 都不经过 tauri Builder)。

use std::path::PathBuf;

pub mod registry;
pub mod server;
pub mod tools;

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
        "serve" => server::serve_stdio(),
        "register" | "unregister" | "status" => run_registry_cli(sub, &args[1..]),
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

/// --agent X 解析:未给或 auto = 所有"已检测到安装"的家;显式指名不要求已安装
/// (用户可能装在非常规位置,检测只是启发)。
fn parse_agent(args: &[String]) -> Option<String> {
    let mut it = args.iter();
    while let Some(a) = it.next() {
        if a == "--agent" {
            return it.next().cloned();
        }
    }
    None
}

fn run_registry_cli(sub: &str, args: &[String]) -> i32 {
    let reg = match registry::Registry::new() {
        Ok(r) => r,
        Err(e) => {
            eprintln!("初始化失败: {e}");
            return 1;
        }
    };
    let agent = parse_agent(args).unwrap_or_else(|| "auto".into());
    let dry_run = args.iter().any(|a| a == "--dry-run");
    let json = args.iter().any(|a| a == "--json");

    if sub == "status" {
        let st = reg.status();
        if json {
            println!("{}", serde_json::to_string_pretty(&st).expect("Serialize 派生结构不会失败"));
        } else {
            for s in &st {
                let state = if !s.installed {
                    "未检测到"
                } else if s.stale {
                    "已注册(路径过期)"
                } else if s.registered {
                    "已注册"
                } else {
                    "未注册"
                };
                println!("{:<16} {}", s.key, state);
            }
        }
        return 0;
    }

    // register / unregister 的目标集合
    let targets: Vec<String> = if agent == "auto" {
        reg.status().into_iter().filter(|s| s.installed).map(|s| s.key).collect()
    } else {
        vec![agent]
    };
    if targets.is_empty() {
        eprintln!("未检测到任何已安装的 Agent;可用 --agent 显式指定,或在 App 设置页复制手动配置。");
        return 1;
    }
    if let Some(w) = reg.quarantine_warning() {
        eprintln!("{w}");
    }
    if dry_run {
        println!("将写入的条目:\n{}", reg.entry_snippet_json());
        println!("目标: {}", targets.join(", "));
        return 0;
    }
    let mut failed = 0;
    for key in &targets {
        let r = if sub == "register" { reg.register(key) } else { reg.unregister(key) };
        match r {
            Ok(()) => println!("{key}: ok"),
            Err(e) => {
                eprintln!("{key}: {e}");
                failed += 1;
            }
        }
    }
    if failed == 0 {
        0
    } else {
        1
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
