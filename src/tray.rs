//! System-tray icon (Windows), so the soundboard can live in the tray while
//! its hotkeys keep working.
//!
//! The icon is drawn from [`crate::icon_art`] at runtime — the same artwork
//! the shipped `.exe` icon is generated from, so they can never drift.
//!
//! Two design points worth knowing:
//! - The tray icon must be created on the main thread (done from `eframe`'s
//!   app-creation callback) and lives as long as the app.
//! - Events arrive through `set_event_handler` callbacks that push into a
//!   channel **and** call `Context::request_repaint()`. That repaint matters:
//!   while the window is hidden to the tray, egui has no reason to run a
//!   frame, so without the wake-up a click on the tray icon would sit
//!   unprocessed. The handlers run on the main thread, so calling into egui
//!   from them is safe.
//!
//! On non-Windows targets the type is a no-op stub so calling code stays
//! platform-agnostic (the app ships on Windows only).

/// What the user asked the tray to do.
// On non-Windows the variants are never constructed (the stub never emits
// anything), so silence the dead-code lint there rather than duplicating.
#[cfg_attr(not(windows), allow(dead_code))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrayAction {
    /// Show and focus the main window.
    Open,
    /// Really quit the app (through the normal save-on-exit path).
    Quit,
}

/// Menu item ids shared by the builder and the event handling.
#[cfg(windows)]
const ID_OPEN: &str = "open";
#[cfg(windows)]
const ID_QUIT: &str = "quit";

#[cfg(windows)]
pub struct Tray {
    /// Kept alive for the app's lifetime; dropping it removes the tray icon.
    _icon: tray_icon::TrayIcon,
    /// Actions pushed by the tray event handlers.
    rx: crossbeam_channel::Receiver<TrayAction>,
}

#[cfg(windows)]
impl Tray {
    /// Build the tray icon and wire its events. Returns `None` (with a log
    /// line) if Windows refuses — the app stays fully usable, it just has no
    /// tray presence.
    pub fn new(ctx: &egui::Context) -> Option<Self> {
        use tray_icon::menu::{Menu, MenuItem, PredefinedMenuItem};
        use tray_icon::{Icon, TrayIconBuilder};

        const SIZE: u32 = 64;
        let rgba = crate::icon_art::draw(SIZE);
        let icon = match Icon::from_rgba(rgba, SIZE, SIZE) {
            Ok(icon) => icon,
            Err(err) => {
                log::warn!("Could not build the tray icon: {err}");
                return None;
            }
        };

        let open = MenuItem::with_id(ID_OPEN, "Open Soundboard", true, None);
        let quit = MenuItem::with_id(ID_QUIT, "Quit", true, None);
        let separator = PredefinedMenuItem::separator();
        let menu = Menu::new();
        let built = menu
            .append(&open)
            .and_then(|()| menu.append(&separator))
            .and_then(|()| menu.append(&quit));
        if let Err(err) = built {
            log::warn!("Could not build the tray menu: {err}");
            return None;
        }

        let icon = match TrayIconBuilder::new()
            .with_menu(Box::new(menu))
            .with_icon(icon)
            .with_tooltip("Open Soundboard — hotkeys keep working")
            // Left click opens the window directly; the menu stays on right
            // click, which is what Windows users expect.
            .with_menu_on_left_click(false)
            .with_menu_on_right_click(true)
            .build()
        {
            Ok(icon) => icon,
            Err(err) => {
                log::warn!("Could not create the tray icon: {err}");
                return None;
            }
        };

        let (tx, rx) = crossbeam_channel::unbounded::<TrayAction>();

        use tray_icon::{MouseButton, MouseButtonState, TrayIconEvent};
        let click_tx = tx.clone();
        let click_ctx = ctx.clone();
        TrayIconEvent::set_event_handler(Some(move |event| {
            if let TrayIconEvent::Click {
                button: MouseButton::Left,
                button_state: MouseButtonState::Up,
                ..
            } = event
            {
                let _ = click_tx.send(TrayAction::Open);
            }
            // Wake egui: the window may be hidden in the tray with no reason
            // to run a frame.
            click_ctx.request_repaint();
        }));

        let menu_tx = tx.clone();
        let menu_ctx = ctx.clone();
        tray_icon::menu::MenuEvent::set_event_handler(Some(move |event: tray_icon::menu::MenuEvent| {
            let action = match event.id.0.as_str() {
                ID_OPEN => TrayAction::Open,
                ID_QUIT => TrayAction::Quit,
                other => {
                    log::debug!("Ignoring unknown tray menu item {other:?}.");
                    return;
                }
            };
            let _ = menu_tx.send(action);
            menu_ctx.request_repaint();
        }));

        log::info!("Tray icon created.");
        Some(Self { _icon: icon, rx })
    }

    /// Drain pending tray events. Never blocks; safe to call every frame.
    pub fn poll(&self) -> Vec<TrayAction> {
        self.rx.try_iter().collect()
    }
}

/// Stub for non-Windows builds: no tray, no actions.
#[cfg(not(windows))]
pub struct Tray;

#[cfg(not(windows))]
impl Tray {
    pub fn new(_ctx: &egui::Context) -> Option<Self> {
        None
    }

    pub fn poll(&self) -> Vec<TrayAction> {
        Vec::new()
    }
}