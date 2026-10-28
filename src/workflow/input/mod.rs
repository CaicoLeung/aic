//! Generic interactive-input primitives shared across the workflow UIs: the
//! setup wizard ([`crate::workflow::setup`]), the commit-confirm menu
//! ([`crate::workflow::confirm`]), and the Batch Review menu
//! ([`crate::workflow::review`]) — a normalized single-choice menu
//! ([`opt_nav`]), a zero-residue Select row ([`select_row`]), a raw-mode text
//! prompt ([`prompt_text`]), the inquire cancel classifier and its Option
//! mapping ([`is_io_cancel`] / [`is_graceful_cancel`] / [`inquire_opt`]),
//! the one-pass [`truncate`], and the shared non-TTY-stdin guard
//! ([`ensure_interactive_terminal`]).
//!
//! These are deliberately provider- and config-agnostic — they know nothing
//! about the fields being edited, only how to drive `inquire` and normalize
//! its Esc / Ctrl-C / closed-stdin outcomes. ADR 0007 names this module as
//! the home for generic interactive-input primitives: the wizard helpers
//! used to live at the bottom of `config.rs` (wrong locality, AIC-17), and
//! the menu-row helpers were lifted here once the second production menu
//! made the duplication visible.

use crate::render::layout::MARGIN;
use anyhow::{Context, Result};
use inquire::{InquireError, Select};
use std::io;

/// Whether an inquire error is a hard cancel — Ctrl-C
/// ([`InquireError::OperationInterrupted`]) or a closed/EOF stdin (surfaced as
/// an [`InquireError::IO`] error of kind `Interrupted`/`UnexpectedEof`). Esc
/// ([`InquireError::OperationCanceled`]) is deliberately excluded: the wizard
/// treats Esc as *back* while the production menus treat every cancel alike,
/// so each caller decides where Esc falls. Shared with `is_graceful_cancel`
/// in `confirm.rs` so the IO-kind sub-clause isn't duplicated.
pub(crate) fn is_io_cancel(e: &InquireError) -> bool {
    matches!(e, InquireError::OperationInterrupted)
        || matches!(
            e,
            InquireError::IO(err) if matches!(
                err.kind(),
                io::ErrorKind::Interrupted | io::ErrorKind::UnexpectedEof
            )
        )
}

/// Whether an inquire error is a graceful user-initiated cancel (Esc,
/// Ctrl-C, or a closed/EOF stdin) rather than an unexpected I/O failure.
/// Esc/Ctrl-C are inquire's own cancel variants; a dropped stdin surfaces as
/// an `IO` error with an `Interrupted`/`UnexpectedEof` kind, so detect those
/// too and treat them as cancels (matching the setup wizard's handling).
pub(crate) fn is_graceful_cancel(e: &InquireError) -> bool {
    // Esc (OperationCanceled) plus the hard cancels shared with the wizard's
    // `opt_nav` (Ctrl-C / closed stdin) — see [`is_io_cancel`].
    matches!(e, InquireError::OperationCanceled) || is_io_cancel(e)
}

/// Map an inquire prompt result to an `Option` (`None` = the user cancelled),
/// turning any graceful cancel into `None` and every other error into a
/// "could not read terminal input" error. The single source of the cancel
/// mapping for callers that collapse Esc / Ctrl-C / closed-stdin into one
/// "no answer" outcome (both production menus and the completion-shell
/// prompt).
///
/// The setup wizard's `opt_nav` does **not** use this: it keeps Esc (Back)
/// distinct from Ctrl-C (Cancel), so it stays a three-way match of its own.
pub(crate) fn inquire_opt<T>(result: Result<T, InquireError>) -> anyhow::Result<Option<T>> {
    match result {
        Ok(value) => Ok(Some(value)),
        Err(e) if is_graceful_cancel(&e) => Ok(None),
        Err(e) => Err(e).context("could not read terminal input"),
    }
}

/// One zero-residue `inquire::Select` row: save the cursor, prompt, restore,
/// and clear everything the prompt drew, mapping Esc / Ctrl-C / closed stdin
/// to `None`. inquire's final frame redraws the menu's full footprint, which
/// would break the caller's exact `clear_last(rows)` erase accounting, so
/// the DECSC/DECRC save/restore contract erases it here — height-independent,
/// so it stays correct no matter how many rows the menu spanned. Shared by
/// the confirm menu and the Batch Review menu (ADR 0007).
pub(crate) fn select_row(
    prompt: &str,
    labels: &[&str],
    help: &str,
) -> anyhow::Result<Option<usize>> {
    let term = console::Term::stderr();
    let _ = term.write_str("\x1b7"); // DECSC: save cursor at the prompt's start
    let choice = Select::new(prompt, labels.to_vec())
        .with_starting_cursor(0)
        .with_help_message(help)
        .raw_prompt();
    let _ = term.write_str("\x1b8"); // DECRC: back to the prompt's start
    let _ = term.clear_to_end_of_screen(); // erase the prompt's footprint

    Ok(inquire_opt(choice)?.map(|option| option.index))
}

/// One interactive menu rendered as timeline frame rows (ADR 0018): the
/// options with the `›` cursor paint as live rows under the active step —
/// below whatever preview or listing already sits there — and arrow keys /
/// Enter pick. Zero-residue by construction: closing the menu is just a
/// shorter frame. `ask_row` also renders the prompt as a `?` row above the
/// options (the merge-pick sub-dialogs); the self-describing menus (confirm
/// four, review actions) pass `false`. Falls back to the inquire
/// [`select_row`] when no frame is painting (no active Run, non-TTY stderr),
/// preserving the pre-timeline behavior for those callers.
pub(crate) fn select_in_frame(
    prompt: &str,
    options: &[&str],
    selected: usize,
    help: &str,
    ask_row: bool,
) -> Result<Option<usize>> {
    let ask = if ask_row { Some(prompt) } else { None };
    match crate::render::timeline::driver::frame_menu(ask, options, selected, help) {
        Some(outcome) => Ok(outcome),
        None => select_row(prompt, options, help),
    }
}

/// Truncate to `max` chars in one pass, appending `…` when a `max+1`-th
/// char existed (avoids walking the string twice). Shared by the confirm
/// menu's subject and the Batch Review listing/labels so every truncated row
/// truncates the same way.
pub(crate) fn truncate(s: &str, max: usize) -> String {
    let mut chars = s.chars();
    let mut out: String = chars.by_ref().take(max).collect();
    if chars.next().is_some() {
        out.push('…');
    }
    out
}

/// Leading glyph for y/n prompts — the shared `?` ask marker (design spec
/// §1.1), in `caution` like every interactive ask row.
fn ask_marker() -> String {
    crate::render::palette::styled_stderr("?", crate::render::palette::caution())
}

/// Read a y/n answer from raw stdin (no inquire — this must work when stdin is
/// a pipe in tests, and the label is written to stderr because `Display` is
/// stderr-only so piped stdout stays clean). An empty answer (just Enter)
/// defaults to `true` (yes); `n` / `no` / any other input returns `false`.
/// The hint renders the default in caps (`[Y/n]`) so Enter's effect is
/// visible, not memorized.
pub(crate) fn prompt_yes_no(label: &str) -> Result<bool> {
    use std::io::Write as _;
    eprint!("{MARGIN}{} {label} [Y/n] ", ask_marker());
    io::stderr().flush()?;
    let mut input = String::new();
    io::stdin().read_line(&mut input)?;
    let trimmed = input.trim().to_lowercase();
    Ok(matches!(trimmed.as_str(), "" | "y" | "yes"))
}

/// The interactive menus — Commit confirmation (issue #78) and Batch Review
/// (issue #157) — require an interactive stdin: they render on stderr but
/// read keys from stdin, so a non-TTY stdin leaves them unanswerable.
/// Returns an error naming the fix when any enabled menu knob meets a
/// non-terminal stdin — the guard runs before any planning or staging, so
/// the run fails cleanly instead of aborting after the first batch is
/// already staged.
///
/// Deliberately unconditional once a knob is on: the guard does not peek at
/// the repo to predict whether a menu would actually appear (Batch Review
/// only shows on the unstaged plan path, so a staged-only or clean repo
/// would never reach it). A state-dependent guard would make the failure
/// flaky for scripted callers — fine while only staged files exist, fatal
/// the moment one goes unstaged — and anyone enabling an interactive knob
/// in a non-TTY context has a config bug worth surfacing every run.
pub(crate) fn ensure_interactive_terminal(
    confirm_enabled: bool,
    review_enabled: bool,
    stdin_tty: bool,
) -> anyhow::Result<()> {
    let knobs = [
        confirm_enabled.then_some("confirm_before_commit"),
        review_enabled.then_some("review_batch_plan"),
    ]
    .into_iter()
    .flatten()
    .collect::<Vec<_>>()
    .join(" and ");
    if !knobs.is_empty() && !stdin_tty {
        anyhow::bail!(
            "{knobs} enabled but stdin is not a terminal — \
             run `aic` from a terminal, or turn the option off"
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every enabled-menu combination passes on an interactive stdin; any
    /// enabled knob with a non-TTY stdin fails fast with a message naming
    /// the knob and the fix — before any planning or staging happens.
    #[test]
    fn ensure_interactive_terminal_guards_non_tty_stdin() {
        assert!(ensure_interactive_terminal(false, false, false).is_ok());
        assert!(ensure_interactive_terminal(true, false, true).is_ok());
        assert!(ensure_interactive_terminal(true, true, true).is_ok());

        let err =
            ensure_interactive_terminal(true, false, false).expect_err("must refuse non-TTY stdin");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("confirm_before_commit"),
            "expected the knob to be named, got: {msg}"
        );
        assert!(
            msg.contains("stdin is not a terminal"),
            "expected a clear non-TTY error, got: {msg}"
        );
        assert!(
            msg.contains("run `aic` from a terminal"),
            "expected the fix to be named, got: {msg}"
        );

        let err =
            ensure_interactive_terminal(false, true, false).expect_err("must refuse non-TTY stdin");
        assert!(
            format!("{err:#}").contains("review_batch_plan"),
            "expected the review knob to be named, got: {err:#}"
        );

        let err =
            ensure_interactive_terminal(true, true, false).expect_err("must refuse non-TTY stdin");
        assert!(
            format!("{err:#}").contains("confirm_before_commit and review_batch_plan"),
            "expected both knobs to be named, got: {err:#}"
        );
    }

    /// Truncation keeps short strings verbatim, cuts at exactly `max` chars,
    /// and marks the cut with `…` only when a `max+1`-th char existed.
    #[test]
    fn truncate_marks_only_actual_cuts() {
        assert_eq!(truncate("short", 10), "short");
        assert_eq!(truncate("1234567890", 10), "1234567890");
        assert_eq!(truncate("1234567890X", 10), "1234567890…");
        // Chars, not bytes: a multi-byte char counts once.
        assert_eq!(truncate("ααααα", 4), "αααα…");
        assert_eq!(truncate("", 4), "");
    }
}
