use std::collections::HashMap;
use std::path::PathBuf;
use std::time::Duration;

use crossbeam_channel::{Receiver, Sender};
use global_hotkey::{
    hotkey::{Code, HotKey as GlobalKey, Modifiers},
    GlobalHotKeyEvent, GlobalHotKeyManager, HotKeyState,
};
use log::{debug, info, warn};

use crate::audio::{AudioCommand, TrimSpec};
use crate::model::{Hotkey, SlotId};

pub const STOP_ALL_ID: SlotId = SlotId::MAX - 1;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BindingAction {
    Play,

    StopAll,
}

#[derive(Clone, Debug)]
pub struct HotkeyBinding {
    pub id: SlotId,
    pub hotkey: Hotkey,
    pub path: Option<PathBuf>,
    pub action: BindingAction,
    pub trim: Option<TrimSpec>,
}

impl HotkeyBinding {
    pub fn play(id: SlotId, hotkey: Hotkey, path: PathBuf, trim: Option<TrimSpec>) -> Self {
        Self {
            id,
            hotkey,
            path: Some(path),
            action: BindingAction::Play,
            trim,
        }
    }

    pub fn stop(hotkey: Hotkey) -> Self {
        Self {
            id: STOP_ALL_ID,
            hotkey,
            path: None,
            action: BindingAction::StopAll,
            trim: None,
        }
    }
}

#[derive(Clone, Debug)]
pub struct PressTarget {
    pub id: SlotId,
    pub path: Option<PathBuf>,
    pub action: BindingAction,
    pub trim: Option<TrimSpec>,
}

#[derive(Debug)]
pub enum HotkeyCommand {
    Rebuild(Vec<HotkeyBinding>),
}

#[derive(Debug)]
pub enum HotkeyEvent {
    Ready,

    Unavailable(String),

    KeyRejected { display: String, reason: String },
}

pub fn spawn_hotkey_thread(
    audio_tx: Sender<AudioCommand>,
) -> (Sender<HotkeyCommand>, Receiver<HotkeyEvent>) {
    let (cmd_tx, cmd_rx) = crossbeam_channel::unbounded::<HotkeyCommand>();
    let (event_tx, event_rx) = crossbeam_channel::unbounded::<HotkeyEvent>();

    std::thread::Builder::new()
        .name("hotkey-listener".to_owned())
        .spawn(move || run_listener(cmd_rx, event_tx, audio_tx))
        .expect("failed to spawn hotkey listener thread");

    (cmd_tx, event_rx)
}

pub fn to_global_hotkeys(binding: &Hotkey) -> Result<Vec<GlobalKey>, String> {
    let mut mods = Modifiers::empty();
    if binding.ctrl {
        mods.insert(Modifiers::CONTROL);
    }
    if binding.shift {
        mods.insert(Modifiers::SHIFT);
    }
    if binding.alt {
        mods.insert(Modifiers::ALT);
    }
    if binding.mac_cmd {
        mods.insert(Modifiers::SUPER);
    }
    let mods = if mods.is_empty() { None } else { Some(mods) };

    let mut keys = Vec::new();
    if let Some(code) = code_for_label(&binding.key) {
        keys.push(GlobalKey::new(mods, code));
    }

    if let Some(twin) = twin_code_for_label(&binding.key) {
        let twin_key = GlobalKey::new(mods, twin);
        if !keys.contains(&twin_key) {
            keys.push(twin_key);
        }
    }

    if keys.is_empty() {
        Err(unsuitable_key_reason(&binding.key))
    } else {
        Ok(keys)
    }
}

fn twin_code_for_label(label: &str) -> Option<Code> {
    if label.len() == 1 && label.chars().next().is_some_and(|ch| ch.is_ascii_digit()) {
        return format!("Numpad{label}").parse::<Code>().ok();
    }
    match label {
        "Plus" => Some(Code::NumpadAdd),
        "Minus" => Some(Code::NumpadSubtract),
        "Slash" => Some(Code::NumpadDivide),
        "Period" => Some(Code::NumpadDecimal),
        "Comma" => Some(Code::NumpadComma),
        "Enter" => Some(Code::NumpadEnter),
        "Equals" => Some(Code::NumpadEqual),
        _ => None,
    }
}

fn code_for_label(label: &str) -> Option<Code> {
    if label.len() == 1 {
        let ch = label.chars().next().expect("len checked");
        if ch.is_ascii_digit() {
            return format!("Digit{ch}").parse::<Code>().ok();
        }
        if ch.is_ascii_uppercase() {
            return format!("Key{ch}").parse::<Code>().ok();
        }
        return None;
    }

    let canonical = match label {
        "Down" => "ArrowDown",
        "Left" => "ArrowLeft",
        "Right" => "ArrowRight",
        "Up" => "ArrowUp",
        "Backtick" => "Backquote",
        "OpenBracket" => "BracketLeft",
        "CloseBracket" => "BracketRight",
        "Menu" => "ContextMenu",

        "Equals" => "Equal",

        "Colon" | "Plus" | "Pipe" | "Questionmark" | "Exclamationmark" | "OpenCurlyBracket"
        | "CloseCurlyBracket" | "ShiftLeft" | "ShiftRight" | "ControlLeft" | "ControlRight"
        | "AltLeft" | "AltRight" | "SuperLeft" | "SuperRight" | "Unidentified" => return None,
        other => other,
    };

    canonical
        .parse::<Code>()
        .ok()
        .filter(|code| *code != Code::Unidentified)
}

fn unsuitable_key_reason(label: &str) -> String {
    match label {
        "Colon" | "Plus" | "Pipe" | "Questionmark" | "Exclamationmark" | "OpenCurlyBracket"
        | "CloseCurlyBracket" => format!(
            "“{label}” needs Shift on most keyboards, so it can't be grabbed on its own. \
             Bind its base key with Shift instead."
        ),
        "ShiftLeft" | "ShiftRight" | "ControlLeft" | "ControlRight" | "AltLeft" | "AltRight"
        | "SuperLeft" | "SuperRight" => {
            "A modifier key on its own can't be a hotkey. Press it together with another key."
                .to_owned()
        }
        _ => format!(
            "“{label}” can't be grabbed as a system-wide hotkey on this computer. \
             Try a function key (F1–F12) or add Ctrl/Alt."
        ),
    }
}

fn run_listener(
    cmd_rx: Receiver<HotkeyCommand>,
    event_tx: Sender<HotkeyEvent>,
    audio_tx: Sender<AudioCommand>,
) {
    let manager = match GlobalHotKeyManager::new() {
        Ok(manager) => manager,
        Err(err) => {
            warn!("Global hotkey backend unavailable: {err}");
            let _ = event_tx.send(HotkeyEvent::Unavailable(format!(
                "System-wide hotkeys aren't available here ({err}). You can still click ▶ to play sounds."
            )));

            while cmd_rx.recv().is_ok() {}
            return;
        }
    };

    let _ = event_tx.send(HotkeyEvent::Ready);
    info!("Global hotkey backend ready.");

    let mut grabbed: Vec<GlobalKey> = Vec::new();
    let mut routing: HashMap<u32, Vec<PressTarget>> = HashMap::new();

    loop {
        pump_win32_messages();

        while let Ok(event) = GlobalHotKeyEvent::receiver().try_recv() {
            if event.state() != HotKeyState::Pressed {
                continue;
            }
            let sent = dispatch_press(&routing, event.id(), &audio_tx);
            if sent > 0 {
                debug!(
                    "Global hotkey id={} pressed; dispatched Play to {sent} slot(s).",
                    event.id()
                );
            }
        }

        match cmd_rx.recv_timeout(PUMP_INTERVAL) {
            Ok(HotkeyCommand::Rebuild(bindings)) => {
                rebuild(&manager, &event_tx, &mut grabbed, &mut routing, bindings);
            }
            Err(crossbeam_channel::RecvTimeoutError::Timeout) => {}
            Err(crossbeam_channel::RecvTimeoutError::Disconnected) => {
                info!("UI hung up; hotkey listener shutting down.");
                break;
            }
        }
    }
}

const PUMP_INTERVAL: Duration = Duration::from_millis(20);

#[cfg(windows)]
fn pump_win32_messages() {
    use windows_sys::Win32::Foundation::HWND;
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        DispatchMessageW, PeekMessageW, TranslateMessage, MSG, PM_REMOVE,
    };
    unsafe {
        let mut msg: MSG = std::mem::zeroed();

        while PeekMessageW(&mut msg, 0 as HWND, 0, 0, PM_REMOVE) != 0 {
            TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
}

#[cfg(not(windows))]
fn pump_win32_messages() {}

fn rebuild(
    manager: &GlobalHotKeyManager,
    event_tx: &Sender<HotkeyEvent>,
    grabbed: &mut Vec<GlobalKey>,
    routing: &mut HashMap<u32, Vec<PressTarget>>,
    bindings: Vec<HotkeyBinding>,
) {
    if !grabbed.is_empty() {
        let _ = manager.unregister_all(grabbed);
        grabbed.clear();
    }
    routing.clear();

    let (by_key, rejected) = group_bindings(bindings);
    for (display, reason) in rejected {
        let _ = event_tx.send(HotkeyEvent::KeyRejected { display, reason });
    }

    for (key, slots) in by_key {
        match manager.register(key) {
            Ok(()) => {
                routing.insert(key.id(), slots);
                grabbed.push(key);
            }
            Err(err) => {
                let _ = event_tx.send(HotkeyEvent::KeyRejected {
                    display: key_to_display(&key),
                    reason: format!(
                        "Another program is already using it, or Windows refused it ({err}). \
                         Pick a different key."
                    ),
                });
            }
        }
    }

    info!("Hotkey rebuild complete: {} OS grabs.", grabbed.len());
}

type GroupedBindings = (HashMap<GlobalKey, Vec<PressTarget>>, Vec<(String, String)>);

fn group_bindings(bindings: Vec<HotkeyBinding>) -> GroupedBindings {
    let mut by_key: HashMap<GlobalKey, Vec<PressTarget>> = HashMap::new();
    let mut rejected = Vec::new();
    for binding in bindings {
        match to_global_hotkeys(&binding.hotkey) {
            Ok(keys) => {
                for key in keys {
                    by_key.entry(key).or_default().push(PressTarget {
                        id: binding.id,
                        path: binding.path.clone(),
                        action: binding.action,
                        trim: binding.trim,
                    });
                }
            }
            Err(reason) => rejected.push((binding.hotkey.display(), reason)),
        }
    }
    (by_key, rejected)
}

fn dispatch_press(
    routing: &HashMap<u32, Vec<PressTarget>>,
    id: u32,
    audio_tx: &Sender<AudioCommand>,
) -> usize {
    let Some(targets) = routing.get(&id) else {
        return 0;
    };
    let mut sent = 0;
    for target in targets {
        let cmd = match target.action {
            BindingAction::Play => match &target.path {
                Some(path) => AudioCommand::Play {
                    id: target.id,
                    path: path.clone(),
                    trim: target.trim,
                },

                None => {
                    warn!("Hotkey target {:?} has no file; skipping.", target.id);
                    continue;
                }
            },
            BindingAction::StopAll => AudioCommand::StopAll,
        };
        if audio_tx.send(cmd).is_ok() {
            sent += 1;
        }
    }
    sent
}

fn key_to_display(key: &GlobalKey) -> String {
    let mut parts = Vec::new();
    if key.mods.contains(Modifiers::CONTROL) {
        parts.push("Ctrl");
    }
    if key.mods.contains(Modifiers::ALT) {
        parts.push("Alt");
    }
    if key.mods.contains(Modifiers::SHIFT) {
        parts.push("Shift");
    }
    if key.mods.contains(Modifiers::SUPER) {
        parts.push("Win");
    }
    parts.push(code_label(&key.key));
    parts.join("+")
}

fn code_label(code: &Code) -> &'static str {
    match code {
        Code::Space => "Space",
        Code::ArrowDown => "Down",
        Code::ArrowLeft => "Left",
        Code::ArrowRight => "Right",
        Code::ArrowUp => "Up",
        _ => code_name(*code),
    }
}

fn code_name(code: Code) -> &'static str {
    match code {
        Code::Backquote => "Backquote",
        Code::Backslash => "Backslash",
        Code::BracketLeft => "BracketLeft",
        Code::BracketRight => "BracketRight",
        Code::Comma => "Comma",
        Code::Digit0 => "Digit0",
        Code::Digit1 => "Digit1",
        Code::Digit2 => "Digit2",
        Code::Digit3 => "Digit3",
        Code::Digit4 => "Digit4",
        Code::Digit5 => "Digit5",
        Code::Digit6 => "Digit6",
        Code::Digit7 => "Digit7",
        Code::Digit8 => "Digit8",
        Code::Digit9 => "Digit9",
        Code::Equal => "Equal",
        Code::Minus => "Minus",
        Code::Period => "Period",
        Code::Quote => "Quote",
        Code::Semicolon => "Semicolon",
        Code::Slash => "Slash",
        Code::Backspace => "Backspace",
        Code::CapsLock => "CapsLock",
        Code::ContextMenu => "ContextMenu",
        Code::Enter => "Enter",
        Code::Space => "Space",
        Code::Tab => "Tab",
        Code::Delete => "Delete",
        Code::End => "End",
        Code::Help => "Help",
        Code::Home => "Home",
        Code::Insert => "Insert",
        Code::PageDown => "PageDown",
        Code::PageUp => "PageUp",
        Code::ArrowDown => "ArrowDown",
        Code::ArrowLeft => "ArrowLeft",
        Code::ArrowRight => "ArrowRight",
        Code::ArrowUp => "ArrowUp",
        Code::NumLock => "NumLock",
        Code::Escape => "Escape",
        Code::PrintScreen => "PrintScreen",
        Code::ScrollLock => "ScrollLock",
        Code::Pause => "Pause",
        Code::BrowserBack => "BrowserBack",
        Code::BrowserFavorites => "BrowserFavorites",
        Code::BrowserForward => "BrowserForward",
        Code::BrowserHome => "BrowserHome",
        Code::BrowserRefresh => "BrowserRefresh",
        Code::BrowserSearch => "BrowserSearch",
        Code::BrowserStop => "BrowserStop",
        Code::LaunchApp1 => "LaunchApp1",
        Code::LaunchApp2 => "LaunchApp2",
        Code::LaunchMail => "LaunchMail",
        Code::MediaPlayPause => "MediaPlayPause",
        Code::MediaStop => "MediaStop",
        Code::MediaTrackNext => "MediaTrackNext",
        Code::MediaTrackPrevious => "MediaTrackPrevious",
        Code::AudioVolumeDown => "AudioVolumeDown",
        Code::AudioVolumeMute => "AudioVolumeMute",
        Code::AudioVolumeUp => "AudioVolumeUp",
        Code::KeyA => "KeyA",
        Code::KeyB => "KeyB",
        Code::KeyC => "KeyC",
        Code::KeyD => "KeyD",
        Code::KeyE => "KeyE",
        Code::KeyF => "KeyF",
        Code::KeyG => "KeyG",
        Code::KeyH => "KeyH",
        Code::KeyI => "KeyI",
        Code::KeyJ => "KeyJ",
        Code::KeyK => "KeyK",
        Code::KeyL => "KeyL",
        Code::KeyM => "KeyM",
        Code::KeyN => "KeyN",
        Code::KeyO => "KeyO",
        Code::KeyP => "KeyP",
        Code::KeyQ => "KeyQ",
        Code::KeyR => "KeyR",
        Code::KeyS => "KeyS",
        Code::KeyT => "KeyT",
        Code::KeyU => "KeyU",
        Code::KeyV => "KeyV",
        Code::KeyW => "KeyW",
        Code::KeyX => "KeyX",
        Code::KeyY => "KeyY",
        Code::KeyZ => "KeyZ",
        Code::F1 => "F1",
        Code::F2 => "F2",
        Code::F3 => "F3",
        Code::F4 => "F4",
        Code::F5 => "F5",
        Code::F6 => "F6",
        Code::F7 => "F7",
        Code::F8 => "F8",
        Code::F9 => "F9",
        Code::F10 => "F10",
        Code::F11 => "F11",
        Code::F12 => "F12",
        Code::F13 => "F13",
        Code::F14 => "F14",
        Code::F15 => "F15",
        Code::F16 => "F16",
        Code::F17 => "F17",
        Code::F18 => "F18",
        Code::F19 => "F19",
        Code::F20 => "F20",
        Code::F21 => "F21",
        Code::F22 => "F22",
        Code::F23 => "F23",
        Code::F24 => "F24",
        Code::Numpad0 => "Numpad0",
        Code::Numpad1 => "Numpad1",
        Code::Numpad2 => "Numpad2",
        Code::Numpad3 => "Numpad3",
        Code::Numpad4 => "Numpad4",
        Code::Numpad5 => "Numpad5",
        Code::Numpad6 => "Numpad6",
        Code::Numpad7 => "Numpad7",
        Code::Numpad8 => "Numpad8",
        Code::Numpad9 => "Numpad9",
        Code::NumpadMultiply => "NumpadMultiply",
        _ => "key",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Hotkey;

    #[test]
    fn rejected_grab_names_the_actual_key() {
        assert_eq!(code_label(&Code::KeyQ), "KeyQ");
        assert_eq!(code_label(&Code::F6), "F6");
        assert_eq!(code_label(&Code::Numpad7), "Numpad7");
        assert_ne!(code_label(&Code::F6), "key");
    }

    #[test]
    fn every_grabbable_egui_key_maps_to_a_code() {
        let refused: &[&str] = &[
            "Colon",
            "Plus",
            "Pipe",
            "Questionmark",
            "Exclamationmark",
            "OpenCurlyBracket",
            "CloseCurlyBracket",
            "ShiftLeft",
            "ShiftRight",
            "ControlLeft",
            "ControlRight",
            "AltLeft",
            "AltRight",
            "SuperLeft",
            "SuperRight",
            "Unidentified",
            "Clear",
        ];

        for key in egui::Key::ALL {
            let label = key.name();
            let mapped = code_for_label(label).is_some();
            let should_refuse = refused.contains(&label);
            assert_eq!(
                mapped, !should_refuse,
                "egui key {label:?} mapping disagrees with the refused list \
                 (mapped={mapped}, expected refusal={should_refuse})",
            );
        }
    }

    #[test]
    fn top_row_equals_grabs() {
        let binding = Hotkey::simple("Equals");
        let keys = to_global_hotkeys(&binding).expect("= must be grabbable");
        assert!(
            keys.iter().any(|k| k.key == Code::Equal),
            "expected the physical Equal key, got {keys:?}",
        );
    }

    fn simple(key: &str) -> Hotkey {
        Hotkey::simple(key)
    }

    #[test]
    fn function_keys_map() {
        for label in ["F1", "F6", "F12", "F24"] {
            let keys = to_global_hotkeys(&simple(label)).expect("F-keys must map");
            assert_eq!(keys.len(), 1);
            assert_eq!(keys[0].mods, Modifiers::empty());
        }
    }

    #[test]
    fn letters_digits_and_space_map() {
        assert_eq!(to_global_hotkeys(&simple("K")).unwrap()[0].key, Code::KeyK);
        assert_eq!(
            to_global_hotkeys(&simple("1")).unwrap()[0].key,
            Code::Digit1
        );
        assert_eq!(
            to_global_hotkeys(&simple("Space")).unwrap()[0].key,
            Code::Space
        );
    }

    #[test]
    fn arrows_and_brackets_translate() {
        assert_eq!(
            to_global_hotkeys(&simple("Down")).unwrap()[0].key,
            Code::ArrowDown
        );
        assert_eq!(
            to_global_hotkeys(&simple("OpenBracket")).unwrap()[0].key,
            Code::BracketLeft
        );
    }

    #[test]
    fn modifiers_compose() {
        let combo = Hotkey {
            ctrl: true,
            shift: true,
            ..simple("K")
        };
        let key = to_global_hotkeys(&combo).unwrap()[0];
        assert!(key.mods.contains(Modifiers::CONTROL));
        assert!(key.mods.contains(Modifiers::SHIFT));
        assert!(!key.mods.contains(Modifiers::ALT));
    }

    #[test]
    fn win_key_maps_to_super() {
        let combo = Hotkey {
            mac_cmd: true,
            ..simple("K")
        };
        let key = to_global_hotkeys(&combo).unwrap()[0];
        assert!(key.mods.contains(Modifiers::SUPER));
    }

    #[test]
    fn shifted_punctuation_without_twins_is_rejected_with_guidance() {
        for label in ["Colon", "Pipe", "Questionmark", "OpenCurlyBracket"] {
            let err = to_global_hotkeys(&simple(label)).expect_err("must reject");
            assert!(err.contains("Shift"), "unexpected message: {err}");
        }
    }

    #[test]
    fn doublers_with_numpad_twins_are_accepted() {
        for (label, twin) in [
            ("Plus", Code::NumpadAdd),
            ("Minus", Code::NumpadSubtract),
            ("Slash", Code::NumpadDivide),
            ("Equals", Code::NumpadEqual),
        ] {
            let keys = to_global_hotkeys(&simple(label)).unwrap();
            assert!(
                keys.iter().any(|k| k.key == twin),
                "{label} should grab {twin:?}"
            );
        }
    }

    #[test]
    fn bare_modifiers_are_rejected() {
        for label in ["ShiftLeft", "ControlRight", "SuperLeft"] {
            assert!(
                to_global_hotkeys(&simple(label)).is_err(),
                "{label} must not map"
            );
        }
    }

    #[test]
    fn digits_grab_both_top_row_and_numpad() {
        let keys = to_global_hotkeys(&simple("7")).unwrap();
        assert_eq!(keys.len(), 2);
        let codes: Vec<Code> = keys.iter().map(|k| k.key).collect();
        assert!(codes.contains(&Code::Digit7));
        assert!(codes.contains(&Code::Numpad7));
    }

    #[test]
    fn plus_grabs_numpad_add_only() {
        let keys = to_global_hotkeys(&simple("Plus")).unwrap();
        assert_eq!(keys.len(), 1);
        assert_eq!(keys[0].key, Code::NumpadAdd);
    }

    #[test]
    fn twins_share_routing_with_the_primary() {
        let bindings = vec![binding(1, "3", "/a.mp3")];
        let (groups, rejected) = group_bindings(bindings);
        assert!(rejected.is_empty());
        assert_eq!(groups.len(), 2);
        for targets in groups.values() {
            assert_eq!(targets.len(), 1);
            assert_eq!(targets[0].id, 1);
        }
    }

    #[test]
    fn identical_bindings_share_one_os_key() {
        let a = to_global_hotkeys(&simple("F6")).unwrap()[0];
        let b = to_global_hotkeys(&simple("F6")).unwrap()[0];
        assert_eq!(a, b);
        assert_eq!(a.id(), b.id());
    }

    fn binding(id: u64, key: &str, path: &str) -> HotkeyBinding {
        HotkeyBinding::play(id, simple(key), PathBuf::from(path), None)
    }

    #[test]
    fn grouping_shares_one_grab_between_duplicates() {
        let bindings = vec![
            binding(1, "F6", "/a.mp3"),
            binding(2, "F6", "/b.mp3"),
            binding(3, "F7", "/c.mp3"),
        ];
        let (groups, rejected) = group_bindings(bindings);
        assert!(rejected.is_empty());

        assert_eq!(groups.len(), 2);
        let shared = to_global_hotkeys(&simple("F6")).unwrap()[0];
        let targets = &groups[&shared];
        assert_eq!(targets.len(), 2);
        assert!(targets.iter().any(|t| t.id == 1));
        assert!(targets.iter().any(|t| t.id == 2));
    }

    #[test]
    fn grouping_collects_unconvertible_keys_as_rejected() {
        let bindings = vec![binding(1, "F6", "/a.mp3"), binding(2, "Colon", "/b.mp3")];
        let (groups, rejected) = group_bindings(bindings);
        assert_eq!(groups.len(), 1);
        assert_eq!(rejected.len(), 1);
        assert!(rejected[0].0.contains("Colon"));
    }

    #[test]
    fn dispatch_forwards_play_to_every_mapped_slot() {
        let key = to_global_hotkeys(&simple("F6")).unwrap()[0];
        let mut routing: HashMap<u32, Vec<PressTarget>> = HashMap::new();
        routing.insert(
            key.id(),
            vec![
                PressTarget {
                    id: 1,
                    path: Some(PathBuf::from("/a.mp3")),
                    action: BindingAction::Play,
                    trim: None,
                },
                PressTarget {
                    id: 2,
                    path: Some(PathBuf::from("/b.mp3")),
                    action: BindingAction::Play,
                    trim: None,
                },
            ],
        );

        let (audio_tx, audio_rx) = crossbeam_channel::unbounded();
        let sent = dispatch_press(&routing, key.id(), &audio_tx);
        assert_eq!(sent, 2);

        let mut got = Vec::new();
        while let Ok(AudioCommand::Play {
            id,
            path,
            trim: None,
        }) = audio_rx.try_recv()
        {
            got.push((id, path));
        }
        got.sort_by_key(|(id, _)| *id);
        assert_eq!(
            got,
            vec![(1, PathBuf::from("/a.mp3")), (2, PathBuf::from("/b.mp3"))]
        );
    }

    #[test]
    fn dispatch_sends_stop_all_for_the_stop_key() {
        let key = to_global_hotkeys(&simple("F12")).unwrap()[0];
        let mut routing: HashMap<u32, Vec<PressTarget>> = HashMap::new();
        routing.insert(
            key.id(),
            vec![PressTarget {
                id: STOP_ALL_ID,
                path: None,
                action: BindingAction::StopAll,
                trim: None,
            }],
        );

        let (audio_tx, audio_rx) = crossbeam_channel::unbounded();
        assert_eq!(dispatch_press(&routing, key.id(), &audio_tx), 1);
        assert!(matches!(audio_rx.try_recv(), Ok(AudioCommand::StopAll)));
    }

    #[test]
    fn stop_binding_groups_alongside_play_bindings() {
        let bindings = vec![
            binding(1, "F6", "/a.mp3"),
            HotkeyBinding::stop(simple("F12")),
        ];
        let (groups, rejected) = group_bindings(bindings);
        assert!(rejected.is_empty());
        assert_eq!(groups.len(), 2);
        let stop_key = to_global_hotkeys(&simple("F12")).unwrap()[0];
        let targets = &groups[&stop_key];
        assert_eq!(targets.len(), 1);
        assert_eq!(targets[0].id, STOP_ALL_ID);
        assert_eq!(targets[0].action, BindingAction::StopAll);
    }

    #[test]
    fn dispatch_ignores_unknown_ids() {
        let routing: HashMap<u32, Vec<PressTarget>> = HashMap::new();
        let (audio_tx, audio_rx) = crossbeam_channel::unbounded();
        assert_eq!(dispatch_press(&routing, 12345, &audio_tx), 0);
        assert!(audio_rx.try_recv().is_err());
    }
}
