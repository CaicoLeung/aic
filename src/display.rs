use console::{Style, Term};
use std::collections::VecDeque;
use std::future::Future;
use std::time::Duration;

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
    /// Terminal width in columns. Read once from the sink's terminal in
    /// [`Display::new`]; injected by tests via [`Display::with_cols`]. A value
    /// of `0` means "unknown" and [`Display::text_width`] falls back to
    /// [`FALLBACK_COLS`] so piped / non-TTY output still wraps sanely.
    cols: usize,
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
        // `.1` is columns (console returns (rows, cols)); console already
        // substitutes a ~80-col default when the size is unknown, so this is
        // non-zero in practice. We still treat 0 as "unknown" downstream.
        let cols = Term::stderr().size().1 as usize;
        Self::with_cols(TermWrite(Term::stderr()), cols)
    }

    /// Injectable constructor: route every emitted line through `out`. Color
    /// capability is read from the sink itself
    /// ([`DisplayWrite::colors_enabled`]), so a non-terminal sink never probes
    /// the real stderr. Used by tests to capture wording; prod keeps
    /// [`Display::new`].
    pub fn with(out: impl DisplayWrite + 'static) -> Self {
        Self::with_cols(out, FALLBACK_COLS)
    }

    /// Injectable constructor with an explicit terminal column count. Used by
    /// tests to drive wrap behavior deterministically without probing the real
    /// terminal; prod keeps [`Display::new`]. `cols == 0` is treated as
    /// "unknown" and resolved to [`FALLBACK_COLS`] inside [`Display::text_width`].
    /// Private: only the two prod constructors above and the inline test module
    /// call it, and both reach private items — no reason to widen the API.
    fn with_cols(out: impl DisplayWrite + 'static, cols: usize) -> Self {
        let colors = out.colors_enabled();
        Self {
            out: Box::new(out),
            colors,
            cols,
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

    /// Write one line through the seam with the shared [`MARGIN`] prefix,
    /// ignoring errors. Every status/banner line funnels through here so the
    /// whole output block sits at a uniform inset — nothing flush with the
    /// terminal edge. Content that wants deeper nesting keeps its own leading
    /// indent in the formatted string; `emit` adds the base margin on top.
    fn emit(&self, line: &str) {
        self.out.write_line(&format!("{MARGIN}{line}"));
    }

    /// Blank separator line — written bare (no margin) to avoid trailing
    /// whitespace.
    fn emit_blank(&self) {
        self.out.write_line("");
    }

    /// Effective text width for wrapped output: the shared width resolution
    /// ([`resolve_cols`] — fallback + [`HARD_CAP`] cap), minus both margins.
    /// A sub-margin reported width saturates down to `0`, which [`wrap_line`]
    /// treats as "don't wrap" — so a pathologically tiny terminal never
    /// panics, it just emits the body unwrapped.
    fn text_width(&self) -> usize {
        resolve_cols(self.cols).saturating_sub(LEFT_MARGIN + RIGHT_MARGIN)
    }

    // ------------------------------------------------------------------
    // Public rendering entry points
    // ------------------------------------------------------------------

    /// Commit-completion line — shown after each commit.
    ///
    /// `prefix` is prepended for batch progress (e.g. `[1/3]`);
    /// pass `""` for single-commit or staged workflows.
    ///
    /// Layout (ADR: commit-line readability): the whole block is inset by
    /// [`LEFT_MARGIN`] so it isn't flush with the terminal edge. The subject
    /// is a single title line and is **never wrapped** (overflow preferable to
    /// truncation/re-flow). The body is greedy word-wrapped to
    /// [`Display::text_width`] with continuation lines aligned under the first
    /// body char; no hanging indent. Blank body lines stay blank.
    pub fn commit_line(&self, hash: &str, message: &str, body: Option<&str>, prefix: &str) {
        let gray = Style::new().true_color(138, 143, 159);

        // Main line: [prefix] ✓ <hash> <message>
        let check = self.styled("\u{2713}", Style::new().green().bold());
        let hash_styled = self.styled(hash, Style::new().true_color(243, 179, 64));
        let pre = if prefix.is_empty() {
            String::new()
        } else {
            format!("{} ", self.styled(prefix, gray.clone()))
        };
        // Subject: margin only, never wrapped (title line).
        self.emit(&format!(
            "{pre}{check} {hash_styled} {}",
            self.styled_subject(message)
        ));

        // Optional body — margin + greedy word-wrap to text_width, gray.
        // The body's old ad-hoc `  ` indent is subsumed by the shared margin so
        // the whole block sits at one uniform inset.
        if let Some(b) = body {
            self.emit_body(b);
        }
    }

    /// Style a conventional-commit subject line the same way in every
    /// renderer: typed `type` in its palette color, gray `(scope)`, bold
    /// description. Unknown/colon-less messages fall back to the full message
    /// in the type palette color. Shared by [`Display::commit_line`] (post-commit)
    /// and [`Display::commit_preview`] (pre-commit confirmation) so what the
    /// user confirms is byte-for-byte what the completed line will show.
    fn styled_subject(&self, message: &str) -> String {
        let gray = Style::new().true_color(138, 143, 159);
        let parsed = CommitType::parse_message(message);
        match parsed.description {
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
        }
    }

    /// Emit a commit body — margin + greedy word-wrap to text_width, gray.
    /// Blank body lines stay blank (no trailing-whitespace margin). Shared by
    /// [`Display::commit_line`] and [`Display::commit_preview`].
    fn emit_body(&self, body: &str) {
        let gray = Style::new().true_color(138, 143, 159);
        let trimmed = body.trim();
        if !trimmed.is_empty() {
            let width = self.text_width();
            for src_line in trimmed.lines() {
                if src_line.is_empty() {
                    self.emit_blank();
                    continue;
                }
                for piece in wrap_line(src_line, width) {
                    self.emit(&self.styled(&piece, gray.clone()));
                }
            }
        }
    }

    /// Pre-commit confirmation preview (issue #78): the exact message that
    /// would be committed — subject styled exactly as the post-commit line,
    /// body in gray — plus a one-line file list, before the confirmation menu
    /// fires. Lets a user who signs commits (GPG) see what they are signing,
    /// or a user on a weaker local model sanity-check the draft before it
    /// lands.
    pub fn commit_preview(&self, message: &str, body: Option<&str>, paths: &[String]) {
        let dim = Style::new().dim();
        self.emit(&self.styled("proposed commit:", dim.clone()));
        self.emit(&self.styled_subject(message));
        if let Some(b) = body {
            self.emit_body(b);
        }
        let files = if paths.len() == 1 {
            paths[0].clone()
        } else {
            format!("{} ({} files)", paths.join(", "), paths.len())
        };
        self.emit(&self.styled(&format!("files: {files}"), dim));
        self.emit_blank();
    }

    // ------------------------------------------------------------------
    // Conflict resolution (ADR 0005)
    // ------------------------------------------------------------------

    /// Header shown when a conflicted repo state is detected.
    pub fn conflict_detected(&self, state: RepoState, count: usize) {
        let yellow = Style::new().yellow().bold();
        let word = if count == 1 { "file" } else { "files" };
        self.emit(&format!(
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
        self.emit(&self.styled(
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
            self.emit(&format!("  {}{}", f.path, tag));
            if let Some(note) = f.kind.size_note() {
                self.emit(&format!("    {}", self.styled(&note, dim.clone())));
            }
        }
        self.emit_blank();
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
        self.emit(&self.styled("proposed resolutions:", dim.clone()));
        let header = Style::new().bold().cyan();
        for line in diff.lines() {
            // Color is computed on the original diff line (leading +,-, or
            // space), then the shared margin is prepended by `emit` — so the
            // sign-based coloring stays correct while the whole diff block
            // sits at the uniform inset.
            let styled = match line.chars().next() {
                Some('+') => self.styled(line, Style::new().green()),
                Some('-') => self.styled(line, Style::new().red()),
                Some(' ') => self.styled(line, dim.clone()),
                None => String::new(),
                _ => self.styled(line, header.clone()),
            };
            self.emit(&styled);
        }
        self.emit_blank();
    }

    /// Per-file outcome lines.
    pub fn resolved(&self, path: &str) {
        let green = Style::new().green().bold();
        self.emit(&format!(
            "{} resolved + staged: {}",
            self.styled("\u{2713}", green),
            path,
        ));
    }

    pub fn skipped(&self, path: &str, reason: &str) {
        let yellow = Style::new().yellow();
        self.emit(&format!(
            "{} skipped: {} ({})",
            self.styled("\u{26A0}", yellow.clone()),
            path,
            reason,
        ));
    }

    pub fn rejected(&self, path: &str) {
        let dim = Style::new().dim();
        self.emit(&format!(
            "{} rejected: {}",
            self.styled("\u{2717}", Style::new().red()),
            self.styled(path, dim),
        ));
    }

    /// Finalize succeeded.
    pub fn finalize_done(&self, state: RepoState) {
        let green = Style::new().green().bold();
        self.emit_blank();
        self.emit(&format!(
            "{} {} finalized",
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

        self.emit_blank();
        self.emit(&format!(
            "{} {approved} resolved + staged",
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
        self.emit(&format!(
            "{} not finalized — {blocker_text}",
            self.styled("\u{26A0}", yellow),
        ));
        self.emit(&format!(
            "  {}",
            self.styled(
                "resolve the remaining files (or re-run `aic resolve`), then:",
                dim,
            ),
        ));
        self.emit(&format!("    {}", self.styled(&finalize_hint(state), cyan)));
    }

    /// `aic resolve` on a clean repo.
    pub fn no_conflicts(&self) {
        let dim = Style::new().dim();
        self.emit(&self.styled("no conflicts — nothing to resolve", dim));
    }

    /// `aic` with nothing staged and nothing unstaged — no work for the LLM.
    pub fn nothing_to_commit(&self) {
        let dim = Style::new().dim();
        self.emit(&self.styled("nothing to commit — working tree clean", dim));
    }

    /// `aic resolve` on a rebase/am state — detected but refused in v1.
    pub fn refused(&self, state: RepoState) {
        let red = Style::new().red().bold();
        self.emit(&format!(
            "{} cannot resolve a {} state in v1",
            self.styled("\u{2717}", red),
            state.label(),
        ));
        self.emit(&format!(
            "  resolve manually, then run {}",
            self.styled(&finalize_hint(state), Style::new().cyan()),
        ));
    }

    /// State is conflicted but the index has no unmerged entries — the user
    /// resolved everything by hand and just needs the finalize step.
    pub fn all_resolved_offer_finalize(&self, state: RepoState) {
        let dim = Style::new().dim();
        self.emit(&self.styled(
            "no unmerged files remain — conflicts already resolved manually",
            dim,
        ));
        self.emit(&format!(
            "  finalize with {}",
            self.styled(&finalize_hint(state), Style::new().cyan()),
        ));
    }

    /// Generic warning line, routed through the shared margin so ad-hoc
    /// status failures stay visually consistent with the rest of the run's
    /// output instead of being dumped flush to the edge via raw `eprintln!`.
    pub fn warn(&self, msg: &str) {
        let yellow = Style::new().yellow().bold();
        self.emit(&format!("{} {msg}", self.styled("\u{26A0}", yellow)));
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

/// Left inset (columns) for the commit-line block. Replaces the body's old
/// ad-hoc `  ` indent so subject and body share one uniform margin.
const LEFT_MARGIN: usize = 2;

/// The actual prefix string corresponding to [`LEFT_MARGIN`] (two spaces),
/// kept as a `&str` so [`Display::emit`] can prepend it without allocating on
/// every line. Re-exported crate-wide so the spinner templates in `main` share
/// this single source of truth instead of re-hardcoding the literal.
pub(crate) const MARGIN: &str = "  ";

/// Right inset (columns) of breathing room, achieved by wrapping shorter — no
/// trailing spaces are ever printed (they break copy-paste and some terminals
/// strip them).
const RIGHT_MARGIN: usize = 2;

/// Hard ceiling on total line width regardless of how wide the terminal is, so
/// body prose doesn't sprawl into 300-col spaghetti on ultrawide screens.
const HARD_CAP: usize = 100;

/// Terminal width assumed when the real width is unknown (`cols == 0`, i.e.
/// piped / non-TTY output). Matches console's own unix default.
const FALLBACK_COLS: usize = 80;

/// Minimum usable width for in-place progress rendering: the spinner glyph +
/// its label need at least this much room, so a pathologically narrow terminal
/// (or a misreported size) doesn't crush the spinner. Applies only to
/// [`terminal_width`] (progress); wrapped body text instead saturates its
/// margin-subtracted width down to `0` (see [`Display::text_width`]).
const MIN_PROGRESS_WIDTH: usize = 20;

/// Resolve a raw terminal column count into a usable width — the single
/// resolution shared by [`terminal_width`] (progress rendering) and
/// [`Display::text_width`] (wrapped body). `cols == 0` (non-TTY / piped, where
/// `Term::stderr()` reports no size) falls back to [`FALLBACK_COLS`]; the
/// result is capped at [`HARD_CAP`] so ultrawide terminals don't sprawl body
/// prose. Consumers add their own tail: progress floors at
/// [`MIN_PROGRESS_WIDTH`], body text subtracts its margins.
fn resolve_cols(cols: usize) -> usize {
    let cols = if cols == 0 { FALLBACK_COLS } else { cols };
    cols.min(HARD_CAP)
}

/// Greedy word-wrap of a single line (no embedded newlines) to `width` display
/// columns, counted in `char`s (not bytes) so CJK commit bodies wrap correctly.
///
/// The author's structure is preserved: this only breaks a line *further* —
/// callers feed it one source line at a time so existing newlines stay as hard
/// breaks. A single token longer than `width` (e.g. a long URL) is hard-broken
/// at the boundary; one ugly wrap beats a horizontal overflow.
///
/// `width == 0` disables wrapping entirely — the line returns as one piece,
/// unchanged. [`Display::text_width`] yields `0` on a sub-margin terminal, so
/// this guard is load-bearing there, not dead code.
///
/// Returns at least one piece; an empty input yields `vec![""]` so blank
/// source lines round-trip as a single empty piece.
pub(crate) fn wrap_line(line: &str, width: usize) -> Vec<String> {
    if width == 0 {
        return vec![line.to_string()];
    }
    let mut out: Vec<String> = Vec::new();
    let mut cur: Vec<char> = Vec::with_capacity(width);
    for word in line.split_whitespace() {
        let w: Vec<char> = word.chars().collect();
        // If the running line can't accept " <word>", flush it first.
        if !cur.is_empty() && cur.len() + 1 + w.len() > width {
            out.push(cur.iter().collect());
            cur.clear();
        }
        if cur.is_empty() {
            // Word starts a new line — hard-break it if it alone exceeds width.
            let mut idx = 0;
            while w.len() - idx > width {
                let chunk: String = w[idx..idx + width].iter().collect();
                out.push(chunk);
                idx += width;
            }
            cur.extend(&w[idx..]);
        } else {
            cur.push(' ');
            cur.extend(&w);
        }
    }
    out.push(cur.iter().collect());
    out
}

/// The git command a user runs to finalize a state by hand, for hand-off /
/// refuse messages. Derived from [`RepoState::finalize_invocation`] (what aic
/// runs) and [`RepoState::manual_finalize_command`] (what the user runs for
/// states aic refuses) — one mapping, no hand-mirroring.
fn finalize_hint(state: RepoState) -> String {
    if state == RepoState::Clean {
        // Unreachable in practice: hints render only for conflict states.
        return "git commit".to_string();
    }
    let args = state
        .finalize_invocation()
        .or_else(|| state.manual_finalize_command())
        .expect("every conflict state has a finalize or manual command");
    format!("git {}", args.join(" "))
}

// ------------------------------------------------------------------
// Progress: in-place spinner + streaming reasoning feed
// ------------------------------------------------------------------

/// Terminal width for in-place progress rendering. Shares the codebase's one
/// width resolution with [`Display::text_width`] via [`resolve_cols`]
/// (`Term::stderr()`, `0`→[`FALLBACK_COLS`], capped at [`HARD_CAP`]); progress
/// additionally floors at [`MIN_PROGRESS_WIDTH`] so the spinner + label keep
/// room, where `text_width` instead subtracts its margins. The reasoning feed
/// below and the spinner templates consume this.
pub(crate) fn terminal_width() -> usize {
    resolve_cols(Term::stderr().size().1 as usize).max(MIN_PROGRESS_WIDTH)
}

/// Shared indicatif spinner style: a braille tick and a prefix matching
/// [`MARGIN`] so the spinner glyph sits at the same 2-column inset as the rest
/// of the run's stderr block — not flush against the edge. One place to
/// change the inset or tick animation for every spinner in the run.
pub(crate) fn spinner_style() -> anyhow::Result<indicatif::ProgressStyle> {
    Ok(indicatif::ProgressStyle::default_spinner()
        .template(&format!("{MARGIN}{{spinner}} {{msg}}"))?
        .tick_chars("⠋⠙⠹⠸⠼⠴⠦⠧⠇⠏"))
}

/// Run `fut` behind an in-place spinner labeled `msg`; the spinner is cleared
/// when the future completes, success or error.
pub(crate) async fn with_spinner<F, T>(msg: &str, fut: F) -> anyhow::Result<T>
where
    F: Future<Output = anyhow::Result<T>>,
{
    let pb = indicatif::ProgressBar::new_spinner();
    pb.set_style(spinner_style()?);
    pb.set_message(msg.to_string());
    pb.enable_steady_tick(SPINNER_TICK);

    let result = fut.await;
    pb.disable_steady_tick();
    pb.finish_and_clear();
    result
}

/// Animation frame rate for every spinner in the run: the braille tick
/// advances once per tick, so a shorter interval is a smoother spin. 50 ms ≈
/// 20 fps, down from the previous 80 ms.
pub(crate) const SPINNER_TICK: Duration = Duration::from_millis(50);

/// How many reasoning rows stay visible at once — a *rendered-row* cap. The
/// window rolls: each new completed line enters at the bottom and the oldest
/// is dropped once the count exceeds this, so the reasoning block never grows
/// past it. Older reasoning scrolls *within* the window (the newest rows
/// always visible), not into the terminal scrollback.
///
/// Enforced on the rendered rows (see [`reasoning_rows`], which applies it
/// after greedy wrap), so a single long line that wraps to several rows still
/// cannot grow the region. [`ThinkingView`] bounds its stored lines to this
/// count as a memory guard; together with the spinner row the whole in-place
/// region stays at [`REASONING_WINDOW`] + 1 rows.
pub(crate) const REASONING_WINDOW: usize = 12;

/// A rolling window over the model's streamed reasoning, sized to
/// [`REASONING_WINDOW`] logical lines (the retention window; the rendered-row
/// cap lives in [`reasoning_rows`]). The caller renders [`push`](Self::push)'s
/// returned window into the spinner's in-place multi-line message, so the block
/// redraws in place — never printed as permanent lines that linger in the
/// scrollback — and is erased once thinking ends ([`ReasoningRenderer::finish`]),
/// leaving the terminal clean for the rest of the run. The cap keeps the block
/// bounded while it streams, so no unbounded region ever accumulates.
///
/// Completed lines (terminated by `\n`) roll into the window; the in-progress
/// partial line (no trailing `\n` yet) is shown as the window's last line while
/// it builds and counts against the same budget. Blank lines are dropped to
/// keep the feed information-dense.
pub(crate) struct ThinkingView {
    lines: VecDeque<String>,
    cur: String,
}

impl ThinkingView {
    pub(crate) fn new() -> Self {
        Self {
            lines: VecDeque::new(),
            cur: String::new(),
        }
    }

    /// Ingest a reasoning delta (may be a partial line, many lines, or empty)
    /// and return the current window: the newest completed lines plus the
    /// in-progress partial line, oldest-first, capped to [`REASONING_WINDOW`]
    /// lines. A delta that ends mid-line leaves the partial buffered and shown
    /// as the window's last line until the next `\n`.
    pub(crate) fn push(&mut self, delta: &str) -> Vec<String> {
        for ch in delta.chars() {
            if ch == '\n' {
                let line = std::mem::take(&mut self.cur);
                if !line.trim().is_empty() {
                    self.push_line(line);
                }
            } else {
                self.cur.push(ch);
            }
        }
        self.window()
    }

    /// Append a completed line, then bound *storage* to [`REASONING_WINDOW`] —
    /// not the visible window. This stops a long chain-of-thought from
    /// retaining every completed line forever; the visible cap lives in
    /// [`window`](Self::window), which also accounts for the in-progress
    /// partial row.
    fn push_line(&mut self, line: String) {
        self.lines.push_back(line);
        if self.lines.len() > REASONING_WINDOW {
            self.lines.pop_front();
        }
    }

    /// The line-level window: the newest [`REASONING_WINDOW`] lines, completed
    /// lines oldest→newest then the in-progress partial as the last line. The
    /// partial counts against the same budget, so a full window trims its
    /// oldest completed line here to make room. This bounds retention
    /// (memory); the *visible* row cap is applied by [`reasoning_rows`] after
    /// wrapping, since one line can render to several rows.
    fn window(&self) -> Vec<String> {
        let mut rows: Vec<String> = self.lines.iter().cloned().collect();
        if !self.cur.trim().is_empty() {
            rows.push(self.cur.clone());
        }
        let start = rows.len().saturating_sub(REASONING_WINDOW);
        rows[start..].to_vec()
    }
}

/// Braille frames for the analysis spinner. Advanced once per reasoning
/// delta (see [`ReasoningRenderer`]) so the glyph spins while reasoning flows
/// and freezes when it stalls — incidental animation that needs no background
/// ticker, since a steady tick is exactly what forced indicatif's flickering
/// full-block repaints.
const REASONING_GLYPHS: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

/// Build the visual rows for one reasoning frame: the spinner+label row on
/// top, then each retained reasoning line greedy-wrapped under the shared
/// `│ ` indent. Pure (no I/O) so the layout is unit-testable; the renderer
/// paints exactly what this returns.
///
/// `feed_width` is the per-piece wrap budget. An empty `window` yields just
/// the spinner row. The reasoning rows (everything below the spinner row) are
/// capped to [`REASONING_WINDOW`]: a long line wraps to several rows, so the
/// newest [`REASONING_WINDOW`] *rendered rows* are kept and the oldest drop
/// out — the top row may start mid-line, exactly like a terminal tail window.
fn reasoning_rows(glyph: &str, label: &str, window: &[String], feed_width: usize) -> Vec<String> {
    let mut rows = Vec::with_capacity(window.len() + 1);
    rows.push(format!("{MARGIN}{glyph} {label}"));
    for line in window {
        for piece in wrap_line(line, feed_width) {
            rows.push(format!("{MARGIN}│ {piece}"));
        }
    }
    // The load-bearing rendered-row cap: the line window alone can't bound
    // the on-screen region, because one long line wraps to many rows. Keep
    // the newest REASONING_WINDOW rows below the spinner row.
    let start = 1 + (rows.len() - 1).saturating_sub(REASONING_WINDOW);
    rows.drain(1..start);
    rows
}

/// Flicker-free in-place renderer for the streaming reasoning window.
///
/// Replaces the indicatif multi-line spinner, whose redraw is a two-phase
/// "blank every row, then repaint every row" — the blank gap scaled with the
/// window height and the spinner's steady tick forced it ~20×/s, so a 12-line
/// (often 20–36 visual rows) block flickered visibly. This renderer paints
/// with **interleaved clear-then-write**: move to the top of the previous
/// frame, then clear a line and rewrite it immediately before descending to
/// the next. At any instant at most one row is blank, and only until it is
/// rewritten — imperceptible regardless of window height or update rate.
///
/// There is no steady tick: a frame is painted only when reasoning changes,
/// and the glyph advances on each paint so it spins with the stream and
/// freezes when it stalls. [`finish`](Self::finish) erases the frame once
/// thinking ends — the reasoning never lingers on screen or in the scrollback
/// (it was in-place all along) — and restores the cursor to the line where the
/// frame began, so the next line of stderr continues with no gap. All writes
/// are best-effort: a closed stderr just stops drawing, never breaks the
/// commit flow. A no-op off a terminal.
pub(crate) struct ReasoningRenderer {
    term: Term,
    label: &'static str,
    feed_width: usize,
    glyph: usize,
    prev_height: usize,
    /// `true` once a frame has been painted and not yet finished — guards
    /// [`Drop`] so a mid-stream error still erases the frame.
    active: bool,
    /// `true` while the terminal cursor is hidden for the active stream. Set
    /// when the first frame emits [`HIDE`] and cleared when [`ReasoningRenderer::finish`] emits
    /// [`SHOW`]. Owning the visibility state explicitly — rather than inferring
    /// it from `prev_height == 0` — is what guarantees every [`HIDE`] is paired
    /// with exactly one [`SHOW`], even if a frame were ever painted with no rows.
    /// That zero-row case is currently unreachable — [`reasoning_rows`] always
    /// yields at least the spinner row, so `first_frame` is true exactly once
    /// per stream — but the flag is retained as a contract guard against a
    /// future caller that paints before any row exists.
    cursor_hidden: bool,
}

/// ANSI escapes for the in-place repaint. Hand-written (rather than via
/// `console::Term`'s cursor/clear methods) so the exact byte sequence is
/// assembled by the pure [`frame_bytes`] helper and its anti-flicker
/// interleaving is unit-testable.
const CLR_LINE: &str = "\x1b[2K"; // erase the current line
const UP: &str = "\x1b[1A"; // cursor up one row
const DOWN: &str = "\x1b[1B"; // cursor down one row

/// Hide the terminal cursor for the whole reasoning stream. While the renderer
/// repaints the frame top-to-bottom the hardware caret would otherwise sit, row
/// by row, at the tail of each freshly written line — mid-row over the
/// *previous* frame's longer text on the rows below — which reads as the caret
/// "jumping between characters" during fast streaming. Emitted once on the
/// first frame and held until [`SHOW`] at [`ReasoningRenderer::finish`], so the
/// caret is simply absent while reasoning flows and reappears, parked, once
/// thinking ends — no per-frame flicker, no smearing across the region.
const HIDE: &str = "\x1b[?25l"; // DECRST — hide cursor
const SHOW: &str = "\x1b[?25h"; // DECSET — show cursor

/// Assemble the byte sequence to repaint `rows` over a previous frame
/// `prev_height` rows tall. The repaint is **interleaved clear-then-write**:
/// move to the top, then for each row `\r` + clear + write, descending between
/// rows; a shorter new frame blanks its stale tail and returns the cursor to
/// the last live row. No two rows are ever blanked without the first being
/// rewritten first — the property that kills the flicker indicatif's
/// "blank-all-then-rewrite-all" repaint suffers from.
///
/// Cursor visibility is a stream-spanning concern, not a per-frame one. The
/// first repaint emits [`HIDE`] (when `prev_height == 0`) and the caret stays
/// hidden for the whole stream; only [`clear_frame_bytes`] at
/// [`ReasoningRenderer::finish`] restores it ([`SHOW`]). Mid-stream repaints
/// emit neither — the caret stays hidden, so where it sits during the traversal
/// is irrelevant and can never smear across the repainted rows.
fn frame_bytes(rows: &[String], prev_height: usize) -> String {
    let height = rows.len();
    let mut out = String::new();
    if prev_height == 0 {
        // First frame of the stream: hide the cursor for the whole stream
        // (restored by clear_frame_bytes at finish). The frame paints in place
        // starting at the cursor's current line — no leading `\n`. Every
        // caller arrives with the cursor at column 0 of a fresh line (stderr
        // output always ends in a newline), so an opening `\n` would only
        // reserve a blank line above the block that `finish` could never
        // reclaim, leaving a permanent gap. Clearing the line first (`\r` +
        // [`CLR_LINE`]) means a mid-line cursor is overwritten cleanly rather
        // than smeared.
        out.push_str(HIDE);
    } else {
        for _ in 0..prev_height.saturating_sub(1) {
            out.push_str(UP);
        }
    }
    for (i, row) in rows.iter().enumerate() {
        out.push('\r');
        out.push_str(CLR_LINE);
        out.push_str(row);
        if i + 1 < height {
            out.push_str(DOWN);
        }
    }
    if height < prev_height {
        for _ in height..prev_height {
            out.push_str(DOWN);
            out.push('\r');
            out.push_str(CLR_LINE);
        }
        for _ in height..prev_height {
            out.push_str(UP);
        }
    }
    out
}

/// Assemble the byte sequence to erase a `prev_height`-row frame, clearing
/// each row from the BOTTOM up so the traversal naturally ends on the frame's
/// TOP row — exactly where the first frame began painting (the cursor's line
/// at stream start). So once thinking ends the cursor sits back on its
/// original line, the block is gone, and the next line of stderr overwrites
/// the blank region from the top with no gap trapped above or below.
///
/// Because the frame never opened with a `\n` (see [`frame_bytes`]), there is
/// no reserved blank line to reclaim and no compensation walk: the bottom-up
/// clear is a single coherent traversal that restores the cursor to its start
/// by construction. (Clearing bottom-up is fine here because this is the final
/// erase, never a mid-stream repaint — the anti-flicker interleaved
/// clear-then-write of [`frame_bytes`] doesn't apply to a one-shot erase.)
///
/// This is the stream's end, so it appends [`SHOW`] to restore the cursor when
/// `cursor_hidden` — i.e. when the first frame's [`HIDE`] was emitted. Keying
/// the restore on the renderer's explicit visibility flag (rather than on
/// `prev_height`) is what makes every [`HIDE`] pair with exactly one [`SHOW`]:
/// a zero-row frame that still hid the cursor restores it; a frame that was
/// never painted (`cursor_hidden == false`) owes nothing. No leading [`HIDE`]:
/// the caret is already hidden, so the clear traversal can't smear it.
fn clear_frame_bytes(prev_height: usize, cursor_hidden: bool) -> String {
    let mut out = String::new();
    // Clear the current (bottom) row, ascend, repeat — ending on the top row.
    for i in 0..prev_height {
        out.push('\r');
        out.push_str(CLR_LINE);
        if i + 1 < prev_height {
            out.push_str(UP);
        }
    }
    if cursor_hidden {
        out.push_str(SHOW);
    }
    out
}

impl ReasoningRenderer {
    /// Bind a renderer to stderr with `label` on the spinner row. `feed_width`
    /// is resolved once from [`terminal_width`]; a resize mid-stream only
    /// changes wrap widths, never correctness.
    pub(crate) fn new(label: &'static str) -> Self {
        Self {
            term: Term::stderr(),
            label,
            feed_width: terminal_width().saturating_sub(6),
            glyph: 0,
            prev_height: 0,
            active: false,
            cursor_hidden: false,
        }
    }

    /// Paint one frame for the reasoning `window`. Safe to call on every
    /// delta; a no-op off a terminal.
    pub(crate) fn paint(&mut self, window: &[String]) {
        if !self.term.is_term() {
            return;
        }
        let glyph = REASONING_GLYPHS[self.glyph % REASONING_GLYPHS.len()];
        self.glyph = self.glyph.wrapping_add(1);
        let rows = reasoning_rows(glyph, self.label, window, self.feed_width);
        self.draw_rows(&rows);
    }

    /// Repaint `rows` in place via [`frame_bytes`]: one write of the assembled
    /// escape sequence, then a flush. Pure byte assembly lives in
    /// [`frame_bytes`] so the anti-flicker interleaving is unit-testable.
    fn draw_rows(&mut self, rows: &[String]) {
        let first_frame = self.prev_height == 0;
        // Record the cursor-hidden state BEFORE the write: the flag mirrors
        // exactly when frame_bytes emits HIDE (prev_height == 0), so finishing
        // the in-memory tracking ahead of the side effect means a hidden cursor
        // can never be stranded — even if a future insertion between here and
        // the write panicked, Drop's finish would still see cursor_hidden and
        // emit the owed SHOW.
        self.cursor_hidden |= first_frame;
        let bytes = frame_bytes(rows, self.prev_height);
        let _ = self.term.write_str(&bytes);
        let _ = self.term.flush();
        self.prev_height = rows.len();
        self.active = true;
    }

    /// End the reasoning stream: erase the whole frame — spinner row and
    /// reasoning rows — so the block vanishes once thinking is done, and
    /// restore the cursor to the line where the frame began (the cursor's
    /// position at stream start), so the rest of the run's stderr continues
    /// with no blank gap above or below. Idempotent; also the [`Drop`]
    /// backstop for an aborted stream.
    pub(crate) fn finish(&mut self) {
        if !self.active {
            return;
        }
        let bytes = clear_frame_bytes(self.prev_height, self.cursor_hidden);
        let _ = self.term.write_str(&bytes);
        let _ = self.term.flush();
        self.prev_height = 0;
        self.active = false;
        self.cursor_hidden = false;
    }
}

impl Drop for ReasoningRenderer {
    fn drop(&mut self) {
        // Backstop: if the stream aborted before `finish`, still erase the
        // frame so the rest of the run's stderr is clean.
        self.finish();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use parking_lot::Mutex;
    use std::sync::Arc;

    /// The contract that replaced the hand-mirrored hint: every conflict state
    /// resolves to the exact command a user runs, derived from the single
    /// `RepoState` mapping (`finalize_invocation` for what aic runs,
    /// `manual_finalize_command` for what the user runs on refused states).
    #[test]
    fn finalize_hint_covers_every_state_with_the_right_command() {
        for (state, expected) in [
            (RepoState::Clean, "git commit"),
            (RepoState::Merge, "git commit --no-edit"),
            (RepoState::CherryPick, "git cherry-pick --continue"),
            (RepoState::CherryPickSequence, "git cherry-pick --continue"),
            (RepoState::Revert, "git revert --continue"),
            (RepoState::RevertSequence, "git revert --continue"),
            (RepoState::Rebase, "git rebase --continue"),
            (RepoState::RebaseInteractive, "git rebase --continue"),
            (RepoState::RebaseMerge, "git rebase --continue"),
            (RepoState::ApplyMailbox, "git am --continue"),
            (RepoState::ApplyMailboxOrRebase, "git am --continue"),
        ] {
            assert_eq!(finalize_hint(state), expected, "state {state:?}");
        }
    }

    /// [`ThinkingView::push`] returns the current window: completed lines
    /// (blank ones dropped) in arrival order, oldest-first.
    #[test]
    fn thinking_view_window_shows_completed_lines_and_drops_blanks() {
        let mut v = ThinkingView::new();
        let window = v.push("line 1\n\nline 2\n");
        assert_eq!(window, vec!["line 1", "line 2"]);
    }

    /// A partial line with no trailing `\n` is the window's last row while it
    /// builds, then collapses into a completed row when the `\n` arrives.
    #[test]
    fn thinking_view_partial_is_last_window_row_until_newline() {
        let mut v = ThinkingView::new();
        assert_eq!(v.push("in progress"), vec!["in progress"]);
        let window = v.push(" done\n");
        assert_eq!(window, vec!["in progress done"]);
    }

    /// One logical line split across several deltas assembles into a single
    /// window row, shown live as it grows.
    #[test]
    fn thinking_view_assembles_split_chunks() {
        let mut v = ThinkingView::new();
        assert_eq!(v.push("hel"), vec!["hel"]);
        assert_eq!(v.push("lo"), vec!["hello"]);
        assert_eq!(v.push(" world\n"), vec!["hello world"]);
    }

    /// A delta containing several `\n`-separated lines yields a window with
    /// each one, in order.
    #[test]
    fn thinking_view_many_lines_one_delta() {
        let mut v = ThinkingView::new();
        assert_eq!(v.push("a\nb\nc\n"), vec!["a", "b", "c"]);
    }

    /// The window rolls: past [`REASONING_WINDOW`] rows the oldest completed
    /// line is dropped, so the window stays capped and always shows the newest
    /// rows.
    #[test]
    fn thinking_view_rolls_at_capacity() {
        let mut v = ThinkingView::new();
        for i in 1..=15 {
            v.push(&format!("line {i}\n"));
        }
        let window = v.push("");
        assert_eq!(window.len(), REASONING_WINDOW);
        assert_eq!(window.first(), Some(&"line 4".to_string()));
        assert_eq!(window.last(), Some(&"line 15".to_string()));
    }

    /// The in-progress partial line counts against the same budget: with the
    /// window full of completed rows, a partial drops the oldest completed row
    /// so the window never exceeds [`REASONING_WINDOW`].
    #[test]
    fn thinking_view_partial_counts_against_budget() {
        let mut v = ThinkingView::new();
        for i in 1..=REASONING_WINDOW {
            v.push(&format!("line {i}\n"));
        }
        let window = v.push("in progress");
        assert_eq!(window.len(), REASONING_WINDOW);
        // oldest completed row rolled out to make room for the partial
        assert_eq!(window.first(), Some(&"line 2".to_string()));
        assert_eq!(window.last(), Some(&"in progress".to_string()));
    }

    /// [`reasoning_rows`] always leads with the spinner+label row, even when
    /// the window is empty (the stream just started).
    #[test]
    fn reasoning_rows_leads_with_spinner_for_empty_window() {
        let rows = reasoning_rows("⠋", "Analyzing", &[], 80);
        assert_eq!(rows, vec![format!("{MARGIN}⠋ Analyzing")]);
    }

    /// Each retained line becomes one indented row when it fits the budget.
    #[test]
    fn reasoning_rows_indents_each_line() {
        let window = vec!["line 1".to_string(), "line 2".to_string()];
        let rows = reasoning_rows("⠙", "Analyzing", &window, 80);
        assert_eq!(
            rows,
            vec![
                format!("{MARGIN}⠙ Analyzing"),
                format!("{MARGIN}│ line 1"),
                format!("{MARGIN}│ line 2"),
            ]
        );
    }

    /// A line longer than the budget wraps to several rows under the same
    /// `│ ` indent — the visual height can exceed the logical-line count.
    #[test]
    fn reasoning_rows_wraps_long_lines_to_multiple_rows() {
        let prefix = format!("{MARGIN}│ ");
        let rows = reasoning_rows("⠹", "Analyzing", &["the quick brown fox".to_string()], 10);
        assert_eq!(rows.first(), Some(&format!("{MARGIN}⠹ Analyzing")));
        // every row after the spinner carries the `│ ` indent…
        for row in &rows[1..] {
            assert!(row.starts_with(&prefix), "unindented row: {row:?}");
        }
        // …and the words survive the greedy wrap, rejoinable losslessly.
        let body: String = rows[1..]
            .iter()
            .filter_map(|r| r.strip_prefix(&prefix))
            .collect::<Vec<_>>()
            .join(" ");
        assert_eq!(body, "the quick brown fox");
    }

    /// The load-bearing rendered-row cap: wrap pieces count against
    /// [`REASONING_WINDOW`], so a window whose lines wrap to more rows than
    /// the budget shows only the newest rows — the oldest wrap pieces (and
    /// whole lines) roll out, the spinner row is never dropped.
    #[test]
    fn reasoning_rows_caps_rendered_rows_to_window_budget() {
        let prefix = format!("{MARGIN}│ ");
        // 12 lines × 2 wrap pieces = 24 rendered rows, over the 12-row budget.
        let window: Vec<String> = (1..=12).map(|i| format!("line {i} with words")).collect();
        let rows = reasoning_rows("⠹", "Analyzing", &window, 10);
        assert_eq!(rows.len(), REASONING_WINDOW + 1);
        assert_eq!(rows.first(), Some(&format!("{MARGIN}⠹ Analyzing")));
        for row in &rows[1..] {
            assert!(row.starts_with(&prefix), "unindented row: {row:?}");
        }
        // The newest rows survived, losslessly rejoinable: lines 7–12 only.
        let body: String = rows[1..]
            .iter()
            .filter_map(|r| r.strip_prefix(&prefix))
            .collect::<Vec<_>>()
            .join(" ");
        assert_eq!(
            body,
            (7..=12)
                .map(|i| format!("line {i} with words"))
                .collect::<Vec<_>>()
                .join(" ")
        );
    }

    /// A single pathological line that wraps far past the budget still cannot
    /// grow the region: only the newest [`REASONING_WINDOW`] rendered rows are
    /// kept, the top row a mid-line cut like a terminal tail window.
    #[test]
    fn reasoning_rows_caps_single_long_line() {
        let prefix = format!("{MARGIN}│ ");
        let long = "word ".repeat(60); // 300 chars → 30 wrap pieces at width 10
        let rows = reasoning_rows("⠹", "Analyzing", &[long], 10);
        assert_eq!(rows.len(), REASONING_WINDOW + 1);
        assert_eq!(rows.first(), Some(&format!("{MARGIN}⠹ Analyzing")));
        // 12 rows of "word word" = 24 words of the 60 — the newest tail only.
        let words: usize = rows[1..]
            .iter()
            .map(|r| r.strip_prefix(&prefix).unwrap().split_whitespace().count())
            .sum();
        assert_eq!(words, 24);
    }

    /// First paint (no previous frame) hides the cursor and clears+writes its
    /// single row in place at the cursor's current line — no leading newline
    /// (which would reserve an unreclaimable blank line above the block) and
    /// no cursor-up preamble.
    #[test]
    fn frame_bytes_first_frame_paints_in_place() {
        let out = frame_bytes(&["only".to_string()], 0);
        assert_eq!(out, format!("{HIDE}\r{CLR_LINE}only"));
        // the first frame never emits a newline: blank lines are unreclaimable.
        assert!(!out.contains('\n'), "first frame must not emit a newline");
    }

    /// The load-bearing anti-flicker property: a same-height repaint clears
    /// each row and rewrites it *immediately* before descending — never the
    /// "blank every row, then rewrite every row" two-phase that flickered. The
    /// full byte sequence is asserted, so any regression to a clear-all-then-
    /// write-all repaint fails here.
    #[test]
    fn frame_bytes_repaint_is_interleaved_clear_then_write() {
        let rows = vec!["a".to_string(), "b".to_string(), "c".to_string()];
        let out = frame_bytes(&rows, 3);
        // up to top (2 rows), then per row: CR + clear + write, descending.
        let expected = format!("{UP}{UP}\r{CLR_LINE}a{DOWN}\r{CLR_LINE}b{DOWN}\r{CLR_LINE}c");
        assert_eq!(out, expected);
        // each cleared row is rewritten before the next is touched: no run of
        // two clears without content between them.
        assert!(
            !out.contains(&format!("{CLR_LINE}{CLR_LINE}")),
            "adjacent clears would blank multiple rows at once (flicker)"
        );
    }

    /// A shorter new frame blanks the stale tail rows below it and returns the
    /// cursor to the last live row, so a shrunken window leaves no ghosts.
    #[test]
    fn frame_bytes_shorter_frame_clears_stale_tail() {
        let out = frame_bytes(&["a".to_string()], 3);
        let expected = format!("{UP}{UP}\r{CLR_LINE}a{DOWN}\r{CLR_LINE}{DOWN}\r{CLR_LINE}{UP}{UP}");
        assert_eq!(out, expected);
    }

    /// A taller new frame descends into fresh rows below the previous frame —
    /// each new row cleared then written in place as it appears.
    #[test]
    fn frame_bytes_taller_frame_descends_into_new_rows() {
        let rows = vec!["a".to_string(), "b".to_string(), "c".to_string()];
        let out = frame_bytes(&rows, 1);
        let expected = format!("\r{CLR_LINE}a{DOWN}\r{CLR_LINE}b{DOWN}\r{CLR_LINE}c");
        assert_eq!(out, expected);
    }

    /// Erasing a frame clears each row from the bottom up — clear the current
    /// (bottom) row, ascend, repeat — so the traversal naturally ends on the
    /// frame's TOP row (the cursor's line at stream start), with no separate
    /// return walk and no blank gap left above the next line of stderr.
    #[test]
    fn clear_frame_bytes_erases_bottom_up_and_ends_at_top() {
        // 2-row frame: clear the bottom row, ascend, clear the top row. Cursor
        // was hidden → SHOW restores it. Ends on the top row by construction.
        let out = clear_frame_bytes(2, true);
        assert_eq!(out, format!("\r{CLR_LINE}{UP}\r{CLR_LINE}{SHOW}"));
        // 1-row frame: a single clear, no movement.
        assert_eq!(clear_frame_bytes(1, true), format!("\r{CLR_LINE}{SHOW}"));
    }

    /// Regression: the reasoning stream must leave NO trace — neither a blank
    /// gap nor a reserved blank line — between the output that preceded the
    /// block and the output that follows it. The frame paints in place at the
    /// cursor's current line (no opening newline), so its top row is the
    /// cursor's start row; `finish` must restore the cursor to that same row
    /// and the whole stream must emit zero newlines (a `\n` is an unreclaimable
    /// blank line in a terminal, the original cause of the lingering empty
    /// area). The old design opened with a `\n` and parked the cursor on the
    /// cleared region's bottom row, leaving up to `REASONING_WINDOW` blank rows
    /// above the next write — the "empty area" after thinking ended.
    #[test]
    fn finish_leaves_no_blank_gap_above_next_output() {
        /// Walk `bytes` from `start_row` and return the final cursor row,
        /// honouring only the moves the renderer emits (`\n`, `\r`, UP, DOWN;
        /// clears and cursor-visibility toggles don't move the row).
        fn cursor_row_after(bytes: &str, start_row: i32) -> i32 {
            let mut row = start_row;
            let mut i = 0;
            while i < bytes.len() {
                if bytes[i..].starts_with('\n') {
                    row += 1;
                    i += 1;
                } else if bytes[i..].starts_with('\r') {
                    i += 1;
                } else if bytes[i..].starts_with(UP) {
                    row -= 1;
                    i += UP.len();
                } else if bytes[i..].starts_with(DOWN) {
                    row += 1;
                    i += DOWN.len();
                } else if bytes[i..].starts_with(CLR_LINE) {
                    i += CLR_LINE.len();
                } else if bytes[i..].starts_with(HIDE) {
                    i += HIDE.len();
                } else if bytes[i..].starts_with(SHOW) {
                    i += SHOW.len();
                } else {
                    i += 1;
                }
            }
            row
        }

        for height in [1usize, 2, 3, REASONING_WINDOW, REASONING_WINDOW + 1] {
            let rows: Vec<String> = (0..height).map(|_| "x".to_string()).collect();
            let paint = frame_bytes(&rows, 0);
            // The stream never emits a newline: a `\n` is a permanently blank
            // line the terminal cannot un-scroll, so the whole in-place stream
            // (paint + erase) must be newline-free to leave no trace.
            assert!(
                !paint.contains('\n'),
                "height {height}: first frame must not emit a newline"
            );
            // The frame paints at the cursor's line, so its bottom row is
            // `height - 1` and that's where the cursor sits after painting.
            let after_paint = cursor_row_after(&paint, 0);
            assert_eq!(after_paint, height as i32 - 1, "paint height {height}");
            // finish must restore the cursor to its START row (0), not the
            // bottom — otherwise `height - 1` blank rows linger above the next
            // line of stderr.
            let after_clear = cursor_row_after(&clear_frame_bytes(height, true), after_paint);
            assert_eq!(
                after_clear, 0,
                "height {height}: cursor must return to start row (0), \
                 not the bottom (row {after_paint})"
            );
        }
    }

    /// The cursor is hidden for the whole reasoning stream and restored only
    /// when thinking ends — otherwise the caret would rest, row by row, at the
    /// tail of each freshly written line (mid-row over the previous frame's
    /// longer text) and read as "jumping between characters" during fast
    /// streaming. The first frame hides it; mid-stream repaints leave it hidden
    /// (no per-frame flicker); the final erase is the only thing that restores
    /// it.
    #[test]
    fn cursor_hidden_for_whole_stream_restored_only_at_finish() {
        // The first frame (prev_height 0) hides the cursor and never restores
        // it — the caret is gone for the entire stream.
        let first = frame_bytes(&["only".to_string()], 0);
        assert!(first.starts_with(HIDE), "first frame must hide the cursor");
        assert!(
            !first.contains(SHOW),
            "first frame must not restore mid-stream"
        );

        // A mid-stream repaint touches neither: the caret stays hidden, so its
        // position during the traversal can't smear across the repainted rows.
        let repaint = frame_bytes(&["a".to_string(), "b".to_string()], 2);
        assert!(
            !repaint.contains(HIDE) && !repaint.contains(SHOW),
            "a mid-stream repaint must not touch cursor visibility"
        );

        // The cursor only comes back when the stream ends (clear_frame_bytes),
        // keyed on the renderer's cursor-hidden flag — not on prev_height.
        let erase = clear_frame_bytes(2, true);
        assert!(erase.ends_with(SHOW), "finish must restore the cursor");
        // A frame that hid no cursor owes no SHOW — geometry only (bottom-up
        // clear ending at the top), no restore.
        assert_eq!(
            clear_frame_bytes(2, false),
            format!("\r{CLR_LINE}{UP}\r{CLR_LINE}")
        );
    }

    /// A zero-height frame that was never painted (`cursor_hidden == false`)
    /// is a no-op — keeps [`ReasoningRenderer::finish`] trivially safe when no
    /// frame was drawn.
    #[test]
    fn clear_frame_bytes_noop_for_zero_rows() {
        assert_eq!(clear_frame_bytes(0, false), "");
    }

    /// The `SHOW` owed is decided by `cursor_hidden`, not `prev_height`: a
    /// zero-row frame whose first draw still hid the cursor must restore it,
    /// even though there are no rows to erase. This is exactly the case that
    /// inferring visibility from `prev_height` would leak — hiding with no
    /// painted row, then finishing with `prev_height == 0` → no `SHOW`.
    #[test]
    fn clear_frame_bytes_restores_cursor_for_zero_row_hidden_frame() {
        assert_eq!(clear_frame_bytes(0, true), SHOW);
    }

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
    fn commit_preview_renders_message_body_and_file_list() {
        let lines = Arc::new(Mutex::new(Vec::new()));
        let d = Display::with(Buf {
            colors: false,
            lines: lines.clone(),
        });
        d.commit_preview(
            "feat(auth): add OAuth2 login support",
            Some("Allow users to sign in via Google and GitHub OAuth2 providers"),
            &["src/auth.rs".to_string(), "src/main.rs".to_string()],
        );
        let got = lines.lock().clone();
        // Header, styled subject, body, and file list all sit at the shared
        // margin; a trailing blank separates the preview from the y/n prompt.
        assert_eq!(got[0], "  proposed commit:");
        assert_eq!(got[1], "  feat(auth): add OAuth2 login support");
        assert_eq!(
            got[2],
            "  Allow users to sign in via Google and GitHub OAuth2 providers"
        );
        assert_eq!(got[3], "  files: src/auth.rs, src/main.rs (2 files)");
        assert_eq!(got[4], "");
    }

    #[test]
    fn commit_preview_singleton_file_list_omits_count() {
        let lines = Arc::new(Mutex::new(Vec::new()));
        let d = Display::with(Buf {
            colors: false,
            lines: lines.clone(),
        });
        d.commit_preview("chore: bump dep", None, &["Cargo.toml".to_string()]);
        let got = lines.lock().clone();
        assert_eq!(got[0], "  proposed commit:");
        assert_eq!(got[1], "  chore: bump dep");
        // Single file: no "(1 files)" suffix; no body line emitted.
        assert_eq!(got[2], "  files: Cargo.toml");
        assert_eq!(got.len(), 4, "no body line expected, got: {got:?}");
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
        // Type prefix "feat" is present, followed by ": add thing". Subject now
        // carries a 2-col left margin; body line sits at the same margin (its
        // old ad-hoc indent was subsumed by the shared margin).
        assert_eq!(got[0], "  [1/3] \u{2713} abc1234 feat: add thing");
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
        // Subject carries a 2-col left margin.
        assert_eq!(got[0], "  \u{2713} def5678 fix(auth): correct token check");
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

    // ------------------------------------------------------------------
    // Margin + width-wrap (commit-line readability)
    // ------------------------------------------------------------------

    #[test]
    fn body_wraps_at_text_width() {
        // cols=80 → text_width = min(80,100) - 4 = 76.
        let lines = Arc::new(Mutex::new(Vec::new()));
        let d = Display::with_cols(
            Buf {
                colors: false,
                lines: lines.clone(),
            },
            80,
        );
        let long = "word ".repeat(40); // 40 tokens, well over 76 cols
        d.commit_line("h000001", "feat: x", Some(&long), "");
        let got = lines.lock().clone();
        // First line is the subject (margin + ✓ + hash + msg).
        assert!(got[0].contains("feat: x"), "subject missing: {:?}", got[0]);
        // Every body line's *content* fits within the text_width budget.
        // (The 2-col left margin is added on emit; right breathing room comes
        // from wrapping shorter, so the whole emitted line is content+margin.)
        for (i, line) in got[1..].iter().enumerate() {
            let content = line.trim_start();
            assert!(
                content.chars().count() <= 76,
                "body line {i} content exceeds 76 cols: {:?}",
                line
            );
        }
        // No content lost: all 40 `word` tokens survive the wrap.
        let joined = got[1..].join(" ");
        assert_eq!(joined.matches("word").count(), 40);
    }

    #[test]
    fn subject_not_wrapped() {
        // Tiny terminal: subject must still be exactly one line, overflow ok.
        let lines = Arc::new(Mutex::new(Vec::new()));
        let d = Display::with_cols(
            Buf {
                colors: false,
                lines: lines.clone(),
            },
            40,
        );
        let long_subject = "feat: a very long subject that definitely overflows the tiny width";
        d.commit_line("h000002", long_subject, None, "");
        let got = lines.lock().clone();
        assert_eq!(got.len(), 1, "subject must be exactly one line: {got:?}");
        assert!(got[0].ends_with(long_subject));
    }

    #[test]
    fn left_margin_present() {
        let lines = Arc::new(Mutex::new(Vec::new()));
        let d = Display::with_cols(
            Buf {
                colors: false,
                lines: lines.clone(),
            },
            80,
        );
        d.commit_line("h000003", "feat: x", Some("body text here"), "");
        let got = lines.lock().clone();
        assert!(!got.is_empty(), "expected output");
        for (i, line) in got.iter().enumerate() {
            assert!(
                line.starts_with("  "),
                "line {i} missing 2-col left margin: {:?}",
                line
            );
        }
    }

    #[test]
    fn non_tty_fallback_width() {
        // cols=0 simulates unknown / non-TTY: text_width must fall back to
        // FALLBACK_COLS (80) → wrap budget 76.
        let lines = Arc::new(Mutex::new(Vec::new()));
        let d = Display::with_cols(
            Buf {
                colors: false,
                lines: lines.clone(),
            },
            0,
        );
        let long = "w ".repeat(50); // 50 tokens, well over 76 cols
        d.commit_line("h000004", "feat: x", Some(&long), "");
        let got = lines.lock().clone();
        for (i, line) in got[1..].iter().enumerate() {
            let content = line.trim_start();
            assert!(
                content.chars().count() <= 76,
                "fallback body line {i} content exceeded 76: {:?}",
                line
            );
        }
        assert_eq!(got[1..].join(" ").matches('w').count(), 50);
    }

    #[test]
    fn sub_margin_terminal_emits_unwrapped_body() {
        // cols below the combined margins saturate text_width to 0; wrap_line
        // then returns the body as one piece instead of looping or panicking.
        let lines = Arc::new(Mutex::new(Vec::new()));
        let d = Display::with_cols(
            Buf {
                colors: false,
                lines: lines.clone(),
            },
            4,
        );
        d.commit_line("h000005", "feat: x", Some("supercalifragilistic token"), "");
        let got = lines.lock().clone();
        assert!(got[0].contains("feat: x"), "subject missing: {:?}", got[0]);
        assert_eq!(got.len(), 2, "body should be one unwrapped piece: {got:?}");
        assert!(got[1].ends_with("token"), "body tail lost: {:?}", got[1]);
    }

    #[test]
    fn wrap_line_hard_breaks_long_token() {
        // A single token longer than the width is hard-broken at the boundary.
        let pieces = wrap_line("abcdefghij", 4);
        assert_eq!(pieces, vec!["abcd", "efgh", "ij"]);
        // "short" (5 chars) > width 4 → also hard-broken.
        assert_eq!(wrap_line("short", 4), vec!["shor", "t"]);
        // Empty input yields one empty piece (blank line round-trips).
        assert_eq!(wrap_line("", 4), vec![""]);
    }

    #[test]
    fn wrap_line_handles_cjk_by_char_count() {
        // 6 CJK chars at width 4 → two lines of 4 and 2 (not byte-based wrap,
        // which would wrap every ~1.3 chars and corrupt output).
        let pieces = wrap_line("你好世界测试", 4);
        assert_eq!(pieces, vec!["你好世界", "测试"]);
    }
}
