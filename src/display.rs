use console::{Style, Term};

use crate::git::{ConflictedFile, RepoState};
use crate::types::CommitType;

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

    /// Commit-completion line — shown after each commit.
    ///
    /// `prefix` is prepended for batch progress (e.g. `[1/3]`);
    /// pass `""` for single-commit or staged workflows.
    pub fn commit_line(&self, hash: &str, message: &str, body: Option<&str>, prefix: &str) {
        let gray = Style::new().true_color(138, 143, 159);

        // Main line: [prefix] ✓ <hash> <message>
        let check = self.styled("\u{2713}", Style::new().green().bold());
        let hash_styled = self.styled(hash, Style::new().true_color(243, 179, 64));

        // Decompose the subject once on CommitType, then style the parts.
        let parsed = CommitType::parse_message(message);
        let msg_styled = match parsed.description {
            Some(desc) => {
                let colored_type = self.styled(parsed.type_name, parsed.commit_type.color());
                let scope = match parsed.scope {
                    Some(s) => self.styled(&format!("({s})"), gray.clone()),
                    None => String::new(),
                };
                let bold_desc = self.styled(desc, Style::new().bold());
                format!("{}{}: {}", colored_type, scope, bold_desc)
            }
            // No colon — unknown type; color the whole message via the type palette.
            None => self.styled(message, parsed.commit_type.color()),
        };

        let pre = if prefix.is_empty() {
            String::new()
        } else {
            format!("{} ", self.styled(prefix, gray.clone()))
        };
        self.writeln(&format!("{pre}{check} {hash_styled} {msg_styled}"));

        // Optional body — indented, gray
        if let Some(b) = body {
            let trimmed = b.trim();
            if !trimmed.is_empty() {
                for bline in trimmed.lines() {
                    self.writeln(&format!("  {}", self.styled(bline, gray.clone())));
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

    /// `aic` with nothing staged and nothing unstaged — no work for the LLM.
    pub fn nothing_to_commit(&self) {
        let dim = Style::new().dim();
        self.writeln(&self.styled("nothing to commit — working tree clean", dim));
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

// ------------------------------------------------------------------
// Internal formatting helpers
// ------------------------------------------------------------------

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
    use parking_lot::Mutex;
    use std::sync::Arc;

    // `console` reads the process-global `colors_enabled()` flag at format
    // time, so every test that flips it via `ColorGuard` races every other.
    // Lock here for the whole test body to serialize the color-env tests and
    // keep the suite safe to run multi-threaded.
    static COLOR_ENV: Mutex<()> = Mutex::new(());

    /// Forces `console` to emit ANSI escapes for the guard's lifetime,
    /// restoring the prior state on drop. `console::Style` only renders
    /// escapes when the global `colors_enabled()` is true; in the test runner
    /// stdout isn't a TTY, so we flip it to observe the truecolor bytes. Safe
    /// here: no other test in this crate renders console styles, so the global
    /// can't race a concurrent assertion.
    struct ColorGuard {
        prev: bool,
    }
    impl ColorGuard {
        fn force() -> Self {
            let prev = console::colors_enabled();
            console::set_colors_enabled(true);
            ColorGuard { prev }
        }
    }
    impl Drop for ColorGuard {
        fn drop(&mut self) {
            console::set_colors_enabled(self.prev);
        }
    }

    /// In-memory sink: shares its line buffer via `Arc` so the test can read
    /// what `Display` wrote after the fact. `colors_enabled` is configurable to
    /// exercise both the styled and plain branches.
    struct Buf {
        colors: bool,
        lines: Arc<Mutex<Vec<String>>>,
    }

    impl DisplayWrite for Buf {
        fn write_line(&self, line: &str) {
            self.lines.lock().push(line.to_string());
        }
        fn colors_enabled(&self) -> bool {
            self.colors
        }
    }

    #[test]
    fn plain_when_colors_disabled() {
        let lines = Arc::new(Mutex::new(Vec::new()));
        let d = Display::with(Buf {
            colors: false,
            lines: lines.clone(),
        });
        d.commit_line("abc1234", "feat: add thing", Some("body line"), "[1/3]");
        let got = lines.lock().clone();
        // No ANSI escapes; [n/m] prefix retained (not collapsed to "n.").
        // Type prefix "feat" is present, followed by ": add thing"
        assert_eq!(got[0], "[1/3] \u{2713} abc1234 feat: add thing");
        assert_eq!(got[1], "  body line");
    }

    #[test]
    fn truecolor_when_colors_enabled() {
        let _env = COLOR_ENV.lock();
        let _guard = ColorGuard::force();
        let lines = Arc::new(Mutex::new(Vec::new()));
        let d = Display::with(Buf {
            colors: true,
            lines: lines.clone(),
        });
        d.commit_line("abc1234", "feat: add thing", Some("body line"), "[1/3]");
        let joined = lines.lock().join("\n");
        // hash #f3b340, feat type green #4ade80, description bold default fg,
        // body + prefix gray #8a8f9f.
        assert!(
            joined.contains("243;179;64"),
            "hash color missing: {joined:?}"
        );
        assert!(
            joined.contains("74;222;128"),
            "feat type green color missing: {joined:?}"
        );
        assert!(
            joined.contains("\u{1b}[1madd thing"),
            "description must be bold with theme default fg: {joined:?}"
        );
        assert!(
            !joined.contains("255;255;255"),
            "subject must not use hardcoded white: {joined:?}"
        );
        assert!(
            joined.contains("138;143;159"),
            "gray color missing: {joined:?}"
        );
        // [n/m] prefix text survives styling (format kept, not "n.").
        assert!(joined.contains("[1/3]"), "prefix text missing: {joined:?}");
    }

    #[test]
    fn fix_type_gets_yellow_orange_color() {
        let _env = COLOR_ENV.lock();
        let _guard = ColorGuard::force();
        let lines = Arc::new(Mutex::new(Vec::new()));
        let d = Display::with(Buf {
            colors: true,
            lines: lines.clone(),
        });
        d.commit_line("def5678", "fix(auth): correct token check", None, "");
        let joined = lines.lock().join("\n");
        // fix type should be yellow/orange #fbbf24
        assert!(
            joined.contains("251;191;36"),
            "fix type yellow/orange color missing: {joined:?}"
        );
        // Scope parens must survive rendering (regression guard).
        assert!(
            joined.contains("(auth)"),
            "scope parens dropped: {joined:?}"
        );
        // Description should be bold
        assert!(
            joined.contains("\u{1b}[1mcorrect token check"),
            "description must be bold: {joined:?}"
        );
    }

    #[test]
    fn scoped_commit_preserves_parens_plain() {
        let lines = Arc::new(Mutex::new(Vec::new()));
        let d = Display::with(Buf {
            colors: false,
            lines: lines.clone(),
        });
        d.commit_line("def5678", "fix(auth): correct token check", None, "");
        let got = lines.lock().clone();
        // Exact visible text — catches the dropped-paren regression directly.
        assert_eq!(got[0], "\u{2713} def5678 fix(auth): correct token check");
    }

    #[test]
    fn each_type_renders_its_color() {
        let _env = COLOR_ENV.lock();
        let _guard = ColorGuard::force();
        for (type_str, rgb) in [
            ("feat", "74;222;128"),
            ("fix", "251;191;36"),
            ("chore", "156;163;175"),
            ("docs", "96;165;250"),
            ("style", "167;139;250"),
            ("refactor", "34;211;238"),
            ("perf", "248;113;113"),
            ("test", "244;114;182"),
        ] {
            let lines = Arc::new(Mutex::new(Vec::new()));
            let d = Display::with(Buf {
                colors: true,
                lines: lines.clone(),
            });
            d.commit_line("hash000", &format!("{type_str}: msg"), None, "");
            let joined = lines.lock().join("\n");
            assert!(
                joined.contains(rgb),
                "{type_str} should render color {rgb}: {joined:?}"
            );
        }
    }

    #[test]
    fn unknown_type_gets_gray_color() {
        let _env = COLOR_ENV.lock();
        let _guard = ColorGuard::force();
        let lines = Arc::new(Mutex::new(Vec::new()));
        let d = Display::with(Buf {
            colors: true,
            lines: lines.clone(),
        });
        d.commit_line("ghi9012", "wip: thing in progress", None, "");
        let joined = lines.lock().join("\n");
        // Unknown type should be gray #9ca3af
        assert!(
            joined.contains("156;163;175"),
            "unknown type gray color missing: {joined:?}"
        );
    }

    #[test]
    fn no_colon_message_gets_gray_unknown_type() {
        let _env = COLOR_ENV.lock();
        let _guard = ColorGuard::force();
        let lines = Arc::new(Mutex::new(Vec::new()));
        let d = Display::with(Buf {
            colors: true,
            lines: lines.clone(),
        });
        d.commit_line("jkl3456", "no colon message", None, "");
        let joined = lines.lock().join("\n");
        // Messages without colon should be gray (unknown type fallback)
        assert!(
            joined.contains("156;163;175"),
            "no-colon message should be gray: {joined:?}"
        );
    }
}
