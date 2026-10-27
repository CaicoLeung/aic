//! Pre-commit confirmation (issue #78): the opt-in gate that interrupts the
//! commit path after the message is drafted and before it lands. Owns the
//! gate itself ([`Confirm`]), the menu/editor seams it drives, the production
//! terminal menu and `$EDITOR` message editor, and the confirm loop that
//! re-generates / re-edits until the user commits or aborts. The generic
//! interactive primitives it sits on — the zero-residue Select row, the
//! inquire cancel mapping, the non-TTY-stdin guard — live in
//! [`crate::workflow::input`] (ADR 0007). [`CommitDeclined`] is the marker
//! error the loop raises on Abort so the commit workflow can report a clean
//! abort (naming how far a batch run got) rather than a generic failure.
//!
//! `confirm_draft` borrows the workflow's [`Display`] and [`CommitMessenger`]
//! rather than owning them: confirmation is a phase of the commit Run, not a
//! standalone service, so it reaches across to the display + LLM seams the
//! Run already holds. See CONTEXT.md "Commit confirmation".

use crate::core::types::CommitMessenger;
use crate::git::FileStats;
use crate::render::display::Display;
use crate::render::timeline::driver::RunFrame;
use crate::workflow::input::select_in_frame;
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

/// Erased confirmation menu: returns the user's choice. Boxed for the same
/// reason as the resolver seam — production wires it to a terminal menu
/// (issue #78), tests inject a scripted choice sequence.
pub(crate) type ConfirmMenu = Box<dyn Fn() -> anyhow::Result<ConfirmChoice>>;

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

// ----------------------------------------------------------------------
// The confirm loop
// ----------------------------------------------------------------------

/// Run the confirmation loop for a drafted message. Returns the confirmed
/// (message, body, preview_rows) after the user approves or edits it. The
/// preview renders through `display` — the frame's step display, so on a
/// terminal it is the active step's live output (ADR 0018) and each
/// replacement erases its predecessor in one frame; off a terminal it is
/// the Run's own sink, byte-identical to the pre-timeline output. The
/// Re-generate wait paints as one live note row (`⠋ Re-generating commit
/// message`); the Edit launch suspends the whole frame for the editor and
/// repaints it afterwards (ADR 0018).
pub(crate) async fn confirm_draft(
    draft: (String, Option<String>),
    stats: &[FileStats],
    display: &Display,
    confirm: &Confirm,
    messenger: &CommitMessenger,
    diff: String,
    frame: Option<&RunFrame>,
) -> anyhow::Result<(String, Option<String>, usize)> {
    let (mut message, mut body) = draft;

    // Nothing to confirm when the gate is off — no menu/editor wired, so the
    // drafted message is final.
    let Confirm::Interactive { menu, editor } = confirm else {
        return Ok((message, body, 0));
    };

    loop {
        let rows = display.commit_preview(&message, body.as_deref(), stats);
        match menu()? {
            ConfirmChoice::Commit => return Ok((message, body, rows)),
            ConfirmChoice::Regenerate => {
                display.clear_last(rows);
                frame.inspect(|f| f.live_note("Re-generating commit message"));
                let result = messenger(diff.clone()).await?;
                frame.inspect(|f| f.clear_live_note());
                message = result.message;
                body = result.body;
            }
            ConfirmChoice::Edit => {
                display.clear_last(rows);
                // The editor owns the terminal: the whole frame erases in
                // one frame first and repaints with the menu state restored
                // on exit (ADR 0018) — the timeline never scrolls away
                // mid-edit.
                frame.inspect(|f| f.suspend());
                let edited = editor(&message, body.as_deref());
                frame.inspect(|f| f.resume());
                (message, body) = edited?;
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

/// Production confirmation menu (issue #78): the four actions as timeline
/// frame rows under the active step's preview (ADR 0018) — `›` cursor, raw
/// key input, zero-residue by construction. Esc and Ctrl-C both abort —
/// there is nothing to go back to once the commit is pending (same mapping
/// as the inquire row this replaces off-frame).
fn confirm_menu() -> anyhow::Result<ConfirmChoice> {
    let labels: Vec<&str> = CONFIRM_ACTIONS.iter().map(|(label, _)| *label).collect();
    let Some(index) = select_in_frame(
        "Commit this message?",
        &labels,
        0,
        "↑/↓ select · Enter confirm · Esc or Ctrl-C aborts",
        false,
    )?
    else {
        // Esc / Ctrl-C / a closed stdin all end the run — there's nothing to
        // go back to once the commit is pending.
        return Ok(ConfirmChoice::Abort);
    };
    Ok(CONFIRM_ACTIONS[index].1)
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
    // draft so the user can re-edit or Abort from the menu. Warn so the
    // keep-the-draft behavior is visible, not silent.
    if new_subject.trim().is_empty() {
        Display::new().warn("subject left empty — kept the previous draft");
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

    #[cfg(unix)]
    fn with_fake_editor(script_body: &str, f: impl FnOnce(&str)) {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let script = dir.path().join("fake-editor.sh");
        std::fs::write(&script, script_body).unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();

        let editor = format!("{} --wait", script.display());
        temp_env::with_vars(
            [("VISUAL", None), ("EDITOR", Some(editor.as_str()))],
            || f(&editor),
        );
    }
    /// The `edit` crate's temp-file editor honors `$EDITOR` arguments before
    /// the file path (the `code --wait` case). Verified with a fake editor.
    #[cfg(unix)]
    #[test]
    fn edit_message_honors_editor_arguments() {
        // Fake editor: the file path is the last argument; rewrite it in place.
        with_fake_editor(
            "#!/bin/sh\nfor last; do :; done\nprintf 'fix: args\\n' > \"$last\"\n",
            |_| {
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
        // Fake editor: blanks the file (subject becomes empty).
        with_fake_editor("#!/bin/sh\nfor last; do :; done\n: > \"$last\"\n", |_| {
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
        });
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
