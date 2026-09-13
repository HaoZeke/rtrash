//! Packaged HUD mark (16×16 RGBA). No extra image crate.

/// Solid mocha-blue tile used as the window and tray icon.
pub fn mark_rgba() -> (Vec<u8>, u32, u32) {
    let w = 16u32;
    let h = 16u32;
    let mut rgba = vec![0u8; (w * h * 4) as usize];
    for y in 0..h {
        for x in 0..w {
            let i = ((y * w + x) * 4) as usize;
            let edge = x == 0 || y == 0 || x + 1 == w || y + 1 == h;
            if edge {
                rgba[i] = 0x18;
                rgba[i + 1] = 0x18;
                rgba[i + 2] = 0x25;
            } else {
                rgba[i] = 0x89;
                rgba[i + 1] = 0xb4;
                rgba[i + 2] = 0xfa;
            }
            rgba[i + 3] = 0xff;
        }
    }
    (rgba, w, h)
}

/// iced window icon from the packaged mark.
pub fn window_icon() -> Option<iced::window::Icon> {
    let (rgba, w, h) = mark_rgba();
    iced::window::icon::from_rgba(rgba, w, h).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn window_icon_is_some() {
        assert!(window_icon().is_some());
    }
}
