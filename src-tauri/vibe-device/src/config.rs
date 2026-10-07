//! Built-in Supported Apps and the stored Target (`target.json`).
//!
//! An app is identified by a platform id: the bundle id on macOS, the
//! lower-case executable file name on Windows. The Device only knows the app
//! index (its logo), so the order of [`SUPPORTED_APPS`] is part of the
//! protocol and identical on every platform.

use serde::{Deserialize, Serialize};
use std::path::Path;

/// One app Vibe Voice can deliver to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SupportedApp {
    /// Name shown on the Device.
    pub name: &'static str,
    /// Canonical platform id (stored in the Target).
    pub bundle_id: &'static str,
    /// Other ids of the same app (renamed executables across versions).
    pub aliases: &'static [&'static str],
}

impl SupportedApp {
    /// Whether `id` names this app.
    pub fn matches(&self, id: &str) -> bool {
        same_id(self.bundle_id, id) || self.aliases.iter().any(|a| same_id(a, id))
    }

    /// The canonical id and the aliases.
    pub fn ids(&self) -> impl Iterator<Item = &'static str> + '_ {
        std::iter::once(self.bundle_id).chain(self.aliases.iter().copied())
    }
}

/// Bundle ids are case-sensitive; Windows file names are not.
fn same_id(a: &str, b: &str) -> bool {
    if cfg!(windows) {
        a.eq_ignore_ascii_case(b)
    } else {
        a == b
    }
}

/// Index of Orca in [`SUPPORTED_APPS`].
pub const ORCA: usize = 0;

#[cfg(not(windows))]
pub const ORCA_BUNDLE_ID: &str = "com.stablyai.orca";
#[cfg(windows)]
pub const ORCA_BUNDLE_ID: &str = "orca.exe";

/// The Supported Apps in Device order. Root row `i` opens list `i + 1`.
#[cfg(not(windows))]
pub const SUPPORTED_APPS: [SupportedApp; 4] = [
    SupportedApp {
        name: "Orca",
        bundle_id: ORCA_BUNDLE_ID,
        aliases: &[],
    },
    SupportedApp {
        name: "微信",
        bundle_id: "com.tencent.xinWeChat",
        aliases: &[],
    },
    // /Applications/ChatGPT.app (Codex); its bundle id is com.openai.codex.
    SupportedApp {
        name: "ChatGPT",
        bundle_id: "com.openai.codex",
        aliases: &[],
    },
    SupportedApp {
        name: "企业微信",
        bundle_id: "com.tencent.WeWorkMac",
        aliases: &[],
    },
];

/// Windows: executable names. WeChat 4.x is `Weixin.exe`, 3.x `WeChat.exe`;
/// the Codex desktop app became `ChatGPT.exe` in July 2026.
#[cfg(windows)]
pub const SUPPORTED_APPS: [SupportedApp; 4] = [
    SupportedApp {
        name: "Orca",
        bundle_id: ORCA_BUNDLE_ID,
        aliases: &[],
    },
    SupportedApp {
        name: "微信",
        bundle_id: "weixin.exe",
        aliases: &["wechat.exe"],
    },
    SupportedApp {
        name: "ChatGPT",
        bundle_id: "chatgpt.exe",
        aliases: &["codex.exe"],
    },
    SupportedApp {
        name: "企业微信",
        bundle_id: "wxwork.exe",
        aliases: &[],
    },
];

/// Index of the Supported App with `bundle_id` (or one of its aliases).
pub fn supported_app(bundle_id: &str) -> Option<usize> {
    SUPPORTED_APPS.iter().position(|a| a.matches(bundle_id))
}

/// All ids of the Supported App `bundle_id` belongs to (just `bundle_id`
/// for an unknown app).
pub fn app_ids(bundle_id: &str) -> Vec<String> {
    match supported_app(bundle_id) {
        Some(i) => SUPPORTED_APPS[i].ids().map(str::to_owned).collect(),
        None => vec![bundle_id.to_owned()],
    }
}

/// The Target, persisted so it survives restarts.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StoredTarget {
    /// A Supported App other than Orca: whatever chat it shows at Insert time.
    App { bundle_id: String },
    /// One Orca Session. `leaf_id` finds it again after Orca re-issued
    /// handles; `worktree` and `title` label it while Orca is not running.
    Orca {
        handle: String,
        #[serde(default)]
        leaf_id: String,
        #[serde(default)]
        worktree: String,
        #[serde(default)]
        title: String,
    },
}

impl StoredTarget {
    /// `None` for a missing or unreadable file, or an app no longer supported.
    pub fn load(path: &Path) -> Option<Self> {
        let text = std::fs::read_to_string(path).ok()?;
        let t: Self = match serde_json::from_str(&text) {
            Ok(t) => t,
            Err(e) => {
                log::warn!("ignoring {}: {e}", path.display());
                return None;
            }
        };
        match &t {
            StoredTarget::App { bundle_id }
                if supported_app(bundle_id).is_none_or(|i| i == ORCA) =>
            {
                None
            }
            _ => Some(t),
        }
    }

    pub fn save(&self, path: &Path) {
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let tmp = path.with_extension("json.tmp");
        let body = serde_json::to_string_pretty(self).unwrap_or_default();
        if std::fs::write(&tmp, body + "\n")
            .and_then(|_| std::fs::rename(&tmp, path))
            .is_err()
        {
            log::warn!("cannot save {}", path.display());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn supported_apps_in_decided_order() {
        let names: Vec<_> = SUPPORTED_APPS.iter().map(|a| a.name).collect();
        assert_eq!(names, ["Orca", "微信", "ChatGPT", "企业微信"]);
        assert_eq!(SUPPORTED_APPS[ORCA].bundle_id, ORCA_BUNDLE_ID);
        for (i, app) in SUPPORTED_APPS.iter().enumerate() {
            for id in app.ids() {
                assert_eq!(supported_app(id), Some(i));
            }
        }
        assert_eq!(supported_app("com.mitchellh.ghostty"), None);
        assert_eq!(supported_app("ghostty.exe"), None);
    }

    #[cfg(not(windows))]
    #[test]
    fn macos_ids() {
        assert_eq!(supported_app("com.tencent.xinWeChat"), Some(1));
        assert_eq!(supported_app("com.openai.codex"), Some(2));
        assert_eq!(supported_app("com.tencent.WeWorkMac"), Some(3));
        // Bundle ids are case-sensitive.
        assert_eq!(supported_app("com.openai.CODEX"), None);
    }

    #[cfg(windows)]
    #[test]
    fn windows_ids_ignore_case_and_aliases() {
        assert_eq!(supported_app("WeChat.exe"), Some(1));
        assert_eq!(supported_app("Weixin.exe"), Some(1));
        assert_eq!(supported_app("ChatGPT.exe"), Some(2));
        assert_eq!(app_ids("WECHAT.EXE"), vec!["weixin.exe", "wechat.exe"]);
    }

    #[test]
    fn stored_target_roundtrip() {
        let dir = std::env::temp_dir().join(format!("vv-target-{}", std::process::id()));
        let path = dir.join("target.json");
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(StoredTarget::load(&path), None);
        let orca = StoredTarget::Orca {
            handle: "term_1".into(),
            leaf_id: "leaf".into(),
            worktree: "my-passport".into(),
            title: "语音输入".into(),
        };
        orca.save(&path);
        assert_eq!(StoredTarget::load(&path), Some(orca));
        let app = StoredTarget::App {
            bundle_id: SUPPORTED_APPS[1].bundle_id.into(),
        };
        app.save(&path);
        assert_eq!(StoredTarget::load(&path), Some(app));
        // Unsupported apps and garbage are ignored.
        std::fs::write(&path, r#"{"app":{"bundle_id":"com.mitchellh.ghostty"}}"#).unwrap();
        assert_eq!(StoredTarget::load(&path), None);
        std::fs::write(&path, "not json").unwrap();
        assert_eq!(StoredTarget::load(&path), None);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
