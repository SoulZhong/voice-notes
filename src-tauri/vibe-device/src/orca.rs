//! Orca Sessions through the `orca` CLI.
//!
//! Orca's Current Conversation is the active leaf of the active tab in the
//! worktree Orca's UI has selected: `orca worktree ps` marks that worktree
//! `isActive` (`--worktree active` would mean the caller's working directory,
//! not the UI), and `orca terminal list --include-visual-layouts` gives each
//! worktree's tab and pane tree.
//!
//! Arguments are always passed as separate argv entries (never through a
//! shell) and values use `--flag=value` so text starting with `-` cannot be
//! mistaken for a flag. The process runner is a trait so tests never touch
//! real terminals.

use serde::Deserialize;
use std::io::Read;
use std::path::PathBuf;
use std::time::{Duration, Instant};

/// One live Orca-managed terminal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OrcaSession {
    pub handle: String,
    pub leaf_id: String,
    /// Orca's worktree id (`<repo-id>::<path>`).
    pub worktree_id: String,
    /// Worktree display name (last path component of the worktree path).
    pub worktree: String,
    /// Terminal title with leading status glyphs removed.
    pub title: String,
    /// Title as Orca reports it (carries the agent's state, see `alerts`).
    pub raw_title: String,
    /// Agent running in the terminal (`agentIdentity`, e.g. "claude").
    pub agent: Option<String>,
    /// Tail of the terminal output.
    pub preview: String,
    /// The agent's last reply read from the rendered screen, filled in by the
    /// Orca watch when the session just started waiting (see `alerts`).
    pub reply: Option<String>,
}

impl OrcaSession {
    /// The Target label: `<worktree> · <title>`.
    pub fn label(&self) -> String {
        match (self.worktree.is_empty(), self.title.is_empty()) {
            (false, false) => format!("{} · {}", self.worktree, self.title),
            (false, true) => self.worktree.clone(),
            (true, false) => self.title.clone(),
            (true, true) => self.handle.clone(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OrcaError {
    /// CLI missing, Orca not running, or unparseable reply.
    Unavailable(String),
    /// The terminal handle no longer exists.
    Stale,
    /// The CLI reported another error.
    Failed(String),
}

impl std::fmt::Display for OrcaError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            OrcaError::Unavailable(m) => write!(f, "Orca unavailable: {m}"),
            OrcaError::Stale => write!(f, "Orca terminal is gone"),
            OrcaError::Failed(m) => write!(f, "Orca error: {m}"),
        }
    }
}

#[derive(Deserialize)]
struct Envelope {
    ok: bool,
    #[serde(default)]
    result: Option<serde_json::Value>,
    #[serde(default)]
    error: Option<ErrorBody>,
}

#[derive(Deserialize)]
struct ErrorBody {
    #[serde(default)]
    code: String,
    #[serde(default)]
    message: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawTerminal {
    handle: String,
    #[serde(default, deserialize_with = "null_default")]
    leaf_id: String,
    #[serde(default, deserialize_with = "null_default")]
    worktree_id: String,
    #[serde(default, deserialize_with = "null_default")]
    worktree_path: String,
    // Orca reports `"title": null` for a terminal that has not set one yet.
    #[serde(default, deserialize_with = "null_default")]
    title: String,
    #[serde(default = "yes", deserialize_with = "null_yes")]
    connected: bool,
    #[serde(default = "yes", deserialize_with = "null_yes")]
    writable: bool,
    #[serde(default, deserialize_with = "null_default")]
    orphaned: bool,
    #[serde(default)]
    agent_identity: Option<String>,
    #[serde(default, deserialize_with = "null_default")]
    preview: String,
}

fn yes() -> bool {
    true
}

/// `null` reads as the type's default, like a missing field. One terminal
/// with a null field must not make the whole list unreadable.
fn null_default<'de, D, T>(d: D) -> Result<T, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Deserialize<'de> + Default,
{
    Ok(Option::<T>::deserialize(d)?.unwrap_or_default())
}

fn null_yes<'de, D: serde::Deserializer<'de>>(d: D) -> Result<bool, D::Error> {
    Ok(Option::<bool>::deserialize(d)?.unwrap_or(true))
}

/// Strip leading spinner/status glyphs such as `✳ ` or `◐ `.
pub fn clean_title(title: &str) -> String {
    title
        .trim_start_matches(|c: char| !c.is_alphanumeric())
        .trim_end()
        .to_owned()
}

fn is_stale_code(code: &str) -> bool {
    code.contains("stale") || code.contains("not_found") || code.contains("unknown_terminal")
}

fn check_envelope(
    stdout: &str,
    exit_ok: bool,
    stderr: &str,
) -> Result<Option<serde_json::Value>, OrcaError> {
    match serde_json::from_str::<Envelope>(stdout.trim()) {
        Ok(env) if env.ok => Ok(env.result),
        Ok(env) => {
            let err = env.error.unwrap_or(ErrorBody {
                code: String::new(),
                message: String::new(),
            });
            if is_stale_code(&err.code) {
                Err(OrcaError::Stale)
            } else if err.code.contains("runtime") || err.code.contains("unavailable") {
                Err(OrcaError::Unavailable(err.code))
            } else {
                Err(OrcaError::Failed(if err.message.is_empty() {
                    err.code
                } else {
                    err.message
                }))
            }
        }
        Err(_) if exit_ok => Ok(None),
        Err(_) => {
            let msg = stderr.trim();
            Err(OrcaError::Unavailable(if msg.is_empty() {
                "no JSON reply".into()
            } else {
                msg.chars().take(200).collect()
            }))
        }
    }
}

/// Parse `orca terminal list --json` output into usable sessions.
pub fn parse_list(stdout: &str) -> Result<Vec<OrcaSession>, OrcaError> {
    let result = check_envelope(stdout, true, "")?
        .ok_or_else(|| OrcaError::Unavailable("unexpected list reply".into()))?;
    let raw: Vec<RawTerminal> = serde_json::from_value(
        result
            .get("terminals")
            .cloned()
            .unwrap_or(serde_json::Value::Array(vec![])),
    )
    .map_err(|e| OrcaError::Unavailable(format!("bad terminal list: {e}")))?;
    Ok(raw
        .into_iter()
        .filter(|t| t.connected && t.writable && !t.orphaned && !t.handle.is_empty())
        .map(|t| OrcaSession {
            worktree: t
                .worktree_path
                .rsplit(['/', '\\'])
                .find(|s| !s.is_empty())
                .unwrap_or("")
                .to_owned(),
            title: clean_title(&t.title),
            raw_title: t.title,
            agent: t.agent_identity.filter(|a| !a.is_empty()),
            preview: t.preview,
            reply: None,
            handle: t.handle,
            leaf_id: t.leaf_id,
            worktree_id: t.worktree_id,
        })
        .collect())
}

/// Live Orca Sessions, ordered for the Device: the Current Conversation
/// first, then the rest of the active worktree, then the other worktrees.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct OrcaSnapshot {
    pub sessions: Vec<OrcaSession>,
    /// Whether `sessions[0]` is the Current Conversation.
    pub has_current: bool,
    /// The worktree Orca's UI has selected, if any.
    pub active_worktree: Option<String>,
    /// Whether the Current Conversation was looked up (`worktree ps` plus
    /// visual layouts). False for the cheaper sessions-only look the Orca
    /// watch takes while Orca is in the background.
    pub layout: bool,
}

impl OrcaSnapshot {
    pub fn current(&self) -> Option<&OrcaSession> {
        self.sessions.first().filter(|_| self.has_current)
    }
}

/// The worktree id `orca worktree ps --json` marks `isActive`.
pub fn parse_active_worktree(stdout: &str) -> Result<Option<String>, OrcaError> {
    let result = check_envelope(stdout, true, "")?
        .ok_or_else(|| OrcaError::Unavailable("unexpected worktree reply".into()))?;
    Ok(result
        .get("worktrees")
        .and_then(|w| w.as_array())
        .into_iter()
        .flatten()
        .find(|w| w.get("isActive").and_then(|a| a.as_bool()) == Some(true))
        .and_then(|w| w.get("worktreeId").and_then(|i| i.as_str()))
        .map(str::to_owned))
}

fn str_field<'a>(v: &'a serde_json::Value, key: &str) -> Option<&'a str> {
    v.get(key).and_then(|x| x.as_str())
}

/// First tab group of a layout tree (groups can be split side by side; the
/// CLI does not say which group has focus, so the first one wins).
fn first_group(node: &serde_json::Value) -> Option<&serde_json::Value> {
    if str_field(node, "type") == Some("group") || node.get("tabs").is_some() {
        return Some(node);
    }
    ["first", "second"]
        .iter()
        .filter_map(|k| node.get(*k))
        .chain(
            node.get("children")
                .and_then(|c| c.as_array())
                .into_iter()
                .flatten(),
        )
        .find_map(first_group)
}

/// Terminal leaves of a pane tree, in order.
fn terminals<'a>(node: &'a serde_json::Value, out: &mut Vec<&'a serde_json::Value>) {
    if str_field(node, "type") == Some("terminal") || node.get("handle").is_some() {
        out.push(node);
        return;
    }
    for k in ["first", "second"] {
        if let Some(n) = node.get(k) {
            terminals(n, out);
        }
    }
    if let Some(children) = node.get("children").and_then(|c| c.as_array()) {
        for n in children {
            terminals(n, out);
        }
    }
}

/// Handle of the active leaf in the active tab of a visual layout `root`.
pub fn active_leaf_handle(root: &serde_json::Value) -> Option<String> {
    let group = first_group(root)?;
    let tabs = group.get("tabs")?.as_array()?;
    let active_tab = str_field(group, "activeTabId");
    let tab = tabs
        .iter()
        .find(|t| active_tab.is_some() && str_field(t, "tabId") == active_tab)
        .or_else(|| tabs.first())?;
    let mut leaves = Vec::new();
    terminals(tab.get("panes")?, &mut leaves);
    let active_leaf = str_field(tab, "activeLeafId");
    let leaf = leaves
        .iter()
        .find(|l| active_leaf.is_some() && str_field(l, "leafId") == active_leaf)
        .or_else(|| {
            leaves
                .iter()
                .find(|l| l.get("active").and_then(|a| a.as_bool()) == Some(true))
        })
        .or_else(|| leaves.first())?;
    str_field(leaf, "handle").map(str::to_owned)
}

/// Build the ordered snapshot from `orca worktree ps --json` (the active
/// worktree; `None` when that call failed) and
/// `orca terminal list --json --include-visual-layouts`.
pub fn parse_snapshot(
    active_worktree: Option<String>,
    list_stdout: &str,
) -> Result<OrcaSnapshot, OrcaError> {
    let sessions = parse_list(list_stdout)?;
    let Some(active) = active_worktree else {
        return Ok(OrcaSnapshot {
            sessions,
            has_current: false,
            active_worktree: None,
            layout: true,
        });
    };
    let layout_handle = serde_json::from_str::<serde_json::Value>(list_stdout.trim())
        .ok()
        .and_then(|v| {
            v.get("result")?
                .get("visualLayouts")?
                .as_array()?
                .iter()
                .find(|l| str_field(l, "worktreeId") == Some(active.as_str()))
                .and_then(|l| active_leaf_handle(l.get("root")?))
        });
    let in_active = |s: &OrcaSession| s.worktree_id == active;
    // Layout first; without one, the first writable terminal of the worktree.
    let current = layout_handle
        .and_then(|h| sessions.iter().position(|s| s.handle == h && in_active(s)))
        .or_else(|| sessions.iter().position(in_active));
    let mut ordered = Vec::with_capacity(sessions.len());
    if let Some(i) = current {
        ordered.push(sessions[i].clone());
    }
    for pass_active in [true, false] {
        ordered.extend(
            sessions
                .iter()
                .enumerate()
                .filter(|(i, s)| Some(*i) != current && in_active(s) == pass_active)
                .map(|(_, s)| s.clone()),
        );
    }
    Ok(OrcaSnapshot {
        sessions: ordered,
        has_current: current.is_some(),
        active_worktree: Some(active),
        layout: true,
    })
}

/// Rendered screen lines from `orca terminal read --screen --json`
/// (`result.terminal.tail`).
pub fn parse_screen(stdout: &str) -> Result<Vec<String>, OrcaError> {
    let result = check_envelope(stdout, true, "")?
        .ok_or_else(|| OrcaError::Unavailable("unexpected read reply".into()))?;
    Ok(result
        .get("terminal")
        .and_then(|t| t.get("tail"))
        .and_then(|t| t.as_array())
        .into_iter()
        .flatten()
        .filter_map(|l| l.as_str().map(str::to_owned))
        .collect())
}

/// Text for a terminal: line breaks would submit the prompt, so they become
/// spaces; other control characters are dropped.
pub fn sanitize_terminal_text(text: &str) -> String {
    text.chars()
        .filter_map(|c| match c {
            '\r' | '\n' | '\t' => Some(' '),
            c if c.is_control() => None,
            c => Some(c),
        })
        .collect()
}

pub struct CmdOutput {
    pub success: bool,
    pub stdout: String,
    pub stderr: String,
}

/// Longest wait for an ordinary CLI call.
pub const CALL_TIMEOUT: Duration = Duration::from_secs(5);
/// Longest wait for `orca open` (launch Orca and wait for its runtime).
pub const OPEN_TIMEOUT: Duration = Duration::from_secs(20);

/// Runs the `orca` CLI with the given arguments, killing it after `timeout`.
pub trait CommandRunner: Send {
    fn run(&mut self, args: &[String], timeout: Duration) -> std::io::Result<CmdOutput>;
}

/// Real runner: spawns the CLI directly (no shell).
pub struct ProcessRunner {
    pub program: PathBuf,
}

impl ProcessRunner {
    /// Locate the CLI: `$VIBE_VOICE_ORCA`, then common install paths, then PATH.
    /// Apps started from Finder get a minimal PATH, hence the fixed paths.
    #[cfg(not(windows))]
    pub fn locate() -> Self {
        let mut candidates: Vec<PathBuf> = Vec::new();
        if let Ok(p) = std::env::var("VIBE_VOICE_ORCA") {
            candidates.push(p.into());
        }
        candidates.push("/usr/local/bin/orca".into());
        candidates.push("/opt/homebrew/bin/orca".into());
        candidates.push("/Applications/Orca.app/Contents/Resources/bin/orca".into());
        if let Some(path) = std::env::var_os("PATH") {
            candidates.extend(std::env::split_paths(&path).map(|d| d.join("orca")));
        }
        let program = candidates
            .into_iter()
            .find(|p| p.is_file())
            .unwrap_or_else(|| PathBuf::from("orca"));
        Self { program }
    }

    /// Windows: `$VIBE_VOICE_ORCA`, the per-user install, then `orca.exe` on
    /// PATH (Orca's Settings › General › Orca CLI registers it there). Only a
    /// real executable is used: a `.cmd` shim would run through `cmd.exe`,
    /// whose quoting would mangle (or execute) dictated text.
    #[cfg(windows)]
    pub fn locate() -> Self {
        let mut candidates: Vec<PathBuf> = Vec::new();
        if let Ok(p) = std::env::var("VIBE_VOICE_ORCA") {
            candidates.push(p.into());
        }
        if let Some(local) = std::env::var_os("LOCALAPPDATA") {
            let programs = PathBuf::from(local).join("Programs");
            for dir in ["Orca", "orca"] {
                candidates.push(programs.join(dir).join("resources").join("bin").join("orca.exe"));
                candidates.push(programs.join(dir).join("orca.exe"));
            }
        }
        if let Some(path) = std::env::var_os("PATH") {
            candidates.extend(std::env::split_paths(&path).map(|d| d.join("orca.exe")));
        }
        let program = candidates
            .into_iter()
            .find(|p| p.is_file() && p.extension().is_some_and(|e| e.eq_ignore_ascii_case("exe")))
            .unwrap_or_else(|| PathBuf::from("orca.exe"));
        Self { program }
    }
}

fn drain(pipe: Option<impl Read + Send + 'static>) -> std::thread::JoinHandle<Vec<u8>> {
    std::thread::spawn(move || {
        let mut buf = Vec::new();
        if let Some(mut p) = pipe {
            let _ = p.read_to_end(&mut buf);
        }
        buf
    })
}

impl CommandRunner for ProcessRunner {
    fn run(&mut self, args: &[String], timeout: Duration) -> std::io::Result<CmdOutput> {
        use std::process::Stdio;
        let mut cmd = std::process::Command::new(&self.program);
        cmd.args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        // Finder-launched apps lack /usr/local/bin; the CLI script needs `env bash`.
        #[cfg(not(windows))]
        {
            let path = std::env::var("PATH").unwrap_or_default();
            cmd.env(
                "PATH",
                format!("{path}:/usr/local/bin:/opt/homebrew/bin:/usr/bin:/bin"),
            );
        }
        // No console window flashing up for every call.
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            const CREATE_NO_WINDOW: u32 = 0x0800_0000;
            cmd.creation_flags(CREATE_NO_WINDOW);
        }
        let mut child = cmd.spawn()?;
        let out = drain(child.stdout.take());
        let err = drain(child.stderr.take());
        let start = Instant::now();
        let status = loop {
            if let Some(st) = child.try_wait()? {
                break st;
            }
            if start.elapsed() >= timeout {
                let _ = child.kill();
                let _ = child.wait();
                return Err(std::io::Error::new(
                    std::io::ErrorKind::TimedOut,
                    format!("orca {} timed out", args.first().map_or("", |a| a)),
                ));
            }
            std::thread::sleep(Duration::from_millis(10));
        };
        Ok(CmdOutput {
            success: status.success(),
            stdout: String::from_utf8_lossy(&out.join().unwrap_or_default()).into_owned(),
            stderr: String::from_utf8_lossy(&err.join().unwrap_or_default()).into_owned(),
        })
    }
}

/// Orca operations the session logic needs.
pub trait OrcaApi {
    /// Live sessions with the Current Conversation first.
    fn snapshot(&mut self) -> Result<OrcaSnapshot, OrcaError>;
    /// Launch Orca and wait (bounded) until its runtime answers.
    fn open(&mut self) -> Result<(), OrcaError>;
    fn send_text(&mut self, handle: &str, text: &str) -> Result<(), OrcaError>;
    fn send_enter(&mut self, handle: &str) -> Result<(), OrcaError>;
    fn send_backspaces(&mut self, handle: &str, count: usize) -> Result<(), OrcaError>;
    fn switch(&mut self, handle: &str) -> Result<(), OrcaError>;
}

pub struct OrcaClient<R: CommandRunner> {
    runner: R,
}

/// How long one screen read may take (on the Orca watch thread).
pub const READ_TIMEOUT: Duration = Duration::from_secs(3);

impl<R: CommandRunner> OrcaClient<R> {
    pub fn new(runner: R) -> Self {
        Self { runner }
    }

    /// Sessions only, in one CLI call and without the Current Conversation:
    /// all the Alerts need. Each call spawns Orca's CLI (Electron as node,
    /// ~0.1 s CPU), so the watch takes this one while Orca is not in front.
    pub fn sessions(&mut self) -> Result<OrcaSnapshot, OrcaError> {
        let list: Vec<String> = ["terminal", "list", "--limit=500", "--json"].map(String::from).into();
        let out = self.run_raw(&list, CALL_TIMEOUT)?;
        let mut snap = parse_snapshot(None, &out.stdout)?;
        snap.layout = false;
        Ok(snap)
    }

    /// Read-only: the rendered screen of one terminal.
    pub fn read_screen(&mut self, handle: &str) -> Result<Vec<String>, OrcaError> {
        let args = vec![
            "terminal".to_owned(),
            "read".to_owned(),
            format!("--terminal={handle}"),
            "--screen".to_owned(),
            "--json".to_owned(),
        ];
        let out = self.run_raw(&args, READ_TIMEOUT)?;
        parse_screen(&out.stdout)
    }

    fn call(&mut self, args: Vec<String>) -> Result<Option<serde_json::Value>, OrcaError> {
        log::debug!(
            "orca {}",
            args.iter()
                .filter(|a| !a.starts_with("--text="))
                .cloned()
                .collect::<Vec<_>>()
                .join(" ")
        );
        self.call_with(args, CALL_TIMEOUT)
    }

    fn call_with(
        &mut self,
        args: Vec<String>,
        timeout: Duration,
    ) -> Result<Option<serde_json::Value>, OrcaError> {
        let out = self.run_raw(&args, timeout)?;
        check_envelope(&out.stdout, out.success, &out.stderr)
    }

    fn run_raw(&mut self, args: &[String], timeout: Duration) -> Result<CmdOutput, OrcaError> {
        let out = self
            .runner
            .run(args, timeout)
            .map_err(|e| OrcaError::Unavailable(format!("cannot run orca: {e}")))?;
        if !out.success && serde_json::from_str::<serde_json::Value>(out.stdout.trim()).is_err() {
            return Err(OrcaError::Unavailable(
                out.stderr.trim().chars().take(200).collect(),
            ));
        }
        Ok(out)
    }

    fn send(&mut self, handle: &str, text: Option<&str>, enter: bool) -> Result<(), OrcaError> {
        let mut args = vec![
            "terminal".to_owned(),
            "send".to_owned(),
            format!("--terminal={handle}"),
        ];
        if let Some(t) = text {
            args.push(format!("--text={t}"));
        }
        if enter {
            args.push("--enter".to_owned());
        }
        args.push("--json".to_owned());
        self.call(args).map(|_| ())
    }
}

impl<R: CommandRunner> OrcaApi for OrcaClient<R> {
    fn snapshot(&mut self) -> Result<OrcaSnapshot, OrcaError> {
        let ps: Vec<String> = ["worktree", "ps", "--limit=500", "--json"]
            .map(String::from)
            .into();
        let out = self.run_raw(&ps, CALL_TIMEOUT)?;
        let active = parse_active_worktree(&out.stdout).unwrap_or_else(|e| {
            log::warn!("orca worktree ps: {e}");
            None
        });
        let list: Vec<String> = [
            "terminal",
            "list",
            "--limit=500",
            "--include-visual-layouts",
            "--json",
        ]
        .map(String::from)
        .into();
        let out = self.run_raw(&list, CALL_TIMEOUT)?;
        parse_snapshot(active, &out.stdout)
    }

    fn open(&mut self) -> Result<(), OrcaError> {
        log::info!("launching Orca (orca open)");
        self.call_with(vec!["open".into(), "--json".into()], OPEN_TIMEOUT)
            .map(|_| ())
    }

    fn send_text(&mut self, handle: &str, text: &str) -> Result<(), OrcaError> {
        let clean = sanitize_terminal_text(text);
        if clean.is_empty() {
            return Ok(());
        }
        self.send(handle, Some(&clean), false)
    }

    fn send_enter(&mut self, handle: &str) -> Result<(), OrcaError> {
        self.send(handle, None, true)
    }

    fn send_backspaces(&mut self, handle: &str, count: usize) -> Result<(), OrcaError> {
        if count == 0 {
            return Ok(());
        }
        self.send(handle, Some(&"\u{7f}".repeat(count)), false)
    }

    fn switch(&mut self, handle: &str) -> Result<(), OrcaError> {
        self.call(vec![
            "terminal".into(),
            "switch".into(),
            format!("--terminal={handle}"),
            "--json".into(),
        ])
        .map(|_| ())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    // Shape captured from `orca terminal list --json` (Orca CLI 1.4.218),
    // with paths and titles anonymised.
    const LIST_JSON: &str = r#"{
      "id": "dacda9a8-a16d-411c-9e7f-a6ed35f35350",
      "ok": true,
      "result": {
        "terminals": [
          {
            "handle": "term_aaa",
            "ptyId": "x::/Users/me/src/voice-notes@@1163288f",
            "incarnationId": "d02c",
            "orphaned": false,
            "worktreeId": "x::/Users/me/src/voice-notes",
            "worktreePath": "/Users/me/src/voice-notes",
            "branch": "refs/heads/master",
            "tabId": "6922",
            "leafId": "leaf-a",
            "title": "✳ 提交 PR 合并发布",
            "connected": true,
            "writable": true,
            "lastOutputAt": 1791177614980,
            "preview": "…",
            "executionHostId": "local",
            "agentIdentity": "claude"
          },
          {
            "handle": "term_bbb", "orphaned": false,
            "worktreePath": "/Users/me/src/my-passport", "leafId": "leaf-b",
            "title": "◐ MacOS语音输入集成", "connected": true, "writable": true
          },
          {
            "handle": "term_ccc", "orphaned": false,
            "worktreePath": "/Users/me/src/AxiomOS-rel", "leafId": "leaf-c",
            "title": "Terminal 1", "connected": true, "writable": true, "agentIdentity": null
          },
          {
            "handle": "term_orphan", "orphaned": true, "worktreePath": "",
            "title": "✳ 新手引导设计", "connected": true, "writable": true
          },
          {
            "handle": "term_ro", "orphaned": false, "worktreePath": "/x/y",
            "title": "read only", "connected": true, "writable": false
          }
        ]
      ,
        "totalCount": 5,
        "truncated": false
      }
    }"#;

    #[test]
    fn parses_real_list_shape() {
        let s = parse_list(LIST_JSON).unwrap();
        assert_eq!(s.len(), 3);
        assert_eq!(s[0].handle, "term_aaa");
        assert_eq!(s[0].leaf_id, "leaf-a");
        assert_eq!(s[0].label(), "voice-notes · 提交 PR 合并发布");
        assert_eq!(s[1].label(), "my-passport · MacOS语音输入集成");
        assert_eq!(s[2].label(), "AxiomOS-rel · Terminal 1");
    }

    // Shape captured from `orca terminal list --json --include-visual-layouts`
    // (Orca CLI 1.4.218), anonymised; wt-b has a split group and a split pane.
    const LAYOUT_JSON: &str = r#"{
      "ok": true,
      "result": {
        "terminals": [
          {"handle": "t_a1", "worktreeId": "r1::/src/wt-a", "worktreePath": "/src/wt-a",
           "tabId": "tab-a1", "leafId": "leaf-a1", "title": "✳ 修复测试",
           "connected": true, "writable": true, "orphaned": false},
          {"handle": "t_b1", "worktreeId": "r2::/src/wt-b", "worktreePath": "/src/wt-b",
           "tabId": "tab-b1", "leafId": "leaf-b1", "title": "Setup",
           "connected": true, "writable": true, "orphaned": false},
          {"handle": "t_b2", "worktreeId": "r2::/src/wt-b", "worktreePath": "/src/wt-b",
           "tabId": "tab-b2", "leafId": "leaf-b2", "title": "◐ 语音输入",
           "connected": true, "writable": true, "orphaned": false},
          {"handle": "t_b3", "worktreeId": "r2::/src/wt-b", "worktreePath": "/src/wt-b",
           "tabId": "tab-b2", "leafId": "leaf-b3", "title": "server",
           "connected": true, "writable": true, "orphaned": false},
          {"handle": "t_c1", "worktreeId": "r3::/src/wt-c", "worktreePath": "/src/wt-c",
           "tabId": "tab-c1", "leafId": "leaf-c1", "title": "Terminal 1",
           "connected": true, "writable": true, "orphaned": false}
        ],
        "visualLayouts": [
          {"worktreeId": "r1::/src/wt-a", "worktreePath": "/src/wt-a",
           "root": {"type": "group", "groupId": "g1", "activeTabId": "tab-a1", "tabs": [
             {"tabId": "tab-a1", "title": "修复测试", "activeLeafId": "leaf-a1",
              "panes": {"type": "terminal", "handle": "t_a1", "tabId": "tab-a1",
                        "leafId": "leaf-a1", "title": "✳ 修复测试", "connected": true, "active": true}}]}},
          {"worktreeId": "r2::/src/wt-b", "worktreePath": "/src/wt-b",
           "root": {"type": "split", "direction": "horizontal",
             "first": {"type": "group", "groupId": "g2", "activeTabId": "tab-b2", "tabs": [
               {"tabId": "tab-b1", "title": "Setup", "activeLeafId": "leaf-b1",
                "panes": {"type": "terminal", "handle": "t_b1", "leafId": "leaf-b1", "active": true}},
               {"tabId": "tab-b2", "title": "语音输入", "activeLeafId": "leaf-b3",
                "panes": {"type": "pane-split", "direction": "vertical",
                  "first": {"type": "terminal", "handle": "t_b2", "leafId": "leaf-b2", "active": false},
                  "second": {"type": "terminal", "handle": "t_b3", "leafId": "leaf-b3", "active": true}}}]},
             "second": {"type": "group", "groupId": "g3", "activeTabId": "tab-x", "tabs": []}}}
        ],
        "totalCount": 5,
        "truncated": false
      }
    }"#;

    const PS_JSON: &str = r#"{"ok": true, "result": {"worktrees": [
      {"worktreeId": "r1::/src/wt-a", "path": "/src/wt-a", "isActive": false},
      {"worktreeId": "r2::/src/wt-b", "path": "/src/wt-b", "isActive": true}
    ]}}"#;

    fn handles(s: &OrcaSnapshot) -> Vec<&str> {
        s.sessions.iter().map(|x| x.handle.as_str()).collect()
    }

    #[test]
    fn current_conversation_from_active_worktree_layout() {
        let active = parse_active_worktree(PS_JSON).unwrap();
        assert_eq!(active.as_deref(), Some("r2::/src/wt-b"));
        let s = parse_snapshot(active, LAYOUT_JSON).unwrap();
        // Active tab tab-b2, active leaf leaf-b3 inside a split pane.
        assert_eq!(s.current().unwrap().handle, "t_b3");
        assert_eq!(s.current().unwrap().label(), "wt-b · server");
        // Current first, then the rest of wt-b, then the others in list order.
        assert_eq!(handles(&s), ["t_b3", "t_b1", "t_b2", "t_a1", "t_c1"]);
    }

    #[test]
    fn current_conversation_fallbacks() {
        // Active worktree without layout: its first writable terminal.
        let s = parse_snapshot(Some("r3::/src/wt-c".into()), LAYOUT_JSON).unwrap();
        assert_eq!(handles(&s)[0], "t_c1");
        assert!(s.has_current);
        // Layout names a handle that is not usable: first terminal of the worktree.
        let no_layout = LAYOUT_JSON.replace(
            "\"handle\": \"t_b3\", \"leafId\"",
            "\"handle\": \"t_gone\", \"leafId\"",
        );
        let s = parse_snapshot(Some("r2::/src/wt-b".into()), &no_layout).unwrap();
        assert_eq!(s.current().unwrap().handle, "t_b1");
        // No active worktree, or one without terminals: no Current Conversation.
        let s = parse_snapshot(None, LAYOUT_JSON).unwrap();
        assert!(s.current().is_none());
        assert_eq!(handles(&s), ["t_a1", "t_b1", "t_b2", "t_b3", "t_c1"]);
        let s = parse_snapshot(Some("r9::/nowhere".into()), LAYOUT_JSON).unwrap();
        assert!(s.current().is_none());
        assert_eq!(
            parse_active_worktree(
                r#"{"ok":true,"result":{"worktrees":[{"worktreeId":"x","isActive":false}]}}"#
            ),
            Ok(None)
        );
    }

    #[test]
    fn layout_walk_defaults() {
        let v: serde_json::Value = serde_json::from_str(
            r#"{"type":"group","activeTabId":"missing","tabs":[
                {"tabId":"t1","panes":{"type":"pane-split",
                  "first":{"type":"terminal","handle":"h1","leafId":"l1","active":false},
                  "second":{"type":"terminal","handle":"h2","leafId":"l2","active":true}}}]}"#,
        )
        .unwrap();
        assert_eq!(active_leaf_handle(&v).as_deref(), Some("h2"));
        assert_eq!(
            active_leaf_handle(&serde_json::json!({"type":"group","tabs":[]})),
            None
        );
    }

    #[test]
    fn parses_screen_reply() {
        let json = r#"{"ok":true,"result":{"terminal":{"handle":"t","status":"running",
            "tail":["line one","  line two"],"truncated":false}}}"#;
        assert_eq!(parse_screen(json).unwrap(), ["line one", "  line two"]);
        assert_eq!(
            parse_screen(r#"{"ok":true,"result":{"terminal":{}}}"#).unwrap(),
            Vec::<String>::new()
        );
        assert_eq!(
            parse_screen(r#"{"ok":false,"error":{"code":"terminal_handle_stale"}}"#),
            Err(OrcaError::Stale)
        );
    }

    #[test]
    fn list_errors() {
        assert_eq!(
            parse_list(r#"{"ok":false,"error":{"code":"terminal_handle_stale","message":"x"}}"#),
            Err(OrcaError::Stale)
        );
        assert!(matches!(
            parse_list("not json"),
            Err(OrcaError::Unavailable(_))
        ));
        assert_eq!(
            parse_list(r#"{"ok":true,"result":{"terminals":[]}}"#).unwrap(),
            vec![]
        );
    }

    #[test]
    fn null_fields_read_as_missing() {
        // Orca 1.4.218 reports `"title": null` for a terminal without a title.
        let s = parse_list(
            r#"{"ok":true,"result":{"terminals":[
              {"handle":"term_a","title":null,"worktreePath":"/x/repo","preview":null,
               "connected":null,"writable":true,"orphaned":null,"agentIdentity":null},
              {"handle":"term_b","title":"✳ 任务","worktreePath":"/x/repo"}
            ]}}"#,
        )
        .unwrap();
        assert_eq!(s.len(), 2);
        assert_eq!(s[0].title, "");
        assert_eq!(s[0].label(), "repo");
        assert_eq!(s[1].title, "任务");
    }

    #[test]
    fn title_cleanup() {
        assert_eq!(clean_title("✳ 任务列表"), "任务列表");
        assert_eq!(clean_title("◐ abc "), "abc");
        assert_eq!(clean_title("Setup"), "Setup");
        assert_eq!(clean_title("✳"), "");
    }

    #[test]
    fn sanitizes_terminal_text() {
        assert_eq!(sanitize_terminal_text("a\nb\r\nc\u{1b}[0m"), "a b  c[0m");
    }

    #[derive(Clone, Default)]
    struct Mock {
        calls: Arc<Mutex<Vec<Vec<String>>>>,
        reply: Arc<Mutex<(bool, String)>>,
    }

    impl CommandRunner for Mock {
        fn run(&mut self, args: &[String], _: Duration) -> std::io::Result<CmdOutput> {
            self.calls.lock().unwrap().push(args.to_vec());
            let (success, stdout) = self.reply.lock().unwrap().clone();
            Ok(CmdOutput {
                success,
                stdout,
                stderr: String::new(),
            })
        }
    }

    struct Missing;
    impl CommandRunner for Missing {
        fn run(&mut self, _: &[String], _: Duration) -> std::io::Result<CmdOutput> {
            Err(std::io::Error::new(std::io::ErrorKind::NotFound, "orca"))
        }
    }

    #[test]
    fn builds_argv_without_shell() {
        let mock = Mock::default();
        *mock.reply.lock().unwrap() = (true, r#"{"ok":true,"result":{}}"#.into());
        let mut c = OrcaClient::new(mock.clone());
        c.send_text("term_1", "--help; rm -rf ~ `x` $(y)\n")
            .unwrap();
        c.send_enter("term_1").unwrap();
        c.send_backspaces("term_1", 3).unwrap();
        c.send_backspaces("term_1", 0).unwrap();
        c.switch("term_1").unwrap();
        c.open().unwrap();
        let calls = mock.calls.lock().unwrap();
        assert_eq!(calls.len(), 5);
        assert_eq!(calls[4], ["open", "--json"]);
        assert_eq!(
            calls[0],
            [
                "terminal",
                "send",
                "--terminal=term_1",
                "--text=--help; rm -rf ~ `x` $(y) ",
                "--json"
            ]
        );
        assert_eq!(
            calls[1],
            ["terminal", "send", "--terminal=term_1", "--enter", "--json"]
        );
        assert_eq!(
            calls[2],
            [
                "terminal",
                "send",
                "--terminal=term_1",
                "--text=\u{7f}\u{7f}\u{7f}",
                "--json"
            ]
        );
        assert_eq!(
            calls[3],
            ["terminal", "switch", "--terminal=term_1", "--json"]
        );
    }

    #[test]
    fn maps_stale_and_missing_cli() {
        let mock = Mock::default();
        *mock.reply.lock().unwrap() =
            (false, r#"{"ok":false,"error":{"code":"terminal_handle_stale","message":"terminal_handle_stale"}}"#.into());
        let mut c = OrcaClient::new(mock);
        assert_eq!(c.send_text("term_x", "hi"), Err(OrcaError::Stale));
        let mut m = OrcaClient::new(Missing);
        assert!(matches!(m.snapshot(), Err(OrcaError::Unavailable(_))));
        assert!(matches!(m.open(), Err(OrcaError::Unavailable(_))));
        assert!(matches!(m.send_enter("t"), Err(OrcaError::Unavailable(_))));
    }
}
