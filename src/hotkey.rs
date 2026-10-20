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

    /// The audio engine was restarted and now listens on a new channel. The
    /// listener keeps the newest sender so presses never target a dead engine.
    SetAudio(Sender<AudioCommand>),
}

#[derive(Debug)]
pub enum HotkeyEvent {
    Ready,

    Unavailable(String),

    KeyRejected {
        display: String,
        reason: String,
    },

    /// A press could not be delivered because the audio engine died. The UI
    /// restarts the engine (which also sends `HotkeyCommand::SetAudio`) and
    /// retries this command, so the press isn't lost.
    AudioUnavailable(AudioCommand),
}

pub fn spawn_hotkey_thread(
    audio_tx: Sender<AudioCommand>,
    ctx: egui::Context,
) -> (Sender<HotkeyCommand>, Receiver<HotkeyEvent>) {
    let (cmd_tx, cmd_rx) = crossbeam_channel::unbounded::<HotkeyCommand>();
    let (event_tx, event_rx) = crossbeam_channel::unbounded::<HotkeyEvent>();

    std::thread::Builder::new()
        .name("hotkey-listener".to_owned())
        .spawn(move || run_listener(cmd_rx, event_tx, audio_tx, ctx))
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
    mut audio_tx: Sender<AudioCommand>,
    ctx: egui::Context,
) {
    let manager = match GlobalHotKeyManager::new() {
        Ok(manager) => manager,
        Err(err) => {
            warn!("Global hotkey backend unavailable: {err}");
            let _ = event_tx.send(HotkeyEvent::Unavailable(format!(
                "System-wide hotkeys aren't available here ({err}). You can still click ▶ to play sounds."
            )));
            ctx.request_repaint();

            while cmd_rx.recv().is_ok() {}
            return;
        }
    };

    let _ = event_tx.send(HotkeyEvent::Ready);
    ctx.request_repaint();
    info!("Global hotkey backend ready.");

    let mut grabbed: Vec<GlobalKey> = Vec::new();
    let mut routing: HashMap<u32, Vec<PressTarget>> = HashMap::new();

    loop {
        if grabbed.is_empty() {
            // M7: No hotkeys registered; completely sleep until a UI command arrives
            match cmd_rx.recv() {
                Ok(HotkeyCommand::Rebuild(bindings)) => {
                    rebuild(
                        &manager,
                        &event_tx,
                        &ctx,
                        &mut grabbed,
                        &mut routing,
                        bindings,
                    );
                }
                Ok(HotkeyCommand::SetAudio(tx)) => {
                    audio_tx = tx;
                }
                Err(_) => {
                    info!("UI hung up; hotkey listener shutting down.");
                    break;
                }
            }
            continue;
        }

        pump_win32_messages();

        while let Ok(event) = GlobalHotKeyEvent::receiver().try_recv() {
            if event.state() != HotKeyState::Pressed {
                continue;
            }
            let (sent, lost) = dispatch_press(&routing, event.id(), &audio_tx, &event_tx);
            if sent > 0 {
                debug!(
                    "Global hotkey id={} pressed; dispatched Play to {sent} slot(s).",
                    event.id()
                );
            }
            if lost {
                // Wake the UI so it can restart the engine and retry the lost
                // press (it will also send us a fresh audio sender).
                ctx.request_repaint();
            }
        }

        match cmd_rx.try_recv() {
            Ok(HotkeyCommand::Rebuild(bindings)) => {
                rebuild(
                    &manager,
                    &event_tx,
                    &ctx,
                    &mut grabbed,
                    &mut routing,
                    bindings,
                );
                continue;
            }
            Ok(HotkeyCommand::SetAudio(tx)) => {
                audio_tx = tx;
            }
            Err(crossbeam_channel::TryRecvError::Disconnected) => {
                info!("UI hung up; hotkey listener shutting down.");
                break;
            }
            Err(crossbeam_channel::TryRecvError::Empty) => {}
        }

        wait_for_message_or_timeout(WAIT_INTERVAL);
    }
}

const WAIT_INTERVAL: Duration = Duration::from_millis(100);

#[cfg(windows)]
fn wait_for_message_or_timeout(dur: Duration) {
    use windows_sys::Win32::UI::WindowsAndMessaging::{MsgWaitForMultipleObjects, QS_ALLINPUT};
    let ms = dur.as_millis().min(u32::MAX as u128) as u32;
    unsafe {
        MsgWaitForMultipleObjects(0, std::ptr::null(), 0, ms, QS_ALLINPUT);
    }
}

#[cfg(not(windows))]
fn wait_for_message_or_timeout(dur: Duration) {
    std::thread::sleep(dur);
}

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
    ctx: &egui::Context,
    grabbed: &mut Vec<GlobalKey>,
    routing: &mut HashMap<u32, Vec<PressTarget>>,
    bindings: Vec<HotkeyBinding>,
) {
    if !grabbed.is_empty() {
        // L11: Unregister keys individually and track failures to prevent leaking OS grabs
        let mut failed = Vec::new();
        for key in grabbed.drain(..) {
            if let Err(err) = manager.unregister(key) {
                warn!("Failed to unregister hotkey {}: {err}", key.id());
                failed.push(key);
            }
        }
        *grabbed = failed;
    }
    routing.clear();

    let (by_key, rejected) = group_bindings(bindings);
    let mut sent_event = false;
    for (display, reason) in rejected {
        let _ = event_tx.send(HotkeyEvent::KeyRejected { display, reason });
        sent_event = true;
    }

    // A binding can map to more than one physical key (for example "7" grabs
    // both Digit7 and Numpad7). Only report the binding as rejected when none
    // of its keys could be registered, so the UI badge stays truthful.
    let mut succeeded_labels: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut failed_labels: Vec<(String, String)> = Vec::new();

    for (key, group) in by_key {
        match manager.register(key) {
            Ok(()) => {
                for label in &group.labels {
                    succeeded_labels.insert(label.clone());
                }
                routing.insert(key.id(), group.targets);
                grabbed.push(key);
            }
            Err(err) => {
                let reason = format!(
                    "Another program is already using it, or Windows refused it ({err}). \
                     Pick a different key."
                );
                for label in group.labels {
                    failed_labels.push((label, reason.clone()));
                }
            }
        }
    }

    for (display, reason) in failed_labels {
        if !succeeded_labels.contains(&display) {
            let _ = event_tx.send(HotkeyEvent::KeyRejected { display, reason });
            sent_event = true;
        }
    }

    if sent_event {
        ctx.request_repaint();
    }
    info!("Hotkey rebuild complete: {} OS grabs.", grabbed.len());
}

type GroupedBindings = (HashMap<GlobalKey, BindingGroup>, Vec<(String, String)>);

/// All bindings that share one physical OS grab. `labels` are the user-facing
/// `Hotkey::display()` values, which is what the UI matches rejections against.
struct BindingGroup {
    labels: Vec<String>,
    targets: Vec<PressTarget>,
}

fn group_bindings(bindings: Vec<HotkeyBinding>) -> GroupedBindings {
    let mut by_key: HashMap<GlobalKey, BindingGroup> = HashMap::new();
    let mut rejected = Vec::new();
    for binding in bindings {
        let label = binding.hotkey.display();
        match to_global_hotkeys(&binding.hotkey) {
            Ok(keys) => {
                for key in keys {
                    let group = by_key.entry(key).or_insert_with(|| BindingGroup {
                        labels: Vec::new(),
                        targets: Vec::new(),
                    });
                    if !group.labels.contains(&label) {
                        group.labels.push(label.clone());
                    }
                    group.targets.push(PressTarget {
                        id: binding.id,
                        path: binding.path.clone(),
                        action: binding.action,
                        trim: binding.trim,
                    });
                }
            }
            Err(reason) => rejected.push((label, reason)),
        }
    }
    (by_key, rejected)
}

fn dispatch_press(
    routing: &HashMap<u32, Vec<PressTarget>>,
    id: u32,
    audio_tx: &Sender<AudioCommand>,
    event_tx: &Sender<HotkeyEvent>,
) -> (usize, bool) {
    let Some(targets) = routing.get(&id) else {
        return (0, false);
    };
    let mut sent = 0;
    let mut lost = false;
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
        match audio_tx.send(cmd) {
            Ok(()) => sent += 1,
            Err(err) => {
                lost = true;
                let _ = event_tx.send(HotkeyEvent::AudioUnavailable(err.into_inner()));
            }
        }
    }
    (sent, lost)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Hotkey;

    #[test]
    fn grouping_carries_the_user_facing_label() {
        let (groups, rejected) = group_bindings(vec![binding(1, "Q", "/a.mp3")]);
        assert!(rejected.is_empty());
        let group = groups.values().next().expect("one grab");
        assert_eq!(group.labels, vec!["Q".to_string()]);
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
        for group in groups.values() {
            assert_eq!(group.targets.len(), 1);
            assert_eq!(group.targets[0].id, 1);
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
        let group = &groups[&shared];
        assert_eq!(group.labels, vec!["F6".to_string()]);
        assert_eq!(group.targets.len(), 2);
        assert!(group.targets.iter().any(|t| t.id == 1));
        assert!(group.targets.iter().any(|t| t.id == 2));
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
        let (event_tx, _event_rx) = crossbeam_channel::unbounded();
        let (sent, lost) = dispatch_press(&routing, key.id(), &audio_tx, &event_tx);
        assert_eq!(sent, 2);
        assert!(!lost);

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
        let (event_tx, _event_rx) = crossbeam_channel::unbounded();
        assert_eq!(
            dispatch_press(&routing, key.id(), &audio_tx, &event_tx),
            (1, false)
        );
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
        let group = &groups[&stop_key];
        assert_eq!(group.targets.len(), 1);
        assert_eq!(group.targets[0].id, STOP_ALL_ID);
        assert_eq!(group.targets[0].action, BindingAction::StopAll);
    }

    #[test]
    fn dispatch_ignores_unknown_ids() {
        let routing: HashMap<u32, Vec<PressTarget>> = HashMap::new();
        let (audio_tx, audio_rx) = crossbeam_channel::unbounded();
        let (event_tx, event_rx) = crossbeam_channel::unbounded();
        assert_eq!(
            dispatch_press(&routing, 12345, &audio_tx, &event_tx),
            (0, false)
        );
        assert!(audio_rx.try_recv().is_err());
        assert!(event_rx.try_recv().is_err());
    }

    #[test]
    fn dispatch_reports_a_dead_audio_engine() {
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

        let (audio_tx, audio_rx) = crossbeam_channel::unbounded::<AudioCommand>();
        drop(audio_rx);
        let (event_tx, event_rx) = crossbeam_channel::unbounded();
        let (sent, lost) = dispatch_press(&routing, key.id(), &audio_tx, &event_tx);
        assert_eq!(sent, 0);
        assert!(lost);
        assert!(matches!(
            event_rx.try_recv(),
            Ok(HotkeyEvent::AudioUnavailable(AudioCommand::StopAll))
        ));
    }
}
