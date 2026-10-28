//! The palette's ratatui twin (ADR 0020): every color decision still lives
//! in [`crate::render::palette`] — this module re-expresses the same role
//! semantics in ratatui's vocabulary, because `console::Style` (0.16)
//! exposes no getters to convert from. The ANSI-16 roles are one-line twins
//! (`success` = green + bold in both vocabularies); the truecolor roles
//! (badge, neutral gray) read the palette's own theme-aware triples via
//! `palette::badge_rgb` / `palette::neutral_gray_rgb` so the WCAG-guarded
//! tables stay the single source. The twin table is pinned by tests against
//! the palette's role semantics.

use ratatui::style::{Color, Modifier, Style};

use crate::render::palette;

/// ✓ glyphs and landed outcomes — [`palette::success`] twin.
pub(crate) fn success() -> Style {
    Style::new().fg(Color::Green).add_modifier(Modifier::BOLD)
}

/// ✗ glyphs and failed outcomes — [`palette::failure`] twin.
pub(crate) fn failure() -> Style {
    Style::new().fg(Color::Red).add_modifier(Modifier::BOLD)
}

/// ⚠ attention rows — [`palette::pending`] twin.
pub(crate) fn pending() -> Style {
    Style::new().fg(Color::Yellow).add_modifier(Modifier::BOLD)
}

/// Soft warning accents — [`palette::caution`] twin.
pub(crate) fn caution() -> Style {
    Style::new().fg(Color::Yellow)
}

/// Secondary text, hints, placeholders — [`palette::muted`] twin.
pub(crate) fn muted() -> Style {
    Style::new().add_modifier(Modifier::DIM)
}

/// The value a row is about — [`palette::emphasis`] twin.
pub(crate) fn emphasis() -> Style {
    Style::new().add_modifier(Modifier::BOLD)
}

/// Default foreground — [`palette::plain`] twin.
pub(crate) fn plain() -> Style {
    Style::new()
}

/// Setup badge color: a stable FNV hash of `name` into the palette's
/// fallback tables — [`palette::badge`] twin, reading the same triple.
pub(crate) fn badge(name: &str) -> Style {
    rgb(palette::badge_rgb(name))
}

fn rgb((r, g, b): (u8, u8, u8)) -> Style {
    Style::new().fg(Color::Rgb(r, g, b))
}
