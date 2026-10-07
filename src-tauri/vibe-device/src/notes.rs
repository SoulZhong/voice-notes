//! Voice Notes Recordings, controlled from the Device (OK double press).
//!
//! The Companion now lives inside Voice Notes, so the control is an in-process
//! call (the host implements [`VoiceNotesApi`]). `start` can still take
//! seconds while the recognition model loads and `stop` blocks until the note
//! is finalized, so the calls run on [`NotesWorker`]'s thread, never on the
//! core loop (Dictation audio must keep flowing). The session only queues
//! [`NotesOp`]s and handles [`NotesReply`]s.

use std::sync::mpsc;

/// Risk kinds Voice Notes reports when a recording starts (`precheck.rs`).
pub const RISK_VOICE_ISOLATION: &str = "voice_isolation";
pub const RISK_BLUETOOTH_MIC: &str = "bluetooth_mic";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NotesPhase {
    Idle,
    Recording,
    Paused,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NotesStatus {
    pub phase: NotesPhase,
    pub elapsed_ms: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NotesError {
    /// Voice Notes is not running (no socket).
    NotRunning,
    /// Voice Notes is not installed.
    NotInstalled,
    /// Launched, but the socket did not answer in time.
    LaunchFailed,
    /// The user has not allowed control ("允许 AI 控制录制" is off).
    ControlDisabled,
    /// Voice Notes refused or failed.
    Failed(String),
}

impl std::fmt::Display for NotesError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            NotesError::NotRunning => write!(f, "Voice Notes is not running"),
            NotesError::NotInstalled => write!(f, "Voice Notes is not installed"),
            NotesError::LaunchFailed => write!(f, "Voice Notes did not start in time"),
            NotesError::ControlDisabled => write!(f, "Voice Notes control is disabled"),
            NotesError::Failed(m) => write!(f, "Voice Notes: {m}"),
        }
    }
}

/// One request for the worker.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NotesOp {
    Status,
    Start,
    Stop,
}

/// The worker's answer to a [`NotesOp`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NotesReply {
    Status(Result<NotesStatus, NotesError>),
    /// Started; the pre-record risk kinds (may be empty).
    Started(Result<Vec<String>, NotesError>),
    Stopped(Result<(), NotesError>),
}

/// Voice Notes control, behind a trait so tests never touch the real app.
pub trait VoiceNotesApi: Send {
    fn status(&mut self) -> Result<NotesStatus, NotesError>;
    /// Start a recording (nothing to do when one is already running).
    fn start(&mut self) -> Result<Vec<String>, NotesError>;
    fn stop(&mut self) -> Result<(), NotesError>;
}

/// Runs Voice Notes calls on a dedicated thread: ops in, replies out.
pub struct NotesWorker {
    tx: mpsc::Sender<NotesOp>,
}

impl NotesWorker {
    /// `reply` is called on the worker thread for every op, in order.
    pub fn spawn<A, F>(mut api: A, reply: F) -> Self
    where
        A: VoiceNotesApi + 'static,
        F: Fn(NotesReply) + Send + 'static,
    {
        let (tx, rx) = mpsc::channel::<NotesOp>();
        std::thread::Builder::new()
            .name("voice-notes".into())
            .spawn(move || {
                for op in rx {
                    let r = match op {
                        NotesOp::Status => NotesReply::Status(api.status()),
                        NotesOp::Start => NotesReply::Started(api.start()),
                        NotesOp::Stop => NotesReply::Stopped(api.stop()),
                    };
                    reply(r);
                }
            })
            .expect("voice-notes thread");
        Self { tx }
    }

    pub fn send(&self, op: NotesOp) {
        let _ = self.tx.send(op);
    }
}

