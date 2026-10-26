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
                old_size: 0,
                new_size: 0,
            },
            FileStats {
                path: "src/main.rs".into(),
                added: 4,
                deleted: 1,
                new: false,
                removed: false,
                binary: false,
                old_size: 0,
                new_size: 0,
            },
        ],
    );
    let got = lines.lock().clone();
    // Pending header + subject carry the `?` marker; body hangs under the
    // subject's message start (`? ` = 2 cols beyond the margin); the
    // file-stats footer hangs at the same column — counts first, then
    // filename, `[new]`/`[del]` tag, and a Σ total; a trailing blank
    // separates the preview from the confirmation menu. `rows` is the whole
    // block, so the caller can erase it after the draft is confirmed.
    assert_eq!(got[0], "  ? proposed commit:");
    assert_eq!(got[1], "  ? feat(auth): add OAuth2 login support");
    assert_eq!(
        got[2],
        "    Allow users to sign in via Google and GitHub OAuth2 providers"
    );
    // File rows carry a blank Σ column (`Σ ` wide); +N and −M each
    // right-align in their own column (" +4" carries the pad). The Σ row
    // hangs under the `?` marker (margin) while file rows hang under the
    // message start (2 cols) — the preview twin of the landed Σ-under-✓
    // geometry; the Σ glyph still sits in the same column as the file
    // rows' reserved blank.
    assert_eq!(got[3], "    +12 −3  src/auth.rs [new]");
    assert_eq!(got[4], "     +4 −1  src/main.rs");
    assert_eq!(got[5], "  Σ ████████░░ +16 −4 · 2 files");
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
            old_size: 0,
            new_size: 0,
        }],
    );
    let got = lines.lock().clone();
    assert_eq!(got[0], "  ? proposed commit:");
    assert_eq!(got[1], "  ? chore: bump dep");
    // Single file: no Σ total line; no body line emitted. The footer hangs
    // under the subject's message start (`? ` marker = 2 cols).
    assert_eq!(got[2], "  +5 −2  Cargo.toml");
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
            old_size: 0,
            new_size: 0,
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
        got.iter()
            .any(|l| l.contains("Σ ██████████ +10 −0 · 10 files")),
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
    let rows = d.emit_file_stats(
        &[
            FileStats {
                path: "img.png".into(),
                added: 0,
                deleted: 0,
                new: true,
                removed: false,
                binary: true,
                old_size: 0,
                new_size: 512,
            },
            FileStats {
                path: "src/old.rs".into(),
                added: 0,
                deleted: 12,
                new: false,
                removed: true,
                binary: false,
                old_size: 0,
                new_size: 0,
            },
        ],
        &SubjectOffsets {
            tick: 0,
            hash: 0,
            message: 2,
        },
        d.text_width(),
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
    assert_eq!(got[2], "  Σ ░░░░░░░░░░ +0 −12 · 2 files");
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
    let rows = d.emit_file_stats(
        &[
            FileStats {
                path: "a.rs".into(),
                added: 5,
                deleted: 0,
                new: false,
                removed: false,
                binary: false,
                old_size: 0,
                new_size: 0,
            },
            FileStats {
                path: "b.rs".into(),
                added: 5,
                deleted: 0,
                new: false,
                removed: false,
                binary: false,
                old_size: 0,
                new_size: 0,
            },
        ],
        &SubjectOffsets {
            tick: 0,
            hash: 0,
            message: 2,
        },
        d.text_width(),
    );
    let got = lines.lock().clone();
    // `+5` right-aligns in a 3-wide column (sized to `+10`), so its `5`
    // sits under the total's `0` of `+10`; both end at the same column.
    // The zero-deletion column renders blank (git --stat convention) but
    // keeps its reserved width, so the name still lands where the Σ row's
    // `(2 files)` label starts.
    assert_eq!(got[0], "     +5     a.rs");
    assert_eq!(got[1], "     +5     b.rs");
    assert_eq!(got[2], "  Σ ██████████ +10 −0 · 2 files");
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
    let rows = d.emit_file_stats(
        &[
            FileStats {
                path: "img.png".into(),
                added: 0,
                deleted: 0,
                new: true,
                removed: false,
                binary: true,
                old_size: 0,
                new_size: 2048,
            },
            FileStats {
                path: "data.bin".into(),
                added: 0,
                deleted: 0,
                new: false,
                removed: false,
                binary: true,
                old_size: 4096,
                new_size: 0,
            },
        ],
        &SubjectOffsets {
            tick: 0,
            hash: 0,
            message: 2,
        },
        d.text_width(),
    );
    let got = lines.lock().clone();
    // New binary keeps `[new]` and shows its full size (`+2.0 KB`); the
    // deleted binary shows a negative delta (`−4.0 KB`). The counts region
    // widens to fit the 16-char binary labels, so the filename column
    // aligns across both rows. T = 0 (all-binary) → the Σ bar is omitted
    // entirely: `Σ +0 −0 · 2 files`.
    assert_eq!(got[0], "    (binary) +2.0 KB  img.png  [new]");
    assert_eq!(got[1], "    (binary) −4.0 KB  data.bin");
    assert_eq!(got[2], "  Σ +0 −0 · 2 files");
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
    let rows = d.emit_file_stats(
        &[
            FileStats {
                path: "x.bin".into(),
                added: 0,
                deleted: 0,
                new: false,
                removed: false,
                binary: true,
                old_size: 1024,
                new_size: 13312,
            },
            FileStats {
                path: "a.rs".into(),
                added: 1,
                deleted: 0,
                new: false,
                removed: false,
                binary: false,
                old_size: 0,
                new_size: 0,
            },
        ],
        &SubjectOffsets {
            tick: 0,
            hash: 0,
            message: 2,
        },
        d.text_width(),
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
    assert_eq!(got[2], "  Σ ██████████ +1 −0 · 2 files");
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
    let rows = d.emit_file_stats(
        &[
            FileStats {
                path: long.clone(),
                added: 1,
                deleted: 0,
                new: false,
                removed: false,
                binary: false,
                old_size: 0,
                new_size: 0,
            },
            FileStats {
                path: "a.rs".into(),
                added: 1,
                deleted: 0,
                new: true,
                removed: false,
                binary: false,
                old_size: 0,
                new_size: 0,
            },
        ],
        &SubjectOffsets {
            tick: 0,
            hash: 0,
            message: 2,
        },
        d.text_width(),
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
    let lines = Arc::new(Mutex::new(Vec::new()));
    let d = Display::with(Buf {
        colors: false,
        lines: lines.clone(),
    });
    d.commit_line(
        "abc1234",
        "feat: add thing",
        "[1/3]",
        &[FileStats {
            path: "src/auth.rs".into(),
            added: 7,
            deleted: 2,
            new: true,
            removed: false,
            binary: false,
            old_size: 0,
            new_size: 0,
        }],
    );
    let got = lines.lock().clone();
    assert!(
        got.iter().any(|l| l.contains("+7 −2   src/auth.rs [new]")),
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
        "[1/2]",
        &[
            FileStats {
                path: "src/a.rs".into(),
                added: 3,
                deleted: 1,
                new: false,
                removed: false,
                binary: false,
                old_size: 0,
                new_size: 0,
            },
            FileStats {
                path: "src/b.rs".into(),
                added: 5,
                deleted: 0,
                new: true,
                removed: false,
                binary: false,
                old_size: 0,
                new_size: 0,
            },
        ],
    );
    let got = lines.lock().clone();
    assert!(
        got.iter().any(|l| {
            l.contains("Σ") && l.contains("+8") && l.contains("−1") && l.contains("· 2 files")
        }),
        "multi-file landed commit must show the Σ total row, got: {got:?}"
    );
}

/// The footer's file rows hang under the commit ID while the subject line
/// carries the whole entry delta, pinned with and without a `[n/m] ` batch
/// prefix (staged / single-commit Runs) so the hang tracks the real subject
/// prefix rather than a frozen column. Columns are derived from the emitted
/// subject line itself (char-counted — the ✓ marker is multi-byte, so a
/// byte offset would overcount), so the asserts stay honest if the prefix
/// format changes. The commit body is NOT part of the entry (ADR 0018
/// redesign): exactly two rows — subject + file row. A single file keeps
/// the footer Σ-less; the Σ-under-✓ anchor is pinned on the preview twin
/// above and the Σ-row geometry tests below.
#[test]
fn commit_footer_hangs_under_subject_hash_and_carries_delta() {
    for prefix in ["[1/2]", ""] {
        let lines = Arc::new(Mutex::new(Vec::new()));
        let d = Display::with(Buf {
            colors: false,
            lines: lines.clone(),
        });
        d.commit_line(
            "abc1234",
            "feat: add thing",
            prefix,
            &[FileStats {
                path: "src/a.rs".into(),
                added: 1,
                deleted: 0,
                new: false,
                removed: false,
                binary: false,
                old_size: 0,
                new_size: 0,
            }],
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
            2,
            "entry = subject row + file row, no body (prefix {prefix:?}): {got:?}"
        );
        assert!(
            got[0].ends_with("· +1 −0"),
            "subject carries the text-line delta (prefix {prefix:?}): {got:?}"
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
            "footer must hang under the commit ID (prefix {prefix:?}): {got:?}"
        );
    }
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
    let lines = Arc::new(Mutex::new(Vec::new()));
    let d = Display::with(Buf {
        colors: false,
        lines: lines.clone(),
    });
    d.commit_line(
        "0875db3",
        "fix(display): align footer",
        "[1/1]",
        &[
            FileStats {
                path: "src/render/display/mod.rs".into(),
                added: 64,
                deleted: 51,
                new: false,
                removed: false,
                binary: false,
                old_size: 0,
                new_size: 0,
            },
            FileStats {
                path: "src/render/display/tests.rs".into(),
                added: 88,
                deleted: 33,
                new: false,
                removed: false,
                binary: false,
                old_size: 0,
                new_size: 0,
            },
        ],
    );
    let got = lines.lock().clone();
    // Region 8 (`+152 −84` sized): block starts at the ✓ column (6 past
    // margin, so `+64` carries 1 pad col), ends 2 short of the message
    // (16), names pin at 16.
    assert_eq!(
        got[0],
        "  [1/1] \u{2713} 0875db3 fix(display): align footer · +152 −84"
    );
    assert_eq!(got[1], "         +64 −51  src/render/display/mod.rs");
    assert_eq!(got[2], "         +88 −33  src/render/display/tests.rs");
    assert_eq!(got[3], "      Σ ██████░░░░ +152 −84 · 2 files");
}

/// The Σ total row hangs under the `✓` marker — exactly `sigma_col` (2)
/// columns left of the file rows' hang under the commit ID — so the Σ
/// glyph lands in the grid column the file rows reserve as blank, and the
/// totals still right-align into the per-file `+N`/`−M` columns.
#[test]
fn commit_footer_sigma_hangs_under_the_tick_marker() {
    let lines = Arc::new(Mutex::new(Vec::new()));
    let d = Display::with(Buf {
        colors: false,
        lines: lines.clone(),
    });
    d.commit_line(
        "abc1234",
        "feat: add thing",
        "[1/2]",
        &[
            FileStats {
                path: "src/a.rs".into(),
                added: 3,
                deleted: 1,
                new: false,
                removed: false,
                binary: false,
                old_size: 0,
                new_size: 0,
            },
            FileStats {
                path: "src/b.rs".into(),
                added: 5,
                deleted: 0,
                new: false,
                removed: false,
                binary: false,
                old_size: 0,
                new_size: 0,
            },
        ],
    );
    let got = lines.lock().clone();
    // `[1/2] ` = 6 cols past the margin: tick at 6, hash at 8, message at
    // 16. Region 5 (`+8 −1` sized): block starts at the hash (8), ends 13,
    // names pin at the message (16) — a 3-col gap — and the Σ glyph hugs
    // the block's left edge at 6, exactly under the ✓.
    assert_eq!(got[0], "  [1/2] \u{2713} abc1234 feat: add thing · +8 −1");
    assert_eq!(got[1], "          +3 −1   src/a.rs");
    assert_eq!(got[2], "          +5      src/b.rs");
    assert_eq!(got[3], "        Σ █████████░ +8 −1 · 2 files");
}

#[test]
fn plain_when_colors_disabled() {
    let lines = Arc::new(Mutex::new(Vec::new()));
    let d = Display::with(Buf {
        colors: false,
        lines: lines.clone(),
    });
    d.commit_line("abc1234", "feat: add thing", "[1/3]", &[]);
    let got = lines.lock().clone();
    // No ANSI escapes; [n/m] prefix retained (not collapsed to "n.").
    // Type prefix "feat" is present, followed by ": add thing". Subject
    // carries the 2-col left margin; empty stats → no delta segment, and
    // the entry is a single row (the body is omitted — ADR 0018 redesign).
    assert_eq!(got.len(), 1);
    assert_eq!(got[0], "  [1/3] \u{2713} abc1234 feat: add thing");
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
    d.commit_line("abc1234", "feat: add thing", "[1/3]", &[]);
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
    d.commit_line("def5678", "fix(auth): correct token check", "", &[]);
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
    d.commit_line("def5678", "fix(auth): correct token check", "", &[]);
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
        d.commit_line("hash000", &format!("{type_str}: msg"), "", &[]);
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
    d.commit_line("ghi9012", "blob: thing in progress", "", &[]);
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
    d.commit_line("jkl3456", "no colon message", "", &[]);
    let joined = lines.lock().join("\n");
    // No-colon messages have no type token to color → muted gray #6b7280
    // (darkened from the old #9ca3af for white-bg readability).
    assert!(
        joined.contains("107;114;128"),
        "no-colon message should be muted gray: {joined:?}"
    );
}

/// Diff bar cell split (v2, spec §3): `add = round(cells · A / T)`, deletes
/// take the rest; single-sign diffs get the full bar; `T == 0` (all-binary)
/// and an empty budget return `(0, 0)` — the caller omits the bar.
#[test]
fn ratio_bar_splits_cells_with_delete_taking_the_remainder() {
    // 12/(12+3) = 0.8 → 8 add cells (the spec's own worked example).
    assert_eq!(ratio_bar(12, 3, 10), (8, 2));
    // Single-sign: full bar in one color.
    assert_eq!(ratio_bar(10, 0, 10), (10, 0));
    assert_eq!(ratio_bar(0, 12, 10), (0, 10));
    // All-binary: no text churn, no bar.
    assert_eq!(ratio_bar(0, 0, 10), (0, 0));
    // Half-away rounding, delete side takes the remainder — never 11 cells.
    assert_eq!(ratio_bar(1, 1, 10), (5, 5));
    assert_eq!(ratio_bar(1, 3, 10), (3, 7));
    assert_eq!(ratio_bar(2, 1, 9), (6, 3));
    // Degenerate budget.
    assert_eq!(ratio_bar(5, 5, 0), (0, 0));
}

/// Byte-size delta formatting (v2, spec §3): sign always present (`−` is
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

/// The Σ bar shrinks before it disappears on narrow terminals (v2, spec §3):
/// a 30-col terminal affords 6 cells, a 26-col one the 4-cell floor, and a
/// 24-col one drops the bar entirely — the counts and `· N files` scope
/// never truncate.
#[test]
fn sigma_bar_shrinks_then_omits_on_narrow_terminals() {
    let stats = |a: usize| {
        vec![
            FileStats {
                path: "a.rs".into(),
                added: a,
                deleted: 0,
                new: false,
                removed: false,
                binary: false,
                old_size: 0,
                new_size: 0,
            },
            FileStats {
                path: "b.rs".into(),
                added: 0,
                deleted: 0,
                new: false,
                removed: false,
                binary: true,
                old_size: 0,
                new_size: 512,
            },
        ]
    };
    for (cols, want_bar) in [(30, " ██████ "), (28, " ████ "), (24, " ")] {
        let lines = Arc::new(Mutex::new(Vec::new()));
        let d = Display::with_cols(
            Buf {
                colors: false,
                lines: lines.clone(),
            },
            cols,
        );
        d.emit_file_stats(
            &stats(10),
            &SubjectOffsets {
                tick: 0,
                hash: 0,
                message: 2,
            },
            d.text_width(),
        );
        let got = lines.lock().clone();
        let sigma = got.last().expect("Σ row present");
        let want = format!("  Σ{want_bar}+10 −0 · 2 files");
        assert_eq!(sigma.trim_end(), want, "cols {cols}: {got:?}");
    }
}
