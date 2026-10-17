//! Phase 2: real local playback + native file picking.
//!
//! The UI owns no audio state itself. It sends [`AudioCommand`]s to the
//! background engine thread (`src/audio.rs`) and reflects the [`AudioEvent`]s
//! that come back (tile highlights, missing-file flags, error toasts).
//!
//! UI construction is split into free functions that *return* the action the
//! user intended (`SlotAction`) rather than mutating app state directly. The
//! app applies those actions after the frame is drawn. This keeps borrows
//! simple and makes the data flow easy to follow.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::time::{Duration, Instant};

use crossbeam_channel::{Receiver, Sender};
use egui::{
    Align, Align2, Button, Color32, CornerRadius, Frame, Layout, Margin, RichText, Sense, Stroke,
    Vec2,
};
use log::debug;

use crate::audio::{spawn_audio_engine, AudioCommand, AudioEvent, TrimSpec, TEST_SOUND_ID};
use crate::config::{self, AppState, STATE_VERSION};
use crate::devices::{list_output_devices, SYSTEM_DEFAULT_LABEL};
use crate::waveform::{build_waveform, Waveform};
use crate::hotkey::{
    spawn_hotkey_thread, to_global_hotkeys, HotkeyBinding, HotkeyCommand, HotkeyEvent, STOP_ALL_ID,
};
use crate::record::{spawn_recorder, RecordCommand, RecordEvent};
use crate::tray::{Tray, TrayAction};
use crate::model::{Board, BoardId, Hotkey, SlotId, SoundSlot};
use crate::theme;

/// Fixed tile geometry (in points). Tiles are large on purpose: easy targets.
const SLOT_W: f32 = 200.0;
const SLOT_H: f32 = 156.0;
const SLOT_PAD: f32 = 12.0;
const SLOT_GAP: f32 = 12.0;

/// Everything the user can ask a tile to do. Collected during drawing and
/// applied afterwards.
enum SlotAction {
    /// Open the native picker; if a file is chosen, append a filled slot.
    AddViaPicker,
    Play(SlotId),
    Stop(SlotId),
    StartRecording(SlotId),
    ClearHotkey(SlotId),
    Remove(SlotId),
    /// Open the trim editor for this slot.
    Edit(SlotId),
    /// Re-link a missing file through the native picker.
    FindAgain(SlotId),
}

/// A pending yes/no question shown as a modal (e.g. duplicate hotkey).
#[derive(Clone)]
struct Confirm {
    message: String,
    slot: SlotId,
    hotkey: Hotkey,
    accept: String,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ToastKind {
    Info,
    Good,
    Warn,
}

impl ToastKind {
    fn colour(self) -> Color32 {
        match self {
            ToastKind::Info => theme::MUTED,
            ToastKind::Good => theme::GOOD,
            ToastKind::Warn => theme::WARN,
        }
    }
}

struct Toast {
    text: String,
    kind: ToastKind,
    until: Instant,
}

pub struct SoundboardApp {
    /// Every board; exactly one is active (visible, grabbed) at a time.
    boards: Vec<Board>,
    active_board: BoardId,

    output_devices: Vec<String>,
    output_device: String,
    volume: f32,

    /// Channels to the background audio engine (see `src/audio.rs`).
    audio_tx: Sender<AudioCommand>,
    audio_rx: Receiver<AudioEvent>,

    /// Channels to the global-hotkey thread (see `src/hotkey.rs`).
    hotkey_tx: Sender<HotkeyCommand>,
    hotkey_rx: Receiver<HotkeyEvent>,
    hotkey_state: HotkeyBackendState,
    /// Channels to the mic recorder thread (see `src/record.rs`).
    record_tx: Sender<RecordCommand>,
    record_rx: Receiver<RecordEvent>,
    /// An in-progress microphone take (started when the red button was hit).
    mic: Option<MicTake>,
    /// `Start` sent, waiting for the recorder to confirm. Drives the
    /// "Starting…" button state and the watchdog that restarts the recorder
    /// thread if a device open wedges inside cpal.
    mic_starting: Option<Instant>,
    /// Microphone picker: discovered input devices + the current choice.
    /// The OS default is frequently a silent virtual endpoint (VB-Cable's
    /// `CABLE Output`), so the user must be able to pick explicitly.
    mic_devices: Vec<String>,
    mic_device: String,
    /// The global Stop key: silences everything, even while minimized.
    /// `None` = not bound yet (toolbar badge invites setting it).
    stop_hotkey: Option<Hotkey>,
    /// Set by every mutation; flushed to disk debounced (see `maybe_autosave`).
    config_dirty: bool,
    last_save: Instant,
    /// Slots whose files decode-failed: excluded from OS grabs until re-linked.
    /// (Missing files are already excluded via `slot.missing`.)
    broken: HashSet<SlotId>,

    /// Which tile is currently waiting for the user to press keys.
    recording: Option<SlotId>,
    /// Tiles with sound currently coming out of the speakers.
    /// Driven by `Started` / `Finished` engine events, not timers.
    playing: HashSet<SlotId>,
    /// The Test Sound beep is playing (it has no tile to highlight).
    testing: bool,

    confirm: Option<Confirm>,
    toast: Option<Toast>,
    /// Rename / delete-board modal. Only one modal shows at a time (hotkey
    /// confirms and board dialogs never overlap: recording must finish first).
    board_dialog: Option<BoardDialog>,
    /// Trim editor state (modal). Only one editor at a time.
    editor: Option<EditorState>,
    /// Decoded waveforms, built off-thread on demand.
    waves: WaveCache,
    /// System-tray icon, `None` if Windows refused it (or non-Windows).
    tray: Option<Tray>,
    /// User preference: closing the window hides to the tray instead of
    /// quitting (see the Settings dialog).
    close_to_tray: bool,
    /// Settings dialog visibility.
    settings_open: bool,
    /// Set when the user asked to quit from the tray, so the close handler
    /// lets the close through instead of hiding to the tray again.
    quit_requested: bool,
    /// Whether the main window is currently shown. Input events are only
    /// meaningful while it is (a hidden viewport gets no fresh events), which
    /// gates hotkey recording.
    window_visible: bool,
}

/// What the trim editor is changing. Bounds are a working copy until Save.
#[derive(Clone)]
struct EditorState {
    slot: SlotId,
    start_frac: f32,
    end_frac: f32,
    /// Which handle the current drag is moving (chosen on drag start).
    drag: Option<EditorHandle>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum EditorHandle {
    Start,
    End,
}

/// Async waveform store: at most one decode per slot in flight; late
/// responses for re-linked files are dropped by path check.
struct WaveCache {
    ready: HashMap<SlotId, Result<Waveform, String>>,
    pending: HashSet<SlotId>,
    tx: crossbeam_channel::Sender<(SlotId, PathBuf, Result<Waveform, String>)>,
    rx: crossbeam_channel::Receiver<(SlotId, PathBuf, Result<Waveform, String>)>,
}

impl WaveCache {
    fn new() -> Self {
        let (tx, rx) = crossbeam_channel::unbounded();
        Self {
            ready: HashMap::new(),
            pending: HashSet::new(),
            tx,
            rx,
        }
    }

    /// Start a background decode unless cached or already running.
    fn request(&mut self, id: SlotId, path: PathBuf) {
        if self.ready.contains_key(&id) || !self.pending.insert(id) {
            return;
        }
        let tx = self.tx.clone();
        std::thread::Builder::new()
            .name("waveform-decode".to_owned())
            .spawn(move || {
                let result = build_waveform(&path);
                let _ = tx.send((id, path, result));
            })
            .expect("failed to spawn waveform thread");
    }

    fn invalidate(&mut self, id: SlotId) {
        self.ready.remove(&id);
        self.pending.remove(&id);
    }
}

/// A board management modal.
enum BoardDialog {
    Rename { id: BoardId, name: String },
    Delete { id: BoardId, name: String, sounds: usize },
}

/// Attribution shown next to the version badge in the title bar. Clicking it
/// opens the author's GitHub profile in the default browser.
const AUTHOR_URL: &str = "https://github.com/IAmThroot";
const AUTHOR_CREDIT: &str = "Built by IAmThroot";

/// Frame budget for the tile glow animation (~30 FPS: smooth, cheap).
const TILE_ANIMATION_FRAME: Duration = Duration::from_millis(33);
/// Frame budget for the recording timer (10 FPS shows tenths cleanly).
const TIMER_FRAME: Duration = Duration::from_millis(100);
/// Deadline for a microphone start before the watchdog fires.
const MIC_START_TIMEOUT: Duration = Duration::from_secs(8);

/// A microphone take in progress.
struct MicTake {
    started: Instant,
}

/// What the status bar truthfully reports about system-wide hotkeys.
#[derive(Clone)]
enum HotkeyBackendState {
    /// Thread starting, no word from the OS yet.
    Starting,
    /// Grabs are live — minimizing is safe.
    Active,
    /// No backend (Wayland dev box) or all grabs refused. Mouse still works.
    Unavailable(String),
}

impl SoundboardApp {
    pub fn new(cc: &eframe::CreationContext<'_>) -> Self {
        theme::apply(&cc.egui_ctx);

        if let Some(path) = config::config_path() {
            log::info!("Config file: {}.", path.display());
        }

        let mut output_devices = list_output_devices();
        let mut output_device = SYSTEM_DEFAULT_LABEL.to_owned();
        let mut volume = 0.7;
        let mut stop_hotkey: Option<Hotkey> = None;
        // Microphone picker: enumerate inputs up front; the saved choice may
        // be a device that is currently unplugged (kept in the list so it
        // reappears when replugged, mirroring the output picker's behavior).
        let mut mic_devices = crate::devices::list_input_devices();
        let mut mic_device = SYSTEM_DEFAULT_LABEL.to_owned();
        let mut close_to_tray = false;

        // Restore the saved state when there is one; otherwise start with one
        // empty board — the grid's "No sounds yet" state points at ➕ Add Sound.
        let mut boards: Vec<Board> = vec![Board::empty(1, "My Sounds")];
        let mut active_board: BoardId = 1;
        if let Some(saved) = config::load() {
            volume = saved.volume;
            stop_hotkey = saved.stop_hotkey;
            boards = saved.boards;
            active_board = saved.active_board;
            if let Some(saved_device) = saved.output_device_name {
                // The saved device may be gone (unplugged cable). Keep the
                // name selected anyway: the engine falls back with a warning,
                // and reappears in the list via Refresh when replugged.
                if !output_devices.contains(&saved_device) {
                    output_devices.insert(1, saved_device.clone());
                }
                output_device = saved_device;
            }
            close_to_tray = saved.close_to_tray;
            if let Some(saved_mic) = saved.input_device_name {
                if !mic_devices.contains(&saved_mic) {
                    mic_devices.insert(1, saved_mic.clone());
                }
                mic_device = saved_mic;
            }
            log::info!(
                "Restored {} boards from config.",
                boards.len()
            );
        }

        // The engine opens the device on its own thread; the UI stays fluid.
        // `DeviceReady` / fallback notes arrive as events on the next frames.
        let (audio_tx, audio_rx) =
            spawn_audio_engine(output_device.clone(), volume);

        // The hotkey thread forwards presses straight to `audio_tx`, so it
        // needs its own clone — presses bypass the UI entirely.
        let (hotkey_tx, hotkey_rx) = spawn_hotkey_thread(audio_tx.clone());

        // The recorder lives on its own thread too; takes arrive as events.
        let (record_tx, record_rx) = spawn_recorder();

        let app = Self {
            boards,
            active_board,
            output_devices,
            output_device,
            volume,
            audio_tx,
            audio_rx,
            hotkey_tx,
            hotkey_rx,
            hotkey_state: HotkeyBackendState::Starting,
            record_tx,
            record_rx,
            mic: None,
            mic_starting: None,
            mic_devices,
            mic_device,
            stop_hotkey,
            config_dirty: false,
            last_save: Instant::now(),
            broken: HashSet::new(),
            recording: None,
            playing: HashSet::new(),
            testing: false,
            confirm: None,
            toast: None,
            board_dialog: None,
            editor: None,
            waves: WaveCache::new(),
            tray: Tray::new(&cc.egui_ctx),
            close_to_tray,
            settings_open: false,
            quit_requested: false,
            window_visible: true,
        };
        // Grab restored bindings, if any (a fresh board has none).
        app.push_hotkey_rebuild();
        // Window icon (title bar / Alt-Tab / taskbar), drawn from the same art
        // as the .exe icon so they can never drift.
        {
            const SIZE: u32 = 64;
            cc.egui_ctx.send_viewport_cmd(egui::ViewportCommand::Icon(Some(
                std::sync::Arc::new(egui::IconData {
                    rgba: crate::icon_art::draw(SIZE),
                    width: SIZE,
                    height: SIZE,
                }),
            )));
        }
        app
    }

    /// Tray interaction and the close-to-tray interception. Every frame.
    ///
    /// Close semantics with the option on: the window button hides the app
    /// (it disappears from the taskbar, the tray icon stays, hotkeys keep
    /// working) and quitting happens from the tray menu. With the option
    /// off, close quits — the app's original behavior.
    fn handle_tray(&mut self, ctx: &egui::Context) {
        if let Some(tray) = &self.tray {
            for action in tray.poll() {
                match action {
                    TrayAction::Open => {
                        ctx.send_viewport_cmd(egui::ViewportCommand::Visible(true));
                        ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
                        self.window_visible = true;
                        ctx.request_repaint();
                    }
                    TrayAction::Quit => {
                        // Let the close through instead of hiding to the tray.
                        self.quit_requested = true;
                        ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                    }
                }
            }
        }

        let close_requested = ctx.input(|i| i.viewport().close_requested());
        // `tray.is_some()` is essential: if the icon failed to create (or the
        // build has no tray), hiding the window would leave no way to bring
        // it back — closing must keep quitting in that case.
        if close_requested && !self.quit_requested && self.close_to_tray && self.tray.is_some() {
            ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
            ctx.send_viewport_cmd(egui::ViewportCommand::Visible(false));
            self.window_visible = false;
            // Flush pending changes now: no window means no chance to explain
            // a save failure later.
            if self.config_dirty {
                self.save_now();
            }
            log::info!("Window hidden; Open Soundboard is in the system tray.");
        }
    }

    /// Settings dialog (⚙ in the toolbar): currently just the tray toggle.
    fn settings_window(&mut self, ctx: &egui::Context) {
        if !self.settings_open {
            return;
        }
        let mut open = true;
        egui::Window::new("Settings")
            .open(&mut open)
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::RIGHT_TOP, egui::vec2(-16.0, 16.0))
            .show(ctx, |ui| {
                // Enabled only when a tray icon actually exists (Windows);
                // the stub platform reports `None`.
                ui.add_enabled_ui(self.tray.is_some(), |ui| {
                    ui.checkbox(
                        &mut self.close_to_tray,
                        "Close the window → hide to the system tray",
                    );
                });
                if self.tray.is_none() {
                    ui.add(
                        egui::Label::new(
                            RichText::new(
                                "Unavailable: this build has no system tray (tray icon creation failed).",
                            )
                            .size(11.5)
                            .color(theme::WARN),
                        )
                        .wrap(),
                    );
                } else {
                    ui.add(
                        egui::Label::new(
                            RichText::new(
                                "With this on, closing the window keeps the app running in the tray. \
                                 Right-click the tray icon for “Quit”.",
                            )
                            .size(11.5)
                            .color(theme::MUTED),
                        )
                        .wrap(),
                    );
                    if self.close_to_tray {
                        ui.label(
                            RichText::new("Your hotkeys keep working while it is in the tray.")
                                .size(11.5)
                                .color(theme::MUTED),
                        );
                    }
                }
            });
        self.settings_open = open;
    }

    fn alloc_id(&mut self) -> SlotId {
        config::next_slot_id(&self.boards)
    }

    fn alloc_board_id(&self) -> BoardId {
        config::next_board_id(&self.boards)
    }

    /// The visible board.
    fn active(&self) -> &Board {
        self.boards
            .iter()
            .find(|b| b.id == self.active_board)
            .expect("active board always exists")
    }

    fn active_mut(&mut self) -> &mut Board {
        let active = self.active_board;
        self.boards
            .iter_mut()
            .find(|b| b.id == active)
            .expect("active board always exists")
    }

    /// Find a slot on any board (presses only ever address the active one,
    /// but engine events are resolved defensively across all boards).
    fn find_slot_mut(&mut self, id: SlotId) -> Option<&mut SoundSlot> {
        self.boards
            .iter_mut()
            .flat_map(|b| b.slots.iter_mut())
            .find(|s| s.id == id)
    }

    // --- feedback ---------------------------------------------------------

    fn toast(&mut self, kind: ToastKind, text: impl Into<String>) {
        self.toast = Some(Toast {
            text: text.into(),
            kind,
            until: Instant::now() + Duration::from_secs(4),
        });
    }

    fn set_hotkey(&mut self, id: SlotId, hotkey: Hotkey) {
        let display = hotkey.display();
        debug!("Hotkey stored: slot={id} key={display}.");
        let risky = hotkey.looks_like_typing();
        if let Some(slot) = self.find_slot_mut(id) {
            slot.hotkey = Some(hotkey);
        }
        self.push_hotkey_rebuild();
        self.mark_dirty();
        if risky {
            self.toast(
                ToastKind::Warn,
                format!("Hotkey set to {display} — it will also fire while you type."),
            );
        } else {
            self.toast(ToastKind::Good, format!("Hotkey set to {display}."));
        }
    }

    /// Send the current grabbable bindings to the hotkey thread.
    /// Slots count when they have a hotkey, point at a file, and are neither
    /// missing nor decode-broken. The Stop key is included when bound.
    /// Each key is owned by at most one target (see `guard_hotkey`).
    fn push_hotkey_rebuild(&self) {
        // Only the ACTIVE board is ever grabbed — other boards' identical keys
        // stay dormant until switched to.
        let mut bindings: Vec<HotkeyBinding> = self
            .active()
            .slots
            .iter()
            .filter(|s| !s.missing && !self.broken.contains(&s.id))
            .filter_map(|s| {
                Some(HotkeyBinding::play(
                    s.id,
                    s.hotkey.clone()?,
                    s.file_path.clone()?,
                    trim_spec_for(s),
                ))
            })
            .collect();
        if let Some(stop) = &self.stop_hotkey {
            bindings.push(HotkeyBinding::stop(stop.clone()));
        }
        let _ = self.hotkey_tx.send(HotkeyCommand::Rebuild(bindings));
    }

    /// Switch to another board: silence the old one, grab the new one's keys.
    fn switch_board(&mut self, id: BoardId) {
        if id == self.active_board {
            return;
        }
        if !self.boards.iter().any(|b| b.id == id) {
            return;
        }
        self.playing.clear();
        self.testing = false;
        self.send(AudioCommand::StopAll);
        self.active_board = id;
        self.push_hotkey_rebuild();
        self.mark_dirty();
    }

    /// Create a board with a unique default name and switch to it.
    fn add_board(&mut self) {
        let mut n = self.boards.len() + 1;
        loop {
            let name = format!("Board {n}");
            if !self.boards.iter().any(|b| b.name == name) {
                let id = self.alloc_board_id();
                self.boards.push(Board::empty(id, name));
                self.switch_board(id);
                self.toast(ToastKind::Good, "New board created.");
                return;
            }
            n += 1;
        }
    }

    fn delete_board(&mut self, id: BoardId) {
        if self.boards.len() <= 1 {
            return;
        }
        let Some(pos) = self.boards.iter().position(|b| b.id == id) else {
            return;
        };
        let removed: Board = self.boards.remove(pos);
        for slot in &removed.slots {
            self.broken.remove(&slot.id);
        }
        self.board_dialog = None;
        // Landing board: neighbour if there is one, else the first.
        let landing = self
            .boards
            .get(pos.min(self.boards.len().saturating_sub(1)))
            .map(|b| b.id)
            .expect("at least one board remains");
        self.switch_board(landing);
        self.mark_dirty();
        self.toast(
            ToastKind::Info,
            format!("Deleted “{}”.", removed.name),
        );
    }

    /// Drop every OS grab (used while recording a key — see `handle_recording`).
    fn suspend_hotkey_grabs(&self) {
        debug!("Suspending all OS grabs while recording.");
        let _ = self.hotkey_tx.send(HotkeyCommand::Rebuild(Vec::new()));
    }

    /// Assign a freshly recorded hotkey. One key = one owner: if the key is
    /// taken, the user is offered a *move* (unbind there, bind here), never a
    /// silent share.
    fn guard_hotkey(&mut self, id: SlotId, hotkey: Hotkey) {
        // Reject keys the OS could never grab before any duplicate chat.
        if let Err(reason) = to_global_hotkeys(&hotkey) {
            debug!("Bind rejected (ungrabbable): slot={id} key={}", hotkey.display());
            self.toast(ToastKind::Warn, reason);
            return;
        }

        // The Stop key is special: it must be freed there first.
        if self.stop_hotkey.as_ref() == Some(&hotkey) {
            debug!("Bind refused (stop key): slot={id} key={}", hotkey.display());
            self.toast(
                ToastKind::Warn,
                format!(
                    "“{}” is your stop key. Right-click the stop badge to remove it first, \
                     then use it here.",
                    hotkey.display()
                ),
            );
            return;
        }

        // Clash scope is the ACTIVE board only: the same key may live on other
        // boards (their grabs are never live at the same time).
        let clash = self
            .active()
            .slots
            .iter()
            .find(|s| s.id != id && s.hotkey.as_ref() == Some(&hotkey))
            .map(|s| (s.id, s.name.clone()));

        if clash.is_none() && !hotkey.looks_like_typing() {
            debug!("Bind set directly: slot={id} key={}", hotkey.display());
            self.set_hotkey(id, hotkey);
            return;
        }

        // A taken key always offers the move; a merely risky key offers
        // "use anyway". When both apply, the move dialog wins and the risk is
        // still surfaced by the warning toast after moving.
        if let Some((other_id, other)) = clash {
            debug!(
                "Bind clash: key={} already on slot={other_id}; showing move dialog for slot={id}.",
                hotkey.display()
            );
            self.confirm = Some(Confirm {
                message: format!(
                    "“{}” already plays “{other}”.\n\nMove it to this sound instead?",
                    hotkey.display(),
                ),
                slot: id,
                hotkey,
                accept: "Move it here".to_owned(),
            });
            return;
        }

        self.confirm = Some(Confirm {
            message: "This key has no Ctrl or Alt, so it will also trigger while you \
                      are typing in other apps.\n\nUse it anyway?"
                .to_owned(),
            slot: id,
            hotkey,
            accept: "Use Anyway".to_owned(),
        });
    }

    /// Validate and store a freshly recorded Stop key. The user may pick any
    /// physically grabbable key — including bare letters, with a heads-up that
    /// they fire while typing. Only two things stay impossible: keys the OS
    /// cannot grab at all (rejected with the technical reason), and keys that
    /// already play a sound (that press would play *and* stop at once).
    fn guard_stop_hotkey(&mut self, hotkey: Hotkey) {
        if let Err(reason) = to_global_hotkeys(&hotkey) {
            self.toast(ToastKind::Warn, reason);
            return;
        }
        // The Stop key is global, so it must not collide on ANY board: a slot
        // holding the same key on an inactive board today becomes a Play+Stop
        // double-trigger the moment that board is switched to.
        if let Some(owner) = self
            .boards
            .iter()
            .flat_map(|b| b.slots.iter())
            .find(|s| s.hotkey.as_ref() == Some(&hotkey))
        {
            let name = owner.name.clone();
            self.toast(
                ToastKind::Warn,
                format!(
                    "“{}” already plays “{name}”. Clear it there first, or pick another stop key.",
                    hotkey.display()
                ),
            );
            return;
        }

        let display = hotkey.display();
        let risky = hotkey.looks_like_typing();
        debug!("Stop key stored: key={display} (risky={risky}).");
        self.stop_hotkey = Some(hotkey);
        self.push_hotkey_rebuild();
        self.mark_dirty();
        if risky {
            self.toast(
                ToastKind::Warn,
                format!(
                    "Stop key set to {display} — heads up: it will also trigger while you type."
                ),
            );
        } else {
            self.toast(
                ToastKind::Good,
                format!("Stop key set to {display}. Press it any time to silence everything."),
            );
        }
    }

    // --- panels -----------------------------------------------------------

    fn top_bar(&mut self, ui: &mut egui::Ui) {
        let frame = Frame::NONE
            .fill(theme::PANEL)
            .inner_margin(Margin::symmetric(16, 12));

        egui::Panel::top("top_bar")
            .frame(frame)
            .show_separator_line(false)
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.label(RichText::new("🔊  Open Soundboard").size(18.0).strong());
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        // From Cargo.toml, so bumping the version there is
                        // enough — the badge can never drift.
                        ui.label(
                            RichText::new(format!("v{}", env!("CARGO_PKG_VERSION")))
                                .size(11.5)
                                .color(theme::MUTED),
                        );
                        ui.label(RichText::new("·").size(11.5).color(theme::MUTED));
                        // Attribution link: egui opens the URL in the
                        // default browser on click.
                        ui.hyperlink_to(
                            RichText::new(AUTHOR_CREDIT).size(11.5).color(theme::MUTED),
                            AUTHOR_URL,
                        )
                        .on_hover_text(format!("Open {AUTHOR_URL} in your browser"));
                    });
                });

                ui.add_space(8.0);

                ui.horizontal(|ui| {
                    ui.label(RichText::new("Where should it play?").size(13.5));
                    let before = self.output_device.clone();
                    egui::ComboBox::from_id_salt("output_device")
                        .selected_text(self.output_device.clone())
                        .width(260.0)
                        .show_ui(ui, |ui| {
                            for device in self.output_devices.clone() {
                                ui.selectable_value(
                                    &mut self.output_device,
                                    device.clone(),
                                    device,
                                );
                            }
                        });
                    // Switching devices drops whatever is playing (the old
                    // OS stream is closed), so clear the highlights now; the
                    // engine confirms with a `DeviceReady` event.
                    if self.output_device != before {
                        self.playing.clear();
                        self.testing = false;
                        self.send(AudioCommand::SetDevice(self.output_device.clone()));
                        self.mark_dirty();
                        self.toast(
                            ToastKind::Info,
                            format!("Switching sound to {:?}…", self.output_device),
                        );
                    }

                    // New hardware (e.g. freshly installed VB-Cable) appears
                    // without restarting the app.
                    if ui
                        .small_button("↻")
                        .on_hover_text("Look for newly plugged in devices")
                        .clicked()
                    {
                        self.output_devices = list_output_devices();
                        // The saved selection may have vanished; keep it if it
                        // is still there, otherwise fall back to System Default.
                        if !self.output_devices.contains(&self.output_device) {
                            self.output_device = SYSTEM_DEFAULT_LABEL.to_owned();
                            self.send(AudioCommand::SetDevice(self.output_device.clone()));
                            self.mark_dirty();
                        }
                        self.toast(
                            ToastKind::Info,
                            "Device list refreshed.",
                        );
                    }

                    let test_label = if self.testing {
                        "🔈  Playing…"
                    } else {
                        "🔈  Test Sound"
                    };
                    if ui
                        .add_enabled(!self.testing, Button::new(test_label))
                        .on_hover_text("Play a short beep through the selected device")
                        .clicked()
                    {
                        self.testing = true;
                        self.send(AudioCommand::PlayTest);
                    }
                });

                ui.add_space(6.0);

                ui.horizontal(|ui| {
                    ui.label(RichText::new("Volume").size(13.5));
                    let slider = ui.add(
                        egui::Slider::new(&mut self.volume, 0.0..=1.0)
                            .show_value(false)
                            .handle_shape(egui::style::HandleShape::Circle),
                    );
                    if slider.changed() {
                        self.send(AudioCommand::SetVolume(self.volume));
                        self.mark_dirty();
                    }
                    ui.label(
                        RichText::new(format!("{}%", (self.volume * 100.0).round() as i32))
                            .size(12.5)
                            .color(theme::MUTED),
                    );
                });

                ui.add_space(4.0);

                ui.label(
                    RichText::new(
                        "Tip: to play through your mic in Discord, Zoom or a game, install a \
                         virtual cable and pick it in the list above.",
                    )
                    .size(11.5)
                    .color(theme::MUTED),
                );
            });
    }

    fn toolbar(&mut self, ui: &mut egui::Ui) {
        let frame = Frame::NONE
            .fill(theme::BG)
            .inner_margin(Margin::symmetric(16, 10));

        egui::Panel::top("toolbar")
            .frame(frame)
            .show_separator_line(false)
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    if ui
                        .add(Button::new(RichText::new("➕  Add Sound").strong()))
                        .on_hover_text("Pick a sound file from your computer")
                        .clicked()
                    {
                        self.add_sound_via_picker();
                    }

                    let any_playing = !self.playing.is_empty() || self.testing;
                    if ui
                        .add_enabled(
                            any_playing,
                            Button::new(RichText::new("⏹  Stop All")),
                        )
                        .on_hover_text("Stop every sound right now")
                        .clicked()
                    {
                        self.playing.clear();
                        self.testing = false;
                        self.send(AudioCommand::StopAll);
                    }

                    // The global Stop key: same action, triggerable from any
                    // app while minimized. Click to bind, right-click to clear.
                    let recording_stop = self.recording == Some(STOP_ALL_ID);
                    let (stop_text, stop_colour) = if recording_stop {
                        (
                            "Press keys…  (Esc to cancel)".to_owned(),
                            theme::TEXT,
                        )
                    } else {
                        match &self.stop_hotkey {
                            Some(hotkey) => (hotkey.display(), theme::TEXT),
                            None => ("Set stop key".to_owned(), theme::MUTED),
                        }
                    };
                    let stop_badge = ui.add_sized(
                        [110.0, 26.0],
                        Button::new(RichText::new(stop_text).size(12.5).color(stop_colour))
                            .fill(theme::BG_SOFT)
                            .stroke(Stroke::new(1.0, theme::BORDER))
                            .corner_radius(CornerRadius::same(8)),
                    );
                    if recording_stop {
                        // Swallow clicks until a key (or Esc) lands.
                    } else if stop_badge.clicked() {
                        debug!("Arming hotkey record for the Stop key.");
                        self.recording = Some(STOP_ALL_ID);
                        self.suspend_hotkey_grabs();
                        self.toast(
                            ToastKind::Info,
                            "Press the key that should stop everything (Esc to cancel).",
                        );
                    } else if stop_badge.secondary_clicked() && self.stop_hotkey.is_some() {
                        self.stop_hotkey = None;
                        self.push_hotkey_rebuild();
                        self.mark_dirty();
                        self.toast(ToastKind::Info, "Stop key removed.");
                    } else {
                        stop_badge.on_hover_text(
                            "The key that silences everything, even while minimized",
                        );
                    }

                    // Microphone recording: red button toggles a take. Takes
                    // land on this board automatically when stopped.
                    if let Some(take) = &self.mic {
                        let elapsed = fmt_time(take.started.elapsed().as_secs_f32());
                        if ui
                            .add_sized(
                                [110.0, 26.0],
                                Button::new(
                                    RichText::new(format!("■ Stop {elapsed}"))
                                        .size(12.5)
                                        .color(Color32::WHITE),
                                )
                                .fill(theme::DANGER)
                                .corner_radius(CornerRadius::same(8)),
                            )
                            .on_hover_text("Stop recording and keep the take (quitting discards it)")
                            .clicked()
                        {
                            let _ = self.record_tx.send(RecordCommand::Stop);
                            self.toast(ToastKind::Info, "Finishing the take…");
                        }
                    } else {
                        // Mic picker (small: the toolbar is shared with the
                        // output picker, so keep it compact). Frozen while a
                        // start is in flight so the choice matches the take.
                        let starting = self.mic_starting.is_some();
                        let mut chosen = self.mic_device.clone();
                        egui::ComboBox::from_id_salt("mic_device")
                            .selected_text(
                                RichText::new(shorten_device(&self.mic_device)).size(12.0),
                            )
                            .width(150.0)
                            .show_ui(ui, |ui| {
                                for device in self.mic_devices.clone() {
                                    ui.selectable_value(&mut chosen, device.clone(), device);
                                }
                            });
                        if chosen != self.mic_device {
                            self.mic_device = chosen;
                            self.mark_dirty();
                        }
                        if ui
                            .small_button("↻")
                            .on_hover_text("Look for newly plugged in microphones")
                            .clicked()
                        {
                            self.mic_devices = crate::devices::list_input_devices();
                            if !self.mic_devices.contains(&self.mic_device) {
                                self.mic_device = SYSTEM_DEFAULT_LABEL.to_owned();
                                self.mark_dirty();
                            }
                        }
                        if starting {
                            // Honest wait state: the device open can take a
                            // moment (and the watchdog recovers if it wedges).
                            ui.add_enabled_ui(false, |ui| {
                                ui.add_sized(
                                    [90.0, 26.0],
                                    egui::Button::new(
                                        RichText::new("● Starting…")
                                            .size(12.5)
                                            .color(theme::MUTED),
                                    )
                                    .fill(theme::BG_SOFT)
                                    .corner_radius(CornerRadius::same(8)),
                                );
                            });
                        } else if ui
                            .add_sized(
                                [90.0, 26.0],
                                Button::new(RichText::new("● Record").size(12.5).color(theme::TEXT))
                                    .fill(theme::BG_SOFT)
                                    .stroke(Stroke::new(1.0, theme::BORDER))
                                    .corner_radius(CornerRadius::same(8)),
                            )
                            .on_hover_text("Record from the selected microphone into this board")
                            .clicked()
                        {
                            let device = (self.mic_device != SYSTEM_DEFAULT_LABEL)
                                .then(|| self.mic_device.clone());
                            self.mic_starting = Some(Instant::now());
                            let _ = self.record_tx.send(RecordCommand::Start(device));
                        }
                    }

                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        let count = self.active().slots.len();
                        let plural = if count == 1 { "" } else { "s" };
                        ui.label(
                            RichText::new(format!("{count} sound{plural}"))
                                .size(12.5)
                                .color(theme::MUTED),
                        );
                        // Settings (system-tray behaviour).
                        if ui
                            .small_button("⚙")
                            .on_hover_text("Settings")
                            .clicked()
                        {
                            self.settings_open = true;
                        }
                    });
                });

                ui.add_space(4.0);

                // Board switcher: pick the visible board, or manage boards.
                ui.horizontal(|ui| {
                    ui.label(RichText::new("Board").size(13.5).color(theme::MUTED));
                    let before = self.active_board;
                    let mut selected = self.active_board;
                    egui::ComboBox::from_id_salt("board_switcher")
                        .selected_text(self.active().name.clone())
                        .width(180.0)
                        .show_ui(ui, |ui| {
                            for board in &self.boards {
                                ui.selectable_value(&mut selected, board.id, &board.name);
                            }
                        });
                    if selected != before {
                        self.switch_board(selected);
                    }

                    if ui
                        .small_button("+ New")
                        .on_hover_text("Create a new empty board and switch to it")
                        .clicked()
                    {
                        self.add_board();
                    }

                    let active_id = self.active_board;
                    let active_name = self.active().name.clone();
                    if ui
                        .small_button("✎ Rename")
                        .on_hover_text("Rename this board")
                        .clicked()
                    {
                        self.board_dialog = Some(BoardDialog::Rename {
                            id: active_id,
                            name: active_name.clone(),
                        });
                    }

                    let can_delete = self.boards.len() > 1;
                    if ui
                        .add_enabled(
                            can_delete,
                            Button::new(RichText::new("🗑 Delete").size(12.5)),
                        )
                        .on_hover_text(if can_delete {
                            "Delete this board and all its sounds"
                        } else {
                            "You need at least one board"
                        })
                        .clicked()
                    {
                        let sounds = self.active().slots.len();
                        self.board_dialog = Some(BoardDialog::Delete {
                            id: active_id,
                            name: active_name.clone(),
                            sounds,
                        });
                    }
                });
            });
    }

    fn status_bar(&mut self, ui: &mut egui::Ui) {
        // Expire the toast before drawing the bar.
        if let Some(t) = &self.toast {
            if Instant::now() >= t.until {
                self.toast = None;
            }
        }

        let frame = Frame::NONE
            .fill(theme::PANEL)
            .inner_margin(Margin::symmetric(16, 6));

        egui::Panel::bottom("status_bar")
            .frame(frame)
            .show_separator_line(false)
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    match &self.hotkey_state {
                        HotkeyBackendState::Active => {
                            ui.label(
                                RichText::new("●  Listening in the background")
                                    .size(11.5)
                                    .color(theme::GOOD),
                            );
                            ui.label(
                                RichText::new("— minimize me, hotkeys keep working.")
                                    .size(11.5)
                                    .color(theme::MUTED),
                            );
                        }
                        HotkeyBackendState::Starting => {
                            ui.label(
                                RichText::new("○  Starting hotkeys…")
                                    .size(11.5)
                                    .color(theme::MUTED),
                            );
                        }
                        HotkeyBackendState::Unavailable(reason) => {
                            ui.label(
                                RichText::new("⚠  System hotkeys unavailable")
                                    .size(11.5)
                                    .color(theme::WARN),
                            )
                            .on_hover_text(reason);
                            ui.label(
                                RichText::new("— click ▶ to play sounds.")
                                    .size(11.5)
                                    .color(theme::MUTED),
                            );
                        }
                    }
                });

                // The toast gets its own full-width row. Sharing the status
                // line made long messages (error text, device names) collide
                // with the status text instead of wrapping below it.
                if let Some(toast) = &self.toast {
                    ui.add_space(2.0);
                    ui.add(
                        egui::Label::new(
                            RichText::new(&toast.text).size(12.0).color(toast.kind.colour()),
                        )
                        .wrap(),
                    );
                }
            });
    }

    fn grid(&mut self, ui: &mut egui::Ui, actions: &mut Vec<SlotAction>) {
        let frame = Frame::NONE
            .fill(theme::BG)
            .inner_margin(Margin::symmetric(16, 8));

        egui::CentralPanel::no_frame().frame(frame).show(ui, |ui| {
            egui::ScrollArea::vertical()
                .auto_shrink([false, false])
                .show(ui, |ui| {
                    if self.active().slots.is_empty() {
                        ui.vertical_centered(|ui| {
                            ui.add_space(64.0);
                            ui.label(
                                RichText::new("No sounds yet")
                                    .size(20.0)
                                    .strong()
                                    .color(theme::TEXT),
                            );
                            ui.add_space(6.0);
                            ui.label(
                                RichText::new(
                                    "Click \"➕  Add Sound\" to create your first slot.",
                                )
                                .color(theme::MUTED),
                            );
                        });
                        return;
                    }

                    ui.spacing_mut().item_spacing.x = SLOT_GAP;

                    // Work out how many tiles fit per row, then draw explicit
                    // rows. Deterministic layout avoids `horizontal_wrapped`
                    // quirks (stray scrollbars, uneven row gaps).
                    let avail = ui.available_width();
                    let columns = (((avail + SLOT_GAP) / (SLOT_W + SLOT_GAP)).floor() as usize)
                        .max(1);

                    // `Some(..)` for each slot, then a trailing `None` = add tile.
                    let mut tiles: Vec<Option<(SlotId, bool, bool)>> = self
                        .active()
                        .slots
                        .iter()
                        .map(|slot| {
                            Some((
                                slot.id,
                                self.playing.contains(&slot.id),
                                self.recording == Some(slot.id),
                            ))
                        })
                        .collect();
                    tiles.push(None);

                    for row in tiles.chunks(columns) {
                        ui.horizontal(|ui| {
                            for tile in row {
                                match tile {
                                    Some((id, playing, recording)) => {
                                        if let Some(slot) =
                                            self.active().slots.iter().find(|s| s.id == *id)
                                        {
                                            if let Some(action) =
                                                draw_slot(ui, slot, *playing, *recording)
                                            {
                                                actions.push(action);
                                            }
                                        }
                                    }
                                    None => draw_add_tile(ui, actions),
                                }
                            }
                        });
                        ui.add_space(SLOT_GAP);
                    }
                });
        });
    }

    fn confirm_dialog(&mut self, ctx: &egui::Context) {
        let Some(pending) = self.confirm.clone() else {
            return;
        };

        let mut accept = false;
        let mut cancel = false;

        egui::Window::new("confirm")
            .title_bar(false)
            .resizable(false)
            .collapsible(false)
            .anchor(Align2::CENTER_CENTER, Vec2::ZERO)
            .show(ctx, |ui| {
                ui.set_max_width(320.0);
                ui.add_space(4.0);
                ui.label(RichText::new(&pending.message).color(theme::TEXT));
                ui.add_space(14.0);
                ui.horizontal(|ui| {
                    if ui
                        .add(Button::new(RichText::new(&pending.accept).strong()).fill(theme::WARN))
                        .clicked()
                    {
                        accept = true;
                    }
                    if ui.button("Cancel").clicked() {
                        cancel = true;
                    }
                });
            });

        if accept {
            // "Move it here": free the key everywhere else first, so one key
            // always has exactly one owner. ("Use Anyway" just sets it.)
            debug!("Confirm accepted ({}) for slot={}.", pending.accept, pending.slot);
            let freed: Vec<String> = self
                .active()
                .slots
                .iter()
                .filter(|s| s.id != pending.slot && s.hotkey.as_ref() == Some(&pending.hotkey))
                .map(|s| s.name.clone())
                .collect();
            for slot in self.active_mut().slots.iter_mut() {
                if slot.id != pending.slot && slot.hotkey.as_ref() == Some(&pending.hotkey) {
                    slot.hotkey = None;
                }
            }
            self.set_hotkey(pending.slot, pending.hotkey);
            if let Some(first) = freed.first() {
                let extra = freed.len() - 1;
                let tail = if extra > 0 {
                    format!(" (and {extra} more)")
                } else {
                    String::new()
                };
                self.toast(
                    ToastKind::Good,
                    format!("Moved the key from “{first}”{tail}."),
                );
            }
            self.confirm = None;
        } else if cancel {
            debug!("Confirm cancelled for slot={}.", pending.slot);
            self.toast(ToastKind::Info, "Hotkey not changed.");
            self.confirm = None;
        }
    }

    /// Render the rename / delete-board modal, if one is open.
    fn board_dialogs(&mut self, ctx: &egui::Context) {
        let mut save_rename: Option<(BoardId, String)> = None;
        let mut close = false;
        let mut do_delete: Option<BoardId> = None;

        match &mut self.board_dialog {
            None => return,
            Some(BoardDialog::Rename { id, name }) => {
                let id = *id;
                egui::Window::new("rename_board")
                    .title_bar(false)
                    .resizable(false)
                    .collapsible(false)
                    .anchor(Align2::CENTER_CENTER, Vec2::ZERO)
                    .show(ctx, |ui| {
                        ui.set_max_width(280.0);
                        ui.label(RichText::new("Rename board").strong().color(theme::TEXT));
                        ui.add_space(8.0);
                        let input = ui.add_sized(
                            [260.0, 28.0],
                            egui::TextEdit::singleline(name).hint_text("Board name"),
                        );
                        input.request_focus();
                        let enter = ui.input(|i| i.key_pressed(egui::Key::Enter));
                        ui.add_space(8.0);
                        ui.horizontal(|ui| {
                            if ui.button("Save").clicked() || enter {
                                save_rename = Some((id, name.clone()));
                            }
                            if ui.button("Cancel").clicked() {
                                close = true;
                            }
                        });
                    });
            }
            Some(BoardDialog::Delete { id, name, sounds }) => {
                let (id, name, sounds) = (*id, name.clone(), *sounds);
                let mut delete = false;
                egui::Window::new("delete_board")
                    .title_bar(false)
                    .resizable(false)
                    .collapsible(false)
                    .anchor(Align2::CENTER_CENTER, Vec2::ZERO)
                    .show(ctx, |ui| {
                        ui.set_max_width(300.0);
                        ui.label(
                            RichText::new(format!(
                                "Delete “{name}” with {sounds} sound{}? This cannot be undone.",
                                if sounds == 1 { "" } else { "s" }
                            ))
                            .color(theme::TEXT),
                        );
                        ui.add_space(12.0);
                        ui.horizontal(|ui| {
                            if ui
                                .add(
                                    Button::new(RichText::new("Delete").strong())
                                        .fill(theme::DANGER),
                                )
                                .clicked()
                            {
                                delete = true;
                            }
                            if ui.button("Cancel").clicked() {
                                close = true;
                            }
                        });
                    });
                if delete {
                    do_delete = Some(id);
                }
            }
        }

        if close {
            self.board_dialog = None;
        }
        if let Some((id, name)) = save_rename {
            let name = name.trim().to_owned();
            if name.is_empty() {
                self.toast(ToastKind::Warn, "A board needs a name.");
            } else {
                if let Some(board) = self.boards.iter_mut().find(|b| b.id == id) {
                    board.name = name.clone();
                }
                self.board_dialog = None;
                self.mark_dirty();
                self.toast(ToastKind::Good, format!("Board renamed to “{name}”."));
            }
        }
        if let Some(id) = do_delete {
            self.delete_board(id);
        }
    }

    // --- input ------------------------------------------------------------

    /// While a slot awaits a hotkey, capture the next key press.
    ///
    /// Grab suspension (see `suspend_hotkey_grabs`): our own `RegisterHotKey`
    /// grabs swallow matching presses before `winit`/`egui` ever see them, so
    /// an already-bound key could never be *re*-recorded — the old sound would
    /// fire while the badge waited forever. Grabs are therefore dropped when
    /// recording starts and restored when it ends (capture, cancel, or the
    /// slot disappearing).
    fn handle_recording(&mut self, ctx: &egui::Context) {
        let Some(id) = self.recording else {
            return;
        };

        let mut captured: Option<Hotkey> = None;
        let mut cancelled = false;

        ctx.input(|input| {
            for event in &input.events {
                if let egui::Event::Key {
                    key,
                    pressed: true,
                    repeat: false,
                    modifiers,
                    ..
                } = event
                {
                    if *key == egui::Key::Escape {
                        cancelled = true;
                        break;
                    }
                    // Modifiers arrive as their own key presses. Pressing
                    // Ctrl a moment before K would otherwise capture *Ctrl*
                    // as the whole binding, ending recording immediately and
                    // landing a bare-modifier combo that can never be
                    // grabbed. Keep waiting; the real key carries the
                    // modifier state in `modifiers`.
                    if is_modifier_key(*key) {
                        continue;
                    }
                    captured = Some(Hotkey {
                        ctrl: modifiers.ctrl,
                        shift: modifiers.shift,
                        alt: modifiers.alt,
                        mac_cmd: modifiers.mac_cmd,
                        key: key.name().to_string(),
                    });
                    break;
                }
            }
        });

        if cancelled {
            debug!("Record cancelled for slot={id}.");
            self.recording = None;
            self.push_hotkey_rebuild();
            self.toast(ToastKind::Info, "Cancelled.");
        } else if let Some(hotkey) = captured {
            debug!("Recorded key={} for slot={id}.", hotkey.display());
            self.recording = None;
            self.push_hotkey_rebuild();
            // The toolbar's Stop key records through the same flow, addressed
            // by its sentinel id.
            if id == STOP_ALL_ID {
                self.guard_stop_hotkey(hotkey);
            } else {
                self.guard_hotkey(id, hotkey);
            }
        }
    }

    // --- audio events -----------------------------------------------------

    /// Fire-and-forget send to the engine. The channel is unbounded and the
    /// receiver lives as long as the app, so this cannot block or fail in
    /// practice; a failure just means the engine is gone (shutdown).
    fn send(&self, cmd: AudioCommand) {
        let _ = self.audio_tx.send(cmd);
    }

    /// Drain every pending engine event. Called once per frame.
    fn poll_audio_events(&mut self) {
        while let Ok(event) = self.audio_rx.try_recv() {
            match event {
                AudioEvent::Started(id) => {
                    if id == TEST_SOUND_ID {
                        self.testing = true;
                    } else {
                        self.playing.insert(id);
                    }
                }
                AudioEvent::Finished(id) => {
                    if id == TEST_SOUND_ID {
                        self.testing = false;
                    } else {
                        self.playing.remove(&id);
                    }
                }
                AudioEvent::FileMissing { id, path } => {
                    self.playing.remove(&id);
                    if let Some(slot) = self.find_slot_mut(id) {
                        slot.missing = true;
                    }
                    // Ungrab it so further presses don't just re-toast.
                    self.push_hotkey_rebuild();
                    self.toast(
                        ToastKind::Warn,
                        format!(
                            "Can't find {} — did you move or delete it? Use Find Again to re-link it.",
                            path.display()
                        ),
                    );
                }
                AudioEvent::DecodeError { id, message } => {
                    if id == TEST_SOUND_ID {
                        self.testing = false;
                    } else {
                        self.playing.remove(&id);
                        // Ungrab decode-broken files until the user re-links.
                        self.broken.insert(id);
                        self.push_hotkey_rebuild();
                    }
                    self.toast(ToastKind::Warn, message);
                }
                AudioEvent::DeviceReady {
                    requested,
                    actual,
                    fallback,
                } => {
                    if let Some(note) = fallback {
                        // The chosen device vanished; adopt whatever we opened.
                        if !actual.is_empty() {
                            self.output_device = actual;
                        }
                        self.playing.clear();
                        self.testing = false;
                        self.toast(ToastKind::Warn, note);
                    } else if requested != SYSTEM_DEFAULT_LABEL && requested != self.output_device
                    {
                        self.output_device = requested;
                    }
                }
                AudioEvent::StreamError(message) => {
                    // Typically a USB/Bluetooth device unplugged mid-playback
                    // on Windows. The stream is dead; clear the glow and say so.
                    self.playing.clear();
                    self.testing = false;
                    self.toast(ToastKind::Warn, message);
                }
            }
        }
    }

    /// Drain every pending hotkey-thread status event. Called once per frame.
    /// Returns true if anything arrived (caller repaints for the status bar).
    fn poll_hotkey_events(&mut self) -> bool {
        let mut changed = false;
        while let Ok(event) = self.hotkey_rx.try_recv() {
            changed = true;
            match event {
                HotkeyEvent::Ready => {
                    self.hotkey_state = HotkeyBackendState::Active;
                }
                HotkeyEvent::Unavailable(reason) => {
                    self.hotkey_state = HotkeyBackendState::Unavailable(reason.clone());
                    self.toast(ToastKind::Warn, reason);
                }
                HotkeyEvent::KeyRejected { display, reason } => {
                    self.toast(
                        ToastKind::Warn,
                        format!("Could not grab {display}: {reason}"),
                    );
                }
            }
        }
        changed
    }

    // --- persistence ------------------------------------------------------

    /// Flag the board for saving. The actual write happens debounced in
    /// `maybe_autosave` (plus once in `on_exit`), so slider drags and rapid
    /// clicks collapse into a single atomic write.
    fn mark_dirty(&mut self) {
        self.config_dirty = true;
    }

    fn snapshot_state(&self) -> AppState {
        AppState {
            version: STATE_VERSION,
            output_device_name: Some(self.output_device.clone()),
            input_device_name: (self.mic_device != SYSTEM_DEFAULT_LABEL)
                .then(|| self.mic_device.clone()),
            close_to_tray: self.close_to_tray,
            volume: self.volume,
            stop_hotkey: self.stop_hotkey.clone(),
            boards: self.boards.clone(),
            active_board: self.active_board,
        }
    }

    fn save_now(&mut self) {
        match config::save(&self.snapshot_state()) {
            Ok(()) => {
                log::info!(
                    "Saved state ({} boards).",
                    self.boards.len()
                );
                self.config_dirty = false;
                self.last_save = Instant::now();
            }
            Err(err) => {
                // Keep running; retry on the next mutation. Warn loudly in the
                // log, once in the UI (no per-frame spam).
                log::warn!("Could not save config: {err}");
                self.config_dirty = false;
                self.last_save = Instant::now();
                self.toast(
                    ToastKind::Warn,
                    "Could not save your soundboard layout. Changes will be lost on exit.",
                );
            }
        }
    }

    /// Flush pending saves at most every 700ms. Called once per frame.
    fn maybe_autosave(&mut self) {
        if self.config_dirty && self.last_save.elapsed() > Duration::from_millis(700) {
            self.save_now();
        }
    }

    /// Drain recorder events. Returns true if anything arrived.
    /// A saved take becomes a slot on the active board automatically.
    /// (If the recorder thread ever died, its sender vanishes too, so
    /// `try_recv` keeps returning an error and this can never block.)
    fn poll_record_events(&mut self) -> bool {
        let mut changed = false;
        while let Ok(event) = self.record_rx.try_recv() {
            changed = true;
            // Any event means the Start command was consumed: the watchdog
            // no longer applies.
            self.mic_starting = None;
            match event {
                RecordEvent::Started { device } => {
                    self.mic = Some(MicTake { started: Instant::now() });
                    // Naming the device makes "recording silence from the
                    // wrong endpoint" obvious the moment it happens.
                    self.toast(ToastKind::Info, format!("Recording from {device}."));
                }
                RecordEvent::Saved {
                    path,
                    duration_secs,
                    cut_short,
                } => {
                    self.mic = None;
                    let name = Self::display_name_for(&path);
                    let id = self.alloc_id();
                    let mut slot = SoundSlot::with_file(id, name.clone(), path);
                    slot.duration_secs = Some(duration_secs);
                    self.active_mut().slots.push(slot);
                    self.mark_dirty();
                    if cut_short {
                        // The mic died mid-take: say so rather than implying
                        // the user stopped it themselves.
                        self.toast(
                            ToastKind::Warn,
                            format!(
                                "The microphone stopped early — kept {duration_secs:.1}s as \
                                 “{name}”."
                            ),
                        );
                    } else {
                        self.toast(
                            ToastKind::Good,
                            format!("Saved “{name}” ({duration_secs:.1}s) to this board."),
                        );
                    }
                }
                RecordEvent::Failed(message) => {
                    self.mic = None;
                    self.toast(ToastKind::Warn, message);
                }
            }
        }
        changed
    }


    // --- apply ------------------------------------------------------------

    fn apply(&mut self, action: SlotAction) {
        match action {
            SlotAction::AddViaPicker => {
                self.mark_dirty();
                self.add_sound_via_picker();
            }
            SlotAction::Play(id) => {
                // Optimistic highlight: the engine confirms with `Started`
                // (or corrects us with `FileMissing` / `DecodeError`).
                let found = self
                    .active()
                    .slots
                    .iter()
                    .find(|s| s.id == id)
                    .cloned();
                if let Some(slot) = found {
                    if let Some(path) = slot.file_path.clone() {
                        self.playing.insert(id);
                        self.send(AudioCommand::Play {
                            id,
                            path,
                            trim: trim_spec_for(&slot),
                        });
                    } else {
                        self.toast(ToastKind::Info, "Pick a sound file for this slot first.");
                    }
                }
            }
            SlotAction::Stop(id) => {
                self.playing.remove(&id);
                self.send(AudioCommand::Stop(id));
            }
            SlotAction::StartRecording(id) => {
                debug!("Arming hotkey record for slot={id}.");
                self.recording = Some(id);
                self.suspend_hotkey_grabs();
                self.toast(ToastKind::Info, "Press the key you want to use (Esc to cancel).");
            }
            SlotAction::ClearHotkey(id) => {
                if let Some(slot) = self.find_slot_mut(id) {
                    slot.hotkey = None;
                }
                self.push_hotkey_rebuild();
                self.mark_dirty();
                self.toast(ToastKind::Info, "Hotkey removed.");
            }
            SlotAction::Remove(id) => {
                self.active_mut().slots.retain(|s| s.id != id);
                self.playing.remove(&id);
                self.broken.remove(&id);
                if self.recording == Some(id) {
                    self.recording = None;
                    // The doomed slot was being recorded: grabs were suspended
                    // for it, so restore the rest.
                    self.push_hotkey_rebuild();
                }
                self.send(AudioCommand::Stop(id));
                self.push_hotkey_rebuild();
                self.mark_dirty();
            }
            SlotAction::FindAgain(id) => {
                self.mark_dirty();
                self.fill_slot_via_picker(id);
            }
            SlotAction::Edit(id) => self.open_editor(id),
        }
    }

    // --- trim editor ------------------------------------------------------

    /// Open the trim editor for a slot. Missing files must be re-linked first
    /// (there is nothing to preview). Kicks off the background waveform
    /// decode; the modal shows progress until it lands.
    fn open_editor(&mut self, id: SlotId) {
        let found = self
            .active()
            .slots
            .iter()
            .find(|s| s.id == id)
            .map(|s| (s.file_path.clone(), s.missing, s.trim_start, s.trim_end));
        match found {
            Some((Some(path), false, start, end)) => {
                self.waves.request(id, path);
                self.editor = Some(EditorState {
                    slot: id,
                    start_frac: start,
                    end_frac: end,
                    drag: None,
                });
            }
            Some((_, true, _, _)) => {
                self.toast(ToastKind::Info, "Find the file first (Find Again), then trim it.");
            }
            _ => {}
        }
    }

    /// Drain finished waveform decodes. Drops responses for files that were
    /// re-linked mid-decode. Returns true if anything arrived.
    fn poll_waves(&mut self) -> bool {
        let mut changed = false;
        while let Ok((id, path, result)) = self.waves.rx.try_recv() {
            changed = true;
            self.waves.pending.remove(&id);
            let current = self
                .boards
                .iter()
                .flat_map(|b| b.slots.iter())
                .find(|s| s.id == id)
                .and_then(|s| s.file_path.clone());
            if current.as_ref() == Some(&path) {
                self.waves.ready.insert(id, result);
            }
        }
        changed
    }

    /// Render the trim editor modal, if open.
    fn editor_dialog(&mut self, ctx: &egui::Context) {
        let Some(state) = self.editor.clone() else {
            return;
        };
        let slot = self
            .boards
            .iter()
            .flat_map(|b| b.slots.iter())
            .find(|s| s.id == state.slot)
            .cloned();
        let Some(slot) = slot else {
            // Slot deleted while editing.
            self.editor = None;
            return;
        };

        let mut close = false;
        let mut save: Option<(f32, f32, f32)> = None;
        let mut preview = false;
        let mut reset = false;

        egui::Window::new("trim_editor")
            .title_bar(false)
            .resizable(false)
            .collapsible(false)
            .anchor(Align2::CENTER_CENTER, Vec2::ZERO)
            .show(ctx, |ui| {
                ui.set_max_width(340.0);
                ui.label(
                    RichText::new(format!("Trim “{}”", slot.name))
                        .size(15.0)
                        .strong()
                        .color(theme::TEXT),
                );
                ui.add_space(4.0);
                ui.label(
                    RichText::new("Drag the blue edges to keep only part of the sound.")
                        .size(11.5)
                        .color(theme::MUTED),
                );
                ui.add_space(8.0);

                match self.waves.ready.get(&state.slot).cloned() {
                    None => {
                        ui.label(RichText::new("Loading sound…").color(theme::MUTED));
                        ui.add_space(100.0);
                    }
                    Some(Err(message)) => {
                        ui.label(RichText::new(message).color(theme::DANGER));
                        ui.add_space(8.0);
                    }
                    Some(Ok(wave)) => {
                        let response = draw_waveform(ui, &wave, state.start_frac, state.end_frac);
                        // Resolve the drag against the live editor state.
                        if let Some(editor) = self.editor.as_mut() {
                            if response.drag_started() {
                                let x = response.interact_pointer_pos().map(|p| {
                                    (p.x - response.rect.left()) / response.rect.width()
                                });
                                if let Some(x) = x {
                                    let ds = (x - editor.start_frac).abs();
                                    let de = (x - editor.end_frac).abs();
                                    editor.drag =
                                        Some(if ds <= de { EditorHandle::Start } else { EditorHandle::End });
                                }
                            }
                            if response.dragged() {
                                if let (Some(pointer), Some(handle)) =
                                    (response.interact_pointer_pos(), editor.drag)
                                {
                                    let frac = ((pointer.x - response.rect.left())
                                        / response.rect.width())
                                    .clamp(0.0, 1.0);
                                    match handle {
                                        EditorHandle::Start => {
                                            editor.start_frac = frac.min(editor.end_frac - MIN_TRIM_GAP);
                                            editor.start_frac = editor.start_frac.max(0.0);
                                        }
                                        EditorHandle::End => {
                                            editor.end_frac = frac.max(editor.start_frac + MIN_TRIM_GAP);
                                            editor.end_frac = editor.end_frac.min(1.0);
                                        }
                                    }
                                }
                            }
                            if response.drag_stopped() {
                                editor.drag = None;
                            }
                        }

                        let (start, end) = (state.start_frac, state.end_frac);
                        ui.add_space(4.0);
                        ui.horizontal(|ui| {
                            ui.label(
                                RichText::new(format!(
                                    "Start {}  •  Keep {}  •  End {}",
                                    fmt_time(start * wave.duration_secs),
                                    fmt_time((end - start).max(0.0) * wave.duration_secs),
                                    fmt_time(end * wave.duration_secs),
                                ))
                                .size(11.5)
                                .color(theme::MUTED),
                            );
                        });
                        ui.add_space(8.0);
                        ui.horizontal(|ui| {
                            if ui.button("▶  Play").clicked() {
                                preview = true;
                            }
                            if ui.button("Reset").clicked() {
                                reset = true;
                            }
                            if ui.button("Save").clicked() {
                                save = Some((start, end, wave.duration_secs));
                            }
                            if ui.button("Cancel").clicked() {
                                close = true;
                            }
                        });
                    }
                }

                // Error state still needs a way out.
                if !matches!(self.waves.ready.get(&state.slot), Some(Ok(_))) {
                    ui.horizontal(|ui| {
                        if ui.button("Close").clicked() {
                            close = true;
                        }
                    });
                }
            });

        if close {
            self.editor = None;
        }
        if reset {
            if let Some(editor) = self.editor.as_mut() {
                editor.start_frac = 0.0;
                editor.end_frac = 1.0;
            }
        }
        if preview {
            self.preview_trim(state.slot, state.start_frac, state.end_frac);
        }
        if let Some((start, end, duration)) = save {
            if let Some(slot) = self.find_slot_mut(state.slot) {
                slot.trim_start = start;
                slot.trim_end = end;
                slot.duration_secs = Some(duration);
            }
            self.editor = None;
            self.push_hotkey_rebuild();
            self.mark_dirty();
            self.toast(ToastKind::Good, "Trim saved.");
        }
    }

    /// Play the editor's current bounds (not yet saved) for preview.
    fn preview_trim(&mut self, id: SlotId, start_frac: f32, end_frac: f32) {
        let duration_secs = match self.waves.ready.get(&id).cloned() {
            Some(Ok(wave)) => wave.duration_secs,
            _ => {
                self.toast(ToastKind::Info, "Still loading — try again in a moment.");
                return;
            }
        };
        let path = self
            .boards
            .iter()
            .flat_map(|b| b.slots.iter())
            .find(|s| s.id == id)
            .and_then(|s| s.file_path.clone());
        if let Some(path) = path {
            self.playing.insert(id);
            self.send(AudioCommand::Play {
                id,
                path,
                trim: Some(TrimSpec {
                    start_frac,
                    end_frac,
                    duration_secs,
                }),
            });
        }
    }
    // --- file picking -------------------------------------------------------
    //
    // `rfd` shows the *native* dialog on each OS (Win32 picker on Windows,
    // portal on this Linux dev box). Same call, no `cfg` needed — which is
    // exactly why the Windows target can be developed from Linux here.

    /// The native "choose a sound" dialog. `None` = the user cancelled.
    ///
    /// Note: `.opus` is deliberately absent. `rodio 0.23` ships no Opus codec
    /// (only mp3/wav/vorbis-ogg/flac/aac), so offering it would be a lie —
    /// every pick would end in a decode-error toast. Revisit if upstream adds
    /// Opus or we add a dedicated Opus pipeline (a later-phase option).
    fn pick_sound_file() -> Option<PathBuf> {
        rfd::FileDialog::new()
            .set_title("Choose a sound")
            .add_filter("Audio", &["mp3", "wav", "ogg", "flac"])
            .pick_file()
    }

    /// Friendly tile name from a path: `party-horn.mp3` → `party-horn`.
    fn display_name_for(path: &std::path::Path) -> String {
        path.file_stem()
            .and_then(|stem| stem.to_str())
            .map(|stem| ellipsize(stem, 18))
            .filter(|stem| !stem.is_empty())
            .unwrap_or_else(|| "Sound".to_owned())
    }

    /// Fill an existing slot from the picker (new sound or re-link).
    fn fill_slot_via_picker(&mut self, id: SlotId) {
        let Some(path) = Self::pick_sound_file() else {
            return; // cancelled — leave the slot untouched
        };
        let name = Self::display_name_for(&path);
        if let Some(slot) = self.find_slot_mut(id) {
            slot.name = name.clone();
            slot.file_path = Some(path);
            slot.missing = false;
            // A new file means new audio: old trim bounds, duration, and
            // waveform no longer describe this slot.
            slot.clear_trim();
        }
        self.waves.invalidate(id);
        // Re-linking clears any decode-broken flag and re-arms the grab.
        self.broken.remove(&id);
        self.push_hotkey_rebuild();
        self.toast(ToastKind::Good, format!("Added \"{name}\". Click ▶ to hear it."));
    }

    /// "Add Sound" flow: pick a file first, then append a filled slot.
    /// Cancelling the dialog adds nothing (no stray empty tiles).
    fn add_sound_via_picker(&mut self) {
        let Some(path) = Self::pick_sound_file() else {
            return;
        };
        let name = Self::display_name_for(&path);
        let id = self.alloc_id();
        self.active_mut().slots.push(SoundSlot::with_file(id, name.clone(), path));
        self.toast(ToastKind::Good, format!("Added \"{name}\". Click ▶ to hear it."));
    }

    /// Decide when the next frame is needed, instead of spinning.
    ///
    /// Unconditional `request_repaint()` per frame pegs a CPU core (measured
    /// 1000+ FPS on the dev box). Each reason below declares the slowest rate
    /// at which it still looks right, and we take the fastest requirement:
    ///
    /// - a tile glow pulse animates, so ~30 FPS is plenty;
    /// - the recording timer shows tenths of a second, so 10 FPS is enough;
    /// - a toast only needs *one* wake-up, when it expires;
    /// - the mic-start watchdog needs one wake-up, at its deadline.
    fn schedule_repaint(&self, ctx: &egui::Context) {
        // Every reason below schedules its own deadline, and they all apply:
        // egui keeps the *earliest* pending repaint (`repaint_delay` is only
        // lowered, never raised), so no early `return` — one would silently
        // shadow another reason's wake-up. Example that bit: a toast on
        // screen during a mic start would return first and drop the watchdog
        // deadline, so the watchdog could only fire on some later, unrelated
        // repaint.

        // Animated glow on playing tiles: smooth enough at 30 FPS.
        if !self.playing.is_empty() || self.testing {
            ctx.request_repaint_after(TILE_ANIMATION_FRAME);
        }
        // Live take timer (`0:03.2`): 10 FPS reads as continuous.
        if self.mic.is_some() {
            ctx.request_repaint_after(TIMER_FRAME);
        }
        // Toast: wake once, when it is due to disappear.
        if let Some(toast) = &self.toast {
            let remaining = toast.until.saturating_duration_since(Instant::now());
            if remaining > Duration::ZERO {
                ctx.request_repaint_after(remaining);
            }
        }
        // Mic-start watchdog: wake once, at its deadline.
        if let Some(since) = self.mic_starting {
            let remaining = MIC_START_TIMEOUT.saturating_sub(since.elapsed());
            ctx.request_repaint_after(remaining);
        }
    }

}

impl eframe::App for SoundboardApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();

        // Everything with a side effect (polling threads, tray, timers,
        // autosave) already ran in `logic`. This pass only paints.
        let mut actions: Vec<SlotAction> = Vec::new();

        self.top_bar(ui);
        self.toolbar(ui);
        self.status_bar(ui);
        self.grid(ui, &mut actions);
        self.settings_window(&ctx);
        self.confirm_dialog(&ctx);
        self.board_dialogs(&ctx);
        self.editor_dialog(&ctx);

        for action in actions {
            self.apply(action);
        }
    }

    /// Runs before every `ui` pass **and** while the window is hidden.
    ///
    /// This split is what makes the tray usable: eframe skips the whole egui
    /// pass for a hidden viewport (`ui` is never called), so a tray click
    /// handled from inside `ui` could never unhide the window — the app would
    /// be permanently stuck in the tray. `logic` is the documented hook for
    /// exactly this, and it must not paint, which is why nothing below draws.
    fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.poll_audio_events();
        if self.poll_hotkey_events() {
            ctx.request_repaint();
        }
        if self.poll_waves() {
            ctx.request_repaint();
        }
        if self.poll_record_events() {
            ctx.request_repaint();
        }

        // Watchdog: a `Start` that gets no answer within 8s means the device
        // open wedged inside cpal (activation has its own 5s timeout; this
        // covers any other blocking call). The old recorder thread is
        // abandoned — its channels die with it — and a fresh one makes the
        // Record button work again instead of dead-ending forever.
        if let Some(since) = self.mic_starting {
            if since.elapsed() > MIC_START_TIMEOUT {
                log::warn!("Mic start timed out; restarting the recorder thread.");
                let (tx, rx) = spawn_recorder();
                self.record_tx = tx;
                self.record_rx = rx;
                self.mic_starting = None;
                self.toast(
                    ToastKind::Warn,
                    "The microphone took too long to open — it may be in use by another app. \
                     Try again, or pick a different microphone.",
                );
                ctx.request_repaint();
            }
        }

        // Tray: click/menu events, and the close-to-tray interception. This
        // MUST be here rather than in `ui`, or a hidden window could never be
        // reopened (see the doc comment above).
        self.handle_tray(ctx);

        // Hotkey recording needs real input events, which only exist for a
        // shown window; skipping it here is correct, not an oversight.
        if self.window_visible {
            self.handle_recording(ctx);
        }

        self.schedule_repaint(ctx);
        self.maybe_autosave();
    }

    /// Flush unsaved changes on the way out so the last 700ms window is never
    /// lost on a deliberate quit.
    fn on_exit(&mut self) {
        if self.config_dirty {
            self.save_now();
        }
    }
}

// --- drawing helpers ------------------------------------------------------

/// Minimum kept slice, as a fraction (keeps the handles from crossing).
const MIN_TRIM_GAP: f32 = 0.02;

/// Editor canvas geometry.
const WAVE_W: f32 = 300.0;
const WAVE_H: f32 = 110.0;

/// Paint the waveform with shaded cut-aways and start/end handles.
/// Returns the response for drag handling by the caller.
fn draw_waveform(ui: &mut egui::Ui, wave: &Waveform, start_frac: f32, end_frac: f32) -> egui::Response {
    let (rect, response) =
        ui.allocate_exact_size(Vec2::new(WAVE_W, WAVE_H), Sense::drag());

    ui.painter().rect(
        rect,
        CornerRadius::same(8),
        theme::BG_SOFT,
        Stroke::new(1.0, theme::BORDER),
        egui::StrokeKind::Inside,
    );

    let mid_y = rect.center().y;
    let half_h = rect.height() / 2.0 - 8.0;
    let x_of = |frac: f32| rect.left() + frac.clamp(0.0, 1.0) * rect.width();

    // Peaks, one segment per bucket; dimmed outside the kept range.
    let mut kept: Vec<[egui::Pos2; 2]> = Vec::new();
    let mut cut: Vec<[egui::Pos2; 2]> = Vec::new();
    for (i, (min, max)) in wave.peaks.iter().enumerate() {
        let frac = (i as f32 + 0.5) / wave.peaks.len() as f32;
        let x = x_of(frac);
        let y_top = mid_y - max.clamp(-1.0, 1.0) * half_h;
        let y_bot = mid_y - min.clamp(-1.0, 1.0) * half_h;
        let segment = [egui::pos2(x, y_top), egui::pos2(x, y_bot.max(y_top + 1.0))];
        if frac >= start_frac && frac <= end_frac {
            kept.push(segment);
        } else {
            cut.push(segment);
        }
    }
    for segment in cut {
        ui.painter().line_segment(segment, Stroke::new(1.0, theme::MUTED));
    }
    for segment in kept {
        ui.painter().line_segment(segment, Stroke::new(1.0, theme::TEXT));
    }

    // Handles: blue edge bars at the kept boundaries.
    for frac in [start_frac, end_frac] {
        let x = x_of(frac);
        ui.painter().rect(
            egui::Rect::from_min_max(
                egui::pos2(x - 4.0, rect.top()),
                egui::pos2(x + 4.0, rect.bottom()),
            ),
            CornerRadius::same(3),
            theme::ACCENT,
            Stroke::NONE,
            egui::StrokeKind::Inside,
        );
    }

    response.on_hover_cursor(egui::CursorIcon::ResizeHorizontal)
}

/// Is this key press just a modifier going down on its own?
///
/// While recording a binding, Ctrl/Shift/Alt/Super are held *before* the real
/// key, so egui delivers their press events first. They must not be captured
/// as the binding itself — the real key carries the state in `modifiers`.
fn is_modifier_key(key: egui::Key) -> bool {
    use egui::Key::*;
    matches!(
        key,
        ControlLeft
            | ControlRight
            | ShiftLeft
            | ShiftRight
            | AltLeft
            | AltRight
            | SuperLeft
            | SuperRight
    )
}

/// Seconds as `M:SS.d` (e.g. `0:03.2`).
fn fmt_time(secs: f32) -> String {
    let secs = secs.max(0.0);
    format!("{}:{:04.1}", secs as u32 / 60, secs % 60.0)
}

/// Shorten a device name for the compact mic picker.
/// `Speakers (Realtek(R) Audio)` → `Speakers (Realtek(R…)`. Full name in hover.
fn shorten_device(name: &str) -> String {
    let max = 22;
    if name.chars().count() <= max {
        return name.to_owned();
    }
    let cut: String = name.chars().take(max.saturating_sub(1)).collect();
    format!("{cut}…")
}

fn draw_slot(
    ui: &mut egui::Ui,
    slot: &SoundSlot,
    playing: bool,
    recording: bool,
) -> Option<SlotAction> {
    let mut action: Option<SlotAction> = None;

    // Reserve the tile's space *exactly*, then paint and lay out inside that
    // rect. This keeps the wrapped grid perfectly aligned (a `Frame` sized to
    // its content staggers tiles inside `horizontal_wrapped`).
    let (rect, response) = ui.allocate_exact_size(Vec2::new(SLOT_W, SLOT_H), Sense::hover());

    let (fill, stroke) = slot_colors(ui, slot, playing, recording);
    ui.painter().rect(
        rect,
        CornerRadius::same(14),
        fill,
        stroke,
        egui::StrokeKind::Inside,
    );

    let inner = rect.shrink(SLOT_PAD);

    ui.scope_builder(
        egui::UiBuilder::new()
            .max_rect(inner)
            .layout(Layout::top_down(Align::Min)),
        |ui| {
            // Header: name + edit + remove.
            ui.horizontal(|ui| {
                ui.label(
                    RichText::new(ellipsize(&slot.name, 14))
                        .size(14.5)
                        .strong()
                        .color(theme::TEXT),
                );
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    if ui
                        .small_button(RichText::new("✕").color(theme::MUTED))
                        .on_hover_text("Remove this sound")
                        .clicked()
                    {
                        action = Some(SlotAction::Remove(slot.id));
                    }
                    if ui
                        .small_button(RichText::new("✎").color(theme::MUTED))
                        .on_hover_text("Trim this sound")
                        .clicked()
                    {
                        action = Some(SlotAction::Edit(slot.id));
                    }
                });
            });

            ui.add_space(6.0);

            if slot.missing {
                ui.label(
                    RichText::new("⚠  Can't find this file")
                        .size(12.5)
                        .color(theme::DANGER),
                );
                ui.add_space(6.0);
                let width = ui.available_width();
                if ui
                    .add_sized(
                        [width, 32.0],
                        Button::new(RichText::new("Find Again").color(theme::TEXT)),
                    )
                    .clicked()
                {
                    action = Some(SlotAction::FindAgain(slot.id));
                }
            } else {
                let label = if playing { "⏹   Stop" } else { "▶   Play" };
                let fill = if playing { theme::GOOD_SOFT } else { theme::ACCENT };
                let width = ui.available_width();
                if ui
                    .add_sized(
                        [width, 36.0],
                        Button::new(RichText::new(label).strong().color(Color32::WHITE))
                            .fill(fill)
                            .corner_radius(CornerRadius::same(8)),
                    )
                    .clicked()
                {
                    action = Some(if playing {
                        SlotAction::Stop(slot.id)
                    } else {
                        SlotAction::Play(slot.id)
                    });
                }
            }

            ui.add_space(6.0);

            // Hotkey badge: click to (re)bind, right-click to clear.
            let badge = hotkey_badge(ui, slot, recording);
            if recording {
                // Ignore clicks while we wait for a key press.
            } else if badge.clicked() {
                action = Some(SlotAction::StartRecording(slot.id));
            } else if badge.secondary_clicked() && slot.hotkey.is_some() {
                action = Some(SlotAction::ClearHotkey(slot.id));
            }
        },
    );

    response.on_hover_cursor(egui::CursorIcon::PointingHand);

    action
}

fn hotkey_badge(ui: &mut egui::Ui, slot: &SoundSlot, recording: bool) -> egui::Response {
    let (text, fill, stroke, colour) = if recording {
        (
            "Press keys…  (Esc to cancel)".to_owned(),
            theme::ACCENT_SOFT,
            Stroke::new(1.5, theme::ACCENT),
            theme::TEXT,
        )
    } else {
        match &slot.hotkey {
            Some(hotkey) => (
                hotkey.display(),
                theme::BG_SOFT,
                Stroke::new(1.0, theme::BORDER),
                theme::TEXT,
            ),
            None => (
                "Click to set a hotkey".to_owned(),
                theme::BG_SOFT,
                Stroke::new(1.0, theme::BORDER),
                theme::MUTED,
            ),
        }
    };

    let response = ui.add_sized(
        [ui.available_width(), 30.0],
        Button::new(RichText::new(text).size(12.5).color(colour))
            .fill(fill)
            .stroke(stroke)
            .corner_radius(CornerRadius::same(8)),
    );

    if !recording && slot.hotkey.is_some() {
        response.on_hover_text("Click to change  •  Right-click to remove")
    } else {
        response
    }
}

fn draw_add_tile(ui: &mut egui::Ui, actions: &mut Vec<SlotAction>) {
    let (rect, response) = ui.allocate_exact_size(Vec2::new(SLOT_W, SLOT_H), Sense::click());
    ui.painter().rect(
        rect,
        CornerRadius::same(14),
        Color32::TRANSPARENT,
        Stroke::new(1.5, theme::BORDER),
        egui::StrokeKind::Inside,
    );

    let inner = rect.shrink(SLOT_PAD);
    ui.scope_builder(
        egui::UiBuilder::new()
            .max_rect(inner)
            .layout(Layout::top_down(Align::Center)),
        |ui| {
            ui.add_space(26.0);
            ui.label(RichText::new("➕").size(26.0).color(theme::ACCENT));
            ui.add_space(2.0);
            ui.label(
                RichText::new("Add Sound")
                    .size(13.0)
                    .color(theme::MUTED),
            );
        },
    );

    if response.clicked() {
        actions.push(SlotAction::AddViaPicker);
    }
    response.on_hover_cursor(egui::CursorIcon::PointingHand);
}

/// Pick the tile's fill + border from its state, with a pulsing green border
/// while it is playing.
fn slot_colors(
    ui: &egui::Ui,
    slot: &SoundSlot,
    playing: bool,
    recording: bool,
) -> (Color32, Stroke) {
    if recording {
        return (theme::ACCENT_SOFT, Stroke::new(2.0, theme::ACCENT));
    }
    if slot.missing {
        return (theme::BG_SOFT, Stroke::new(1.5, theme::DANGER));
    }
    if playing {
        let time = ui.ctx().input(|i| i.time) as f32;
        let alpha = 0.55 + 0.45 * (time * 5.0).sin();
        let pulse = Color32::from_rgba_unmultiplied(0x22, 0xc5, 0x5e, (alpha * 255.0) as u8);
        return (theme::GOOD_TINT, Stroke::new(2.5, pulse));
    }
    (theme::BG_SOFT, Stroke::new(1.0, theme::BORDER))
}

/// Trim snapshot for playback commands: `Some` only when the slot is actually
/// trimmed *and* its exact duration is known. Otherwise the engine plays whole.
fn trim_spec_for(slot: &SoundSlot) -> Option<TrimSpec> {
    let duration_secs = slot.duration_secs?;
    if !slot.is_trimmed() {
        return None;
    }
    Some(TrimSpec {
        start_frac: slot.trim_start,
        end_frac: slot.trim_end,
        duration_secs,
    })
}

/// Trim a long name to `max` characters with an ellipsis.
fn ellipsize(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        text.to_owned()
    } else {
        let truncated: String = text.chars().take(max.saturating_sub(1)).collect();
        format!("{truncated}…")
    }
}
