//! The static commit panel: the [`Display`] core, its [`DisplayWrite`] sink
//! seam, and the commit-line engine (`commit_line`, `commit_preview`,
//! body/file-stats renderers). The resolve-flow UI is NOT here — it lives in
//! [`crate::git::conflict`] next to its domain, rendering through this module's
//! `styled`/`emit` primitives so both surfaces share one seam and one margin.

use std::sync::Arc;

use console::{Style, Term};

use crate::git::FileStats;
use crate::render::commit_type::CommitType;
use crate::render::layout::{FALLBACK_COLS, MARGIN, clamp_to_width, resolve_cols, wrap_line};
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
    out: Arc<dyn DisplayWrite>,
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

    /// Injectable constructor with an explicit terminal column count. Used
    /// by tests to drive wrap behavior deterministically without probing the
    /// real terminal, and by the timeline driver as the frame-capture
    /// constructor (its rows must wrap to the frame's width, not the probed
    /// terminal's); prod keeps [`Display::new`]. `cols == 0` is treated as
    /// "unknown" and resolved to [`FALLBACK_COLS`] inside [`Display::text_width`].
    pub(crate) fn with_cols(out: impl DisplayWrite + 'static, cols: usize) -> Self {
        let colors = out.colors_enabled();
        Self {
            out: Arc::new(out),
            colors,
            cols,
        }
    }

    /// A second [`Display`] sharing this one's sink — same bytes, same
    /// colors, same width. The Run timeline driver (ADR 0018) hands these
    /// out so step-scoped output (commit entries, previews, warnings) lands
    /// in the same place as the run's own lines, whether the sink is the
    /// real stderr or a test buffer.
    pub(crate) fn share(&self) -> Self {
        Self {
            out: Arc::clone(&self.out),
            colors: self.colors,
            cols: self.cols,
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
    /// panics, it just emits the body unwrapped. `pub(crate)` because the
    /// Batch Review listing truncates its rows to this same budget — one
    /// width source keeps every surface's terminal edge in agreement.
    pub(crate) fn text_width(&self) -> usize {
        resolve_cols(self.cols).saturating_sub(LEFT_MARGIN + RIGHT_MARGIN)
    }

    // ------------------------------------------------------------------
    // Public rendering entry points
    // ------------------------------------------------------------------

    /// Commit entry table row (ADR 0018 redesign) — one row per landed
    /// batch, flushed to permanent scrollback below the Run timeline frame.
    ///
    /// Layout: the subject line is a single full-width title row —
    /// `✓ <hash> <styled subject>` — never re-flowed, clamped with `…`
    /// at the terminal's edge, carrying no delta: the footer's per-file
    /// counts and Σ row own the numbers, `git log` owns the prose; the
    /// pre-commit preview ([`Self::commit_preview`]) still shows the body
    /// in full before the commit lands. The clamp keeps the row one
    /// physical terminal line — the flush printer's cursor arithmetic
    /// assumes one row per emitted row (subjects are prompt-capped at 72
    /// chars but nothing validates that). The file-stats footer hangs
    /// shallower — file rows under the commit ID, the Σ total under the
    /// `✓` marker — so the footer reads as keyed to the ref line.
    /// `stats` render on the [`Self::text_width`] budget: the same prose
    /// boundary every other Run surface wraps at, so the Σ Diff bar's
    /// right edge is where text ends, not the terminal's real edge.
    /// `floor` is the Run-wide counts geometry ([`counts_geometry`] over
    /// the Run's whole diff) so every entry of a multi-batch Run shares
    /// one counts grid — batch-local digit counts can't drift the
    /// columns. `pub(crate)`: callers are the Run flow and its tests.
    pub(crate) fn commit_line(
        &self,
        hash: &str,
        message: &str,
        stats: &[FileStats],
        floor: CountsGeometry,
    ) {
        // Main line: ✓ <hash> <message> — one full-width table row per
        // landed batch (ADR 0018 redesign). Commit ID: amber #d97706, bold
        // so the short token qualifies as WCAG AA Large (3:1) on both
        // themes.
        let check = self.styled("\u{2713}", palette::success());
        let hash_styled = self.styled(hash, commit_id_color().bold());
        // The subject clamps to the row's remaining effective columns
        // (margin + `✓ ` + hash + space already spent): the entry is a
        // permanent terminal line and must never wrap to two physical
        // rows.
        let budget =
            resolve_cols(self.cols).saturating_sub(LEFT_MARGIN + 2 + hash.chars().count() + 1);
        self.emit(&format!(
            "{check} {hash_styled} {}",
            self.styled_subject(&clamp_to_width(message, budget))
        ));

        // File rows + Σ anchor at the hash with names pinned to the message
        // start, on the shared text budget.
        let subject = Self::subject_offsets(hash);
        self.emit_file_stats(stats, &subject, self.text_width(), floor);
    }

    /// Subject-prefix geometry, as [`SubjectOffsets`]: the marker sits at
    /// column 0, the hash at 2, the message at `2 + hash + 1`. Every entry
    /// of a Run shares it — no per-batch prefix — so all rows hang on the
    /// same columns. Pure, so the geometry is unit-testable without a sink.
    fn subject_offsets(hash: &str) -> SubjectOffsets {
        SubjectOffsets {
            tick: 0,
            hash: 2,
            message: 2 + hash.chars().count() + 1,
        }
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

    /// Emit a commit body — hang-indented by `indent` columns beyond the
    /// shared margin, greedy word-wrap to `text_width − indent` (saturating;
    /// a pathologically deep indent on a tiny terminal degrades to
    /// [`wrap_line`]'s `width == 0` "don't wrap" convention instead of
    /// panicking), gray. Blank body lines stay bare blanks (no
    /// trailing-whitespace indent). Shared by [`Display::commit_line`] and
    /// [`Display::commit_preview`].
    fn emit_body(&self, body: &str, indent: usize) -> usize {
        // Muted gray, darkened from #8a8f9f to #6b7280 for light-bg readability.
        let gray = neutral_gray();
        let trimmed = body.trim();
        let mut rows = 0;
        if !trimmed.is_empty() {
            let pad = " ".repeat(indent);
            let width = self.text_width().saturating_sub(indent);
            for src_line in trimmed.lines() {
                if src_line.is_empty() {
                    self.emit_blank();
                    rows += 1;
                    continue;
                }
                for piece in wrap_line(src_line, width) {
                    self.emit(&format!("{pad}{}", self.styled(&piece, gray.clone())));
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
    /// under the per-file counts; filenames left-align at `name_col` —
    /// pinned under the subject's message text (`subject.message`) so the footer
    /// reads as keyed to the commit message — with the same gap on every
    /// row. The counts block anchors at the hash column (`subject.hash`) and its
    /// right edge is capped 2 short of the message start: a block wider than
    /// the hash→message span (4-digit Σ totals) grows leftward instead of
    /// pushing names off the message, clamped 2 columns left of the hash —
    /// exactly the ✓ marker's column, where the Σ glyph itself sits (`Σ `
    /// wide, hugging the block's left edge). A zero per-file count renders
    /// as a blank column (`git diff --stat` never shows zeros); the Σ total
    /// row keeps both totals even at zero, like git's own summary line. Green `+N`, red
    /// `−M`, muted directories with emphasized basenames, a green-bold `[new]` / red-bold `[del]` tag,
    /// and a bold-cyan `Σ +X −Y <bar>` total row when more than one
    /// file (Commit entry v2, ADR 0018: the Diff bar — `█` addition / `░`
    /// deletion cells over text line counts only, shrinking then omitted on
    /// narrow terminals). Binary files render `(binary) <byte-delta>`
    /// right-aligned in the counts region, which widens to fit the widest
    /// such label when any shown file is binary; a binary file that is new
    /// or removed keeps its `[new]`/`[del]` tag.
    ///
    /// Glyph colors follow [`Display::review_section`]'s diff-line convention
    /// (`+` green, `-` red); directories use [`neutral_gray`] like body text
    /// while basenames take `emphasis` (ADR 0018 redesign). The caller's
    /// `width` is the row budget: full terminal width for landed Commit
    /// entries, the 100-column prose cap for the in-frame preview.
    /// Widths use the codebase's char-count model (same as [`wrap_line`]), so
    /// the grid holds exactly for ASCII paths and approximately for CJK ones.
    /// Column widths come from the shown files only. A filename wider than
    /// the grid is truncated with `…` — the one case the footer truncates,
    /// since a broken column defeats the grid; on a pathologically narrow
    /// terminal (`name_cap == 0`) the grid degrades to unpadded overflow,
    /// matching [`wrap_line`]'s `width == 0` convention. The
    /// [`FILE_STATS_CAP`] bounds height. Returns the rows emitted, for the
    /// preview's erase accounting.
    fn emit_file_stats(
        &self,
        stats: &[FileStats],
        subject: &SubjectOffsets,
        width: usize,
        floor: CountsGeometry,
    ) -> usize {
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
        // Grid geometry. One counts block per footer: `+N` and `−M` each
        // right-align in their own column with a 1-char gap, so the total
        // row's numbers land exactly under the per-file counts. The block
        // anchors at the hash column; its right edge is capped 2 short of
        // the message start so the name column can pin there — wider blocks
        // (4-digit Σ totals) grow leftward, clamped at the tick column
        // (`SubjectOffsets::tick`). The Σ row adds its `Σ ` glyph at the
        // block's left edge — landing under the ✓ whenever the block starts
        // at the hash (the common case, since hash = ✓ + 2) — and needs the
        // block to start ≥ 2 so the glyph never crosses the margin. Tag
        // column exists only when a shown file carries one (` [new]` /
        // ` [del]` are both 6 chars). Name column: widest shown name,
        // capped so the row fits the resolved text width.
        let total_added: usize = stats.iter().map(|s| s.added).sum();
        let total_deleted: usize = stats.iter().map(|s| s.deleted).sum();
        // The Σ glyph's own cell width ("Σ ") — the σ column every footer
        // reserves left of the counts block (blank on file rows, drawn on
        // the Σ row of multi-file footers).
        let sigma_cell: usize = 2;
        // Binary rows carry the byte-size delta label `(binary) +12.3 KB`
        // (Commit entry v2, ADR 0018) — the counts region widens to the
        // widest binary label and the lead pad keeps the name column
        // aligned across text, binary, and Σ rows.
        // Column widths come from [`counts_geometry`] — one shared pure
        // derivation — with every field floored by the Run-wide geometry:
        // flooring the region total alone is not enough, because
        // per-entry minus widths still size where the `+` column's right
        // edge lands. A batch whose counts are all 1-digit renders with
        // its siblings' plus, minus, AND region widths, so no column of
        // the flushed table drifts across entries.
        let geo = counts_geometry(stats);
        let plus_width = geo.plus.max(floor.plus);
        let minus_width = geo.minus.max(floor.minus);
        let sep = if plus_width > 0 && minus_width > 0 {
            1
        } else {
            0
        };
        let base_region = plus_width + sep + minus_width;
        let counts_region = geo.region.max(floor.region).max(base_region);
        let lead = " ".repeat(counts_region - base_region);
        // Fixed 2-column gutter between the counts block and the names.
        let name_gap: usize = 2;
        // Block anchor: start at the hash, slide left only as far as the
        // tick column (`SubjectOffsets::tick`) when the capped right edge
        // demands it. The Σ row's glyph needs 2 columns of its own left of
        // the block — and so does every entry WITHOUT a Σ row: the σ
        // column is part of the shared grid (file rows render it blank),
        // and a Run-wide floor means single-file and multi-file entries
        // must anchor identically or their name columns drift apart.
        let start = subject
            .hash
            .min(
                subject
                    .message
                    .saturating_sub(name_gap)
                    .saturating_sub(counts_region),
            )
            .max(subject.tick)
            .max(sigma_cell);
        let right = start + counts_region;
        // Name column pins under the message start, pushed right only when
        // the counts block is too wide to leave a gap.
        let name_col = subject.message.max(right + name_gap);
        let gap = " ".repeat(name_col - right);
        let start_pad = " ".repeat(start);
        let tag_col = if shown_stats.iter().any(|s| s.new || s.removed) {
            6
        } else {
            0
        };
        let name_cap = width.saturating_sub(name_col + tag_col);
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
                let label = binary_label(s);
                let pad = counts_region - label.chars().count();
                let delta = format_bytes_delta(s.old_size, s.new_size);
                let delta_style = if delta.starts_with('−') {
                    red.clone()
                } else {
                    green.clone()
                };
                format!(
                    "{}{} {}",
                    " ".repeat(pad),
                    self.styled("(binary)", gray.clone()),
                    self.styled(&delta, delta_style)
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
                format!("{lead}{}", fmt_columns(&plus, &minus))
            };
            // Name column: truncated with `…` when wider than the cap,
            // padded to the grid width otherwise. Coloring (ADR 0018
            // redesign): the directory stays muted gray while the basename
            // takes `emphasis` — the eye finds the file, the path stays
            // quiet. Truncation first, coloring after, so the `…` rides the
            // basename's style; colors-off renders the same plain text as
            // before.
            let mut name = s.path.clone();
            if align && name.chars().count() > name_width {
                let keep = name_width.saturating_sub(1);
                name = format!("{}…", name.chars().take(keep).collect::<String>());
            }
            let styled_name = match name.rfind('/') {
                Some(i) => format!(
                    "{}{}",
                    self.styled(&name[..=i], gray.clone()),
                    self.styled(&name[i + 1..], palette::emphasis())
                ),
                None => self.styled(&name, palette::emphasis()),
            };
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
                    "{start_pad}{counts}{gap}{styled_name}{}{}",
                    " ".repeat(name_pad),
                    tag
                )
                .trim_end(),
            );
            rows += 1;
        }
        if stats.len() > Self::FILE_STATS_CAP {
            // The Σ total row below carries no file count — the elision
            // line names only how many rows were cut, and sits in the name
            // column with the
            // filenames it stands in for.
            self.emit(&format!(
                "{}{}",
                " ".repeat(name_col),
                self.styled(&format!("… {} more", stats.len() - shown), gray.clone())
            ));
            rows += 1;
        }
        if stats.len() > 1 {
            let plus = format!("+{total_added}");
            let minus = format!("−{total_deleted}");
            // Commit entry v2 (ADR 0018 amendment 3): `Σ +A −D <bar>` —
            // the Diff bar trails the counts and fills EVERY remaining
            // cell of the text budget (the same prose boundary every other
            // Run surface wraps at — like the reasoning feed — not the
            // terminal's real edge), so the row reads totals-first and the
            // bar is the full-bleed ratio visual. Binary files contribute
            // 0 to the text-line totals, so they never skew the ratio; an
            // all-binary footer (T = 0) omits the bar (`diff_bar` → (0,0)).
            const MIN_BAR_CELLS: usize = 4;
            // The row hangs under the subject (sigma_pad) before Σ even
            // starts, so the pad must be inside the fixed spend: budgeting
            // the bar from the raw `width` alone overshoots by the hang and
            // wraps the row on the terminal (found by the PTY harness at
            // 100 cols: a 103-column Σ row). Σ's own cell + gaps + counts
            // + the bar's leading space complete the spend; every count is
            // fixed-width, so the counts never truncate — only the bar
            // shrinks, and it disappears below the floor.
            let sigma_pad_width = start.saturating_sub(sigma_cell);
            // The counts ride the same region as the file rows (lead pad +
            // right-aligned columns) instead of their natural width, so
            // under a Run-wide floor the Σ numbers still land under the
            // per-file counts and the bar starts at the same column in
            // every entry of the Run. Budget: the bar's leading space is
            // the only spend beyond the fixed prefix, so the row ends
            // exactly at the text boundary — margin 2 + the full capped
            // text budget — never past it.
            let fixed = sigma_pad_width + sigma_cell + counts_region + 1;
            let cells = width.saturating_sub(fixed);
            let (add_cells, del_cells) = if cells >= MIN_BAR_CELLS {
                diff_bar(total_added, total_deleted, cells)
            } else {
                (0, 0)
            };
            let bar = if add_cells + del_cells > 0 {
                format!(
                    " {}{}",
                    self.styled(&"█".repeat(add_cells), green.clone()),
                    self.styled(&"░".repeat(del_cells), red.clone())
                )
            } else {
                String::new()
            };
            let sigma_pad = " ".repeat(sigma_pad_width);
            self.emit(&format!(
                "{sigma_pad}{} {lead}{}{bar}",
                self.styled("Σ", sigma_color().bold()),
                fmt_columns(&plus, &minus),
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
    /// the file-stats footer ([`Display::emit_file_stats`]). The preview's
    /// own prefix rule: body hangs under the subject's message start (the
    /// `? ` marker is the whole prefix, so 2 cols); the footer has no ref
    /// line to key on, so its counts anchor at the margin — nudged 2 cols
    /// right when the Σ row is present, so the glyph still lands under the
    /// `?` — and names pin to the message start.
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
        // The preview rides the live frame, where a wrapped row would
        // desync the whole-frame repaint — same clamp as the landed entry.
        let budget = resolve_cols(self.cols).saturating_sub(LEFT_MARGIN + 2);
        self.emit(&format!(
            "{} {}",
            self.styled("?", pending.clone()),
            self.styled_subject(&clamp_to_width(message, budget))
        ));
        let mut rows = 2;
        // `? ` — the preview subject's marker prefix: body hangs under the
        // message start (2 cols); the footer anchors its counts at the
        // margin (no ref line) — nudged 2 right on multi-file footers so
        // the Σ glyph lands under the `?` — with names pinned to the
        // message start (see [`Self::subject_offsets`] for the landed
        // twin's rule).
        const PREVIEW_OFFSETS: SubjectOffsets = SubjectOffsets {
            tick: 0,
            hash: 0,
            message: 2,
        };
        if let Some(b) = body {
            rows += self.emit_body(b, PREVIEW_OFFSETS.message);
        }
        rows += self.emit_file_stats(
            stats,
            &PREVIEW_OFFSETS,
            self.text_width(),
            CountsGeometry::ZERO,
        );
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

/// Columns (beyond the shared [`MARGIN`]) where the subject line's `✓`
/// marker, short hash, and message text start — the hang anchors for the
/// body (message start), the footer's counts block (hash start), and its
/// Σ glyph (tick). Plain char lengths, because the styled subject carries
/// ANSI bytes, not columns.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct SubjectOffsets {
    /// Column of the `✓` marker — the Σ glyph's landing spot, and the
    /// counts block's leftward clamp.
    tick: usize,
    /// Column of the short hash — the footer counts block's anchor.
    hash: usize,
    /// Column of the subject's message text — the body hang and the
    /// footer's name pin.
    message: usize,
}

/// The Diff bar cell split (Commit entry v2, ADR 0018): `add_cells =
/// round(cells · A / T)` over **text line counts only** (callers pass text
/// totals; binary churn never feeds the bar), `T = A + D`. `T == 0`
/// (all-binary) returns `(0, 0)` — the caller omits the bar. A single-sign
/// diff returns the full bar in one color. Rounding can never overflow the
/// budget: the delete side takes whatever is left. Pure.
fn diff_bar(added: usize, deleted: usize, cells: usize) -> (usize, usize) {
    let total = added + deleted;
    if total == 0 || cells == 0 {
        return (0, 0);
    }
    let add = ((added as f64 / total as f64) * cells as f64).round() as usize;
    let add = add.min(cells);
    (add, cells - add)
}

/// Counts-grid geometry for one Commit entry footer (Commit entry v2, ADR
/// 0018): the `+N`/`−M` column widths — sized to the per-file maxima AND
/// the Σ totals, since the total can carry more digits than any single
/// file (ten `+1` → `+10`) and an all-binary diff totals `+0`/`−0` where
/// every per-file width is 0 — and the full counts-region width, which
/// additionally covers the widest binary label `(binary) +12.3 KB`. Pure,
/// so the Run computes it ONCE over all its files and floors every entry's
/// footer with the result: per-entry digit counts can't drift the table's
/// columns across batches.
pub(crate) fn counts_geometry(stats: &[FileStats]) -> CountsGeometry {
    let total_added: usize = stats.iter().map(|s| s.added).sum();
    let total_deleted: usize = stats.iter().map(|s| s.deleted).sum();
    let plus = stats
        .iter()
        .map(|s| {
            if s.binary {
                0
            } else {
                format!("+{}", s.added).chars().count()
            }
        })
        .max()
        .unwrap_or(0)
        .max(format!("+{total_added}").chars().count());
    let minus = stats
        .iter()
        .map(|s| {
            if s.binary {
                0
            } else {
                format!("−{}", s.deleted).chars().count()
            }
        })
        .max()
        .unwrap_or(0)
        .max(format!("−{total_deleted}").chars().count());
    let sep = if plus > 0 && minus > 0 { 1 } else { 0 };
    let base = plus + sep + minus;
    let region = stats
        .iter()
        .map(|s| binary_label(s).chars().count())
        .max()
        .unwrap_or(0)
        .max(base);
    CountsGeometry {
        plus,
        minus,
        region,
    }
}

/// The [`counts_geometry`] result. `region` is the full counts-block width
/// (columns incl. the `+`/`−` gap and binary-label coverage); `plus` /
/// `minus` are the right-aligned column widths inside it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct CountsGeometry {
    pub(crate) plus: usize,
    pub(crate) minus: usize,
    pub(crate) region: usize,
}

impl CountsGeometry {
    /// The all-zero floor: the caller keeps its own per-entry geometry.
    /// The pre-commit preview passes this — it renders alone and is
    /// erased on confirm, so it never shares a table with siblings.
    pub(crate) const ZERO: Self = Self {
        plus: 0,
        minus: 0,
        region: 0,
    };
}

/// A binary file's counts-region label (Commit entry v2, ADR 0018):
/// `(binary) <byte-delta>`. One home shared by the row renderer and
/// [`counts_geometry`]'s region width, so the label and the width it
/// sizes can never drift apart. Pure.
fn binary_label(s: &FileStats) -> String {
    if s.binary {
        format!("(binary) {}", format_bytes_delta(s.old_size, s.new_size))
    } else {
        String::new()
    }
}

/// Format a blob-size delta for a binary per-file row (ADR 0018): sign
/// always present (`+`/`−` — U+2212, matching the `−M` counts convention);
/// `< 1024` renders whole bytes (`+512 B`), else one decimal of the fitting
/// KB/MB/GB unit (÷1024 steps). Pure, byte-exact-testable.
fn format_bytes_delta(old: u64, new: u64) -> String {
    let delta = new as i64 - old as i64;
    let sign = if delta < 0 { '−' } else { '+' };
    let abs = delta.unsigned_abs();
    let body = if abs < 1024 {
        format!("{abs} B")
    } else {
        let kb = abs as f64 / 1024.0;
        if kb < 1024.0 {
            format!("{kb:.1} KB")
        } else {
            let mb = kb / 1024.0;
            if mb < 1024.0 {
                format!("{mb:.1} MB")
            } else {
                format!("{:.1} GB", mb / 1024.0)
            }
        }
    };
    format!("{sign}{body}")
}

#[cfg(test)]
mod tests;
