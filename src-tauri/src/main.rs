// Prevents additional console window on Windows in release, DO NOT REMOVE!!
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

fn main() {
    // `voice-notes mcp ...` 走无 GUI 的 CLI/stdio 分支,必须在 tauri Builder 之前
    // 拦截——MCP 客户端 spawn 本进程时绝不能弹窗口/托盘。LaunchServices 正常打开
    // App 不带参数,不受影响。
    let args: Vec<String> = std::env::args().collect();
    if args.get(1).map(String::as_str) == Some("mcp") {
        std::process::exit(app_lib::mcp::cli_main(&args[2..]));
    }
    app_lib::run()
}
