//! StatusNotifier tray for the long-lived HUD. Quit exits the HUD only.

use std::sync::mpsc::{self, Receiver, SyncSender};
use std::sync::{Mutex, OnceLock};

/// Tray identity (matches overlay app id).
pub const TRAY_ID: &str = "me.rgoswami.rtrash-hud";
/// Tooltip.
pub const TRAY_TOOLTIP: &str = "rtrash";
pub const MENU_SHOW_ID: &str = "show";
pub const MENU_QUIT_ID: &str = "quit";
pub const MENU_SHOW_LABEL: &str = "Show";
pub const MENU_QUIT_LABEL: &str = "Quit rtrash HUD";

/// Operator action the iced loop should run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrayAction {
    Show,
    Toggle,
    Quit,
}

/// Mouse button on a tray click.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrayButton {
    Left,
    Right,
    Other,
}

/// Button edge for a tray click.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrayClickState {
    Down,
    Up,
}

/// Keeps the platform tray service alive (no-op when the host has no tray).
pub struct HudTray {
    _keep: (),
}

/// Map a menu item id to an action.
pub fn action_from_menu_id(id: &str) -> Option<TrayAction> {
    match id {
        MENU_SHOW_ID => Some(TrayAction::Show),
        MENU_QUIT_ID => Some(TrayAction::Quit),
        _ => None,
    }
}

/// Left-button release toggles. Other clicks do not.
pub fn action_from_click(button: TrayButton, state: TrayClickState) -> Option<TrayAction> {
    match (button, state) {
        (TrayButton::Left, TrayClickState::Up) => Some(TrayAction::Toggle),
        _ => None,
    }
}

/// Quit the HUD process. Does not empty the trash.
pub fn quit_process() -> ! {
    std::process::exit(0)
}

/// Register the tray. Failure is non-fatal (headless / no StatusNotifier).
pub fn install() -> Result<HudTray, String> {
    let _ = action_sender();
    #[cfg(target_os = "linux")]
    {
        linux::install().map_err(|e| e.to_string())?;
    }
    Ok(HudTray { _keep: () })
}

fn rgba_to_argb(rgba: &[u8]) -> Vec<u8> {
    let mut out = rgba.to_vec();
    let mut i = 0;
    while i + 3 < out.len() {
        out[i..i + 4].rotate_right(1);
        i += 4;
    }
    out
}

#[cfg(target_os = "linux")]
mod linux {
    use super::{
        action_sender, rgba_to_argb, TrayAction, MENU_QUIT_LABEL, MENU_SHOW_LABEL, TRAY_ID,
        TRAY_TOOLTIP,
    };
    use ksni::blocking::TrayMethods;
    use ksni::{Icon, Tray};

    pub struct RtrashTray {
        tx: std::sync::mpsc::SyncSender<TrayAction>,
        icon: Icon,
    }

    impl Tray for RtrashTray {
        fn id(&self) -> String {
            TRAY_ID.into()
        }
        fn title(&self) -> String {
            TRAY_TOOLTIP.into()
        }
        fn category(&self) -> ksni::Category {
            ksni::Category::ApplicationStatus
        }
        fn icon_pixmap(&self) -> Vec<Icon> {
            vec![self.icon.clone()]
        }
        fn activate(&mut self, _x: i32, _y: i32) {
            let _ = self.tx.send(TrayAction::Toggle);
        }
        fn menu(&self) -> Vec<ksni::MenuItem<Self>> {
            use ksni::menu::*;
            vec![
                StandardItem {
                    label: MENU_SHOW_LABEL.into(),
                    activate: Box::new(|this: &mut Self| {
                        let _ = this.tx.send(TrayAction::Show);
                    }),
                    ..Default::default()
                }
                .into(),
                StandardItem {
                    label: MENU_QUIT_LABEL.into(),
                    activate: Box::new(|this: &mut Self| {
                        let _ = this.tx.send(TrayAction::Quit);
                    }),
                    ..Default::default()
                }
                .into(),
            ]
        }
    }

    pub fn install() -> Result<(), String> {
        let (rgba, width, height) = crate::brand::mark_rgba();
        let tray = RtrashTray {
            tx: action_sender(),
            icon: Icon {
                width: i32::try_from(width).map_err(|e| e.to_string())?,
                height: i32::try_from(height).map_err(|e| e.to_string())?,
                data: rgba_to_argb(&rgba),
            },
        };
        let _handle = tray.spawn().map_err(|err| err.to_string())?;
        // Keep the handle by leaking: the HUD process owns the tray for life.
        std::mem::forget(_handle);
        Ok(())
    }
}

/// Non-blocking tray action.
pub fn try_recv() -> Option<TrayAction> {
    let guard = action_pair().1.lock().ok()?;
    guard.try_recv().ok()
}

fn action_pair() -> &'static (SyncSender<TrayAction>, Mutex<Receiver<TrayAction>>) {
    static PAIR: OnceLock<(SyncSender<TrayAction>, Mutex<Receiver<TrayAction>>)> = OnceLock::new();
    PAIR.get_or_init(|| {
        let (tx, rx) = mpsc::sync_channel(16);
        (tx, Mutex::new(rx))
    })
}

fn action_sender() -> SyncSender<TrayAction> {
    action_pair().0.clone()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn left_up_toggles() {
        assert_eq!(
            action_from_click(TrayButton::Left, TrayClickState::Up),
            Some(TrayAction::Toggle)
        );
        assert_eq!(
            action_from_click(TrayButton::Right, TrayClickState::Up),
            None
        );
    }

    #[test]
    fn menu_quit_is_quit_not_empty() {
        assert_eq!(action_from_menu_id(MENU_QUIT_ID), Some(TrayAction::Quit));
        assert_ne!(MENU_QUIT_LABEL.to_ascii_lowercase().contains("empty"), true);
    }
}
