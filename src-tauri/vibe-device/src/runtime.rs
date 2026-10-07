//! Process wiring: the BLE thread, the core loop, the Orca watch and the
//! Voice Notes worker. The host starts a [`Runtime`] when the user connects a
//! Device and stops it when they turn the feature off.

use crate::ble::{self, LinkEnd, LinkEvent, LinkOptions, LinkState};
use crate::config::{self, StoredTarget};
use crate::notes::{NotesReply, NotesWorker, VoiceNotesApi};
use crate::orca::{OrcaApi, OrcaClient, OrcaError, OrcaSnapshot, ProcessRunner};
use crate::platform::{self, SystemInjector};
use crate::protocol::DeviceFrame;
use crate::session::{
    Companion, DictationEvent, RecogEvent, Recognizer, RecognizerHealth, RecognizerStartError,
};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// Where recognizer results go; the recognizer calls it from any thread.
pub type RecogSink = Arc<dyn Fn(RecogEvent) + Send + Sync>;

/// Builds the recognizer on the core thread (Apple Speech objects must be
/// created and used there).
pub type RecognizerFactory = Box<dyn FnOnce(RecogSink) -> Box<dyn Recognizer> + Send>;

impl Recognizer for Box<dyn Recognizer> {
    fn health(&mut self) -> RecognizerHealth {
        (**self).health()
    }
    fn start(&mut self, dict: u8) -> Result<(), RecognizerStartError> {
        (**self).start(dict)
    }
    fn push(&mut self, pcm: &[i16]) {
        (**self).push(pcm)
    }
    fn finish(&mut self) {
        (**self).finish()
    }
    fn cancel(&mut self) {
        (**self).cancel()
    }
}

/// What the runtime tells Voice Notes. Called from the runtime's threads.
pub trait Host: Send + Sync + 'static {
    /// The link changed state (for the settings page and the tray).
    fn link_state(&self, state: &LinkState);
    /// The link gave up and needs the user (pairing failed, bond lost).
    fn link_ended(&self, end: LinkEnd);
    /// A Device linked; remember its name to prefer it next time.
    fn device_connected(&self, name: &str);
    /// The Device said HELLO: its protocol version and firmware string. A
    /// version other than [`crate::protocol::PROTOCOL_VERSION`] means one side needs updating.
    fn device_hello(&self, ver: u8, fw: &str);
    /// A Dictation was delivered or undone (for the dictation notes).
    fn dictation(&self, event: DictationEvent);
}

pub struct RuntimeConfig {
    /// Where the Target is persisted.
    pub target_store: PathBuf,
    pub link: LinkOptions,
    /// Play a simulated Device from this directory instead of Bluetooth
    /// (end-to-end tests; see [`crate::sim`]).
    pub sim: Option<PathBuf>,
}

enum CoreEvent {
    Link(LinkEvent),
    Recog(RecogEvent),
    Notes(NotesReply),
    OrcaWatch(Result<OrcaSnapshot, OrcaError>),
    Shutdown,
}

const HEALTH_INTERVAL: Duration = Duration::from_secs(2);

/// A running Device link. Dropping it stops everything.
pub struct Runtime {
    core_tx: mpsc::Sender<CoreEvent>,
    ble_stop: Option<tokio::sync::oneshot::Sender<()>>,
    alive: Arc<AtomicBool>,
}

impl Runtime {
    pub fn start(
        cfg: RuntimeConfig,
        recognizer: RecognizerFactory,
        notes: impl VoiceNotesApi + 'static,
        host: Arc<dyn Host>,
    ) -> std::io::Result<Self> {
        let (core_tx, core_rx) = mpsc::channel::<CoreEvent>();
        let (out_tx, out_rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
        let (stop_tx, stop_rx) = tokio::sync::oneshot::channel::<()>();
        let alive = Arc::new(AtomicBool::new(true));
        let linked = Arc::new(AtomicBool::new(false));

        // BLE on its own tokio runtime.
        {
            let link_tx = core_tx.clone();
            let host = host.clone();
            let opts = cfg.link.clone();
            let sim = cfg.sim.clone();
            std::thread::Builder::new()
                .name("device-ble".into())
                .spawn(move || {
                    let rt = match tokio::runtime::Builder::new_multi_thread()
                        .worker_threads(2)
                        .enable_all()
                        .build()
                    {
                        Ok(rt) => rt,
                        Err(e) => {
                            log::error!("device: tokio runtime: {e}");
                            return;
                        }
                    };
                    let events: ble::EventSink = Arc::new(move |e| {
                        let _ = link_tx.send(CoreEvent::Link(e));
                    });
                    let last = std::sync::Mutex::new(None::<LinkState>);
                    let state_host = host.clone();
                    let state: ble::StateSink = Arc::new(move |s: &LinkState| {
                        let mut last = last.lock().unwrap_or_else(|e| e.into_inner());
                        if last.as_ref() != Some(s) {
                            log::info!("device link: {}", s.text());
                            state_host.link_state(s);
                            *last = Some(s.clone());
                        }
                    });
                    let end = rt.block_on(async move {
                        let link = async move {
                            match sim {
                                Some(dir) => crate::sim::run(dir, out_rx, events, state).await,
                                None => ble::run(out_rx, opts, events, state).await,
                            }
                        };
                        tokio::select! {
                            end = link => end,
                            _ = stop_rx => LinkEnd::Stopped,
                        }
                    });
                    log::info!("device link ended: {end:?}");
                    if end != LinkEnd::Stopped {
                        host.link_ended(end);
                    }
                    rt.shutdown_timeout(Duration::from_secs(1));
                })?;
        }

        // Orca watch: polls the terminal list every 2 s while linked, on its
        // own thread (the CLI takes ~0.1 s), for Alerts and the Orca cache.
        {
            let linked = linked.clone();
            let alive = alive.clone();
            let tx = core_tx.clone();
            std::thread::Builder::new()
                .name("device-orca-watch".into())
                .spawn(move || orca_watch(linked, alive, tx))?;
        }

        // The Companion on its own thread.
        {
            let self_tx = core_tx.clone();
            let host = host.clone();
            let pinned = cfg.sim.is_some();
            std::thread::Builder::new()
                .name("device-core".into())
                .spawn(move || {
                    core_loop(cfg.target_store, core_rx, self_tx, out_tx, recognizer, notes, host, linked, pinned)
                })?;
        }

        Ok(Self {
            core_tx,
            ble_stop: Some(stop_tx),
            alive,
        })
    }

    pub fn stop(&mut self) {
        self.alive.store(false, Ordering::Relaxed);
        if let Some(tx) = self.ble_stop.take() {
            let _ = tx.send(());
        }
        let _ = self.core_tx.send(CoreEvent::Shutdown);
    }
}

impl Drop for Runtime {
    fn drop(&mut self) {
        self.stop();
    }
}

fn orca_watch(linked: Arc<AtomicBool>, alive: Arc<AtomicBool>, tx: mpsc::Sender<CoreEvent>) {
    use crate::alerts::{TurnWatch, screen_message};
    let mut client = OrcaClient::new(ProcessRunner::locate());
    let mut turns = TurnWatch::default();
    while alive.load(Ordering::Relaxed) {
        std::thread::sleep(HEALTH_INTERVAL);
        if !linked.load(Ordering::Relaxed) || !platform::app_running(config::ORCA_BUNDLE_ID) {
            turns = TurnWatch::default();
            continue;
        }
        let mut snap = client.snapshot();
        // A session just finished its turn: read its rendered screen once for
        // the agent's reply (the list preview holds only status lines).
        // Read-only, bounded, and here rather than on the core loop.
        if let Ok(snap) = snap.as_mut() {
            for handle in turns.turned_waiting(&snap.sessions) {
                let reply = match client.read_screen(&handle) {
                    Ok(lines) => screen_message(&lines),
                    Err(e) => {
                        log::info!("screen read for {handle}: {e}");
                        None
                    }
                };
                if let Some(s) = snap.sessions.iter_mut().find(|s| s.handle == handle) {
                    s.reply = reply;
                }
            }
        }
        if tx.send(CoreEvent::OrcaWatch(snap)).is_err() {
            return;
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn core_loop(
    store: PathBuf,
    rx: mpsc::Receiver<CoreEvent>,
    self_tx: mpsc::Sender<CoreEvent>,
    out: tokio::sync::mpsc::UnboundedSender<Vec<u8>>,
    make_recognizer: RecognizerFactory,
    notes_api: impl VoiceNotesApi + 'static,
    host: Arc<dyn Host>,
    linked: Arc<AtomicBool>,
    pinned: bool,
) {
    let notes_tx = self_tx.clone();
    let recognizer = make_recognizer(Arc::new(move |e| {
        let _ = self_tx.send(CoreEvent::Recog(e));
    }));
    let orca = OrcaClient::new(ProcessRunner::locate());
    // Voice Notes calls can block for seconds (model load, finalize): they
    // run on their own thread so Dictation audio keeps flowing.
    let notes = NotesWorker::spawn(notes_api, move |r| {
        let _ = notes_tx.send(CoreEvent::Notes(r));
    });
    let target = StoredTarget::load(&store);
    log::info!("device: stored Target {target:?}");
    let mut c = Companion::new(SystemInjector, recognizer, orca, target, Some(store));
    if pinned {
        c.pin_target();
    }
    let mut connected = false;
    let mut deadline: Option<Instant> = None;
    let mut next_health = Instant::now() + HEALTH_INTERVAL;
    loop {
        // Focus, permissions and Orca tabs change while running: refresh the
        // Target and STATUS while linked.
        if connected {
            deadline = Some(deadline.map_or(next_health, |d| d.min(next_health)));
        }
        let ev = match deadline {
            Some(d) => match rx.recv_timeout(d.saturating_duration_since(Instant::now())) {
                Ok(ev) => Some(ev),
                Err(RecvTimeoutError::Timeout) => None,
                Err(RecvTimeoutError::Disconnected) => break,
            },
            None => match rx.recv() {
                Ok(ev) => Some(ev),
                Err(_) => break,
            },
        };
        let now = Instant::now();
        match ev {
            Some(CoreEvent::Shutdown) => break,
            Some(CoreEvent::Link(LinkEvent::Connected(name))) => {
                log::info!("Device {name} linked; waiting for HELLO");
                connected = true;
                linked.store(true, Ordering::Relaxed);
                host.device_connected(&name);
            }
            Some(CoreEvent::Link(LinkEvent::Disconnected)) => {
                connected = false;
                linked.store(false, Ordering::Relaxed);
                c.on_disconnected();
            }
            Some(CoreEvent::Link(LinkEvent::Frame(bytes))) => match DeviceFrame::decode(&bytes) {
                Ok(frame) => {
                    if !matches!(frame, DeviceFrame::Audio(_)) {
                        log::debug!("<- {frame:?}");
                    }
                    if let DeviceFrame::Hello { ver, fw } = &frame {
                        host.device_hello(*ver, fw);
                    }
                    c.handle_frame(frame, now);
                }
                Err(e) => log::warn!("bad frame from Device: {e}"),
            },
            Some(CoreEvent::Recog(e)) => c.handle_recog(e, now),
            Some(CoreEvent::Notes(r)) => c.handle_notes(r, now),
            Some(CoreEvent::OrcaWatch(r)) => c.handle_orca_watch(r, now),
            None => {}
        }
        if connected && now >= next_health {
            next_health = now + HEALTH_INTERVAL;
            c.refresh(now);
            c.notes_poll();
        }
        for op in c.take_notes_ops() {
            notes.send(op);
        }
        for e in c.take_events() {
            host.dictation(e);
        }
        deadline = c.poll(Instant::now());
        for f in c.take_outbox() {
            log::debug!("-> {f:?}");
            if connected {
                let _ = out.send(f.encode());
            }
        }
    }
    c.recognizer.cancel();
    linked.store(false, Ordering::Relaxed);
    log::info!("device core loop stopped");
}
