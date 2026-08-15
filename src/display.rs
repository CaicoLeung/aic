//! The static commit panel: the [`Display`] core, its [`DisplayWrite`] sink
//! seam, and the commit-line engine (`commit_line`, `commit_preview`,
//! body/file-stats renderers). The resolve-flow UI is NOT here — it lives in
//! [`crate::conflict`] next to its domain, rendering through this module's
//! `styled`/`emit` primitives so both surfaces share one seam and one margin.

use console::{Style, Term};

use crate::commit_type::CommitType;
use crate::git::FileStats;
use crate::layout::{FALLBACK_COLS, MARGIN, resolve_cols, wrap_line};
use crate::palette;
use crate::palette::{commit_id_color, neutral_gray, sigma_color};

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

    /// Erase the last `n` emitted rows. Terminal sinks emit the ANSI erase
    /// sequence for the rows above the cursor (and end with the cursor at the
    /// top of the cleared region); in-memory sinks drop the lines from their
    /// buffer so `lines()` keeps reflecting the visible screen. `n == 0` is a
    /// no-op. Used to remove a confirmed or replaced commit preview so no
    /// draft residue stays next to the ✓ lines of landed commits.
    fn clear_last(&self, n: usize);

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

    fn clear_last(&self, n: usize) {
        // `clear_last_lines` erases the n rows above the cursor and parks the
        // cursor at the top of the cleared region — exactly "this preview is
        // gone, keep writing from here".
        if n > 0 {
            let _ = self.0.clear_last_lines(n);
        }
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

    /// Erase the last `n` emitted rows — removes a confirmed or replaced
    /// commit preview ([`Display::commit_preview`]) so the draft doesn't
    /// linger next to the ✓ lines of landed batches. `n == 0` (e.g.
    /// confirmation disabled) is a no-op.
    pub fn clear_last(&self, n: usize) {
        self.out.clear_last(n);
    }

    /// Effective text width for wrapped output: the shared width resolution
    /// ([`resolve_cols`] — non-TTY fallback + hard cap), minus both margins.
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
    ///
    /// `stats` render as the file-stats footer ([`Display::emit_file_stats`]) —
    /// the landed twin of the preview's footer, so the confirmed draft and the
    /// completed line show the same file information.
    pub fn commit_line(
        &self,
        hash: &str,
        message: &str,
        body: Option<&str>,
        prefix: &str,
        stats: &[FileStats],
    ) {
        // Muted gray for prefix/body/scope — read from the single-source
        // palette so it can't drift from the WCAG-guarded value.
        let gray = neutral_gray();

        // Main line: [prefix] ✓ <hash> <message>
        let check = self.styled("\u{2713}", Style::new().green().bold());
        // Commit ID: amber #d97706, bold so the short token qualifies as WCAG
        // AA Large (3:1) on both themes — the old #f3b340 read at ~1.9:1 on
        // white. Bold is also the right visual weight for a ref.
        let hash_styled = self.styled(hash, commit_id_color().bold());
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
        self.emit_file_stats(stats);
    }

    /// Style a conventional-commit subject line the same way in every
    /// renderer: typed `type` in its palette color, gray `(scope)`, bold
    /// description. Unknown/colon-less messages fall back to the full message
    /// in the type palette color. Shared by [`Display::commit_line`] (post-commit)
    /// and [`Display::commit_preview`] (pre-commit confirmation) so what the
    /// user confirms is byte-for-byte what the completed line will show.
    fn styled_subject(&self, message: &str) -> String {
        // Muted gray for the optional `(scope)` — matches the body/prefix tone.
        let gray = neutral_gray();
        let parsed = CommitType::parse_message(message);
        match parsed.description {
            Some(desc) => {
                // Type token: named palette color, or deterministic hash color
                // for unrecognized non-empty types, or neutral gray for empty.
                // Bolded so the short token qualifies as WCAG AA Large (3:1) on
                // both themes — the old bright type colors (feat #4ade80 etc.)
                // read at ~1.7:1 on white.
                let colored_type = self.styled(
                    parsed.type_name,
                    CommitType::color_for(parsed.type_name).bold(),
                );
                let scope = match parsed.scope {
                    Some(s) => self.styled(&format!("({s})"), gray.clone()),
                    None => String::new(),
                };
                let bold_desc = self.styled(desc, Style::new().bold());
                format!("{}{}: {}", colored_type, scope, bold_desc)
            }
            // No colon — no type token to color; render the whole message in
            // muted gray so it reads as a non-conventional fallback.
            None => self.styled(message, gray),
        }
    }

    /// Emit a commit body — margin + greedy word-wrap to text_width, gray.
    /// Blank body lines stay blank (no trailing-whitespace margin). Shared by
    /// [`Display::commit_line`] and [`Display::commit_preview`].
    fn emit_body(&self, body: &str) -> usize {
        // Muted gray, darkened from #8a8f9f to #6b7280 for light-bg readability.
        let gray = neutral_gray();
        let trimmed = body.trim();
        let mut rows = 0;
        if !trimmed.is_empty() {
            let width = self.text_width();
            for src_line in trimmed.lines() {
                if src_line.is_empty() {
                    self.emit_blank();
                    rows += 1;
                    continue;
                }
                for piece in wrap_line(src_line, width) {
                    self.emit(&self.styled(&piece, gray.clone()));
                    rows += 1;
                }
            }
        }
        rows
    }

    /// Files beyond this many are elided from the footer with a
    /// `… N more (N files)` line — the cap the old comma-joined preview used,
    /// kept so a huge batch can't blow the screen.
    const FILE_STATS_CAP: usize = 8;

    /// File-stats footer for a commit entry, shared by [`Display::commit_preview`]
    /// and [`Display::commit_line`] so what the user confirms is what the ✓
    /// line shows — rendered as an aligned grid (`git diff --stat` style):
    /// `+N` and `−M` each right-align in their own column, with the `Σ` glyph
    /// in a column of its own on the total row, so the totals land exactly
    /// under the per-file counts; filenames left-align in the next column,
    /// tags in the last. Green `+N`, red `−M`, muted filenames, a
    /// green-bold `[new]` / red-bold `[del]` tag, and a bold-cyan `Σ +X −Y
    /// total row when more than one file. Binary files render `(binary)`
    /// right-aligned in the counts region, which widens to fit the label when
    /// any shown file is binary; a binary file that is new or removed keeps
    /// its `[new]`/`[del]` tag.
    ///
    /// Glyph colors follow [`Display::review_section`]'s diff-line convention
    /// (`+` green, `-` red); filenames use [`neutral_gray`] like body text.
    /// Widths use the codebase's char-count model (same as [`wrap_line`]), so
    /// the grid holds exactly for ASCII paths and approximately for CJK ones.
    /// Column widths come from the shown files only. A filename wider than
    /// the grid is truncated with `…` — the one case the footer truncates,
    /// since a broken column defeats the grid; on a pathologically narrow
    /// terminal (`name_cap == 0`) the grid degrades to unpadded overflow,
    /// matching [`wrap_line`]'s `width == 0` convention. The
    /// [`FILE_STATS_CAP`] bounds height. Returns the rows emitted, for the
    /// preview's erase accounting.
    fn emit_file_stats(&self, stats: &[FileStats]) -> usize {
        if stats.is_empty() {
            return 0;
        }
        let gray = neutral_gray();
        let green = Style::new().green();
        let red = Style::new().red();
        let new_tag = Style::new().green().bold();
        let del_tag = Style::new().red().bold();
        let mut rows = 0;
        let shown = stats.len().min(Self::FILE_STATS_CAP);
        let shown_stats = &stats[..shown];
        let plus_len = |s: &FileStats| {
            if s.binary {
                0
            } else {
                format!("+{}", s.added).chars().count()
            }
        };
        let minus_len = |s: &FileStats| {
            if s.binary {
                0
            } else {
                format!("−{}", s.deleted).chars().count()
            }
        };

        // Grid geometry. Counts region: the Σ glyph has its own column —
        // 2 wide (`Σ `, blank on file rows) so it never touches the numbers.
        // Always reserved (even for a single-file commit) so counts align
        // across every commit's footer — a multi-file commit's file rows
        // have a blank where Σ would be, same as a single-file commit's one
        // row. Then `+N` and `−M` each right-align in their own column with a
        // 1-char gap, so the total row's numbers land exactly under the
        // per-file counts. Tag column exists only when a shown file carries
        // one (` [new]` / ` [del]` are both 6 chars). Name column: widest
        // shown name, capped so the row fits the resolved text width.
        let total_added: usize = stats.iter().map(|s| s.added).sum();
        let total_deleted: usize = stats.iter().map(|s| s.deleted).sum();
        let sigma_col = 2;
        // Size to the Σ total too, not just the per-file max: the total can
        // carry more digits than any single file (ten `+1` → `+10`), and an
        // all-binary diff totals `+0`/`−0` where every per-file width is 0 —
        // sizing to the total keeps the Σ row's numbers landing exactly under
        // the per-file counts in both cases.
        let plus_width = shown_stats
            .iter()
            .map(plus_len)
            .max()
            .unwrap_or(0)
            .max(format!("+{total_added}").chars().count());
        let minus_width = shown_stats
            .iter()
            .map(minus_len)
            .max()
            .unwrap_or(0)
            .max(format!("−{total_deleted}").chars().count());
        let sep = if plus_width > 0 && minus_width > 0 {
            1
        } else {
            0
        };
        // The counts region must also fit the `(binary)` label when any shown
        // file is binary — otherwise `(binary)` (8 chars) overflows a narrower
        // region and the binary rows drift out of line with the Σ total row.
        // The extra width becomes leading pad (`lead`) on the text rows and
        // the Σ row, so all three row kinds — text file, binary file, Σ total
        // — occupy the same `counts_region` and the filename column lands at
        // one column across every row.
        let binary_label = "(binary)".chars().count();
        let base_region = sigma_col + plus_width + sep + minus_width;
        let counts_region = if shown_stats.iter().any(|s| s.binary) {
            base_region.max(binary_label)
        } else {
            base_region
        };
        let lead = " ".repeat(counts_region - base_region);
        let tag_col = if shown_stats.iter().any(|s| s.new || s.removed) {
            6
        } else {
            0
        };
        let name_cap = self
            .text_width()
            .saturating_sub(counts_region + 2 + tag_col);
        let align = name_cap > 0;
        let name_width = if align {
            shown_stats
                .iter()
                .map(|s| s.path.chars().count())
                .max()
                .unwrap_or(0)
                .min(name_cap)
        } else {
            0
        };
        let sigma_blank = " ".repeat(sigma_col);
        let sep_str = if sep > 0 { " " } else { "" };
        // Shared `+N`/`−M` column formatter — file rows and the Σ total row
        // pad identically, so the alignment math has one home and the two
        // rows cannot drift apart.
        let fmt_columns = |plus: &str, minus: &str| {
            format!(
                "{}{}{}{}{}",
                " ".repeat(plus_width.saturating_sub(plus.chars().count())),
                self.styled(plus, green.clone()),
                sep_str,
                " ".repeat(minus_width.saturating_sub(minus.chars().count())),
                self.styled(minus, red.clone()),
            )
        };

        for s in shown_stats {
            // Counts region: Σ column (blank on file rows), then `+N` and
            // `−M` right-aligned to their own columns.
            let counts = if s.binary {
                let pad = counts_region - binary_label;
                format!(
                    "{}{}",
                    " ".repeat(pad),
                    self.styled("(binary)", gray.clone())
                )
            } else {
                let plus = format!("+{}", s.added);
                let minus = format!("−{}", s.deleted);
                format!("{lead}{sigma_blank}{}", fmt_columns(&plus, &minus))
            };
            // Name column: truncated with `…` when wider than the cap,
            // padded to the grid width otherwise.
            let mut name = s.path.clone();
            if align && name.chars().count() > name_width {
                let keep = name_width.saturating_sub(1);
                name = format!("{}…", name.chars().take(keep).collect::<String>());
            }
            let name_pad = name_width.saturating_sub(name.chars().count());
            let tag = if s.new {
                self.styled(" [new]", new_tag.clone())
            } else if s.removed {
                self.styled(" [del]", del_tag.clone())
            } else {
                String::new()
            };
            // trim_end drops the trailing name padding on rows without a tag —
            // plain spaces, so it is safe with ANSI styling enabled too.
            self.emit(
                format!(
                    "{counts}  {}{}{}",
                    self.styled(&name, gray.clone()),
                    " ".repeat(name_pad),
                    tag
                )
                .trim_end(),
            );
            rows += 1;
        }
        if stats.len() > Self::FILE_STATS_CAP {
            self.emit(&self.styled(
                &format!("… {} more ({} files)", stats.len() - shown, stats.len()),
                gray.clone(),
            ));
            rows += 1;
        }
        if stats.len() > 1 {
            let plus = format!("+{total_added}");
            let minus = format!("−{total_deleted}");
            // `Σ` sits in its own column (padded to the column width, like
            // the file rows' blank) in cyan-600 — harmonizing with the green
            // `+N` additions while staying distinct from the red `−M`; the
            // totals right-align into the same `+N` / `−M` columns above.
            let sigma_text = format!(
                "{}{}",
                self.styled("Σ", sigma_color().bold()),
                " ".repeat(sigma_col.saturating_sub(1)),
            );
            self.emit(&format!(
                "{lead}{}{}  {}",
                sigma_text,
                fmt_columns(&plus, &minus),
                self.styled(&format!("({} files)", stats.len()), gray.clone()),
            ));
            rows += 1;
        }
        rows
    }

    /// Pre-commit confirmation preview (issue #78): the exact message that
    /// would be committed, framed as *pending* so it can't be mistaken for the
    /// ✓ lines of already-landed batches — a yellow `?` marker on the header
    /// and subject (the subject keeps its conventional-commit coloring, so the
    /// draft previews the exact styling the ✓ line will use), gray body, and
    /// the file-stats footer ([`Display::emit_file_stats`]).
    ///
    /// Returns how many rows the preview occupies, so the caller can erase it
    /// with [`Display::clear_last`] once the draft is confirmed or replaced —
    /// a confirmed draft never lingers on screen.
    pub fn commit_preview(&self, message: &str, body: Option<&str>, stats: &[FileStats]) -> usize {
        let pending = Style::new().yellow().bold();
        self.emit(&format!(
            "{} {}",
            self.styled("?", pending.clone()),
            self.styled("proposed commit:", pending.clone())
        ));
        self.emit(&format!(
            "{} {}",
            self.styled("?", pending.clone()),
            self.styled_subject(message)
        ));
        let mut rows = 2;
        if let Some(b) = body {
            rows += self.emit_body(b);
        }
        rows += self.emit_file_stats(stats);
        self.emit_blank();
        rows + 1
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
// Layout: margins specific to the static panel engine
// ------------------------------------------------------------------

/// Left inset (columns) for the commit-line block. Replaces the body's old
/// ad-hoc `  ` indent so subject and body share one uniform margin. The
/// matching prefix string is the shared [`MARGIN`] (now in
/// [`crate::layout`]); the column count stays here because only the panel
/// engine's `text_width` subtracts it.
const LEFT_MARGIN: usize = 2;

/// Right inset (columns) of breathing room, achieved by wrapping shorter — no
/// trailing spaces are ever printed (they break copy-paste and some terminals
/// strip them).
const RIGHT_MARGIN: usize = 2;

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
        fn clear_last(&self, n: usize) {
            let mut lines = self.lines.lock();
            let keep = lines.len().saturating_sub(n);
            lines.truncate(keep);
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
        let rows = d.commit_preview(
            "feat(auth): add OAuth2 login support",
            Some("Allow users to sign in via Google and GitHub OAuth2 providers"),
            &[
                FileStats {
                    path: "src/auth.rs".into(),
                    added: 12,
                    deleted: 3,
                    new: true,
                    removed: false,
                    binary: false,
                },
                FileStats {
                    path: "src/main.rs".into(),
                    added: 4,
                    deleted: 1,
                    new: false,
                    removed: false,
                    binary: false,
                },
            ],
        );
        let got = lines.lock().clone();
        // Pending header + subject carry the `?` marker; body sits at the
        // shared margin; the file-stats footer is aligned with it — counts
        // first, then filename, `[new]`/`[del]` tag, and a Σ total; a trailing
        // blank separates the preview from the confirmation menu. `rows` is
        // the whole block, so the caller can erase it after the draft is
        // confirmed.
        assert_eq!(got[0], "  ? proposed commit:");
        assert_eq!(got[1], "  ? feat(auth): add OAuth2 login support");
        assert_eq!(
            got[2],
            "  Allow users to sign in via Google and GitHub OAuth2 providers"
        );
        // File rows carry a blank Σ column (`Σ ` wide); +N and −M each
        // right-align in their own column (" +4" carries the pad). The Σ
        // row's +16/−4 end exactly where +12/−3 and +4/−1 end, and the Σ
        // glyph sits in the same column as the file rows' blank.
        assert_eq!(got[3], "    +12 −3  src/auth.rs [new]");
        assert_eq!(got[4], "     +4 −1  src/main.rs");
        assert_eq!(got[5], "  Σ +16 −4  (2 files)");
        assert_eq!(got[6], "");
        assert_eq!(rows, 7, "header + subject + body + 2 files + total + blank");
    }

    #[test]
    fn commit_preview_singleton_file_list_omits_count() {
        let lines = Arc::new(Mutex::new(Vec::new()));
        let d = Display::with(Buf {
            colors: false,
            lines: lines.clone(),
        });
        let rows = d.commit_preview(
            "chore: bump dep",
            None,
            &[FileStats {
                path: "Cargo.toml".into(),
                added: 5,
                deleted: 2,
                new: false,
                removed: false,
                binary: false,
            }],
        );
        let got = lines.lock().clone();
        assert_eq!(got[0], "  ? proposed commit:");
        assert_eq!(got[1], "  ? chore: bump dep");
        // Single file: no Σ total line; no body line emitted.
        assert_eq!(got[2], "    +5 −2  Cargo.toml");
        assert_eq!(got.len(), 4, "no body line expected, got: {got:?}");
        assert_eq!(rows, 4, "header + subject + file + blank");
    }

    /// `clear_last` drops the most recent rows from the buffer (the in-memory
    /// analogue of erasing a preview on a real terminal), and `0` is a no-op —
    /// so the confirmed-draft erase never touches earlier commit lines.
    #[test]
    fn clear_last_erases_only_the_most_recent_rows() {
        let lines = Arc::new(Mutex::new(Vec::new()));
        let d = Display::with(Buf {
            colors: false,
            lines: lines.clone(),
        });
        d.emit("keep me");
        d.emit("preview line 1");
        d.emit("preview line 2");
        d.emit("preview line 3");
        d.clear_last(3);
        assert_eq!(lines.lock().clone(), vec!["  keep me".to_string()]);

        // n == 0 is a no-op.
        d.clear_last(0);
        assert_eq!(lines.lock().clone(), vec!["  keep me".to_string()]);

        // n larger than the buffer just empties it (no panic).
        d.clear_last(99);
        assert!(lines.lock().is_empty());
    }

    /// A batch with more than 8 files keeps the preview line bounded: the
    /// first 8 are named, the rest are summarized.
    #[test]
    fn commit_preview_truncates_long_file_lists() {
        let lines = Arc::new(Mutex::new(Vec::new()));
        let d = Display::with(Buf {
            colors: false,
            lines: lines.clone(),
        });
        let stats: Vec<FileStats> = (1..=10)
            .map(|i| FileStats {
                path: format!("src/f{i}.rs"),
                added: 1,
                deleted: 0,
                new: false,
                removed: false,
                binary: false,
            })
            .collect();
        let rows = d.commit_preview("feat: big", None, &stats);
        let got = lines.lock().clone();
        assert!(
            got.iter().any(|l| l.contains("… 2 more (10 files)")),
            "expected a truncated file list, got: {got:?}"
        );
        assert!(
            got.iter().any(|l| l.contains("Σ +10 −0  (10 files)")),
            "expected a total over all files, got: {got:?}"
        );
        assert!(
            got.iter().any(|l| l.contains("src/f8.rs")),
            "the 8th file must be shown, got: {got:?}"
        );
        assert!(
            !got.iter().any(|l| l.contains("src/f9.rs")),
            "the 9th file must be elided, got: {got:?}"
        );
        assert_eq!(
            rows, 13,
            "header + subject + 8 files + elision + total + blank"
        );
    }

    /// The footer's edge rendering: binary files show `(binary)` instead of
    /// counts and keep their `[new]`/`[del]` tag; deleted files carry `[del]`,
    /// and the Σ total sums across all entries.
    #[test]
    fn file_stats_footer_marks_binary_and_deleted_files() {
        let lines = Arc::new(Mutex::new(Vec::new()));
        let d = Display::with(Buf {
            colors: false,
            lines: lines.clone(),
        });
        let rows = d.emit_file_stats(&[
            FileStats {
                path: "img.png".into(),
                added: 0,
                deleted: 0,
                new: true,
                removed: false,
                binary: true,
            },
            FileStats {
                path: "src/old.rs".into(),
                added: 0,
                deleted: 12,
                new: false,
                removed: true,
                binary: false,
            },
        ]);
        let got = lines.lock().clone();
        // A new binary file keeps its `[new]` tag (the binary label replaces
        // the counts, not the tag). "(binary)" spans the counts region; the
        // name pads to align with src/old.rs (10 chars).
        assert_eq!(got[0], "  (binary)  img.png    [new]");
        assert_eq!(got[1], "    +0 −12  src/old.rs [del]");
        assert_eq!(got[2], "  Σ +0 −12  (2 files)");
        assert_eq!(rows, 3, "2 files + total");
    }

    /// The Σ total can carry more digits than any single file (two `+5` files
    /// total `+10`); the column must size to the total so `+10` lands exactly
    /// under each `+5` rather than overflowing into the gap. Regression for
    /// the per-file-max-only column width.
    #[test]
    fn file_stats_footer_aligns_total_wider_than_per_file_counts() {
        let lines = Arc::new(Mutex::new(Vec::new()));
        let d = Display::with(Buf {
            colors: false,
            lines: lines.clone(),
        });
        let rows = d.emit_file_stats(&[
            FileStats {
                path: "a.rs".into(),
                added: 5,
                deleted: 0,
                new: false,
                removed: false,
                binary: false,
            },
            FileStats {
                path: "b.rs".into(),
                added: 5,
                deleted: 0,
                new: false,
                removed: false,
                binary: false,
            },
        ]);
        let got = lines.lock().clone();
        // `+5` right-aligns in a 3-wide column (sized to `+10`), so its `5`
        // sits under the total's `0` of `+10`; both end at the same column.
        assert_eq!(got[0], "     +5 −0  a.rs");
        assert_eq!(got[1], "     +5 −0  b.rs");
        assert_eq!(got[2], "  Σ +10 −0  (2 files)");
        assert_eq!(rows, 3, "2 files + total");
    }

    /// An all-binary diff has no per-file counts (every width is 0), yet the
    /// Σ row still renders a stable, gapped `+0 −0` — not a jammed `+0−0`.
    /// Regression for the all-binary column collapse.
    #[test]
    fn file_stats_footer_stable_columns_when_all_files_binary() {
        let lines = Arc::new(Mutex::new(Vec::new()));
        let d = Display::with(Buf {
            colors: false,
            lines: lines.clone(),
        });
        let rows = d.emit_file_stats(&[
            FileStats {
                path: "img.png".into(),
                added: 0,
                deleted: 0,
                new: true,
                removed: false,
                binary: true,
            },
            FileStats {
                path: "data.bin".into(),
                added: 0,
                deleted: 0,
                new: false,
                removed: false,
                binary: true,
            },
        ]);
        let got = lines.lock().clone();
        // New binary keeps `[new]`; non-new binary carries no tag. The counts
        // region widens to fit `(binary)` (8 > the `+0`/`−0` base region of 7),
        // so the Σ row gains a leading pad and its `(2 files)` label lands in
        // the same column as the filenames above.
        assert_eq!(got[0], "  (binary)  img.png  [new]");
        assert_eq!(got[1], "  (binary)  data.bin");
        assert_eq!(got[2], "   Σ +0 −0  (2 files)");
        assert_eq!(rows, 3, "2 files + total");
    }

    /// A binary file alongside a text file whose counts region is narrower
    /// than `(binary)`: the region widens to 8 and every row — text, binary,
    /// Σ — carries the same leading pad, so `(binary)`'s right edge, the text
    /// `−M`, and the filename column all line up. Regression for the
    /// binary-overflow column drift between file rows and the Σ row.
    #[test]
    fn file_stats_footer_mixed_binary_keeps_columns_aligned() {
        let lines = Arc::new(Mutex::new(Vec::new()));
        let d = Display::with(Buf {
            colors: false,
            lines: lines.clone(),
        });
        let rows = d.emit_file_stats(&[
            FileStats {
                path: "x.bin".into(),
                added: 0,
                deleted: 0,
                new: false,
                removed: false,
                binary: true,
            },
            FileStats {
                path: "a.rs".into(),
                added: 1,
                deleted: 0,
                new: false,
                removed: false,
                binary: false,
            },
        ]);
        let got = lines.lock().clone();
        // base region (Σ 2 + `+1` 2 + gap 1 + `−0` 2 = 7) widens to 8 for
        // `(binary)`; the text row and Σ row each carry one leading pad, so
        // all three rows' counts end at the same column and the filenames
        // start at the same column.
        assert_eq!(got[0], "  (binary)  x.bin");
        assert_eq!(got[1], "     +1 −0  a.rs");
        assert_eq!(got[2], "   Σ +1 −0  (2 files)");
        assert_eq!(rows, 3, "2 files + total");
    }

    /// A filename wider than the grid's name column is truncated with `…`
    /// (char-count model) so the columns — and the tag column — stay intact:
    /// the short file's name pads to the same width and its `[new]` tag lands
    /// at the same column as a tag on the truncated row would.
    #[test]
    fn file_stats_footer_truncates_long_names_to_keep_the_grid() {
        let lines = Arc::new(Mutex::new(Vec::new()));
        let d = Display::with(Buf {
            colors: false,
            lines: lines.clone(),
        });
        let long = "x".repeat(70);
        let rows = d.emit_file_stats(&[
            FileStats {
                path: long.clone(),
                added: 1,
                deleted: 0,
                new: false,
                removed: false,
                binary: false,
            },
            FileStats {
                path: "a.rs".into(),
                added: 1,
                deleted: 0,
                new: true,
                removed: false,
                binary: false,
            },
        ]);
        let got = lines.lock().clone();
        // text_width is 76 (80 - 2 - 2); counts region = Σ (2) + `+1` (2) +
        // gap (1) + `−0` (2) = 7; name column = 76 - 7 (counts) - 2 (gap)
        // - 6 (tag column) = 61 → 60 chars + "…". File rows carry a blank
        // Σ column.
        assert_eq!(got[0], format!("    +1 −0  {}", "x".repeat(60) + "…"));
        assert_eq!(got[1], format!("    +1 −0  a.rs{} [new]", " ".repeat(57)));
        assert_eq!(rows, 3, "2 files + total");
    }

    /// The landed line shows the same footer as the preview — the file stats
    /// survive the commit.
    #[test]
    fn commit_line_renders_file_stats_footer() {
        let lines = Arc::new(Mutex::new(Vec::new()));
        let d = Display::with(Buf {
            colors: false,
            lines: lines.clone(),
        });
        d.commit_line(
            "abc1234",
            "feat: add thing",
            None,
            "[1/3]",
            &[FileStats {
                path: "src/auth.rs".into(),
                added: 7,
                deleted: 2,
                new: true,
                removed: false,
                binary: false,
            }],
        );
        let got = lines.lock().clone();
        assert!(
            got.iter().any(|l| l.contains("+7 −2  src/auth.rs [new]")),
            "committed line must show the footer, got: {got:?}"
        );
        assert!(
            !got.iter().any(|l| l.contains("Σ")),
            "single file must not get a total line, got: {got:?}"
        );
    }

    /// The preview path's Σ row is covered above; the landed ✓ line must show
    /// the same total row for a multi-file commit — `commit_line` and
    /// `commit_preview` share `emit_file_stats`, but this pins the contract on
    /// the landed entry so a regression that drops the Σ row only post-commit
    /// (e.g. a guard misplaced between the two callers) fails here.
    #[test]
    fn commit_line_renders_sigma_row_for_multiple_files() {
        let lines = Arc::new(Mutex::new(Vec::new()));
        let d = Display::with(Buf {
            colors: false,
            lines: lines.clone(),
        });
        d.commit_line(
            "abc1234",
            "feat: add thing",
            None,
            "[1/2]",
            &[
                FileStats {
                    path: "src/a.rs".into(),
                    added: 3,
                    deleted: 1,
                    new: false,
                    removed: false,
                    binary: false,
                },
                FileStats {
                    path: "src/b.rs".into(),
                    added: 5,
                    deleted: 0,
                    new: true,
                    removed: false,
                    binary: false,
                },
            ],
        );
        let got = lines.lock().clone();
        assert!(
            got.iter().any(|l| {
                l.contains("Σ") && l.contains("+8") && l.contains("−1") && l.contains("(2 files)")
            }),
            "multi-file landed commit must show the Σ total row, got: {got:?}"
        );
    }

    #[test]
    fn plain_when_colors_disabled() {
        let lines = Arc::new(Mutex::new(Vec::new()));
        let d = Display::with(Buf {
            colors: false,
            lines: lines.clone(),
        });
        d.commit_line(
            "abc1234",
            "feat: add thing",
            Some("body line"),
            "[1/3]",
            &[],
        );
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
        d.commit_line(
            "abc1234",
            "feat: add thing",
            Some("body line"),
            "[1/3]",
            &[],
        );
        let joined = lines.lock().join("\n");
        // hash #d97706 (bold), feat type green #15803d (bold), description bold
        // default fg, body + prefix muted gray #6b7280.
        assert!(
            joined.contains("217;119;6"),
            "hash amber color missing: {joined:?}"
        );
        assert!(
            joined.contains("21;128;61"),
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
            joined.contains("107;114;128"),
            "muted gray color missing: {joined:?}"
        );
        // [n/m] prefix text survives styling (format kept, not "n.").
        assert!(joined.contains("[1/3]"), "prefix text missing: {joined:?}");
    }

    #[test]
    fn fix_type_gets_orange_color() {
        let _env = COLOR_ENV.lock();
        let _guard = ColorGuard::force();
        let lines = Arc::new(Mutex::new(Vec::new()));
        let d = Display::with(Buf {
            colors: true,
            lines: lines.clone(),
        });
        d.commit_line("def5678", "fix(auth): correct token check", None, "", &[]);
        let joined = lines.lock().join("\n");
        // fix type should be orange #ea580c (re-toned from #fbbf24 for white-bg
        // readability — see types::NAMED_PALETTE).
        assert!(
            joined.contains("234;88;12"),
            "fix type orange color missing: {joined:?}"
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
        d.commit_line("def5678", "fix(auth): correct token check", None, "", &[]);
        let got = lines.lock().clone();
        // Exact visible text — catches the dropped-paren regression directly.
        // Subject carries a 2-col left margin.
        assert_eq!(got[0], "  \u{2713} def5678 fix(auth): correct token check");
    }

    /// Spot-check that `styled_subject` wires `CommitType::color_for` through
    /// to the rendered bytes for a representative spread: a re-toned original
    /// type (feat), a new type (ci), the promoted-from-gray chore, and a
    /// named neutral (wip). The exhaustive palette + WCAG 3:1 guard lives in
    /// `types::tests::all_colors_pass_wcag_aa_large_on_both_themes` — this
    /// test only pins the display wiring.
    #[test]
    fn each_type_renders_its_palette_color() {
        let _env = COLOR_ENV.lock();
        let _guard = ColorGuard::force();
        for (type_str, rgb) in [
            ("feat", "21;128;61"),
            ("ci", "99;102;241"),
            ("chore", "15;118;110"),
            ("wip", "100;116;139"),
        ] {
            let lines = Arc::new(Mutex::new(Vec::new()));
            let d = Display::with(Buf {
                colors: true,
                lines: lines.clone(),
            });
            d.commit_line("hash000", &format!("{type_str}: msg"), None, "", &[]);
            let joined = lines.lock().join("\n");
            assert!(
                joined.contains(rgb),
                "{type_str} should render color {rgb}: {joined:?}"
            );
        }
    }

    /// Unrecognized (non-empty) type tokens go through the deterministic hash
    /// fallback — they must render a fallback-palette color, NOT the neutral
    /// gray (the old behavior collapsed everything unmatched to gray, which
    /// read as "uncolored"). Stability/distribution is pinned in `types::tests`.
    #[test]
    fn unrecognized_type_gets_hash_fallback_color() {
        let _env = COLOR_ENV.lock();
        let _guard = ColorGuard::force();
        let lines = Arc::new(Mutex::new(Vec::new()));
        let d = Display::with(Buf {
            colors: true,
            lines: lines.clone(),
        });
        d.commit_line("ghi9012", "blob: thing in progress", None, "", &[]);
        let joined = lines.lock().join("\n");
        assert!(
            !joined.contains("107;114;128"),
            "unrecognized type must not collapse to neutral gray: {joined:?}"
        );
        // It must be one of the six fallback-palette RGBs.
        let hits_fallback = [
            (13, 148, 136),
            (147, 51, 234),
            (194, 65, 12),
            (14, 116, 144),
            (168, 85, 247),
            (59, 130, 246),
        ]
        .iter()
        .any(|(r, g, b)| joined.contains(&format!("{r};{g};{b}")));
        assert!(
            hits_fallback,
            "unrecognized should hit fallback palette: {joined:?}"
        );
    }

    #[test]
    fn no_colon_message_gets_muted_gray() {
        let _env = COLOR_ENV.lock();
        let _guard = ColorGuard::force();
        let lines = Arc::new(Mutex::new(Vec::new()));
        let d = Display::with(Buf {
            colors: true,
            lines: lines.clone(),
        });
        d.commit_line("jkl3456", "no colon message", None, "", &[]);
        let joined = lines.lock().join("\n");
        // No-colon messages have no type token to color → muted gray #6b7280
        // (darkened from the old #9ca3af for white-bg readability).
        assert!(
            joined.contains("107;114;128"),
            "no-colon message should be muted gray: {joined:?}"
        );
    }
}
