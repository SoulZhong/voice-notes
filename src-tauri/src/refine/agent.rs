//! A2 Aing 的 Agent provider:spawn 本机 Agent CLI(Claude Code / Codex / Gemini /
//! Cursor),让它经自家 MCP server(`voice-notes mcp serve`)读 Aing 稿并调
//! `apply_refined_texts` 写回。与 llm.rs 的 HTTP provider 并列,由 lib.rs 按
//! settings.refine_provider 二选一。
//!
//! 核心原则:**成败判定不信 Agent 的任何输出**——各家 CLI 输出格式互不相同且随版本
//! 漂移,唯一可信的是盘上 refined.json 的终态(写回工具会把 stages.llm 置 "done",
//! 见 store::refined::apply_refined_texts)。spawn 前管线刚整写过 refined.json
//! (stages.llm=="off"),因此「跑完后盘上 llm=="done"」当且仅当 Agent 真调过写回工具。
//!
//! 每次调用在系统临时目录建一次性工作区(scratch dir)作为子进程 cwd:
//! - 隔离:避免 Agent 把用户某个项目目录当工作区,加载到无关的项目级配置/记忆;
//! - 注入:Gemini(.gemini/settings.json)与 Cursor(.cursor/mcp.json)只认工作区
//!   配置文件,MCP server 条目写在这里;Claude 用内联 --mcp-config + --strict-mcp-config,
//!   Codex 用 -c 覆盖,均不落盘、不碰用户全局配置。

use crate::store::{load_refined, RefinedParagraph};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

/// Agent 一次完整 Aing(读稿+逐段修订+写回)的墙钟上限。跑满视为挂死,杀进程判失败。
pub const REFINE_TIMEOUT_S: u64 = 900;
/// 标题一发一收的上限。
pub const TITLE_TIMEOUT_S: u64 = 120;
/// 「测试运行」探测的超时:只验能启动+能产出,远短于 Aing 的 REFINE_TIMEOUT_S。
pub const PROBE_TIMEOUT_S: u64 = 60;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentKind {
    Claude,
    Codex,
    Gemini,
    Cursor,
}

impl AgentKind {
    /// settings.refine_agent 的取值 ↔ 枚举。未知值返回 None(前端下拉之外的手改)。
    pub fn from_key(key: &str) -> Option<Self> {
        match key {
            "claude" => Some(Self::Claude),
            "codex" => Some(Self::Codex),
            "gemini" => Some(Self::Gemini),
            "cursor" => Some(Self::Cursor),
            _ => None,
        }
    }

    pub fn key(self) -> &'static str {
        match self {
            Self::Claude => "claude",
            Self::Codex => "codex",
            Self::Gemini => "gemini",
            Self::Cursor => "cursor",
        }
    }

    /// 可执行文件名(Cursor 的 CLI 叫 cursor-agent,不是 cursor——后者是 GUI)。
    pub fn bin_name(self) -> &'static str {
        match self {
            Self::Claude => "claude",
            Self::Codex => "codex",
            Self::Gemini => "gemini",
            Self::Cursor => "cursor-agent",
        }
    }
}

/// 解析 Agent CLI 可执行文件。用户显式指定路径时只认该路径(存在性检查,不回退——
/// 显式配置错了就该报错,静默换一个二进制跑是意外行为);未指定时按常见安装位置探测,
/// 最后试 PATH(GUI 进程从 launchd 继承的 PATH 通常没有 ~/.local/bin 等用户目录,
/// 所以固定路径探测在前,`which` 只是开发环境/CLI 场景的兜底)。
pub fn resolve_bin(kind: AgentKind, override_path: &str) -> Option<PathBuf> {
    if !override_path.trim().is_empty() {
        let p = PathBuf::from(override_path.trim());
        return p.is_file().then_some(p);
    }
    let home = std::env::var("HOME").unwrap_or_default();
    let name = kind.bin_name();
    let mut candidates = vec![
        PathBuf::from(&home).join(".local/bin").join(name),
        PathBuf::from("/opt/homebrew/bin").join(name),
        PathBuf::from("/usr/local/bin").join(name),
    ];
    if kind == AgentKind::Claude {
        // claude 官方迁移安装器的自管位置。
        candidates.push(PathBuf::from(&home).join(".claude/local/claude"));
    }
    // nvm 全局包(codex/gemini 常经 npm -g 安装):扫各 node 版本的 bin。
    if let Ok(rd) = std::fs::read_dir(PathBuf::from(&home).join(".nvm/versions/node")) {
        for e in rd.flatten() {
            candidates.push(e.path().join("bin").join(name));
        }
    }
    if let Some(hit) = candidates.into_iter().find(|p| p.is_file()) {
        return Some(hit);
    }
    // PATH 兜底
    let out = Command::new("which").arg(name).output().ok()?;
    if !out.status.success() {
        return None;
    }
    let p = PathBuf::from(String::from_utf8_lossy(&out.stdout).trim().to_string());
    p.is_file().then_some(p)
}

/// 探测全部四家的解析结果,供设置页展示「已检测到/未检测到」。
pub fn probe_all() -> Vec<(&'static str, Option<String>)> {
    [AgentKind::Claude, AgentKind::Codex, AgentKind::Gemini, AgentKind::Cursor]
        .into_iter()
        .map(|k| (k.key(), resolve_bin(k, "").map(|p| p.display().to_string())))
        .collect()
}

/// 「测试运行」:用配好的 CLI 跑一句极短提示,验证能启动并产出。不依赖任何笔记,
/// 不落 AI 日志。成功返回 stdout 摘要;失败返回归类原因。
pub fn probe_run(provider: &str, bin_override: &str, model: &str) -> Result<String, String> {
    let kind = AgentKind::from_key(provider).ok_or_else(|| format!("未知 Agent: {provider}"))?;
    let bin = resolve_bin(kind, bin_override)
        .ok_or_else(|| format!("未找到 {} 命令行工具:请先安装并登录,或指定 CLI 路径", kind.bin_name()))?;
    let scratch = make_scratch("probe").map_err(|e| format!("建工作区失败: {e}"))?;
    let prompt = "只回复两个字:正常。不要任何解释。";
    let run = (|| -> anyhow::Result<(bool, String, String)> {
        let cmd = title_command(kind, &bin, model, prompt, &scratch);
        run_with_timeout(cmd, &scratch, PROBE_TIMEOUT_S)
    })();
    let _ = std::fs::remove_dir_all(&scratch);
    match run {
        Err(e) => {
            let s = e.to_string();
            if s.contains("超时") {
                Err(format!("测试超时({PROBE_TIMEOUT_S}s):CLI 可能未登录或卡住"))
            } else {
                Err(format!("启动失败:{s}"))
            }
        }
        Ok((exit_ok, stdout, err_tail)) => {
            if !exit_ok {
                Err(format!("退出码非 0;stderr 尾部:\n{err_tail}"))
            } else if stdout.trim().is_empty() {
                Err("无输出:CLI 可能未登录或未产出结果".to_string())
            } else {
                let first: String = stdout.trim().lines().next().unwrap_or("").chars().take(30).collect();
                Ok(format!("CLI 可用,返回:{first}"))
            }
        }
    }
}

/// Aing 指令。与 llm.rs SYSTEM_PROMPT 同一套四类修订规则,但流程改为「读稿→修订→
/// 工具写回」;各家 CLI 对 MCP 工具的暴露名前缀不同(claude 是 mcp__server__tool,
/// gemini/cursor 是裸名),提示词里只用裸名,由各家自行映射。
fn refine_prompt(note_id: &str) -> String {
    format!(
        "你是会议逐字稿 Aing 助手。任务:Aing voice-notes 笔记 {note_id} 的 Aing 稿文本。\n\
         步骤:\n\
         1. 调用 MCP 工具 get_note,参数 {{\"note_id\":\"{note_id}\",\"format\":\"segments\"}},\
         取返回的 paragraphs 数组(段落下标从 0 计;若返回 refined=false 说明还没有 Aing 稿,直接结束并说明)。\n\
         2. 逐段检查,只做四类修订,除此之外禁止任何改动(不改句式和语义,不合并/拆分段落):\n\
         a) 纠正同音/近音错字(如「肯计→肯定」),不确定时保留原文;\
         b) 实体归一:同一人名/产品名/术语全文统一为最常见写法;\
         c) 删除无意义的「嗯」「呃」及紧邻重复(「我们我们→我们」),保留「吧」「啊」等语气词;\
         d) 英文与中文之间加空格,产品名保持原大小写。\n\
         3. 调用 MCP 工具 apply_refined_texts 一次性写回,参数 \
         {{\"note_id\":\"{note_id}\",\"updates\":[{{\"index\":段落下标,\"text\":\"该段修订后的完整文本\"}},...],\
         \"model\":\"你的模型名\"}};只提交有改动的段落;若全文确无需要修订,updates 传空数组 []。\n\
         只允许使用这两个 MCP 工具;不要读写任何文件,不要执行任何命令。完成后回复一行「完成」即可。"
    )
}

/// voice-notes 自身二进制路径(Agent spawn `<exe> mcp serve` 用)。
/// VN_SELF_EXE 供 e2e 注入(cargo test 进程的 current_exe 是测试二进制,充当不了
/// MCP server;与 mcp::app_data_dir 的 VN_APP_DATA 同一惯例,生产不设)。
fn self_exe() -> anyhow::Result<PathBuf> {
    if let Ok(p) = std::env::var("VN_SELF_EXE") {
        if !p.is_empty() {
            return Ok(PathBuf::from(p));
        }
    }
    Ok(std::env::current_exe()?)
}

/// mcpServers JSON 条目(claude 内联 / gemini settings.json / cursor mcp.json 共用形状)。
fn mcp_servers_json(exe: &Path) -> serde_json::Value {
    serde_json::json!({
        "mcpServers": {
            "voice-notes": { "command": exe.to_string_lossy(), "args": ["mcp", "serve"] }
        }
    })
}

/// 组装一次 Aing 调用:返回配置好参数/cwd 的 Command。scratch 已存在;按 kind 落好
/// 工作区配置文件。纯组装不 spawn,供单测检查参数面。
fn refine_command(
    kind: AgentKind,
    bin: &Path,
    model: &str,
    prompt: &str,
    exe: &Path,
    scratch: &Path,
) -> anyhow::Result<Command> {
    let mut cmd = Command::new(bin);
    cmd.current_dir(scratch);
    match kind {
        AgentKind::Claude => {
            // --strict-mcp-config:只用内联这一份,不加载用户全局 MCP 配置(否则用户
            // 注册过的其它 server 全部起进程,慢且面大)。白名单只放行两只只读/约束写工具。
            cmd.args(["-p", prompt, "--output-format", "json", "--strict-mcp-config"])
                .arg("--mcp-config")
                .arg(mcp_servers_json(exe).to_string())
                .args([
                    "--allowedTools",
                    "mcp__voice-notes__get_note,mcp__voice-notes__apply_refined_texts",
                    "--max-turns",
                    "30",
                ]);
            if !model.is_empty() {
                cmd.args(["--model", model]);
            }
        }
        AgentKind::Codex => {
            // codex exec 非交互;MCP server 经 -c 配置覆盖注入(TOML 裸键允许连字符),
            // 不写用户 ~/.codex/config.toml。scratch 不是 git 仓库,须 --skip-git-repo-check。
            cmd.args(["exec", "--skip-git-repo-check", "--sandbox", "read-only"])
                .arg("-c")
                .arg(format!("mcp_servers.voice-notes.command={:?}", exe.to_string_lossy()))
                .arg("-c")
                .arg(r#"mcp_servers.voice-notes.args=["mcp","serve"]"#);
            if !model.is_empty() {
                cmd.args(["-m", model]);
            }
            cmd.arg(prompt);
        }
        AgentKind::Gemini => {
            // gemini 只认 settings.json 里的 mcpServers → 写进一次性工作区的项目级配置;
            // coreTools=[] 关掉全部内置工具(shell/文件读写),这样 yolo 自动批准的只剩
            // 白名单 MCP server 的两只工具,面收得比默认(内置工具全开)更小。
            let dir = scratch.join(".gemini");
            std::fs::create_dir_all(&dir)?;
            let mut settings = mcp_servers_json(exe);
            settings["coreTools"] = serde_json::json!([]);
            std::fs::write(dir.join("settings.json"), serde_json::to_vec_pretty(&settings)?)?;
            cmd.args([
                prompt,
                "-o",
                "json",
                "--approval-mode",
                "yolo",
                "--allowed-mcp-server-names",
                "voice-notes",
            ]);
            if !model.is_empty() {
                cmd.args(["-m", model]);
            }
        }
        AgentKind::Cursor => {
            // cursor-agent 只认工作区 .cursor/mcp.json;--trust 免首次信任交互,
            // --approve-mcps 自动批准 MCP server。不给 --force(那是放行 shell 命令的)。
            let dir = scratch.join(".cursor");
            std::fs::create_dir_all(&dir)?;
            std::fs::write(dir.join("mcp.json"), serde_json::to_vec_pretty(&mcp_servers_json(exe))?)?;
            cmd.args(["-p", prompt, "--output-format", "text", "--trust", "--approve-mcps"])
                .arg("--workspace")
                .arg(scratch);
            if !model.is_empty() {
                cmd.args(["--model", model]);
            }
        }
    }
    Ok(cmd)
}

/// 标题一发一收(无 MCP、无工具)。输出解析统一为「stdout 最后一个非空行」——各家
/// 文本模式的最终答复都在末尾,前面混进的日志/横幅靠调用方的长度守卫兜底拒绝。
fn title_command(kind: AgentKind, bin: &Path, model: &str, prompt: &str, scratch: &Path) -> Command {
    let mut cmd = Command::new(bin);
    cmd.current_dir(scratch);
    match kind {
        AgentKind::Claude => {
            // --strict-mcp-config 且不给 --mcp-config = 零 MCP server。
            cmd.args(["-p", prompt, "--strict-mcp-config", "--max-turns", "1"]);
            if !model.is_empty() {
                cmd.args(["--model", model]);
            }
        }
        AgentKind::Codex => {
            cmd.args(["exec", "--skip-git-repo-check", "--sandbox", "read-only"]);
            if !model.is_empty() {
                cmd.args(["-m", model]);
            }
            cmd.arg(prompt);
        }
        AgentKind::Gemini => {
            cmd.arg(prompt);
            if !model.is_empty() {
                cmd.args(["-m", model]);
            }
        }
        AgentKind::Cursor => {
            cmd.args(["-p", prompt, "--output-format", "text", "--trust"])
                .arg("--workspace")
                .arg(scratch);
            if !model.is_empty() {
                cmd.args(["--model", model]);
            }
        }
    }
    cmd
}

/// GUI 从 Finder/launchd 启动不继承 shell 的代理变量;需经本地代理出海的网络环境
/// 下,Agent CLI 直连 API 会被 403(真机实锤:同一台机器终端带 proxy 环境变量即成功,
/// GUI spawn 无代理变量即 403 Request not allowed)。spawn 前若环境里没有任何代理
/// 变量,读 macOS 系统代理(scutil --proxy)注入;环境已有代理(终端 dev/CLI 场景)
/// 一律不动——显式配置永远优先。
fn proxy_env_to_inject() -> Vec<(String, String)> {
    const KEYS: [&str; 4] = ["http_proxy", "HTTP_PROXY", "https_proxy", "HTTPS_PROXY"];
    if KEYS.iter().any(|k| std::env::var_os(k).is_some()) {
        return Vec::new();
    }
    let Ok(out) = Command::new("scutil").arg("--proxy").output() else { return Vec::new() };
    parse_scutil_proxy(&String::from_utf8_lossy(&out.stdout))
}

/// 解析 `scutil --proxy` 输出为待注入的代理环境变量(纯函数供单测)。
/// 只认显式启用(XxxEnable=1)且主机/端口齐全的条目;有注入时补 no_proxy 本机段,
/// 免得经代理绕一圈去连 localhost(MCP serve 是 stdio 不走网,这是对未来的防御)。
fn parse_scutil_proxy(text: &str) -> Vec<(String, String)> {
    let get = |key: &str| -> Option<String> {
        text.lines().find_map(|l| {
            l.trim().strip_prefix(key)?.trim().strip_prefix(':').map(|v| v.trim().to_string())
        })
    };
    let enabled = |key: &str| get(key).as_deref() == Some("1");
    let mut out: Vec<(String, String)> = Vec::new();
    let mut push_pair = |lower: &str, upper: &str, host: Option<String>, port: Option<String>| {
        if let (Some(h), Some(p)) = (host, port) {
            let url = format!("http://{h}:{p}");
            out.push((lower.into(), url.clone()));
            out.push((upper.into(), url));
        }
    };
    if enabled("HTTPEnable") {
        push_pair("http_proxy", "HTTP_PROXY", get("HTTPProxy"), get("HTTPPort"));
    }
    if enabled("HTTPSEnable") {
        push_pair("https_proxy", "HTTPS_PROXY", get("HTTPSProxy"), get("HTTPSPort"));
    }
    if !out.is_empty() {
        out.push(("no_proxy".into(), "localhost,127.0.0.1".into()));
        out.push(("NO_PROXY".into(), "localhost,127.0.0.1".into()));
    }
    out
}

/// spawn + 限时等待。stdout/stderr 重定向到 scratch 下的文件——用管道的话,子进程
/// 输出超过管道缓冲而这边只在轮询 try_wait 不读管道,会互相卡死。超时 kill 判失败。
/// 返回 (exit_ok, stdout, stderr尾部)。
fn run_with_timeout(mut cmd: Command, scratch: &Path, timeout_s: u64) -> anyhow::Result<(bool, String, String)> {
    for (k, v) in proxy_env_to_inject() {
        cmd.env(k, v);
    }
    let out_path = scratch.join("agent-stdout.log");
    let err_path = scratch.join("agent-stderr.log");
    let child_out = std::fs::File::create(&out_path)?;
    let child_err = std::fs::File::create(&err_path)?;
    let mut child = cmd
        .stdin(Stdio::null())
        .stdout(Stdio::from(child_out))
        .stderr(Stdio::from(child_err))
        .spawn()?;
    let started = std::time::Instant::now();
    let status = loop {
        if let Some(st) = child.try_wait()? {
            break st;
        }
        if started.elapsed().as_secs() >= timeout_s {
            let _ = child.kill();
            let _ = child.wait();
            anyhow::bail!("Agent 进程超时({timeout_s}s),已杀");
        }
        std::thread::sleep(std::time::Duration::from_millis(300));
    };
    let stdout = std::fs::read_to_string(&out_path).unwrap_or_default();
    let stderr = std::fs::read_to_string(&err_path).unwrap_or_default();
    let err_tail: String = stderr.lines().rev().take(8).collect::<Vec<_>>().into_iter().rev().collect::<Vec<_>>().join("\n");
    Ok((status.success(), stdout, err_tail))
}

/// 一次性工作区:系统临时目录下按 pid+序号唯一。调用方负责(尽力)清理。
fn make_scratch(tag: &str) -> anyhow::Result<PathBuf> {
    static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let n = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!("vn-agent-{}-{}-{}", std::process::id(), n, tag));
    std::fs::create_dir_all(&dir)?;
    Ok(dir)
}

/// Agent Aing 主入口。前置:管线刚整写过 refined.json(stages.llm=="off")。
/// 成功判据(与 Agent 输出无关):跑完后盘上 refined.json 可读且 stages.llm=="done"、
/// 段落数不变(写回工具本就不可能改段落数,这里是对「Agent 绕开工具直写文件」的兜底)。
/// log=Some 时整轮调用(命令行+提示词+stdout/stderr+以盘上判定的结果)记入 AI 日志。
pub fn run_refine(
    note_dir: &Path,
    note_id: &str,
    kind: AgentKind,
    bin: &Path,
    model: &str,
    log: Option<&crate::ailog::Ctx>,
) -> anyhow::Result<()> {
    let before = load_refined(note_dir)
        .ok_or_else(|| anyhow::anyhow!("盘上没有可 Aing 的 refined.json(应先跑本地两段)"))?;
    anyhow::ensure!(before.stages.llm != "done", "refined.json 的 llm 阶段已是 done,无法用盘上终态判定本轮成败");
    let scratch = make_scratch(note_id)?;
    let prompt = refine_prompt(note_id);
    let started = std::time::Instant::now();
    let result = (|| -> anyhow::Result<(Vec<String>, bool, String, String)> {
        let exe = self_exe()?;
        let cmd = refine_command(kind, bin, model, &prompt, &exe, &scratch)?;
        let args: Vec<String> = cmd.get_args().map(|a| a.to_string_lossy().into_owned()).collect();
        let (exit_ok, stdout, err_tail) = run_with_timeout(cmd, &scratch, REFINE_TIMEOUT_S)?;
        Ok((args, exit_ok, stdout, err_tail))
    })();
    let verdict: anyhow::Result<()> = match &result {
        Err(e) => Err(anyhow::anyhow!("{e}")),
        Ok((_, exit_ok, _, err_tail)) => (|| {
            let after =
                load_refined(note_dir).ok_or_else(|| anyhow::anyhow!("跑完后 refined.json 不可读"))?;
            anyhow::ensure!(
                after.paragraphs.len() == before.paragraphs.len(),
                "段落数改变({} → {}),疑似绕开写回工具,判失败",
                before.paragraphs.len(),
                after.paragraphs.len()
            );
            anyhow::ensure!(
                after.stages.llm == "done",
                "Agent 未完成写回(exit_ok={exit_ok},盘上 llm={});stderr 尾部:\n{err_tail}",
                after.stages.llm
            );
            Ok(())
        })(),
    };
    if let Some(ctx) = log {
        let response = match &result {
            Ok((_, exit_ok, stdout, err_tail)) => serde_json::json!({
                "exit_ok": exit_ok, "stdout": stdout, "stderr_tail": err_tail,
            }),
            Err(_) => serde_json::Value::Null,
        };
        crate::ailog::record(
            ctx,
            crate::ailog::Draft {
                kind: "agent_refine",
                provider: kind.key().into(),
                model: Some(model.to_string()).filter(|m| !m.is_empty()),
                endpoint: Some(bin.display().to_string()),
                request: serde_json::json!({
                    "args": result.as_ref().map(|(args, ..)| args.clone()).unwrap_or_default(),
                    "prompt": prompt,
                }),
                response,
                status: if verdict.is_ok() { "ok" } else { "error" },
                error: verdict.as_ref().err().map(|e| e.to_string()),
                duration_ms: started.elapsed().as_millis() as u64,
            },
        );
    }
    let _ = std::fs::remove_dir_all(&scratch);
    verdict
}

/// 为整场笔记生成主题标题(语义与 llm::gen_title 一致:锦上添花,失败即放弃)。
pub fn gen_title(
    kind: AgentKind,
    bin: &Path,
    model: &str,
    paragraphs: &[RefinedParagraph],
    log: Option<&crate::ailog::Ctx>,
) -> anyhow::Result<String> {
    let mut text = String::new();
    for p in paragraphs {
        if text.chars().count() > 1500 {
            break;
        }
        text.push_str(&p.text);
        text.push('\n');
    }
    if text.trim().is_empty() {
        anyhow::bail!("Aing 稿无内容,不生成标题");
    }
    let prompt = format!(
        "只输出一个不超过 12 个字的中文标题,概括下面这场对话的核心主题;不要引号、标点或任何解释。\n\n{text}"
    );
    let scratch = make_scratch("title")?;
    let started = std::time::Instant::now();
    let run = (|| -> anyhow::Result<(bool, String, String)> {
        let cmd = title_command(kind, bin, model, &prompt, &scratch);
        run_with_timeout(cmd, &scratch, TITLE_TIMEOUT_S)
    })();
    let result: anyhow::Result<String> = match &run {
        Err(e) => Err(anyhow::anyhow!("{e}")),
        Ok((exit_ok, stdout, err_tail)) => (|| {
            anyhow::ensure!(*exit_ok, "标题进程退出非 0;stderr 尾部:\n{err_tail}");
            extract_title(stdout)
        })(),
    };
    if let Some(ctx) = log {
        let response = match &run {
            Ok((exit_ok, stdout, err_tail)) => serde_json::json!({
                "exit_ok": exit_ok, "stdout": stdout, "stderr_tail": err_tail,
            }),
            Err(_) => serde_json::Value::Null,
        };
        crate::ailog::record(
            ctx,
            crate::ailog::Draft {
                kind: "title",
                provider: kind.key().into(),
                model: Some(model.to_string()).filter(|m| !m.is_empty()),
                endpoint: Some(bin.display().to_string()),
                request: serde_json::json!({ "prompt": prompt }),
                response,
                status: if result.is_ok() { "ok" } else { "error" },
                error: result.as_ref().err().map(|e| e.to_string()),
                duration_ms: started.elapsed().as_millis() as u64,
            },
        );
    }
    let _ = std::fs::remove_dir_all(&scratch);
    result
}

/// stdout 最后一个非空行 → 去引号 → 长度守卫(与 llm::gen_title 同一守卫:空、
/// 超长、含换行都视为不服从指令,放弃)。
fn extract_title(stdout: &str) -> anyhow::Result<String> {
    let last = stdout.lines().rev().find(|l| !l.trim().is_empty()).unwrap_or("");
    let title = last.trim().trim_matches(['"', '“', '”', '「', '」', '《', '》', '。']).trim().to_string();
    if title.is_empty() || title.chars().count() > 20 {
        anyhow::bail!("标题不合规,放弃: {title:?}");
    }
    Ok(title)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::{write_refined_atomic, RefineStages, RefinedDoc};

    fn doc(llm: &str, texts: &[&str]) -> RefinedDoc {
        RefinedDoc {
            schema_version: 1,
            generated_at: "t".into(),
            llm_model: None,
            stages: RefineStages { filter: "done".into(), recluster: "done".into(), llm: llm.into() },
            discarded_seqs: vec![],
            paragraphs: texts
                .iter()
                .map(|t| RefinedParagraph {
                    speaker: "R1".into(),
                    name: None,
                    person_id: None,
                    start_ms: 0,
                    end_ms: 1000,
                    text: (*t).into(),
                    source_seqs: vec![0],
                })
                .collect(),
        }
    }

    /// 写一个假 Agent 可执行脚本。body 里可用 $1..(prompt 等参数),测试把要写的
    /// 目标路径直接烤进脚本文本。
    fn fake_bin(dir: &Path, body: &str) -> PathBuf {
        let p = dir.join("fake-agent");
        std::fs::write(&p, format!("#!/bin/sh\n{body}\n")).unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
        p
    }

    #[test]
    fn kind_key_roundtrip_and_bin_names() {
        for k in [AgentKind::Claude, AgentKind::Codex, AgentKind::Gemini, AgentKind::Cursor] {
            assert_eq!(AgentKind::from_key(k.key()), Some(k));
        }
        assert_eq!(AgentKind::from_key("bogus"), None);
        assert_eq!(AgentKind::Cursor.bin_name(), "cursor-agent", "Cursor 的 CLI 是 cursor-agent");
    }

    #[test]
    fn resolve_bin_override_must_exist_and_never_falls_back() {
        let tmp = tempfile::tempdir().unwrap();
        let missing = tmp.path().join("no-such-bin");
        assert!(resolve_bin(AgentKind::Claude, missing.to_str().unwrap()).is_none(), "显式路径不存在不得回退探测");
        let bin = fake_bin(tmp.path(), "exit 0");
        assert_eq!(resolve_bin(AgentKind::Claude, bin.to_str().unwrap()), Some(bin));
    }

    #[test]
    fn refine_command_claude_has_strict_mcp_and_allowlist() {
        let tmp = tempfile::tempdir().unwrap();
        let cmd = refine_command(
            AgentKind::Claude,
            Path::new("/bin/echo"),
            "haiku",
            "P",
            Path::new("/app/voice-notes"),
            tmp.path(),
        )
        .unwrap();
        let args: Vec<String> = cmd.get_args().map(|a| a.to_string_lossy().into_owned()).collect();
        assert!(args.contains(&"--strict-mcp-config".to_string()), "必须隔离用户全局 MCP 配置: {args:?}");
        assert!(args.iter().any(|a| a.contains("mcp__voice-notes__get_note")), "白名单缺 get_note");
        assert!(args.iter().any(|a| a.contains("apply_refined_texts")), "白名单缺写回工具");
        assert!(args.iter().any(|a| a.contains("\"mcpServers\"")), "缺内联 mcp-config");
        assert!(args.contains(&"haiku".to_string()));
    }

    #[test]
    fn refine_command_gemini_and_cursor_write_workspace_configs() {
        let tmp = tempfile::tempdir().unwrap();
        let _ = refine_command(AgentKind::Gemini, Path::new("/bin/echo"), "", "P", Path::new("/app/vn"), tmp.path()).unwrap();
        let gemini: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(tmp.path().join(".gemini/settings.json")).unwrap()).unwrap();
        assert_eq!(gemini["mcpServers"]["voice-notes"]["command"], "/app/vn");
        assert_eq!(gemini["coreTools"], serde_json::json!([]), "内置工具必须全关,yolo 才收得住面");
        let tmp2 = tempfile::tempdir().unwrap();
        let cmd = refine_command(AgentKind::Cursor, Path::new("/bin/echo"), "", "P", Path::new("/app/vn"), tmp2.path()).unwrap();
        let cursor: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(tmp2.path().join(".cursor/mcp.json")).unwrap()).unwrap();
        assert_eq!(cursor["mcpServers"]["voice-notes"]["args"][0], "mcp");
        let args: Vec<String> = cmd.get_args().map(|a| a.to_string_lossy().into_owned()).collect();
        assert!(!args.contains(&"--force".to_string()), "不得放行 shell 命令");
    }

    #[test]
    fn run_refine_trusts_disk_not_agent_exit_code() {
        // Agent 退出 0 但没写回 → 失败;假 Agent 把 llm 置 done → 成功。
        let note = tempfile::tempdir().unwrap();
        write_refined_atomic(note.path(), &doc("off", &["原文"])).unwrap();
        let bins = tempfile::tempdir().unwrap();
        let lazy = fake_bin(bins.path(), "exit 0");
        let err = run_refine(note.path(), "n1", AgentKind::Claude, &lazy, "", None).unwrap_err().to_string();
        assert!(err.contains("未完成写回"), "退出 0 不算成功: {err}");

        // 烤进真实路径的假 Agent:模拟经写回工具完成(llm→done)。带 AI 日志上下文,
        // 顺带断言整轮调用(命令行+提示词+盘上判定结果)全量留痕。
        let refined_path = note.path().join("refined.json");
        let done_json = serde_json::to_string(&doc("done", &["修订"])).unwrap();
        let diligent = fake_bin(
            bins.path(),
            &format!("cat > {} <<'EOF'\n{}\nEOF\nexit 0", refined_path.display(), done_json),
        );
        let logs = tempfile::tempdir().unwrap();
        let ctx = crate::ailog::Ctx { data_root: logs.path().to_path_buf(), note_id: "n1".into() };
        run_refine(note.path(), "n1", AgentKind::Claude, &diligent, "", Some(&ctx)).unwrap();
        let v = crate::ailog::query(logs.path(), &crate::ailog::Filter::default());
        assert_eq!(v["total"], 1);
        let e = &v["entries"][0];
        assert_eq!(e["kind"], "agent_refine");
        assert_eq!(e["provider"], "claude");
        assert_eq!(e["status"], "ok");
        assert!(e["request"]["prompt"].as_str().unwrap().contains("apply_refined_texts"), "提示词全量");
        assert!(e["request"]["args"].as_array().unwrap().iter().any(|a| a == "--strict-mcp-config"), "命令行全量");
        assert_eq!(e["response"]["exit_ok"], true);
    }

    #[test]
    fn run_refine_rejects_paragraph_count_change_and_requires_baseline() {
        let note = tempfile::tempdir().unwrap();
        // 无基线 refined.json → 拒绝
        assert!(run_refine(note.path(), "n1", AgentKind::Claude, Path::new("/bin/true"), "", None).is_err());
        // llm 已是 done → 拒绝(无法判定本轮)
        write_refined_atomic(note.path(), &doc("done", &["a"])).unwrap();
        assert!(run_refine(note.path(), "n1", AgentKind::Claude, Path::new("/bin/true"), "", None).is_err());
        // 段落数被改 → 判失败
        write_refined_atomic(note.path(), &doc("off", &["a", "b"])).unwrap();
        let bins = tempfile::tempdir().unwrap();
        let mutant_json = serde_json::to_string(&doc("done", &["只剩一段"])).unwrap();
        let mutant = fake_bin(
            bins.path(),
            &format!("cat > {} <<'EOF'\n{}\nEOF\nexit 0", note.path().join("refined.json").display(), mutant_json),
        );
        let err = run_refine(note.path(), "n1", AgentKind::Claude, &mutant, "", None).unwrap_err().to_string();
        assert!(err.contains("段落数"), "{err}");
    }

    #[test]
    fn extract_title_takes_last_line_and_guards_length() {
        assert_eq!(extract_title("日志横幅\n\n「产品评审」\n").unwrap(), "产品评审");
        assert!(extract_title("").is_err());
        assert!(extract_title(&"字".repeat(40)).is_err(), "超长拒绝");
    }

    #[test]
    fn gen_title_via_fake_agent_and_empty_doc_bails() {
        let bins = tempfile::tempdir().unwrap();
        let bin = fake_bin(bins.path(), "echo 发布计划评审");
        let ps = doc("done", &["讨论了发布计划。"]).paragraphs;
        assert_eq!(gen_title(AgentKind::Claude, &bin, "", &ps, None).unwrap(), "发布计划评审");
        assert!(gen_title(AgentKind::Claude, &bin, "", &[], None).is_err(), "空稿不发起");
    }

    #[test]
    fn parse_scutil_proxy_extracts_enabled_entries_only() {
        let real = "<dictionary> {\n  ExceptionsList : <array> {\n    0 : 127.0.0.1\n  }\n  HTTPEnable : 1\n  HTTPPort : 7890\n  HTTPProxy : 127.0.0.1\n  HTTPSEnable : 1\n  HTTPSPort : 7890\n  HTTPSProxy : 127.0.0.1\n  ProxyAutoConfigEnable : 0\n  SOCKSEnable : 1\n  SOCKSPort : 7890\n  SOCKSProxy : 127.0.0.1\n}";
        let pairs = parse_scutil_proxy(real);
        let get = |k: &str| pairs.iter().find(|(key, _)| key == k).map(|(_, v)| v.as_str());
        assert_eq!(get("http_proxy"), Some("http://127.0.0.1:7890"));
        assert_eq!(get("HTTPS_PROXY"), Some("http://127.0.0.1:7890"));
        assert_eq!(get("no_proxy"), Some("localhost,127.0.0.1"), "有注入必带本机豁免");
        assert_eq!(pairs.len(), 6, "http/https 大小写各一 + no_proxy 两份");

        // 系统代理关闭 → 不注入任何东西
        let off = "<dictionary> {\n  HTTPEnable : 0\n  HTTPSEnable : 0\n}";
        assert!(parse_scutil_proxy(off).is_empty());
        // 启用但缺端口 → 跳过该条目,不造出残缺 URL
        let broken = "<dictionary> {\n  HTTPEnable : 1\n  HTTPProxy : 127.0.0.1\n}";
        assert!(parse_scutil_proxy(broken).is_empty());
    }

    #[test]
    fn probe_run_unknown_provider_errs() {
        let e = super::probe_run("nope", "", "").unwrap_err();
        assert!(e.contains("未知 Agent"), "得到: {e}");
    }

    #[test]
    fn probe_run_missing_bin_errs() {
        // override 指向不存在路径 → resolve_bin 返回 None,不 spawn 任何进程。
        let e = super::probe_run("claude", "/definitely/not/here/claude", "").unwrap_err();
        assert!(e.contains("未找到"), "得到: {e}");
    }

    #[test]
    fn probe_all_covers_four_agents() {
        let keys: Vec<&str> = probe_all().into_iter().map(|(k, _)| k).collect();
        assert_eq!(keys, vec!["claude", "codex", "gemini", "cursor"]);
    }

    /// 真机 e2e(默认 ignore):真 claude 走完整 run_refine 链路——spawn → 自家
    /// mcp serve → get_note → apply_refined_texts 写回 → 盘上校验,并断言 AI 日志
    /// 两侧留痕(本进程 agent_refine + serve 子进程 mcp_apply)。
    /// 运行:VN_SELF_EXE=<voice-notes 二进制绝对路径> cargo test --lib \
    ///       e2e_claude_refine -- --ignored --nocapture
    /// 依赖:claude CLI 已装已登录;消耗少量订阅额度(haiku,一次 Aing)。
    #[test]
    #[ignore]
    fn e2e_claude_refine_leaves_full_ailog_trail() {
        let Ok(self_exe) = std::env::var("VN_SELF_EXE") else {
            eprintln!("跳过:未设 VN_SELF_EXE(需指向 voice-notes 二进制)");
            return;
        };
        assert!(Path::new(&self_exe).is_file(), "VN_SELF_EXE 不存在: {self_exe}");
        let Some(bin) = resolve_bin(AgentKind::Claude, "") else {
            eprintln!("跳过:本机未检测到 claude CLI");
            return;
        };
        let _guard =
            crate::mcp::ENV_VAR_LOCK.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let tmp = tempfile::tempdir().unwrap();
        std::env::set_var("VN_APP_DATA", tmp.path()); // serve 子进程经环境继承同一数据根
        let note_id = "20260712-090000";
        let note_dir = tmp.path().join("notes").join(note_id);
        std::fs::create_dir_all(&note_dir).unwrap();
        std::fs::write(
            note_dir.join("meta.json"),
            serde_json::json!({
                "schema_version": 1, "id": note_id, "title": "e2e 会议",
                "started_at": "2026-07-12T09:00:00+08:00",
                "ended_at": "2026-07-12T09:10:00+08:00", "state": "complete"
            })
            .to_string(),
        )
        .unwrap();
        std::fs::write(
            note_dir.join("segments.jsonl"),
            r#"{"seq":0,"source":"mic","text":"我们肯计要在下周发布新版本","start_ms":0,"end_ms":4000,"speaker":"S1"}"#
                .to_string()
                + "\n",
        )
        .unwrap();
        std::fs::write(
            note_dir.join("speakers.json"),
            serde_json::json!({ "S1": { "name": "张三", "sources": ["mic"], "count": 1 } }).to_string(),
        )
        .unwrap();
        write_refined_atomic(
            &note_dir,
            &doc("off", &["我们肯计要在下周发布新版本", "嗯嗯这个这个方案我觉得可以", "用claude code来做 Aing 没问题"]),
        )
        .unwrap();

        let ctx = crate::ailog::Ctx { data_root: tmp.path().to_path_buf(), note_id: note_id.into() };
        run_refine(&note_dir, note_id, AgentKind::Claude, &bin, "haiku", Some(&ctx))
            .expect("真 claude Aing 应成功(需已登录)");

        let after = load_refined(&note_dir).unwrap();
        assert_eq!(after.stages.llm, "done");
        assert_eq!(after.paragraphs.len(), 3);
        assert!(!after.paragraphs[0].text.contains("肯计"), "错字应被纠正: {}", after.paragraphs[0].text);

        // 日志两侧齐:本进程的 agent_refine + serve 子进程的 mcp_apply。
        let v = crate::ailog::query(tmp.path(), &crate::ailog::Filter::default());
        eprintln!("=== AI 日志({} 条)===", v["total"]);
        for e in v["entries"].as_array().unwrap() {
            eprintln!("{}", serde_json::to_string_pretty(e).unwrap());
        }
        let entries = v["entries"].as_array().unwrap();
        let agent = entries.iter().find(|e| e["kind"] == "agent_refine").expect("缺 agent_refine 条目");
        assert_eq!(agent["status"], "ok");
        assert_eq!(agent["provider"], "claude");
        assert!(agent["request"]["prompt"].as_str().unwrap().contains(note_id));
        assert!(agent["response"]["exit_ok"].as_bool().unwrap());
        let apply = entries.iter().find(|e| e["kind"] == "mcp_apply").expect("缺 mcp_apply 条目(serve 子进程写)");
        assert_eq!(apply["status"], "ok");
        assert!(!apply["request"]["updates"].as_array().unwrap().is_empty(), "写回应含逐段修订全文");
        std::env::remove_var("VN_APP_DATA");
    }
}
