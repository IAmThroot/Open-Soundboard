//! Core data model for Open Soundboard.
//!
//! These types are intentionally decoupled from `egui` and already `Serialize`/
//! `Deserialize`-ready so that Phase 4 (persistence) can save them as-is.
//! Nothing here touches audio or the OS — Phase 1 is pure state + layout.

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// Stable identity for a slot, independent of its grid position.
/// Unique across *all* boards so ids never collide on switches or merges.
pub type SlotId = u64;

/// Stable identity for a board.
pub type BoardId = u64;

/// One board: a named grid of sound slots. Hotkeys are scoped per board —
/// the same key may mean different sounds on different boards, because only
/// the active board's bindings are ever grabbed OS-wide.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Board {
    pub id: BoardId,
    pub name: String,
    #[serde(default)]
    pub slots: Vec<SoundSlot>,
}

impl Board {
    pub fn empty(id: BoardId, name: impl Into<String>) -> Self {
        Self {
            id,
            name: name.into(),
            slots: Vec::new(),
        }
    }
}

/// A keyboard shortcut the user bound to a slot, e.g. `Ctrl+Shift+K`.
///
/// `key` stores the human-readable key label (see `egui::Key::name()`), e.g.
/// `"F6"`, `"K"`, `"Space"`. Phase 3 will map this label back to a physical
/// key code when registering the global hotkey.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Hotkey {
    pub ctrl: bool,
    pub shift: bool,
    pub alt: bool,
    pub mac_cmd: bool,
    pub key: String,
}

impl Hotkey {
    /// A bare key with no modifiers, e.g. `F6`.
    /// Currently only exercised by tests; kept as the obvious constructor.
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn simple(key: impl Into<String>) -> Self {
        Self {
            ctrl: false,
            shift: false,
            alt: false,
            mac_cmd: false,
            key: key.into(),
        }
    }

    /// Human-friendly rendering used on the slot badge, e.g. `Ctrl+Shift+K`.
    pub fn display(&self) -> String {
        let mut parts: Vec<&str> = Vec::new();
        if self.ctrl {
            parts.push("Ctrl");
        }
        if self.alt {
            parts.push("Alt");
        }
        if self.shift {
            parts.push("Shift");
        }
        // The Windows key. `global-hotkey` calls the flag `mac_cmd` because
        // its API is cross-platform, but this app ships on Windows only, so
        // the badge must say what the user actually pressed.
        if self.mac_cmd {
            parts.push("Win");
        }

        if parts.is_empty() {
            self.key.clone()
        } else {
            format!("{}+{}", parts.join("+"), self.key)
        }
    }

    /// True when this binding would fire while the user is simply typing in
    /// another app (no modifier + a normal character key). We warn before
    /// accepting these.
    pub fn looks_like_typing(&self) -> bool {
        if self.ctrl || self.alt || self.mac_cmd {
            return false;
        }
        !is_safe_standalone_key(&self.key)
    }
}

/// Function keys are safe to use on their own; almost everything else steals
/// normal typing.
fn is_safe_standalone_key(label: &str) -> bool {
    match label.strip_prefix('F') {
        Some(number) => number
            .parse::<u8>()
            .map(|n| (1..=24).contains(&n))
            .unwrap_or(false),
        None => false,
    }
}

/// One sound tile on the board.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SoundSlot {
    pub id: SlotId,
    /// Friendly name shown on the tile (filename without extension).
    pub name: String,
    /// `None` means the tile is empty and waiting for the user to pick a file.
    #[serde(default)]
    pub file_path: Option<PathBuf>,
    #[serde(default)]
    pub hotkey: Option<Hotkey>,
    /// Set when a previously configured file can no longer be found on disk.
    #[serde(default)]
    pub missing: bool,
    /// Trim bounds as fractions of the total sound (0.0–1.0). Playback skips
    /// everything outside `[trim_start, trim_end]`. Old configs lack these and
    /// default to the full sound.
    #[serde(default)]
    pub trim_start: f32,
    #[serde(default = "default_trim_end")]
    pub trim_end: f32,
    /// Exact sound length in seconds, measured during waveform decoding.
    /// Persisted so trimmed playback works on the first press after a restart
    /// without re-decoding. `None` = unknown (plays full).
    #[serde(default)]
    pub duration_secs: Option<f32>,
}

fn default_trim_end() -> f32 {
    1.0
}

impl SoundSlot {
    /// True when the trim actually cuts something off.
    pub fn is_trimmed(&self) -> bool {
        self.trim_start > 0.0 || self.trim_end < 1.0
    }

    /// Forget trim edits (used when the file is re-linked).
    pub fn clear_trim(&mut self) {
        self.trim_start = 0.0;
        self.trim_end = 1.0;
        self.duration_secs = None;
    }
}

impl SoundSlot {
    /// A tile that points at a real file.
    pub fn with_file(id: SlotId, name: impl Into<String>, path: impl Into<PathBuf>) -> Self {
        Self {
            id,
            name: name.into(),
            file_path: Some(path.into()),
            hotkey: None,
            missing: false,
            trim_start: 0.0,
            trim_end: 1.0,
            duration_secs: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn badge_renders_modifiers_in_readable_order() {
        let hotkey = Hotkey {
            ctrl: true,
            shift: true,
            alt: false,
            mac_cmd: false,
            key: "K".to_owned(),
        };
        assert_eq!(hotkey.display(), "Ctrl+Shift+K");
    }

    /// The Windows key is labelled `mac_cmd` by `global-hotkey`'s
    /// cross-platform API, but this app ships on Windows only — the badge
    /// must say what the user actually pressed.
    #[test]
    fn windows_key_badge_says_win_not_cmd() {
        let hotkey = Hotkey {
            ctrl: false,
            shift: false,
            alt: false,
            mac_cmd: true,
            key: "J".to_owned(),
        };
        assert_eq!(hotkey.display(), "Win+J");
        assert!(!hotkey.display().contains("Cmd"));
    }

    #[test]
    fn bare_key_has_no_modifier_prefix() {
        assert_eq!(Hotkey::simple("F6").display(), "F6");
    }

    #[test]
    fn win_key_alone_is_not_treated_as_safe_typing() {
        let hotkey = Hotkey {
            ctrl: false,
            shift: false,
            alt: false,
            mac_cmd: true,
            key: "J".to_owned(),
        };
        assert!(!hotkey.looks_like_typing());
    }
}
