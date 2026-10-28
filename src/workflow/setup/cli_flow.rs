//! The CLI-agent backend sub-flow of the setup wizard: the preset picker
//! and its rows. The verify probe itself lives in [`super::verify`] and runs
//! automatically when the flow completes (R3).

use super::verify::{Verified, auto_verify, pause_done, smoke_check};
use super::*;
use crate::llm::cli_agent::{PRESETS, cli_preset};
use crate::render::{oneshot, palette};

/// Configure the CLI-agent backend: pick a preset (claude/codex/pi/opencode/
/// omp/gemini/cursor/windsurf/copilot/trae/qwen) and write it to config.
/// Completing the flow — choosing a preset, or `Done` with a command already
/// set — auto-runs the connectivity probe (R3); a failed probe offers
/// Edit (re-enter the picker) / Retry / Save anyway, and the outcome lands
/// on the top menu's status line.
/// Custom commands are intentionally not offered — every streaming backend
/// needs a dedicated decoder for its stdout envelope (claude `stream-json`,
/// pi `--mode json`, opencode `--format json`), and a free-form command has
/// none, so it would silently run in plain-text mode with no reasoning feed
/// and no clean answer extraction. The other Backend's fields are kept
/// dormant by `finalize`, so configuring a CLI here does not wipe an
/// API-provider config. Returns `true` only on Ctrl-C (cancel setup).
pub(super) fn run_cli_flow(deps: &SetupDeps, draft: &mut Draft) -> Result<bool> {
    loop {
        show_screen("CLI agent")?;
        let rows = cli_menu_rows();
        let labels: Vec<String> = rows.iter().map(CliRow::label).collect();
        match opt_nav("CLI agent backend", &labels, 0)? {
            OptNav::Back => return Ok(false),
            OptNav::Cancel => return Ok(true),
            OptNav::Value(i) => match rows.get(i).copied() {
                Some(CliRow::Preset(name)) => {
                    let spec = cli_preset(name).unwrap();
                    eprintln!();
                    oneshot::stderr_line(&smoke_check(&spec.command), palette::muted());
                    draft.cli = CliConfig {
                        command: Some(spec.command),
                        args: Some(spec.args),
                        timeout_secs: Some(spec.timeout_secs),
                        encoding: Some(spec.encoding),
                    };
                    pause_done()?;
                    return auto_verify_cli(deps, draft);
                }
                // Done with a configured CLI re-probes it (R3); with nothing
                // configured there is nothing to probe — straight to the menu.
                Some(CliRow::Done) => {
                    if draft.active_cli_command().is_some() {
                        return auto_verify_cli(deps, draft);
                    }
                    return Ok(false);
                }
                // `opt_nav` only ever returns an index inside `labels`, so an
                // out-of-range row cannot occur in practice; re-show the menu
                // rather than panic if it ever does.
                None => continue,
            },
        }
    }
}

/// One auto-verify pass for the CLI backend (R3). `Edit` re-enters the
/// preset picker; any settled outcome returns to the top menu with the
/// status recorded. Returns `true` only on Ctrl-C.
fn auto_verify_cli(deps: &SetupDeps, draft: &mut Draft) -> Result<bool> {
    let target = draft.active_cli_command().unwrap_or("cli").to_string();
    match auto_verify(
        draft,
        &target,
        &deps.verify_cli,
        &super::verify::show_cli_success,
        "Verify — CLI agent",
        &[
            "Common causes: the CLI is not installed, not",
            "authenticated, or timed out. Run the CLI once to log in.",
        ],
    )? {
        None => Ok(true),
        Some(Verified::Edit) => run_cli_flow(deps, draft),
        Some(_) => Ok(false),
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
    /// Return to the main menu.
    Done,
}

impl CliRow {
    /// The text shown for this row in the [`opt_nav`] menu. Preset rows
    /// lead with the R4 badge: the preset name's first letter, colored by a
    /// stable hash of the name (ADR 0018 amendment); inline ANSI is safe,
    /// inquire's frame renderer is ANSI-aware.
    pub(super) fn label(&self) -> String {
        match self {
            CliRow::Preset(name) => format!("{} {name}", cli_badge(name)),
            CliRow::Done => format!("{DOT} Done — back to main menu"),
        }
    }
}

/// The R4 badge cell for one CLI-agent row: the preset name's first letter,
/// uppercased, bolded, and colored via [`palette::badge`] hashed on the
/// name. Pure — returns the styled string.
pub(super) fn cli_badge(name: &str) -> String {
    let letter = name
        .chars()
        .next()
        .unwrap_or('·')
        .to_uppercase()
        .to_string();
    palette::badge(name).bold().apply_to(letter).to_string()
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
