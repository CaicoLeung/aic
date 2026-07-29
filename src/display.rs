use console::{Style, Term};

use crate::git::{ConflictedFile, RepoState};

/// Line-based write seam behind [`Display`].
///
/// Prod wires it to [`TermWrite`] (stderr via `console::Term`); tests wire it
/// to an in-memory buffer so emitted wording can be asserted without
/// capturing the process's real stderr. Every method on `Display` ultimately
/// funnels through [`DisplayWrite::write_line`], so swapping the sink is the
/// only seam needed — styling helpers stay unchanged.
///
/// Color capability is a property of the sink, not the environment: a sink
/// that renders to a real terminal reports its color support via
/// [`DisplayWrite::colors_enabled`], and a non-terminal sink (e.g. an
/// in-memory buffer) inherits the `false` default. This keeps `Display` from
/// probing the real stderr when its lines are routed elsewhere.
pub trait DisplayWrite: Send + Sync {
    /// Append a line to the sink. Implementations must be fire-and-forget:
    /// write failures never propagate (status output, never load-bearing).
    fn write_line(&self, line: &str);

    /// Whether the sink can render ANSI color. Terminal sinks override this
    /// to report real color support; non-terminal sinks inherit the `false`
    /// default. `Display` caches the value once at construction.
    fn colors_enabled(&self) -> bool {
        false
    }
}

/// Clean line-based terminal output — no panels, no box-drawing.
///
/// Every method writes to stderr. Color-aware: when colors are disabled
/// (piped output, NO_COLOR, non-TTY) output is plain text with no ANSI
/// escapes.
///
/// Write errors are intentionally ignored: this is fire-and-forget
/// status output to stderr (e.g. a closed pipe), never load-bearing.
pub struct Display {
    out: Box<dyn DisplayWrite>,
    colors: bool,
}

/// Prod sink: stderr via `console::Term`. Write errors are dropped to keep the
/// fire-and-forget contract from [`Display`] intact.
struct TermWrite(Term);

impl DisplayWrite for TermWrite {
    fn write_line(&self, line: &str) {
        let _ = self.0.write_line(line);
    }

    // Colors are a property of where lines land: this sink writes to stderr,
    // so it reports stderr's real color capability rather than letting `Display`
    // probe the environment regardless of sink.
    fn colors_enabled(&self) -> bool {
        console::colors_enabled_stderr()
    }
}

impl Display {
    /// Prod constructor: stderr sink, color-aware, fire-and-forget. Behavior
    /// matches the pre-seam `Display` on every axis the fire-and-forget
    /// contract cares about — same sink (stderr via `console::Term`), same
    /// color probe (`colors_enabled_stderr()`), write errors still ignored.
    pub fn new() -> Self {
        Self::with(TermWrite(Term::stderr()))
    }

    /// Injectable constructor: route every emitted line through `out`. Color
    /// capability is read from the sink itself
    /// ([`DisplayWrite::colors_enabled`]), so a non-terminal sink never probes
    /// the real stderr. Used by tests to capture wording; prod keeps
    /// [`Display::new`].
    pub fn with(out: impl DisplayWrite + 'static) -> Self {
        let colors = out.colors_enabled();
        Self {
            out: Box::new(out),
            colors,
        }
    }

    // ------------------------------------------------------------------
    // Helpers
    // ------------------------------------------------------------------

    /// Apply a console `Style` to text. Returns plain text when colors
    /// are disabled (piped output, NO_COLOR, non-TTY).
    fn styled(&self, text: &str, s: Style) -> String {
        if self.colors {
            s.apply_to(text).to_string()
        } else {
            text.to_string()
        }
    }

    /// Write a line through the seam, ignoring errors.
    fn writeln(&self, line: &str) {
        self.out.write_line(line);
    }

    // ------------------------------------------------------------------
    // Public rendering entry points
    // ------------------------------------------------------------------

    /// Compact notice after formatting Rust files.
    pub fn formatted_notice(&self, count: usize) {
        let word = if count == 1 { "file" } else { "files" };
        let msg = self.styled(
            &format!("  Formatted {} Rust {}", count, word),
            Style::new().dim(),
        );
        self.writeln(&msg);
    }

    /// Batch plan summary shown when unstaged changes are split into
    /// logical commits.
    pub fn batch_summary(&self, batches: &[BatchSummary<'_>]) {
        let count = batches.len();
        if count == 0 {
            return;
        }

        let label = match count {
            1 => "1 commit planned:".to_string(),
            n => format!("{n} commits planned:"),
        };
        self.writeln(&label);

        for (i, b) in batches.iter().enumerate() {
            let reason_part = b.reason.map(|r| format!("[{r}] ")).unwrap_or_default();
            let file_part = format_files_preview(b.files);
            let line = format!("  {}. {}{}", i + 1, reason_part, file_part);
            self.writeln(&line);
        }

        self.writeln(""); // blank separator
    }

    /// Commit-completion line — shown after each commit.
    ///
    /// `prefix` is prepended for batch progress (e.g. `[1/3]`);
    /// pass `""` for single-commit or staged workflows.
    pub fn commit_line(&self, hash: &str, message: &str, body: Option<&str>, prefix: &str) {
        let green_bold = Style::new().green().bold();
        let dim = Style::new().dim();

        // Main line: [prefix] ✓ <hash> <message>
        let check = self.styled("\u{2713}", green_bold.clone());
        let hash_styled = self.styled(hash, Style::new().cyan());
        let msg_styled = self.styled(message, green_bold);
        let pre = if prefix.is_empty() {
            String::new()
        } else {
            format!("{prefix} ")
        };
        self.writeln(&format!("{pre}{check} {hash_styled} {msg_styled}"));

        // Optional body — indented, dim
        if let Some(b) = body {
            let trimmed = b.trim();
            if !trimmed.is_empty() {
                for bline in trimmed.lines() {
                    self.writeln(&format!("  {}", self.styled(bline, dim.clone())));
                }
            }
        }
    }

    // ------------------------------------------------------------------
    // Conflict resolution (ADR 0005)
    // ------------------------------------------------------------------

    /// Header shown when a conflicted repo state is detected.
    pub fn conflict_detected(&self, state: RepoState, count: usize) {
        let yellow = Style::new().yellow().bold();
        let word = if count == 1 { "file" } else { "files" };
        self.writeln(&format!(
            "{} conflicts detected — repo is mid-{} ({} {})",
            self.styled("\u{26A0}", yellow.clone()),
            state.label(),
            count,
            word,
        ));
    }

    /// One-line prompt for the default-run auto-detect: `Resolve now? [y/n]`.
    pub fn resolve_prompt(&self, state: RepoState) {
        let yellow = Style::new().yellow();
        self.writeln(&self.styled(
            &format!("repo is mid-{}; resolve with aic now?", state.label()),
            yellow,
        ));
    }

    /// List conflicted files with their kind. Resolvable files are unmarked;
    /// unresolvable ones carry a `(reason)` tag.
    pub fn conflicted_summary(&self, files: &[ConflictedFile]) {
        let dim = Style::new().dim();
        let yellow = Style::new().yellow();
        for f in files {
            let tag = if f.kind.resolvable() {
                String::new()
            } else {
                format!(
                    " {}",
                    self.styled(&format!("({})", f.kind.reason()), yellow.clone())
                )
            };
            self.writeln(&format!("  {}{}", f.path, tag));
            if let crate::git::ConflictKind::Oversized { bytes, lines } = &f.kind {
                self.writeln(&format!(
                    "    {}",
                    self.styled(
                        &format!("{bytes} bytes, {lines} lines (> cap)"),
                        dim.clone()
                    ),
                ));
            }
        }
        self.writeln("");
    }

    /// Render the combined review diff (original worktree -> LLM resolution).
    /// Coloring by leading sign so a glance distinguishes additions, context,
    /// and the per-file path header:
    ///   `+` addition → green, `-` deletion → red, ` ` context → dim,
    ///   anything else → a bare file path acting as a section header → bold
    ///   cyan. A path can't be mistaken for a diff line because unified-diff
    ///   bodies only ever start with `+`/`-`/` `.
    pub fn review_section(&self, diff: &str) {
        let dim = Style::new().dim();
        self.writeln(&self.styled("proposed resolutions:", dim.clone()));
        let header = Style::new().bold().cyan();
        for line in diff.lines() {
            let styled = match line.chars().next() {
                Some('+') => self.styled(line, Style::new().green()),
                Some('-') => self.styled(line, Style::new().red()),
                Some(' ') => self.styled(line, dim.clone()),
                None => String::new(),
                _ => self.styled(line, header.clone()),
            };
            self.writeln(&styled);
        }
        self.writeln("");
    }

    /// Per-file outcome lines.
    pub fn resolved(&self, path: &str) {
        let green = Style::new().green().bold();
        self.writeln(&format!(
            "  {} resolved + staged: {}",
            self.styled("\u{2713}", green),
            path,
        ));
    }

    pub fn skipped(&self, path: &str, reason: &str) {
        let yellow = Style::new().yellow();
        self.writeln(&format!(
            "  {} skipped: {} ({})",
            self.styled("\u{26A0}", yellow.clone()),
            path,
            reason,
        ));
    }

    pub fn rejected(&self, path: &str) {
        let dim = Style::new().dim();
        self.writeln(&format!(
            "  {} rejected: {}",
            self.styled("\u{2717}", Style::new().red()),
            self.styled(path, dim),
        ));
    }

    /// Finalize succeeded.
    pub fn finalize_done(&self, state: RepoState) {
        let green = Style::new().green().bold();
        self.writeln(&format!(
            "\n{} {} finalized",
            self.styled("\u{2713}", green.clone()),
            self.styled(state.label(), green),
        ));
    }

    /// Partial: approved files staged, but some files still block finalize.
    /// Breaks the blockers down by kind so the user knows whether to re-run
    /// `aic resolve` (rejected/failed), retry a flaky LLM call (failed), or
    /// resolve a binary/oversized file by hand (unresolvable). The old single
    /// "unresolved" count conflated all three.
    pub fn handoff(
        &self,
        approved: usize,
        rejected: usize,
        failed: usize,
        unresolvable: usize,
        state: RepoState,
    ) {
        let yellow = Style::new().yellow();
        let green = Style::new().green().bold();
        let dim = Style::new().dim();
        let cyan = Style::new().cyan();

        self.writeln(&format!(
            "\n{} {approved} resolved + staged",
            self.styled("\u{2713}", green),
        ));

        // Only list blocker categories that actually occurred.
        let mut blockers: Vec<String> = Vec::new();
        if rejected > 0 {
            blockers.push(format!("{rejected} rejected"));
        }
        if failed > 0 {
            blockers.push(format!("{failed} failed to resolve"));
        }
        if unresolvable > 0 {
            blockers.push(format!("{unresolvable} need manual resolution"));
        }
        let blocker_text = if blockers.is_empty() {
            String::from("nothing left")
        } else {
            blockers.join(", ")
        };
        self.writeln(&format!(
            "{} not finalized — {blocker_text}",
            self.styled("\u{26A0}", yellow),
        ));
        self.writeln(&format!(
            "  {}",
            self.styled(
                "resolve the remaining files (or re-run `aic resolve`), then:",
                dim,
            ),
        ));
        self.writeln(&format!("    {}", self.styled(finalize_hint(state), cyan)));
    }

    /// `aic resolve` on a clean repo.
    pub fn no_conflicts(&self) {
        let dim = Style::new().dim();
        self.writeln(&self.styled("no conflicts — nothing to resolve", dim));
    }

    /// `aic resolve` on a rebase/am state — detected but refused in v1.
    pub fn refused(&self, state: RepoState) {
        let red = Style::new().red().bold();
        self.writeln(&format!(
            "{} cannot resolve a {} state in v1",
            self.styled("\u{2717}", red),
            state.label(),
        ));
        self.writeln(&format!(
            "  resolve manually, then run {}",
            self.styled(finalize_hint(state), Style::new().cyan()),
        ));
    }

    /// State is conflicted but the index has no unmerged entries — the user
    /// resolved everything by hand and just needs the finalize step.
    pub fn all_resolved_offer_finalize(&self, state: RepoState) {
        let dim = Style::new().dim();
        self.writeln(&self.styled(
            "no unmerged files remain — conflicts already resolved manually",
            dim,
        ));
        self.writeln(&format!(
            "  finalize with {}",
            self.styled(finalize_hint(state), Style::new().cyan()),
        ));
    }
}

impl Default for Display {
    fn default() -> Self {
        Self::new()
    }
}

/// A batch's files and optional reason, passed to [`Display::batch_summary`].
pub struct BatchSummary<'a> {
    pub files: &'a [String],
    pub reason: Option<&'a str>,
}

// ------------------------------------------------------------------
// Internal formatting helpers
// ------------------------------------------------------------------

/// Compact one-file preview for batch-summary lines.
fn format_files_preview(files: &[String]) -> String {
    if files.is_empty() {
        return String::new();
    }
    if files.len() == 1 {
        return files[0].clone();
    }
    format!("{} (+{} more)", files[0], files.len() - 1)
}

/// The git command a user runs to finalize a state by hand, for hand-off /
/// refuse messages. Mirrors `RepoState::finalize_invocation` but as a single
/// display string (no aic involvement).
fn finalize_hint(state: RepoState) -> &'static str {
    match state {
        RepoState::Merge => "git commit",
        RepoState::CherryPick | RepoState::CherryPickSequence => "git cherry-pick --continue",
        RepoState::Revert | RepoState::RevertSequence => "git revert --continue",
        RepoState::Rebase | RepoState::RebaseInteractive | RepoState::RebaseMerge => {
            "git rebase --continue"
        }
        RepoState::ApplyMailbox | RepoState::ApplyMailboxOrRebase => "git am --continue",
        RepoState::Clean => "git commit",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_preview_empty() {
        assert_eq!(format_files_preview(&[]), "");
    }

    #[test]
    fn file_preview_one() {
        assert_eq!(format_files_preview(&["foo.rs".into()]), "foo.rs");
    }

    #[test]
    fn file_preview_many() {
        let files: Vec<String> = ["a.rs", "b.rs", "c.rs"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        assert_eq!(format_files_preview(&files), "a.rs (+2 more)");
    }
}
