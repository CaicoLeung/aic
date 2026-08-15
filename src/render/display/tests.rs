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
    // name pads to align with src/old.rs (10 chars). The deletion-only file
    // renders no `+0` column (git --stat convention) — its `−12` still ends
    // where the Σ row's `−12` ends.
    assert_eq!(got[0], "  (binary)  img.png    [new]");
    assert_eq!(got[1], "       −12  src/old.rs [del]");
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
    // The zero-deletion column renders blank (git --stat convention) but
    // keeps its reserved width, so the name still lands where the Σ row's
    // `(2 files)` label starts.
    assert_eq!(got[0], "     +5     a.rs");
    assert_eq!(got[1], "     +5     b.rs");
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
    // start at the same column. The text row's zero-deletion column renders
    // blank but keeps its width, so `a.rs` still lands under `x.bin`.
    assert_eq!(got[0], "  (binary)  x.bin");
    assert_eq!(got[1], "     +1     a.rs");
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
