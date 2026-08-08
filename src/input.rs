//! Generic interactive-input primitives shared by the setup wizard
//! ([`crate::setup`]) and the commit-confirm menu ([`crate::confirm`]):
//! a normalized single-choice menu ([`opt_nav`]) and a raw-mode text prompt
//! ([`prompt_text`]), plus the inquire cancel classifier ([`is_io_cancel`])
//! they share with `confirm.rs`'s [`crate::confirm::is_graceful_cancel`].
//!
//! These are deliberately provider- and config-agnostic — they know nothing
//! about the fields being edited, only how to drive `inquire` and normalize
//! its Esc / Ctrl-C / closed-stdin outcomes. They used to live at the bottom
//! of `config.rs`; the locality was wrong (`confirm.rs` reached into
//! `config::is_io_cancel` for a predicate that is not about config at all),
//! so they got their own module (AIC-17).

use anyhow::{Context, Result};
use inquire::list_option::ListOption;
use inquire::validator::Validation;
use inquire::{InquireError, Password, Select, Text};
use std::io;

/// Outcome of a single-choice menu ([`opt_nav`]): the chosen row index, or
/// Esc (back) / Ctrl-C (cancel). inquire models Esc and Ctrl-C as error
/// variants ([`InquireError::OperationCanceled`] /
/// [`InquireError::OperationInterrupted`]); this normalizes them into the
/// wizard's nav vocabulary so every menu dispatches identically.
pub(crate) enum OptNav {
    Value(usize),
    Back,
    Cancel,
}

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

/// Render an `inquire::Select` menu over `options` and map its three outcomes
/// — choose (index), Esc (back), Ctrl-C/closed-stdin (cancel) — onto
/// [`OptNav`]. `default` is the row highlighted when the menu opens (inquire's
/// `starting_cursor`). Uses `raw_prompt` so the chosen **index** is recoverable
/// (inquire's plain `prompt` returns only the value), which keeps the wizard's
/// index-based dispatch untouched.
///
/// `options` is borrowed (`&[String]`) and cloned once here
/// (`options.to_vec()`) because `inquire::Select` owns its list — borrowing
/// lets call sites keep one reusable `Vec` across loop iterations without a
/// per-iteration clone. Filtering stays ON (inquire's default): in 0.7.5 the
/// no-filter path (`render_select_prompt` -> `print_prompt`) omits the newline
/// after the prompt and glues the first option to it (`? prompt  opt0`),
/// whereas the filter path ends the prompt line with a newline. So
/// `.without_filtering()` is deliberately NOT used — it regresses the layout.
/// The cost is a filter input line + type-to-filter narrowing; see ADR-0007.
pub(crate) fn opt_nav(prompt: &str, options: &[String], default: usize) -> Result<OptNav> {
    match Select::new(prompt, options.to_vec())
        .with_starting_cursor(default)
        .raw_prompt()
    {
        Ok(ListOption { index, .. }) => Ok(OptNav::Value(index)),
        Err(InquireError::OperationCanceled) => Ok(OptNav::Back),
        // Ctrl-C or a closed/EOF stdin — both cancel the wizard. Esc is
        // handled above as Back.
        Err(e) if is_io_cancel(&e) => Ok(OptNav::Cancel),
        Err(e) => Err(e).context("could not read terminal input"),
    }
}

/// Outcome of a raw-mode text prompt.
pub(crate) enum TextAct {
    Value(String),
    Back,
    Cancel,
}

/// Read a line of text via the `inquire` crate, which intercepts Esc (back)
/// and Ctrl-C (cancel) natively as error variants. `masked` hides each typed
/// char (for secrets). `initial` is offered as the kept value when the user
/// submits an empty line. `allow_empty` admits an empty submit; otherwise
/// `empty_hint` is shown (via inquire's validator) and the prompt retries until
/// non-empty.
pub(crate) fn prompt_text(
    prompt: &str,
    masked: bool,
    initial: Option<&str>,
    allow_empty: bool,
    empty_hint: &str,
) -> Result<TextAct> {
    // Show the current value as a help line; an empty submit keeps it.
    let help = match (initial, masked) {
        (Some(_), true) => Some("current: •••• (leave blank to keep)".to_string()),
        (Some(d), false) => Some(format!("current: {d} (leave blank to keep)")),
        _ => None,
    };

    let prompt_result = if masked {
        let mut p = Password::new(prompt);
        if let Some(h) = help.as_deref() {
            p = p.with_help_message(h);
        }
        if !allow_empty {
            let hint = empty_hint.to_string();
            p = p.with_validator(move |v: &str| {
                if v.trim().is_empty() {
                    Ok(Validation::Invalid(hint.clone().into()))
                } else {
                    Ok(Validation::Valid)
                }
            });
        }
        p.prompt()
    } else {
        let mut t = Text::new(prompt);
        if let Some(h) = help.as_deref() {
            t = t.with_help_message(h);
        }
        if !allow_empty {
            let hint = empty_hint.to_string();
            t = t.with_validator(move |v: &str| {
                if v.trim().is_empty() {
                    Ok(Validation::Invalid(hint.clone().into()))
                } else {
                    Ok(Validation::Valid)
                }
            });
        }
        t.prompt()
    };

    match prompt_result {
        Ok(v) => {
            let trimmed = v.trim().to_string();
            if trimmed.is_empty() {
                // Empty submit: keep the initial when present, else honor
                // allow_empty (the validator already enforced `required`).
                if let Some(d) = initial {
                    return Ok(TextAct::Value(d.to_string()));
                }
                if allow_empty {
                    return Ok(TextAct::Value(String::new()));
                }
            }
            Ok(TextAct::Value(trimmed))
        }
        // Esc — back out of this step.
        Err(InquireError::OperationCanceled) => Ok(TextAct::Back),
        // Ctrl-C — cancel the whole setup, same as everywhere in the wizard.
        Err(InquireError::OperationInterrupted) => Ok(TextAct::Cancel),
        Err(e) => Err(e).context("could not read terminal input"),
    }
}
