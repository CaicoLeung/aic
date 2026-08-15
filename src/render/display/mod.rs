//! The static commit panel: the [`Display`] core, its [`DisplayWrite`] sink
//! seam, and the commit-line engine (`commit_line`, `commit_preview`,
//! body/file-stats renderers). The resolve-flow UI is NOT here — it lives in
//! [`crate::git::conflict`] next to its domain, rendering through this module's
//! `styled`/`emit` primitives so both surfaces share one seam and one margin.

use console::{Style, Term};

use crate::git::FileStats;
use crate::render::commit_type::CommitType;
use crate::render::layout::{FALLBACK_COLS, MARGIN, resolve_cols, wrap_line};
use crate::render::palette;
use crate::render::palette::{commit_id_color, neutral_gray, sigma_color};

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
    /// are disabled (piped output, NO_COLOR, non-TTY). `pub(crate)` because
    /// the resolve-flow UI in `conflict` renders through these same
    /// primitives — one seam, no parallel styling path.
    pub(crate) fn styled(&self, text: &str, s: Style) -> String {
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
    pub(crate) fn emit(&self, line: &str) {
        self.out.write_line(&format!("{MARGIN}{line}"));
    }

    /// Blank separator line — written bare (no margin) to avoid trailing
    /// whitespace.
    pub(crate) fn emit_blank(&self) {
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
        let check = self.styled("\u{2713}", palette::success());
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
                let bold_desc = self.styled(desc, palette::emphasis());
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
    /// `… N more` line — the file count lives in the Σ row, so it is not
    /// repeated here; the cap is the one the old comma-joined preview used,
    /// kept so a huge batch can't blow the screen.
    const FILE_STATS_CAP: usize = 8;

    /// File-stats footer for a commit entry, shared by [`Display::commit_preview`]
    /// and [`Display::commit_line`] so what the user confirms is what the ✓
    /// line shows — rendered as an aligned grid (`git diff --stat` style):
    /// `+N` and `−M` each right-align in their own column, with the `Σ` glyph
    /// in a column of its own on the total row, so the totals land exactly
    /// under the per-file counts; filenames left-align in the next column,
    /// tags in the last. A zero per-file count renders as a blank column
    /// (`git diff --stat` never shows zeros); the Σ total row keeps both
    /// totals even at zero, like git's own summary line. Green `+N`, red
    /// `−M`, muted filenames, a green-bold `[new]` / red-bold `[del]` tag,
    /// and a bold-cyan `Σ +X −Y` total row when more than one file. Binary
    /// files render `(binary)` right-aligned in the counts region, which
    /// widens to fit the label when any shown file is binary; a binary file
    /// that is new or removed keeps its `[new]`/`[del]` tag.
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
        let green = palette::added();
        let red = palette::removed();
        let new_tag = palette::added_strong();
        let del_tag = palette::removed_strong();
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
        // rows cannot drift apart. Empty strings (a suppressed zero column)
        // skip styling: `styled("")` would wrap nothing in ANSI escapes and
        // embed invisible escape pairs mid-line.
        let fmt_columns = |plus: &str, minus: &str| {
            let paint = |text: &str, s: Style| {
                if text.is_empty() {
                    String::new()
                } else {
                    self.styled(text, s)
                }
            };
            format!(
                "{}{}{}{}{}",
                " ".repeat(plus_width.saturating_sub(plus.chars().count())),
                paint(plus, green.clone()),
                sep_str,
                " ".repeat(minus_width.saturating_sub(minus.chars().count())),
                paint(minus, red.clone()),
            )
        };

        for s in shown_stats {
            // Counts region: Σ column (blank on file rows), then `+N` and
            // `−M` right-aligned to their own columns. A zero count renders
            // as an empty string — `git diff --stat` never shows zeros, and
            // the column pad keeps the grid aligned for the rows that do
            // carry the count. The Σ total row below keeps both totals even
            // at zero (git's own summary line does), and the width math
            // already sizes to the totals, so geometry is unchanged.
            let counts = if s.binary {
                let pad = counts_region - binary_label;
                format!(
                    "{}{}",
                    " ".repeat(pad),
                    self.styled("(binary)", gray.clone())
                )
            } else {
                let plus = if s.added > 0 {
                    format!("+{}", s.added)
                } else {
                    String::new()
                };
                let minus = if s.deleted > 0 {
                    format!("−{}", s.deleted)
                } else {
                    String::new()
                };
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
            // The Σ total row below already carries the `(N files)` count —
            // repeating it here was noise; the elision line names only how
            // many rows were cut.
            self.emit(&self.styled(&format!("… {} more", stats.len() - shown), gray.clone()));
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
        let pending = palette::pending();
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

    /// `aic` with nothing staged and nothing unstaged — no work for the LLM.
    pub fn nothing_to_commit(&self) {
        self.emit(&self.styled("nothing to commit — working tree clean", palette::muted()));
    }

    /// Generic warning line, routed through the shared margin so ad-hoc
    /// status failures stay visually consistent with the rest of the run's
    /// output instead of being dumped flush to the edge via raw `eprintln!`.
    pub fn warn(&self, msg: &str) {
        self.emit(&format!(
            "{} {msg}",
            self.styled("\u{26A0}", palette::pending())
        ));
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
/// [`crate::render::layout`]); the column count stays here because only the panel
/// engine's `text_width` subtracts it.
const LEFT_MARGIN: usize = 2;

/// Right inset (columns) of breathing room, achieved by wrapping shorter — no
/// trailing spaces are ever printed (they break copy-paste and some terminals
/// strip them).
const RIGHT_MARGIN: usize = 2;

#[cfg(test)]
mod tests;
