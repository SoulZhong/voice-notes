//! In-app pairing on Windows.
//!
//! The Device accepts only encrypted, authenticated and bonded links (LE
//! Secure Connections, it displays a 6-digit passkey). Windows does not prompt
//! for the passkey when an app reads such characteristics — the access just
//! fails with "insufficient authentication" — and btleplug has no pairing API.
//! So Voice Notes pairs through WinRT itself: custom pairing with
//! `ProvidePin`, the passkey typed by the user into Voice Notes. Once paired,
//! the bond lives in Windows and btleplug's reads and writes just work.
//!
//! Two traps (reported by others pairing passkey devices from apps):
//! - plain `Pairing().PairAsync()` only handles confirm-only pairing and fails
//!   for a passkey device: use `Custom().PairAsync(ProvidePin)`;
//! - asking for protection level `EncryptionAndAuthentication` makes Windows
//!   fail before it ever raises `PairingRequested`; `Default` (what
//!   `Custom().PairAsync` uses) works.

use crate::ble::PinProvider;
use std::sync::Arc;
use std::time::Duration;
use windows::Devices::Bluetooth::BluetoothLEDevice;
use windows::Devices::Enumeration::{
    DeviceInformationCustomPairing, DevicePairingKinds, DevicePairingRequestedEventArgs,
    DevicePairingResultStatus, DeviceUnpairingResultStatus,
};
use windows::Foundation::TypedEventHandler;
use windows::core::HSTRING;

/// Longest wait for the user to type the passkey.
pub const PIN_TIMEOUT: Duration = Duration::from_secs(120);

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PairError {
    /// The user cancelled or did not answer in time.
    Cancelled,
    /// Wrong passkey, the Device refused, or Windows failed.
    Failed(String),
}

impl std::fmt::Display for PairError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PairError::Cancelled => write!(f, "pairing cancelled"),
            PairError::Failed(m) => write!(f, "pairing failed: {m}"),
        }
    }
}

fn device(address: u64) -> windows::core::Result<BluetoothLEDevice> {
    BluetoothLEDevice::FromBluetoothAddressAsync(address)?.join()
}

/// Whether Windows holds a bond with the Device at `address`.
pub fn is_paired(address: u64) -> bool {
    match device(address).and_then(|d| d.DeviceInformation()?.Pairing()?.IsPaired()) {
        Ok(p) => p,
        Err(e) => {
            log::warn!("pairing state of {address:012x}: {e}");
            false
        }
    }
}

/// Pair with the Device, asking `pins` for the passkey it displays.
pub fn pair(address: u64, name: &str, pins: Arc<dyn PinProvider>) -> Result<(), PairError> {
    let fail = |e: windows::core::Error| PairError::Failed(e.message().to_string());
    let dev = device(address).map_err(fail)?;
    let info = dev.DeviceInformation().map_err(fail)?;
    let pairing = info.Pairing().map_err(fail)?;
    if pairing.IsPaired().unwrap_or(false) {
        return Ok(());
    }
    let custom: DeviceInformationCustomPairing = pairing.Custom().map_err(fail)?;
    let cancelled = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let handler = {
        let name = name.to_owned();
        let cancelled = cancelled.clone();
        TypedEventHandler::<DeviceInformationCustomPairing, DevicePairingRequestedEventArgs>::new(
            move |_, args| {
                let args = args.ok()?;
                let kind = args.PairingKind()?;
                log::info!("pairing requested ({:?})", kind.0);
                if kind == DevicePairingKinds::ConfirmOnly {
                    return args.Accept();
                }
                // Hold the pairing open while the user types.
                let deferral = args.GetDeferral()?;
                match pins.request_pin(&name) {
                    Some(pin) => {
                        let pin: String = pin.chars().filter(char::is_ascii_digit).collect();
                        args.AcceptWithPin(&HSTRING::from(pin))?;
                    }
                    None => cancelled.store(true, std::sync::atomic::Ordering::SeqCst),
                }
                deferral.Complete()
            },
        )
    };
    let token = custom.PairingRequested(&handler).map_err(fail)?;
    let kinds = DevicePairingKinds::ProvidePin | DevicePairingKinds::ConfirmOnly;
    let result = custom.PairAsync(kinds).and_then(|op| op.join());
    let _ = custom.RemovePairingRequested(token);
    let result = result.map_err(fail)?;
    let status = result.Status().map_err(fail)?;
    log::info!("pairing {name}: status {}", status.0);
    if status == DevicePairingResultStatus::Paired
        || status == DevicePairingResultStatus::AlreadyPaired
    {
        Ok(())
    } else if cancelled.load(std::sync::atomic::Ordering::SeqCst) {
        Err(PairError::Cancelled)
    } else {
        Err(PairError::Failed(pairing_status_text(status.0)))
    }
}

/// Remove Windows' bond with the Device (it was re-flashed or forgot us).
pub fn unpair(address: u64) -> Result<(), PairError> {
    let fail = |e: windows::core::Error| PairError::Failed(e.message().to_string());
    let dev = device(address).map_err(fail)?;
    let pairing = dev.DeviceInformation().map_err(fail)?.Pairing().map_err(fail)?;
    if !pairing.IsPaired().unwrap_or(false) {
        return Ok(());
    }
    let status = pairing
        .UnpairAsync()
        .and_then(|op| op.join())
        .and_then(|r| r.Status())
        .map_err(fail)?;
    if status == DeviceUnpairingResultStatus::Unpaired
        || status == DeviceUnpairingResultStatus::AlreadyUnpaired
    {
        Ok(())
    } else {
        Err(PairError::Failed(format!("unpair status {}", status.0)))
    }
}

/// `DevicePairingResultStatus` values worth telling apart in the log.
fn pairing_status_text(code: i32) -> String {
    let what = match code {
        1 => "not ready to pair",
        2 => "not paired",
        4 => "connection rejected",
        5 => "too many connections",
        6 => "hardware failure",
        7 => "authentication timeout",
        8 => "authentication not allowed",
        9 => "authentication failure (wrong passkey?)",
        10 => "no supported profiles",
        11 => "protection level could not be met",
        12 => "access denied",
        13 => "invalid ceremony data",
        14 => "pairing canceled",
        15 => "operation already in progress",
        16 => "required handler not registered",
        17 => "rejected by handler",
        18 => "remote device has association",
        19 => "failed",
        _ => "unknown",
    };
    format!("{what} (status {code})")
}
