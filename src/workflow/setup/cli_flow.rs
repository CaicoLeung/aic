//! The CLI-agent backend sub-flow's vocabulary: the preset rows the picker
//! shows. The verify probe itself lives in [`super::verify`] and the screen
//! state machine in [`super::app`].

use crate::llm::cli_agent::PRESETS;

/// One selectable row of the CLI-agent menu: the label shown to the user and
/// the action selecting it takes, paired. Building the list once means every
/// selectable row has an action **by construction** — there is no separate
/// index arithmetic to drift out of sync, so no row can ever be "unmapped".
/// (An earlier version put a "Choose a preset:" *label* in as row 0 with no
/// matching action; selecting it panicked the TUI at the `unreachable!`.)
#[derive(Clone, Copy, PartialEq, Debug)]
pub(super) enum CliRow {
    /// A named preset (an entry of [`PRESETS`]).
    Preset(&'static str),
    /// Return to the main menu.
    Done,
}

/// Build the CLI-agent menu rows in presentation order: the presets, then
/// `Done`. The manual Verify row is gone (R3) — the probe runs automatically
/// when the flow completes. A label row is deliberately absent — every row
/// is an actionable [`CliRow`].
pub(super) fn cli_menu_rows() -> Vec<CliRow> {
    let mut rows: Vec<CliRow> = PRESETS.iter().copied().map(CliRow::Preset).collect();
    rows.push(CliRow::Done);
    rows
}
