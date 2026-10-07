//! Companion behaviour: the protocol state machine, independent of BLE,
//! Apple Speech, and macOS input injection (those sit behind traits).
//!
//! The driver feeds Device frames, recognizer events and time in; outgoing
//! frames collect in an outbox the driver drains and writes to the Device.
//!
//! The Target is stored and follows Mac focus: whenever a Supported App is
//! frontmost (checked every refresh and right before each Insert and
//! Submit), the Target becomes that app's Current Conversation; focus on any
//! other app leaves it unchanged. With no Target, or when the targeted Orca
//! Session is gone, Orca's Current Conversation takes its place (launching
//! Orca for an Insert or Submit). Insert, Submit and Undo bring the Target to
//! the front first; a Jump in the Device picker sets it.

use crate::alerts::{AlertEvent, AlertTracker, ClearReason};
use crate::audio::AudioAssembler;
use crate::config::{ORCA, ORCA_BUNDLE_ID, SUPPORTED_APPS, StoredTarget, supported_app};
use crate::orca::{OrcaApi, OrcaError, OrcaSession, OrcaSnapshot};
use crate::protocol::{
    APP_NONE, Action, CompanionFrame, DeviceFrame, FLAG_CURRENT, FLAG_NOT_RUNNING, FLAG_SUBLIST,
    LIST_ORCA, LIST_ROOT, PROTOCOL_VERSION, Status, StatusCode, TargetKind, utf8_head,
};
use crate::protocol::{NotesNotice, NotesState, alert_frames};
use crate::notes::{
    NotesError, NotesOp, NotesPhase, NotesReply, NotesStatus, RISK_BLUETOOTH_MIC,
    RISK_VOICE_ISOLATION,
};
use std::path::PathBuf;
use std::time::{Duration, Instant};
use unicode_segmentation::UnicodeSegmentation;

/// Minimum spacing of PARTIAL frames (about 5 per second).
pub const PARTIAL_INTERVAL: Duration = Duration::from_millis(200);
/// How long to wait for the final result after DICT_STOP.
pub const FINAL_TIMEOUT: Duration = Duration::from_secs(3);
/// Longest Orca Session list sent to the Device (its picker holds 24 rows).
pub const MAX_ORCA_ITEMS: usize = 24;
/// Label budgets that fit the Device's buffers (and always a frame):
/// list rows hold 71 bytes, the Target label 127.
pub const ITEM_LABEL_BYTES: usize = 71;
pub const STATE_LABEL_BYTES: usize = 127;
/// How long the periodic refresh reuses an Orca snapshot while Orca is
/// frontmost (its tabs can change any moment) and while it is in the
/// background (only the CLI or a Jump can change them then).
pub const ORCA_CACHE_FRONT: Duration = Duration::from_secs(2);
pub const ORCA_CACHE_BACKGROUND: Duration = Duration::from_secs(10);
/// Insert, Submit, HELLO, DICT_START, lists and Jumps ask Orca afresh, but
/// share one answer within an operation.
pub const ORCA_CACHE_FRESH: Duration = Duration::from_millis(500);

/// Shown on the Device (16 px body font: GB2312).
pub const LABEL_CURRENT_CONVERSATION: &str = "当前会话";
pub const LABEL_ORCA_WILL_LAUNCH: &str = "未运行，将自动启动";
pub const LABEL_NO_SESSION: &str = "没有会话";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InjectError {
    NotRunning,
    Permission,
    Failed(String),
}

/// macOS app control for Supported Apps other than Orca.
pub trait Injector {
    fn accessibility_trusted(&mut self) -> bool;
    fn is_running(&mut self, bundle_id: &str) -> bool;
    /// Bundle id of the frontmost app.
    fn frontmost_bundle_id(&mut self) -> Option<String>;
    /// Focused window title of a running app (its Target Title).
    fn window_title(&mut self, bundle_id: &str) -> Option<String>;
    /// Bring a running app to the front (Jump). Never launches it.
    fn activate(&mut self, bundle_id: &str) -> Result<(), InjectError>;
    /// Activate the app and paste `text` without Enter.
    fn insert(&mut self, bundle_id: &str, text: &str) -> Result<(), InjectError>;
    /// Activate the app and press Return.
    fn submit(&mut self, bundle_id: &str) -> Result<(), InjectError>;
    /// Activate the app and press Delete `count` times.
    fn delete_back(&mut self, bundle_id: &str, count: usize) -> Result<(), InjectError>;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecognizerHealth {
    Ready,
    NoPermission,
    Unavailable,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RecognizerStartError {
    NoPermission,
    Unavailable,
    Failed(String),
}

/// Streaming speech recognition. Results arrive later as [`RecogEvent`]s.
pub trait Recognizer {
    fn health(&mut self) -> RecognizerHealth;
    fn start(&mut self, dict: u8) -> Result<(), RecognizerStartError>;
    fn push(&mut self, pcm: &[i16]);
    /// No more audio (DICT_STOP); a final result should follow.
    fn finish(&mut self);
    fn cancel(&mut self);
}

/// Longest Dictation audio kept for the dictation note (10 minutes).
pub const MAX_RECORD_SAMPLES: usize = 16_000 * 600;

/// Where a delivered Dictation went, as the dictation notes group it: one
/// note per conversation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecordTarget {
    /// Stable conversation key: an Orca Session (by leaf id), or an app plus
    /// its window title (the conversation it shows; a rename starts a new note).
    pub key: String,
    /// Supported App name ("Orca", "微信", …).
    pub app: String,
    /// Conversation label (`<worktree> · <title>`, or the window title).
    pub label: String,
}

/// Dictation outcomes for the host's dictation notes, drained with
/// [`Companion::take_events`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DictationEvent {
    /// Text was inserted into `target`. `pcm` is the 16 kHz mono audio.
    Delivered {
        dict: u8,
        text: String,
        target: RecordTarget,
        pcm: Vec<i16>,
    },
    /// The latest delivered Dictation was undone on the Device.
    Undone { dict: u8 },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RecogEvent {
    Partial { dict: u8, text: String },
    Final { dict: u8, text: String },
    Error { dict: u8, message: String },
}

/// Where a Segment went, so UNDO hits the same place.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Dest {
    App { bundle_id: String },
    Orca { handle: String },
}

#[derive(Debug, Clone)]
struct Segment {
    dict: u8,
    dest: Dest,
    /// User-perceived characters (grapheme clusters) = Delete presses.
    chars: usize,
}

#[derive(Debug, PartialEq, Eq)]
enum Phase {
    Recording,
    Finalizing { deadline: Instant },
}

#[derive(Debug)]
struct Dictation {
    id: u8,
    audio: AudioAssembler,
    phase: Phase,
    /// Recognizer could not start or failed: the status to report.
    failure: Option<Status>,
    /// Utterances Speech already finalized or restarted after a pause.
    committed: String,
    /// Latest raw transcription of the current utterance.
    raw: String,
    /// `committed` + `raw`: the whole Dictation so far.
    best: String,
    sent_partial: String,
    last_partial_at: Option<Instant>,
    partial_pending: bool,
    /// The decoded audio, for the dictation note.
    pcm: Vec<i16>,
}

/// The Target as the Device shows it (TARGET_STATE).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TargetView {
    pub status: Status,
    pub kind: TargetKind,
    /// Supported App index for the Device's logo, or [`APP_NONE`].
    pub app: u8,
    pub label: String,
}

/// How a resolution may touch Orca.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Need {
    /// Periodic refresh: reuse a recent Orca snapshot, never launch.
    Refresh,
    /// Show the Target now: ask Orca afresh, never launch.
    Show,
    /// Insert, Submit or Undo: ask Orca afresh and launch it when needed.
    Act,
}

/// The resolved Target: where text goes (or why it cannot) and its view.
#[derive(Debug)]
struct Resolved {
    dest: Result<Dest, Status>,
    view: TargetView,
}

fn orca_target(s: &OrcaSession) -> StoredTarget {
    StoredTarget::Orca {
        handle: s.handle.clone(),
        leaf_id: s.leaf_id.clone(),
        worktree: s.worktree.clone(),
        title: s.title.clone(),
    }
}

/// `"<worktree> · <title>"` of a stored Orca Session.
fn stored_orca_label(worktree: &str, title: &str) -> String {
    match (worktree.is_empty(), title.is_empty()) {
        (false, false) => format!("{worktree} · {title}"),
        (false, true) => worktree.to_owned(),
        _ => title.to_owned(),
    }
}

/// Join two utterances; a space only between ASCII words.
fn join_utterances(a: &str, b: &str) -> String {
    let (a, b) = (a.trim_end(), b.trim());
    if a.is_empty() {
        return b.to_owned();
    }
    if b.is_empty() {
        return a.to_owned();
    }
    // A pause ended the first utterance: close it so the two don't run together.
    let a = end_sentence(a);
    let a = a.trim_end();
    let space = a.chars().last().is_some_and(|c| c.is_ascii_punctuation() || c.is_ascii_alphanumeric())
        && b.chars().next().is_some_and(|c| c.is_ascii_alphanumeric());
    if space {
        format!("{a} {b}")
    } else {
        format!("{a}{b}")
    }
}

/// Characters that already end (or properly pause) a sentence.
const SENTENCE_END: &str = "。！？.!?…；;：:，,、\"'”’」』）)】]";

/// Make a Segment end with punctuation so consecutive Inserts stay separate
/// sentences: Chinese text gets "。", English text gets ". " (with a space
/// so the next Insert doesn't glue onto it).
fn end_sentence(text: &str) -> String {
    let t = text.trim_end();
    let Some(last) = t.chars().last() else {
        return String::new();
    };
    if SENTENCE_END.contains(last) {
        return if last.is_ascii() { format!("{t} ") } else { t.to_owned() };
    }
    // A Chinese sentence that happens to end in an English word still gets "。".
    let chinese = t.chars().any(|c| ('\u{4e00}'..='\u{9fff}').contains(&c));
    if last.is_ascii() && !chinese { format!("{t}. ") } else { format!("{t}。") }
}


/// Whether `new` starts a different utterance rather than revising `prev`.
/// Revisions keep a common prefix; a restart after a pause shares almost none
/// and is shorter than what came before.
fn restarted_utterance(prev: &str, new: &str) -> bool {
    let prev_len = prev.chars().count();
    if prev_len < 4 {
        return false;
    }
    let common = prev
        .chars()
        .zip(new.chars())
        .take_while(|(a, b)| a == b)
        .count();
    common < 2 && new.chars().count() < prev_len
}

/// Count of user-perceived characters (Chinese characters count 1 each).
pub fn undo_count(text: &str) -> usize {
    text.graphemes(true).count()
}

fn cap_chars(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_owned()
    } else {
        let mut out: String = s.chars().take(max.saturating_sub(1)).collect();
        out.push('…');
        out
    }
}

/// `"<name> · <title>"` within `max_bytes`, keeping the name and cutting the
/// title (with `…`) so the Device never has to drop the start of a label.
pub fn compose_label(name: &str, title: Option<&str>, max_bytes: usize) -> String {
    let name = cap_chars(name, 16);
    let name = utf8_head(&name, max_bytes).to_owned();
    let Some(title) = title.map(str::trim).filter(|t| !t.is_empty()) else {
        return name;
    };
    const SEP: &str = " · ";
    let room = max_bytes.saturating_sub(name.len() + SEP.len());
    if room < 7 {
        return name;
    }
    let title = if title.len() <= room {
        title.to_owned()
    } else {
        format!("{}…", utf8_head(title, room - '…'.len_utf8()))
    };
    format!("{name}{SEP}{title}")
}

/// The Voice Notes Recording as last known, plus the ops queued for the
/// worker thread. Independent of Dictation, Target and Undo.
#[derive(Debug)]
struct Notes {
    state: NotesState,
    /// Recording time as of `at` (it advances only while recording).
    elapsed_ms: u64,
    at: Instant,
    action_pending: bool,
    poll_pending: bool,
    /// Last NOTES_STATE sent: state, elapsed seconds, when.
    sent: Option<(NotesState, u32, Instant)>,
    ops: Vec<NotesOp>,
}

impl Notes {
    fn new() -> Self {
        Self {
            state: NotesState::Idle,
            elapsed_ms: 0,
            at: Instant::now(),
            action_pending: false,
            poll_pending: false,
            sent: None,
            ops: Vec::new(),
        }
    }

    fn elapsed_s(&self, now: Instant) -> u32 {
        let mut ms = self.elapsed_ms;
        if self.state == NotesState::Recording {
            ms += now.saturating_duration_since(self.at).as_millis() as u64;
        }
        (ms / 1000).min(u64::from(u32::MAX)) as u32
    }

    fn set(&mut self, state: NotesState, elapsed_ms: u64, now: Instant) {
        self.state = state;
        self.elapsed_ms = elapsed_ms;
        self.at = now;
    }
}

/// How far the Device's own count may drift before NOTES_STATE is resent.
pub const NOTES_DRIFT_S: u32 = 2;

fn start_notice(risks: &[String]) -> NotesNotice {
    match risks.first().map(String::as_str) {
        None => NotesNotice::Started,
        Some(RISK_VOICE_ISOLATION) => NotesNotice::RiskVoiceIsolation,
        Some(RISK_BLUETOOTH_MIC) => NotesNotice::RiskBluetooth,
        Some(_) => NotesNotice::RiskOther,
    }
}

fn start_error_notice(e: &NotesError) -> NotesNotice {
    match e {
        NotesError::NotInstalled => NotesNotice::NotInstalled,
        NotesError::LaunchFailed | NotesError::NotRunning => NotesNotice::LaunchFailed,
        NotesError::ControlDisabled => NotesNotice::ControlDisabled,
        NotesError::Failed(_) => NotesNotice::StartFailed,
    }
}

pub struct Companion<I: Injector, R: Recognizer, O: OrcaApi> {
    pub injector: I,
    pub recognizer: R,
    pub orca: O,
    /// Last Orca snapshot and when it was taken.
    orca_cache: Option<(Instant, OrcaSnapshot)>,
    /// Rows of the last Orca list sent, for TARGET_SELECT.
    orca_rows: Vec<OrcaSession>,
    /// The Target; `None` until first resolved.
    target: Option<StoredTarget>,
    /// Where the Target is persisted (`None` in tests).
    store: Option<PathBuf>,
    /// Last TARGET_STATE sent; `None` until HELLO.
    view: Option<TargetView>,
    dictation: Option<Dictation>,
    last_segment: Option<Segment>,
    orca_failed: bool,
    last_status: Option<StatusCode>,
    outbox: Vec<CompanionFrame>,
    events: Vec<DictationEvent>,
    notes: Notes,
    alerts: AlertTracker,
}

impl<I: Injector, R: Recognizer, O: OrcaApi> Companion<I, R, O> {
    /// `target` is the stored Target; `store` where to persist changes.
    pub fn new(
        injector: I,
        recognizer: R,
        orca: O,
        target: Option<StoredTarget>,
        store: Option<PathBuf>,
    ) -> Self {
        Self {
            injector,
            recognizer,
            orca,
            orca_cache: None,
            orca_rows: Vec::new(),
            target,
            store,
            view: None,
            dictation: None,
            last_segment: None,
            orca_failed: false,
            last_status: None,
            outbox: Vec::new(),
            events: Vec::new(),
            notes: Notes::new(),
            alerts: AlertTracker::default(),
        }
    }

    /// Frames to write to the Device, in order.
    pub fn take_outbox(&mut self) -> Vec<CompanionFrame> {
        std::mem::take(&mut self.outbox)
    }

    /// Dictation outcomes since the last call, in order.
    pub fn take_events(&mut self) -> Vec<DictationEvent> {
        std::mem::take(&mut self.events)
    }

    pub fn target(&self) -> Option<&StoredTarget> {
        self.target.as_ref()
    }

    pub fn dictation_active(&self) -> bool {
        self.dictation.is_some()
    }

    fn send(&mut self, f: CompanionFrame) {
        self.outbox.push(f);
    }

    // ----- link -----------------------------------------------------------

    /// The BLE link dropped: abandon any Dictation without inserting.
    pub fn on_disconnected(&mut self) {
        if let Some(d) = self.dictation.take() {
            log::info!("link lost during dictation {}; abandoned", d.id);
            self.recognizer.cancel();
        }
        self.outbox.clear();
        self.last_status = None;
        self.view = None;
        self.orca_rows.clear();
        // The Device forgets its Alerts; start tracking afresh on relink.
        self.alerts = AlertTracker::default();
    }

    // ----- frames ---------------------------------------------------------

    pub fn handle_frame(&mut self, frame: DeviceFrame, now: Instant) {
        match frame {
            DeviceFrame::Hello { ver, fw } => self.on_hello(ver, &fw, now),
            DeviceFrame::DictStart { dict } => self.on_dict_start(dict, now),
            DeviceFrame::Audio(a) => {
                if let Some(d) = self.dictation.as_mut()
                    && d.phase == Phase::Recording
                    && d.failure.is_none()
                    && let Some(pcm) = d.audio.push(&a)
                {
                    if d.pcm.len() < MAX_RECORD_SAMPLES {
                        d.pcm.extend_from_slice(&pcm);
                    }
                    self.recognizer.push(&pcm);
                }
            }
            DeviceFrame::DictStop { dict } => self.on_dict_stop(dict, now),
            DeviceFrame::DictCancel { dict } => self.on_dict_cancel(dict),
            DeviceFrame::Submit => self.on_submit(now),
            DeviceFrame::Undo => self.on_undo(now),
            DeviceFrame::TargetsReq { list } => self.on_targets_req(list, now),
            DeviceFrame::TargetSelect { list, index } => self.on_target_select(list, index, now),
            DeviceFrame::NotesToggle => self.on_notes_toggle(now),
            DeviceFrame::AlertOpen { id } => self.on_alert_open(id, now),
            DeviceFrame::AlertDismiss { id } => {
                self.alerts.take(id);
            }
        }
    }

    fn on_hello(&mut self, ver: u8, fw: &str, now: Instant) {
        log::info!("Device HELLO ver={ver} fw={fw:?}");
        if ver != PROTOCOL_VERSION {
            log::warn!("Device protocol version {ver}, Companion speaks {PROTOCOL_VERSION}");
        }
        // A new HELLO means the Device (re)started: any old Dictation is gone.
        if let Some(d) = self.dictation.take() {
            log::info!("dictation {} abandoned by HELLO", d.id);
            self.recognizer.cancel();
        }
        self.send(CompanionFrame::HelloAck {
            ver: PROTOCOL_VERSION,
        });
        let r = self.resolve(Need::Show, true, now);
        self.send_state(r.view);
        self.last_status = None;
        self.report_health();
        self.notes.sent = None;
        self.send_notes(NotesNotice::None, now);
        self.notes_poll();
        for a in self.alerts.pending() {
            self.send_alert(AlertEvent::Raise(a));
        }
    }

    fn on_dict_start(&mut self, dict: u8, now: Instant) {
        log::info!("dictation {dict} started");
        if let Some(old) = self.dictation.take() {
            log::warn!("dictation {} replaced by {dict}", old.id);
            self.recognizer.cancel();
        }
        // After a new Dictation starts there is nothing to undo.
        self.last_segment = None;
        let r = self.resolve(Need::Show, true, now);
        self.send_state(r.view);
        let failure = match self.recognizer.start(dict) {
            Ok(()) => None,
            Err(e) => {
                log::error!("recognizer start failed: {e:?}");
                Some(match e {
                    RecognizerStartError::NoPermission => Status::Permission,
                    _ => Status::RecognizerError,
                })
            }
        };
        self.report_health();
        self.dictation = Some(Dictation {
            id: dict,
            audio: AudioAssembler::new(dict),
            phase: Phase::Recording,
            failure,
            committed: String::new(),
            raw: String::new(),
            best: String::new(),
            sent_partial: String::new(),
            last_partial_at: None,
            partial_pending: false,
            pcm: Vec::new(),
        });
    }

    fn on_dict_stop(&mut self, dict: u8, now: Instant) {
        let Some(d) = self.dictation.as_mut().filter(|d| d.id == dict) else {
            log::warn!("DICT_STOP for unknown dictation {dict}");
            self.send(CompanionFrame::Result {
                dict,
                status: Status::Empty,
                text: String::new(),
            });
            return;
        };
        if d.phase != Phase::Recording {
            return;
        }
        log::info!(
            "dictation {dict} stopped: {} frames, {} lost, {} dropped",
            d.audio.frames,
            d.audio.lost_frames,
            d.audio.dropped_frames
        );
        if let Some(status) = d.failure {
            self.dictation = None;
            self.send(CompanionFrame::Result {
                dict,
                status,
                text: String::new(),
            });
            return;
        }
        d.phase = Phase::Finalizing {
            deadline: now + FINAL_TIMEOUT,
        };
        self.recognizer.finish();
    }

    fn on_dict_cancel(&mut self, dict: u8) {
        log::info!("dictation {dict} cancelled");
        if self.dictation.as_ref().is_some_and(|d| d.id == dict) {
            self.dictation = None;
            self.recognizer.cancel();
        }
        self.send(CompanionFrame::Result {
            dict,
            status: Status::Cancelled,
            text: String::new(),
        });
    }

    // ----- recognizer -----------------------------------------------------

    pub fn handle_recog(&mut self, ev: RecogEvent, now: Instant) {
        let id = match &ev {
            RecogEvent::Partial { dict, .. }
            | RecogEvent::Final { dict, .. }
            | RecogEvent::Error { dict, .. } => *dict,
        };
        let Some(d) = self.dictation.as_mut().filter(|d| d.id == id) else {
            return;
        };
        match ev {
            RecogEvent::Partial { text, .. } => {
                // After a long pause Speech may start a fresh transcription
                // without finalizing: keep what was said before.
                if restarted_utterance(&d.raw, &text) {
                    d.committed = join_utterances(&d.committed, &d.raw);
                }
                d.raw = text;
                d.best = join_utterances(&d.committed, &d.raw);
                d.partial_pending = d.best != d.sent_partial;
                self.flush_partial(now);
            }
            RecogEvent::Final { text, .. } => {
                log::info!(
                    "dictation {id}: final {} chars (utterance {} chars, committed {} chars)",
                    text.chars().count(),
                    d.raw.chars().count(),
                    d.committed.chars().count()
                );
                // Speech sometimes finalizes with an empty transcription after
                // endAudio; keep the recognized Partial Text instead.
                let utterance = if text.trim().is_empty() {
                    std::mem::take(&mut d.raw)
                } else {
                    text
                };
                d.committed = join_utterances(&d.committed, &utterance);
                d.raw.clear();
                d.best = d.committed.clone();
                if matches!(d.phase, Phase::Finalizing { .. }) {
                    let best = d.best.clone();
                    self.complete(best, now);
                } else {
                    // Apple finalized early (after a long pause): the task is
                    // over, so keep listening with a new one.
                    d.partial_pending = d.best != d.sent_partial;
                    log::info!("dictation {id}: early final, restarting recognizer");
                    if let Err(e) = self.recognizer.start(id) {
                        log::error!("recognizer restart failed: {e:?}");
                    }
                    self.flush_partial(now);
                }
            }
            RecogEvent::Error { message, .. } => {
                log::warn!("recognizer error in dictation {id}: {message}");
                if matches!(d.phase, Phase::Finalizing { .. }) {
                    if d.best.trim().is_empty() {
                        // "No speech detected" ends here too: report EMPTY
                        // unless nothing at all could be recognized.
                        let dict = d.id;
                        self.dictation = None;
                        let status = if message.contains("1110")
                            || message.to_lowercase().contains("no speech")
                        {
                            Status::Empty
                        } else {
                            Status::RecognizerError
                        };
                        self.send(CompanionFrame::Result {
                            dict,
                            status,
                            text: String::new(),
                        });
                    } else {
                        let best = d.best.clone();
                        self.complete(best, now);
                    }
                } else if d.best.trim().is_empty() {
                    d.failure = Some(Status::RecognizerError);
                }
            }
        }
    }

    fn flush_partial(&mut self, now: Instant) {
        let Some(d) = self.dictation.as_mut() else {
            return;
        };
        if !d.partial_pending {
            return;
        }
        if let Some(t) = d.last_partial_at
            && now < t + PARTIAL_INTERVAL
        {
            return;
        }
        d.partial_pending = false;
        d.last_partial_at = Some(now);
        d.sent_partial = d.best.clone();
        let f = CompanionFrame::Partial {
            dict: d.id,
            text: d.best.clone(),
        };
        self.send(f);
    }

    /// Run timers. Returns when to call again.
    pub fn poll(&mut self, now: Instant) -> Option<Instant> {
        self.flush_partial(now);
        let d = self.dictation.as_ref()?;
        if let Phase::Finalizing { deadline } = d.phase
            && now >= deadline
        {
            log::warn!("final result timed out; using best partial");
            let best = d.best.clone();
            self.recognizer.cancel();
            self.complete(best, now);
            return None;
        }
        let d = self.dictation.as_ref()?;
        let partial_due = if d.partial_pending {
            d.last_partial_at.map(|t| t + PARTIAL_INTERVAL)
        } else {
            None
        };
        let final_due = match d.phase {
            Phase::Finalizing { deadline } => Some(deadline),
            Phase::Recording => None,
        };
        match (partial_due, final_due) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (a, b) => a.or(b),
        }
    }

    /// Finish the Dictation: Insert the Segment into the Target and reply
    /// RESULT.
    fn complete(&mut self, text: String, now: Instant) {
        let Some(d) = self.dictation.take() else {
            return;
        };
        let dict = d.id;
        let text = end_sentence(text.trim());
        if text.is_empty() {
            log::info!("dictation {dict}: nothing recognized");
            self.send(CompanionFrame::Result {
                dict,
                status: Status::Empty,
                text: String::new(),
            });
            return;
        }
        let r = self.resolve(Need::Act, true, now);
        let status = match r.dest {
            Err(status) => status,
            Ok(dest) => {
                log::info!(
                    "dictation {dict}: delivering {} chars to {dest:?}",
                    text.chars().count()
                );
                let st = self.deliver(&dest, &text, now);
                if st == Status::Ok {
                    let target = self.record_target(&dest);
                    self.events.push(DictationEvent::Delivered {
                        dict,
                        text: text.clone(),
                        target,
                        pcm: d.pcm,
                    });
                    self.last_segment = Some(Segment {
                        dict,
                        dest,
                        chars: undo_count(&text),
                    });
                }
                st
            }
        };
        self.send_state(r.view);
        self.report_health();
        log::info!(
            "dictation {dict}: {status:?} ({} chars)",
            text.chars().count()
        );
        let sent = if status == Status::Ok {
            text
        } else {
            String::new()
        };
        self.send(CompanionFrame::Result {
            dict,
            status,
            text: sent,
        });
    }

    /// The conversation `dest` stands for, for the dictation notes.
    fn record_target(&mut self, dest: &Dest) -> RecordTarget {
        match dest {
            Dest::Orca { handle } => {
                let name = SUPPORTED_APPS[ORCA].name.to_owned();
                match &self.target {
                    Some(StoredTarget::Orca {
                        handle: h,
                        leaf_id,
                        worktree,
                        title,
                    }) if h == handle => RecordTarget {
                        key: format!(
                            "orca:{}",
                            if leaf_id.is_empty() { handle } else { leaf_id }
                        ),
                        app: name,
                        label: stored_orca_label(worktree, title),
                    },
                    _ => RecordTarget {
                        key: format!("orca:{handle}"),
                        app: name,
                        label: handle.clone(),
                    },
                }
            }
            Dest::App { bundle_id } => {
                let index = supported_app(bundle_id);
                let canonical = index.map_or(bundle_id.as_str(), |i| SUPPORTED_APPS[i].bundle_id);
                let name = index.map_or("?", |i| SUPPORTED_APPS[i].name).to_owned();
                let title = self
                    .injector
                    .window_title(bundle_id)
                    .map(|t| t.trim().to_owned())
                    .filter(|t| !t.is_empty());
                match title {
                    Some(t) => RecordTarget {
                        key: format!("app:{canonical}:{t}"),
                        app: name,
                        label: t,
                    },
                    None => RecordTarget {
                        key: format!("app:{canonical}"),
                        label: name.clone(),
                        app: name,
                    },
                }
            }
        }
    }

    fn deliver(&mut self, dest: &Dest, text: &str, now: Instant) -> Status {
        match dest {
            Dest::App { bundle_id } => {
                let b = bundle_id.clone();
                self.inject(|i| i.insert(&b, text))
            }
            Dest::Orca { handle } => {
                let r = self
                    .orca_front(handle, now)
                    .and_then(|()| self.orca.send_text(handle, text));
                self.orca_status(r)
            }
        }
    }

    /// Bring an Orca Session to the front: switch its tab, activate Orca.
    fn orca_front(&mut self, handle: &str, now: Instant) -> Result<(), OrcaError> {
        self.orca.switch(handle)?;
        if let Err(e) = self.injector.activate(ORCA_BUNDLE_ID) {
            log::warn!("activate Orca: {e:?}");
        }
        // Orca reports the new active tab a moment later; until the next
        // fresh snapshot, treat this session as its Current Conversation so
        // following focus to Orca keeps it.
        if let Some((at, snap)) = self.orca_cache.as_mut()
            && let Some(i) = snap.sessions.iter().position(|s| s.handle == handle)
        {
            let s = snap.sessions.remove(i);
            snap.sessions.insert(0, s);
            snap.has_current = true;
            *at = now;
        }
        Ok(())
    }

    fn inject(&mut self, f: impl FnOnce(&mut I) -> Result<(), InjectError>) -> Status {
        inject_status(f(&mut self.injector))
    }

    fn orca_status(&mut self, r: Result<(), OrcaError>) -> Status {
        match r {
            Ok(()) => {
                self.orca_failed = false;
                Status::Ok
            }
            Err(e) => {
                // The terminal may be gone: ask Orca afresh next time.
                self.orca_cache = None;
                self.orca_error(&e);
                Status::TargetUnavailable
            }
        }
    }

    fn orca_error(&mut self, e: &OrcaError) {
        log::warn!("{e}");
        if matches!(e, OrcaError::Unavailable(_)) {
            self.orca_failed = true;
        }
    }

    // ----- actions --------------------------------------------------------

    fn on_submit(&mut self, now: Instant) {
        let r = self.resolve(Need::Act, true, now);
        let status = match r.dest {
            Err(s) => s,
            Ok(Dest::App { bundle_id }) => self.inject(|i| i.submit(&bundle_id)),
            Ok(Dest::Orca { handle }) => {
                let r = self
                    .orca_front(&handle, now)
                    .and_then(|()| self.orca.send_enter(&handle));
                self.orca_status(r)
            }
        };
        // The submitted Segment has left the input, so backspaces would hit new text.
        if status == Status::Ok {
            self.last_segment = None;
        }
        self.send_state(r.view);
        self.report_health();
        self.send(CompanionFrame::ActionResult {
            action: Action::Submit,
            status,
        });
    }

    /// UNDO goes to where the latest Segment went, whatever the Target is now.
    fn on_undo(&mut self, now: Instant) {
        let Some(seg) = self.last_segment.clone() else {
            self.send(CompanionFrame::ActionResult {
                action: Action::Undo,
                status: Status::NothingToUndo,
            });
            return;
        };
        let n = seg.chars;
        let status = match &seg.dest {
            Dest::App { bundle_id } => {
                let b = bundle_id.clone();
                if self.injector.is_running(&b) {
                    self.inject(|i| i.delete_back(&b, n))
                } else {
                    Status::TargetUnavailable
                }
            }
            Dest::Orca { handle } => {
                let r = self
                    .orca_front(handle, now)
                    .and_then(|()| self.orca.send_backspaces(handle, n));
                self.orca_status(r)
            }
        };
        if status == Status::Ok {
            self.last_segment = None;
            self.events.push(DictationEvent::Undone { dict: seg.dict });
        }
        self.report_health();
        self.send(CompanionFrame::ActionResult {
            action: Action::Undo,
            status,
        });
    }

    // ----- Target ---------------------------------------------------------

    fn set_target(&mut self, t: StoredTarget) {
        if self.target.as_ref() == Some(&t) {
            return;
        }
        log::info!("Target set to {t:?}");
        if let Some(p) = &self.store {
            t.save(p);
        }
        self.target = Some(t);
    }

    /// Mac focus on a Supported App sets the Target to its Current
    /// Conversation; focus elsewhere leaves it unchanged.
    fn follow_focus(&mut self, need: Need, now: Instant) {
        let Some(i) = self
            .injector
            .frontmost_bundle_id()
            .as_deref()
            .and_then(supported_app)
        else {
            return;
        };
        if i != ORCA {
            self.set_target(StoredTarget::App {
                bundle_id: SUPPORTED_APPS[i].bundle_id.to_owned(),
            });
            return;
        }
        match self.orca_snapshot(need, true, now) {
            Ok(snap) => {
                if let Some(s) = snap.current() {
                    let t = orca_target(s);
                    self.set_target(t);
                }
            }
            Err(e) => self.orca_error(&e),
        }
    }

    /// Resolve the Target (after following focus when `follow`).
    fn resolve(&mut self, need: Need, follow: bool, now: Instant) -> Resolved {
        if follow {
            self.follow_focus(need, now);
        }
        match self.target.clone() {
            Some(StoredTarget::App { bundle_id }) => {
                let index = supported_app(&bundle_id);
                let name = index.map_or("?", |i| SUPPORTED_APPS[i].name);
                let app = index.map_or(APP_NONE, |i| i as u8);
                if !self.injector.is_running(&bundle_id) {
                    // Never launched: report it.
                    return Resolved {
                        dest: Err(Status::TargetUnavailable),
                        view: TargetView {
                            status: Status::TargetUnavailable,
                            kind: TargetKind::App,
                            app,
                            label: compose_label(name, None, STATE_LABEL_BYTES),
                        },
                    };
                }
                let status = if self.injector.accessibility_trusted() {
                    Status::Ok
                } else {
                    Status::Permission
                };
                let title = self.injector.window_title(&bundle_id);
                Resolved {
                    dest: Ok(Dest::App { bundle_id }),
                    view: TargetView {
                        status,
                        kind: TargetKind::App,
                        app,
                        label: compose_label(name, title.as_deref(), STATE_LABEL_BYTES),
                    },
                }
            }
            stored => self.resolve_orca(stored, need, now),
        }
    }

    /// The targeted Orca Session, or Orca's Current Conversation when there
    /// is no Target or the session is gone (stored silently). Launches Orca
    /// for [`Need::Act`].
    fn resolve_orca(&mut self, stored: Option<StoredTarget>, need: Need, now: Instant) -> Resolved {
        let name = SUPPORTED_APPS[ORCA].name;
        let (handle, leaf_id, stored_label) = match &stored {
            Some(StoredTarget::Orca {
                handle,
                leaf_id,
                worktree,
                title,
            }) => (
                handle.clone(),
                leaf_id.clone(),
                Some(stored_orca_label(worktree, title)),
            ),
            _ => (String::new(), String::new(), None),
        };
        let label = |extra: Option<&str>| compose_label(name, extra, STATE_LABEL_BYTES);
        let unavailable = |label: String| Resolved {
            dest: Err(Status::TargetUnavailable),
            view: TargetView {
                status: Status::TargetUnavailable,
                kind: TargetKind::Orca,
                app: ORCA as u8,
                label,
            },
        };
        if !self.injector.is_running(ORCA_BUNDLE_ID) {
            self.orca_cache = None;
            if need != Need::Act {
                // Usable: the next Insert or Submit launches Orca.
                let extra = stored_label.as_deref().unwrap_or(LABEL_ORCA_WILL_LAUNCH);
                return Resolved {
                    dest: Err(Status::TargetUnavailable),
                    view: TargetView {
                        status: Status::Ok,
                        kind: TargetKind::Orca,
                        app: ORCA as u8,
                        label: label(Some(extra)),
                    },
                };
            }
            if let Err(e) = self.orca.open() {
                self.orca_error(&e);
                return unavailable(label(stored_label.as_deref()));
            }
        }
        let snap = match self.orca_snapshot(need, false, now) {
            Ok(s) => s,
            Err(e) => {
                self.orca_error(&e);
                return unavailable(label(stored_label.as_deref()));
            }
        };
        let found = snap
            .sessions
            .iter()
            .find(|s| !handle.is_empty() && s.handle == handle)
            .or_else(|| {
                snap.sessions
                    .iter()
                    .find(|s| !leaf_id.is_empty() && s.leaf_id == leaf_id)
            })
            .or_else(|| snap.current())
            .cloned();
        match found {
            Some(s) => {
                self.set_target(orca_target(&s));
                Resolved {
                    dest: Ok(Dest::Orca {
                        handle: s.handle.clone(),
                    }),
                    view: TargetView {
                        status: Status::Ok,
                        kind: TargetKind::Orca,
                        app: ORCA as u8,
                        label: label(Some(&s.label())),
                    },
                }
            }
            None => unavailable(label(Some(LABEL_NO_SESSION))),
        }
    }

    fn orca_snapshot(
        &mut self,
        need: Need,
        orca_front: bool,
        now: Instant,
    ) -> Result<OrcaSnapshot, OrcaError> {
        if let Some((at, snap)) = &self.orca_cache {
            let max_age = match need {
                Need::Refresh if orca_front => ORCA_CACHE_FRONT,
                Need::Refresh => ORCA_CACHE_BACKGROUND,
                _ => ORCA_CACHE_FRESH,
            };
            if now.saturating_duration_since(*at) < max_age {
                return Ok(snap.clone());
            }
        }
        let snap = self.orca.snapshot()?;
        self.orca_failed = false;
        self.orca_cache = Some((now, snap.clone()));
        Ok(snap)
    }

    fn send_state(&mut self, v: TargetView) {
        log::info!("Target: {} ({:?})", v.label, v.status);
        self.view = Some(v.clone());
        self.send(CompanionFrame::TargetState {
            status: v.status,
            kind: v.kind,
            app: v.app,
            label: v.label,
        });
    }

    /// Index of the Supported App the Target is in (Orca without one).
    fn target_app(&self) -> usize {
        match &self.target {
            Some(StoredTarget::App { bundle_id }) => supported_app(bundle_id).unwrap_or(ORCA),
            _ => ORCA,
        }
    }

    // ----- picker (Jump) --------------------------------------------------

    fn item(&mut self, list: u8, index: usize, count: usize, flags: u8, label: String) {
        self.send(CompanionFrame::TargetItem {
            list,
            index: index as u8,
            count: count as u8,
            flags,
            label,
        });
    }

    fn on_targets_req(&mut self, list: u8, now: Instant) {
        let count = match list {
            LIST_ROOT => {
                let n = SUPPORTED_APPS.len();
                let current = self.target_app();
                for (i, app) in SUPPORTED_APPS.iter().enumerate() {
                    let mut flags = FLAG_SUBLIST;
                    if !self.injector.is_running(app.bundle_id) {
                        flags |= FLAG_NOT_RUNNING;
                    }
                    if i == current {
                        flags |= FLAG_CURRENT;
                    }
                    self.item(
                        list,
                        i,
                        n,
                        flags,
                        compose_label(app.name, None, ITEM_LABEL_BYTES),
                    );
                }
                n
            }
            LIST_ORCA => self.list_orca(now),
            l if usize::from(l) <= SUPPORTED_APPS.len() => {
                let app = SUPPORTED_APPS[usize::from(l) - 1];
                if self.injector.is_running(app.bundle_id) {
                    let title = self.injector.window_title(app.bundle_id);
                    let label = compose_label(
                        LABEL_CURRENT_CONVERSATION,
                        title.as_deref(),
                        ITEM_LABEL_BYTES,
                    );
                    self.item(list, 0, 1, FLAG_CURRENT, label);
                    1
                } else {
                    0 // never launched from the picker
                }
            }
            other => {
                log::warn!("TARGETS_REQ for unknown list {other}");
                0
            }
        };
        self.send(CompanionFrame::TargetEnd {
            list,
            count: count as u8,
        });
    }

    /// Orca Sessions, Current Conversation first. Launches Orca if needed.
    fn list_orca(&mut self, now: Instant) -> usize {
        self.orca_rows.clear();
        if !self.injector.is_running(ORCA_BUNDLE_ID)
            && let Err(e) = self.orca.open()
        {
            self.orca_error(&e);
            self.report_health();
            return 0;
        }
        let snap = match self.orca_snapshot(Need::Show, false, now) {
            Ok(s) => s,
            Err(e) => {
                self.orca_error(&e);
                self.report_health();
                return 0;
            }
        };
        let rows: Vec<_> = snap.sessions.iter().take(MAX_ORCA_ITEMS).cloned().collect();
        for (i, s) in rows.iter().enumerate() {
            let flags = if i == 0 && snap.has_current {
                FLAG_CURRENT
            } else {
                0
            };
            let label = compose_label(&s.worktree, Some(&s.title), ITEM_LABEL_BYTES);
            self.item(LIST_ORCA, i, rows.len(), flags, label);
        }
        self.orca_rows = rows;
        self.report_health();
        self.orca_rows.len()
    }

    /// Jump: the chosen conversation becomes the Target and comes to the
    /// front; reply with the Target.
    fn on_target_select(&mut self, list: u8, index: u8, now: Instant) {
        let i = usize::from(index);
        let jumped: Result<(), Status> = match list {
            LIST_ORCA if i < self.orca_rows.len() => {
                let s = self.orca_rows[i].clone();
                let r = self.orca_front(&s.handle, now);
                let st = self.orca_status(r);
                if st == Status::Ok {
                    self.set_target(orca_target(&s));
                    Ok(())
                } else {
                    Err(st)
                }
            }
            l if l > LIST_ORCA && usize::from(l) <= SUPPORTED_APPS.len() && i == 0 => {
                let app = SUPPORTED_APPS[usize::from(l) - 1];
                if self.injector.is_running(app.bundle_id) {
                    self.set_target(StoredTarget::App {
                        bundle_id: app.bundle_id.to_owned(),
                    });
                    self.injector
                        .activate(app.bundle_id)
                        .map_err(|e| inject_status(Err(e)))
                } else {
                    Err(Status::TargetUnavailable)
                }
            }
            _ => {
                log::warn!("TARGET_SELECT {list}/{index}: nothing to jump to");
                Ok(())
            }
        };
        // Do not follow focus here: the app just activated may not be
        // reported frontmost yet.
        let mut view = self.resolve(Need::Show, false, now).view;
        if let Err(status) = jumped {
            view.status = status;
        }
        self.send_state(view);
        self.report_health();
    }

    // ----- STATUS and refresh --------------------------------------------

    /// The most important Companion-level problem right now.
    pub fn health_code(&mut self) -> (StatusCode, &'static str) {
        match self.recognizer.health() {
            RecognizerHealth::NoPermission => {
                return (StatusCode::SpeechPermission, "需要语音识别权限");
            }
            RecognizerHealth::Unavailable => {
                return (StatusCode::RecognizerUnavailable, "中文语音识别不可用");
            }
            RecognizerHealth::Ready => {}
        }
        match self.view.as_ref().map(|v| v.kind) {
            Some(TargetKind::App) if !self.injector.accessibility_trusted() => {
                (StatusCode::AccessibilityPermission, "需要辅助功能权限")
            }
            Some(TargetKind::Orca) if self.orca_failed => {
                (StatusCode::OrcaUnavailable, "Orca 不可用")
            }
            _ => (StatusCode::Clear, ""),
        }
    }

    /// Periodic check while linked and not dictating: follow focus, recompute
    /// the Target (cheaply, see [`ORCA_CACHE_FRONT`]) and send TARGET_STATE
    /// only when it changed, then STATUS only when it changed.
    pub fn refresh(&mut self, now: Instant) {
        if self.dictation.is_some() || self.view.is_none() {
            return;
        }
        let r = self.resolve(Need::Refresh, true, now);
        if self.view.as_ref() != Some(&r.view) {
            self.send_state(r.view);
        }
        self.report_health();
    }

    /// Send STATUS if it changed since last sent.
    pub fn report_health(&mut self) {
        let (code, text) = self.health_code();
        if self.last_status != Some(code) {
            self.last_status = Some(code);
            self.send(CompanionFrame::Status {
                code,
                text: text.to_owned(),
            });
        }
    }
}

// ----- Alerts ----------------------------------------------------------------
//
// The Orca watch thread polls `orca terminal list` every 2 s while linked and
// hands the snapshot over here; nothing on this path runs the CLI itself.

impl<I: Injector, R: Recognizer, O: OrcaApi> Companion<I, R, O> {
    fn send_alert(&mut self, ev: AlertEvent) {
        if self.view.is_none() {
            return; // not past HELLO; pending Alerts are resent then
        }
        match ev {
            AlertEvent::Raise(a) => {
                log::info!(
                    "Alert {} raised: {} ({} bytes of message)",
                    a.id,
                    a.label,
                    a.message.len()
                );
                for f in alert_frames(a.id, ORCA as u8, &a.label, &a.message) {
                    self.send(f);
                }
            }
            AlertEvent::Clear { id, reason } => {
                let why = match reason {
                    ClearReason::Working => "working again",
                    ClearReason::Closed => "session closed",
                };
                log::info!("Alert {id} cleared: {why}");
                self.send(CompanionFrame::AlertClear { id });
            }
        }
    }

    /// A poll from the Orca watch thread: refresh the Orca cache and raise or
    /// clear Alerts.
    pub fn handle_orca_watch(&mut self, r: Result<OrcaSnapshot, OrcaError>, now: Instant) {
        let snap = match r {
            Ok(s) => s,
            Err(e) => {
                log::debug!("Orca watch: {e}");
                return;
            }
        };
        let events = self.alerts.update(&snap.sessions);
        self.orca_cache = Some((now, snap));
        for ev in events {
            self.send_alert(ev);
        }
    }

    /// ALERT_OPEN: Jump to the Alert's session and reply TARGET_STATE.
    fn on_alert_open(&mut self, id: u8, now: Instant) {
        let Some(alert) = self.alerts.take(id) else {
            log::info!("ALERT_OPEN {id}: no such Alert");
            self.send(CompanionFrame::AlertClear { id });
            let view = self.resolve(Need::Show, false, now).view;
            self.send_state(view);
            return;
        };
        let cached = self
            .orca_cache
            .as_ref()
            .and_then(|(_, snap)| snap.sessions.iter().find(|s| s.handle == alert.handle))
            .cloned();
        let session = match cached {
            Some(s) => Some(s),
            None => self
                .orca_snapshot(Need::Show, false, now)
                .ok()
                .and_then(|snap| snap.sessions.into_iter().find(|s| s.handle == alert.handle)),
        };
        let jumped = match session {
            Some(s) => {
                let r = self.orca_front(&s.handle, now);
                match self.orca_status(r) {
                    Status::Ok => {
                        self.set_target(orca_target(&s));
                        Ok(())
                    }
                    st => Err(st),
                }
            }
            None => Err(Status::TargetUnavailable),
        };
        let mut view = self.resolve(Need::Show, false, now).view;
        if let Err(status) = jumped {
            view.status = status;
        }
        self.send_state(view);
        self.report_health();
    }
}

// ----- Voice Notes Recording ---------------------------------------------
//
// Voice Notes calls run on a worker thread (`voice_notes::NotesWorker`); this
// only queues ops and handles replies, so Dictation, Target and Undo are never
// touched and never wait for Voice Notes.

impl<I: Injector, R: Recognizer, O: OrcaApi> Companion<I, R, O> {
    /// Ops for the Voice Notes worker, in order.
    pub fn take_notes_ops(&mut self) -> Vec<NotesOp> {
        std::mem::take(&mut self.notes.ops)
    }

    pub fn notes_state(&self) -> NotesState {
        self.notes.state
    }

    /// Ask for the Voice Notes status (every 2 s while linked) unless a
    /// request is already outstanding.
    pub fn notes_poll(&mut self) {
        if !self.notes.poll_pending && !self.notes.action_pending {
            self.notes.poll_pending = true;
            self.notes.ops.push(NotesOp::Status);
        }
    }

    fn send_notes(&mut self, notice: NotesNotice, now: Instant) {
        if self.view.is_none() {
            return; // not past HELLO
        }
        let elapsed_s = self.notes.elapsed_s(now);
        let state = self.notes.state;
        self.notes.sent = Some((state, elapsed_s, now));
        self.send(CompanionFrame::NotesState {
            state,
            elapsed_s,
            notice,
        });
    }

    /// Resend NOTES_STATE when the state changed or the Device's count drifted.
    fn notes_changed(&mut self, now: Instant) {
        let due = match self.notes.sent {
            None => true,
            Some((state, s, at)) => {
                let predicted = if state == NotesState::Recording {
                    s.saturating_add(now.saturating_duration_since(at).as_secs() as u32)
                } else {
                    s
                };
                state != self.notes.state
                    || predicted.abs_diff(self.notes.elapsed_s(now)) > NOTES_DRIFT_S
            }
        };
        if due {
            self.send_notes(NotesNotice::None, now);
        }
    }

    /// NOTES_TOGGLE: start when idle, stop when recording or paused.
    fn on_notes_toggle(&mut self, now: Instant) {
        if self.notes.action_pending {
            self.send_notes(NotesNotice::None, now);
            return;
        }
        let (next, op) = match self.notes.state {
            NotesState::Idle => (NotesState::Starting, NotesOp::Start),
            NotesState::Recording | NotesState::Paused => (NotesState::Stopping, NotesOp::Stop),
            NotesState::Starting | NotesState::Stopping => {
                self.send_notes(NotesNotice::None, now);
                return;
            }
        };
        log::info!("Voice Notes: {op:?}");
        let elapsed = self.notes.elapsed_s(now) as u64 * 1000;
        self.notes.set(next, elapsed, now);
        self.notes.action_pending = true;
        self.notes.ops.push(op);
        self.send_notes(NotesNotice::None, now);
    }

    fn apply_notes_status(&mut self, st: NotesStatus, now: Instant) {
        let state = match st.phase {
            NotesPhase::Idle => NotesState::Idle,
            NotesPhase::Recording => NotesState::Recording,
            NotesPhase::Paused => NotesState::Paused,
        };
        self.notes.set(state, st.elapsed_ms, now);
    }

    /// A reply from the Voice Notes worker.
    pub fn handle_notes(&mut self, reply: NotesReply, now: Instant) {
        match reply {
            NotesReply::Status(r) => {
                self.notes.poll_pending = false;
                if self.notes.action_pending {
                    return; // stale: a start or stop is in flight
                }
                match r {
                    Ok(st) => self.apply_notes_status(st, now),
                    Err(NotesError::NotRunning) => self.notes.set(NotesState::Idle, 0, now),
                    Err(e) => {
                        log::debug!("Voice Notes status: {e}");
                        return;
                    }
                }
                self.notes_changed(now);
            }
            NotesReply::Started(r) => {
                self.notes.action_pending = false;
                let notice = match r {
                    Ok(risks) => {
                        if !risks.is_empty() {
                            log::warn!("Voice Notes recording with risks {risks:?}");
                        }
                        self.notes.set(NotesState::Recording, 0, now);
                        start_notice(&risks)
                    }
                    Err(e) => {
                        log::warn!("Voice Notes start: {e}");
                        self.notes.set(NotesState::Idle, 0, now);
                        start_error_notice(&e)
                    }
                };
                self.send_notes(notice, now);
                self.notes_poll();
            }
            NotesReply::Stopped(r) => {
                self.notes.action_pending = false;
                let notice = match r {
                    Ok(()) => {
                        self.notes.set(NotesState::Idle, 0, now);
                        NotesNotice::Stopped
                    }
                    Err(e) => {
                        log::warn!("Voice Notes stop: {e}");
                        let ms = self.notes.elapsed_ms;
                        self.notes.set(NotesState::Recording, ms, now);
                        if e == NotesError::ControlDisabled {
                            NotesNotice::ControlDisabled
                        } else {
                            NotesNotice::StopFailed
                        }
                    }
                };
                self.send_notes(notice, now);
                self.notes_poll();
            }
        }
    }
}

fn inject_status(r: Result<(), InjectError>) -> Status {
    match r {
        Ok(()) => Status::Ok,
        Err(InjectError::NotRunning) => Status::TargetUnavailable,
        Err(InjectError::Permission) => Status::Permission,
        Err(InjectError::Failed(m)) => {
            log::error!("inject failed: {m}");
            Status::TargetUnavailable
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adpcm::{AdpcmState, encode};
    use crate::protocol::AudioFrame;
    use std::collections::HashSet;

    const WECHAT: &str = SUPPORTED_APPS[1].bundle_id;
    const CHATGPT: &str = SUPPORTED_APPS[2].bundle_id;
    const GHOSTTY: &str = "com.mitchellh.ghostty";

    #[derive(Default)]
    struct FakeInjector {
        trusted: bool,
        running: HashSet<String>,
        log: Vec<String>,
        fail_insert: Option<InjectError>,
        frontmost: Option<String>,
    }

    impl Injector for FakeInjector {
        fn accessibility_trusted(&mut self) -> bool {
            self.trusted
        }
        fn is_running(&mut self, b: &str) -> bool {
            self.running.contains(b)
        }
        fn frontmost_bundle_id(&mut self) -> Option<String> {
            self.frontmost.clone()
        }
        fn window_title(&mut self, b: &str) -> Option<String> {
            Some(if b == WECHAT { "张三" } else { "window" }.into())
        }
        fn activate(&mut self, b: &str) -> Result<(), InjectError> {
            if !self.running.contains(b) {
                return Err(InjectError::NotRunning);
            }
            self.log.push(format!("activate {b}"));
            self.frontmost = Some(b.into());
            Ok(())
        }
        fn insert(&mut self, b: &str, text: &str) -> Result<(), InjectError> {
            if let Some(e) = self.fail_insert.clone() {
                return Err(e);
            }
            self.log.push(format!("insert {b} {text}"));
            Ok(())
        }
        fn submit(&mut self, b: &str) -> Result<(), InjectError> {
            self.log.push(format!("submit {b}"));
            Ok(())
        }
        fn delete_back(&mut self, b: &str, n: usize) -> Result<(), InjectError> {
            self.log.push(format!("delete {b} {n}"));
            Ok(())
        }
    }

    #[derive(Default)]
    struct FakeRecognizer {
        health: Option<RecognizerHealth>,
        started: Vec<u8>,
        samples: usize,
        finished: usize,
        cancelled: usize,
    }

    impl Recognizer for FakeRecognizer {
        fn health(&mut self) -> RecognizerHealth {
            self.health.unwrap_or(RecognizerHealth::Ready)
        }
        fn start(&mut self, dict: u8) -> Result<(), RecognizerStartError> {
            match self.health() {
                RecognizerHealth::NoPermission => Err(RecognizerStartError::NoPermission),
                RecognizerHealth::Unavailable => Err(RecognizerStartError::Unavailable),
                RecognizerHealth::Ready => {
                    self.started.push(dict);
                    Ok(())
                }
            }
        }
        fn push(&mut self, pcm: &[i16]) {
            self.samples += pcm.len();
        }
        fn finish(&mut self) {
            self.finished += 1;
        }
        fn cancel(&mut self) {
            self.cancelled += 1;
        }
    }

    #[derive(Default)]
    struct FakeOrca {
        snap: OrcaSnapshot,
        unavailable: bool,
        open_fails: bool,
        snapshots: usize,
        log: Vec<String>,
    }

    impl OrcaApi for FakeOrca {
        fn snapshot(&mut self) -> Result<OrcaSnapshot, OrcaError> {
            self.snapshots += 1;
            if self.unavailable {
                return Err(OrcaError::Unavailable("down".into()));
            }
            Ok(self.snap.clone())
        }
        fn open(&mut self) -> Result<(), OrcaError> {
            self.log.push("open".into());
            if self.open_fails {
                Err(OrcaError::Unavailable("launch timed out".into()))
            } else {
                Ok(())
            }
        }
        fn send_text(&mut self, h: &str, t: &str) -> Result<(), OrcaError> {
            self.check(h)?;
            self.log.push(format!("text {h} {t}"));
            Ok(())
        }
        fn send_enter(&mut self, h: &str) -> Result<(), OrcaError> {
            self.check(h)?;
            self.log.push(format!("enter {h}"));
            Ok(())
        }
        fn send_backspaces(&mut self, h: &str, n: usize) -> Result<(), OrcaError> {
            self.check(h)?;
            self.log.push(format!("bs {h} {n}"));
            Ok(())
        }
        fn switch(&mut self, h: &str) -> Result<(), OrcaError> {
            self.check(h)?;
            self.log.push(format!("switch {h}"));
            // Like Orca: the session's tab becomes active.
            let i = self
                .snap
                .sessions
                .iter()
                .position(|s| s.handle == h)
                .unwrap();
            let s = self.snap.sessions.remove(i);
            self.snap.sessions.insert(0, s);
            self.snap.has_current = true;
            Ok(())
        }
    }

    impl FakeOrca {
        fn check(&self, h: &str) -> Result<(), OrcaError> {
            if self.unavailable {
                return Err(OrcaError::Unavailable("down".into()));
            }
            if self.snap.sessions.iter().any(|s| s.handle == h) {
                Ok(())
            } else {
                Err(OrcaError::Stale)
            }
        }
    }

    type C = Companion<FakeInjector, FakeRecognizer, FakeOrca>;

    fn session(h: &str, wt: &str, title: &str) -> OrcaSession {
        OrcaSession {
            handle: h.into(),
            leaf_id: format!("leaf_{h}"),
            worktree_id: format!("repo::/src/{wt}"),
            worktree: wt.into(),
            title: title.into(),
            raw_title: format!("✳ {title}"),
            agent: Some("claude".into()),
            preview: String::new(),
            reply: None,
        }
    }

    /// WeChat frontmost, no Target yet; Orca running with my-passport active.
    fn companion() -> C {
        let mut inj = FakeInjector {
            trusted: true,
            frontmost: Some(WECHAT.into()),
            ..Default::default()
        };
        inj.running.insert(ORCA_BUNDLE_ID.into());
        inj.running.insert(WECHAT.into());
        inj.running.insert(GHOSTTY.into());
        let orca = FakeOrca {
            snap: OrcaSnapshot {
                sessions: vec![
                    session("term_a", "my-passport", "语音输入"),
                    session("term_b", "my-passport", "server"),
                    session("term_c", "voice-notes", "PR"),
                ],
                has_current: true,
                active_worktree: Some("repo::/src/my-passport".into()),
            },
            ..Default::default()
        };
        Companion::new(inj, FakeRecognizer::default(), orca, None, None)
    }

    fn hello(c: &mut C, now: Instant) -> Vec<CompanionFrame> {
        c.handle_frame(
            DeviceFrame::Hello {
                ver: PROTOCOL_VERSION,
                fw: "0.2".into(),
            },
            now,
        );
        c.take_outbox()
    }

    fn state(status: Status, kind: TargetKind, label: &str) -> CompanionFrame {
        let name = label.split(" · ").next().unwrap();
        let app = SUPPORTED_APPS
            .iter()
            .position(|a| a.name == name)
            .map_or(APP_NONE, |i| i as u8);
        CompanionFrame::TargetState {
            status,
            kind,
            app,
            label: label.into(),
        }
    }

    fn wechat_state() -> CompanionFrame {
        state(Status::Ok, TargetKind::App, "微信 · 张三")
    }

    /// Ghostty (not a Supported App) frontmost, no Target yet.
    fn companion_elsewhere() -> C {
        let mut c = companion();
        c.injector.frontmost = Some(GHOSTTY.into());
        c
    }

    fn orca_state(label: &str) -> CompanionFrame {
        state(Status::Ok, TargetKind::Orca, &format!("Orca · {label}"))
    }

    fn orca_a_state() -> CompanionFrame {
        state(
            Status::Ok,
            TargetKind::Orca,
            "Orca · my-passport · 语音输入",
        )
    }

    fn audio(dict: u8, seq: u16) -> DeviceFrame {
        let mut st = AdpcmState::default();
        let pcm: Vec<i16> = (0..320)
            .map(|n| ((n as f32 * 0.3).sin() * 3000.0) as i16)
            .collect();
        DeviceFrame::Audio(AudioFrame {
            dict,
            seq,
            pred: 0,
            index: 0,
            adpcm: encode(&mut st, &pcm),
        })
    }

    /// Run a whole Dictation that recognizes `text`.
    fn dictate(c: &mut C, dict: u8, text: &str, t0: Instant) -> Vec<CompanionFrame> {
        c.handle_frame(DeviceFrame::DictStart { dict }, t0);
        c.handle_frame(audio(dict, 0), t0);
        c.handle_frame(DeviceFrame::DictStop { dict }, t0);
        c.handle_recog(
            RecogEvent::Final {
                dict,
                text: text.into(),
            },
            t0,
        );
        c.take_outbox()
    }

    fn partial(c: &mut C, dict: u8, text: &str, t: Instant) {
        c.handle_recog(
            RecogEvent::Partial {
                dict,
                text: text.into(),
            },
            t,
        );
    }

    fn action(action: Action, status: Status) -> CompanionFrame {
        CompanionFrame::ActionResult { action, status }
    }

    #[test]
    fn pause_restart_keeps_first_sentence() {
        let mut c = companion();
        let t0 = Instant::now();
        c.handle_frame(DeviceFrame::DictStart { dict: 1 }, t0);
        partial(&mut c, 1, "把这个函数", t0);
        partial(&mut c, 1, "把这个函数改成异步", t0);
        // Long pause: Speech starts over with only the new sentence.
        partial(&mut c, 1, "然后", t0);
        partial(&mut c, 1, "然后加测试", t0);
        c.handle_frame(DeviceFrame::DictStop { dict: 1 }, t0);
        c.handle_recog(
            RecogEvent::Final {
                dict: 1,
                text: "然后加测试。".into(),
            },
            t0,
        );
        assert_eq!(
            c.injector.log,
            [format!("insert {WECHAT} 把这个函数改成异步。然后加测试。")]
        );
    }

    #[test]
    fn early_final_restarts_and_accumulates() {
        let mut c = companion();
        let t0 = Instant::now();
        c.handle_frame(DeviceFrame::DictStart { dict: 1 }, t0);
        partial(&mut c, 1, "first part", t0);
        c.handle_recog(
            RecogEvent::Final {
                dict: 1,
                text: "First part.".into(),
            },
            t0,
        );
        assert_eq!(c.recognizer.started, [1, 1], "restarted after early final");
        partial(&mut c, 1, "second", t0);
        c.handle_frame(DeviceFrame::DictStop { dict: 1 }, t0);
        c.handle_recog(
            RecogEvent::Final {
                dict: 1,
                text: "Second part.".into(),
            },
            t0,
        );
        assert_eq!(
            c.injector.log,
            [format!("insert {WECHAT} First part. Second part. ")]
        );
    }

    #[test]
    fn segments_end_with_punctuation() {
        assert_eq!(end_sentence("把函数改成异步"), "把函数改成异步。");
        assert_eq!(end_sentence("已经有句号。"), "已经有句号。");
        assert_eq!(end_sentence("真的吗？"), "真的吗？");
        assert_eq!(end_sentence("run the tests"), "run the tests. ");
        assert_eq!(end_sentence("done!"), "done! ");
        assert_eq!(end_sentence("改成 async"), "改成 async。");
        assert_eq!(end_sentence("  "), "");
    }

    #[test]
    fn revisions_are_not_restarts() {
        assert!(!restarted_utterance("把这个函数改成", "把这个函数改成异步"));
        assert!(!restarted_utterance("把这个寒暑", "把这个函数"));
        assert!(restarted_utterance("把这个函数改成异步", "然后"));
        assert!(!restarted_utterance("你好", "再见"));
        assert_eq!(join_utterances("use", "async"), "use. async");
        assert_eq!(join_utterances("改成异步。", "然后"), "改成异步。然后");
    }

    #[test]
    fn empty_final_falls_back_to_partial() {
        let mut c = companion();
        let t0 = Instant::now();
        c.handle_frame(DeviceFrame::DictStart { dict: 1 }, t0);
        c.handle_frame(audio(1, 0), t0);
        c.handle_recog(
            RecogEvent::Partial {
                dict: 1,
                text: "把函数改成异步".into(),
            },
            t0,
        );
        c.handle_frame(DeviceFrame::DictStop { dict: 1 }, t0);
        c.handle_recog(
            RecogEvent::Final {
                dict: 1,
                text: String::new(),
            },
            t0,
        );
        assert_eq!(
            c.injector.log,
            [format!("insert {WECHAT} 把函数改成异步。")]
        );
    }

    #[test]
    fn dictation_inserts_and_replies_result() {
        let mut c = companion();
        let out = dictate(&mut c, 4, " 把这个函数改成异步 ", Instant::now());
        assert!(matches!(out[0], CompanionFrame::TargetState { .. }));
        assert_eq!(
            out.last().unwrap(),
            &CompanionFrame::Result {
                dict: 4,
                status: Status::Ok,
                text: "把这个函数改成异步。".into()
            }
        );
        assert_eq!(
            c.injector.log,
            [format!("insert {WECHAT} 把这个函数改成异步。")]
        );
        assert_eq!(c.recognizer.started, [4]);
        assert_eq!(c.recognizer.samples, 320);
        assert_eq!(c.recognizer.finished, 1);
        assert!(!c.dictation_active());
    }

    #[test]
    fn audio_gaps_reach_recognizer_as_silence() {
        let mut c = companion();
        let now = Instant::now();
        c.handle_frame(DeviceFrame::DictStart { dict: 1 }, now);
        c.handle_frame(audio(1, 0), now);
        c.handle_frame(audio(1, 2), now);
        c.handle_frame(audio(2, 3), now); // wrong dictation
        assert_eq!(c.recognizer.samples, 320 * 3);
    }

    #[test]
    fn partials_are_throttled() {
        let mut c = companion();
        let t0 = Instant::now();
        c.handle_frame(DeviceFrame::DictStart { dict: 1 }, t0);
        c.take_outbox();
        c.handle_recog(
            RecogEvent::Partial {
                dict: 1,
                text: "把".into(),
            },
            t0,
        );
        c.handle_recog(
            RecogEvent::Partial {
                dict: 1,
                text: "把这".into(),
            },
            t0 + Duration::from_millis(50),
        );
        c.handle_recog(
            RecogEvent::Partial {
                dict: 1,
                text: "把这个".into(),
            },
            t0 + Duration::from_millis(100),
        );
        assert_eq!(
            c.take_outbox(),
            [CompanionFrame::Partial {
                dict: 1,
                text: "把".into()
            }]
        );
        let due = c.poll(t0 + Duration::from_millis(120)).unwrap();
        assert_eq!(due, t0 + PARTIAL_INTERVAL);
        assert!(c.take_outbox().is_empty());
        assert_eq!(c.poll(due), None);
        assert_eq!(
            c.take_outbox(),
            [CompanionFrame::Partial {
                dict: 1,
                text: "把这个".into()
            }]
        );
        // Stale dictation ids are ignored.
        c.handle_recog(
            RecogEvent::Partial {
                dict: 9,
                text: "x".into(),
            },
            t0 + Duration::from_secs(1),
        );
        assert!(c.take_outbox().is_empty());
    }

    #[test]
    fn final_timeout_uses_best_partial() {
        let mut c = companion();
        let t0 = Instant::now();
        c.handle_frame(DeviceFrame::DictStart { dict: 2 }, t0);
        c.handle_recog(
            RecogEvent::Partial {
                dict: 2,
                text: "你好".into(),
            },
            t0,
        );
        c.handle_frame(DeviceFrame::DictStop { dict: 2 }, t0);
        c.take_outbox();
        assert_eq!(
            c.poll(t0 + Duration::from_secs(1)),
            Some(t0 + FINAL_TIMEOUT)
        );
        c.poll(t0 + FINAL_TIMEOUT);
        assert_eq!(
            c.take_outbox(),
            [
                wechat_state(),
                CompanionFrame::Result {
                    dict: 2,
                    status: Status::Ok,
                    text: "你好。".into()
                }
            ]
        );
        assert_eq!(c.recognizer.cancelled, 1);
        // A late final for a finished dictation is ignored.
        c.handle_recog(
            RecogEvent::Final {
                dict: 2,
                text: "你好。".into(),
            },
            t0 + FINAL_TIMEOUT,
        );
        assert!(c.take_outbox().is_empty());
    }

    #[test]
    fn empty_and_no_speech() {
        let mut c = companion();
        let out = dictate(&mut c, 1, "  ", Instant::now());
        assert_eq!(
            out.last().unwrap(),
            &CompanionFrame::Result {
                dict: 1,
                status: Status::Empty,
                text: String::new()
            }
        );
        let t0 = Instant::now();
        c.handle_frame(DeviceFrame::DictStart { dict: 2 }, t0);
        c.handle_frame(DeviceFrame::DictStop { dict: 2 }, t0);
        c.handle_recog(
            RecogEvent::Error {
                dict: 2,
                message: "kAFAssistantErrorDomain 1110 No speech detected".into(),
            },
            t0,
        );
        assert_eq!(
            c.take_outbox().last().unwrap(),
            &CompanionFrame::Result {
                dict: 2,
                status: Status::Empty,
                text: String::new()
            }
        );
        assert!(c.injector.log.is_empty());
    }

    #[test]
    fn recognizer_error_reported() {
        let mut c = companion();
        let t0 = Instant::now();
        c.handle_frame(DeviceFrame::DictStart { dict: 3 }, t0);
        c.handle_frame(DeviceFrame::DictStop { dict: 3 }, t0);
        c.handle_recog(
            RecogEvent::Error {
                dict: 3,
                message: "boom".into(),
            },
            t0,
        );
        assert_eq!(
            c.take_outbox().last().unwrap(),
            &CompanionFrame::Result {
                dict: 3,
                status: Status::RecognizerError,
                text: String::new()
            }
        );
        c.recognizer.health = Some(RecognizerHealth::NoPermission);
        c.handle_frame(DeviceFrame::DictStart { dict: 4 }, t0);
        c.handle_frame(audio(4, 0), t0);
        c.handle_frame(DeviceFrame::DictStop { dict: 4 }, t0);
        let out = c.take_outbox();
        assert!(out.contains(&CompanionFrame::Status {
            code: StatusCode::SpeechPermission,
            text: "需要语音识别权限".into()
        }));
        assert_eq!(
            out.last().unwrap(),
            &CompanionFrame::Result {
                dict: 4,
                status: Status::Permission,
                text: String::new()
            }
        );
        assert_eq!(c.recognizer.samples, 0);
    }

    #[test]
    fn cancel_discards() {
        let mut c = companion();
        let t0 = Instant::now();
        c.handle_frame(DeviceFrame::DictStart { dict: 5 }, t0);
        c.handle_recog(
            RecogEvent::Partial {
                dict: 5,
                text: "abc".into(),
            },
            t0,
        );
        c.take_outbox();
        c.handle_frame(DeviceFrame::DictCancel { dict: 5 }, t0);
        assert_eq!(
            c.take_outbox(),
            [CompanionFrame::Result {
                dict: 5,
                status: Status::Cancelled,
                text: String::new()
            }]
        );
        assert_eq!(c.recognizer.cancelled, 1);
        c.handle_recog(
            RecogEvent::Final {
                dict: 5,
                text: "abc".into(),
            },
            t0,
        );
        assert!(c.take_outbox().is_empty());
        assert!(c.injector.log.is_empty());
    }

    #[test]
    fn disconnect_abandons_without_insert() {
        let mut c = companion();
        let t0 = Instant::now();
        c.handle_frame(DeviceFrame::DictStart { dict: 6 }, t0);
        c.handle_frame(DeviceFrame::DictStop { dict: 6 }, t0);
        c.on_disconnected();
        c.handle_recog(
            RecogEvent::Final {
                dict: 6,
                text: "x".into(),
            },
            t0,
        );
        assert!(c.take_outbox().is_empty());
        assert!(c.injector.log.is_empty());
        assert_eq!(c.recognizer.cancelled, 1);
    }

    #[test]
    fn hello_gets_ack_then_target_state() {
        let mut c = companion();
        let out = hello(&mut c, Instant::now());
        assert_eq!(
            out,
            [
                CompanionFrame::HelloAck { ver: 2 },
                wechat_state(),
                CompanionFrame::Status {
                    code: StatusCode::Clear,
                    text: String::new()
                },
                CompanionFrame::NotesState {
                    state: NotesState::Idle,
                    elapsed_s: 0,
                    notice: NotesNotice::None
                }
            ]
        );
    }

    #[test]
    fn frontmost_supported_app_becomes_the_target() {
        let mut c = companion();
        let t0 = Instant::now();
        let out = dictate(&mut c, 1, "晚上吃什么", t0);
        assert_eq!(out[0], wechat_state(), "TARGET_STATE at DICT_START");
        assert_eq!(out[out.len() - 2], wechat_state(), "and after the Insert");
        c.handle_frame(DeviceFrame::Undo, t0);
        c.handle_frame(DeviceFrame::Submit, t0);
        assert_eq!(
            c.injector.log,
            [
                format!("insert {WECHAT} 晚上吃什么。"),
                format!("delete {WECHAT} 6"),
                format!("submit {WECHAT}")
            ]
        );
        assert_eq!(
            c.take_outbox(),
            [
                action(Action::Undo, Status::Ok),
                wechat_state(),
                action(Action::Submit, Status::Ok)
            ]
        );
        assert!(c.orca.log.is_empty());
        assert_eq!(
            c.target(),
            Some(&StoredTarget::App {
                bundle_id: WECHAT.into()
            })
        );
    }

    #[test]
    fn focus_elsewhere_keeps_the_target() {
        let mut c = companion();
        let t0 = Instant::now();
        hello(&mut c, t0);
        c.injector.frontmost = Some(GHOSTTY.into());
        let out = dictate(&mut c, 1, "你好", t0);
        assert_eq!(out[0], wechat_state());
        c.handle_frame(DeviceFrame::Submit, t0);
        assert_eq!(
            c.injector.log,
            [format!("insert {WECHAT} 你好。"), format!("submit {WECHAT}")]
        );
    }

    #[test]
    fn focus_change_right_before_insert_counts() {
        let mut c = companion();
        let t0 = Instant::now();
        c.injector.running.insert(CHATGPT.into());
        c.handle_frame(DeviceFrame::DictStart { dict: 1 }, t0);
        c.injector.frontmost = Some(CHATGPT.into());
        c.handle_frame(DeviceFrame::DictStop { dict: 1 }, t0);
        c.handle_recog(
            RecogEvent::Final {
                dict: 1,
                text: "hi".into(),
            },
            t0,
        );
        let out = c.take_outbox();
        assert_eq!(out[0], wechat_state());
        assert!(out.contains(&state(Status::Ok, TargetKind::App, "ChatGPT · window")));
        assert_eq!(c.injector.log, [format!("insert {CHATGPT} hi. ")]);
    }

    #[test]
    fn no_target_is_orca_current_conversation() {
        let mut c = companion_elsewhere();
        let t0 = Instant::now();
        assert_eq!(hello(&mut c, t0)[1], orca_a_state());
        let out = dictate(&mut c, 1, "运行测试", t0);
        assert_eq!(out[0], orca_a_state());
        assert_eq!(
            out.last().unwrap(),
            &CompanionFrame::Result {
                dict: 1,
                status: Status::Ok,
                text: "运行测试。".into()
            }
        );
        c.handle_frame(DeviceFrame::Undo, t0);
        c.handle_frame(DeviceFrame::Submit, t0);
        // Brought to the front, then delivered through the CLI (no paste).
        assert_eq!(
            c.orca.log,
            [
                "switch term_a",
                "text term_a 运行测试。",
                "switch term_a",
                "bs term_a 5",
                "switch term_a",
                "enter term_a"
            ]
        );
        assert_eq!(
            c.injector.log,
            vec![format!("activate {ORCA_BUNDLE_ID}"); 3]
        );
    }

    #[test]
    fn orca_not_running_is_launched_for_insert_and_submit() {
        let mut c = companion_elsewhere();
        c.injector.running.remove(ORCA_BUNDLE_ID);
        let t0 = Instant::now();
        assert_eq!(hello(&mut c, t0)[1], orca_state("未运行，将自动启动"));
        assert!(c.orca.log.is_empty(), "showing the Target never launches");
        assert_eq!(c.target(), None);
        let out = dictate(&mut c, 1, "继续", t0);
        assert_eq!(
            out.last().unwrap(),
            &CompanionFrame::Result {
                dict: 1,
                status: Status::Ok,
                text: "继续。".into()
            }
        );
        assert_eq!(c.orca.log, ["open", "switch term_a", "text term_a 继续。"]);
        // Orca quit again: the stored session labels the Target until the
        // next Submit launches Orca.
        c.take_outbox();
        c.refresh(t0 + Duration::from_secs(2));
        assert!(c.take_outbox().is_empty());
        c.handle_frame(DeviceFrame::Submit, t0);
        assert_eq!(c.orca.log[3..], ["open", "switch term_a", "enter term_a"]);
    }

    #[test]
    fn orca_launch_failure_is_target_unavailable() {
        let mut c = companion_elsewhere();
        c.injector.running.remove(ORCA_BUNDLE_ID);
        c.orca.open_fails = true;
        let t0 = Instant::now();
        hello(&mut c, t0);
        let out = dictate(&mut c, 1, "继续", t0);
        assert!(out.contains(&state(Status::TargetUnavailable, TargetKind::Orca, "Orca")));
        assert!(out.contains(&CompanionFrame::Status {
            code: StatusCode::OrcaUnavailable,
            text: "Orca 不可用".into()
        }));
        assert_eq!(
            out.last().unwrap(),
            &CompanionFrame::Result {
                dict: 1,
                status: Status::TargetUnavailable,
                text: String::new()
            }
        );
        // Orca came up by other means: STATUS clears on the next Submit.
        c.orca.open_fails = false;
        c.injector.running.insert(ORCA_BUNDLE_ID.into());
        c.handle_frame(DeviceFrame::Submit, t0);
        let out = c.take_outbox();
        assert!(out.contains(&CompanionFrame::Status {
            code: StatusCode::Clear,
            text: String::new()
        }));
        assert_eq!(out.last().unwrap(), &action(Action::Submit, Status::Ok));
    }

    #[test]
    fn orca_without_current_conversation() {
        let mut c = companion_elsewhere();
        c.orca.snap.has_current = false;
        let out = hello(&mut c, Instant::now());
        assert_eq!(
            out[1],
            state(
                Status::TargetUnavailable,
                TargetKind::Orca,
                "Orca · 没有会话"
            )
        );
        c.handle_frame(DeviceFrame::Submit, Instant::now());
        assert_eq!(
            c.take_outbox().last().unwrap(),
            &action(Action::Submit, Status::TargetUnavailable)
        );
    }

    #[test]
    fn gone_orca_session_is_replaced_silently() {
        let mut c = companion_elsewhere();
        // Restored Target whose handle Orca re-issued: found by its pane.
        c.target = Some(StoredTarget::Orca {
            handle: "term_old".into(),
            leaf_id: "leaf_term_b".into(),
            worktree: "my-passport".into(),
            title: "server".into(),
        });
        let t0 = Instant::now();
        assert_eq!(hello(&mut c, t0)[1], orca_state("my-passport · server"));
        assert!(
            matches!(c.target(), Some(StoredTarget::Orca { handle, .. }) if handle == "term_b")
        );
        // The pane closed: Orca's Current Conversation takes over.
        c.orca.snap.sessions.remove(1);
        let out = dictate(&mut c, 1, "继续", t0 + Duration::from_secs(1));
        assert_eq!(out[0], orca_a_state());
        assert_eq!(c.orca.log, ["switch term_a", "text term_a 继续。"]);
        assert!(
            matches!(c.target(), Some(StoredTarget::Orca { handle, .. }) if handle == "term_a")
        );
    }

    #[test]
    fn focus_on_orca_targets_its_current_conversation() {
        let mut c = companion();
        let t0 = Instant::now();
        hello(&mut c, t0);
        c.injector.frontmost = Some(ORCA_BUNDLE_ID.into());
        c.refresh(t0 + Duration::from_secs(2));
        assert_eq!(c.take_outbox(), [orca_a_state()]);
        // Orca shows another tab.
        c.orca.snap.sessions.swap(0, 1);
        c.refresh(t0 + Duration::from_secs(4));
        assert_eq!(c.take_outbox(), [orca_state("my-passport · server")]);
    }

    #[test]
    fn target_app_not_running_is_never_launched() {
        let mut c = companion();
        let t0 = Instant::now();
        hello(&mut c, t0);
        c.injector.running.remove(WECHAT);
        c.injector.frontmost = Some(GHOSTTY.into());
        c.handle_frame(DeviceFrame::Submit, t0);
        assert_eq!(
            c.take_outbox(),
            [
                state(Status::TargetUnavailable, TargetKind::App, "微信"),
                action(Action::Submit, Status::TargetUnavailable)
            ]
        );
        assert!(c.injector.log.is_empty() && c.orca.log.is_empty());
    }

    #[test]
    fn undo_uses_the_segment_destination() {
        let mut c = companion();
        let t0 = Instant::now();
        dictate(&mut c, 1, "你好", t0);
        // Focus moves to Orca: the Target follows, Undo does not.
        c.injector.frontmost = Some(ORCA_BUNDLE_ID.into());
        c.refresh(t0 + Duration::from_secs(2));
        c.handle_frame(DeviceFrame::Undo, t0);
        assert_eq!(
            c.injector.log.last().unwrap(),
            &format!("delete {WECHAT} 3")
        );
        // The app quit: nothing is launched.
        c.injector.frontmost = Some(WECHAT.into());
        dictate(&mut c, 3, "你好", t0);
        c.injector.running.remove(WECHAT);
        c.take_outbox();
        c.handle_frame(DeviceFrame::Undo, t0);
        assert_eq!(
            c.take_outbox(),
            [action(Action::Undo, Status::TargetUnavailable)]
        );
    }

    #[test]
    fn root_list_marks_the_target_app() {
        let mut c = companion();
        let now = Instant::now();
        hello(&mut c, now);
        c.handle_frame(DeviceFrame::TargetsReq { list: 0 }, now);
        let out = c.take_outbox();
        let rows: Vec<(u8, String)> = out[..4]
            .iter()
            .map(|f| match f {
                CompanionFrame::TargetItem {
                    list: 0,
                    count: 4,
                    flags,
                    label,
                    ..
                } => (*flags, label.clone()),
                other => panic!("{other:?}"),
            })
            .collect();
        let s = FLAG_SUBLIST;
        assert_eq!(
            rows,
            [
                (s, "Orca".to_owned()),
                (s | FLAG_CURRENT, "微信".to_owned()),
                (s | FLAG_NOT_RUNNING, "ChatGPT".to_owned()),
                (s | FLAG_NOT_RUNNING, "企业微信".to_owned()),
            ]
        );
        assert_eq!(out[4], CompanionFrame::TargetEnd { list: 0, count: 4 });
    }

    #[test]
    fn orca_list_current_first_then_jump() {
        let mut c = companion();
        let now = Instant::now();
        hello(&mut c, now);
        c.handle_frame(DeviceFrame::TargetsReq { list: 1 }, now);
        let item = |index, flags, label: &str| CompanionFrame::TargetItem {
            list: 1,
            index,
            count: 3,
            flags,
            label: label.into(),
        };
        assert_eq!(
            c.take_outbox(),
            [
                item(0, FLAG_CURRENT, "my-passport · 语音输入"),
                item(1, 0, "my-passport · server"),
                item(2, 0, "voice-notes · PR"),
                CompanionFrame::TargetEnd { list: 1, count: 3 },
            ]
        );
        c.handle_frame(DeviceFrame::TargetSelect { list: 1, index: 2 }, now);
        assert_eq!(c.take_outbox(), [orca_state("voice-notes · PR")]);
        assert_eq!(c.orca.log, ["switch term_c"]);
        assert_eq!(c.injector.log, [format!("activate {ORCA_BUNDLE_ID}")]);
        // Orca is now frontmost; before it reports the new tab, following
        // focus keeps the jumped-to session.
        c.refresh(now + Duration::from_millis(1500));
        assert!(c.take_outbox().is_empty());
        let out = dictate(&mut c, 1, "继续", now + Duration::from_millis(1600));
        assert_eq!(out[0], orca_state("voice-notes · PR"));
        assert_eq!(c.orca.log.last().unwrap(), "text term_c 继续。");
        // A stale row: the reply reports the failure.
        c.orca.snap.sessions.retain(|s| s.handle != "term_c");
        c.handle_frame(DeviceFrame::TargetSelect { list: 1, index: 2 }, now);
        assert!(matches!(
            &c.take_outbox()[0],
            CompanionFrame::TargetState {
                status: Status::TargetUnavailable,
                ..
            }
        ));
    }

    #[test]
    fn orca_list_launches_orca() {
        let mut c = companion_elsewhere();
        c.injector.running.remove(ORCA_BUNDLE_ID);
        let now = Instant::now();
        hello(&mut c, now);
        c.handle_frame(DeviceFrame::TargetsReq { list: 1 }, now);
        assert_eq!(c.orca.log, ["open"]);
        assert_eq!(
            c.take_outbox().last().unwrap(),
            &CompanionFrame::TargetEnd { list: 1, count: 3 }
        );
        // Orca down: STATUS 3 (the Target is Orca) and an empty list.
        c.injector.running.insert(ORCA_BUNDLE_ID.into());
        c.orca.unavailable = true;
        c.handle_frame(
            DeviceFrame::TargetsReq { list: 1 },
            now + Duration::from_secs(1),
        );
        let out = c.take_outbox();
        assert!(out.contains(&CompanionFrame::Status {
            code: StatusCode::OrcaUnavailable,
            text: "Orca 不可用".into()
        }));
        assert_eq!(
            out.last().unwrap(),
            &CompanionFrame::TargetEnd { list: 1, count: 0 }
        );
    }

    #[test]
    fn app_lists_have_one_current_row_and_jump() {
        let mut c = companion_elsewhere();
        let now = Instant::now();
        hello(&mut c, now);
        c.handle_frame(DeviceFrame::TargetsReq { list: 2 }, now);
        assert_eq!(
            c.take_outbox(),
            [
                CompanionFrame::TargetItem {
                    list: 2,
                    index: 0,
                    count: 1,
                    flags: FLAG_CURRENT,
                    label: "当前会话 · 张三".into()
                },
                CompanionFrame::TargetEnd { list: 2, count: 1 }
            ]
        );
        c.handle_frame(DeviceFrame::TargetSelect { list: 2, index: 0 }, now);
        assert_eq!(c.take_outbox(), [wechat_state()]);
        assert_eq!(c.injector.log, [format!("activate {WECHAT}")]);
        // ChatGPT is not running: empty list, never launched; a stray
        // select reports it and keeps the Target.
        c.handle_frame(DeviceFrame::TargetsReq { list: 3 }, now);
        assert_eq!(
            c.take_outbox(),
            [CompanionFrame::TargetEnd { list: 3, count: 0 }]
        );
        c.handle_frame(DeviceFrame::TargetSelect { list: 3, index: 0 }, now);
        assert_eq!(
            c.take_outbox(),
            [state(
                Status::TargetUnavailable,
                TargetKind::App,
                "微信 · 张三"
            )]
        );
        assert_eq!(c.injector.log.len(), 1);
        // Unknown lists are empty; root selects just report the Target.
        c.handle_frame(DeviceFrame::TargetsReq { list: 9 }, now);
        assert_eq!(
            c.take_outbox(),
            [CompanionFrame::TargetEnd { list: 9, count: 0 }]
        );
        c.handle_frame(DeviceFrame::TargetSelect { list: 0, index: 1 }, now);
        assert_eq!(c.take_outbox(), [wechat_state()]);
    }

    #[test]
    fn refresh_sends_target_state_only_on_change() {
        let mut c = companion();
        let t0 = Instant::now();
        c.refresh(t0);
        assert!(c.take_outbox().is_empty(), "nothing before HELLO");
        hello(&mut c, t0);
        c.refresh(t0 + Duration::from_secs(2));
        assert!(c.take_outbox().is_empty(), "unchanged stays quiet");
        c.injector.frontmost = Some(GHOSTTY.into());
        c.refresh(t0 + Duration::from_secs(4));
        assert!(
            c.take_outbox().is_empty(),
            "focus elsewhere keeps the Target"
        );
        // Accessibility revoked, then granted.
        c.injector.trusted = false;
        c.refresh(t0 + Duration::from_secs(8));
        assert_eq!(
            c.take_outbox(),
            [
                state(Status::Permission, TargetKind::App, "微信 · 张三"),
                CompanionFrame::Status {
                    code: StatusCode::AccessibilityPermission,
                    text: "需要辅助功能权限".into()
                }
            ]
        );
        c.injector.trusted = true;
        c.refresh(t0 + Duration::from_secs(10));
        assert_eq!(
            c.take_outbox(),
            [
                wechat_state(),
                CompanionFrame::Status {
                    code: StatusCode::Clear,
                    text: String::new()
                }
            ]
        );
        // Not while dictating.
        c.handle_frame(DeviceFrame::DictStart { dict: 1 }, t0);
        c.take_outbox();
        c.injector.frontmost = Some(ORCA_BUNDLE_ID.into());
        c.refresh(t0 + Duration::from_secs(12));
        assert!(c.take_outbox().is_empty());
    }

    #[test]
    fn refresh_reuses_orca_snapshot() {
        let mut c = companion_elsewhere();
        let t0 = Instant::now();
        hello(&mut c, t0);
        assert_eq!(c.orca.snapshots, 1);
        // Orca Target in the background: reuse for 10 s.
        c.refresh(t0 + Duration::from_secs(2));
        c.refresh(t0 + Duration::from_secs(8));
        assert_eq!(c.orca.snapshots, 1);
        c.refresh(t0 + Duration::from_secs(10));
        assert_eq!(c.orca.snapshots, 2);
        // Orca frontmost: ask every 2 s.
        c.injector.frontmost = Some(ORCA_BUNDLE_ID.into());
        c.refresh(t0 + Duration::from_secs(11));
        assert_eq!(c.orca.snapshots, 2);
        c.refresh(t0 + Duration::from_secs(12));
        assert_eq!(c.orca.snapshots, 3);
        // An app Target never asks Orca.
        c.injector.frontmost = Some(WECHAT.into());
        c.refresh(t0 + Duration::from_secs(30));
        c.injector.frontmost = Some(GHOSTTY.into());
        c.refresh(t0 + Duration::from_secs(40));
        c.handle_frame(DeviceFrame::Submit, t0 + Duration::from_secs(41));
        assert_eq!(c.orca.snapshots, 3);
    }

    #[test]
    fn target_survives_restart() {
        let dir = std::env::temp_dir().join(format!("vv-sess-{}", std::process::id()));
        let path = dir.join("target.json");
        let _ = std::fs::remove_dir_all(&dir);
        let mut c = companion();
        c.store = Some(path.clone());
        hello(&mut c, Instant::now());
        let stored = StoredTarget::load(&path);
        assert_eq!(
            stored,
            Some(StoredTarget::App {
                bundle_id: WECHAT.into()
            })
        );
        let mut c2 = companion_elsewhere();
        c2.target = stored;
        assert_eq!(hello(&mut c2, Instant::now())[1], wechat_state());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn hello_reports_missing_permissions() {
        let mut c = companion();
        c.injector.trusted = false;
        let out = hello(&mut c, Instant::now());
        assert_eq!(
            out[1],
            state(Status::Permission, TargetKind::App, "微信 · 张三")
        );
        assert!(matches!(
            out[2],
            CompanionFrame::Status {
                code: StatusCode::AccessibilityPermission,
                ..
            }
        ));
        // An Orca Target does not need Accessibility.
        let mut c = companion_elsewhere();
        c.injector.trusted = false;
        let out = hello(&mut c, Instant::now());
        assert!(matches!(
            out[2],
            CompanionFrame::Status {
                code: StatusCode::Clear,
                ..
            }
        ));
        c.recognizer.health = Some(RecognizerHealth::NoPermission);
        let out = hello(&mut c, Instant::now());
        assert!(matches!(
            out[2],
            CompanionFrame::Status {
                code: StatusCode::SpeechPermission,
                ..
            }
        ));
    }
    #[test]
    fn undo_once_counts_graphemes() {
        let mut c = companion();
        let t0 = Instant::now();
        dictate(&mut c, 1, "改成异步👍🏽ok", t0);
        c.handle_frame(DeviceFrame::Undo, t0);
        c.handle_frame(DeviceFrame::Undo, t0);
        assert_eq!(
            c.take_outbox(),
            [
                action(Action::Undo, Status::Ok),
                action(Action::Undo, Status::NothingToUndo),
            ]
        );
        assert_eq!(
            c.injector.log.last().unwrap(),
            &format!("delete {WECHAT} 8")
        );
        assert_eq!(undo_count("é"), 1);
    }

    #[test]
    fn new_dictation_clears_undo() {
        let mut c = companion();
        let t0 = Instant::now();
        dictate(&mut c, 1, "你好", t0);
        c.handle_frame(DeviceFrame::DictStart { dict: 2 }, t0);
        c.handle_frame(DeviceFrame::DictCancel { dict: 2 }, t0);
        c.take_outbox();
        c.handle_frame(DeviceFrame::Undo, t0);
        assert_eq!(
            c.take_outbox(),
            [action(Action::Undo, Status::NothingToUndo)]
        );
    }

    #[test]
    fn submit_clears_undo() {
        let mut c = companion();
        let t0 = Instant::now();
        dictate(&mut c, 1, "你好", t0);
        c.handle_frame(DeviceFrame::Submit, t0);
        c.take_outbox();
        c.handle_frame(DeviceFrame::Undo, t0);
        assert_eq!(
            c.take_outbox(),
            [action(Action::Undo, Status::NothingToUndo)]
        );
    }

    #[test]
    fn insert_permission_failure() {
        let mut c = companion();
        c.injector.fail_insert = Some(InjectError::Permission);
        let out = dictate(&mut c, 1, "你好", Instant::now());
        assert_eq!(
            out.last().unwrap(),
            &CompanionFrame::Result {
                dict: 1,
                status: Status::Permission,
                text: String::new()
            }
        );
        c.handle_frame(DeviceFrame::Undo, Instant::now());
        assert_eq!(
            c.take_outbox(),
            [action(Action::Undo, Status::NothingToUndo)]
        );
    }

    #[test]
    fn labels_fit_and_keep_name() {
        for max in [ITEM_LABEL_BYTES, STATE_LABEL_BYTES] {
            let l = compose_label("企业微信", Some(&"很长的会话标题".repeat(20)), max);
            assert!(l.starts_with("企业微信 · "));
            assert!(l.len() <= max, "{} > {max}", l.len());
            assert!(l.ends_with('…'));
        }
        assert_eq!(compose_label("Orca", Some("  "), 71), "Orca");
        assert_eq!(
            compose_label("my-passport", Some("语音输入"), 71),
            "my-passport · 语音输入"
        );
        let long_name = compose_label(&"名".repeat(40), Some("t"), 71);
        assert!(long_name.starts_with(&"名".repeat(15)) && long_name.len() <= 71);
    }

    // ----- Voice Notes Recording -----------------------------------------

    fn notes(state: NotesState, elapsed_s: u32, notice: NotesNotice) -> CompanionFrame {
        CompanionFrame::NotesState {
            state,
            elapsed_s,
            notice,
        }
    }

    fn notes_frames(out: &[CompanionFrame]) -> Vec<CompanionFrame> {
        out.iter()
            .filter(|f| matches!(f, CompanionFrame::NotesState { .. }))
            .cloned()
            .collect()
    }

    fn rec(elapsed_ms: u64) -> NotesReply {
        NotesReply::Status(Ok(NotesStatus {
            phase: NotesPhase::Recording,
            elapsed_ms,
        }))
    }

    #[test]
    fn hello_reports_notes_and_polls() {
        let mut c = companion();
        let t0 = Instant::now();
        let out = hello(&mut c, t0);
        assert_eq!(
            out.last().unwrap(),
            &notes(NotesState::Idle, 0, NotesNotice::None)
        );
        assert_eq!(c.take_notes_ops(), [NotesOp::Status]);
        // A poll is not repeated while one is outstanding.
        c.notes_poll();
        assert!(c.take_notes_ops().is_empty());
        // Started from the Mac: the Device learns it from the poll.
        c.handle_notes(rec(65_000), t0);
        assert_eq!(
            c.take_outbox(),
            [notes(NotesState::Recording, 65, NotesNotice::None)]
        );
        // The Device counts on its own; small drift stays quiet.
        c.notes_poll();
        c.handle_notes(rec(67_000), t0 + Duration::from_secs(2));
        assert!(c.take_outbox().is_empty());
        // Paused on the Mac.
        c.notes_poll();
        c.handle_notes(
            NotesReply::Status(Ok(NotesStatus {
                phase: NotesPhase::Paused,
                elapsed_ms: 68_000,
            })),
            t0 + Duration::from_secs(4),
        );
        assert_eq!(
            c.take_outbox(),
            [notes(NotesState::Paused, 68, NotesNotice::None)]
        );
        // Voice Notes quit: idle.
        c.notes_poll();
        c.handle_notes(NotesReply::Status(Err(NotesError::NotRunning)), t0);
        assert_eq!(
            c.take_outbox(),
            [notes(NotesState::Idle, 0, NotesNotice::None)]
        );
    }

    #[test]
    fn notes_toggle_start_and_stop() {
        let mut c = companion();
        let t0 = Instant::now();
        hello(&mut c, t0);
        c.take_notes_ops();
        c.handle_notes(NotesReply::Status(Err(NotesError::NotRunning)), t0);
        c.take_outbox();
        c.handle_frame(DeviceFrame::NotesToggle, t0);
        assert_eq!(c.take_notes_ops(), [NotesOp::Start]);
        assert_eq!(
            c.take_outbox(),
            [notes(NotesState::Starting, 0, NotesNotice::None)]
        );
        // A second press while starting does nothing new; polls wait.
        c.handle_frame(DeviceFrame::NotesToggle, t0);
        c.notes_poll();
        assert!(c.take_notes_ops().is_empty());
        c.take_outbox();
        c.handle_notes(NotesReply::Started(Ok(vec![RISK_BLUETOOTH_MIC.into()])), t0);
        assert_eq!(
            c.take_outbox(),
            [notes(NotesState::Recording, 0, NotesNotice::RiskBluetooth)]
        );
        assert_eq!(c.take_notes_ops(), [NotesOp::Status]);
        c.handle_notes(rec(3_000), t0 + Duration::from_secs(3));
        c.handle_frame(DeviceFrame::NotesToggle, t0 + Duration::from_secs(3));
        assert_eq!(c.take_notes_ops(), [NotesOp::Stop]);
        assert_eq!(
            c.take_outbox(),
            [notes(NotesState::Stopping, 3, NotesNotice::None)]
        );
        c.handle_notes(NotesReply::Stopped(Ok(())), t0 + Duration::from_secs(4));
        assert_eq!(
            c.take_outbox(),
            [notes(NotesState::Idle, 0, NotesNotice::Stopped)]
        );
    }

    #[test]
    fn notes_notices() {
        assert_eq!(start_notice(&[]), NotesNotice::Started);
        assert_eq!(
            start_notice(&[RISK_VOICE_ISOLATION.into(), RISK_BLUETOOTH_MIC.into()]),
            NotesNotice::RiskVoiceIsolation
        );
        assert_eq!(start_notice(&["new_kind".into()]), NotesNotice::RiskOther);
        for (e, n) in [
            (NotesError::NotInstalled, NotesNotice::NotInstalled),
            (NotesError::LaunchFailed, NotesNotice::LaunchFailed),
            (NotesError::ControlDisabled, NotesNotice::ControlDisabled),
            (NotesError::Failed("x".into()), NotesNotice::StartFailed),
        ] {
            let mut c = companion();
            let t0 = Instant::now();
            hello(&mut c, t0);
            c.handle_frame(DeviceFrame::NotesToggle, t0);
            c.take_outbox();
            c.handle_notes(NotesReply::Started(Err(e)), t0);
            assert_eq!(c.take_outbox(), [notes(NotesState::Idle, 0, n)]);
        }
        // A failed stop keeps the recording.
        let mut c = companion();
        let t0 = Instant::now();
        hello(&mut c, t0);
        c.take_notes_ops();
        c.handle_notes(rec(10_000), t0);
        c.handle_frame(DeviceFrame::NotesToggle, t0);
        c.take_outbox();
        c.handle_notes(NotesReply::Stopped(Err(NotesError::Failed("x".into()))), t0);
        assert_eq!(
            c.take_outbox(),
            [notes(NotesState::Recording, 10, NotesNotice::StopFailed)]
        );
    }

    #[test]
    fn notes_and_dictation_run_side_by_side() {
        let mut c = companion();
        let t0 = Instant::now();
        hello(&mut c, t0);
        c.take_notes_ops();
        dictate(&mut c, 1, "第一句", t0);
        // Toggle during a Dictation: it neither stops nor cancels it.
        c.handle_frame(DeviceFrame::DictStart { dict: 2 }, t0);
        partial(&mut c, 2, "继续", t0);
        c.take_outbox();
        c.handle_frame(DeviceFrame::NotesToggle, t0);
        assert!(c.dictation_active());
        assert_eq!(c.take_notes_ops(), [NotesOp::Start]);
        // Audio keeps flowing while Voice Notes starts (on its own thread).
        c.handle_frame(audio(2, 0), t0);
        c.handle_frame(audio(2, 1), t0);
        assert_eq!(c.recognizer.samples, 320 * 3);
        c.handle_notes(NotesReply::Started(Ok(vec![])), t0);
        assert!(c.dictation_active());
        let out = c.take_outbox();
        assert_eq!(
            notes_frames(&out),
            [
                notes(NotesState::Starting, 0, NotesNotice::None),
                notes(NotesState::Recording, 0, NotesNotice::Started)
            ]
        );
        assert!(!out.iter().any(|f| matches!(
            f,
            CompanionFrame::TargetState { .. } | CompanionFrame::Result { .. }
        )));
        // The Dictation finishes normally and its Segment is undoable.
        c.handle_frame(DeviceFrame::DictStop { dict: 2 }, t0);
        c.handle_recog(
            RecogEvent::Final {
                dict: 2,
                text: "继续写".into(),
            },
            t0,
        );
        // Voice Notes stops while the Segment is pending Undo.
        c.handle_frame(DeviceFrame::NotesToggle, t0);
        c.handle_notes(NotesReply::Stopped(Ok(())), t0);
        c.handle_frame(DeviceFrame::Undo, t0);
        assert_eq!(
            c.injector.log,
            [
                format!("insert {WECHAT} 第一句。"),
                format!("insert {WECHAT} 继续写。"),
                format!("delete {WECHAT} 4")
            ]
        );
        assert_eq!(
            c.target(),
            Some(&StoredTarget::App {
                bundle_id: WECHAT.into()
            })
        );
        // Polls are independent of the Dictation-time refresh pause.
        c.handle_frame(DeviceFrame::DictStart { dict: 3 }, t0);
        c.take_notes_ops();
        c.handle_notes(NotesReply::Status(Err(NotesError::NotRunning)), t0);
        assert!(c.dictation_active());
        c.notes_poll();
        assert_eq!(c.take_notes_ops(), [NotesOp::Status]);
    }

    #[test]
    fn notes_state_waits_for_hello() {
        let mut c = companion();
        c.handle_frame(DeviceFrame::NotesToggle, Instant::now());
        assert!(c.take_outbox().is_empty());
        assert_eq!(c.take_notes_ops(), [NotesOp::Start]);
    }

    // ----- Alerts ----------------------------------------------------------

    fn watch_snap(c: &C, titles: &[(&str, &str)]) -> OrcaSnapshot {
        let mut snap = c.orca.snap.clone();
        for (h, t) in titles {
            let s = snap.sessions.iter_mut().find(|s| s.handle == *h).unwrap();
            s.raw_title = (*t).into();
            s.preview = "测试都通过了，要提交吗？\n✻ Baked for 3s".into();
        }
        snap
    }

    fn alert_frames(out: &[CompanionFrame]) -> Vec<CompanionFrame> {
        out.iter()
            .filter(|f| {
                matches!(
                    f,
                    CompanionFrame::Alert { .. } | CompanionFrame::AlertClear { .. }
                )
            })
            .cloned()
            .collect()
    }

    #[test]
    fn alert_raised_on_turn_end_and_opened() {
        let mut c = companion();
        let t0 = Instant::now();
        hello(&mut c, t0);
        let snaps_before = c.orca.snapshots;
        let snap = watch_snap(&c, &[("term_c", "◐ PR")]);
        c.handle_orca_watch(Ok(snap), t0);
        assert!(
            alert_frames(&c.take_outbox()).is_empty(),
            "first sight never alerts"
        );
        let snap = watch_snap(&c, &[("term_c", "✳ PR")]);
        c.handle_orca_watch(Ok(snap), t0 + Duration::from_secs(2));
        assert_eq!(
            c.take_outbox(),
            [CompanionFrame::Alert {
                id: 1,
                app: 0,
                label: "voice-notes · PR".into(),
                message: "测试都通过了，要提交吗？".into()
            }]
        );
        // The watch result fed the cache; the core path ran no CLI.
        assert_eq!(c.orca.snapshots, snaps_before);
        // A reconnect resends it after HELLO.
        let out = hello(&mut c, t0 + Duration::from_secs(3));
        assert!(matches!(
            out.last().unwrap(),
            CompanionFrame::Alert { id: 1, .. }
        ));
        // Open: Jump to that session.
        c.handle_frame(
            DeviceFrame::AlertOpen { id: 1 },
            t0 + Duration::from_secs(4),
        );
        assert_eq!(c.take_outbox(), [orca_state("voice-notes · PR")]);
        assert_eq!(c.orca.log, ["switch term_c"]);
        assert_eq!(c.injector.log, [format!("activate {ORCA_BUNDLE_ID}")]);
        assert!(
            matches!(c.target(), Some(StoredTarget::Orca { handle, .. }) if handle == "term_c")
        );
        // A stale id: cleared on the Device, Target reported.
        c.handle_frame(
            DeviceFrame::AlertOpen { id: 1 },
            t0 + Duration::from_secs(5),
        );
        let out = c.take_outbox();
        assert_eq!(out[0], CompanionFrame::AlertClear { id: 1 });
        assert!(matches!(out[1], CompanionFrame::TargetState { .. }));
    }

    #[test]
    fn alert_cleared_dismissed_and_focus_independent() {
        let mut c = companion();
        let t0 = Instant::now();
        hello(&mut c, t0);
        c.handle_orca_watch(Ok(watch_snap(&c, &[("term_b", "◐ server")])), t0);
        c.handle_orca_watch(Ok(watch_snap(&c, &[("term_b", "✳ server")])), t0);
        assert_eq!(alert_frames(&c.take_outbox()).len(), 1);
        // Working again: the Alert vanishes.
        c.handle_orca_watch(Ok(watch_snap(&c, &[("term_b", "◑ server")])), t0);
        assert_eq!(c.take_outbox(), [CompanionFrame::AlertClear { id: 1 }]);
        // Dismissed on the Device: no Clear later, no repeat while waiting.
        c.handle_orca_watch(Ok(watch_snap(&c, &[("term_b", "✳ server")])), t0);
        c.take_outbox();
        c.handle_frame(DeviceFrame::AlertDismiss { id: 1 }, t0);
        c.handle_orca_watch(Ok(watch_snap(&c, &[("term_b", "◑ server")])), t0);
        assert!(c.take_outbox().is_empty());
        // Mirrors Orca's notifications: the session the user is looking at
        // in Orca (frontmost, current) alerts too, and stays until it works.
        c.injector.frontmost = Some(ORCA_BUNDLE_ID.into());
        c.handle_orca_watch(
            Ok(watch_snap(
                &c,
                &[("term_a", "◐ 语音输入"), ("term_b", "◑ server")],
            )),
            t0,
        );
        c.handle_orca_watch(
            Ok(watch_snap(
                &c,
                &[("term_a", "✳ 语音输入"), ("term_b", "◑ server")],
            )),
            t0,
        );
        let out = alert_frames(&c.take_outbox());
        assert!(
            matches!(out[..], [CompanionFrame::Alert { id: 2, .. }]),
            "{out:?}"
        );
        c.handle_orca_watch(
            Ok(watch_snap(
                &c,
                &[("term_a", "✳ 语音输入"), ("term_b", "◑ server")],
            )),
            t0,
        );
        assert!(c.take_outbox().is_empty(), "focus does not clear it");
        // Errors from the watch are ignored.
        c.handle_orca_watch(Err(OrcaError::Unavailable("down".into())), t0);
        assert!(c.take_outbox().is_empty());
    }

    #[test]
    fn alerts_do_not_disturb_dictation() {
        let mut c = companion();
        let t0 = Instant::now();
        hello(&mut c, t0);
        c.handle_orca_watch(Ok(watch_snap(&c, &[("term_c", "◐ PR")])), t0);
        c.handle_frame(DeviceFrame::DictStart { dict: 1 }, t0);
        partial(&mut c, 1, "你好", t0);
        c.take_outbox();
        c.handle_orca_watch(Ok(watch_snap(&c, &[("term_c", "✳ PR")])), t0);
        assert!(c.dictation_active());
        assert!(matches!(
            c.take_outbox()[..],
            [CompanionFrame::Alert { .. }]
        ));
        c.handle_frame(DeviceFrame::DictStop { dict: 1 }, t0);
        c.handle_recog(
            RecogEvent::Final {
                dict: 1,
                text: "你好".into(),
            },
            t0,
        );
        assert_eq!(c.injector.log, [format!("insert {WECHAT} 你好。")]);
        // Link loss forgets Alerts; a relink starts tracking afresh.
        c.on_disconnected();
        hello(&mut c, t0);
        c.handle_orca_watch(Ok(watch_snap(&c, &[("term_c", "✳ PR")])), t0);
        assert!(alert_frames(&c.take_outbox()).is_empty());
    }

    #[test]
    fn delivered_and_undone_dictations_become_events() {
        let mut c = companion();
        let t0 = Instant::now();
        hello(&mut c, t0);
        c.handle_frame(DeviceFrame::DictStart { dict: 7 }, t0);
        c.handle_frame(DeviceFrame::DictStop { dict: 7 }, t0);
        c.handle_recog(
            RecogEvent::Final {
                dict: 7,
                text: "你好".into(),
            },
            t0,
        );
        assert_eq!(
            c.take_events(),
            [DictationEvent::Delivered {
                dict: 7,
                text: "你好。".into(),
                target: RecordTarget {
                    key: format!("app:{WECHAT}:张三"),
                    app: "微信".into(),
                    label: "张三".into(),
                },
                pcm: vec![],
            }]
        );
        c.handle_frame(DeviceFrame::Undo, t0);
        assert_eq!(c.take_events(), [DictationEvent::Undone { dict: 7 }]);
        // Nothing left to undo: no event.
        c.handle_frame(DeviceFrame::Undo, t0);
        assert!(c.take_events().is_empty());
    }

    #[test]
    fn orca_dictation_event_names_the_session() {
        let mut c = companion();
        c.injector.frontmost = Some(ORCA_BUNDLE_ID.into());
        let t0 = Instant::now();
        hello(&mut c, t0);
        c.handle_frame(DeviceFrame::DictStart { dict: 2 }, t0);
        c.handle_frame(DeviceFrame::DictStop { dict: 2 }, t0);
        c.handle_recog(
            RecogEvent::Final {
                dict: 2,
                text: "跑一下测试".into(),
            },
            t0,
        );
        let ev = c.take_events();
        let [DictationEvent::Delivered { target, .. }] = &ev[..] else {
            panic!("expected one delivery, got {ev:?}");
        };
        assert!(target.key.starts_with("orca:"), "{target:?}");
        assert_eq!(target.app, "Orca");
        assert_eq!(target.label, "my-passport · 语音输入");
    }

    #[test]
    fn cancelled_or_failed_dictations_make_no_event() {
        let mut c = companion();
        let t0 = Instant::now();
        hello(&mut c, t0);
        c.handle_frame(DeviceFrame::DictStart { dict: 3 }, t0);
        c.handle_frame(DeviceFrame::DictCancel { dict: 3 }, t0);
        c.injector.fail_insert = Some(InjectError::Failed("x".into()));
        c.handle_frame(DeviceFrame::DictStart { dict: 4 }, t0);
        c.handle_frame(DeviceFrame::DictStop { dict: 4 }, t0);
        c.handle_recog(
            RecogEvent::Final {
                dict: 4,
                text: "你好".into(),
            },
            t0,
        );
        assert!(c.take_events().is_empty());
    }
}
