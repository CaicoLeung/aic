//! The CLI-agent backend sub-flow of the setup wizard: the preset picker
//! and its rows. The verify probe itself lives in [`super::verify`].

use super::verify::{pause_done, smoke_check, step_verify_cli};
use super::*;
use crate::llm::cli_agent::{PRESETS, cli_preset};

/// Configure the CLI-agent backend: pick a preset (claude/codex/pi/opencode).
/// Custom commands are intentionally not offered — every streaming backend
/// needs a dedicated decoder for its stdout envelope (claude `stream-json`,
/// pi `--mode json`, opencode `--format json`), and a free-form command has
/// none, so it would silently run in plain-text mode with no reasoning feed
/// and no clean answer extraction. The other Backend's fields are kept
/// dormant by `finalize`, so configuring a CLI here does not wipe an
/// API-provider config. Returns `true` only on Ctrl-C (cancel setup).
pub(super) fn run_cli_flow(draft: &mut Draft) -> Result<bool> {
    loop {
        show_screen("CLI agent")?;
        let rows = cli_menu_rows(draft.active_cli_command().is_some());
        let labels: Vec<String> = rows.iter().map(CliRow::label).collect();
        match opt_nav("CLI agent backend", &labels, 0)? {
            OptNav::Back => return Ok(false),
            OptNav::Cancel => return Ok(true),
            OptNav::Value(i) => match rows.get(i).copied() {
                Some(CliRow::Preset(name)) => {
                    let spec = cli_preset(name).unwrap();
                    eprintln!("\n{}", smoke_check(&spec.command));
                    draft.cli = CliConfig {
                        command: Some(spec.command),
                        args: Some(spec.args),
                        timeout_secs: Some(spec.timeout_secs),
                        encoding: Some(spec.encoding),
                    };
                    pause_done()?;
                    return Ok(false);
                }
                Some(CliRow::Verify) => {
                    if step_verify_cli(draft)? == Nav::Cancel {
                        return Ok(true);
                    }
                    continue;
                }
                Some(CliRow::Done) => return Ok(false),
                // `opt_nav` only ever returns an index inside `labels`, so an
                // out-of-range row cannot occur in practice; re-show the menu
                // rather than panic if it ever does.
                None => continue,
            },
        }
    }
}

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
    /// Probe install + auth now — only present when a command is set.
    Verify,
    /// Return to the main menu.
    Done,
}

impl CliRow {
    /// The text shown for this row in the [`opt_nav`] menu.
    fn label(&self) -> String {
        match self {
            CliRow::Preset(name) => format!("{ICON_SELECT} {name}"),
            CliRow::Verify => format!("{ICON_VERIFY} Verify (probe the CLI now)"),
            CliRow::Done => format!("{ICON_DONE} Done — back to main menu"),
        }
    }
}

/// Build the CLI-agent menu rows in presentation order. `command_set` gates the
/// Verify row, which only makes sense once a CLI is configured. A label row is
/// deliberately absent — every row is an actionable [`CliRow`].
pub(super) fn cli_menu_rows(command_set: bool) -> Vec<CliRow> {
    let mut rows: Vec<CliRow> = PRESETS.iter().copied().map(CliRow::Preset).collect();
    if command_set {
        rows.push(CliRow::Verify);
    }
    rows.push(CliRow::Done);
    rows
}
