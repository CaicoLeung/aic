use super::*;
use parking_lot::Mutex;
use std::sync::Arc;

// `console` reads the process-global `colors_enabled()` flag at format
// time, so every test that flips it via `ColorGuard` races every other.
// Lock here for the whole test body to serialize the color-env tests and
// keep the suite safe to run multi-threaded.
static COLOR_ENV: Mutex<()> = Mutex::new(());

/// Takes both process-global color-state locks for tests asserting exact
/// truecolor bytes: `COLOR_ENV` (console's enabled flag, flipped by
/// `ColorGuard`) and the palette neon toggle — a `set_neon` in a parallel
/// `palette::tests` test would otherwise reroute accessors mid-assertion.
fn color_locks() -> (
    parking_lot::MutexGuard<'static, ()>,
    parking_lot::MutexGuard<'static, ()>,
) {
    (
        COLOR_ENV.lock(),
        crate::render::palette::NEON_TOGGLE_LOCK.lock(),
    )
}

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

/// Plain text-file `FileStats`; flagged variants use `..fs(..)` struct
/// update.
fn fs(path: &str, added: usize, deleted: usize) -> FileStats {
    FileStats {
        path: path.into(),
        added,
        deleted,
        ..Default::default()
    }
}

/// A `Display` over a fresh `Buf` sink, plain or styled, plus the sink's
/// shared line buffer for post-hoc assertions.
fn disp(colors: bool) -> (Display, Arc<Mutex<Vec<String>>>) {
    let lines = Arc::new(Mutex::new(Vec::new()));
    (
        Display::with(Buf {
            colors,
            lines: lines.clone(),
        }),
        lines,
    )
}

/// `disp` with an explicit terminal width.
fn disp_cols(cols: usize) -> (Display, Arc<Mutex<Vec<String>>>) {
    let lines = Arc::new(Mutex::new(Vec::new()));
    (
        Display::with_cols(
            Buf {
                colors: false,
                lines: lines.clone(),
            },
            cols,
        ),
        lines,
    )
}

/// `emit_file_stats` with the tail every plain-footer test repeats: the
/// preview's prefix-free geometry (tick 0, hash 0, message 2), the
/// text-width budget, and zeroed counts geometry.
fn preview_footer(d: &Display, stats: &[FileStats]) -> usize {
    d.emit_file_stats(
        stats,
        &SubjectOffsets {
            tick: 0,
            hash: 0,
            message: 2,
        },
        d.text_width(),
        CountsGeometry::ZERO,
    )
}

/// One `commit_line` over a fresh truecolor sink with the process-global
/// color state locked and forced — the emitted lines joined with `\n`,
/// ready for truecolor substring asserts.
fn styled_line(hash: &str, msg: &str) -> String {
    let _locks = color_locks();
    let _guard = ColorGuard::force();
    let (d, lines) = disp(true);
    d.commit_line(hash, msg, &[], CountsGeometry::ZERO);
    lines.lock().join("\n")
}

#[test]
fn commit_preview_renders_message_body_and_file_list() {
    let (d, lines) = disp(false);
    let rows = d.commit_preview(
        "feat(auth): add OAuth2 login support",
        Some("Allow users to sign in via Google and GitHub OAuth2 providers"),
        &[
            FileStats {
                new: true,
                ..fs("src/auth.rs", 12, 3)
            },
            fs("src/main.rs", 4, 1),
        ],
    );
    let got = lines.lock().clone();
    // The subject row's `?` marker alone frames the block as pending (no
    // header row — one `?` is enough and the frame keeps the row); body
    // hangs under the subject's message start (`? ` = 2 cols beyond the
    // margin); the file-stats footer hangs at the same column — counts
    // first, then filename, `[new]`/`[del]` tag, and a Σ total; a trailing
    // blank separates the preview from the confirmation menu. `rows` is the
    // whole block, so the caller can erase it after the draft is confirmed.
    assert_eq!(got[0], "  ? feat(auth): add OAuth2 login support");
    assert_eq!(
        got[1],
        "    Allow users to sign in via Google and GitHub OAuth2 providers"
    );
    // File rows carry a blank Σ column (`Σ ` wide); +N and −M each
    // right-align in their own column (" +4" carries the pad). The Σ row
    // hangs under the `?` marker (margin) while file rows hang under the
    // message start (2 cols) — the preview twin of the landed Σ-under-✓
    // geometry; the Σ glyph still sits in the same column as the file
    // rows' reserved blank.
    assert_eq!(got[2], "    +12 −3  src/auth.rs [new]");
    assert_eq!(got[3], "     +4 −1  src/main.rs");
    assert_eq!(
        got[4],
        format!("  Σ +16 −4 {}{}", "█".repeat(54), "░".repeat(13))
    );
    assert_eq!(got[5], "");
    assert_eq!(rows, 6, "subject + body + 2 files + total + blank");
}

/// A single-file batch still carries the Σ total row: the row is gated on
/// the footer existing, not on the file count, so a one-file commit shows
/// the same totals-first ratio bar as its multi-file siblings.
#[test]
fn commit_preview_single_file_still_shows_sigma_total() {
    let (d, lines) = disp(false);
    let rows = d.commit_preview("chore: bump dep", None, &[fs("Cargo.toml", 5, 2)]);
    let got = lines.lock().clone();
    assert_eq!(got[0], "  ? chore: bump dep");
    // Single file, no body: the footer hangs under the subject's message
    // start (`? ` marker = 2 cols) and anchors at the shared σ column like
    // every landed entry — and still carries the Σ total row with its
    // full-bleed ratio bar (text budget 76; fixed spend `Σ ` + the `+5 −2`
    // region + the bar's gap = 8 → 68 cells; 5/7 of 68 rounds to 49 add).
    assert_eq!(got[1], "    +5 −2  Cargo.toml");
    assert_eq!(
        got[2],
        format!("  Σ +5 −2 {}{}", "█".repeat(49), "░".repeat(19))
    );
    assert_eq!(got.len(), 4, "no body line expected, got: {got:?}");
    assert_eq!(rows, 4, "subject + file + total + blank");
}

/// `clear_last` drops the most recent rows from the buffer (the in-memory
/// analogue of erasing a preview on a real terminal), and `0` is a no-op —
/// so the confirmed-draft erase never touches earlier commit lines.
#[test]
fn clear_last_erases_only_the_most_recent_rows() {
    let (d, lines) = disp(false);
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
    let (d, lines) = disp(false);
    let stats: Vec<FileStats> = (1..=10)
        .map(|i| FileStats {
            path: format!("src/f{i}.rs"),
            ..fs("", 1, 0)
        })
        .collect();
    let rows = d.commit_preview("feat: big", None, &stats);
    let got = lines.lock().clone();
    assert!(
        got.iter().any(|l| l.contains("… 2 more")),
        "expected a truncated file list, got: {got:?}"
    );
    assert!(
        !got.iter().any(|l| l.contains("… 2 more (")),
        "the elision line must not repeat the Σ row's (N files) count, got: {got:?}"
    );
    assert!(
        got.iter().any(|l| l.contains("Σ +10 −0 █")),
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
    assert_eq!(rows, 12, "subject + 8 files + elision + total + blank");
}

/// The footer's edge rendering: binary files show `(binary)` instead of
/// counts and keep their `[new]`/`[del]` tag; deleted files carry `[del]`,
/// and the Σ total sums across all entries.
#[test]
fn file_stats_footer_marks_binary_and_deleted_files() {
    let (d, lines) = disp(false);
    let rows = preview_footer(
        &d,
        &[
            FileStats {
                new: true,
                binary: true,
                new_size: 512,
                ..fs("img.png", 0, 0)
            },
            FileStats {
                removed: true,
                ..fs("src/old.rs", 0, 12)
            },
        ],
    );
    let got = lines.lock().clone();
    // A new binary file keeps its `[new]` tag (the binary label replaces
    // the line counts, not the tag) and shows its full blob size as the
    // byte delta (`+512 B`). `(binary) +512 B` (15 chars) spans the widened
    // counts region; the name pads to align with src/old.rs (10 chars). The
    // deletion-only file renders no `+0` column (git --stat convention) —
    // its `−12` right-aligns to the region like every text row. The Σ bar
    // is deletion-only → all `░` cells.
    assert_eq!(got[0], "    (binary) +512 B  img.png    [new]");
    assert_eq!(got[1], "                −12  src/old.rs [del]");
    // Deletion-only → every bar cell is `░`; the Σ counts right-align into
    // the 15-cell region (9-cell lead), trading 9 bar cells for the
    // alignment — the row still ends at the same budget edge. Spelled with
    // `repeat` so the pinned width cannot drift by a miscounted glyph.
    assert_eq!(got[2], format!("  Σ          +0 −12 {}", "░".repeat(58)));
    assert_eq!(rows, 3, "2 files + total");
}

/// The Σ total can carry more digits than any single file (two `+5` files
/// total `+10`); the column must size to the total so `+10` lands exactly
/// under each `+5` rather than overflowing into the gap. Regression for
/// the per-file-max-only column width.
#[test]
fn file_stats_footer_aligns_total_wider_than_per_file_counts() {
    let (d, lines) = disp(false);
    let rows = preview_footer(&d, &[fs("a.rs", 5, 0), fs("b.rs", 5, 0)]);
    let got = lines.lock().clone();
    // `+5` right-aligns in a 3-wide column (sized to `+10`), so its `5`
    // sits under the total's `0` of `+10`; both end at the same column.
    // The zero-deletion column renders blank (git --stat convention) but
    // keeps its reserved width, so the name still lands where the Σ row's
    // total's `+10` column.
    assert_eq!(got[0], "     +5     a.rs");
    assert_eq!(got[1], "     +5     b.rs");
    assert_eq!(got[2], format!("  Σ +10 −0 {}", "█".repeat(67)));
    assert_eq!(rows, 3, "2 files + total");
}

/// An all-binary diff has no per-file counts (every width is 0), yet the
/// Σ row still renders a stable, gapped `+0 −0` — not a jammed `+0−0`.
/// Regression for the all-binary column collapse.
#[test]
fn file_stats_footer_stable_columns_when_all_files_binary() {
    let (d, lines) = disp(false);
    let rows = preview_footer(
        &d,
        &[
            FileStats {
                new: true,
                binary: true,
                new_size: 2048,
                ..fs("img.png", 0, 0)
            },
            FileStats {
                binary: true,
                old_size: 4096,
                ..fs("data.bin", 0, 0)
            },
        ],
    );
    let got = lines.lock().clone();
    // New binary keeps `[new]` and shows its full size (`+2.0 KB`); the
    // deleted binary shows a negative delta (`−4.0 KB`). The counts region
    // widens to fit the 16-char binary labels, so the filename column
    // aligns across both rows — and the Σ counts right-align into that
    // same region (11-cell lead), landing under the labels' right edge.
    // T = 0 (all-binary) → the Σ bar is omitted entirely: `Σ +0 −0`.
    assert_eq!(got[0], "    (binary) +2.0 KB  img.png  [new]");
    assert_eq!(got[1], "    (binary) −4.0 KB  data.bin");
    assert_eq!(got[2], "  Σ            +0 −0");
    assert_eq!(rows, 3, "2 files + total");
}

/// A binary file alongside a text file whose counts region is narrower
/// than `(binary)`: the region widens to 8 and every row — text, binary,
/// Σ — carries the same leading pad, so `(binary)`'s right edge, the text
/// `−M`, and the filename column all line up. Regression for the
/// binary-overflow column drift between file rows and the Σ row.
#[test]
fn file_stats_footer_mixed_binary_keeps_columns_aligned() {
    let (d, lines) = disp(false);
    let rows = preview_footer(
        &d,
        &[
            FileStats {
                binary: true,
                old_size: 1024,
                new_size: 13312,
                ..fs("x.bin", 0, 0)
            },
            fs("a.rs", 1, 0),
        ],
    );
    let got = lines.lock().clone();
    // base region (`+1` 2 + gap 1 + `−0` 2 = 5) widens to 17 for
    // `(binary) +12.0 KB`; the text row carries the lead pad, so both rows'
    // counts end at the same column and the filenames start at the same
    // column. The text row's zero-deletion column renders blank but keeps
    // its width, so `a.rs` still lands under `x.bin`. Byte churn stays
    // per-file: the Σ bar/counts are text-lines-only (single-sign → full
    // `█` bar).
    assert_eq!(got[0], "    (binary) +12.0 KB  x.bin");
    assert_eq!(got[1], "                +1     a.rs");
    assert_eq!(got[2], format!("  Σ             +1 −0 {}", "█".repeat(56)));
    assert_eq!(rows, 3, "2 files + total");
}

/// A filename wider than the grid's name column is truncated with `…`
/// (char-count model) so the columns — and the tag column — stay intact:
/// the short file's name pads to the same width and its `[new]` tag lands
/// at the same column as a tag on the truncated row would.
#[test]
fn file_stats_footer_truncates_long_names_to_keep_the_grid() {
    let (d, lines) = disp(false);
    let long = "x".repeat(70);
    let rows = preview_footer(
        &d,
        &[
            FileStats {
                path: long.clone(),
                ..fs("", 1, 0)
            },
            FileStats {
                new: true,
                ..fs("a.rs", 1, 0)
            },
        ],
    );
    let got = lines.lock().clone();
    // text_width is 76 (80 - 2 - 2); counts region = Σ (2) + `+1` (2) +
    // gap (1) + `−0` (2) = 7; name column = 76 - 7 (counts) - 2 (gap)
    // - 6 (tag column) = 61 → 60 chars + "…". File rows carry a blank
    // Σ column; the zero-deletion column renders blank but keeps its
    // width, so the name column math is unchanged.
    assert_eq!(got[0], format!("    +1     {}", "x".repeat(60) + "…"));
    assert_eq!(got[1], format!("    +1     a.rs{} [new]", " ".repeat(57)));
    assert_eq!(rows, 3, "2 files + total");
}

/// The landed line shows the same footer as the preview — the file stats
/// survive the commit.
#[test]
fn commit_line_renders_file_stats_footer() {
    let (d, lines) = disp(false);
    d.commit_line(
        "abc1234",
        "feat: add thing",
        &[FileStats {
            new: true,
            ..fs("src/auth.rs", 7, 2)
        }],
        CountsGeometry::ZERO,
    );
    let got = lines.lock().clone();
    assert!(
        got.iter().any(|l| l.contains("+7 −2   src/auth.rs [new]")),
        "committed line must show the footer, got: {got:?}"
    );
    // Single file still carries the Σ total row (the footer gate is on
    // existence, not file count); 68 cells, 7/9 of them add.
    assert!(
        got.iter()
            .any(|l| *l == format!("  Σ +7 −2 {}{}", "█".repeat(53), "░".repeat(15))),
        "single-file landed commit must show the Σ total row, got: {got:?}"
    );
}

/// The preview path's Σ row is covered above; the landed ✓ line must show
/// the same total row for a multi-file commit — `commit_line` and
/// `commit_preview` share `emit_file_stats`, but this pins the contract on
/// the landed entry so a regression that drops the Σ row only post-commit
/// (e.g. a guard misplaced between the two callers) fails here.
#[test]
fn commit_line_renders_sigma_row_for_multiple_files() {
    let (d, lines) = disp(false);
    d.commit_line(
        "abc1234",
        "feat: add thing",
        &[
            fs("src/a.rs", 3, 1),
            FileStats {
                new: true,
                ..fs("src/b.rs", 5, 0)
            },
        ],
        CountsGeometry::ZERO,
    );
    let got = lines.lock().clone();
    assert!(
        got.iter()
            .any(|l| l.contains("Σ") && l.contains("+8") && l.contains("−1")),
        "multi-file landed commit must show the Σ total row, got: {got:?}"
    );
}

/// The footer's file rows hang under the commit ID while the subject line
/// carries NO delta (ADR 0018 amendment 3: the footer's per-file counts and
/// Σ row own the numbers, `git log` owns the prose). Columns are derived
/// from the emitted subject line itself (char-counted — the ✓ marker is
/// multi-byte, so a byte offset would overcount), so the asserts stay
/// honest if the entry format changes. The commit body is NOT part of the
/// entry (ADR 0018 redesign): exactly three rows — subject + file row +
/// Σ row (single file included). The Σ-under-✓ anchor is pinned on
/// the preview twin above and the Σ-row geometry tests below.
#[test]
fn commit_footer_hangs_under_subject_hash_without_delta() {
    let (d, lines) = disp(false);
    d.commit_line(
        "abc1234",
        "feat: add thing",
        &[fs("src/a.rs", 1, 0)],
        CountsGeometry::ZERO,
    );
    let got = lines.lock().clone();
    let cols = |needle: &str| {
        got[0]
            .find(needle)
            .map(|i| got[0][..i].chars().count())
            .unwrap_or_else(|| panic!("subject must carry {needle:?}: {got:?}"))
    };
    let msg_col = cols("feat: add thing");
    let hash_col = cols("abc1234");
    assert_eq!(
        got.len(),
        3,
        "entry = subject + file row + Σ row, no body: {got:?}"
    );
    assert!(
        got[0].ends_with("feat: add thing") && !got[0].contains("· +"),
        "subject is title-only, no delta suffix: {got:?}"
    );
    // Counts (`+1`, region 2) anchor at the hash; the gap to the name
    // fills the rest of the hash→message span (uniform across rows).
    assert_eq!(
        got[1],
        format!(
            "{}+1{}src/a.rs",
            " ".repeat(hash_col),
            " ".repeat(msg_col - hash_col - 2)
        ),
        "footer must hang under the commit ID: {got:?}"
    );
    // The Σ row lands under the ✓: fixed spend `Σ ` + region 5 + the
    // bar's gap = 8 → 68 cells, all add (T = 1 added, 0 deleted).
    assert_eq!(
        got[2],
        format!("  Σ +1 −0 {}", "█".repeat(68)),
        "single-file entry keeps its Σ total row: {got:?}"
    );
}

/// When the counts block is exactly as wide as the hash→message span
/// minus the gap — the `+152 −84` shape a big multi-file Run produces —
/// the block slides left to the ✓ column instead of pushing names off the
/// message: counts still end 2 short of the message start, names still pin
/// under the message, gap stays 2, and the Σ glyph leads the block from the
/// prefix's tail. The regression this pins: names drifting right of the
/// commit message when Σ totals carry more digits than any single file.
#[test]
fn commit_footer_wide_counts_grow_left_not_right() {
    let (d, lines) = disp(false);
    d.commit_line(
        "0875db3",
        "fix(display): align footer",
        &[
            fs("src/render/display/mod.rs", 64, 51),
            fs("src/render/display/tests.rs", 88, 33),
        ],
        CountsGeometry::ZERO,
    );
    let got = lines.lock().clone();
    // Region 8 (`+152 −84` sized): block starts at the hash column (2),
    // ends 2 short of the message (10), names pin at 12.
    assert_eq!(got[0], "  \u{2713} 0875db3 fix(display): align footer");
    assert_eq!(got[1], "     +64 −51  src/render/display/mod.rs");
    assert_eq!(got[2], "     +88 −33  src/render/display/tests.rs");
    // edge (no wrap): computed, not hand-counted glyphs.
    assert_eq!(
        got[3],
        format!("  Σ +152 −84 {}{}", "█".repeat(42), "░".repeat(23))
    );
}

/// The Σ total row hangs under the `✓` marker — exactly `sigma_col` (2)
/// columns left of the file rows' hang under the commit ID — so the Σ
/// glyph lands in the grid column the file rows reserve as blank, and the
/// totals still right-align into the per-file `+N`/`−M` columns.
#[test]
fn commit_footer_sigma_hangs_under_the_tick_marker() {
    let (d, lines) = disp(false);
    d.commit_line(
        "abc1234",
        "feat: add thing",
        &[fs("src/a.rs", 3, 1), fs("src/b.rs", 5, 0)],
        CountsGeometry::ZERO,
    );
    let got = lines.lock().clone();
    // Prefix-free geometry: tick at 0, hash at 2, message at 10. Region 5
    // (`+8 −1` sized): block starts at the hash (2), ends 7, names pin at
    // the message (10) — a 3-col gap — and the Σ glyph hugs the block's
    // left edge at 0, exactly under the ✓.
    assert_eq!(got[0], "  \u{2713} abc1234 feat: add thing");
    assert_eq!(got[1], "    +3 −1   src/a.rs");
    assert_eq!(got[2], "    +5      src/b.rs");
    assert_eq!(
        got[3],
        format!("  Σ +8 −1 {}{}", "█".repeat(60), "░".repeat(8))
    );
}

/// Run-wide counts region (ADR 0018 amendment 3): every entry of a Run
/// floors its counts grid with [`counts_geometry`] over the Run's WHOLE
/// diff, so a 1-digit batch landing beside a 3-digit sibling keeps the
/// same columns — the small batch's counts right-align into the widened
/// region and its name column matches the wide batch's exactly. Every
/// field of the geometry is floored (plus/minus/region): a region-only
/// floor still let per-entry minus widths shift the `+` column's edge.
/// Without the floor, each entry sizes to its own digits and the flushed
/// table's names/count columns drift apart across entries.
#[test]
fn commit_line_region_floor_keeps_entries_aligned() {
    let run = vec![fs("src/big.rs", 152, 84), fs("src/a.rs", 1, 0)];
    let floor = counts_geometry(&run);
    assert_eq!(floor.region, 8, "`+152 −84` sizes the Run-wide region");
    assert_eq!(
        (floor.plus, floor.minus),
        (4, 3),
        "…and the `+`/`−` columns inside it: `+152`/`−84` are the widest"
    );

    let (d, lines) = disp(false);
    d.commit_line("abc1234", "feat: small", &[fs("src/a.rs", 1, 0)], floor);
    d.commit_line("def5678", "feat: big", &[fs("src/big.rs", 152, 84)], floor);
    let got = lines.lock().clone();
    // Small entry: every field floored (plus 4, minus 3, region 8) and
    // anchored at the shared σ column (start 2) — its `+1` right-aligns
    // under the wide batch's `+152`, and names pin at the same column
    // (14). Each entry is now subject + file + Σ row, so the big entry's
    // rows sit at 3/4/5.
    assert_eq!(got[1], "      +1      src/a.rs");
    // Big entry: own geometry equals the floor; identical name column.
    assert_eq!(got[4], "    +152 −84  src/big.rs");
    // Both Σ rows ride the same floored region (8) and bar budget
    // (76 − `Σ ` − region − gap = 65 cells): the small entry's totals
    // right-align into the widened region and both bars start at the
    // same column (152/236 of 65 rounds to 42 add cells).
    assert_eq!(got[2], format!("  Σ   +1  −0 {}", "█".repeat(65)));
    assert_eq!(
        got[5],
        format!("  Σ +152 −84 {}{}", "█".repeat(42), "░".repeat(23))
    );
    let col = |l: &str, needle: &str| {
        l.find(needle)
            .map(|i| l[..i].chars().count())
            .unwrap_or_else(|| panic!("{needle:?} missing: {l:?}"))
    };
    assert_eq!(
        col(&got[1], "src/a.rs"),
        col(&got[4], "src/big.rs"),
        "name columns must not drift between entries: {got:?}"
    );
}

#[test]
fn plain_when_colors_disabled() {
    let (d, lines) = disp(false);
    d.commit_line("abc1234", "feat: add thing", &[], CountsGeometry::ZERO);
    let got = lines.lock().clone();
    // No ANSI escapes. Type prefix "feat" is present, followed by
    // ": add thing". Subject carries the 2-col left margin; empty stats →
    // no delta segment, and the entry is a single row (the body is
    // omitted — ADR 0018 redesign).
    assert_eq!(got.len(), 1);
    assert_eq!(got[0], "  \u{2713} abc1234 feat: add thing");
}

#[test]
fn truecolor_when_colors_enabled() {
    let joined = styled_line("abc1234", "feat: add thing");
    // hash #d97706 (bold), feat type green #15803d (bold), description
    // bold default fg.
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
}

#[test]
fn fix_type_gets_orange_color() {
    let joined = styled_line("def5678", "fix(auth): correct token check");
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
    let (d, lines) = disp(false);
    d.commit_line(
        "def5678",
        "fix(auth): correct token check",
        &[],
        CountsGeometry::ZERO,
    );
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
    for (type_str, rgb) in [
        ("feat", "21;128;61"),
        ("ci", "99;102;241"),
        ("chore", "15;118;110"),
        ("wip", "100;116;139"),
    ] {
        let joined = styled_line("hash000", &format!("{type_str}: msg"));
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
    let joined = styled_line("ghi9012", "blob: thing in progress");
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
    let joined = styled_line("jkl3456", "no colon message");
    // No-colon messages have no type token to color → muted gray #6b7280
    // (darkened from the old #9ca3af for white-bg readability).
    assert!(
        joined.contains("107;114;128"),
        "no-colon message should be muted gray: {joined:?}"
    );
}

/// Diff bar cell split (v2, ADR 0018): `add = round(cells · A / T)`, deletes
/// take the rest; single-sign diffs get the full bar; `T == 0` (all-binary)
/// and an empty budget return `(0, 0)` — the caller omits the bar.
#[test]
fn diff_bar_splits_cells_with_delete_taking_the_remainder() {
    // 12/(12+3) = 0.8 → 8 add cells (the spec's own worked example).
    assert_eq!(diff_bar(12, 3, 10), (8, 2));
    // Single-sign: full bar in one color.
    assert_eq!(diff_bar(10, 0, 10), (10, 0));
    assert_eq!(diff_bar(0, 12, 10), (0, 10));
    // All-binary: no text churn, no bar.
    assert_eq!(diff_bar(0, 0, 10), (0, 0));
    // Half-away rounding, delete side takes the remainder — never 11 cells.
    assert_eq!(diff_bar(1, 1, 10), (5, 5));
    assert_eq!(diff_bar(1, 3, 10), (3, 7));
    assert_eq!(diff_bar(2, 1, 9), (6, 3));
    // Degenerate budget.
    assert_eq!(diff_bar(5, 5, 0), (0, 0));
}

/// Byte-size delta formatting (v2, ADR 0018): sign always present (`−` is
/// U+2212, matching the `−M` counts convention); `< 1024` renders whole
/// bytes, else one decimal of the fitting KB/MB/GB unit (÷1024 steps).
#[test]
fn format_bytes_delta_covers_units_and_signs() {
    assert_eq!(format_bytes_delta(0, 0), "+0 B");
    assert_eq!(format_bytes_delta(0, 512), "+512 B");
    assert_eq!(format_bytes_delta(600, 100), "−500 B");
    assert_eq!(format_bytes_delta(0, 1024), "+1.0 KB");
    assert_eq!(format_bytes_delta(0, 12595), "+12.3 KB");
    assert_eq!(format_bytes_delta(46080, 0), "−45.0 KB");
    assert_eq!(format_bytes_delta(0, 3 * 1024 * 1024), "+3.0 MB");
    assert_eq!(format_bytes_delta(5 * 1024 * 1024 * 1024, 0), "−5.0 GB");
    // New binary → full size (old = 0); deleted → negative of the old size.
    assert_eq!(format_bytes_delta(0, 200), "+200 B");
    assert_eq!(format_bytes_delta(200, 0), "−200 B");
}

/// The Σ bar fills the row's remaining width (v3, ADR 0018 amendment 3):
/// a 30-col terminal affords 7 cells, a 28-col one 5, and a 24-col one
/// drops the bar entirely (below the 4-cell floor) — the counts never
/// truncate.
#[test]
fn sigma_bar_shrinks_then_omits_on_narrow_terminals() {
    let stats = |a: usize| {
        vec![
            fs("a.rs", a, 0),
            FileStats {
                binary: true,
                new_size: 512,
                ..fs("b.rs", 0, 0)
            },
        ]
    };
    for (cols, cells) in [(30, 8), (28, 6), (24, 0), (16, 0)] {
        let (d, lines) = disp_cols(cols);
        preview_footer(&d, &stats(10));
        let got = lines.lock().clone();
        let sigma = got.last().expect("Σ row present");
        let bar = if cells > 0 {
            format!(" {}", "█".repeat(cells))
        } else {
            String::new()
        };
        let want = format!("  Σ          +10 −0{bar}");
        assert_eq!(sigma.trim_end(), want, "cols {cols}: {got:?}");
    }
}

/// The Σ row's full-bleed bar is budgeted against the row's whole prefix
/// — subject hang included — within the TEXT budget (`text_width`, the
/// same prose boundary the reasoning feed wraps at), NOT the terminal's
/// real edge: on a 140-col terminal the bar still ends at the capped
/// prose edge, never wrapping the row (without the hang in the budget
/// the row hit 103 columns on a 100-col terminal — caught by the PTY
/// render harness). Contract: margin (2) + the capped text budget bound
/// the row.
#[test]
fn sigma_bar_ends_at_the_text_boundary_never_the_terminal_edge() {
    let (d, lines) = disp_cols(140);
    let stats = vec![fs("a.rs", 60, 0), fs("b.rs", 4, 1)];
    // Prefix-free entry geometry: tick 0, hash 2, message 10 — the Σ row
    // starts at the margin. `text_width` caps at the 100-col prose edge
    // even though the terminal reports 140.
    d.emit_file_stats(
        &stats,
        &SubjectOffsets {
            tick: 0,
            hash: 2,
            message: 10,
        },
        d.text_width(),
        CountsGeometry::ZERO,
    );
    let got = lines.lock().clone();
    let sigma = got.last().expect("Σ row present");
    assert!(sigma.contains('█'), "bar present: {sigma:?}");
    assert_eq!(
        sigma.chars().count(),
        98,
        "the Σ row ends exactly at the capped text boundary (margin 2 + the full 96-col text budget), never at the terminal edge: {sigma:?}"
    );
}

/// Subject rows clamp to the terminal's effective width (`…` marking the
/// cut): the entry row is a permanent terminal line and the preview row
/// rides the live frame — either wrapping to two physical rows desyncs
/// the painter/flush cursor arithmetic, which assumes one row per emitted
/// row. Commit subjects are prompt-capped at 72 chars but nothing
/// validates that, so the renderer owns the guard.
#[test]
fn commit_line_subject_clamps_to_the_terminal_width() {
    let (d, lines) = disp_cols(40);
    let message = format!("feat: {}", "x".repeat(60));
    d.commit_line("abc1234", &message, &[], CountsGeometry::ZERO);
    let got = lines.lock().clone();
    let row = &got[0];
    // Budget 28 (40 − margin 2 − `✓ ` 2 − hash 7 − space 1): the subject
    // carries the cut marker and the whole row fits 40 columns.
    assert!(row.contains('…'), "the cut is marked: {row:?}");
    assert!(
        row.chars().count() <= 40,
        "row must fit the terminal width: {row:?}"
    );
    assert!(
        row.starts_with("  \u{2713} abc1234 feat: "),
        "head kept: {row:?}"
    );
}

/// The preview subject clamps the same way (it rides the live frame,
/// where a wrapped row corrupts every later whole-frame repaint) —
/// budget is 2 wider than the entry's (no hash cell).
#[test]
fn commit_preview_subject_clamps_to_the_terminal_width() {
    let (d, lines) = disp_cols(30);
    let message = format!("feat: {}", "y".repeat(60));
    d.commit_preview(&message, None, &[]);
    let got = lines.lock().clone();
    let row = &got[0];
    assert!(
        row.chars().count() <= 30,
        "row must fit the terminal width: {row:?}"
    );
    assert!(row.starts_with("  ? feat: "), "head kept: {row:?}");
}
