//! HUD window-mode and shortcut prefs.

/// `RTRASH_HUD_WINDOW=1` boots a decorated managed window.
pub const WINDOW_MODE_ENV: &str = "RTRASH_HUD_WINDOW";
/// `RTRASH_HUD_SHORTCUT` (default Ctrl+Shift+T). Unused on Wayland.
pub const SHORTCUT_ENV: &str = "RTRASH_HUD_SHORTCUT";

/// True for 1 / true / yes.
pub fn env_flag(name: &str) -> bool {
    matches!(
        std::env::var(name).ok().as_deref().map(str::trim),
        Some(v) if matches!(v.to_ascii_lowercase().as_str(), "1" | "true" | "yes")
    )
}

/// Decorated window boot.
pub fn window_mode() -> bool {
    env_flag(WINDOW_MODE_ENV)
}

/// Parse a chord like `Ctrl+Shift+T`. Empty / invalid is None.
pub fn parse_shortcut(raw: &str) -> Option<Shortcut> {
    let mut ctrl = false;
    let mut shift = false;
    let mut alt = false;
    let mut key = None;
    for part in raw.split('+') {
        match part.trim().to_ascii_lowercase().as_str() {
            "ctrl" | "control" | "cmd" => ctrl = true,
            "shift" => shift = true,
            "alt" | "mod1" => alt = true,
            "" => {}
            other if other.len() == 1 => {
                key = other.chars().next();
            }
            _ => return None,
        }
    }
    Some(Shortcut {
        ctrl,
        shift,
        alt,
        key: key?,
    })
}

/// In-process global chord. Linux registers only when `WAYLAND_DISPLAY` is unset.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Shortcut {
    pub ctrl: bool,
    pub shift: bool,
    pub alt: bool,
    pub key: char,
}

/// Active shortcut from env or the default.
pub fn configured_shortcut() -> Shortcut {
    std::env::var(SHORTCUT_ENV)
        .ok()
        .as_deref()
        .and_then(parse_shortcut)
        .unwrap_or(Shortcut {
            ctrl: true,
            shift: true,
            alt: false,
            key: 't',
        })
}

/// True when an in-process grab should be attempted (not Wayland).
pub fn should_register_global() -> bool {
    std::env::var_os("WAYLAND_DISPLAY").is_none()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_ctrl_shift_t() {
        let s = parse_shortcut("Ctrl+Shift+T").unwrap();
        assert!(s.ctrl && s.shift && !s.alt && s.key == 't');
    }

    #[test]
    fn wayland_skips_register() {
        #[allow(unused_unsafe)]
        unsafe {
            std::env::set_var("WAYLAND_DISPLAY", "wayland-0");
        }
        assert!(!should_register_global());
        #[allow(unused_unsafe)]
        unsafe {
            std::env::remove_var("WAYLAND_DISPLAY");
        }
    }
}
