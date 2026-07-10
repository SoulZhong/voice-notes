//! record 控制 CLI:start/stop/pause/resume/status/live 经 super::bridge::call 打到运行中的 GUI。

use super::bridge;

const USAGE: &str = "用法: voice-notes record <start|stop|pause|resume|status|live> [选项]\n  \
start [--title 标题] | stop | pause | resume | status | live [--tail N]\n  通用: --json 输出原始 JSON";

/// 取 `--flag 值` 的值(未出现返回 None)。
fn flag_value(args: &[String], name: &str) -> Option<String> {
    args.iter().position(|a| a == name).and_then(|i| args.get(i + 1).cloned())
}

/// 拒绝未知 flag(以 -- 开头且不在白名单;白名单里带值的 flag 其值不算 flag)。
fn reject_unknown(args: &[String], allowed: &[&str]) -> Result<(), String> {
    let mut i = 0;
    while i < args.len() {
        let a = &args[i];
        if a.starts_with("--") {
            if !allowed.contains(&a.as_str()) {
                return Err(format!("未知选项 {a}"));
            }
            // 带值 flag(--title/--tail)跳过其值
            if a == "--title" || a == "--tail" {
                i += 1;
            }
        }
        i += 1;
    }
    Ok(())
}

fn usage_err(msg: &str) -> i32 {
    eprintln!("{msg}\n{USAGE}");
    2
}

/// 人读渲染:status 显状态,start/stop 显 note_id,其余回退紧凑 JSON。
fn render_human(sub: &str, data: &serde_json::Value) -> String {
    match sub {
        "status" => format!(
            "状态: {} | note: {} | 时长: {}ms\n",
            data.get("state").and_then(|v| v.as_str()).unwrap_or("?"),
            data.get("note_id").and_then(|v| v.as_str()).filter(|s| !s.is_empty()).unwrap_or("-"),
            data.get("elapsed_ms").and_then(|v| v.as_u64()).unwrap_or(0),
        ),
        "start" | "stop" => format!(
            "note_id: {}\n",
            data.get("note_id").and_then(|v| v.as_str()).unwrap_or("-"),
        ),
        _ => format!("{data}\n"),
    }
}

pub fn record_cli(args: &[String]) -> i32 {
    let Some((sub, rest)) = args.split_first() else {
        eprintln!("{USAGE}");
        return 2;
    };
    let json_out = rest.iter().any(|a| a == "--json");
    let result = match sub.as_str() {
        "start" => {
            if let Err(m) = reject_unknown(rest, &["--title", "--json"]) {
                return usage_err(&m);
            }
            let extra = match flag_value(rest, "--title") {
                Some(t) => serde_json::json!({ "title": t }),
                None => serde_json::json!({}),
            };
            bridge::call("start", extra)
        }
        "stop" | "pause" | "resume" | "status" => {
            if let Err(m) = reject_unknown(rest, &["--json"]) {
                return usage_err(&m);
            }
            bridge::call(sub, serde_json::json!({}))
        }
        "live" => {
            if let Err(m) = reject_unknown(rest, &["--tail", "--json"]) {
                return usage_err(&m);
            }
            let tail = match flag_value(rest, "--tail") {
                Some(v) => match v.parse::<u64>() {
                    Ok(n) => n,
                    Err(_) => return usage_err("--tail 需要整数"),
                },
                None => 20,
            };
            bridge::call("live", serde_json::json!({ "tail": tail }))
        }
        _ => {
            eprintln!("{USAGE}");
            return 2;
        }
    };
    match result {
        Ok(data) => {
            if json_out {
                println!("{}", serde_json::to_string_pretty(&data).unwrap_or_default());
            } else {
                print!("{}", render_human(sub, &data));
            }
            0
        }
        Err(msg) => {
            eprintln!("{msg}");
            1
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_helpers() {
        let args = vec!["--title".to_string(), "评审会".to_string(), "--json".to_string()];
        assert_eq!(flag_value(&args, "--title").as_deref(), Some("评审会"));
        assert_eq!(flag_value(&args, "--tail"), None);
        assert!(reject_unknown(&args, &["--title", "--json"]).is_ok());
        assert!(reject_unknown(&args, &["--json"]).is_err(), "未知 --title 应报错");
    }

    #[test]
    fn usage_errors_exit_2_without_touching_bridge() {
        assert_eq!(record_cli(&["bogus".into()]), 2, "未知子命令");
        assert_eq!(record_cli(&[]), 2, "缺子命令");
        assert_eq!(record_cli(&["start".into(), "--nope".into()]), 2, "未知 flag");
        assert_eq!(record_cli(&["live".into(), "--tail".into(), "abc".into()]), 2, "--tail 非整数");
    }
}
