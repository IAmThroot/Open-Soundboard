#[cfg_attr(not(windows), allow(dead_code))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrayAction {
    Open,

    Quit,
}

#[cfg(windows)]
const ID_OPEN: &str = "open";
#[cfg(windows)]
const ID_QUIT: &str = "quit";

#[cfg(windows)]
pub struct Tray {
    _icon: tray_icon::TrayIcon,

    rx: crossbeam_channel::Receiver<TrayAction>,
}

#[cfg(windows)]
impl Tray {
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

            click_ctx.request_repaint();
        }));

        let menu_tx = tx.clone();
        let menu_ctx = ctx.clone();
        tray_icon::menu::MenuEvent::set_event_handler(Some(
            move |event: tray_icon::menu::MenuEvent| {
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
            },
        ));

        log::info!("Tray icon created.");
        Some(Self { _icon: icon, rx })
    }

    pub fn poll(&self) -> Vec<TrayAction> {
        self.rx.try_iter().collect()
    }
}

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
