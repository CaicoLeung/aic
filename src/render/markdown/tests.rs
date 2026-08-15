use super::*;

/// [`reasoning_rows`] with `Some(elapsed)` appends a rising seconds
/// count to the spinner row, so the wait stays visible while the model
/// is silent between deltas (the steady-tick repaint path). The window
/// rows are unchanged.
#[test]
fn reasoning_rows_spinner_shows_elapsed_when_ticking() {
    let rows = reasoning_rows(
        "⠙",
        "Analyzing",
        &["a line".to_string()],
        80,
        Some(Duration::from_secs(7)),
        12,
        false,
    );
    assert_eq!(rows.first(), Some(&format!("{MARGIN}⠙ Analyzing… 7s")));
    assert_eq!(rows[1], format!("{MARGIN}│ a line"));
}

/// [`reasoning_rows`] always leads with the spinner+label row, even when
/// the window is empty (the stream just started).
#[test]
fn reasoning_rows_leads_with_spinner_for_empty_window() {
    let rows = reasoning_rows("⠋", "Analyzing", &[], 80, None, 12, false);
    assert_eq!(rows, vec![format!("{MARGIN}⠋ Analyzing")]);
}

/// Each retained line becomes one indented row when it fits the budget.
#[test]
fn reasoning_rows_indents_each_line() {
    let window = vec!["line 1".to_string(), "line 2".to_string()];
    let rows = reasoning_rows("⠙", "Analyzing", &window, 80, None, 12, false);
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
    let rows = reasoning_rows(
        "⠹",
        "Analyzing",
        &["the quick brown fox".to_string()],
        10,
        None,
        12,
        false,
    );
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
/// the row budget, so a window whose lines wrap to more rows than
/// the budget shows only the newest rows — the oldest wrap pieces (and
/// whole lines) roll out, the spinner row is never dropped.
#[test]
fn reasoning_rows_caps_rendered_rows_to_window_budget() {
    let prefix = format!("{MARGIN}│ ");
    // 12 lines × 2 wrap pieces = 24 rendered rows, over the 12-row budget.
    let window: Vec<String> = (1..=12).map(|i| format!("line {i} with words")).collect();
    let rows = reasoning_rows("⠹", "Analyzing", &window, 10, None, 12, false);
    assert_eq!(rows.len(), 13);
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
/// grow the region: only the newest max_rows rendered rows are
/// kept, the top row a mid-line cut like a terminal tail window.
#[test]
fn reasoning_rows_caps_single_long_line() {
    let prefix = format!("{MARGIN}│ ");
    let long = "word ".repeat(60); // 300 chars → 30 wrap pieces at width 10
    let rows = reasoning_rows("⠹", "Analyzing", &[long], 10, None, 12, false);
    assert_eq!(rows.len(), 13);
    assert_eq!(rows.first(), Some(&format!("{MARGIN}⠹ Analyzing")));
    // 12 rows of "word word" = 24 words of the 60 — the newest tail only.
    let words: usize = rows[1..]
        .iter()
        .map(|r| r.strip_prefix(&prefix).unwrap().split_whitespace().count())
        .sum();
    assert_eq!(words, 24);
}

/// Plain prose classifies as Normal, leaving the text untouched.
#[test]
fn classify_plain_prose_is_normal() {
    let (kind, text, _) = classify_line("just thinking out loud", false);
    assert_eq!(kind, LineKind::Normal);
    assert_eq!(text, "just thinking out loud");
}

/// An ATX heading (`#` + space, or bare hashes at end of line) classifies
/// as Heading with the markers stripped — the "bold titles". The space or
/// end-of-line requirement keeps `#include`, shebangs, and `#tag` out of
/// the heading kind.
#[test]
fn classify_atx_heading_strips_markers() {
    let (kind, text, _) = classify_line("## Plan", false);
    assert_eq!(kind, LineKind::Heading);
    assert_eq!(text, "Plan");
    // CommonMark: `#` alone at end of line is still a heading.
    assert_eq!(classify_line("#", false).0, LineKind::Heading);
    assert_eq!(classify_line("######", false).0, LineKind::Heading);
    // No space and content follows: not a heading.
    assert_eq!(
        classify_line("#include <stdio.h>", false).0,
        LineKind::Normal
    );
    assert_eq!(classify_line("#!/bin/bash", false).0, LineKind::Normal);
    assert_eq!(classify_line("#tag", false).0, LineKind::Normal);
    // Seven hashes: past the ATX limit, not a heading.
    assert_eq!(classify_line("####### deep", false).0, LineKind::Normal);
}

/// A fenced code block toggles Code state across its lines: the fence line
/// is CodeFence, content lines are Code, and state closes on the matching
/// fence. Both ``` and ~~~ open/close.
#[test]
fn classify_fence_toggles_code_state() {
    let (k0, _, s0) = classify_line("```rust", false);
    assert_eq!(k0, LineKind::CodeFence);
    assert!(s0);
    let (k1, _, s1) = classify_line("let x = 1;", s0);
    assert_eq!(k1, LineKind::Code);
    assert!(s1);
    let (k2, _, s2) = classify_line("```", s1);
    assert_eq!(k2, LineKind::CodeFence);
    assert!(!s2);
    // Back to Normal after the closer; tilde fences open too.
    assert_eq!(classify_line("done", s2).0, LineKind::Normal);
    assert!(classify_line("~~~", false).2);
}

/// An unclosed fence (stream ended mid-block) leaves subsequent lines as
/// Code — matching "render as code" for a half-written block.
#[test]
fn classify_unclosed_fence_keeps_code() {
    let (_, _, in_code) = classify_line("```python", false);
    assert_eq!(classify_line("def f():", in_code).0, LineKind::Code);
}

/// A list-item marker classifies as ListItem: unordered markers (`-`,
/// `*`, `+`) render with a `•` bullet, ordered markers (`1.`, `12)`)
/// keep their number. The required trailing space keeps `*bold*` and a
/// bare `-` in Normal.
#[test]
fn classify_list_item_replaces_marker() {
    let (kind, text, _) = classify_line("- first", false);
    assert_eq!(kind, LineKind::ListItem);
    assert_eq!(text, "• first");
    assert_eq!(classify_line("* second", false).1, "• second");
    assert_eq!(classify_line("+ third", false).1, "• third");
    // ordered keeps its marker (the number is semantic)
    let (k, t, _) = classify_line("1. step", false);
    assert_eq!(k, LineKind::ListItem);
    assert_eq!(t, "1. step");
    assert_eq!(classify_line("12) big", false).1, "12) big");
    // no trailing space, or a bare marker → not a list
    assert_eq!(classify_line("*bold*", false).0, LineKind::Normal);
    assert_eq!(classify_line("-", false).0, LineKind::Normal);
}

/// A `>` blockquote classifies as Blockquote with the markers (and
/// nesting) stripped. A leading `>` with no following space still quotes.
#[test]
fn classify_blockquote_strips_markers() {
    let (kind, text, _) = classify_line("> quoted text", false);
    assert_eq!(kind, LineKind::Blockquote);
    assert_eq!(text, "quoted text");
    assert_eq!(classify_line(">>nested", false).1, "nested");
    assert_eq!(classify_line("> > deep", false).1, "deep");
}

/// [`reasoning_rows`] classifies markdown lines and still honours the
/// rendered-row cap + spinner row. Asserts only length and the spinner
/// (ANSI is environment-dependent on the test process); kind correctness
/// lives in the classify_* tests above.
#[test]
fn reasoning_rows_handles_markdown_lines_and_caps() {
    let window = vec![
        "# Heading".to_string(),
        "```rs".to_string(),
        "let x = 2;".to_string(),
        "```".to_string(),
    ];
    let rows = reasoning_rows("⠹", "Analyzing", &window, 80, None, 2, false);
    assert_eq!(rows.len(), 3); // spinner + newest 2 rows
    assert_eq!(rows.first(), Some(&format!("{MARGIN}⠹ Analyzing")));
}

/// The style-emission path is live: a bold style actually emits ANSI when
/// forced (in the real run `paint`'s `is_term` guard gates emission; this
/// forces it so the test process — not a TTY — still observes the escape).
/// Guards the *apply* half of styling; the per-kind routing is covered by
/// [`kind_style_routes_each_line_kind`].
#[test]
fn styling_path_emits_ansi_when_forced() {
    let s = Style::new().bold().force_styling(true);
    let styled = s.apply_to("hi").to_string();
    assert!(
        styled.contains("\x1b["),
        "no ANSI escape emitted: {styled:?}"
    );
}

/// [`kind_style`] routes each [`LineKind`] to its style: every content kind
/// except `Normal` and `ListItem` (whose `•` bullet is its own signal)
/// selects a style. Catches a routing regression — e.g. `Code` returning
/// `None` — that would silently strip styling from a whole line kind, which
/// the TTY-gated emission test above cannot see (ANSI is stripped in a
/// non-TTY test run).
#[test]
fn kind_style_routes_each_line_kind() {
    assert!(kind_style(LineKind::Normal).is_none());
    assert!(kind_style(LineKind::Heading).is_some());
    assert!(kind_style(LineKind::Code).is_some());
    assert!(kind_style(LineKind::CodeFence).is_some());
    assert!(kind_style(LineKind::ListItem).is_none());
    assert!(kind_style(LineKind::Blockquote).is_some());
}

/// [`style_kind`] leaves plain reasoning lines untouched: `Normal`
/// short-circuits before any [`Style`], so unstyled prose round-trips
/// verbatim and is never accidentally bolded or coloured.
#[test]
fn style_kind_normal_is_verbatim_passthrough() {
    assert_eq!(
        style_kind("plain reasoning", LineKind::Normal),
        "plain reasoning"
    );
}

/// [`parse_inline`] leaves unmarked prose as one [`Span::Plain`] segment.
#[test]
fn parse_inline_plain_is_one_segment() {
    assert_eq!(
        parse_inline("just text"),
        vec![("just text".to_string(), Span::Plain)]
    );
}

/// `**bold**` and `` `code` `` become their own segments; surrounding text
/// stays Plain. The markers are consumed, not kept in the text.
#[test]
fn parse_inline_bold_and_code_segments() {
    assert_eq!(
        parse_inline("a **b** `c` d"),
        vec![
            ("a ".to_string(), Span::Plain),
            ("b".to_string(), Span::Bold),
            (" ".to_string(), Span::Plain),
            ("c".to_string(), Span::Code),
            (" d".to_string(), Span::Plain),
        ]
    );
}

/// Optimistic (accepted trade-off): an opener with no matching closer on
/// the line is rendered styled anyway, so a half-streamed `**bold` shows
/// bold and `` `code `` shows code before the closer arrives — the partial
/// line is re-parsed next delta and self-corrects. This is streamdown-parser's
/// natural behaviour (stateful toggle + end-of-line flush), chosen over
/// literal-until-closed to reuse the library verbatim.
#[test]
fn parse_inline_unclosed_opener_is_optimistic() {
    assert_eq!(
        parse_inline("**bold"),
        vec![("bold".to_string(), Span::Bold)]
    );
    assert_eq!(
        parse_inline("`code"),
        vec![("code".to_string(), Span::Code)]
    );
}

/// Code spans are atomic: `` `a **b** c` `` is one Code segment whose
/// backticked content is not re-scanned for `**`.
#[test]
fn parse_inline_code_span_is_atomic() {
    assert_eq!(
        parse_inline("`a **b** c`"),
        vec![("a **b** c".to_string(), Span::Code)]
    );
}

/// [`span_style`] routes each [`Span`]: Bold and Code select a style, Plain
/// selects none (mirrors [`kind_style`]'s routing test).
#[test]
fn span_style_routes_each_span() {
    assert!(span_style(Span::Plain).is_none());
    assert!(span_style(Span::Bold).is_some());
    assert!(span_style(Span::Code).is_some());
}

/// [`wrap_styled`] keeps [`wrap_line`]'s greedy contract: a long line wraps
/// to several rows whose words rejoin losslessly.
#[test]
fn wrap_styled_preserves_words_losslessly() {
    let rows = wrap_inline(&parse_inline("the quick brown fox"), 10, None);
    assert_eq!(rows.join(" "), "the quick brown fox");
}

/// The re-open invariant (Q3=A): a bold span wider than the wrap budget
/// splits across rows, and each row re-opens bold — a row beginning
/// mid-span styles identically to one bold from its own start, so a wrapped
/// `**bold**` run stays coloured on every row. Colors are forced because
/// `console` otherwise strips ANSI off a TTY-less test process; this is
/// safe because Plain/Normal prose never styles, so the other tests' exact
/// byte assertions are unaffected.
#[test]
fn wrap_styled_reopens_span_across_wrap_break() {
    console::set_colors_enabled(true);
    // One bold word at width 4 → three wrapped rows, all beginning mid-bold.
    let rows = wrap_inline(&parse_inline("**abcdefghij**"), 4, None);
    assert!(rows.len() >= 2);
    for r in &rows {
        assert!(r.contains("\x1b["), "row lost its bold re-open: {r:?}");
    }
    // Re-open: a row beginning mid-bold still opens bold. render_row resets
    // its span at every row, so a span split by a wrap break is re-opened,
    // not carried — this is exactly why each wrapped bold row is coloured.
    assert!(
        render_runs(&[('a', Span::Bold)], &|sp| resolve_style(*sp, None)).starts_with("\x1b[1m")
    );
}

/// The load-bearing ANSI-blind property (ADR 0013): a bold line wrapped
/// narrow keeps every row's *visible* width within the budget — the wrap
/// counted plain chars (via `layout::wrap_words`) and applied styling
/// after, so escape bytes never inflated a row. This is the direct check
/// the other wrap tests assert only indirectly.
#[test]
fn wrap_runs_visible_width_stays_within_budget() {
    console::set_colors_enabled(true);
    for budget in [4, 8, 12] {
        let rows = wrap_inline(
            &parse_inline("**bold one two three four five six seven**"),
            budget,
            None,
        );
        assert!(!rows.is_empty());
        for r in &rows {
            let vis = console::strip_ansi_codes(r).chars().count();
            assert!(
                vis <= budget,
                "budget {budget}: visible width {vis} > budget: {r:?}"
            );
        }
    }
}

/// [`render_runs`] coalesces consecutive same-tag chars into one styled run
/// and re-opens the ANSI at a tag change — the isolated behaviour the wrap
/// re-open relies on. Two bold runs split by a plain space open bold twice.
#[test]
fn render_runs_coalesces_runs_and_reopens_mid_tag() {
    console::set_colors_enabled(true);
    let s = render_runs(
        &[
            ('a', Span::Bold),
            ('b', Span::Bold),
            (' ', Span::Plain),
            ('c', Span::Bold),
        ],
        &|sp| resolve_style(*sp, None),
    );
    assert_eq!(s.matches("\x1b[1m").count(), 2, "two bold re-opens: {s:?}");
    assert!(s.contains("ab") && s.contains('c'));
}

/// List items, blockquotes, and headings carry inline `**bold**` and must
/// parse it (strip the asterisks) like Normal prose — the bug that left
/// `**` visible in every non-Normal prose kind.
#[test]
fn reasoning_rows_strips_bold_in_prose_kinds() {
    let window = vec![
        "# **Heading**".to_string(),
        "- **bold** item".to_string(),
        "> **bold** quote".to_string(),
        "plain **bold** text".to_string(),
    ];
    let rows = reasoning_rows("⠹", "thinking", &window, 80, None, 20, false);
    let joined = rows.join("\n");
    assert!(
        !joined.contains("**"),
        "literal markdown asterisks survived in a prose kind: {joined:?}"
    );
    // the word itself survives — only its asterisks were stripped
    assert!(joined.contains("bold"), "bold text was dropped: {joined:?}");
}

/// `resolve_style` is the layering rule: Plain inherits the line-kind base,
/// Bold/Code override it. A blockquote is dim, so a plain run on it stays
/// dim while a bold run drops the dim for full bold.
#[test]
fn resolve_style_plain_inherits_base_bold_overrides() {
    let dim = Style::new().dim();
    assert_eq!(resolve_style(Span::Plain, Some(&dim)), Some(dim.clone()));
    assert_eq!(
        resolve_style(Span::Bold, Some(&dim)),
        Some(Style::new().bold())
    );
    assert_eq!(resolve_style(Span::Plain, None), None);
}

/// [`loading_rows`] before the grace deadline: just the spinner row with
/// the elapsed count, no notice — a slow-to-first-token backend is never
/// falsely labeled.
#[test]
fn loading_rows_pre_grace_is_spinner_plus_elapsed_only() {
    let rows = loading_rows(
        "⠋",
        "Analyzing changes",
        Duration::from_secs(3),
        LoadingNotice::None,
        120,
    );
    assert_eq!(rows, vec![format!("{MARGIN}⠋ Analyzing changes… 3s")]);
}

/// [`loading_rows`] past the grace deadline for a plain backend adds the
/// [`SILENT_NOTICE`] as an indented body row, mirroring a reasoning
/// frame's shape so the in-place repaint hands off cleanly later.
#[test]
fn loading_rows_post_grace_silent_adds_notice() {
    let rows = loading_rows(
        "⠙",
        "Analyzing changes",
        Duration::from_secs(8),
        LoadingNotice::Silent,
        120,
    );
    assert_eq!(
        rows,
        vec![
            format!("{MARGIN}⠙ Analyzing changes… 8s"),
            format!("{MARGIN}│ {SILENT_NOTICE}"),
        ]
    );
}

/// [`loading_rows`] past the grace deadline for a streaming-capable
/// backend adds the cold-start notice built from the carried program name
/// — a cold start is not a capability gap, so it must not be labeled
/// non-streaming. The program name interpolates, so `pi`/`claude`/custom
/// backends are each labeled by their own command, never hardcoded
/// "Claude".
#[test]
fn loading_rows_post_grace_cold_start_adds_notice() {
    let rows = loading_rows(
        "⠹",
        "Analyzing changes",
        Duration::from_secs(7),
        LoadingNotice::ColdStart("claude".to_string()),
        160,
    );
    let expected = cold_start_notice("claude");
    assert_eq!(
        rows,
        vec![
            format!("{MARGIN}⠹ Analyzing changes… 7s"),
            format!("{MARGIN}│ {expected}"),
        ]
    );
}

/// The notice wraps under the same `│ ` indent as reasoning rows once it
/// exceeds the budget, so a narrow terminal cannot overflow the frame.
#[test]
fn loading_rows_wraps_notice_to_budget() {
    let rows = loading_rows(
        "⠹",
        "Analyzing changes",
        Duration::from_secs(9),
        LoadingNotice::Silent,
        12,
    );
    assert_eq!(
        rows.first(),
        Some(&format!("{MARGIN}⠹ Analyzing changes… 9s"))
    );
    // every body row is indented and the wrapped pieces rejoin losslessly.
    let prefix = format!("{MARGIN}│ ");
    let body: String = rows[1..]
        .iter()
        .filter_map(|r| r.strip_prefix(&prefix))
        .collect::<Vec<_>>()
        .join(" ");
    assert_eq!(body, SILENT_NOTICE);
}

/// [`reasoning_rows`] honours the stream's fence state entering the
/// window: mid-block content stays Code and a closer closes — the
/// classification a full-stream scan would give, regardless of how much
/// of the block scrolled out of the window.
#[test]
fn reasoning_rows_classifies_with_running_fence_state() {
    let prefix = format!("{MARGIN}│ ");
    // Window whose opener is gone: [code, closer, prose] with entering
    // state `true`. The closer must close; prose must be Normal.
    let window = vec![
        "let x = 1;".to_string(),
        "```".to_string(),
        "done".to_string(),
    ];
    let rows = reasoning_rows("⠹", "Analyzing", &window, 80, None, 12, true);
    assert_eq!(rows.first(), Some(&format!("{MARGIN}⠹ Analyzing")));
    let body: Vec<String> = rows[1..]
        .iter()
        .map(|r| console::strip_ansi_codes(r.strip_prefix(&prefix).unwrap()).to_string())
        .collect();
    assert_eq!(body, vec!["  let x = 1;", "```", "done"]);
    // And a window that starts mid-block with `false` (no opener at all,
    // e.g. the stream genuinely began outside a block) classifies the
    // first fence as an opener.
    let rows = reasoning_rows("⠹", "Analyzing", &window, 80, None, 12, false);
    assert_eq!(rows.len(), 4, "spinner + 3 rows survive the cap");
}

/// A code block body is indented two columns, the fence stays flush, and the
/// total row width never exceeds the prose budget (the body wraps to
/// `feed_width - 2` so the indent is "free").
#[test]
fn reasoning_rows_indents_code_body_keeps_fence_flush() {
    let prefix = format!("{MARGIN}│ ");
    let window = vec![
        "```rust".to_string(),
        "let x = 1;".to_string(),
        "```".to_string(),
    ];
    let rows = reasoning_rows("⠹", "thinking", &window, 20, None, 10, false);
    // strip ANSI so the assertions are about geometry (indent + overflow),
    // not whichever global colour state another parallel test raced in.
    let body: Vec<String> = rows[1..]
        .iter()
        .map(|r| console::strip_ansi_codes(r.strip_prefix(&prefix).unwrap()).to_string())
        .collect();
    // fence flush, code body indented two
    assert_eq!(body[0], "```rust");
    assert_eq!(body[1], "  let x = 1;");
    assert_eq!(body[2], "```");
    for r in &rows {
        assert!(
            console::strip_ansi_codes(r).chars().count() <= 24,
            "indented code row overflowed: {:?}",
            r
        );
    }
}

#[test]
fn fence_lang_extracts_token_and_handles_closers() {
    assert_eq!(fence_lang("```rust"), Some("rust"));
    assert_eq!(fence_lang("~~~python"), Some("python"));
    assert_eq!(fence_lang("```"), None);
    assert_eq!(fence_lang("```  js  "), Some("js"));
}

#[test]
fn reasoning_rows_highlights_tagged_code_with_syntect() {
    console::set_colors_enabled(true);
    // A ```rust block whose opener is in view is highlighted per token:
    // syntect emits TrueColor (38;2;r;g;b) regions, distinct from the
    // uniform cyan (36) fallback used when no language is known.
    let rows = reasoning_rows(
        "⠹",
        "thinking",
        &[
            "```rust".to_string(),
            "let x: i32 = 1;".to_string(),
            "```".to_string(),
        ],
        40,
        None,
        10,
        false,
    );
    let code_row = rows
        .iter()
        .find(|r| console::strip_ansi_codes(r).contains("let x"))
        .expect("code body row present");
    assert!(
        code_row.contains("\x1b[38;2;"),
        "tagged code block was not syntect-highlighted: {code_row:?}"
    );
}

#[test]
fn reasoning_rows_falls_back_to_cyan_without_language() {
    console::set_colors_enabled(true);
    // Bare ``` fence (no language) → uniform cyan, no TrueColor regions,
    // and so does a block whose opener scrolled out (mid-block window).
    let bare = reasoning_rows(
        "⠹",
        "thinking",
        &[
            "```".to_string(),
            "let x = 1;".to_string(),
            "```".to_string(),
        ],
        40,
        None,
        10,
        false,
    );
    let bare_code = bare
        .iter()
        .find(|r| console::strip_ansi_codes(r).contains("let x"))
        .unwrap();
    assert!(
        bare_code.contains("\x1b[36m"),
        "bare fence should be cyan: {bare_code:?}"
    );
    assert!(
        !bare_code.contains("\x1b[38;2;"),
        "bare fence must not highlight: {bare_code:?}"
    );

    // Opener scrolled out: window begins inside a block, so no highlighter
    // exists and the tail is uniform cyan — the documented lazy limit.
    let orphan = reasoning_rows(
        "⠹",
        "thinking",
        &["let y = 2;".to_string()],
        40,
        None,
        10,
        true,
    );
    let orphan_code = orphan
        .iter()
        .find(|r| console::strip_ansi_codes(r).contains("let y"))
        .unwrap();
    assert!(
        orphan_code.contains("\x1b[36m") && !orphan_code.contains("\x1b[38;2;"),
        "scrolled-out opener should fall back to cyan: {orphan_code:?}"
    );
}
