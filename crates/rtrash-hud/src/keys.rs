//! Map iced keys onto [`rtrash::tui_binds::Action`].

use iced::keyboard::{key::Named, Key, Modifiers};
use rtrash::tui_binds::{Action, Keymap};

use crate::catalog::Pane;

/// HUD interaction mode (same roles as the TUI).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// Catalog browse.
    Browse,
    /// Filter draft.
    Filter,
    /// Confirm restore / empty.
    Confirm,
    /// Help overlay.
    Help,
    /// Put path field is focused.
    PutInput,
}

/// Convert an iced key event into a TUI action using the shared keymap.
pub fn resolve(map: &Keymap, key: &Key, mods: Modifiers, mode: Mode) -> Option<Action> {
    let (code, km) = iced_to_crossterm(key, mods)?;
    match mode {
        Mode::Browse => map.resolve_browse(code, km),
        Mode::Filter => map.resolve_filter(code, km),
        Mode::Confirm => map.resolve_confirm(code, km),
        Mode::Help => map.resolve_help(code, km),
        Mode::PutInput => None,
    }
}

/// Pane switch from digit keys (not in the TUI catalog).
pub fn pane_digit(key: &Key, mods: Modifiers) -> Option<Pane> {
    if mods.control() || mods.alt() {
        return None;
    }
    match key {
        Key::Character(c) => {
            let d = c.chars().next()?.to_digit(10)? as u8;
            Pane::from_digit(d)
        }
        _ => None,
    }
}

fn iced_to_crossterm(
    key: &Key,
    mods: Modifiers,
) -> Option<(crossterm::event::KeyCode, crossterm::event::KeyModifiers)> {
    use crossterm::event::{KeyCode, KeyModifiers};
    let mut km = KeyModifiers::NONE;
    if mods.control() {
        km |= KeyModifiers::CONTROL;
    }
    if mods.alt() {
        km |= KeyModifiers::ALT;
    }
    if mods.shift() {
        km |= KeyModifiers::SHIFT;
    }
    let code = match key {
        Key::Named(Named::ArrowUp) => KeyCode::Up,
        Key::Named(Named::ArrowDown) => KeyCode::Down,
        Key::Named(Named::ArrowLeft) => KeyCode::Left,
        Key::Named(Named::ArrowRight) => KeyCode::Right,
        Key::Named(Named::PageUp) => KeyCode::PageUp,
        Key::Named(Named::PageDown) => KeyCode::PageDown,
        Key::Named(Named::Home) => KeyCode::Home,
        Key::Named(Named::End) => KeyCode::End,
        Key::Named(Named::Enter) => KeyCode::Enter,
        Key::Named(Named::Escape) => KeyCode::Esc,
        Key::Named(Named::Tab) => KeyCode::Tab,
        Key::Named(Named::Backspace) => KeyCode::Backspace,
        Key::Named(Named::Delete) => KeyCode::Delete,
        Key::Named(Named::Space) => KeyCode::Char(' '),
        Key::Named(Named::F11) => KeyCode::F(11),
        Key::Character(c) => {
            let ch = c.chars().next()?;
            KeyCode::Char(ch)
        }
        _ => return None,
    };
    Some((code, km))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn browse_j_is_move_down() {
        let map = Keymap::builtin();
        let act = resolve(
            &map,
            &Key::Character("j".into()),
            Modifiers::empty(),
            Mode::Browse,
        );
        assert_eq!(act, Some(Action::MoveDown));
    }

    #[test]
    fn digit_2_is_restore_pane() {
        assert_eq!(
            pane_digit(&Key::Character("2".into()), Modifiers::empty()),
            Some(Pane::Restore)
        );
    }
}
