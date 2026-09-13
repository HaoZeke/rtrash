//! Mocha tokens on icedtea 0.17.

use iced::Color;
use icedtea::m3::{Density, DensityName, ElevationPolicy, ShapePolicy};
use icedtea::theme::Tokens;

/// Mocha mapped onto icedtea semantic tokens.
pub fn tokens() -> Tokens {
    let base = Color::from_rgb8(0x1e, 0x1e, 0x2e);
    let surface = Color::from_rgb8(0x31, 0x32, 0x44);
    let mantle = Color::from_rgb8(0x18, 0x18, 0x25);
    let text = Color::from_rgb8(0xcd, 0xd6, 0xf4);
    let subtext = Color::from_rgb8(0xa6, 0xad, 0xc8);
    let blue = Color::from_rgb8(0x89, 0xb4, 0xfa);
    let peach = Color::from_rgb8(0xfa, 0xb3, 0x87);
    let green = Color::from_rgb8(0xa6, 0xe3, 0xa1);
    let yellow = Color::from_rgb8(0xf9, 0xe2, 0xaf);
    let red = Color::from_rgb8(0xf3, 0x8b, 0xa8);
    let overlay = Color::from_rgb8(0x58, 0x5b, 0x70);
    Tokens::from_aliases(
        base, surface, mantle, text, subtext, blue, peach, green, yellow, red, overlay,
    )
    .with_density(Density::named(DensityName::Compact))
    .with_shape(ShapePolicy::Material)
    .with_elevation(ElevationPolicy::Flat)
}

/// iced theme from the mocha tokens.
pub fn theme() -> iced::Theme {
    icedtea::theme::iced_theme("rtrash-mocha", tokens())
}
