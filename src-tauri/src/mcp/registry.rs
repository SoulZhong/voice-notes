//! 本机 Agent 的 MCP 注册器:检测安装、把 voice-notes 条目写进各家配置。
//! 原则:只动自己的键(voice-notes),解析失败拒写,写前备份,幂等。

use serde::Serialize;
use std::path::{Path, PathBuf};

/// 配置文件格式。JSON 家族统一顶层键 "mcpServers";Codex 是 TOML 的 [mcp_servers.*]。
#[derive(Clone, Copy, PartialEq)]
pub enum Fmt {
    Json,
    Toml,
}

pub struct AgentDef {
    pub key: &'static str,
    pub name: &'static str,
    /// 相对 $HOME 的安装检测路径(目录或文件,存在即视为已安装)。
    detect_rel: &'static str,
    /// 相对 $HOME 的配置文件路径。
    config_rel: &'static str,
    pub fmt: Fmt,
}

/// 内置支持的五家(已拍板:第二梯队不内置,靠设置页手动配置卡片)。
pub const AGENTS: &[AgentDef] = &[
    AgentDef { key: "claude-code", name: "Claude Code", detect_rel: ".claude", config_rel: ".claude.json", fmt: Fmt::Json },
    AgentDef { key: "claude-desktop", name: "Claude Desktop", detect_rel: "Library/Application Support/Claude", config_rel: "Library/Application Support/Claude/claude_desktop_config.json", fmt: Fmt::Json },
    AgentDef { key: "cursor", name: "Cursor", detect_rel: ".cursor", config_rel: ".cursor/mcp.json", fmt: Fmt::Json },
    AgentDef { key: "codex", name: "Codex CLI", detect_rel: ".codex", config_rel: ".codex/config.toml", fmt: Fmt::Toml },
    AgentDef { key: "gemini", name: "Gemini CLI", detect_rel: ".gemini", config_rel: ".gemini/settings.json", fmt: Fmt::Json },
];

#[derive(Debug, Clone, Serialize)]
pub struct AgentStatus {
    pub key: String,
    pub name: String,
    pub installed: bool,
    pub registered: bool,
    /// 已注册条目里的 command(未注册为 None)。
    pub command: Option<String>,
    /// 已注册但 command ≠ 当前二进制(App 被移动/换装过)。
    pub stale: bool,
}

/// home/exe 显式注入:生产走 new()(真 $HOME + current_exe),测试注入 tempdir。
pub struct Registry {
    home: PathBuf,
    exe: PathBuf,
}

impl Registry {
    pub fn new() -> anyhow::Result<Self> {
        let home = std::env::var("HOME").map_err(|_| anyhow::anyhow!("HOME 不可用"))?;
        let exe = std::env::current_exe()?.canonicalize()?;
        Ok(Self::with(PathBuf::from(home), exe))
    }

    pub fn with(home: PathBuf, exe: PathBuf) -> Self {
        Self { home, exe }
    }

    fn def(key: &str) -> anyhow::Result<&'static AgentDef> {
        AGENTS.iter().find(|a| a.key == key).ok_or_else(|| anyhow::anyhow!("未知 Agent: {key}"))
    }

    fn config_path(&self, def: &AgentDef) -> PathBuf {
        self.home.join(def.config_rel)
    }

    /// 手动配置卡片/README 用的 JSON 片段(command 为本机真实路径)。
    pub fn entry_snippet_json(&self) -> String {
        serde_json::to_string_pretty(&serde_json::json!({
            "voice-notes": { "command": self.exe.to_string_lossy(), "args": ["mcp", "serve"] }
        }))
        .expect("静态结构序列化不会失败")
    }

    pub fn status(&self) -> Vec<AgentStatus> {
        AGENTS.iter().map(|d| self.status_one(d)).collect()
    }

    fn status_one(&self, def: &AgentDef) -> AgentStatus {
        let installed = self.home.join(def.detect_rel).exists();
        let command = self.read_command(def);
        let registered = command.is_some();
        let stale = command.as_deref().map(|c| Path::new(c) != self.exe.as_path()).unwrap_or(false);
        AgentStatus {
            key: def.key.into(),
            name: def.name.into(),
            installed,
            registered,
            command,
            stale,
        }
    }

    /// 读已注册条目的 command;未注册/文件缺失/解析失败一律 None(status 是只读探测,不报错)。
    fn read_command(&self, def: &AgentDef) -> Option<String> {
        let text = std::fs::read_to_string(self.config_path(def)).ok()?;
        match def.fmt {
            Fmt::Json => {
                let v: serde_json::Value = serde_json::from_str(&text).ok()?;
                Some(v.get("mcpServers")?.get("voice-notes")?.get("command")?.as_str()?.to_string())
            }
            Fmt::Toml => {
                let doc: toml_edit::DocumentMut = text.parse().ok()?;
                Some(doc.get("mcp_servers")?.get("voice-notes")?.get("command")?.as_str()?.to_string())
            }
        }
    }

    pub fn register(&self, key: &str) -> anyhow::Result<()> {
        let def = Self::def(key)?;
        let path = self.config_path(def);
        match def.fmt {
            Fmt::Json => self.upsert_json(&path),
            Fmt::Toml => self.upsert_toml(&path),
        }
    }

    pub fn unregister(&self, key: &str) -> anyhow::Result<()> {
        let def = Self::def(key)?;
        let path = self.config_path(def);
        if !path.exists() {
            return Ok(()); // 幂等:没有配置文件自然没有条目
        }
        match def.fmt {
            Fmt::Json => self.remove_json(&path),
            Fmt::Toml => self.remove_toml(&path),
        }
    }

    /// 修复 stale 注册(App 被移动/重装后 command 指向旧路径):重写为当前 exe。
    /// 开发态二进制(路径含 /target/)跳过——否则开发机会把用户配置指向 debug 构建。
    pub fn heal(&self) -> anyhow::Result<u32> {
        if self.exe.components().any(|c| c.as_os_str() == "target") {
            return Ok(0);
        }
        let mut healed = 0u32;
        for st in self.status() {
            if st.registered && st.stale {
                // register 即覆盖式 upsert,天然就是"改正"。单家失败不挡其余家。
                if self.register(&st.key).is_ok() {
                    healed += 1;
                }
            }
        }
        Ok(healed)
    }

    /// exe 带 com.apple.quarantine 时的提示(未签名 App 被 Agent spawn 会失败)。
    /// 纯提示不阻断;xattr 不存在/查询失败按无隔离处理。
    pub fn quarantine_warning(&self) -> Option<String> {
        let out = std::process::Command::new("/usr/bin/xattr")
            .arg("-p")
            .arg("com.apple.quarantine")
            .arg(&self.exe)
            .output()
            .ok()?;
        if out.status.success() {
            Some(format!(
                "警告: {} 带 com.apple.quarantine 隔离标记,Agent 可能无法启动它。\n请执行: xattr -dr com.apple.quarantine /Applications/voice-notes.app",
                self.exe.display()
            ))
        } else {
            None
        }
    }

    fn upsert_json(&self, path: &Path) -> anyhow::Result<()> {
        let mut root: serde_json::Value = match std::fs::read_to_string(path) {
            Ok(text) if !text.trim().is_empty() => serde_json::from_str(&text).map_err(|e| {
                anyhow::anyhow!("{} 不是合法 JSON,拒绝写入(请手动修复或手动配置): {e}", path.display())
            })?,
            _ => serde_json::json!({}),
        };
        let obj = root.as_object_mut().ok_or_else(|| anyhow::anyhow!("{} 顶层不是对象,拒绝写入", path.display()))?;
        let servers = obj.entry("mcpServers").or_insert_with(|| serde_json::json!({}));
        let servers = servers
            .as_object_mut()
            .ok_or_else(|| anyhow::anyhow!("{} 的 mcpServers 不是对象,拒绝写入", path.display()))?;
        servers.insert(
            "voice-notes".into(),
            serde_json::json!({ "command": self.exe.to_string_lossy(), "args": ["mcp", "serve"] }),
        );
        write_with_backup(path, &(serde_json::to_string_pretty(&root)? + "\n"))
    }

    fn remove_json(&self, path: &Path) -> anyhow::Result<()> {
        let text = std::fs::read_to_string(path)?;
        let mut root: serde_json::Value = serde_json::from_str(&text)
            .map_err(|e| anyhow::anyhow!("{} 不是合法 JSON,拒绝写入: {e}", path.display()))?;
        let Some(servers) = root.get_mut("mcpServers").and_then(|v| v.as_object_mut()) else {
            return Ok(());
        };
        if servers.remove("voice-notes").is_none() {
            return Ok(()); // 本就没有:不产生写入(也就不产生备份)
        }
        write_with_backup(path, &(serde_json::to_string_pretty(&root)? + "\n"))
    }

    fn upsert_toml(&self, path: &Path) -> anyhow::Result<()> {
        let mut doc: toml_edit::DocumentMut = match std::fs::read_to_string(path) {
            Ok(text) => text.parse().map_err(|e| {
                anyhow::anyhow!("{} 不是合法 TOML,拒绝写入(请手动修复或手动配置): {e}", path.display())
            })?,
            Err(_) => toml_edit::DocumentMut::new(),
        };
        let mut args = toml_edit::Array::new();
        args.push("mcp");
        args.push("serve");
        let servers = doc.entry("mcp_servers").or_insert(toml_edit::Item::Table(toml_edit::Table::new()));
        let servers = servers.as_table_mut().ok_or_else(|| anyhow::anyhow!("{} 的 mcp_servers 不是表,拒绝写入", path.display()))?;
        servers.set_implicit(true);
        let mut entry = toml_edit::Table::new();
        entry["command"] = toml_edit::value(self.exe.to_string_lossy().as_ref());
        entry["args"] = toml_edit::value(args);
        servers.insert("voice-notes", toml_edit::Item::Table(entry));
        write_with_backup(path, &doc.to_string())
    }

    fn remove_toml(&self, path: &Path) -> anyhow::Result<()> {
        let text = std::fs::read_to_string(path)?;
        let mut doc: toml_edit::DocumentMut = text
            .parse()
            .map_err(|e| anyhow::anyhow!("{} 不是合法 TOML,拒绝写入: {e}", path.display()))?;
        let removed = doc
            .get_mut("mcp_servers")
            .and_then(|t| t.as_table_mut())
            .map(|t| t.remove("voice-notes").is_some())
            .unwrap_or(false);
        if !removed {
            return Ok(());
        }
        write_with_backup(path, &doc.to_string())
    }
}

/// 写前把现有文件备份为 `<file>.vn.bak`(覆盖旧备份),再 tmp+rename 原子写。
/// 父目录不存在则创建(如 .cursor/mcp.json 首次注册)。
fn write_with_backup(path: &Path, content: &str) -> anyhow::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    if path.exists() {
        let mut bak = path.as_os_str().to_owned();
        bak.push(".vn.bak");
        std::fs::copy(path, PathBuf::from(&bak))?;
    }
    let mut tmp = path.as_os_str().to_owned();
    tmp.push(".vn.tmp");
    let tmp = PathBuf::from(tmp);
    std::fs::write(&tmp, content)?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn reg(home: &Path) -> Registry {
        Registry::with(home.to_path_buf(), PathBuf::from("/Applications/voice-notes.app/Contents/MacOS/voice-notes"))
    }

    #[test]
    fn detects_installed_by_path_presence() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join(".cursor")).unwrap();
        let st = reg(tmp.path()).status();
        let cursor = st.iter().find(|s| s.key == "cursor").unwrap();
        assert!(cursor.installed && !cursor.registered);
        let gemini = st.iter().find(|s| s.key == "gemini").unwrap();
        assert!(!gemini.installed);
    }

    #[test]
    fn register_creates_minimal_json_and_is_idempotent() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join(".cursor")).unwrap();
        let r = reg(tmp.path());
        r.register("cursor").unwrap();
        r.register("cursor").unwrap(); // 幂等:重复注册 = 覆盖
        let v: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(tmp.path().join(".cursor/mcp.json")).unwrap()).unwrap();
        assert_eq!(v["mcpServers"]["voice-notes"]["command"], "/Applications/voice-notes.app/Contents/MacOS/voice-notes");
        assert_eq!(v["mcpServers"]["voice-notes"]["args"], serde_json::json!(["mcp", "serve"]));
        let st = r.status();
        let cursor = st.iter().find(|s| s.key == "cursor").unwrap();
        assert!(cursor.registered && !cursor.stale);
        assert!(tmp.path().join(".cursor/mcp.json.vn.bak").exists(), "二次写入前留了备份");
    }

    #[test]
    fn register_preserves_unrelated_keys() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(
            tmp.path().join(".claude.json"),
            r#"{"theme":"dark","mcpServers":{"other":{"command":"/bin/x"}}}"#,
        )
        .unwrap();
        let r = reg(tmp.path());
        r.register("claude-code").unwrap();
        let v: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(tmp.path().join(".claude.json")).unwrap()).unwrap();
        assert_eq!(v["theme"], "dark", "无关顶层键保留");
        assert_eq!(v["mcpServers"]["other"]["command"], "/bin/x", "别人的 server 条目保留");
        assert!(v["mcpServers"]["voice-notes"].is_object());
    }

    #[test]
    fn corrupt_json_is_rejected_not_overwritten() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join(".claude.json"), "{oops").unwrap();
        let r = reg(tmp.path());
        assert!(r.register("claude-code").is_err(), "坏文件必须拒写");
        assert_eq!(std::fs::read_to_string(tmp.path().join(".claude.json")).unwrap(), "{oops", "原文件原封不动");
    }

    #[test]
    fn unregister_removes_only_own_entry_and_is_idempotent() {
        let tmp = tempfile::tempdir().unwrap();
        let r = reg(tmp.path());
        r.register("cursor").unwrap();
        r.unregister("cursor").unwrap();
        r.unregister("cursor").unwrap(); // 不存在时静默成功
        let v: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(tmp.path().join(".cursor/mcp.json")).unwrap()).unwrap();
        assert!(v["mcpServers"].get("voice-notes").is_none());
    }

    #[test]
    fn stale_when_command_differs_from_exe() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(
            tmp.path().join(".claude.json"),
            r#"{"mcpServers":{"voice-notes":{"command":"/old/path/voice-notes","args":["mcp","serve"]}}}"#,
        )
        .unwrap();
        let st = reg(tmp.path()).status();
        let cc = st.iter().find(|s| s.key == "claude-code").unwrap();
        assert!(cc.registered && cc.stale);
        assert_eq!(cc.command.as_deref(), Some("/old/path/voice-notes"));
    }

    #[test]
    fn codex_toml_roundtrip_preserves_comments() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join(".codex")).unwrap();
        std::fs::write(
            tmp.path().join(".codex/config.toml"),
            "# 用户自己的注释\nmodel = \"o3\"\n\n[mcp_servers.other]\ncommand = \"/bin/x\"\n",
        )
        .unwrap();
        let r = reg(tmp.path());
        r.register("codex").unwrap();
        let text = std::fs::read_to_string(tmp.path().join(".codex/config.toml")).unwrap();
        assert!(text.contains("# 用户自己的注释"), "toml_edit 保注释:{text}");
        assert!(text.contains("model = \"o3\""));
        assert!(text.contains("[mcp_servers.other]"));
        assert!(!text.contains("[mcp_servers]\n"), "不得注入空的 [mcp_servers] 表头: {text}");
        let st = r.status();
        let codex = st.iter().find(|s| s.key == "codex").unwrap();
        assert!(codex.registered && !codex.stale, "TOML read_command 也要通:{codex:?}");
        // 注销后条目消失、其余保留
        r.unregister("codex").unwrap();
        let text = std::fs::read_to_string(tmp.path().join(".codex/config.toml")).unwrap();
        assert!(!text.contains("voice-notes"));
        assert!(text.contains("[mcp_servers.other]"));
    }

    #[test]
    fn codex_toml_created_when_missing_and_corrupt_rejected() {
        let tmp = tempfile::tempdir().unwrap();
        let r = reg(tmp.path());
        r.register("codex").unwrap(); // 文件不存在 → 创建最小结构
        let text = std::fs::read_to_string(tmp.path().join(".codex/config.toml")).unwrap();
        assert!(text.contains("[mcp_servers.voice-notes]"), "{text}");
        std::fs::write(tmp.path().join(".codex/config.toml"), "= 不是 toml =").unwrap();
        assert!(r.register("codex").is_err(), "坏 TOML 拒写");
    }

    #[test]
    fn heal_rewrites_stale_and_skips_dev_binary() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(
            tmp.path().join(".claude.json"),
            r#"{"mcpServers":{"voice-notes":{"command":"/old/voice-notes","args":["mcp","serve"]}}}"#,
        )
        .unwrap();
        // 生产二进制:自愈生效
        let r = reg(tmp.path());
        assert_eq!(r.heal().unwrap(), 1);
        assert!(!r.status().iter().find(|s| s.key == "claude-code").unwrap().stale);
        // 开发二进制(路径含 /target/):不动用户配置
        std::fs::write(
            tmp.path().join(".claude.json"),
            r#"{"mcpServers":{"voice-notes":{"command":"/old/voice-notes","args":["mcp","serve"]}}}"#,
        )
        .unwrap();
        let dev = Registry::with(tmp.path().to_path_buf(), PathBuf::from("/repo/src-tauri/target/debug/voice-notes"));
        assert_eq!(dev.heal().unwrap(), 0);
        assert!(dev.status().iter().find(|s| s.key == "claude-code").unwrap().stale, "开发态保持 stale 不改写");
    }
}
