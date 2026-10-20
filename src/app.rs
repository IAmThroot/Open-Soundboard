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
use crate::hotkey::{
    spawn_hotkey_thread, to_global_hotkeys, HotkeyBinding, HotkeyCommand, HotkeyEvent, STOP_ALL_ID,
};
use crate::model::{Board, BoardId, Hotkey, SlotId, SoundSlot};
use crate::record::{spawn_recorder, RecordCommand, RecordEvent};
use crate::theme;
use crate::tray::{Tray, TrayAction};
use crate::waveform::{build_waveform, Waveform};

const PREVIEW_TRIM_ID: SlotId = u64::MAX - 2;

const SLOT_W: f32 = 200.0;
const SLOT_H: f32 = 156.0;
const SLOT_PAD: f32 = 12.0;
const SLOT_GAP: f32 = 12.0;

enum SlotAction {
    AddViaPicker,
    Play(SlotId),
    Stop(SlotId),
    StartRecording(SlotId),
    ClearHotkey(SlotId),
    Remove(SlotId),

    Edit(SlotId),

    FindAgain(SlotId),
}

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
    boards: Vec<Board>,
    active_board: BoardId,

    output_devices: Vec<String>,
    output_device: String,
    volume: f32,

    audio_tx: Sender<AudioCommand>,
    audio_rx: Receiver<AudioEvent>,

    hotkey_tx: Sender<HotkeyCommand>,
    hotkey_rx: Receiver<HotkeyEvent>,
    hotkey_state: HotkeyBackendState,

    record_tx: Sender<RecordCommand>,
    record_rx: Receiver<RecordEvent>,

    mic: Option<MicTake>,

    mic_starting: Option<Instant>,

    /// Board that was active when the user pressed Record; the finished take
    /// is added there even if they switch boards while recording.
    mic_start_board: Option<BoardId>,

    mic_devices: Vec<String>,
    mic_device: String,

    stop_hotkey: Option<Hotkey>,

    config_dirty: bool,
    last_save: Instant,
    save_backoff: Duration,
    save_error: Option<String>,
    read_only: bool,

    save_tx: Sender<config::SaveCommand>,
    save_rx: Receiver<config::SaveResult>,
    save_in_flight: bool,
    edit_generation: u64,

    ctx: egui::Context,

    broken: HashSet<SlotId>,

    next_slot_id: SlotId,

    recording: Option<SlotId>,

    playing: HashSet<SlotId>,

    /// The trim editor's preview uses `PREVIEW_TRIM_ID`; it is tracked
    /// separately so Stop All can enable/disable correctly.
    preview_playing: bool,

    testing: bool,

    confirm: Option<Confirm>,
    toast: Option<Toast>,

    board_dialog: Option<BoardDialog>,

    editor: Option<EditorState>,

    waves: WaveCache,

    rejected_hotkeys: HashSet<String>,
    notified_rejected_hotkeys: HashSet<String>,

    tray: Option<Tray>,

    close_to_tray: bool,

    settings_open: bool,

    quit_requested: bool,

    window_visible: bool,
}

#[derive(Clone)]
struct EditorState {
    slot: SlotId,
    start_frac: f32,
    end_frac: f32,

    drag: Option<EditorHandle>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum EditorHandle {
    Start,
    End,
}

struct WaveCache {
    ready: HashMap<SlotId, Result<Waveform, String>>,
    order: Vec<SlotId>,
    pending: HashSet<SlotId>,
    req_tx: crossbeam_channel::Sender<(SlotId, PathBuf)>,
    res_rx: crossbeam_channel::Receiver<(SlotId, PathBuf, Result<Waveform, String>)>,
}

const MAX_CACHED_WAVEFORMS: usize = 16;

impl WaveCache {
    fn new(ctx: &egui::Context) -> Self {
        let (req_tx, req_rx) = crossbeam_channel::unbounded::<(SlotId, PathBuf)>();
        let (res_tx, res_rx) = crossbeam_channel::unbounded();
        let ctx = ctx.clone();

        std::thread::Builder::new()
            .name("waveform-worker".to_owned())
            .spawn(move || {
                while let Ok((id, path)) = req_rx.recv() {
                    let result = match std::panic::catch_unwind(|| build_waveform(&path)) {
                        Ok(r) => r,
                        Err(_) => Err("Waveform decoder thread panicked".to_string()),
                    };
                    let _ = res_tx.send((id, path, result));
                    ctx.request_repaint();
                }
            })
            .expect("failed to spawn waveform worker thread");

        Self {
            ready: HashMap::new(),
            order: Vec::new(),
            pending: HashSet::new(),
            req_tx,
            res_rx,
        }
    }

    fn request(&mut self, id: SlotId, path: PathBuf) {
        if self.ready.contains_key(&id) || !self.pending.insert(id) {
            return;
        }

        while self.order.len() >= MAX_CACHED_WAVEFORMS {
            let evict = self.order.remove(0);
            self.ready.remove(&evict);
        }

        let _ = self.req_tx.send((id, path));
    }

    fn touch(&mut self, id: SlotId) {
        if self.ready.contains_key(&id) {
            self.order.retain(|&x| x != id);
            self.order.push(id);
        }
    }

    fn invalidate(&mut self, id: SlotId) {
        self.ready.remove(&id);
        self.order.retain(|&x| x != id);
        self.pending.remove(&id);
    }
}

enum BoardDialog {
    Rename {
        id: BoardId,
        name: String,
        focused: bool,
    },
    Delete {
        id: BoardId,
        name: String,
        sounds: usize,
    },
}

const AUTHOR_URL: &str = "https://github.com/IAmThroot";
const AUTHOR_CREDIT: &str = "Built by IAmThroot";

const TILE_ANIMATION_FRAME: Duration = Duration::from_millis(33);

const TIMER_FRAME: Duration = Duration::from_millis(100);

const MIC_START_TIMEOUT: Duration = Duration::from_secs(8);

struct MicTake {
    started: Instant,
    board: BoardId,
}

#[derive(Clone)]
enum HotkeyBackendState {
    Starting,

    Active,

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

        let mut mic_devices = crate::devices::list_input_devices();
        let mut mic_device = SYSTEM_DEFAULT_LABEL.to_owned();
        let mut close_to_tray = false;

        let mut boards: Vec<Board> = vec![Board::empty(1, "My Sounds")];
        let mut active_board: BoardId = 1;
        let mut read_only = false;
        let mut save_error = None;

        match config::load() {
            config::LoadOutcome::Loaded(saved) => {
                volume = saved.volume;
                stop_hotkey = saved.stop_hotkey;
                boards = saved.boards;
                active_board = saved.active_board;
                if let Some(saved_device) = saved.output_device_name {
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
                log::info!("Restored {} boards from config.", boards.len());
            }
            config::LoadOutcome::CleanFresh => {
                log::info!("No existing config; starting fresh.");
            }
            config::LoadOutcome::ReadError(err) => {
                log::error!("Config read error: {err}. Starting in read-only mode to prevent overwriting existing data.");
                read_only = true;
                save_error = Some(format!("Could not read configuration file: {err}. Saving is disabled to protect your files."));
            }
        }

        let (audio_tx, audio_rx) =
            spawn_audio_engine(output_device.clone(), volume, cc.egui_ctx.clone());

        let (hotkey_tx, hotkey_rx) = spawn_hotkey_thread(audio_tx.clone(), cc.egui_ctx.clone());

        let (record_tx, record_rx) = spawn_recorder(cc.egui_ctx.clone());

        let (save_tx, save_rx) = config::spawn_saver(cc.egui_ctx.clone());

        let next_slot_id = config::next_slot_id(&boards);

        let mut app = Self {
            ctx: cc.egui_ctx.clone(),
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
            mic_start_board: None,
            mic_devices,
            mic_device,
            stop_hotkey,
            config_dirty: false,
            last_save: Instant::now(),
            save_backoff: Duration::from_millis(700),
            save_error,
            read_only,
            save_tx,
            save_rx,
            save_in_flight: false,
            edit_generation: 0,
            broken: HashSet::new(),
            next_slot_id,
            recording: None,
            playing: HashSet::new(),
            preview_playing: false,
            testing: false,
            confirm: None,
            toast: None,
            board_dialog: None,
            editor: None,
            waves: WaveCache::new(&cc.egui_ctx),
            rejected_hotkeys: HashSet::new(),
            notified_rejected_hotkeys: HashSet::new(),
            tray: Tray::new(&cc.egui_ctx),
            close_to_tray,
            settings_open: false,
            quit_requested: false,
            window_visible: true,
        };

        if app.read_only {
            app.toast(
                ToastKind::Warn,
                "Warning: Existing config could not be read. Saving is disabled to protect your files.",
            );
        }

        app.push_hotkey_rebuild();

        {
            const SIZE: u32 = 64;
            cc.egui_ctx
                .send_viewport_cmd(egui::ViewportCommand::Icon(Some(std::sync::Arc::new(
                    egui::IconData {
                        rgba: crate::icon_art::draw(SIZE),
                        width: SIZE,
                        height: SIZE,
                    },
                ))));
        }
        app
    }

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
                        self.quit_requested = true;
                        ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                    }
                }
            }
        }

        let close_requested = ctx.input(|i| i.viewport().close_requested());

        if close_requested && !self.quit_requested && self.close_to_tray && self.tray.is_some() {
            ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
            ctx.send_viewport_cmd(egui::ViewportCommand::Visible(false));
            self.window_visible = false;

            if self.recording.is_some() {
                self.recording = None;
                self.push_hotkey_rebuild();
                self.toast(
                    ToastKind::Info,
                    "Hotkey recording cancelled — window hidden to tray.",
                );
            }

            self.stop_preview();

            if self.config_dirty {
                self.save_now();
            }
            log::info!("Window hidden; Open Soundboard is in the system tray.");
        }
    }

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


                ui.add_enabled_ui(self.tray.is_some(), |ui| {
                    if ui
                        .checkbox(
                            &mut self.close_to_tray,
                            "Close the window → hide to the system tray",
                        )
                        .changed()
                    {
                        self.mark_dirty();
                    }
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
        let id = self.next_slot_id;
        self.next_slot_id = id.saturating_add(1);
        id
    }

    fn alloc_board_id(&self) -> BoardId {
        config::next_board_id(&self.boards)
    }

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

    fn find_slot_mut(&mut self, id: SlotId) -> Option<&mut SoundSlot> {
        self.boards
            .iter_mut()
            .flat_map(|b| b.slots.iter_mut())
            .find(|s| s.id == id)
    }

    fn toast(&mut self, kind: ToastKind, text: impl Into<String>) {
        self.toast = Some(Toast {
            text: text.into(),
            kind,
            until: Instant::now() + Duration::from_secs(4),
        });
        self.ctx.request_repaint();
    }

    fn set_hotkey(&mut self, id: SlotId, hotkey: Hotkey) {
        let display = hotkey.display();
        debug!("Hotkey stored: slot={id} key={display}.");
        let risky = hotkey.looks_like_typing();
        let old_key = self.find_slot_mut(id).and_then(|s| {
            let old = s.hotkey.take();
            s.hotkey = Some(hotkey);
            old
        });
        if let Some(old) = old_key {
            self.notified_rejected_hotkeys.remove(&old.display());
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

    fn push_hotkey_rebuild(&mut self) {
        self.rejected_hotkeys.clear();
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

    fn switch_board(&mut self, id: BoardId) {
        if id == self.active_board {
            return;
        }
        if !self.boards.iter().any(|b| b.id == id) {
            return;
        }
        if self.recording.take().is_some() {
            self.toast(
                ToastKind::Info,
                "Hotkey recording cancelled — you switched boards.",
            );
        }
        // A pending hotkey confirmation targets a slot on the old board; letting
        // it survive the switch would apply the key to a board that is no longer
        // bound.
        self.confirm = None;
        self.playing.clear();
        self.preview_playing = false;
        self.testing = false;
        self.send(AudioCommand::StopAll);
        self.active_board = id;
        self.push_hotkey_rebuild();
        self.mark_dirty();
    }

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
        let deleting_active = id == self.active_board;
        let removed: Board = self.boards.remove(pos);
        let capture_removed = self
            .recording
            .is_some_and(|rid| removed.slots.iter().any(|s| s.id == rid));
        let editor_removed = self
            .editor
            .as_ref()
            .is_some_and(|editor| removed.slots.iter().any(|s| s.id == editor.slot));
        for slot in &removed.slots {
            self.broken.remove(&slot.id);
            self.waves.invalidate(slot.id);
            if self.playing.remove(&slot.id) {
                self.send(AudioCommand::Stop(slot.id));
            }
        }
        self.board_dialog = None;
        // Any pending hotkey confirmation may refer to a slot on the board that
        // is being removed; never apply it after the board is gone.
        self.confirm = None;

        if editor_removed {
            self.stop_preview();
            self.editor = None;
        }
        if capture_removed {
            self.recording = None;
            self.toast(
                ToastKind::Info,
                "Hotkey recording cancelled — its board was deleted.",
            );
        }

        let landing = self
            .boards
            .get(pos.min(self.boards.len().saturating_sub(1)))
            .map(|b| b.id)
            .expect("at least one board remains");
        if deleting_active {
            self.switch_board(landing);
        } else if capture_removed {
            self.push_hotkey_rebuild();
        }
        self.mark_dirty();
        self.toast(ToastKind::Info, format!("Deleted “{}”.", removed.name));
    }

    fn suspend_hotkey_grabs(&self) {
        debug!("Suspending all OS grabs while recording.");
        let _ = self.hotkey_tx.send(HotkeyCommand::Rebuild(Vec::new()));
    }

    fn guard_hotkey(&mut self, id: SlotId, hotkey: Hotkey) {
        if let Err(reason) = to_global_hotkeys(&hotkey) {
            debug!(
                "Bind rejected (ungrabbable): slot={id} key={}",
                hotkey.display()
            );
            self.toast(ToastKind::Warn, reason);
            return;
        }

        if self.stop_hotkey.as_ref() == Some(&hotkey) {
            debug!(
                "Bind refused (stop key): slot={id} key={}",
                hotkey.display()
            );
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

    fn guard_stop_hotkey(&mut self, hotkey: Hotkey) {
        if let Err(reason) = to_global_hotkeys(&hotkey) {
            self.toast(ToastKind::Warn, reason);
            return;
        }

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
                        ui.label(
                            RichText::new(format!("v{}", env!("CARGO_PKG_VERSION")))
                                .size(11.5)
                                .color(theme::MUTED),
                        );
                        ui.label(RichText::new("·").size(11.5).color(theme::MUTED));

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

                    if ui
                        .small_button("↻")
                        .on_hover_text("Look for newly plugged in devices")
                        .clicked()
                    {
                        self.output_devices = list_output_devices();

                        if !self.output_devices.contains(&self.output_device) {
                            self.output_device = SYSTEM_DEFAULT_LABEL.to_owned();
                            self.send(AudioCommand::SetDevice(self.output_device.clone()));
                            self.mark_dirty();
                        }
                        self.toast(ToastKind::Info, "Device list refreshed.");
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

                    let any_playing = !self.playing.is_empty() || self.testing || self.preview_playing;
                    if ui
                        .add_enabled(any_playing, Button::new(RichText::new("⏹  Stop All")))
                        .on_hover_text("Stop every sound right now")
                        .clicked()
                    {
                        self.playing.clear();
                        self.testing = false;
                        self.preview_playing = false;
                        self.send(AudioCommand::StopAll);
                    }

                    let recording_stop = self.recording == Some(STOP_ALL_ID);
                    let stop_rejected = self
                        .stop_hotkey
                        .as_ref()
                        .map(|k| self.rejected_hotkeys.contains(&k.display()))
                        .unwrap_or(false);
                    let (stop_text, stop_colour, stop_stroke) = if recording_stop {
                        (
                            "Press keys…  (Esc to cancel)".to_owned(),
                            theme::TEXT,
                            Stroke::new(1.5, theme::ACCENT),
                        )
                    } else {
                        match &self.stop_hotkey {
                            Some(hotkey) if stop_rejected => (
                                format!("⚠ {}", hotkey.display()),
                                theme::WARN,
                                Stroke::new(1.0, theme::WARN),
                            ),
                            Some(hotkey) => (
                                hotkey.display(),
                                theme::TEXT,
                                Stroke::new(1.0, theme::BORDER),
                            ),
                            None => (
                                "Set stop key".to_owned(),
                                theme::MUTED,
                                Stroke::new(1.0, theme::BORDER),
                            ),
                        }
                    };
                    let badge_width = if recording_stop { 190.0 } else { 110.0 };
                    let stop_badge = ui.add_sized(
                        [badge_width, 26.0],
                        Button::new(RichText::new(stop_text).size(12.5).color(stop_colour))
                            .fill(theme::BG_SOFT)
                            .stroke(stop_stroke)
                            .corner_radius(CornerRadius::same(8)),
                    );
                    if recording_stop {
                    } else if stop_badge.clicked() {
                        debug!("Arming hotkey record for the Stop key.");
                        self.recording = Some(STOP_ALL_ID);
                        self.suspend_hotkey_grabs();
                        self.toast(
                            ToastKind::Info,
                            "Press the key that should stop everything (Esc to cancel).",
                        );
                    } else if stop_badge.secondary_clicked() && self.stop_hotkey.is_some() {
                        if let Some(old) = &self.stop_hotkey {
                            self.notified_rejected_hotkeys.remove(&old.display());
                        }
                        self.stop_hotkey = None;
                        self.push_hotkey_rebuild();
                        self.mark_dirty();
                        self.toast(ToastKind::Info, "Stop key removed.");
                    } else if stop_rejected {
                        stop_badge.on_hover_text(
                            "This stop key could not be registered (used by another program). Click to change  •  Right-click to remove",
                        );
                    } else {
                        stop_badge.on_hover_text(
                            "The key that silences everything, even while minimized",
                        );
                    }

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
                            .on_hover_text(
                                "Stop recording and keep the take (quitting discards it)",
                            )
                            .clicked()
                        {
                            let _ = self.record_tx.send(RecordCommand::Stop);
                            self.toast(ToastKind::Info, "Finishing the take…");
                        }
                    } else {
                        let starting = self.mic_starting.is_some();
                        ui.add_enabled_ui(!starting, |ui| {
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
                        });
                        if starting {
                            ui.add_enabled_ui(false, |ui| {
                                ui.add_sized(
                                    [90.0, 26.0],
                                    egui::Button::new(
                                        RichText::new("● Starting…").size(12.5).color(theme::MUTED),
                                    )
                                    .fill(theme::BG_SOFT)
                                    .corner_radius(CornerRadius::same(8)),
                                );
                            });
                        } else if ui
                            .add_sized(
                                [90.0, 26.0],
                                Button::new(
                                    RichText::new("● Record").size(12.5).color(theme::TEXT),
                                )
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
                            self.mic_start_board = Some(self.active_board);
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

                        if ui.small_button("⚙").on_hover_text("Settings").clicked() {
                            self.settings_open = true;
                        }
                    });
                });

                ui.add_space(4.0);

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
                            focused: false,
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
                ui.horizontal(|ui| match &self.hotkey_state {
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
                });

                if let Some(toast) = &self.toast {
                    ui.add_space(2.0);
                    ui.add(
                        egui::Label::new(
                            RichText::new(&toast.text)
                                .size(12.0)
                                .color(toast.kind.colour()),
                        )
                        .wrap(),
                    );
                } else if let Some(err) = &self.save_error {
                    ui.add_space(2.0);
                    ui.add(
                        egui::Label::new(
                            RichText::new(format!("⚠  Save error: {err}"))
                                .size(11.5)
                                .color(theme::WARN),
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
                                RichText::new("Click \"➕  Add Sound\" to create your first slot.")
                                    .color(theme::MUTED),
                            );
                        });
                        return;
                    }

                    ui.spacing_mut().item_spacing.x = SLOT_GAP;

                    let avail = ui.available_width();
                    let columns =
                        (((avail + SLOT_GAP) / (SLOT_W + SLOT_GAP)).floor() as usize).max(1);

                    let total = self.active().slots.len() + 1;
                    let indices: Vec<usize> = (0..total).collect();

                    for row in indices.chunks(columns) {
                        ui.horizontal(|ui| {
                            for &idx in row {
                                if idx < self.active().slots.len() {
                                    let slot = &self.active().slots[idx];
                                    let playing = self.playing.contains(&slot.id);
                                    let recording = self.recording == Some(slot.id);
                                    let rejected = slot
                                        .hotkey
                                        .as_ref()
                                        .map(|k| self.rejected_hotkeys.contains(&k.display()))
                                        .unwrap_or(false);
                                    if let Some(action) =
                                        draw_slot(ui, slot, playing, recording, rejected)
                                    {
                                        actions.push(action);
                                    }
                                } else {
                                    draw_add_tile(ui, actions);
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
            debug!(
                "Confirm accepted ({}) for slot={}.",
                pending.accept, pending.slot
            );
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

    fn board_dialogs(&mut self, ctx: &egui::Context) {
        let mut save_rename: Option<(BoardId, String)> = None;
        let mut close = false;
        let mut do_delete: Option<BoardId> = None;

        match &mut self.board_dialog {
            None => return,
            Some(BoardDialog::Rename { id, name, focused }) => {
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
                        if !*focused {
                            input.request_focus();
                            *focused = true;
                        }
                        let enter = (input.has_focus() || input.lost_focus())
                            && ui.input(|i| i.key_pressed(egui::Key::Enter));
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

            if id == STOP_ALL_ID {
                self.guard_stop_hotkey(hotkey);
            } else {
                self.guard_hotkey(id, hotkey);
            }
        }
    }

    fn send(&mut self, cmd: AudioCommand) {
        let mut cmd = cmd;
        if let Err(err) = self.audio_tx.send(cmd) {
            log::error!("Audio engine send failed ({err}); restarting engine.");
            cmd = err.into_inner();
            let (tx, rx) =
                spawn_audio_engine(self.output_device.clone(), self.volume, self.ctx.clone());
            self.audio_tx = tx;
            self.audio_rx = rx;
            self.playing.clear();
            self.preview_playing = false;
            self.testing = false;
            // Retry the command that discovered the dead engine, otherwise
            // the first click/hotkey after a crash would be silently lost.
            if let Err(err) = self.audio_tx.send(cmd) {
                log::error!("Audio engine restart failed to accept the command: {err}");
            }
            if let Err(err) = self
                .hotkey_tx
                .send(HotkeyCommand::SetAudio(self.audio_tx.clone()))
            {
                log::warn!("Could not give the hotkey listener the new audio sender: {err}");
            }
            self.toast(
                ToastKind::Warn,
                "The audio engine stopped unexpectedly and was restarted.",
            );
        }
    }

    fn poll_audio_events(&mut self) {
        while let Ok(event) = self.audio_rx.try_recv() {
            match event {
                AudioEvent::Started(id) => {
                    if id == TEST_SOUND_ID {
                        self.testing = true;
                    } else if id == PREVIEW_TRIM_ID {
                        self.preview_playing = true;
                    } else {
                        self.playing.insert(id);
                        if self.broken.remove(&id) {
                            self.push_hotkey_rebuild();
                        }
                    }
                }
                AudioEvent::Finished(id) => {
                    if id == TEST_SOUND_ID {
                        self.testing = false;
                    } else if id == PREVIEW_TRIM_ID {
                        self.preview_playing = false;
                    } else {
                        self.playing.remove(&id);
                    }
                }
                AudioEvent::FileMissing { id, path } => {
                    if id == PREVIEW_TRIM_ID {
                        self.preview_playing = false;
                        self.toast(
                            ToastKind::Warn,
                            format!("Can't find {} — did you move or delete it?", path.display()),
                        );
                    } else {
                        self.playing.remove(&id);
                        if let Some(slot) = self.find_slot_mut(id) {
                            slot.missing = true;
                        }

                        self.push_hotkey_rebuild();
                        self.toast(
                            ToastKind::Warn,
                            format!(
                                "Can't find {} — did you move or delete it? Use Find Again to re-link it.",
                                path.display()
                            ),
                        );
                    }
                }
                AudioEvent::DecodeError { id, message } => {
                    if id == TEST_SOUND_ID {
                        self.testing = false;
                    } else if id == PREVIEW_TRIM_ID {
                        self.preview_playing = false;
                    } else {
                        self.playing.remove(&id);
                        self.broken.insert(id);
                        self.push_hotkey_rebuild();
                    }
                    self.toast(ToastKind::Warn, message);
                }
                AudioEvent::DeviceReady { actual, fallback } => {
                    if let Some(note) = fallback {
                        if !actual.is_empty() {
                            self.output_device = actual;
                        }
                        self.playing.clear();
                        self.preview_playing = false;
                        self.testing = false;
                        self.toast(ToastKind::Warn, note);
                    }
                }
                AudioEvent::StreamError(message) => {
                    self.playing.clear();
                    self.preview_playing = false;
                    self.testing = false;
                    self.toast(ToastKind::Warn, message);
                }
            }
        }
    }

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
                    self.rejected_hotkeys.insert(display.clone());
                    if self.notified_rejected_hotkeys.insert(display.clone()) {
                        self.toast(
                            ToastKind::Warn,
                            format!("Could not grab {display}: {reason}"),
                        );
                    }
                }
                HotkeyEvent::AudioUnavailable(cmd) => {
                    log::warn!("Hotkey press found a dead audio engine; restarting and retrying.");
                    self.send(cmd);
                }
            }
        }
        changed
    }

    fn mark_dirty(&mut self) {
        self.edit_generation = self.edit_generation.wrapping_add(1);
        self.config_dirty = true;
        self.ctx.request_repaint();
        self.ctx.request_repaint_after(Duration::from_millis(700));
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
        if self.read_only {
            log::warn!("Config save skipped: application is in read-only mode.");
            return;
        }
        if self.save_in_flight {
            // A write is already running; the newer generation stays dirty and
            // the autosave timer picks it up when the current one finishes.
            return;
        }

        let generation = self.edit_generation;
        let command = config::SaveCommand::Save {
            generation,
            state: self.snapshot_state(),
            ack: None,
        };
        match self.save_tx.send(command) {
            Ok(()) => self.save_in_flight = true,
            Err(_) => {
                log::error!("Config saver thread is gone; saving inline.");
                let result = config::save(&self.snapshot_state());
                self.apply_save_result(generation, result);
            }
        }
    }

    fn apply_save_result(&mut self, generation: u64, result: Result<(), String>) {
        self.save_in_flight = false;
        match result {
            Ok(()) => {
                log::info!("Saved state ({} boards).", self.boards.len());
                self.last_save = Instant::now();
                self.save_backoff = Duration::from_millis(700);
                if generation == self.edit_generation {
                    self.config_dirty = false;
                }
                if self.save_error.is_some() {
                    self.save_error = None;
                    self.toast(ToastKind::Good, "Soundboard layout saved successfully.");
                }
            }
            Err(err) => {
                log::warn!("Could not save config: {err}");
                self.config_dirty = true;
                self.last_save = Instant::now();
                self.save_backoff = (self.save_backoff * 2).min(Duration::from_secs(30));
                if self.save_error.is_none() {
                    self.toast(
                        ToastKind::Warn,
                        "Could not save your soundboard layout. Retrying in background...",
                    );
                }
                self.save_error = Some(err);
                self.ctx.request_repaint_after(self.save_backoff);
            }
        }
        self.ctx.request_repaint();
    }

    fn poll_save_results(&mut self) -> bool {
        let mut changed = false;
        while let Ok((generation, result)) = self.save_rx.try_recv() {
            changed = true;
            self.apply_save_result(generation, result);
        }
        changed
    }

    fn maybe_autosave(&mut self) {
        if self.config_dirty
            && !self.read_only
            && !self.save_in_flight
            && self.last_save.elapsed() >= self.save_backoff
        {
            self.save_now();
        }
    }

    fn poll_record_events(&mut self) -> bool {
        let mut changed = false;
        while let Ok(event) = self.record_rx.try_recv() {
            changed = true;

            self.mic_starting = None;
            match event {
                RecordEvent::Started { device } => {
                    let board = self.mic_start_board.take().unwrap_or(self.active_board);
                    self.mic = Some(MicTake {
                        started: Instant::now(),
                        board,
                    });

                    self.toast(ToastKind::Info, format!("Recording from {device}."));
                }
                RecordEvent::Saved {
                    path,
                    duration_secs,
                    capped,
                    dropped_chunks,
                    stream_died,
                } => {
                    let target_board = self
                        .mic
                        .as_ref()
                        .map(|take| take.board)
                        .unwrap_or(self.active_board);
                    self.mic = None;
                    self.mic_start_board = None;

                    let name = Self::display_name_for(&path);
                    let id = self.alloc_id();
                    let mut slot = SoundSlot::with_file(id, name.clone(), path);
                    slot.duration_secs = Some(duration_secs);

                    let board_name = match self.boards.iter().find(|b| b.id == target_board) {
                        Some(board) => board.name.clone(),
                        None => self.active().name.clone(),
                    };
                    if let Some(board) = self.boards.iter_mut().find(|b| b.id == target_board) {
                        board.slots.push(slot);
                    } else {
                        // The original board was deleted while recording; keep
                        // the take rather than throwing it away.
                        self.active_mut().slots.push(slot);
                    }
                    self.mark_dirty();

                    let seconds = format!("{duration_secs:.1}s");
                    if capped {
                        self.toast(
                            ToastKind::Warn,
                            format!(
                                "Reached the 10-minute limit — saved {seconds} as “{name}” on \
                                 “{board_name}”."
                            ),
                        );
                    } else if stream_died {
                        self.toast(
                            ToastKind::Warn,
                            format!(
                                "The microphone stopped early — kept {seconds} as “{name}” on \
                                 “{board_name}”."
                            ),
                        );
                    } else if dropped_chunks > 0 {
                        self.toast(
                            ToastKind::Warn,
                            format!(
                                "Saved “{name}” ({seconds}) on “{board_name}”, but {dropped_chunks} \
                                 audio chunk(s) were dropped — it may have gaps."
                            ),
                        );
                    } else {
                        self.toast(
                            ToastKind::Good,
                            format!("Saved “{name}” ({seconds}) to “{board_name}”."),
                        );
                    }
                }
                RecordEvent::Failed(message) => {
                    self.mic = None;
                    self.mic_start_board = None;
                    self.toast(ToastKind::Warn, message);
                }
            }
        }
        changed
    }

    fn apply(&mut self, action: SlotAction) {
        match action {
            SlotAction::AddViaPicker => {
                self.add_sound_via_picker();
            }
            SlotAction::Play(id) => {
                self.stop_preview();
                let found = self.active().slots.iter().find(|s| s.id == id).cloned();
                if let Some(slot) = found {
                    if let Some(path) = slot.file_path.clone() {
                        if self.broken.remove(&id) {
                            self.push_hotkey_rebuild();
                        }
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
                self.toast(
                    ToastKind::Info,
                    "Press the key you want to use (Esc to cancel).",
                );
            }
            SlotAction::ClearHotkey(id) => {
                let old_key = self.find_slot_mut(id).and_then(|s| s.hotkey.take());
                if let Some(old) = old_key {
                    self.notified_rejected_hotkeys.remove(&old.display());
                }
                self.push_hotkey_rebuild();
                self.mark_dirty();
                self.toast(ToastKind::Info, "Hotkey removed.");
            }
            SlotAction::Remove(id) => {
                if self.editor.as_ref().is_some_and(|editor| editor.slot == id) {
                    self.stop_preview();
                    self.editor = None;
                }
                self.active_mut().slots.retain(|s| s.id != id);
                self.playing.remove(&id);
                self.broken.remove(&id);

                self.waves.invalidate(id);
                if self.recording == Some(id) {
                    self.recording = None;

                    self.push_hotkey_rebuild();
                }
                self.send(AudioCommand::Stop(id));
                self.push_hotkey_rebuild();
                self.mark_dirty();
            }
            SlotAction::FindAgain(id) => {
                self.fill_slot_via_picker(id);
            }
            SlotAction::Edit(id) => self.open_editor(id),
        }
    }

    fn open_editor(&mut self, id: SlotId) {
        let found = self
            .active()
            .slots
            .iter()
            .find(|s| s.id == id)
            .map(|s| (s.file_path.clone(), s.missing, s.trim_start, s.trim_end));
        match found {
            Some((Some(path), false, start, end)) => {
                self.stop_preview();
                self.waves.request(id, path);
                self.editor = Some(EditorState {
                    slot: id,
                    start_frac: start,
                    end_frac: end,
                    drag: None,
                });
            }
            Some((_, true, _, _)) => {
                self.toast(
                    ToastKind::Info,
                    "Find the file first (Find Again), then trim it.",
                );
            }
            _ => {}
        }
    }

    fn poll_waves(&mut self) -> bool {
        let mut changed = false;
        while let Ok((id, path, result)) = self.waves.res_rx.try_recv() {
            changed = true;
            self.waves.pending.remove(&id);
            let current = self
                .boards
                .iter()
                .flat_map(|b| b.slots.iter())
                .find(|s| s.id == id)
                .and_then(|s| s.file_path.clone());
            if current.as_ref() == Some(&path) {
                self.waves.order.retain(|&x| x != id);
                self.waves.order.push(id);
                self.waves.ready.insert(id, result);
            }
        }
        changed
    }

    fn editor_dialog(&mut self, ctx: &egui::Context) {
        let Some(slot_id) = self.editor.as_ref().map(|editor| editor.slot) else {
            return;
        };
        let slot = self
            .boards
            .iter()
            .flat_map(|b| b.slots.iter())
            .find(|s| s.id == slot_id)
            .cloned();
        let Some(slot) = slot else {
            self.stop_preview();
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

                match self.waves.ready.get(&slot_id).cloned() {
                    None => {
                        ui.label(RichText::new("Loading sound…").color(theme::MUTED));
                        ui.add_space(100.0);
                    }
                    Some(Err(message)) => {
                        ui.label(RichText::new(&message).color(theme::DANGER));
                        ui.add_space(4.0);
                        if let Some(path) = &slot.file_path {
                            if ui.button("⟳ Retry Loading").clicked() {
                                self.waves.invalidate(slot_id);
                                self.waves.request(slot_id, path.clone());
                            }
                        }
                        ui.add_space(8.0);
                    }
                    Some(Ok(wave)) => {
                        self.waves.touch(slot_id);
                        let (start_frac, end_frac) = {
                            let editor = self.editor.as_ref().expect("editor open");
                            (editor.start_frac, editor.end_frac)
                        };
                        let response = draw_waveform(ui, &wave, start_frac, end_frac);
                        if let Some(editor) = self.editor.as_mut() {
                            if response.drag_started() {
                                let x = response
                                    .interact_pointer_pos()
                                    .map(|p| (p.x - response.rect.left()) / response.rect.width());
                                if let Some(x) = x {
                                    let ds = (x - editor.start_frac).abs();
                                    let de = (x - editor.end_frac).abs();
                                    editor.drag = Some(if ds <= de {
                                        EditorHandle::Start
                                    } else {
                                        EditorHandle::End
                                    });
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
                                            editor.start_frac =
                                                frac.min(editor.end_frac - MIN_TRIM_GAP);
                                            editor.start_frac = editor.start_frac.max(0.0);
                                        }
                                        EditorHandle::End => {
                                            editor.end_frac =
                                                frac.max(editor.start_frac + MIN_TRIM_GAP);
                                            editor.end_frac = editor.end_frac.min(1.0);
                                        }
                                    }
                                }
                            }
                            if response.drag_stopped() {
                                editor.drag = None;
                            }
                        }

                        let (start, end) = self
                            .editor
                            .as_ref()
                            .map(|e| (e.start_frac, e.end_frac))
                            .unwrap_or((start_frac, end_frac));
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
                            let preview_label = if self.preview_playing {
                                "⏹  Stop"
                            } else {
                                "▶  Play"
                            };
                            if ui.button(preview_label).clicked() {
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

                if !matches!(self.waves.ready.get(&slot_id), Some(Ok(_))) {
                    ui.horizontal(|ui| {
                        if ui.button("Close").clicked() {
                            close = true;
                        }
                    });
                }
            });

        if close {
            self.stop_preview();
            self.editor = None;
        }
        if reset {
            self.stop_preview();
            if let Some(editor) = self.editor.as_mut() {
                editor.start_frac = 0.0;
                editor.end_frac = 1.0;
            }
        }
        if preview {
            if self.preview_playing {
                self.stop_preview();
            } else {
                let editor = self.editor.as_ref().expect("editor open");
                self.preview_trim(slot_id, editor.start_frac, editor.end_frac);
            }
        }
        if let Some((start, end, duration)) = save {
            self.stop_preview();
            if let Some(slot) = self.find_slot_mut(slot_id) {
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

    /// Stops the trim-editor preview without touching normal playback.
    fn stop_preview(&mut self) {
        if self.preview_playing {
            self.send(AudioCommand::Stop(PREVIEW_TRIM_ID));
            self.preview_playing = false;
        }
    }

    fn preview_trim(&mut self, id: SlotId, start_frac: f32, end_frac: f32) {
        let duration_secs = match self.waves.ready.get(&id).cloned() {
            Some(Ok(wave)) => {
                self.waves.touch(id);
                wave.duration_secs
            }
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
            self.send(AudioCommand::Play {
                id: PREVIEW_TRIM_ID,
                path,
                trim: Some(TrimSpec {
                    start_frac,
                    end_frac,
                    duration_secs,
                }),
            });
            // Track the request immediately so closing the editor during the
            // hand-off still sends the matching Stop.
            self.preview_playing = true;
        }
    }

    fn pick_sound_file() -> Option<PathBuf> {
        rfd::FileDialog::new()
            .set_title("Choose a sound")
            .add_filter("Audio", &["mp3", "wav", "ogg", "flac", "m4a", "aac"])
            .pick_file()
    }

    fn display_name_for(path: &std::path::Path) -> String {
        path.file_stem()
            .and_then(|stem| stem.to_str())
            .map(|stem| ellipsize(stem, 18))
            .filter(|stem| !stem.is_empty())
            .unwrap_or_else(|| "Sound".to_owned())
    }

    fn fill_slot_via_picker(&mut self, id: SlotId) {
        let Some(path) = Self::pick_sound_file() else {
            return;
        };
        self.stop_preview();
        let name = Self::display_name_for(&path);
        let editor_open_for_slot = self.editor.as_ref().is_some_and(|e| e.slot == id);
        if let Some(slot) = self.find_slot_mut(id) {
            slot.name = name.clone();
            slot.file_path = Some(path.clone());
            slot.missing = false;

            slot.clear_trim();
        }
        self.playing.remove(&id);
        self.send(AudioCommand::Stop(id));
        self.waves.invalidate(id);
        if editor_open_for_slot {
            if let Some(editor) = self.editor.as_mut() {
                editor.start_frac = 0.0;
                editor.end_frac = 1.0;
                editor.drag = None;
            }
            self.waves.request(id, path);
        }

        self.broken.remove(&id);
        self.push_hotkey_rebuild();
        self.mark_dirty();
        self.toast(
            ToastKind::Good,
            format!("Added \"{name}\". Click ▶ to hear it."),
        );
    }

    fn add_sound_via_picker(&mut self) {
        let Some(path) = Self::pick_sound_file() else {
            return;
        };
        let name = Self::display_name_for(&path);
        let id = self.alloc_id();
        self.active_mut()
            .slots
            .push(SoundSlot::with_file(id, name.clone(), path));
        self.mark_dirty();
        self.toast(
            ToastKind::Good,
            format!("Added \"{name}\". Click ▶ to hear it."),
        );
    }

    fn schedule_repaint(&self, ctx: &egui::Context) {
        if !self.playing.is_empty() || self.testing {
            ctx.request_repaint_after(TILE_ANIMATION_FRAME);
        }

        if self.mic.is_some() {
            ctx.request_repaint_after(TIMER_FRAME);
        }

        if let Some(toast) = &self.toast {
            let remaining = toast.until.saturating_duration_since(Instant::now());
            if remaining > Duration::ZERO {
                ctx.request_repaint_after(remaining);
            }
        }

        if let Some(since) = self.mic_starting {
            let remaining = MIC_START_TIMEOUT.saturating_sub(since.elapsed());
            ctx.request_repaint_after(remaining);
        }

        if self.config_dirty && !self.read_only && !self.save_in_flight {
            let elapsed = self.last_save.elapsed();
            if elapsed < self.save_backoff {
                ctx.request_repaint_after(self.save_backoff - elapsed);
            }
        }
    }
}

impl eframe::App for SoundboardApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();

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

        if let Some(since) = self.mic_starting {
            if since.elapsed() > MIC_START_TIMEOUT {
                log::warn!("Mic start timed out; restarting the recorder thread.");
                let (tx, rx) = spawn_recorder(self.ctx.clone());
                self.record_tx = tx;
                self.record_rx = rx;
                self.mic_starting = None;
                self.mic_start_board = None;
                self.toast(
                    ToastKind::Warn,
                    "The microphone took too long to open — it may be in use by another app. \
                     Try again, or pick a different microphone.",
                );
                ctx.request_repaint();
            }
        }

        self.handle_tray(ctx);

        if self.window_visible {
            self.handle_recording(ctx);
        }

        if self.poll_save_results() {
            ctx.request_repaint();
        }
        self.schedule_repaint(ctx);
        self.maybe_autosave();
    }

    fn on_exit(&mut self) {
        if self.read_only {
            return;
        }

        // Let any write that is already running finish, then queue a final
        // snapshot containing the newest edits and wait for it to land.
        let deadline = Instant::now() + Duration::from_secs(5);
        while self.save_in_flight && Instant::now() < deadline {
            if let Ok((generation, result)) = self.save_rx.recv_timeout(Duration::from_millis(50)) {
                self.apply_save_result(generation, result);
            }
        }

        if !self.config_dirty {
            return;
        }

        let (ack_tx, ack_rx) = crossbeam_channel::bounded(1);
        let command = config::SaveCommand::Save {
            generation: self.edit_generation,
            state: self.snapshot_state(),
            ack: Some(ack_tx),
        };
        if self.save_tx.send(command).is_ok() {
            if ack_rx.recv_timeout(Duration::from_secs(5)).is_err() {
                log::warn!("Timed out waiting for the final config save.");
            }
        } else if let Err(err) = config::save(&self.snapshot_state()) {
            log::warn!("Could not save config on exit: {err}");
        }
    }
}

const MIN_TRIM_GAP: f32 = 0.02;

const WAVE_W: f32 = 300.0;
const WAVE_H: f32 = 110.0;

fn draw_waveform(
    ui: &mut egui::Ui,
    wave: &Waveform,
    start_frac: f32,
    end_frac: f32,
) -> egui::Response {
    let (rect, response) = ui.allocate_exact_size(Vec2::new(WAVE_W, WAVE_H), Sense::drag());

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

    for (i, (min, max)) in wave.peaks.iter().enumerate() {
        let frac = (i as f32 + 0.5) / wave.peaks.len() as f32;
        let x = x_of(frac);
        let y_top = mid_y - max.clamp(-1.0, 1.0) * half_h;
        let y_bot = mid_y - min.clamp(-1.0, 1.0) * half_h;
        let segment = [egui::pos2(x, y_top), egui::pos2(x, y_bot.max(y_top + 1.0))];
        let colour = if frac >= start_frac && frac <= end_frac {
            theme::TEXT
        } else {
            theme::MUTED
        };
        ui.painter().line_segment(segment, Stroke::new(1.0, colour));
    }

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

fn fmt_time(secs: f32) -> String {
    let tenths = (secs.max(0.0) * 10.0).round() as u64;
    let total_secs = tenths / 10;
    let mins = total_secs / 60;
    let rem_secs = total_secs % 60;
    let rem_tenths = tenths % 10;
    format!("{mins}:{rem_secs:02}.{rem_tenths}")
}

fn shorten_device(name: &str) -> String {
    ellipsize(name, 22)
}

fn draw_slot(
    ui: &mut egui::Ui,
    slot: &SoundSlot,
    playing: bool,
    recording: bool,
    hotkey_rejected: bool,
) -> Option<SlotAction> {
    let mut action: Option<SlotAction> = None;

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
                let fill = if playing {
                    theme::GOOD_SOFT
                } else {
                    theme::ACCENT
                };
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

            let badge = hotkey_badge(ui, slot, recording, hotkey_rejected);
            if recording {
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

fn hotkey_badge(
    ui: &mut egui::Ui,
    slot: &SoundSlot,
    recording: bool,
    rejected: bool,
) -> egui::Response {
    let (text, fill, stroke, colour) = if recording {
        (
            "Press keys…  (Esc to cancel)".to_owned(),
            theme::ACCENT_SOFT,
            Stroke::new(1.5, theme::ACCENT),
            theme::TEXT,
        )
    } else {
        match &slot.hotkey {
            Some(hotkey) if rejected => (
                format!("⚠ {}", hotkey.display()),
                theme::BG_SOFT,
                Stroke::new(1.0, theme::WARN),
                theme::WARN,
            ),
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
        if rejected {
            response.on_hover_text(
                "This hotkey could not be registered (used by another program). Click to change  •  Right-click to remove",
            )
        } else {
            response.on_hover_text("Click to change  •  Right-click to remove")
        }
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
            ui.label(RichText::new("Add Sound").size(13.0).color(theme::MUTED));
        },
    );

    if response.clicked() {
        actions.push(SlotAction::AddViaPicker);
    }
    response.on_hover_cursor(egui::CursorIcon::PointingHand);
}

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

fn ellipsize(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        text.to_owned()
    } else {
        let truncated: String = text.chars().take(max.saturating_sub(1)).collect();
        format!("{truncated}…")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fmt_time_formats_minutes_seconds_and_tenths() {
        assert_eq!(fmt_time(0.0), "0:00.0");
        assert_eq!(fmt_time(3.2), "0:03.2");
        assert_eq!(fmt_time(65.0), "1:05.0");
        assert_eq!(fmt_time(123.4), "2:03.4");
        assert_eq!(fmt_time(59.96), "1:00.0");

        assert_eq!(fmt_time(-5.0), "0:00.0");
    }

    #[test]
    fn ellipsize_truncates_with_ellipsis() {
        assert_eq!(ellipsize("short", 10), "short");
        assert_eq!(ellipsize("longer-than-max", 10), "longer-th…");
        assert_eq!(ellipsize("a", 1), "a");
        assert_eq!(ellipsize("ab", 1), "…");
    }

    #[test]
    fn shorten_device_caps_at_22_chars() {
        assert_eq!(shorten_device("Mic"), "Mic");
        let long = "Speakers (Realtek(R) Audio)";
        let out = shorten_device(long);
        assert!(out.chars().count() <= 22);
        assert!(out.ends_with('…'));
    }

    #[test]
    fn trim_spec_for_returns_none_when_untrimmed_or_no_duration() {
        let slot = SoundSlot::with_file(1, "x", "/x.mp3");
        assert!(trim_spec_for(&slot).is_none());

        let mut trimmed = SoundSlot::with_file(2, "y", "/y.mp3");
        trimmed.trim_start = 0.25;
        trimmed.trim_end = 0.75;

        assert!(trim_spec_for(&trimmed).is_none());

        trimmed.duration_secs = Some(8.0);
        let spec = trim_spec_for(&trimmed).unwrap();
        assert!((spec.start_frac - 0.25).abs() < f32::EPSILON);
        assert!((spec.end_frac - 0.75).abs() < f32::EPSILON);
        assert!((spec.duration_secs - 8.0).abs() < f32::EPSILON);
    }
}
