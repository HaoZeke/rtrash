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
    Ok(HudTray { _keep: () })
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
