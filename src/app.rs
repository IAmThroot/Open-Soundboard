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
use crate::record::{spawn_recorder, RecordAttempt, RecordCommand, RecordEvent, RecordMessage};
use crate::theme;
use crate::tray::{Tray, TrayAction};
use crate::waveform::{build_waveform, Waveform};

const PREVIEW_TRIM_ID: SlotId = u64::MAX - 2;

const SLOT_W: f32 = 200.0;
const SLOT_H: f32 = 156.0;
const SLOT_PAD: f32 = 12.0;
const SLOT_GAP: f32 = 12.0;

enum SlotAction {
    Play(SlotId),
    Rename(SlotId),
    Move(SlotId, isize),
    MoveBefore(SlotId, SlotId),
    Volume(SlotId, f32),
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

enum OutputState {
    Opening,
    Ready {
        actual: String,
        warning: Option<String>,
    },
    Unavailable(String),
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum RecordingPhase {
    Opening,
    Recording,
    Finishing,
    Cancelling,
}

struct RecordingSession {
    attempt: RecordAttempt,
    board: BoardId,
    since: Instant,
    phase: RecordingPhase,
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
    record_rx: Receiver<RecordMessage>,

    mic_session: Option<RecordingSession>,
    next_mic_attempt: u64,

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
    pending_playback: HashSet<SlotId>,

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
    recorder_open: bool,
    search: String,
    rename_sound: Option<(SlotId, String)>,
    undo: Vec<(Vec<Board>, BoardId)>,
    bundle_job: Option<Receiver<Result<Option<Board>, String>>>,
    output_state: OutputState,

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

struct WaveRequest {
    id: SlotId,
    path: PathBuf,
    token: std::sync::Arc<std::sync::atomic::AtomicBool>,
}

struct WaveResult {
    request: WaveRequest,
    result: Result<Waveform, String>,
}

struct WaveCache {
    ready: HashMap<SlotId, Result<Waveform, String>>,
    order: Vec<SlotId>,
    pending: HashMap<SlotId, std::sync::Arc<std::sync::atomic::AtomicBool>>,
    outstanding: usize,
    req_tx: Sender<WaveRequest>,
    res_rx: Receiver<WaveResult>,
}

const MAX_CACHED_WAVEFORMS: usize = 16;

impl WaveCache {
    fn new(ctx: &egui::Context) -> Self {
        let (req_tx, req_rx) = crossbeam_channel::bounded::<WaveRequest>(MAX_CACHED_WAVEFORMS);
        let (res_tx, res_rx) = crossbeam_channel::unbounded();
        let ctx = ctx.clone();
        std::thread::Builder::new()
            .name("waveform-worker".to_owned())
            .spawn(move || {
                while let Ok(request) = req_rx.recv() {
                    let result = if request.token.load(std::sync::atomic::Ordering::Acquire) {
                        Err("Cancelled".to_owned())
                    } else {
                        match std::panic::catch_unwind(|| build_waveform(&request.path)) {
                            Ok(r) => r,
                            Err(_) => Err("Waveform decoder thread panicked".to_string()),
                        }
                    };
                    if res_tx.send(WaveResult { request, result }).is_err() {
                        break;
                    }
                    ctx.request_repaint();
                }
            })
            .expect("failed to spawn waveform worker thread");
        Self {
            ready: HashMap::new(),
            order: Vec::new(),
            pending: HashMap::new(),
            outstanding: 0,
            req_tx,
            res_rx,
        }
    }

    fn request(&mut self, id: SlotId, path: PathBuf) {
        if self.ready.contains_key(&id)
            || self.pending.contains_key(&id)
            || self.outstanding >= MAX_CACHED_WAVEFORMS
        {
            return;
        }
        let token = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        if self
            .req_tx
            .try_send(WaveRequest {
                id,
                path,
                token: token.clone(),
            })
            .is_ok()
        {
            self.pending.insert(id, token);
            self.outstanding += 1;
        }
    }

    fn complete(&mut self, request: &WaveRequest) -> bool {
        self.outstanding = self.outstanding.saturating_sub(1);
        let current = self
            .pending
            .get(&request.id)
            .is_some_and(|token| std::sync::Arc::ptr_eq(token, &request.token));
        if current {
            self.pending.remove(&request.id);
        }
        current && !request.token.load(std::sync::atomic::Ordering::Acquire)
    }

    fn insert(&mut self, id: SlotId, result: Result<Waveform, String>) {
        self.order.retain(|&x| x != id);
        while self.order.len() >= MAX_CACHED_WAVEFORMS {
            let evict = self.order.remove(0);
            self.ready.remove(&evict);
        }
        self.ready.insert(id, result);
        self.order.push(id);
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
        if let Some(token) = self.pending.remove(&id) {
            token.store(true, std::sync::atomic::Ordering::Release);
        }
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
            mic_session: None,
            next_mic_attempt: 1,
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
            pending_playback: HashSet::new(),
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
            recorder_open: false,
            search: String::new(),
            rename_sound: None,
            undo: Vec::new(),
            bundle_job: None,
            output_state: OutputState::Opening,
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


                ui.label(format!("Open Soundboard v{}", env!("CARGO_PKG_VERSION")));
                ui.hyperlink_to(AUTHOR_CREDIT, AUTHOR_URL);
                ui.separator();
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
        self.checkpoint();
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
        if self.recording.is_some() {
            self.suspend_hotkey_grabs();
            return;
        }
        self.rejected_hotkeys.clear();
        let mut bindings: Vec<HotkeyBinding> = self
            .active()
            .slots
            .iter()
            .filter(|s| !s.missing && !self.broken.contains(&s.id))
            .filter_map(|s| {
                let mut binding = HotkeyBinding::play(
                    s.id,
                    s.hotkey.clone()?,
                    s.file_path.clone()?,
                    trim_spec_for(s),
                );
                binding.volume = s.volume;
                Some(binding)
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
        self.search.clear();
        self.push_hotkey_rebuild();
        self.mark_dirty();
    }

    fn add_board(&mut self) {
        self.checkpoint();
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
        self.checkpoint();
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
        egui::Panel::top("header")
            .frame(
                Frame::NONE
                    .fill(theme::PANEL)
                    .inner_margin(Margin::symmetric(18, 12)),
            )
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.label(RichText::new("Open Soundboard").size(19.0).strong());
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        if ui.button("Settings").clicked() {
                            self.settings_open = true;
                        }
                        ui.add(
                            egui::TextEdit::singleline(&mut self.search)
                                .hint_text("Search this board…")
                                .desired_width(170.0),
                        );
                    });
                });
            });
    }

    fn master_controls(&mut self, ui: &mut egui::Ui) {
        egui::Panel::bottom("master_controls")
.frame(Frame::NONE.fill(theme::PANEL).inner_margin(Margin::symmetric(16, 10)))
.show(ui, |ui| {
            ui.horizontal_wrapped(|ui| {
                ui.label("Output");
                    let before = self.output_device.clone();
                    egui::ComboBox::from_id_salt("output_device")
                        .selected_text(ellipsize(&self.output_device, 34))
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
                        self.output_state = OutputState::Opening;
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
                            self.output_devices.insert(1, self.output_device.clone());
                        }
                        self.output_state = OutputState::Opening;
                        self.send(AudioCommand::RefreshDevice);
                        self.toast(
                            ToastKind::Info,
                            "Device list refreshed; reopening selected output.",
                        );
                    }

                    let test_label = if self.testing {
                        "Testing…"
                    } else {
                        "Test"
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
            ui.horizontal_wrapped(|ui| {
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
                    let any_playing = !self.pending_playback.is_empty() || !self.playing.is_empty() || self.testing || self.preview_playing;
                    if ui
                        .add_enabled(any_playing, Button::new(RichText::new("■ Stop all")))
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


            });
            match &self.output_state {
                OutputState::Opening => { ui.label("Opening output…"); }
                OutputState::Unavailable(message) => { ui.label(RichText::new(format!("Output unavailable: {message}")).color(theme::WARN)); }
                OutputState::Ready { actual, warning } => {
                    ui.label(RichText::new(format!("Playing through: {actual}")).small().color(theme::MUTED));
                    if let Some(warning) = warning { ui.label(RichText::new(warning).color(theme::WARN)); }
                }
            }

        });
    }

    fn bundle_action(&mut self, import: bool) {
        if self.bundle_job.is_some() {
            return;
        }
        let dialog = rfd::FileDialog::new().add_filter("Soundboard bundle", &["osboard"]);
        let path = if import {
            dialog.pick_file()
        } else {
            dialog.set_file_name("board.osboard").save_file()
        };
        let Some(path) = path else {
            return;
        };
        let board = self.active().clone();
        let (tx, rx) = crossbeam_channel::bounded(1);
        self.bundle_job = Some(rx);
        let ctx = self.ctx.clone();
        std::thread::spawn(move || {
            let result = if import {
                crate::paths::recordings_dir()
                    .and_then(|root| {
                        crate::board_bundle::import(&path, &root.join("imported-boards"))
                    })
                    .map(Some)
            } else {
                crate::board_bundle::export(board, &path).map(|()| None)
            };
            let _ = tx.send(result);
            ctx.request_repaint();
        });
    }

    fn poll_bundle(&mut self) {
        let result = self.bundle_job.as_ref().map(|rx| rx.try_recv());
        match result {
            Some(Ok(result)) => {
                self.bundle_job = None;
                match result {
                    Ok(Some(mut board)) => {
                        self.checkpoint();
                        board.id = self.alloc_board_id();
                        for slot in &mut board.slots {
                            slot.id = self.alloc_id();
                        }
                        let id = board.id;
                        self.boards.push(board);
                        self.switch_board(id);
                        self.toast(
                            ToastKind::Good,
                            "Board imported. Assign hotkeys when ready.",
                        );
                    }
                    Ok(None) => self.toast(ToastKind::Good, "Board exported with its audio files."),
                    Err(message) => self.toast(ToastKind::Warn, message),
                }
            }
            Some(Err(crossbeam_channel::TryRecvError::Disconnected)) => {
                self.bundle_job = None;
                self.toast(ToastKind::Warn, "Board transfer stopped unexpectedly.");
            }
            _ => {}
        }
    }

    fn board_menu(&mut self, ui: &mut egui::Ui, id: BoardId) {
        let Some(board) = self.boards.iter().find(|b| b.id == id) else {
            return;
        };
        let name = board.name.clone();
        let sounds = board.slots.len();
        ui.menu_button("…", |ui| {
            if ui.button("Rename board").clicked() {
                self.board_dialog = Some(BoardDialog::Rename {
                    id,
                    name: name.clone(),
                    focused: false,
                });
                ui.close();
            }
            if ui
                .add_enabled(self.boards.len() > 1, Button::new("Delete board"))
                .clicked()
            {
                self.board_dialog = Some(BoardDialog::Delete { id, name, sounds });
                ui.close();
            }
        });
    }

    fn navigation(&mut self, ui: &mut egui::Ui) {
        if ui.available_width() < 780.0 {
            egui::Panel::top("compact_boards").show(ui, |ui| {
                ui.horizontal(|ui| {
                    let mut selected = self.active_board;
                    egui::ComboBox::from_id_salt("boards")
                        .selected_text(ellipsize(&self.active().name, 24))
                        .show_ui(ui, |ui| {
                            for board in &self.boards {
                                ui.selectable_value(&mut selected, board.id, &board.name);
                            }
                        });
                    self.switch_board(selected);
                    self.board_menu(ui, self.active_board);
                    if ui.button("+ New board").clicked() {
                        self.add_board();
                    }
                });
            });
        } else {
            egui::Panel::left("boards_sidebar")
                .exact_size(176.0)
                .frame(Frame::NONE.fill(theme::PANEL).inner_margin(12))
                .show(ui, |ui| {
                    ui.label(RichText::new("BOARDS").small().color(theme::MUTED));
                    if ui.button("+ New board").clicked() {
                        self.add_board();
                    }
                    ui.separator();
                    egui::ScrollArea::vertical().show(ui, |ui| {
                        let boards: Vec<_> =
                            self.boards.iter().map(|b| (b.id, b.name.clone())).collect();
                        for (id, name) in boards {
                            ui.push_id(id, |ui| {
                                ui.horizontal(|ui| {
                                    if ui
                                        .add_sized(
                                            [108.0, 32.0],
                                            egui::Button::new(ellipsize(&name, 16))
                                                .selected(id == self.active_board),
                                        )
                                        .on_hover_text(&name)
                                        .clicked()
                                    {
                                        self.switch_board(id);
                                    }
                                    self.board_menu(ui, id);
                                });
                            });
                        }
                    });
                });
        }
    }

    fn toolbar(&mut self, ui: &mut egui::Ui) {
        egui::Panel::top("board_heading")
            .frame(
                Frame::NONE
                    .fill(theme::BG)
                    .inner_margin(Margin::symmetric(16, 14)),
            )
            .show(ui, |ui| {
                ui.horizontal_wrapped(|ui| {
                    ui.label(
                        RichText::new(ellipsize(&self.active().name, 28))
                            .size(21.0)
                            .strong(),
                    );
                    ui.label(
                        RichText::new(format!("{} sounds", self.active().slots.len()))
                            .color(theme::MUTED),
                    );
                    if ui
                        .add_enabled(!self.undo.is_empty(), Button::new("Undo"))
                        .clicked()
                    {
                        self.undo_edit();
                    }
                    if ui
                        .add_enabled(self.bundle_job.is_none(), Button::new("Import board"))
                        .clicked()
                    {
                        self.bundle_action(true);
                    }
                    if ui
                        .add_enabled(self.bundle_job.is_none(), Button::new("Export board"))
                        .clicked()
                    {
                        self.bundle_action(false);
                    }
                    if self.bundle_job.is_some() {
                        ui.label("Transferring board…");
                    }
                    if ui.button("+ Add sound").clicked() {
                        self.add_sound_via_picker();
                    }
                    let label = if self
                        .mic_session
                        .as_ref()
                        .is_some_and(|s| s.phase == RecordingPhase::Recording)
                    {
                        "Recording…"
                    } else {
                        "Record"
                    };
                    if ui.button(label).clicked() {
                        self.recorder_open = true;
                    }
                });
            });
    }

    fn recorder_window(&mut self, ctx: &egui::Context) {
        if !self.recorder_open {
            return;
        }
        let mut open = true;
        egui::Window::new("Record a sound")
            .open(&mut open).resizable(false).default_width(360.0)
            .show(ctx, |ui| {
                ui.label("Stop saves the take. Cancel discards it.");
                let busy = self.mic_session.is_some();
                ui.add_enabled_ui(!busy, |ui| {
                    ui.horizontal(|ui| {
                        ui.label("Microphone");
                        let previous = self.mic_device.clone();
                        egui::ComboBox::from_id_salt("mic_device")
                            .selected_text(shorten_device(&self.mic_device))
                            .show_ui(ui, |ui| {
                                for name in &self.mic_devices {
                                    ui.selectable_value(&mut self.mic_device, name.clone(), name);
                                }
                            });
                        if previous != self.mic_device { self.mark_dirty(); }
                        if ui.button("Refresh").clicked() {
                            self.mic_devices = crate::devices::list_input_devices();
                        }
                    });
                });
                ui.add_space(8.0);
                if let Some(take) = self.mic_session.as_ref().filter(|s| s.phase == RecordingPhase::Recording) {
                    ui.label(RichText::new(format!("Recording  {}", fmt_time(take.since.elapsed().as_secs_f32()))).size(24.0).color(theme::DANGER));
                    if let Some(attempt) = self.mic_session.as_ref().map(|s| &s.attempt) {
                        ui.add(egui::ProgressBar::new(attempt.level()).text("Microphone level"));
                    }
                    if ui.button("■ Stop and save").clicked() {
                        if self.record_tx.send(RecordCommand::Stop).is_err() {
                            self.toast(ToastKind::Warn, "Recorder unavailable. Restart the app.");
                        } else {
                            if let Some(session) = &mut self.mic_session { session.phase = RecordingPhase::Finishing; }
                            self.toast(ToastKind::Info, "Finishing the take…");
                        }
                    }
                } else if busy {
                    ui.label(match self.mic_session.as_ref().map(|s| s.phase) {
                        Some(RecordingPhase::Finishing) => "Saving recording…",
                        Some(RecordingPhase::Cancelling) => "Cancelling recording…",
                        _ => "Opening microphone…",
                    });
                } else if ui.button("● Start recording").clicked() {
                    let device = (self.mic_device != SYSTEM_DEFAULT_LABEL).then(|| self.mic_device.clone());
                    let attempt = RecordAttempt::new(self.next_mic_attempt);
                    self.next_mic_attempt = self.next_mic_attempt.wrapping_add(1);
                    self.mic_session = Some(RecordingSession { attempt: attempt.clone(), board: self.active_board, since: Instant::now(), phase: RecordingPhase::Opening });
                    if self.record_tx.send(RecordCommand::Start { device, attempt }).is_err() {
                        self.mic_session = None;
                        self.toast(ToastKind::Warn, "Recorder unavailable. Restart the app.");
                    }
                }
                if let Some(attempt) = self.mic_session.as_ref().map(|s| &s.attempt) {
                    if attempt.is_cancelled() {
                        ui.label(RichText::new("Waiting for the microphone to stop. Restart the app if the driver never returns.").color(theme::WARN));
                    } else if ui.button("Cancel take").clicked() { attempt.cancel(); }
                    ui.label(RichText::new("Closing this panel keeps the recording running.").small().color(theme::MUTED));
                }
            });
        self.recorder_open = open;
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
                ui.horizontal_wrapped(|ui| match &self.hotkey_state {
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
                }
                if let Some(err) = &self.save_error {
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
                                RichText::new("Click \"+ Add sound\" to create your first slot.")
                                    .color(theme::MUTED),
                            );
                        });
                        return;
                    }

                    ui.spacing_mut().item_spacing.x = SLOT_GAP;

                    let avail = ui.available_width();
                    let columns =
                        (((avail + SLOT_GAP) / (SLOT_W + SLOT_GAP)).floor() as usize).max(1);

                    let query = self.search.trim().to_lowercase();
                    let indices: Vec<usize> = self
                        .active()
                        .slots
                        .iter()
                        .enumerate()
                        .filter(|(_, slot)| slot.name.to_lowercase().contains(&query))
                        .map(|(i, _)| i)
                        .collect();
                    if indices.is_empty() {
                        ui.label("No sounds match your search.");
                    }
                    let tile_width = (avail - SLOT_GAP * (columns - 1) as f32) / columns as f32;

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
                                    let action = ui
                                        .push_id(slot.id, |ui| {
                                            draw_slot(
                                                ui, slot, playing, recording, rejected, tile_width,
                                            )
                                        })
                                        .inner;
                                    if let Some(action) = action {
                                        actions.push(action);
                                    }
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
            self.checkpoint();
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
        match &cmd {
            AudioCommand::Play { id, .. } => {
                self.pending_playback.insert(*id);
            }
            AudioCommand::Stop(id) => {
                self.pending_playback.remove(id);
            }
            AudioCommand::StopAll => {
                self.pending_playback.clear();
            }
            _ => {}
        }
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
                    self.pending_playback.remove(&id);
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
                    self.pending_playback.remove(&id);
                    if id == TEST_SOUND_ID {
                        self.testing = false;
                    } else if id == PREVIEW_TRIM_ID {
                        self.preview_playing = false;
                    } else {
                        self.playing.remove(&id);
                    }
                }
                AudioEvent::FileMissing { id, path } => {
                    self.pending_playback.remove(&id);
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
                    self.pending_playback.remove(&id);
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
                    self.output_state = if actual.is_empty() {
                        OutputState::Unavailable(
                            fallback
                                .clone()
                                .unwrap_or_else(|| "No output available".into()),
                        )
                    } else {
                        OutputState::Ready {
                            actual: actual.clone(),
                            warning: fallback.clone(),
                        }
                    };
                    if let Some(note) = fallback {
                        if !actual.is_empty() {
                            // Preserve the requested routing preference.
                            log::info!("Actual fallback output: {actual}");
                        }
                        self.playing.clear();
                        self.preview_playing = false;
                        self.testing = false;
                        self.toast(ToastKind::Warn, note);
                    }
                }
                AudioEvent::DeviceUnavailable { id, message } => {
                    self.pending_playback.remove(&id);
                    self.output_state = OutputState::Unavailable(message.clone());
                    self.playing.remove(&id);
                    if id == TEST_SOUND_ID {
                        self.testing = false;
                    }
                    if id == PREVIEW_TRIM_ID {
                        self.preview_playing = false;
                    }
                    self.toast(ToastKind::Warn, message);
                }
                AudioEvent::StreamError(message) => {
                    self.output_state = OutputState::Unavailable(message.clone());
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

    fn check_mic_timeout(&mut self) {
        if let Some(session) = &mut self.mic_session {
            if session.attempt.is_cancelled() {
                session.phase = RecordingPhase::Cancelling;
            }
            if session.phase == RecordingPhase::Opening
                && session.since.elapsed() > MIC_START_TIMEOUT
            {
                session.attempt.cancel();
                session.phase = RecordingPhase::Cancelling;
                self.toast(ToastKind::Warn, "Microphone is still opening; waiting for the driver. Restart if it never returns.");
            }
        }
    }

    fn poll_record_events(&mut self) -> bool {
        let mut changed = false;
        let disconnected = loop {
            let RecordMessage { attempt_id, event } = match self.record_rx.try_recv() {
                Ok(message) => message,
                Err(crossbeam_channel::TryRecvError::Empty) => break false,
                Err(crossbeam_channel::TryRecvError::Disconnected) => break true,
            };
            if !self
                .mic_session
                .as_ref()
                .is_some_and(|s| s.attempt.id == attempt_id)
            {
                continue;
            }
            let event = if self
                .mic_session
                .as_ref()
                .is_some_and(|s| s.attempt.is_cancelled())
            {
                if matches!(event, RecordEvent::Started { .. }) {
                    continue;
                }
                if let RecordEvent::Saved { path, .. } = &event {
                    let _ = std::fs::remove_file(path);
                }
                RecordEvent::Cancelled
            } else {
                event
            };
            let target_board = self
                .mic_session
                .as_ref()
                .map_or(self.active_board, |s| s.board);
            if !matches!(event, RecordEvent::Started { .. }) {
                self.mic_session = None;
            }
            changed = true;

            match event {
                RecordEvent::Started { device } => {
                    if let Some(session) = &mut self.mic_session {
                        session.phase = RecordingPhase::Recording;
                        session.since = Instant::now();
                    }

                    self.toast(ToastKind::Info, format!("Recording from {device}."));
                }
                RecordEvent::Saved {
                    path,
                    duration_secs,
                    capped,
                    dropped_chunks,
                    stream_died,
                } => {
                    self.checkpoint();
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
                RecordEvent::Cancelled => {
                    self.mic_session = None;
                    self.toast(
                        ToastKind::Info,
                        "Microphone attempt cancelled. You can try again.",
                    );
                }
                RecordEvent::Failed(message) => {
                    self.mic_session = None;
                    self.toast(ToastKind::Warn, message);
                }
            }
        };
        if self.mic_session.is_some() && disconnected {
            self.mic_session = None;
            self.toast(
                ToastKind::Warn,
                "The recorder stopped unexpectedly. Restart the app to record again.",
            );
            changed = true;
        }
        changed
    }

    fn apply(&mut self, action: SlotAction) {
        match action {
            SlotAction::Rename(id) => {
                self.rename_sound = self.find_slot_mut(id).map(|s| (id, s.name.clone()));
            }
            SlotAction::MoveBefore(id, target) => {
                let source = self.active().slots.iter().position(|s| s.id == id);
                let destination = self.active().slots.iter().position(|s| s.id == target);
                if let (Some(source), Some(destination)) = (source, destination) {
                    if source != destination {
                        self.checkpoint();
                        let slot = self.active_mut().slots.remove(source);
                        let destination = if source < destination {
                            destination - 1
                        } else {
                            destination
                        };
                        self.active_mut().slots.insert(destination, slot);
                        self.mark_dirty();
                    }
                }
            }
            SlotAction::Move(id, offset) => {
                if let Some(index) = self.active().slots.iter().position(|s| s.id == id) {
                    let destination = (index as isize + offset)
                        .clamp(0, self.active().slots.len() as isize - 1)
                        as usize;
                    if index != destination {
                        self.checkpoint();
                        self.active_mut().slots.swap(index, destination);
                        self.mark_dirty();
                    }
                }
            }
            SlotAction::Volume(id, volume) => {
                self.checkpoint();
                if let Some(slot) = self.find_slot_mut(id) {
                    slot.volume = volume;
                }
                self.send(AudioCommand::SetSlotVolume(id, volume));
                self.push_hotkey_rebuild();
                self.mark_dirty();
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
                            volume: slot.volume,
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
                self.checkpoint();
                let old_key = self.find_slot_mut(id).and_then(|s| s.hotkey.take());
                if let Some(old) = old_key {
                    self.notified_rejected_hotkeys.remove(&old.display());
                }
                self.push_hotkey_rebuild();
                self.mark_dirty();
                self.toast(ToastKind::Info, "Hotkey removed.");
            }
            SlotAction::Remove(id) => {
                self.checkpoint();
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
        while let Ok(WaveResult { request, result }) = self.waves.res_rx.try_recv() {
            let valid = self.waves.complete(&request);
            let WaveRequest { id, path, .. } = request;
            changed = true;
            let current = self
                .boards
                .iter()
                .flat_map(|b| b.slots.iter())
                .find(|s| s.id == id)
                .and_then(|s| s.file_path.clone());
            if valid && current.as_ref() == Some(&path) {
                self.waves.insert(id, result);
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
            self.checkpoint();
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
                volume: self
                    .boards
                    .iter()
                    .flat_map(|b| &b.slots)
                    .find(|s| s.id == id)
                    .map_or(1.0, |s| s.volume),
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
            .map(str::to_owned)
            .filter(|stem| !stem.is_empty())
            .unwrap_or_else(|| "Sound".to_owned())
    }

    fn fill_slot_via_picker(&mut self, id: SlotId) {
        let Some(path) = Self::pick_sound_file() else {
            return;
        };
        self.checkpoint();
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

    fn checkpoint(&mut self) {
        if self.undo.len() == 20 {
            self.undo.remove(0);
        }
        self.undo.push((self.boards.clone(), self.active_board));
    }

    fn undo_edit(&mut self) {
        if let Some((boards, active)) = self.undo.pop() {
            self.send(AudioCommand::StopAll);
            self.playing.clear();
            self.preview_playing = false;
            self.testing = false;
            self.editor = None;
            self.rename_sound = None;
            self.confirm = None;
            self.recording = None;
            self.boards = boards;
            self.active_board = active;
            self.broken.clear();
            self.search.clear();
            let cached: Vec<_> = self
                .waves
                .ready
                .keys()
                .chain(self.waves.pending.keys())
                .copied()
                .collect();
            for id in cached {
                self.waves.invalidate(id);
            }
            self.push_hotkey_rebuild();
            self.mark_dirty();
            self.toast(ToastKind::Info, "Last edit undone.");
        }
    }

    fn import_sounds(&mut self, paths: Vec<PathBuf>) {
        let paths: Vec<_> = paths
            .into_iter()
            .filter(|p| {
                p.is_file()
                    && p.extension().and_then(|e| e.to_str()).is_some_and(|e| {
                        ["mp3", "wav", "ogg", "flac", "m4a", "aac"]
                            .contains(&e.to_lowercase().as_str())
                    })
            })
            .collect();
        if paths.is_empty() {
            self.toast(ToastKind::Warn, "No supported audio files selected.");
            return;
        }
        self.checkpoint();
        let count = paths.len();
        for path in paths {
            let id = self.alloc_id();
            let name = Self::display_name_for(&path);
            self.active_mut()
                .slots
                .push(SoundSlot::with_file(id, name, path));
        }
        self.search.clear();
        self.mark_dirty();
        self.toast(ToastKind::Good, format!("Added {count} sounds."));
    }

    fn add_sound_via_picker(&mut self) {
        if let Some(paths) = rfd::FileDialog::new()
            .set_title("Choose sounds")
            .add_filter("Audio", &["mp3", "wav", "ogg", "flac", "m4a", "aac"])
            .pick_files()
        {
            self.import_sounds(paths);
        }
    }

    fn rename_sound_dialog(&mut self, ctx: &egui::Context) {
        let Some((id, mut name)) = self.rename_sound.clone() else {
            return;
        };
        let mut open = true;
        let mut save = false;
        egui::Window::new("Rename sound")
            .open(&mut open)
            .collapsible(false)
            .show(ctx, |ui| {
                ui.text_edit_singleline(&mut name);
                save = ui
                    .add_enabled(!name.trim().is_empty(), Button::new("Save name"))
                    .clicked();
            });
        if save {
            self.checkpoint();
            if let Some(slot) = self.find_slot_mut(id) {
                slot.name = name.trim().into();
            }
            self.mark_dirty();
            self.rename_sound = None;
        } else {
            self.rename_sound = open.then_some((id, name));
        }
    }

    fn schedule_repaint(&self, ctx: &egui::Context) {
        if !self.playing.is_empty() || self.testing {
            ctx.request_repaint_after(TILE_ANIMATION_FRAME);
        }

        if self
            .mic_session
            .as_ref()
            .is_some_and(|s| s.phase == RecordingPhase::Recording)
        {
            ctx.request_repaint_after(TIMER_FRAME);
        }

        if let Some(toast) = &self.toast {
            let remaining = toast.until.saturating_duration_since(Instant::now());
            if remaining > Duration::ZERO {
                ctx.request_repaint_after(remaining);
            }
        }

        if let Some(session) = self
            .mic_session
            .as_ref()
            .filter(|s| s.phase == RecordingPhase::Opening)
        {
            ctx.request_repaint_after(MIC_START_TIMEOUT.saturating_sub(session.since.elapsed()));
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
        self.status_bar(ui);
        self.master_controls(ui);
        self.navigation(ui);
        self.toolbar(ui);
        self.grid(ui, &mut actions);
        self.settings_window(&ctx);
        self.recorder_window(&ctx);
        self.rename_sound_dialog(&ctx);
        self.confirm_dialog(&ctx);
        self.board_dialogs(&ctx);
        self.editor_dialog(&ctx);

        for action in actions {
            self.apply(action);
        }
    }

    fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        let dropped = ctx.input(|i| {
            i.raw
                .dropped_files
                .iter()
                .map(|f| f.path().to_path_buf())
                .collect::<Vec<_>>()
        });
        if !dropped.is_empty() {
            self.import_sounds(dropped);
        }
        self.poll_audio_events();
        if self.poll_hotkey_events() {
            ctx.request_repaint();
        }
        if self.poll_waves() {
            ctx.request_repaint();
        }
        if let Some(id) = self.editor.as_ref().map(|e| e.slot) {
            if let Some(path) = self
                .boards
                .iter()
                .flat_map(|b| &b.slots)
                .find(|s| s.id == id)
                .and_then(|s| s.file_path.clone())
            {
                self.waves.request(id, path);
            }
        }
        if self.poll_record_events() {
            ctx.request_repaint();
        }

        self.poll_bundle();
        self.check_mic_timeout();

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
    width: f32,
) -> Option<SlotAction> {
    let mut action: Option<SlotAction> = None;

    let (rect, response) = ui.allocate_exact_size(Vec2::new(width, SLOT_H), Sense::hover());

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
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    let drag = ui
                        .add(egui::Label::new("⠿").sense(Sense::drag()))
                        .on_hover_text("Drag to reorder");
                    drag.dnd_set_drag_payload(slot.id);
                    ui.menu_button("…", |ui| {
                        let mut gain = slot.volume;
                        if ui
                            .add(egui::Slider::new(&mut gain, 0.0..=1.0).text("Volume"))
                            .changed()
                        {
                            action = Some(SlotAction::Volume(slot.id, gain));
                        }
                        for (label, next) in [
                            ("Rename", SlotAction::Rename(slot.id)),
                            ("Move earlier", SlotAction::Move(slot.id, -1)),
                            ("Move later", SlotAction::Move(slot.id, 1)),
                            ("Trim sound", SlotAction::Edit(slot.id)),
                            ("Replace file", SlotAction::FindAgain(slot.id)),
                            ("Clear hotkey", SlotAction::ClearHotkey(slot.id)),
                            ("Remove sound", SlotAction::Remove(slot.id)),
                        ] {
                            if ui.button(label).clicked() {
                                action = Some(next);
                                ui.close();
                            }
                        }
                    });
                    ui.add(
                        egui::Label::new(RichText::new(&slot.name).size(15.0).strong()).truncate(),
                    )
                    .on_hover_text(&slot.name);
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
                let label = if playing { "■ Stop" } else { "▶   Play" };
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

    if let Some(source) = response.dnd_release_payload::<SlotId>() {
        if *source != slot.id {
            action = Some(SlotAction::MoveBefore(*source, slot.id));
        }
    }

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

    fn test_session(id: u64) -> RecordingSession {
        RecordingSession {
            attempt: RecordAttempt::new(id),
            board: 1,
            since: Instant::now(),
            phase: RecordingPhase::Opening,
        }
    }

    fn test_app() -> SoundboardApp {
        let ctx = egui::Context::default();
        let boards = vec![Board::empty(1, "Test")];
        let active_board = 1;
        let output_devices = vec![];
        let output_device = SYSTEM_DEFAULT_LABEL.to_owned();
        let volume = 0.7;
        let (audio_tx, audio_rx) = crossbeam_channel::unbounded();
        // The fixture does not run audio, hotkey, recorder, or persistence workers.
        let audio_rx = {
            drop(audio_rx);
            crossbeam_channel::unbounded().1
        };
        let (hotkey_tx, _) = crossbeam_channel::unbounded();
        let (_, hotkey_rx) = crossbeam_channel::unbounded();
        let (record_tx, _) = crossbeam_channel::unbounded();
        let (_, record_rx) = crossbeam_channel::unbounded();
        let (save_tx, _) = crossbeam_channel::unbounded();
        let (_, save_rx) = crossbeam_channel::unbounded();
        let mic_devices = vec![];
        let mic_device = SYSTEM_DEFAULT_LABEL.to_owned();
        let stop_hotkey = None;
        let save_error = None;
        let read_only = false;
        let close_to_tray = false;
        let next_slot_id = 1;
        SoundboardApp {
            ctx: ctx.clone(),
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
            mic_session: None,
            next_mic_attempt: 1,
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
            pending_playback: HashSet::new(),
            preview_playing: false,
            testing: false,
            confirm: None,
            toast: None,
            board_dialog: None,
            editor: None,
            waves: WaveCache::new(&ctx),
            rejected_hotkeys: HashSet::new(),
            notified_rejected_hotkeys: HashSet::new(),
            tray: None,
            close_to_tray,
            settings_open: false,
            recorder_open: false,
            search: String::new(),
            rename_sound: None,
            undo: Vec::new(),
            bundle_job: None,
            output_state: OutputState::Opening,
            quit_requested: false,
            window_visible: true,
        }
    }

    #[test]
    fn reorder_remove_and_undo_preserve_ids_and_settings() {
        let mut app = test_app();
        app.boards[0].slots = vec![
            SoundSlot::with_file(10, "First", "/one.wav"),
            SoundSlot::with_file(11, "Second", "/two.wav"),
        ];
        app.apply(SlotAction::Move(11, -1));
        assert_eq!(app.active().slots[0].id, 11);
        app.apply(SlotAction::Volume(11, 0.25));
        app.apply(SlotAction::Remove(11));
        app.undo_edit();
        assert_eq!(app.active().slots[0].id, 11);
        assert_eq!(app.active().slots[0].volume, 0.25);
        app.undo_edit();
        assert_eq!(app.active().slots[0].volume, 1.0);
        app.undo_edit();
        assert_eq!(app.active().slots[0].id, 10);
    }

    #[test]
    fn recording_completion_keeps_original_board_in_finishing_phase() {
        let mut app = test_app();
        app.boards.push(Board::empty(2, "Other"));
        app.active_board = 2;
        app.mic_session = Some(RecordingSession {
            phase: RecordingPhase::Finishing,
            ..test_session(7)
        });
        let (tx, rx) = crossbeam_channel::unbounded();
        app.record_rx = rx;
        tx.send(RecordMessage {
            attempt_id: 7,
            event: RecordEvent::Saved {
                path: "/take.wav".into(),
                duration_secs: 1.0,
                capped: false,
                dropped_chunks: 0,
                stream_died: false,
            },
        })
        .unwrap();
        app.poll_record_events();
        assert_eq!(app.boards[0].slots.len(), 1);
        assert!(app.boards[1].slots.is_empty());
        assert!(app.mic_session.is_none());
    }

    #[test]
    fn responsive_layout_renders_with_search_recording_and_output_warning() {
        for size in [
            egui::vec2(620.0, 460.0),
            egui::vec2(820.0, 620.0),
            egui::vec2(1200.0, 800.0),
        ] {
            let mut app = test_app();
            let ctx = app.ctx.clone();
            theme::apply(&ctx);
            app.boards[0].slots.push(SoundSlot::with_file(
                1,
                "A long sound name for layout validation",
                "/test.wav",
            ));
            app.output_state = OutputState::Ready {
                actual: "Speakers".into(),
                warning: Some("Using default output".into()),
            };
            app.recorder_open = true;
            app.mic_session = Some(test_session(1));
            app.mic_session.as_mut().unwrap().phase = RecordingPhase::Recording;
            for query in ["", "no matches"] {
                app.search = query.into();
                let mut output = ctx.run_ui(
                    egui::RawInput {
                        screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, size)),
                        ..Default::default()
                    },
                    |ui| {
                        app.top_bar(ui);
                        app.status_bar(ui);
                        app.master_controls(ui);
                        app.navigation(ui);
                        app.toolbar(ui);
                        assert!(ui.available_width() > 300.0);
                        assert!(ui.available_height() > 80.0);
                        app.grid(ui, &mut vec![]);
                        app.recorder_window(&ctx);
                    },
                );
                assert!(!output.shapes.is_empty());
                output.textures_delta.clear();
            }
        }
    }

    #[test]
    fn timeout_cancels_once_without_replacing_worker_or_enabling_retry() {
        let mut app = test_app();
        let (tx, _rx) = crossbeam_channel::unbounded();
        app.record_tx = tx.clone();
        app.mic_session = Some(test_session(3));
        app.mic_session.as_mut().unwrap().since =
            Instant::now() - MIC_START_TIMEOUT - Duration::from_secs(1);
        for _ in 0..10 {
            app.check_mic_timeout();
        }
        assert!(app.record_tx.same_channel(&tx));
        assert!(app.mic_session.as_ref().unwrap().attempt.is_cancelled());
        assert!(app.mic_session.is_some());
        assert_eq!(app.next_mic_attempt, 1);
    }

    #[test]
    fn waveform_outstanding_limit_includes_invalidated_requests_and_editor_retry() {
        let (req_tx, req_rx) = crossbeam_channel::bounded(MAX_CACHED_WAVEFORMS);
        let (_, res_rx) = crossbeam_channel::unbounded();
        let mut cache = WaveCache {
            ready: HashMap::new(),
            order: vec![],
            pending: HashMap::new(),
            outstanding: 0,
            req_tx,
            res_rx,
        };
        for id in 0..32 {
            cache.request(id, "/test.wav".into());
        }
        assert_eq!(cache.outstanding, 16);
        cache.invalidate(0);
        cache.request(99, "/editor.wav".into());
        assert!(!cache.pending.contains_key(&99));
        assert!(!cache.complete(&req_rx.try_recv().unwrap()));
        cache.request(99, "/editor.wav".into());
        assert!(cache.pending.contains_key(&99));
        assert_eq!(cache.outstanding, 16);
        assert_eq!(req_rx.len(), 16);
    }

    #[test]
    fn fallback_does_not_replace_saved_output_preference() {
        let mut app = test_app();
        app.output_device = "Requested cable".into();
        let (tx, rx) = crossbeam_channel::unbounded();
        app.audio_rx = rx;
        tx.send(AudioEvent::DeviceReady {
            actual: "Speakers".into(),
            fallback: Some("Using speakers".into()),
        })
        .unwrap();
        app.poll_audio_events();
        assert_eq!(
            app.snapshot_state().output_device_name.as_deref(),
            Some("Requested cable")
        );
    }

    #[test]
    fn recorder_disconnect_clears_startup_state() {
        let mut app = test_app();
        app.mic_session = Some(test_session(1));

        assert!(app.poll_record_events());
        assert!(app.mic_session.is_none());
        assert!(app.mic_session.is_none());
    }

    #[test]
    fn cancelled_saved_event_discards_take_instead_of_adding_slot() {
        let mut app = test_app();
        let attempt = RecordAttempt::new(3);
        attempt.cancel();
        app.mic_session = Some(RecordingSession {
            attempt,
            ..test_session(1)
        });
        let path = std::env::temp_dir().join(format!("cancelled-take-{}.wav", std::process::id()));
        std::fs::write(&path, b"cancelled take").unwrap();
        let (tx, rx) = crossbeam_channel::unbounded();
        app.record_rx = rx;
        tx.send(RecordMessage {
            attempt_id: 3,
            event: RecordEvent::Saved {
                path: path.clone(),
                duration_secs: 1.0,
                capped: false,
                dropped_chunks: 0,
                stream_died: false,
            },
        })
        .unwrap();
        app.poll_record_events();
        assert!(!path.exists());
        assert!(app.boards[0].slots.is_empty());
        assert!(app.mic_session.is_none());
    }

    #[test]
    fn device_errors_preserve_bindings_but_file_errors_disable_them() {
        let mut app = test_app();
        let mut slot = SoundSlot::with_file(7, "valid", "/valid.wav");
        slot.hotkey = Some(Hotkey::simple("F7"));
        app.boards[0].slots.push(slot);
        let (tx, rx) = crossbeam_channel::unbounded();
        app.audio_rx = rx;
        tx.send(AudioEvent::DeviceUnavailable {
            id: 7,
            message: "offline".into(),
        })
        .unwrap();
        app.poll_audio_events();
        assert!(!app.broken.contains(&7));
        assert!(app.boards[0].slots[0].hotkey.is_some());
        tx.send(AudioEvent::DecodeError {
            id: 7,
            message: "bad file".into(),
        })
        .unwrap();
        app.poll_audio_events();
        assert!(app.broken.contains(&7));
    }

    #[test]
    fn rebuild_stays_suspended_during_capture() {
        let mut app = test_app();
        let (tx, rx) = crossbeam_channel::unbounded();
        app.hotkey_tx = tx;
        app.stop_hotkey = Some(Hotkey::simple("F8"));
        app.recording = Some(7);
        app.push_hotkey_rebuild();
        assert!(matches!(rx.try_recv(), Ok(HotkeyCommand::Rebuild(b)) if b.is_empty()));
        app.recording = None;
        app.push_hotkey_rebuild();
        assert!(matches!(rx.try_recv(), Ok(HotkeyCommand::Rebuild(b)) if b.len() == 1));
    }

    #[test]
    fn waveform_completion_enforces_capacity_and_rejects_stale_tokens() {
        let mut cache = WaveCache::new(&egui::Context::default());
        for id in 0..40 {
            cache.insert(id, Err("test".into()));
        }
        assert_eq!(cache.ready.len(), MAX_CACHED_WAVEFORMS);
        let old = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        cache.pending.insert(99, old.clone());
        cache.outstanding = 1;
        cache.invalidate(99);
        let new = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        cache.pending.insert(99, new.clone());
        cache.outstanding += 1;
        assert!(!cache.complete(&WaveRequest {
            id: 99,
            path: "/old".into(),
            token: old
        }));
        assert!(std::sync::Arc::ptr_eq(
            cache.pending.get(&99).unwrap(),
            &new
        ));
        assert_eq!(cache.outstanding, 1);
    }

    #[test]
    fn cancelled_and_stale_recording_events_cannot_start_a_take() {
        let mut app = test_app();
        let attempt = RecordAttempt::new(2);
        attempt.cancel();
        app.mic_session = Some(RecordingSession {
            attempt,
            ..test_session(1)
        });

        let (tx, rx) = crossbeam_channel::unbounded();
        app.record_rx = rx;
        for attempt_id in [1, 2] {
            tx.send(RecordMessage {
                attempt_id,
                event: RecordEvent::Started {
                    device: "late".into(),
                },
            })
            .unwrap();
        }
        app.poll_record_events();
        assert!(!app
            .mic_session
            .as_ref()
            .is_some_and(|s| s.phase == RecordingPhase::Recording));
        assert!(app.mic_session.is_some());
        tx.send(RecordMessage {
            attempt_id: 2,
            event: RecordEvent::Cancelled,
        })
        .unwrap();
        app.poll_record_events();
        assert!(app.mic_session.is_none());
        assert!(app.mic_session.is_none());
    }

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
