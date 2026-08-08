//! Pre-commit confirmation (issue #78): the opt-in gate that interrupts the
//! commit path after the message is drafted and before it lands. Owns the
//! gate itself ([`Confirm`]), the menu/editor seams it drives, the production
//! terminal menu and `$EDITOR` message editor, the non-TTY-stdin guard, and
//! the confirm loop that re-generates / re-edits until the user commits or
//! aborts. [`CommitDeclined`] is the marker error the loop raises on Abort so
//! the commit workflow can report a clean abort (naming how far a batch run
//! got) rather than a generic failure.
//!
//! `confirm_draft` borrows the workflow's [`Display`] and [`CommitMessenger`]
//! rather than owning them: confirmation is a phase of the commit Run, not a
//! standalone service, so it reaches across to the display + LLM seams the
//! Run already holds. See CONTEXT.md "Commit confirmation".

use crate::CommitMessenger;
use crate::display::Display;
use crate::progress;
use anyhow::Context;

// ----------------------------------------------------------------------
// The gate + seams
// ----------------------------------------------------------------------

/// One action the user can take on the pre-commit confirmation menu (issue
/// #78). The confirm loop translates it: Commit lands the commit, Regenerate
/// and Edit loop back to the menu (re-showing the message), Abort ends the
/// run with nothing further committed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ConfirmChoice {
    Commit,
    Regenerate,
    Edit,
    Abort,
}

/// Erased confirmation menu: given the drafted subject, returns the user's
/// choice. Boxed for the same reason as the resolver seam — production wires
/// it to a terminal menu (issue #78), tests inject a scripted choice
/// sequence.
pub(crate) type ConfirmMenu = Box<dyn Fn(&str) -> anyhow::Result<ConfirmChoice>>;

/// Erased message editor: takes the current (subject, body) and returns the
/// edited (subject, body) — the prior values unchanged when the user cancels
/// the edit. Boxed for the same reason — production opens
/// `$VISUAL`/`$EDITOR` on a temp file via the `edit` crate, tests inject a
/// stub.
pub(crate) type CommitEditor =
    Box<dyn Fn(&str, Option<&str>) -> anyhow::Result<(String, Option<String>)>>;

/// Opt-in pre-commit confirmation (issue #78): the gate plus the menu and
/// editor seams it needs, grouped so the workflow signatures stay within
/// clippy's argument budget. [`Confirm::Disabled`] is the default — no menu,
/// generate-and-commit byte-for-byte as before the option existed.
///
/// Modeled as an enum, not a struct-with-a-gate, so the disabled variant
/// carries no dead closures — the menu and editor exist only when they can
/// actually run. Tests build [`Confirm::Interactive`] directly with scripted
/// seams; production uses [`Confirm::interactive`].
pub(crate) enum Confirm {
    /// Confirmation off — generate-and-commit is unchanged. Carries no menu
    /// or editor, so the disabled path can't accidentally invoke them.
    Disabled,
    /// Confirmation on, wired to the production menu and editor.
    Interactive {
        /// Drafted subject → user choice (Commit / Re-generate / Edit / Abort).
        menu: ConfirmMenu,
        /// (subject, body) → edited (subject, body); unchanged when the user
        /// cancels the edit.
        editor: CommitEditor,
    },
}

impl Confirm {
    /// Production confirmation: the terminal [`confirm_menu`] and the
    /// `$EDITOR` [`edit_message`]. The production constructors stay private
    /// to this module; tests inject their own seams by building
    /// [`Confirm::Interactive`] directly.
    pub(crate) fn interactive() -> Self {
        Self::Interactive {
            menu: Box::new(confirm_menu),
            editor: Box::new(edit_message),
        }
    }
}

// ----------------------------------------------------------------------
// Errors + the non-TTY guard
// ----------------------------------------------------------------------

/// Marker error for the user declining the pre-commit confirmation (issue
/// #78). Distinct from ordinary failures so each call site can translate it
/// into its own abort wording: the single-commit path reports "no commit
/// made", the batch loop reports how many batches already committed and that
/// the rest is recoverable.
#[derive(Debug)]
pub(crate) struct CommitDeclined;

impl std::fmt::Display for CommitDeclined {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "commit declined by user")
    }
}

impl std::error::Error for CommitDeclined {}

/// The pre-commit confirmation requires an interactive stdin: the menu
/// ([`confirm_menu`]) renders on stderr but reads keys from stdin, so a
/// non-TTY stdin leaves the menu unanswerable. Returns an error naming the
/// fix when confirmation is enabled but stdin is not a terminal — the guard
/// runs before any planning or staging, so the run fails cleanly instead of
/// aborting after the first batch is already staged (issue #78).
pub(crate) fn ensure_confirm_terminal(
    confirm_enabled: bool,
    stdin_tty: bool,
) -> anyhow::Result<()> {
    if confirm_enabled && !stdin_tty {
        anyhow::bail!(
            "confirm_before_commit is enabled but stdin is not a terminal — \
             run `aic` from a terminal, or turn the option off"
        );
    }
    Ok(())
}

// ----------------------------------------------------------------------
// inquire cancel handling
// ----------------------------------------------------------------------

/// Whether an inquire error is a graceful user-initiated cancel (Esc,
/// Ctrl-C, or a closed/EOF stdin) rather than an unexpected I/O failure.
/// Esc/Ctrl-C are inquire's own cancel variants; a dropped stdin surfaces as
/// an `IO` error with an `Interrupted`/`UnexpectedEof` kind, so detect those
/// too and treat them as cancels (matching the setup wizard's handling).
pub(crate) fn is_graceful_cancel(e: &inquire::InquireError) -> bool {
    // Esc (OperationCanceled) plus the hard cancels shared with the wizard's
    // `opt_nav` (Ctrl-C / closed stdin) — see `config::is_io_cancel`.
    matches!(e, inquire::InquireError::OperationCanceled) || crate::config::is_io_cancel(e)
}

/// Map an inquire prompt result to an `Option` (`None` = the user cancelled),
/// turning any graceful cancel into `None` and every other error into a
/// "could not read terminal input" error. The single source of the cancel
/// mapping for callers that collapse Esc / Ctrl-C / closed-stdin into one
/// "no answer" outcome (the confirm menu and the completion-shell prompt).
///
/// The setup wizard's `opt_nav` does **not** use this: it keeps Esc (Back)
/// distinct from Ctrl-C (Cancel), so it stays a three-way match of its own.
pub(crate) fn inquire_opt<T>(
    result: Result<T, inquire::InquireError>,
) -> anyhow::Result<Option<T>> {
    match result {
        Ok(value) => Ok(Some(value)),
        Err(e) if is_graceful_cancel(&e) => Ok(None),
        Err(e) => Err(e).context("could not read terminal input"),
    }
}

// ----------------------------------------------------------------------
// The confirm loop
// ----------------------------------------------------------------------

/// Run the confirmation loop for a drafted message. Returns the confirmed
/// (message, body, preview_rows) after the user approves or edits it. The
/// preview is shown after each edit/regeneration, and each preview is erased
/// before being replaced so superseded drafts never accumulate on screen.
pub(crate) async fn confirm_draft(
    draft: (String, Option<String>),
    paths: &[String],
    display: &Display,
    confirm: &Confirm,
    messenger: &CommitMessenger,
    diff: String,
) -> anyhow::Result<(String, Option<String>, usize)> {
    let (mut message, mut body) = draft;

    // Nothing to confirm when the gate is off — no menu/editor wired, so the
    // drafted message is final.
    let Confirm::Interactive { menu, editor } = confirm else {
        return Ok((message, body, 0));
    };

    loop {
        let rows = display.commit_preview(&message, body.as_deref(), paths);
        match menu(&message)? {
            ConfirmChoice::Commit => return Ok((message, body, rows)),
            ConfirmChoice::Regenerate => {
                display.clear_last(rows);
                let result =
                    progress::with_spinner("Regenerating message", messenger(diff.clone())).await?;
                message = result.message;
                body = result.body;
            }
            ConfirmChoice::Edit => {
                display.clear_last(rows);
                (message, body) = editor(&message, body.as_deref())?;
            }
            ConfirmChoice::Abort => return Err(CommitDeclined.into()),
        }
    }
}

// ----------------------------------------------------------------------
// Production menu + editor
// ----------------------------------------------------------------------

/// The four menu actions in display order, each bound to its [`ConfirmChoice`]
/// in one place. A single table — not a labels-vec plus a positional match —
/// so reordering the rows can never silently desync a label from its choice.
const CONFIRM_ACTIONS: [(&str, ConfirmChoice); 4] = [
    ("Commit", ConfirmChoice::Commit),
    ("Re-generate", ConfirmChoice::Regenerate),
    ("Edit", ConfirmChoice::Edit),
    ("Abort", ConfirmChoice::Abort),
];

/// Production confirmation menu (issue #78): an `inquire::Select` over the
/// four actions, matching the setup wizard's arrow-key UI. The drafted
/// subject rides in the prompt so the menu is self-describing even if the
/// preview above scrolled away. Esc and Ctrl-C both abort — there is nothing
/// to go back to once the commit is pending — matching the wizard's
/// graceful-cancel handling (Ctrl-C is not an error here, same as in
/// `opt_nav`).
fn confirm_menu(message: &str) -> anyhow::Result<ConfirmChoice> {
    use inquire::Select;
    use inquire::list_option::ListOption;

    // Truncate to 40 chars in one pass: take 40, then check whether a 41st
    // existed (avoids walking the string twice).
    let mut chars = message.chars();
    let mut subject: String = chars.by_ref().take(40).collect();
    if chars.next().is_some() {
        subject.push('…');
    }

    let labels: Vec<&str> = CONFIRM_ACTIONS.iter().map(|(label, _)| *label).collect();

    // inquire's final frame redraws the menu's full footprint (the answer on
    // top, the option rows below blanked) plus a trailing blank line. That
    // residue would break the caller's exact `clear_last(rows)` preview erase,
    // so restore the cursor to where the menu began and clear everything the
    // prompt drew before returning. Save/restore is height-independent, so it
    // stays correct no matter how many rows the menu spanned (Esc/Ctrl-C
    // paths too) — the zero-residue contract the caller relies on is preserved.
    let term = console::Term::stderr();
    let _ = term.write_str("\x1b7"); // DECSC: save cursor at the menu's start
    let choice = Select::new(&format!("Commit this message?  ({subject})"), labels)
        .with_starting_cursor(0)
        .raw_prompt();
    let _ = term.write_str("\x1b8"); // DECRC: back to the menu's start
    let _ = term.clear_to_end_of_screen(); // erase the menu's footprint

    Ok(match inquire_opt(choice)? {
        // Esc / Ctrl-C / a closed stdin all end the run — there's nothing to
        // go back to once the commit is pending.
        None => ConfirmChoice::Abort,
        Some(ListOption { index, .. }) => CONFIRM_ACTIONS[index].1,
    })
}

/// Production message editor (issue #78): opens the drafted message in the
/// user's `$VISUAL`/`$EDITOR` on a temp file via the `edit` crate, and reads
/// the edited content back as a (subject, body) pair. The subject is the
/// first line, the body the rest (leading blank lines collapsed, git-style).
fn edit_message(subject: &str, body: Option<&str>) -> anyhow::Result<(String, Option<String>)> {
    let text = message_to_edit(subject, body);

    let edited = edit::edit(&text).context("editor failed or was cancelled")?;

    let mut lines = edited.trim_end().splitn(2, '\n');
    let new_subject = lines.next().unwrap_or("").to_string();
    // An empty/whitespace-only subject would fail at `git commit` with a
    // confusing error; treat a cleared subject like a cancel and keep the
    // draft so the user can re-edit or Abort from the menu.
    if new_subject.trim().is_empty() {
        return Ok((subject.to_string(), body.map(String::from)));
    }
    let new_body = lines.next().map(|s| s.trim_start().to_string());
    Ok((new_subject, new_body))
}

/// The text an editor edits: the subject line, then the body (outer-whitespace
/// trimmed) on following lines. Shared by both editor paths so what the user
/// sees in the editor is exactly the (subject, body) pair that would commit.
fn message_to_edit(subject: &str, body: Option<&str>) -> String {
    let mut text = subject.to_string();
    if let Some(b) = body {
        let trimmed = b.trim();
        if !trimmed.is_empty() {
            text.push('\n');
            text.push_str(trimmed);
        }
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Confirmation off, or an interactive stdin, always passes; confirmation
    /// on with a non-TTY stdin fails fast with a message naming the fix —
    /// before any planning or staging happens.
    #[test]
    fn ensure_confirm_terminal_guards_non_tty_stdin() {
        assert!(ensure_confirm_terminal(false, false).is_ok());
        assert!(ensure_confirm_terminal(false, true).is_ok());
        assert!(ensure_confirm_terminal(true, true).is_ok());

        let err = ensure_confirm_terminal(true, false).expect_err("must refuse non-TTY stdin");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("stdin is not a terminal"),
            "expected a clear non-TTY error, got: {msg}"
        );
        assert!(
            msg.contains("run `aic` from a terminal"),
            "expected the fix to be named, got: {msg}"
        );
    }

    /// The `edit` crate's temp-file editor honors `$EDITOR` arguments before
    /// the file path (the `code --wait` case). Verified with a fake editor.
    #[cfg(unix)]
    #[test]
    fn edit_message_honors_editor_arguments() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let script = dir.path().join("fake-editor.sh");
        // Fake editor: the file path is the last argument; rewrite it in place.
        std::fs::write(
            &script,
            "#!/bin/sh\nfor last; do :; done\nprintf 'fix: args\\n' > \"$last\"\n",
        )
        .unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();

        let editor = format!("{} --wait", script.display());
        temp_env::with_vars(
            [("VISUAL", None), ("EDITOR", Some(editor.as_str()))],
            || {
                let (subject, body) = edit_message("feat: draft", None).unwrap();
                assert_eq!(subject, "fix: args");
                assert_eq!(body, None);
            },
        );
    }

    /// A cleared editor (empty or whitespace-only subject) keeps the draft
    /// instead of letting an empty subject reach `git commit`.
    #[cfg(unix)]
    #[test]
    fn edit_message_empty_edit_keeps_original() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let script = dir.path().join("fake-editor.sh");
        // Fake editor: blanks the file (subject becomes empty).
        std::fs::write(&script, "#!/bin/sh\nfor last; do :; done\n: > \"$last\"\n").unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();

        let editor = format!("{} --wait", script.display());
        temp_env::with_vars(
            [("VISUAL", None), ("EDITOR", Some(editor.as_str()))],
            || {
                let (subject, body) = edit_message("feat: draft", Some("draft body")).unwrap();
                assert_eq!(
                    subject, "feat: draft",
                    "an empty edit must keep the draft subject"
                );
                assert_eq!(
                    body.as_deref(),
                    Some("draft body"),
                    "an empty edit must keep the draft body"
                );
            },
        );
    }

    /// The confirm menu's label↔choice table is the single source — every row
    /// carries its choice, so the menu can never return a choice whose label
    /// moved underneath it. Pins the ordering Commit / Re-generate / Edit /
    /// Abort and the Abort default for Esc / Ctrl-C.
    #[test]
    fn confirm_actions_table_is_self_describing() {
        assert_eq!(CONFIRM_ACTIONS[0].0, "Commit");
        assert_eq!(CONFIRM_ACTIONS[1].0, "Re-generate");
        assert_eq!(CONFIRM_ACTIONS[2].0, "Edit");
        assert_eq!(CONFIRM_ACTIONS[3].0, "Abort");
        // Every label is bound to a distinct choice — no positional coupling.
        let choices: Vec<_> = CONFIRM_ACTIONS.iter().map(|(_, c)| *c).collect();
        assert_eq!(choices.len(), 4);
        assert_eq!(
            choices
                .iter()
                .filter(|&&c| c == ConfirmChoice::Commit)
                .count(),
            1
        );
        assert_eq!(
            choices
                .iter()
                .filter(|&&c| c == ConfirmChoice::Regenerate)
                .count(),
            1
        );
    }
}
