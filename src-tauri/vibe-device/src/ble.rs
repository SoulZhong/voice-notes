//! BLE central: find a `VibeVoice-` Device, connect, pair, subscribe to NUS TX
//! and carry frames both ways. Reconnects forever with backoff.
//!
//! Pairing differs by platform. macOS pairs by itself: the first access to an
//! encrypted characteristic makes the system ask for the passkey. Windows
//! never asks, so the link pairs through WinRT first (`pair_windows`) with
//! the passkey from a [`PinProvider`] (the Voice Notes pairing dialog).

use crate::protocol::{MAX_FRAME, NAME_PREFIX, NUS_RX, NUS_SERVICE, NUS_TX, ty};
use btleplug::api::{
    Central, CentralEvent, Characteristic, Manager as _, Peripheral as _, ScanFilter, WriteType,
};
use btleplug::platform::{Adapter, Manager, Peripheral};
use futures::StreamExt;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc::UnboundedReceiver;
use tokio::time::{sleep, timeout};
use uuid::Uuid;

/// A healthy write with response completes within one or two connection
/// intervals; far longer means the link is gone.
const WRITE_TIMEOUT: Duration = Duration::from_secs(5);

/// Link events delivered to the Companion.
#[derive(Debug)]
pub enum LinkEvent {
    Connected(String),
    Frame(Vec<u8>),
    Disconnected,
}

/// Link state for the Voice Notes UI and the log.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum LinkState {
    /// No Bluetooth adapter, Bluetooth off, or permission denied.
    Unavailable,
    Searching,
    Connecting { name: String },
    /// Waiting for the passkey (macOS: the system prompt; Windows: ours).
    Pairing { name: String },
    PairingFailed { name: String, reason: String },
    /// Windows holds a bond the Device no longer has (re-flashed, or it
    /// dropped this computer): pair again.
    BondLost { name: String },
    Connected { name: String },
    /// Lost or failed; trying again shortly.
    Retrying,
}

impl LinkState {
    /// Short Chinese text for the UI.
    pub fn text(&self) -> String {
        match self {
            LinkState::Unavailable => "蓝牙不可用".into(),
            LinkState::Searching => "搜索设备…".into(),
            LinkState::Connecting { name } => format!("连接中 {name}"),
            LinkState::Pairing { name } => {
                if cfg!(windows) {
                    format!("配对中 {name}:请输入设备屏幕上的 6 位配对码")
                } else {
                    format!("配对中 {name}:请在系统弹窗输入设备屏幕上的 6 位配对码")
                }
            }
            LinkState::PairingFailed { name, .. } => format!("{name} 配对失败"),
            LinkState::BondLost { name } => format!("{name} 的配对已失效,请重新配对"),
            LinkState::Connected { name } => format!("已连接 {name}"),
            LinkState::Retrying => "未连接,稍后重试".into(),
        }
    }
}

pub type StateSink = Arc<dyn Fn(&LinkState) + Send + Sync>;
pub type EventSink = Arc<dyn Fn(LinkEvent) + Send + Sync>;

/// Asks the user for the passkey the Device shows. Blocks until the user
/// answers; `None` when they cancel or time out. Only Windows asks.
pub trait PinProvider: Send + Sync {
    fn request_pin(&self, device_name: &str) -> Option<String>;
}

/// What the link should look for.
#[derive(Clone, Default)]
pub struct LinkOptions {
    /// Exact Device name to prefer (the one paired last); any Device when
    /// `None` or out of range.
    pub wanted: Option<String>,
    /// Passkey source for in-app pairing (Windows).
    pub pins: Option<Arc<dyn PinProvider>>,
}

const SCAN_WINDOW: Duration = Duration::from_secs(4);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);
/// Time allowed for the user to type the passkey into the macOS prompt.
const PAIRING_WINDOW: Duration = Duration::from_secs(120);
const MAX_BACKOFF: Duration = Duration::from_secs(30);

fn uuid(s: &str) -> Uuid {
    Uuid::parse_str(s).expect("valid UUID constant")
}

async fn adapter(manager: &Manager) -> Option<Adapter> {
    match manager.adapters().await {
        Ok(list) => list.into_iter().next(),
        Err(e) => {
            log::warn!("Bluetooth adapters unavailable: {e}");
            None
        }
    }
}

/// A Device seen during a scan.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct FoundDevice {
    pub name: String,
    pub rssi: Option<i16>,
    /// Whether this computer holds a bond with it (Windows only; `None`
    /// where the system does not tell).
    pub paired: Option<bool>,
}

/// All Devices in range, strongest first.
async fn scan_devices(adapter: &Adapter, window: Duration) -> Vec<(Peripheral, FoundDevice)> {
    if let Err(e) = adapter.start_scan(ScanFilter::default()).await {
        log::warn!("cannot scan (Bluetooth off or permission denied?): {e}");
        return Vec::new();
    }
    sleep(window).await;
    let mut found: Vec<(Peripheral, FoundDevice)> = Vec::new();
    if let Ok(list) = adapter.peripherals().await {
        for p in list {
            let Ok(Some(props)) = p.properties().await else {
                continue;
            };
            let Some(name) = props.local_name else {
                continue;
            };
            if !name.starts_with(NAME_PREFIX) || found.iter().any(|(_, d)| d.name == name) {
                continue;
            }
            let paired = paired_state(&p).await;
            found.push((
                p,
                FoundDevice {
                    name,
                    rssi: props.rssi,
                    paired,
                },
            ));
        }
    }
    let _ = adapter.stop_scan().await;
    found.sort_by_key(|(_, d)| std::cmp::Reverse(d.rssi.unwrap_or(i16::MIN)));
    found
}

/// Scan once for nearby Devices (for the "连接设备" dialog).
pub async fn scan(window: Duration) -> Result<Vec<FoundDevice>, String> {
    let manager = Manager::new().await.map_err(|e| e.to_string())?;
    let adapter = adapter(&manager)
        .await
        .ok_or_else(|| "no Bluetooth adapter".to_string())?;
    Ok(scan_devices(&adapter, window)
        .await
        .into_iter()
        .map(|(_, d)| d)
        .collect())
}

/// Remove this computer's bond with the Device `name` so the next link pairs
/// afresh. Windows only: macOS keeps bonds in System Settings.
pub async fn forget(name: &str) -> Result<(), String> {
    #[cfg(windows)]
    {
        let manager = Manager::new().await.map_err(|e| e.to_string())?;
        let adapter = adapter(&manager)
            .await
            .ok_or_else(|| "no Bluetooth adapter".to_string())?;
        let found = scan_devices(&adapter, SCAN_WINDOW).await;
        let Some((p, _)) = found.into_iter().find(|(_, d)| d.name == name) else {
            return Err(format!("{name} is not in range"));
        };
        let address = u64::from(p.address());
        tokio::task::spawn_blocking(move || crate::pair_windows::unpair(address))
            .await
            .map_err(|e| e.to_string())?
            .map_err(|e| e.to_string())
    }
    #[cfg(not(windows))]
    {
        Err(format!("remove {name} in the system Bluetooth settings"))
    }
}

#[cfg(windows)]
async fn paired_state(p: &Peripheral) -> Option<bool> {
    let address = u64::from(p.address());
    tokio::task::spawn_blocking(move || crate::pair_windows::is_paired(address))
        .await
        .ok()
}

#[cfg(not(windows))]
async fn paired_state(_: &Peripheral) -> Option<bool> {
    None
}

/// Scan until a Device shows up. Prefers `wanted` (exact name) when given.
async fn find_device(adapter: &Adapter, wanted: Option<&str>) -> Option<(Peripheral, String)> {
    scan_devices(adapter, SCAN_WINDOW)
        .await
        .into_iter()
        .find(|(_, d)| wanted.is_none_or(|w| w == d.name))
        .map(|(p, d)| (p, d.name))
}

fn is_auth_error(msg: &str) -> bool {
    let m = msg.to_lowercase();
    m.contains("auth") || m.contains("encrypt") || m.contains("pair") || m.contains("insufficient")
}

/// Windows: make sure Windows holds a bond before touching the Device.
/// `Err` ends the link: pairing needs the user again.
#[cfg(windows)]
async fn ensure_paired(
    p: &Peripheral,
    name: &str,
    opts: &LinkOptions,
    state: &StateSink,
) -> Result<(), Session> {
    if paired_state(p).await == Some(true) {
        return Ok(());
    }
    let Some(pins) = opts.pins.clone() else {
        log::warn!("{name} is not paired and no passkey source is set");
        return Err(Session::PairingFailed);
    };
    state(&LinkState::Pairing {
        name: name.to_owned(),
    });
    let address = u64::from(p.address());
    let owned = name.to_owned();
    let r = tokio::task::spawn_blocking(move || crate::pair_windows::pair(address, &owned, pins))
        .await;
    match r {
        Ok(Ok(())) => {
            log::info!("paired with {name}");
            Ok(())
        }
        Ok(Err(e)) => {
            log::warn!("pairing {name}: {e}");
            let reason = match e {
                crate::pair_windows::PairError::Cancelled => "已取消".to_owned(),
                crate::pair_windows::PairError::Failed(m) => m,
            };
            state(&LinkState::PairingFailed {
                name: name.to_owned(),
                reason,
            });
            Err(Session::PairingFailed)
        }
        Err(e) => {
            log::warn!("pairing task: {e}");
            Err(Session::Failed)
        }
    }
}

#[cfg(not(windows))]
async fn ensure_paired(
    _: &Peripheral,
    _: &str,
    _: &LinkOptions,
    _: &StateSink,
) -> Result<(), Session> {
    Ok(())
}

/// How subscribing ended.
enum Subscribed {
    Yes,
    No,
    /// Windows: bonded, yet the Device refuses encryption — its bond is gone.
    BondLost,
}

/// Subscribe to TX, waiting for the user to complete pairing if needed.
async fn subscribe_with_pairing(
    p: &Peripheral,
    tx: &Characteristic,
    name: &str,
    state: &StateSink,
) -> Subscribed {
    let deadline = tokio::time::Instant::now() + PAIRING_WINDOW;
    let mut prompted = false;
    loop {
        match timeout(Duration::from_secs(60), p.subscribe(tx)).await {
            Ok(Ok(())) => return Subscribed::Yes,
            Ok(Err(e)) => {
                let msg = e.to_string();
                if is_auth_error(&msg) {
                    if cfg!(windows) {
                        // Paired just before (ensure_paired): the bond is stale.
                        log::warn!("{name} refused the bonded link ({msg})");
                        return Subscribed::BondLost;
                    }
                    if !prompted {
                        prompted = true;
                        state(&LinkState::Pairing {
                            name: name.to_owned(),
                        });
                        log::warn!(
                            "pairing required ({msg}); type the passkey shown on the Device into the system prompt"
                        );
                    }
                } else {
                    log::warn!("subscribe failed: {msg}");
                }
            }
            Err(_) => log::warn!("subscribe timed out (pairing prompt still open?)"),
        }
        if tokio::time::Instant::now() >= deadline || !p.is_connected().await.unwrap_or(false) {
            return Subscribed::No;
        }
        sleep(Duration::from_secs(2)).await;
    }
}

/// How one connection attempt ended.
#[derive(Debug, PartialEq, Eq)]
enum Session {
    /// The link came up (and later dropped).
    Ran,
    Failed,
    /// In-app pairing failed or was cancelled (Windows).
    #[cfg_attr(not(windows), allow(dead_code))]
    PairingFailed,
    BondLost,
}

/// Why [`run`] gave up. Each needs the user: retrying on its own would only
/// pop the passkey dialog again and again.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LinkEnd {
    /// The runtime stopped.
    Stopped,
    PairingFailed,
    BondLost,
}

/// Run one connection until it drops.
async fn run_session(
    adapter: &Adapter,
    p: Peripheral,
    name: &str,
    opts: &LinkOptions,
    outgoing: &mut UnboundedReceiver<Vec<u8>>,
    events: &EventSink,
    state: &StateSink,
) -> Session {
    if let Err(end) = ensure_paired(&p, name, opts, state).await {
        return end;
    }
    state(&LinkState::Connecting {
        name: name.to_owned(),
    });
    match timeout(CONNECT_TIMEOUT, p.connect()).await {
        Ok(Ok(())) => {}
        Ok(Err(e)) => {
            log::warn!("connect {name}: {e}");
            return Session::Failed;
        }
        Err(_) => {
            log::warn!("connect {name}: timeout");
            let _ = p.disconnect().await;
            return Session::Failed;
        }
    }
    if let Err(e) = p.discover_services().await {
        log::warn!("discover services: {e}");
        let _ = p.disconnect().await;
        return Session::Failed;
    }
    let chars = p.characteristics();
    let tx_uuid = uuid(NUS_TX);
    let rx_uuid = uuid(NUS_RX);
    let service = uuid(NUS_SERVICE);
    let tx = chars
        .iter()
        .find(|c| c.uuid == tx_uuid && c.service_uuid == service)
        .cloned();
    let rx = chars
        .iter()
        .find(|c| c.uuid == rx_uuid && c.service_uuid == service)
        .cloned();
    let (Some(tx), Some(rx)) = (tx, rx) else {
        log::warn!("{name} has no Nordic UART Service");
        let _ = p.disconnect().await;
        return Session::Failed;
    };
    let mut notifications = match p.notifications().await {
        Ok(s) => s,
        Err(e) => {
            log::warn!("notifications: {e}");
            let _ = p.disconnect().await;
            return Session::Failed;
        }
    };
    match subscribe_with_pairing(&p, &tx, name, state).await {
        Subscribed::Yes => {}
        Subscribed::No => {
            let _ = p.disconnect().await;
            return Session::Failed;
        }
        Subscribed::BondLost => {
            let _ = p.disconnect().await;
            state(&LinkState::BondLost {
                name: name.to_owned(),
            });
            return Session::BondLost;
        }
    }
    // Frames queued for an earlier link are stale.
    while outgoing.try_recv().is_ok() {}
    log::info!("connected to {name}");
    state(&LinkState::Connected {
        name: name.to_owned(),
    });
    events(LinkEvent::Connected(name.to_owned()));

    let id = p.id();
    let mut central = adapter.events().await.ok();
    let mut check = tokio::time::interval(Duration::from_secs(3));
    loop {
        tokio::select! {
            n = notifications.next() => match n {
                Some(n) if n.uuid == tx_uuid => events(LinkEvent::Frame(n.value)),
                Some(_) => {}
                None => break,
            },
            out = outgoing.recv() => {
                let Some(frame) = out else { break };
                debug_assert!(frame.len() <= MAX_FRAME);
                // PARTIAL is superseded by the next one; everything else must land.
                let kind = if frame.first() == Some(&ty::PARTIAL) { WriteType::WithoutResponse } else { WriteType::WithResponse };
                // A write to a link the system silently replaced never completes,
                // and this loop would then stop reading the Device's frames too.
                match timeout(WRITE_TIMEOUT, p.write(&rx, &frame, kind)).await {
                    Ok(Ok(())) => {}
                    Ok(Err(e)) => {
                        log::warn!("write failed: {e}");
                        if !p.is_connected().await.unwrap_or(false) {
                            break;
                        }
                    }
                    Err(_) => {
                        log::warn!("write timed out; reconnecting");
                        break;
                    }
                }
            }
            ev = async { match central.as_mut() { Some(s) => s.next().await, None => std::future::pending().await } } => {
                if let Some(CentralEvent::DeviceDisconnected(d)) = ev && d == id {
                    break;
                }
            }
            _ = check.tick() => {
                if !timeout(WRITE_TIMEOUT, p.is_connected()).await.ok().and_then(Result::ok).unwrap_or(false) {
                    break;
                }
            }
        }
    }
    log::info!("disconnected from {name}");
    events(LinkEvent::Disconnected);
    let _ = p.disconnect().await;
    Session::Ran
}

/// Keep a Device connected. Returns only when the user has to act (see
/// [`LinkEnd`]); the runtime stops it by dropping the future.
pub async fn run(
    mut outgoing: UnboundedReceiver<Vec<u8>>,
    opts: LinkOptions,
    events: EventSink,
    state: StateSink,
) -> LinkEnd {
    let mut backoff = Duration::from_secs(1);
    let manager = loop {
        match Manager::new().await {
            Ok(m) => break m,
            Err(e) => {
                log::error!("Bluetooth unavailable: {e}");
                state(&LinkState::Unavailable);
                sleep(MAX_BACKOFF).await;
            }
        }
    };
    let mut last_name: Option<String> = opts.wanted.clone();
    loop {
        if outgoing.is_closed() {
            return LinkEnd::Stopped;
        }
        let Some(adapter) = adapter(&manager).await else {
            state(&LinkState::Unavailable);
            sleep(backoff).await;
            backoff = (backoff * 2).min(MAX_BACKOFF);
            continue;
        };
        state(&LinkState::Searching);
        let mut found = find_device(&adapter, last_name.as_deref()).await;
        if found.is_none() && last_name.is_some() {
            // The remembered Device is away; accept any Vibe Voice Device.
            found = find_device(&adapter, None).await;
        }
        let Some((p, name)) = found else {
            // Nothing in range yet: keep scanning without growing the backoff.
            sleep(Duration::from_secs(1)).await;
            continue;
        };
        let session = run_session(&adapter, p, &name, &opts, &mut outgoing, &events, &state).await;
        match session {
            Session::Ran => {
                last_name = Some(name);
                backoff = Duration::from_secs(1);
                state(&LinkState::Retrying);
            }
            Session::BondLost => return LinkEnd::BondLost,
            Session::PairingFailed => return LinkEnd::PairingFailed,
            Session::Failed => {
                backoff = (backoff * 2).min(MAX_BACKOFF);
                state(&LinkState::Retrying);
            }
        }
        sleep(backoff).await;
    }
}
