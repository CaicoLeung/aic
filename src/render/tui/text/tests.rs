//! Text-input unit tests: insertion, caret movement over multi-byte chars,
//! backspace, and the empty-value validator retry — all state machine, no
//! terminal.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use super::{TextAct, TextInput};

fn key(code: KeyCode) -> KeyEvent {
    KeyEvent::from(code)
}

fn ctrl_c() -> KeyEvent {
    KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL)
}

fn field(allow_empty: bool) -> TextInput {
    TextInput::new("Value", Some("kept"), allow_empty, "value cannot be empty")
}

/// Chars insert at the caret and Enter submits the whole value.
#[test]
fn typing_then_enter_submits_the_value() {
    let mut t = field(true);
    for c in "abc".chars() {
        t.on_key(key(KeyCode::Char(c)));
    }
    assert_eq!(
        t.on_key(key(KeyCode::Enter)),
        Some(TextAct::Submit("abc".into()))
    );
}

/// An empty submit on a field that requires a value is refused and retried
/// — the validator fires inline, the prompt never exits.
#[test]
fn empty_submit_on_required_field_is_refused() {
    let mut t = field(false);
    assert_eq!(t.on_key(key(KeyCode::Enter)), None);
    // The retry then accepts a real value.
    t.on_key(key(KeyCode::Char('x')));
    assert_eq!(
        t.on_key(key(KeyCode::Enter)),
        Some(TextAct::Submit("x".into()))
    );
}

/// Backspace deletes behind the caret; ←/→ move it by char, not byte —
/// multi-byte input must not split a char.
#[test]
fn caret_moves_and_deletes_by_char_not_byte() {
    let mut t = field(true);
    for c in "a你b".chars() {
        t.on_key(key(KeyCode::Char(c)));
    }
    t.on_key(key(KeyCode::Left)); // caret between 你 and b
    t.on_key(key(KeyCode::Backspace)); // deletes 你 whole
    assert_eq!(
        t.on_key(key(KeyCode::Enter)),
        Some(TextAct::Submit("ab".into()))
    );
}

/// Esc is Back and Ctrl-C is Cancel — same semantics as the picker.
#[test]
fn esc_backs_out_and_ctrl_c_cancels() {
    let mut t = field(true);
    assert_eq!(t.on_key(key(KeyCode::Esc)), Some(TextAct::Back));
    assert_eq!(t.on_key(ctrl_c()), Some(TextAct::Cancel));
}
