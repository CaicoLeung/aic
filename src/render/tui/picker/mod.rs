//! The shared list picker (ADR 0020): a prompt, a filter line, and the
//! option rows with the `›` menu cursor (ADR 0018 marker vocabulary).
//! Type-to-filter narrows the rows live — the UX inquire's Select kept
//! (ADR 0007) and the one behavior long lists (live-fetched models) cannot
//! lose. Rows carry a caller-supplied tag; Enter reports the chosen row's
//! tag, never its index, so a pick handler dispatches on what the row *is*
//! and cannot drift from the builder's row order. Pure state + two entry
//! points: [`Picker::on_key`] maps a key event to a pick, [`Picker::draw`]
//! paints it into a frame area, so the whole component is hermetically
//! testable with ratatui's `TestBackend`.

use crossterm::event::{KeyCode, KeyEvent};
use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use super::is_cancel;
use crate::render::layout::MARGIN;
use crate::render::tui::style as tstyle;

/// The outcome of one key press on a picker.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Pick<T> {
    /// Enter: the chosen row's tag. The filtered→original index mapping
    /// stays inside the picker.
    Selected(T),
    /// Esc: go back one level.
    Back,
    /// Ctrl-C / closed input: cancel the whole surface.
    Cancel,
}

/// One selectable row: its styled cells for rendering and the plain text
/// the filter matches against.
#[derive(Clone)]
pub(crate) struct PickerItem {
    plain: String,
    line: Line<'static>,
}

impl PickerItem {
    /// Append one styled cell, keeping the rendered line and the filter's
    /// plain text in lockstep — the only sanctioned mutation.
    pub(crate) fn push(&mut self, text: impl Into<String>, style: Style) {
        let text = text.into();
        self.plain.push_str(&text);
        self.line.spans.push(Span::styled(text, style));
    }
}

/// Build one row from styled segments — the picker twin of the ANSI label
/// fns the raw-ANSI surfaces use. `(text, style)` pairs keep call sites
/// building the rendered spans and the plain filter text from the same
/// literals.
pub(crate) fn item<S: Into<String>>(cells: Vec<(S, Style)>) -> PickerItem {
    let mut row = PickerItem {
        plain: String::new(),
        line: Line::default(),
    };
    for (text, style) in cells {
        row.push(text, style);
    }
    row
}

/// A filterable single-choice list over tagged rows: `T` is the action (or
/// value) selecting a row produces.
#[derive(Clone)]
pub(crate) struct Picker<T> {
    prompt: String,
    items: Vec<(PickerItem, T)>,
    filter: String,
    /// Cursor position within the **filtered** view; clamped to its length.
    cursor: usize,
}

impl<T> Picker<T> {
    pub(crate) fn new(prompt: &str, items: Vec<(PickerItem, T)>) -> Self {
        Self {
            prompt: prompt.to_string(),
            items,
            filter: String::new(),
            cursor: 0,
        }
    }

    /// [`Picker::new`] with the cursor preset — restoring a screen that
    /// remembers its selection (the provider list opens on the active
    /// provider).
    pub(crate) fn new_at(prompt: &str, items: Vec<(PickerItem, T)>, cursor: usize) -> Self {
        Self {
            cursor,
            ..Self::new(prompt, items)
        }
    }

    /// Indices of the rows still visible under the active filter, in list
    /// order. Pure.
    fn visible(&self) -> Vec<usize> {
        if self.filter.is_empty() {
            return (0..self.items.len()).collect();
        }
        let needle = self.filter.to_lowercase();
        (0..self.items.len())
            .filter(|&i| self.items[i].0.plain.to_lowercase().contains(&needle))
            .collect()
    }

    /// Paint the picker into `area`: prompt, filter (only while non-empty),
    /// then the visible row window around the cursor. The `›` cursor marks
    /// the selected row; unselected rows get the matching two-space inset.
    /// Rows never wrap — the window scrolls instead, mirroring the frame
    /// accounting rule the raw-ANSI surfaces follow.
    pub(crate) fn draw(&self, area: Rect, f: &mut ratatui::Frame) {
        let visible = self.visible();
        let rows_budget = area.height.saturating_sub(2) as usize; // prompt + filter rows
        let start = self.cursor.saturating_sub(rows_budget.saturating_sub(1));
        let window: Vec<usize> = visible
            .iter()
            .copied()
            .skip(start)
            .take(rows_budget.max(1))
            .collect();

        let mut lines = vec![Line::from(Span::styled(
            format!("{MARGIN}? {}", self.prompt),
            tstyle::plain(),
        ))];
        if !self.filter.is_empty() {
            lines.push(Line::from(Span::styled(
                format!("{MARGIN}  filter: {}", self.filter),
                tstyle::muted(),
            )));
        }
        for (n, &i) in window.iter().enumerate() {
            let marker = if start + n == self.cursor {
                "› "
            } else {
                "  "
            };
            let mut line = Line::from(Span::styled(format!("{MARGIN}{marker}"), tstyle::plain()));
            line.spans
                .extend(self.items[i].0.line.spans.iter().cloned());
            lines.push(line);
        }
        f.render_widget(Paragraph::new(lines), area);
    }
}

impl<T: Clone> Picker<T> {
    /// Map a key press to a [`Pick`]; typing narrows the filter, arrows move
    /// the cursor (clamped, never wrapping — an off-screen wrap loses the
    /// cursor), Enter picks the row under the cursor.
    pub(crate) fn on_key(&mut self, key: KeyEvent) -> Option<Pick<T>> {
        if is_cancel(key) {
            return Some(Pick::Cancel);
        }
        match key.code {
            KeyCode::Up => {
                self.cursor = self.cursor.saturating_sub(1);
                None
            }
            KeyCode::Down => {
                let len = self.visible().len();
                self.cursor = (self.cursor + 1).min(len.saturating_sub(1));
                None
            }
            KeyCode::Enter => {
                let visible = self.visible();
                visible
                    .get(self.cursor)
                    .map(|&i| Pick::Selected(self.items[i].1.clone()))
            }
            KeyCode::Esc => Some(Pick::Back),
            KeyCode::Backspace => {
                self.filter.pop();
                self.cursor = 0;
                None
            }
            KeyCode::Char(c) => {
                self.filter.push(c);
                self.cursor = 0;
                None
            }
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests;
