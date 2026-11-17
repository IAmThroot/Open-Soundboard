use serde::{Deserialize, Serialize};
use std::path::PathBuf;

pub type SlotId = u64;

pub type BoardId = u64;

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

#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Hotkey {
    pub ctrl: bool,
    pub shift: bool,
    pub alt: bool,
    pub mac_cmd: bool,
    pub key: String,
}

impl Hotkey {
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

        if self.mac_cmd {
            parts.push("Win");
        }

        if parts.is_empty() {
            self.key.clone()
        } else {
            format!("{}+{}", parts.join("+"), self.key)
        }
    }

    pub fn looks_like_typing(&self) -> bool {
        if self.ctrl || self.alt || self.mac_cmd {
            return false;
        }
        !is_safe_standalone_key(&self.key)
    }
}

fn is_safe_standalone_key(label: &str) -> bool {
    match label {
        label if label.starts_with('F') => label[1..]
            .parse::<u8>()
            .is_ok_and(|n| (1..=24).contains(&n)),

        "ArrowUp" | "ArrowDown" | "ArrowLeft" | "ArrowRight" | "Home" | "End" | "PageUp"
        | "PageDown" | "Insert" | "Delete" | "Escape" | "NumLock" | "CapsLock" | "ScrollLock"
        | "Pause" | "PrintScreen" | "MediaPlayPause" | "MediaStop" | "MediaTrackNext"
        | "MediaTrackPrevious" | "AudioVolumeDown" | "AudioVolumeMute" | "AudioVolumeUp"
        | "BrowserBack" | "BrowserForward" | "BrowserRefresh" | "BrowserHome" | "BrowserSearch"
        | "BrowserStop" | "LaunchApp1" | "LaunchApp2" | "LaunchMail" | "ContextMenu" | "Help" => {
            true
        }
        _ => false,
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SoundSlot {
    pub id: SlotId,

    pub name: String,

    #[serde(default = "default_trim_end")]
    pub volume: f32,

    #[serde(default)]
    pub file_path: Option<PathBuf>,
    #[serde(default)]
    pub hotkey: Option<Hotkey>,

    #[serde(default)]
    pub missing: bool,

    #[serde(default)]
    pub trim_start: f32,
    #[serde(default = "default_trim_end")]
    pub trim_end: f32,

    #[serde(default)]
    pub duration_secs: Option<f32>,
}

fn default_trim_end() -> f32 {
    1.0
}

impl SoundSlot {
    pub fn is_trimmed(&self) -> bool {
        self.trim_start > 0.0 || self.trim_end < 1.0
    }

    pub fn clear_trim(&mut self) {
        self.trim_start = 0.0;
        self.trim_end = 1.0;
        self.duration_secs = None;
    }

    pub fn with_file(id: SlotId, name: impl Into<String>, path: impl Into<PathBuf>) -> Self {
        Self {
            id,
            name: name.into(),
            volume: 1.0,
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
    fn legacy_sound_defaults_to_full_volume() {
        let slot: SoundSlot = serde_json::from_str(r#"{"id":1,"name":"Old sound"}"#).unwrap();
        assert_eq!(slot.volume, 1.0);
        let mut slot = slot;
        slot.volume = 0.35;
        let back: SoundSlot = serde_json::from_str(&serde_json::to_string(&slot).unwrap()).unwrap();
        assert_eq!(back.volume, 0.35);
    }

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

    #[test]
    fn plain_letters_and_digits_look_like_typing() {
        assert!(Hotkey::simple("K").looks_like_typing());
        assert!(Hotkey::simple("7").looks_like_typing());
        assert!(Hotkey::simple("Space").looks_like_typing());
    }

    #[test]
    fn non_text_keys_do_not_look_like_typing() {
        for key in [
            "F1",
            "F6",
            "F24",
            "ArrowUp",
            "ArrowDown",
            "ArrowLeft",
            "ArrowRight",
            "Home",
            "End",
            "PageUp",
            "PageDown",
            "Insert",
            "Delete",
            "Escape",
            "NumLock",
            "CapsLock",
            "ScrollLock",
            "Pause",
            "PrintScreen",
            "MediaPlayPause",
            "MediaStop",
            "MediaTrackNext",
            "MediaTrackPrevious",
            "AudioVolumeDown",
            "AudioVolumeMute",
            "AudioVolumeUp",
            "BrowserBack",
            "BrowserRefresh",
            "LaunchApp1",
            "LaunchMail",
            "ContextMenu",
            "Help",
        ] {
            assert!(
                !Hotkey::simple(key).looks_like_typing(),
                "{key} should be safe as a standalone hotkey",
            );
        }
    }

    #[test]
    fn modifiers_override_typing_risk() {
        assert!(Hotkey::simple("k").looks_like_typing());
        let with_ctrl = Hotkey {
            ctrl: true,
            ..Hotkey::simple("k")
        };
        assert!(!with_ctrl.looks_like_typing());
    }
}
