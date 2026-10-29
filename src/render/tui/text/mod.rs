//! The shared text input (ADR 0020): a labeled single-line editor with the
//! wizard's Esc/Ctrl-C semantics and an inline validator. Keys are editing
//! commands; Enter submits (or shows the empty-hint and retries when the
//! value may not be empty). The caret renders as a reversed cell — a
//! visible insertion point without per-frame cursor-mode toggling. Pure
//! state + [`TextInput::on_key`] / [`TextInput::draw`], hermetically
//! testable with `TestBackend`.
use crossterm::event::{KeyCode, KeyEvent};
use ratatui::layout::Rect;
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use super::ICON_ALERT;
use super::is_cancel;
use crate::render::layout::MARGIN;
use crate::render::tui::style as tstyle;

/// The outcome of one key press on a text input.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum TextAct {
    Submit(String),
    /// Esc: go back one level.
    Back,
    /// Ctrl-C / closed input: cancel the whole surface.
    Cancel,
}

/// A single-line text field with validation.
#[derive(Clone)]
pub(crate) struct TextInput {
    prompt: String,
    value: String,
    /// Caret position as a char boundary index into `value`.
    cursor: usize,
    /// The kept value surfaced as a help line (the "current" hint).
    initial: Option<String>,
    allow_empty: bool,
    empty_hint: String,
    error: Option<String>,
}

impl TextInput {
    pub(crate) fn new(
        prompt: &str,
        initial: Option<&str>,
        allow_empty: bool,
        empty_hint: &str,
    ) -> Self {
        Self {
            prompt: prompt.to_string(),
            value: String::new(),
            cursor: 0,
            initial: initial.map(str::to_string),
            allow_empty,
            empty_hint: empty_hint.to_string(),
            error: None,
        }
    }

    /// Map a key press to a [`TextAct`]; printable chars insert at the caret,
    /// Backspace deletes behind it, ←/→ move by char, Enter submits (the
    /// empty-hint fires and retries when empty is not allowed).
    pub(crate) fn on_key(&mut self, key: KeyEvent) -> Option<TextAct> {
        if is_cancel(key) {
            return Some(TextAct::Cancel);
        }
        match key.code {
            KeyCode::Enter => {
                // Leading/trailing whitespace never carries meaning in the
                // values this field edits (keys, base URLs, models) — and a
                // whitespace-only paste must fail validation like an empty
                // one instead of submitting a blank key/URL.
                if self.value.trim().is_empty() && !self.allow_empty {
                    self.error = Some(
                        if self.empty_hint.is_empty() {
                            "value cannot be empty"
                        } else {
                            &self.empty_hint
                        }
                        .to_string(),
                    );
                    return None;
                }
                self.error = None;
                Some(TextAct::Submit(self.value.trim().to_string()))
            }
            KeyCode::Esc => Some(TextAct::Back),
            KeyCode::Backspace => {
                if self.cursor > 0 {
                    let boundary = self.prev_boundary();
                    self.value.replace_range(boundary..self.cursor, "");
                    self.cursor = boundary;
                }
                None
            }
            KeyCode::Left => {
                self.cursor = self.prev_boundary();
                None
            }
            KeyCode::Right => {
                if self.cursor < self.value.len() {
                    self.cursor = self
                        .value
                        .char_indices()
                        .map(|(i, _)| i)
                        .find(|&i| i > self.cursor)
                        .unwrap_or(self.value.len());
                }
                None
            }
            KeyCode::Char(c) => {
                self.value.insert(self.cursor, c);
                self.cursor = self
                    .value
                    .char_indices()
                    .map(|(i, _)| i)
                    .find(|&i| i > self.cursor)
                    .unwrap_or(self.value.len());
                self.error = None;
                None
            }
            _ => None,
        }
    }

    /// The char boundary before the caret (0 at the start).
    fn prev_boundary(&self) -> usize {
        self.value
            .char_indices()
            .map(|(i, _)| i)
            .take_while(|&i| i < self.cursor)
            .last()
            .unwrap_or(0)
    }

    /// Paint the field into `area`: prompt, the value with its reversed-cell
    /// caret, then the initial-value hint / validation error.
    pub(crate) fn draw(&self, area: Rect, f: &mut ratatui::Frame) {
        let caret_style = tstyle::plain().add_modifier(ratatui::style::Modifier::REVERSED);
        let mut spans = vec![Span::styled(
            format!("{MARGIN}? {} ", self.prompt),
            tstyle::plain(),
        )];
        let chars: Vec<(usize, char)> = self.value.char_indices().collect();
        let mut rendered_caret = false;
        for &(i, c) in chars.iter() {
            if i == self.cursor {
                spans.push(Span::styled(c.to_string(), caret_style));
                rendered_caret = true;
            } else {
                spans.push(Span::styled(c.to_string(), tstyle::plain()));
            }
        }
        if !rendered_caret {
            spans.push(Span::styled(" ", caret_style));
        }
        let mut lines = vec![Line::from(spans)];
        if let Some(initial) = &self.initial {
            lines.push(Line::from(Span::styled(
                format!("{MARGIN}  current: {initial}"),
                tstyle::muted(),
            )));
        }
        if let Some(error) = &self.error {
            lines.push(Line::from(Span::styled(
                format!("{MARGIN}  {ICON_ALERT} {error}"),
                tstyle::failure(),
            )));
        }
        f.render_widget(Paragraph::new(lines), area);
    }
}

#[cfg(test)]
mod tests;
