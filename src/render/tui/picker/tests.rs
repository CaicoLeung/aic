//! Picker unit tests: filter narrowing, cursor clamping, and the tag
//! mapping Enter reports — the state-machine half, no terminal needed.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::style::Style;

use super::{Pick, Picker, item};

fn key(code: KeyCode) -> KeyEvent {
    KeyEvent::from(code)
}

fn ctrl_c() -> KeyEvent {
    KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL)
}

fn three_rows() -> Picker<&'static str> {
    Picker::new(
        "Pick",
        vec![
            (item(vec![("alpha", Style::new())]), "alpha"),
            (item(vec![("beta", Style::new())]), "beta"),
            (item(vec![("gamma", Style::new())]), "gamma"),
        ],
    )
}

/// Enter picks the row under the cursor, reported as that row's tag — the
/// dispatch contract every screen relies on.
#[test]
fn enter_reports_the_rows_tag() {
    let mut p = three_rows();
    p.on_key(key(KeyCode::Down));
    assert_eq!(p.on_key(key(KeyCode::Enter)), Some(Pick::Selected("beta")));
}

/// Typing narrows the visible rows; Enter then reports the surviving
/// match's own tag, not its position in the filtered view.
#[test]
fn filter_narrows_and_enter_maps_back_to_the_original_tag() {
    let mut p = three_rows();
    p.on_key(key(KeyCode::Char('a'))); // filter "a"
    p.on_key(key(KeyCode::Char('m'))); // filter "am" -> "gamma"
    assert_eq!(p.on_key(key(KeyCode::Enter)), Some(Pick::Selected("gamma")));
}

/// The cursor never runs past either end — clamped, not wrapped.
#[test]
fn cursor_clamps_at_both_ends() {
    let mut p = three_rows();
    p.on_key(key(KeyCode::Up));
    assert_eq!(p.on_key(key(KeyCode::Enter)), Some(Pick::Selected("alpha")));
    for _ in 0..5 {
        p.on_key(key(KeyCode::Down));
    }
    assert_eq!(p.on_key(key(KeyCode::Enter)), Some(Pick::Selected("gamma")));
}

/// An empty filtered view swallows Enter (nothing to pick) instead of
/// reporting a stale row.
#[test]
fn enter_on_no_matches_is_a_no_op() {
    let mut p = three_rows();
    p.on_key(key(KeyCode::Char('z')));
    assert_eq!(p.on_key(key(KeyCode::Enter)), None);
}

/// Backspace edits the filter from the right and restores the full list.
#[test]
fn backspace_widens_the_filter_again() {
    let mut p = three_rows();
    p.on_key(key(KeyCode::Char('b')));
    p.on_key(key(KeyCode::Char('e')));
    p.on_key(key(KeyCode::Backspace));
    assert_eq!(p.on_key(key(KeyCode::Enter)), Some(Pick::Selected("beta")));
}

/// Esc is Back and Ctrl-C is Cancel on every screen — the wizard's
/// whole-surface exit semantics live here once.
#[test]
fn esc_backs_out_and_ctrl_c_cancels() {
    let mut p = three_rows();
    assert_eq!(p.on_key(key(KeyCode::Esc)), Some(Pick::Back));
    assert_eq!(p.on_key(ctrl_c()), Some(Pick::Cancel));
}
