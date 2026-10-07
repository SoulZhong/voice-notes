//! AI Passport (Vibe Voice) device support, moved into Voice Notes from
//! vibe-voice-input `host/vibe-voice` (see `docs/adr/0001-…`).
//!
//! Pure, unit-tested modules (`protocol`, `adpcm`, `audio`, `config`, `orca`,
//! `alerts`, `session`) are kept apart from the platform glue (`ble`,
//! `inject_*`, `speech_apple`, `pair_windows`). The host (Voice Notes) plugs in
//! the recognizer, the recording control and the dictation notes through the
//! traits in `session` and `notes`, and drives everything with [`runtime`].
//!
//! The wire protocol is defined by vibe-voice-input `docs/vibe-voice/protocol.md`;
//! that document is the single source of truth, this crate follows it.

pub mod adpcm;
pub mod alerts;
pub mod audio;
pub mod ble;
pub mod config;
pub mod notes;
pub mod orca;
pub mod protocol;
pub mod runtime;
pub mod session;
pub mod sim;

#[cfg(target_os = "macos")]
pub mod inject_macos;
#[cfg(target_os = "macos")]
pub mod speech_apple;

#[cfg(windows)]
pub mod inject_windows;
#[cfg(windows)]
pub mod pair_windows;

/// Platform glue the runtime needs without knowing the platform.
pub mod platform {
    #[cfg(target_os = "macos")]
    pub use crate::inject_macos::{MacInjector as SystemInjector, app_running, display_asleep};

    #[cfg(windows)]
    pub use crate::inject_windows::{WinInjector as SystemInjector, app_running, display_asleep};

    #[cfg(not(any(target_os = "macos", windows)))]
    pub use self::unsupported::{SystemInjector, app_running, display_asleep};

    #[cfg(not(any(target_os = "macos", windows)))]
    mod unsupported {
        use crate::session::{InjectError, Injector};

        pub fn app_running(_: &str) -> bool {
            false
        }

        pub fn display_asleep() -> bool {
            false
        }

        /// Text insertion is not implemented on this platform.
        #[derive(Default)]
        pub struct SystemInjector;

        impl Injector for SystemInjector {
            fn accessibility_trusted(&mut self) -> bool {
                false
            }
            fn is_running(&mut self, _: &str) -> bool {
                false
            }
            fn frontmost_bundle_id(&mut self) -> Option<String> {
                None
            }
            fn window_title(&mut self, _: &str) -> Option<String> {
                None
            }
            fn activate(&mut self, _: &str) -> Result<(), InjectError> {
                Err(InjectError::NotRunning)
            }
            fn insert(&mut self, _: &str, _: &str) -> Result<(), InjectError> {
                Err(InjectError::NotRunning)
            }
            fn submit(&mut self, _: &str) -> Result<(), InjectError> {
                Err(InjectError::NotRunning)
            }
            fn delete_back(&mut self, _: &str, _: usize) -> Result<(), InjectError> {
                Err(InjectError::NotRunning)
            }
        }
    }
}
