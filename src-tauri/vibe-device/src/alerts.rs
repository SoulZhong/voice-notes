//! Alerts: an Orca agent session finished its turn and waits for the user.
//!
//! The Orca CLI has no event stream, so the agent's state is read from the
//! terminal title in `orca terminal list --json`:
//!
//! - Claude Code puts a spinner glyph in front of the title while it works
//!   (`◐ ◑ ◒ ◓`, Braille `⠋…`, `✶ ✻ ✽ ✢ ·` and similar) and `✳` when it is idle,
//!   waiting for the user.
//! - For other agents Orca writes the title itself (its agent table in
//!   `app.asar`): `"<Agent>"` while working, `"<Agent> - action required"` when
//!   it asks for permission and `"<Agent> ready"` when idle — Codex, Cursor
//!   Agent, OpenCode, Pi, OMP, Droid, Hermes, Devin, ZCode. Orca does not
//!   synthesize a working title for Codex (it keeps Codex's own title), so a
//!   Codex turn only alerts when its title was recognizably working before.
//! - Gemini CLI: `✦ Gemini CLI` working, `◇ Gemini CLI` idle.
//!
//! Anything else (plain shells, unrecognized titles) is [`AgentState::Unknown`]
//! and never alerts.

use crate::orca::{OrcaSession, clean_title};
use crate::protocol::utf8_head;
use std::collections::HashMap;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentState {
    Working,
    Waiting,
    Unknown,
}

/// Titles Orca writes for agents without their own convention:
/// (working, permission, idle), from Orca's agent table.
const ORCA_AGENT_TITLES: &[(&str, &str, &str)] = &[
    ("Codex", "Codex - action required", "Codex ready"),
    ("Cursor Agent", "Cursor - action required", "Cursor ready"),
    ("OpenCode", "OpenCode - action required", "OpenCode ready"),
    ("Pi", "Pi - action required", "Pi ready"),
    ("OMP", "OMP - action required", "OMP ready"),
    ("Droid", "Droid - action required", "Droid ready"),
    ("Hermes", "Hermes - action required", "Hermes ready"),
    ("Devin", "Devin - action required", "Devin ready"),
    ("ZCode", "ZCode - action required", "ZCode ready"),
];

fn is_spinner(c: char) -> bool {
    matches!(
        c,
        '◐' | '◑'
            | '◒'
            | '◓'
            | '◴'
            | '◵'
            | '◶'
            | '◷'
            | '✶'
            | '✻'
            | '✽'
            | '✢'
            | '·'
            | '✦'
            | '⏳'
    ) || ('\u{2800}'..='\u{28FF}').contains(&c)
}

/// The agent's state as its terminal title shows it.
pub fn title_state(title: &str) -> AgentState {
    let t = title.trim();
    let Some(first) = t.chars().next() else {
        return AgentState::Unknown;
    };
    if first == '✳' || first == '◇' {
        return AgentState::Waiting;
    }
    if is_spinner(first) {
        return AgentState::Working;
    }
    for (working, permission, idle) in ORCA_AGENT_TITLES {
        if t == *working {
            return AgentState::Working;
        }
        if t == *permission || t == *idle {
            return AgentState::Waiting;
        }
    }
    AgentState::Unknown
}

/// Box drawing, block elements and similar TUI chrome.
fn is_chrome_char(c: char) -> bool {
    ('\u{2500}'..='\u{259F}').contains(&c) || matches!(c, '│' | '┃' | '╭' | '╮' | '╯' | '╰')
}

fn strip_ansi(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\u{1b}' {
            match chars.peek() {
                Some('[') => {
                    chars.next();
                    for c in chars.by_ref() {
                        if ('@'..='~').contains(&c) {
                            break;
                        }
                    }
                }
                Some(_) => {
                    chars.next();
                }
                None => {}
            }
        } else if !c.is_control() || c == '\n' {
            out.push(c);
        }
    }
    out
}

/// Whether a preview line is TUI noise rather than the agent's words.
fn is_noise(line: &str) -> bool {
    let t = line.trim();
    // Too short to say anything ("6", ":").
    if t.chars().filter(|c| c.is_alphanumeric()).count() < 2 {
        return true;
    }
    let chrome = t.chars().filter(|c| is_chrome_char(*c)).count();
    if chrome * 2 >= t.chars().count() {
        return true;
    }
    let first = t.chars().next().unwrap_or(' ');
    if is_spinner(first) || matches!(first, '✔' | '✓' | '⎿' | '>' | '❯' | '$' | '%') {
        return true;
    }
    const NOISE: &[&str] = &[
        "new task?",
        "Resume this session",
        "claude --resume",
        "/exit",
        "(base) ",
        "Switching from",
        "for agents",
        "Ran ",
    ];
    if NOISE.iter().any(|n| t.starts_with(n)) || t.starts_with('/') || t.contains(" · done ") {
        return true;
    }
    // Shell prompts such as "user@host dir %".
    (t.contains('@') && (t.ends_with('%') || t.ends_with('$') || t.ends_with('#')))
        || t.contains("--dangerously-skip-permissions")
}

/// Join wrapped terminal lines: a space only between ASCII words.
fn join_lines(a: &str, b: &str) -> String {
    if a.is_empty() {
        return b.to_owned();
    }
    // A space between ASCII text (words, or after ASCII punctuation); none
    // around CJK, where wrapped lines join directly.
    let space = a
        .chars()
        .last()
        .is_some_and(|c| c.is_ascii_alphanumeric() || c.is_ascii_punctuation())
        && b.chars().next().is_some_and(|c| c.is_ascii_alphanumeric());
    if space {
        format!("{a} {b}")
    } else {
        format!("{a}{b}")
    }
}

/// The readable end of a terminal preview: the agent's last lines before the
/// prompt box, without status and chrome lines. `None` when nothing is left.
pub fn preview_message(preview: &str) -> Option<String> {
    let clean = strip_ansi(preview);
    let lines: Vec<&str> = clean.lines().collect();
    // Claude Code's prompt box starts at the first full-width rule; what
    // follows is the input line and the status line, not the agent's reply.
    let end = lines
        .iter()
        .position(|l| {
            let t = l.trim();
            t.chars().count() >= 8 && t.chars().all(is_chrome_char)
        })
        .unwrap_or(lines.len());
    // "※ recap: …" is Claude Code's own summary of the turn: prefer it.
    let is_recap = |l: &str| {
        let t = l.trim_start();
        t.starts_with('※') || t.starts_with("recap:")
    };
    if let Some(i) = lines[..end].iter().rposition(|l| is_recap(l)) {
        let mut text = lines[i]
            .trim_start()
            .trim_start_matches('※')
            .trim()
            .to_owned();
        if let Some(rest) = text.strip_prefix("recap:") {
            text = rest.trim().to_owned();
        }
        for l in &lines[i + 1..end] {
            let t = l.trim();
            if is_noise(t) {
                break;
            }
            text = join_lines(&text, t);
        }
        if let Some(cut) = text.find("(disable recaps") {
            text.truncate(cut);
        }
        let text = text.trim().to_owned();
        if !text.is_empty() {
            return Some(text);
        }
    }
    let kept: Vec<String> = lines[..end]
        .iter()
        .map(|l| l.trim())
        .filter(|t| !is_noise(t))
        .map(str::to_owned)
        .collect();
    let tail = &kept[kept.len().saturating_sub(3)..];
    let text = tail
        .iter()
        .fold(String::new(), |acc, l| join_lines(&acc, l));
    let text = text.trim().to_owned();
    (!text.is_empty()).then_some(text)
}

/// Shown when the preview has nothing readable.
pub const DEFAULT_MESSAGE: &str = "等待你的回复";
/// Label budget within the ALERT frame; the message continues in
/// ALERT_MORE frames up to [`MESSAGE_BYTES`].
pub const LABEL_BYTES: usize = 63;
pub const MESSAGE_BYTES: usize = 360;

/// Whether a screen line is a full-width rule (the prompt box borders).
fn is_rule(line: &str) -> bool {
    let t = line.trim();
    t.chars().count() >= 8 && t.chars().all(|c| c == '─' || c == '━' || c == '═')
}

/// The agent's words from a rendered Claude Code screen
/// (`orca terminal read --screen`): the last "⏺ " reply block that is not a
/// tool call (what Orca's notification shows), else the "※ recap:" paragraph. Status lines, the
/// prompt box and the status line rows below it are ignored.
pub fn screen_message(lines: &[String]) -> Option<String> {
    // Everything from the prompt box down is input and status rows: stop at
    // the last rule that sits right above the "❯" prompt.
    let end = lines
        .iter()
        .enumerate()
        .rev()
        .find(|(i, l)| {
            is_rule(l)
                && lines
                    .get(i + 1)
                    .is_some_and(|n| n.trim_start().starts_with('❯'))
        })
        .map(|(i, _)| i)
        .unwrap_or(lines.len());
    let body = &lines[..end];

    // Continuation lines of a block: indented, or blank between indented ones.
    let block = |start: usize, first: &str| -> String {
        let mut text = first.trim().to_owned();
        for l in &body[start + 1..] {
            if l.trim().is_empty() {
                continue;
            }
            if !l.starts_with(' ') {
                break;
            }
            let t = l.trim();
            if t.starts_with('⎿') || is_chrome_line(t) {
                continue;
            }
            text = join_lines(&text, &strip_table_chars(t));
        }
        text
    };

    // (a) The last reply block that is not a tool call ("⏺ Bash(…)"): the same
    // message Orca's macOS notification shows.
    for (i, l) in body.iter().enumerate().rev() {
        let Some(rest) = l.trim_start().strip_prefix('⏺') else {
            continue;
        };
        let rest = rest.trim();
        if is_tool_call(rest) || rest.is_empty() {
            continue;
        }
        let text = block(i, rest);
        let text = text.trim();
        if !text.is_empty() {
            return Some(text.to_owned());
        }
    }
    // (b) Claude Code's recap of the turn, when the reply is off screen.
    if let Some(i) = body.iter().rposition(|l| l.trim_start().starts_with('※')) {
        let first = body[i].trim_start().trim_start_matches('※').trim();
        let first = first.strip_prefix("recap:").unwrap_or(first).trim();
        let mut text = block(i, first);
        if let Some(cut) = text.find("(disable recaps") {
            text.truncate(cut);
        }
        let text = text.trim();
        if !text.is_empty() {
            return Some(text.to_owned());
        }
    }
    None
}

/// "Bash(cargo test)", "Read(src/x.rs)", "Update(…)": a tool call header.
fn is_tool_call(s: &str) -> bool {
    match s.find('(') {
        Some(i) if i > 0 => s[..i]
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == ' ' || c == ':' || c == '-'),
        _ => false,
    }
}

/// Table borders ("│ a │ b │") inside a line become single spaces.
fn strip_table_chars(t: &str) -> String {
    let replaced: String = t.chars().map(|c| if is_chrome_char(c) { ' ' } else { c }).collect();
    replaced.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Box drawing and similar rows inside a block.
fn is_chrome_line(t: &str) -> bool {
    let chrome = t.chars().filter(|c| is_chrome_char(*c)).count();
    chrome * 2 >= t.chars().count()
}

/// Keep the start of `s` within `max` bytes, with a trailing `…` when cut,
/// like the notification text on the Mac.
fn head_within(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_owned();
    }
    let mut end = max - '…'.len_utf8();
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &s[..end])
}

/// One Alert as sent to the Device.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Alert {
    pub id: u8,
    pub handle: String,
    pub label: String,
    pub message: String,
}

/// Why an Alert went away.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClearReason {
    /// The session started working again.
    Working,
    /// The session closed.
    Closed,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AlertEvent {
    Raise(Alert),
    Clear { id: u8, reason: ClearReason },
}

/// The agent state of a listed session (agents only).
pub fn session_state(s: &OrcaSession) -> AgentState {
    if s.agent.is_some() {
        title_state(&s.raw_title)
    } else {
        AgentState::Unknown
    }
}

/// On the Orca watch thread: which sessions just went working → waiting, so
/// their screens are read once (the tracker in the session repeats the same
/// decision from the same snapshots).
#[derive(Debug, Default)]
pub struct TurnWatch {
    last: HashMap<String, AgentState>,
}

impl TurnWatch {
    pub fn turned_waiting(&mut self, sessions: &[OrcaSession]) -> Vec<String> {
        let mut out = Vec::new();
        let mut seen = HashMap::new();
        for s in sessions {
            let state = session_state(s);
            if self.last.get(&s.handle) == Some(&AgentState::Working)
                && state == AgentState::Waiting
            {
                out.push(s.handle.clone());
            }
            seen.insert(s.handle.clone(), state);
        }
        self.last = seen;
        out
    }
}

/// Tracks each Orca Session's agent state across polls and raises an Alert
/// on a working → waiting transition.
#[derive(Debug, Default)]
pub struct AlertTracker {
    last: HashMap<String, AgentState>,
    ids: HashMap<String, u8>,
    next_id: u8,
    pending: HashMap<String, Alert>,
}

impl AlertTracker {
    /// Stable per-session id (1..=255, reused round-robin).
    fn id_for(&mut self, handle: &str) -> u8 {
        if let Some(id) = self.ids.get(handle) {
            return *id;
        }
        self.next_id = if self.next_id == u8::MAX {
            1
        } else {
            self.next_id + 1
        };
        let id = self.next_id;
        self.ids.retain(|_, v| *v != id);
        self.ids.insert(handle.to_owned(), id);
        id
    }

    /// Feed one poll. Like Orca's own notifications, every turn end raises
    /// an Alert, whether or not the user is looking at the session.
    pub fn update(&mut self, sessions: &[OrcaSession]) -> Vec<AlertEvent> {
        let mut events = Vec::new();
        let mut seen = HashMap::new();
        for s in sessions {
            let state = session_state(s);
            seen.insert(s.handle.clone(), state);
            let before = self.last.get(&s.handle).copied();
            if before == Some(AgentState::Working) && state == AgentState::Waiting {
                let id = self.id_for(&s.handle);
                let title = clean_title(&s.raw_title);
                let label = if s.worktree.is_empty() {
                    title
                } else if title.is_empty() {
                    s.worktree.clone()
                } else {
                    format!("{} · {title}", s.worktree)
                };
                let message = s
                    .reply
                    .clone()
                    .filter(|r| !r.trim().is_empty())
                    .or_else(|| preview_message(&s.preview))
                    .unwrap_or_else(|| DEFAULT_MESSAGE.into());
                let alert = Alert {
                    id,
                    handle: s.handle.clone(),
                    label: utf8_head(&label, LABEL_BYTES).to_owned(),
                    message: head_within(&message, MESSAGE_BYTES),
                };
                self.pending.insert(s.handle.clone(), alert.clone());
                events.push(AlertEvent::Raise(alert));
            } else if let Some(a) = self.pending.get(&s.handle)
                && state != AgentState::Waiting
            {
                events.push(AlertEvent::Clear {
                    id: a.id,
                    reason: ClearReason::Working,
                });
                self.pending.remove(&s.handle);
            }
        }
        // Sessions that disappeared: drop their state and any pending Alert.
        let gone: Vec<String> = self
            .pending
            .keys()
            .filter(|h| !seen.contains_key(*h))
            .cloned()
            .collect();
        for h in gone {
            if let Some(a) = self.pending.remove(&h) {
                events.push(AlertEvent::Clear {
                    id: a.id,
                    reason: ClearReason::Closed,
                });
            }
        }
        self.last = seen;
        events
    }

    /// Pending Alerts, oldest id first (to resend after HELLO).
    pub fn pending(&self) -> Vec<Alert> {
        let mut v: Vec<Alert> = self.pending.values().cloned().collect();
        v.sort_by_key(|a| a.id);
        v
    }

    /// Remove a pending Alert by id (opened or dismissed on the Device).
    pub fn take(&mut self, id: u8) -> Option<Alert> {
        let handle = self.pending.iter().find(|(_, a)| a.id == id)?.0.clone();
        self.pending.remove(&handle)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn title_states() {
        for t in [
            "◐ 修复",
            "◑ x",
            "◒ x",
            "◓ x",
            "⠋ Grok",
            "⠹ x",
            "✶ x",
            "✦ Gemini CLI",
        ] {
            assert_eq!(title_state(t), AgentState::Working, "{t}");
        }
        for t in [
            "✳ 任务列表",
            "◇ Gemini CLI",
            "Codex ready",
            "Codex - action required",
            "Cursor ready",
        ] {
            assert_eq!(title_state(t), AgentState::Waiting, "{t}");
        }
        assert_eq!(title_state("Codex"), AgentState::Working);
        for t in [
            "Terminal 1",
            "需要添加的脚本",
            "Setup",
            "",
            "Codex readyish",
            "zsh",
        ] {
            assert_eq!(title_state(t), AgentState::Unknown, "{t}");
        }
    }

    // Previews captured from `orca terminal list --json` (shortened).
    #[test]
    fn preview_messages() {
        let p = "1 项（改动小，直接修掉这次撞到的问题）。第 2 项会先摸清方式，出方案给你确认后再写。\n✻ Churned for 1m 12s · done 8:30 AM\n✔ Update installed · Restart to update\n────────────────────────────────\nPR，更新并合并 #247，打 tag 发版。";
        assert_eq!(
            preview_message(p).as_deref(),
            Some(
                "1 项（改动小，直接修掉这次撞到的问题）。第 2 项会先摸清方式，出方案给你确认后再写。"
            )
        );
        let p = "帮你起草\n12315 投诉文本和给苹果客服的书面说明。\n✻ Cooked for 1m 18s · done 7:16 PM\nnew task? /clear to save 280.4k tokens\n──────────────────────────────\nnew task? /clear to save 368.3k tokens";
        assert_eq!(
            preview_message(p).as_deref(),
            Some("帮你起草12315 投诉文本和给苹果客服的书面说明。")
        );
        let p = "or 33s · done 5:57 PM\n※ recap: 我们在写面试评价，下一步看你是否要重写。 (disable recaps in\n/config)\nnew task? /clear to save 192.3k tokens\n────────────────────────────";
        assert_eq!(
            preview_message(p).as_deref(),
            Some("我们在写面试评价，下一步看你是否要重写。")
        );
        assert_eq!(
            preview_message(
                "new task? /clear to save 1k tokens\n──────────────\n· ← 1 agent\nResume this session with:"
            ),
            None
        );
        assert_eq!(
            preview_message(
                "(base) teemo@localhost nook % claude '--dangerously-skip-permissions'"
            ),
            None
        );
        assert_eq!(
            preview_message("\u{1b}[1mDone\u{1b}[0m: all tests pass").as_deref(),
            Some("Done: all tests pass")
        );
        assert_eq!(preview_message(""), None);
        // The "※" scrolled out of the preview; one-character lines are noise.
        assert_eq!(
            preview_message("recap:\n我们在准备两份合同。\nnew task? /clear to save 1k").as_deref(),
            Some("我们在准备两份合同。")
        );
        assert_eq!(preview_message(":\n6"), None);
    }

    fn sess(h: &str, title: &str, agent: Option<&str>) -> OrcaSession {
        OrcaSession {
            handle: h.into(),
            leaf_id: String::new(),
            worktree_id: String::new(),
            worktree: "my-passport".into(),
            title: clean_title(title),
            raw_title: title.into(),
            agent: agent.map(Into::into),
            preview: "改好了，要我提交吗？\n✻ Baked for 3s".into(),
            reply: None,
        }
    }

    fn raise_ids(ev: &[AlertEvent]) -> Vec<u8> {
        ev.iter()
            .filter_map(|e| match e {
                AlertEvent::Raise(a) => Some(a.id),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn transitions_raise_and_clear() {
        let mut t = AlertTracker::default();
        let c = Some("claude");
        // First sight never alerts, even when already waiting.
        assert!(
            t.update(&[sess("a", "◐ 修复", c), sess("b", "✳ 文档", c)])
                .is_empty()
        );
        // a: working -> waiting raises; b stays waiting: nothing.
        let ev = t.update(&[sess("a", "✳ 修复", c), sess("b", "✳ 文档", c)]);
        assert_eq!(
            ev,
            [AlertEvent::Raise(Alert {
                id: 1,
                handle: "a".into(),
                label: "my-passport · 修复".into(),
                message: "改好了，要我提交吗？".into()
            })]
        );
        assert_eq!(t.pending().len(), 1);
        // Still waiting: no repeat.
        assert!(t.update(&[sess("a", "✳ 修复", c)]).is_empty());
        // Working again: cleared; waiting again: raised with the same id.
        assert_eq!(
            t.update(&[sess("a", "◓ 修复", c)]),
            [AlertEvent::Clear {
                id: 1,
                reason: ClearReason::Working
            }]
        );
        assert_eq!(raise_ids(&t.update(&[sess("a", "✳ 修复", c)])), [1]);
        // Still waiting (even while the user looks at it in Orca): kept.
        assert!(t.update(&[sess("a", "✳ 修复", c)]).is_empty());
        assert_eq!(t.pending().len(), 1);
        // Session closed: its pending Alert is cleared, no Alert for it.
        t.update(&[sess("a", "◐ 修复", c)]);
        assert_eq!(raise_ids(&t.update(&[sess("a", "✳ 修复", c)])), [1]);
        assert_eq!(
            t.update(&[]),
            [AlertEvent::Clear {
                id: 1,
                reason: ClearReason::Closed
            }]
        );
        assert!(t.update(&[sess("a", "✳ 修复", c)]).is_empty());
    }

    #[test]
    fn unknown_and_agentless_never_alert() {
        let mut t = AlertTracker::default();
        t.update(&[
            sess("x", "◐ build", None),
            sess("y", "Terminal 1", Some("claude")),
        ]);
        assert!(
            t.update(&[sess("x", "✳ build", None), sess("y", "✳ y", Some("claude"))])
                .is_empty()
        );
        // Codex through Orca's titles.
        let mut t = AlertTracker::default();
        t.update(&[sess("c", "Codex", Some("codex"))]);
        assert_eq!(
            raise_ids(&t.update(&[sess("c", "Codex ready", Some("codex"))])),
            [1]
        );
    }

    #[test]
    fn take_and_limits() {
        let mut t = AlertTracker::default();
        let c = Some("claude");
        let mut long = sess("a", "◐ x", c);
        t.update(&[long.clone()]);
        long.raw_title = format!("✳ {}", "很长的标题".repeat(20));
        long.preview = "结论".repeat(100);
        let ev = t.update(&[long]);
        let AlertEvent::Raise(a) = &ev[0] else {
            panic!()
        };
        assert!(a.label.len() <= LABEL_BYTES && a.message.len() <= MESSAGE_BYTES);
        assert!(a.message.starts_with("结论") && a.message.ends_with('…'));
        assert_eq!(t.take(a.id).map(|x| x.handle), Some("a".into()));
        assert_eq!(t.take(a.id), None);
        // Distinct sessions get distinct ids.
        let mut t = AlertTracker::default();
        t.update(&[sess("a", "◐ a", c), sess("b", "◐ b", c)]);
        assert_eq!(
            raise_ids(&t.update(&[sess("a", "✳ a", c), sess("b", "✳ b", c)])),
            [1, 2]
        );
    }

    fn screen(text: &str) -> Vec<String> {
        text.lines().map(str::to_owned).collect()
    }

    /// The shape of a rendered Claude Code screen after a turn (generic text).
    const SCREEN: &str = "\
⏺ Bash(cargo test --offline)
  ⎿  test result: ok. 12 passed
⏺ I updated the parser and the tests pass. The old path is
  removed.

  Should I also update the docs?
✻ Churned for 2m 14s · done 8:14 PM
※ recap: The parser now reads the screen. Next: decide whether to
  update the docs. (disable recaps in /config)
                                       ✔ Update installed · Restart to update
──────────────────────────────────────────────────────────
❯
──────────────────────────────────────────────────────────
  Project｜#12 example task
  [Model] │ ~/src/example │ main
  Context ██░░░░░░ 21%
  ✓ Bash ×13 | ✓ Read ×4
  ⏵⏵ auto mode on (shift+tab to cycle)
⏺ 1
◯ background task";

    #[test]
    fn screen_prefers_last_reply() {
        assert_eq!(
            screen_message(&screen(SCREEN)).as_deref(),
            Some(
                "I updated the parser and the tests pass. The old path is removed. Should I also update the docs?"
            )
        );
        // Table borders inside a reply are dropped.
        let table = "⏺ Results:\n  │ name │ ok │\n  │ a    │ 1  │\n────────────\n❯\n────────────";
        assert_eq!(
            screen_message(&screen(table)).as_deref(),
            Some("Results: name ok a 1")
        );
    }

    #[test]
    fn screen_falls_back_to_recap() {
        // The reply scrolled off screen: only the recap is left.
        let no_reply: String = SCREEN
            .lines()
            .filter(|l| !l.starts_with("⏺ I updated") && !l.starts_with("  The old path") && !l.starts_with("  Should I"))
            .collect::<Vec<_>>()
            .join("\n");
        let got = screen_message(&screen(&no_reply));
        assert!(got.is_some());
        // Tool-call blocks and their output are skipped.
        let tools = "⏺ Explaining first.\n⏺ Read(src/lib.rs)\n  ⎿  Read 40 lines\n✻ Baked for 3s\n────────────\n❯ \n────────────";
        assert_eq!(
            screen_message(&screen(tools)).as_deref(),
            Some("Explaining first.")
        );
        // Chinese wraps join without spaces.
        let zh = "⏺ 已经改好了，测试\n  全部通过。\n────────────\n❯\n────────────";
        assert_eq!(
            screen_message(&screen(zh)).as_deref(),
            Some("已经改好了，测试全部通过。")
        );
        // Nothing readable: no message (the caller falls back).
        let none = "✻ Churned for 1s\n────────────\n❯\n────────────\n⏺ 1";
        assert_eq!(screen_message(&screen(none)), None);
        assert_eq!(screen_message(&[]), None);
    }

    #[test]
    fn turn_watch_matches_tracker() {
        let c = Some("claude");
        let mut w = TurnWatch::default();
        assert!(
            w.turned_waiting(&[sess("a", "✳ x", c)]).is_empty(),
            "first sight"
        );
        w.turned_waiting(&[sess("a", "◐ x", c), sess("b", "◐ y", None)]);
        assert_eq!(
            w.turned_waiting(&[sess("a", "✳ x", c), sess("b", "✳ y", None)]),
            ["a"]
        );
        assert!(w.turned_waiting(&[sess("a", "✳ x", c)]).is_empty());
    }

    #[test]
    fn reply_from_screen_wins_and_is_capped() {
        let c = Some("claude");
        let mut t = AlertTracker::default();
        t.update(&[sess("a", "◐ x", c)]);
        let mut s = sess("a", "✳ x", c);
        s.reply = Some("word ".repeat(100));
        let ev = t.update(&[s]);
        let AlertEvent::Raise(a) = &ev[0] else {
            panic!()
        };
        assert!(a.message.len() <= MESSAGE_BYTES && a.message.ends_with('…'));
        assert!(a.message.len() > 300, "uses the longer budget");
    }
}
