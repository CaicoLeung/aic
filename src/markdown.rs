//! Pure streaming-Markdown painter: line → styled rows.
//!
//! ADR 0013's renderer contract lives here — the line→styled-rows function
//! family ([`reasoning_rows`], [`loading_rows`]) that turns a partial Markdown
//! reasoning window into ANSI-styled terminal rows. Zero I/O: classification
//! ([`classify_line`]), inline `**bold**`/`` `code` `` parsing, syntect code
//! highlighting, and the generic [`wrap_runs`] wrap engine. The *when-to-paint*
//! policy is [`crate::reasoning_feed`]; the terminal surface that frames these
//! rows is [`crate::progress::ReasoningRenderer`].

use console::{Color, Style};
use std::sync::LazyLock;
use std::time::Duration;
use syntect::easy::HighlightLines;
use syntect::highlighting::{self as hl, ThemeSet};
use syntect::parsing::SyntaxSet;

use crate::layout::{MARGIN, wrap_line, wrap_words};

/// Markdown render kind for one reasoning line. Streaming markdown can't be
/// fully parsed (the input is partial), so classification is line-local with a
/// running fence state — robust where it matters (headings, fenced code
/// blocks). The state is carried across the whole stream by
/// [`crate::progress::ThinkingView`], so a code block whose opener has already scrolled out of
/// the window still colours its content correctly, and a closer after a long
/// block still closes it. Inline `**bold**` / `` `code` `` *are* parsed for
/// [`LineKind::Normal`] prose (see [`parse_inline`] + [`wrap_styled`]), which
/// re-opens a span that a wrap break split across two rows; other kinds stay
/// on the wrap-safe single-style-per-line path.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum LineKind {
    /// Plain reasoning prose — no styling.
    Normal,
    /// An ATX heading (`#`..`######` + space) — the "bold titles". Rendered
    /// bold, with the leading `#` markers stripped.
    Heading,
    /// A line inside a fenced code block — the "code blocks". Rendered in a
    /// distinct colour so code reads as code, like the coding-agent UIs aic
    /// mirrors.
    Code,
    /// A ``` / ~~~ fence line itself — rendered dim so it reads as a delimiter
    /// rather than content.
    CodeFence,
    /// A list item (`- ` / `* ` / `+ ` / `1. `) — [`list_item_body`] has
    /// already replaced the unordered marker with a `•` bullet (and kept the
    /// ordered marker), so the kind needs no style of its own: the bullet is
    /// the visual signal. A wrapped item uses the shared indent (no hanging
    /// indent) — the window is transient and erased, so list alignment is not
    /// worth per-line wrap machinery.
    ListItem,
    /// A `>` blockquote — rendered dim, with the `>` markers (and nesting)
    /// stripped by [`strip_blockquote`].
    Blockquote,
}

/// Whether `s` is an ATX heading opener: one to six `#`, then end-of-line or
/// a space (CommonMark — `#` alone is a heading). The space requirement (or
/// end of line) keeps `#include`, `#!/bin/bash`, and `#tag` out of the
/// heading kind — only `# Title`-shaped lines qualify.
fn is_atx_heading(s: &str) -> bool {
    let hashes = s.bytes().take_while(|&b| b == b'#').count();
    matches!(hashes, 1..=6) && matches!(s.as_bytes().get(hashes), None | Some(&b' '))
}

/// Classify one reasoning line for rendering, given whether the running scan
/// is already inside a fenced code block. Returns the line's kind, the text to
/// wrap (headings have their `#` markers stripped), and the fence state to
/// carry into the next line. Pure (no I/O) so classification is unit-testable
/// independently of the ANSI styling.
pub(crate) fn classify_line(line: &str, in_code: bool) -> (LineKind, String, bool) {
    let trimmed = line.trim_start();
    if trimmed.starts_with("```") || trimmed.starts_with("~~~") {
        (LineKind::CodeFence, trimmed.to_string(), !in_code)
    } else if in_code {
        (LineKind::Code, line.to_string(), in_code)
    } else if is_atx_heading(trimmed) {
        let body = trimmed.trim_start_matches('#').trim_start();
        (LineKind::Heading, body.to_string(), in_code)
    } else if let Some(body) = list_item_body(trimmed) {
        (LineKind::ListItem, body, in_code)
    } else if is_blockquote(trimmed) {
        (LineKind::Blockquote, strip_blockquote(trimmed), in_code)
    } else {
        (LineKind::Normal, line.to_string(), in_code)
    }
}

/// A list-item marker at the start of a line: `-`, `*`, or `+` followed by a
/// space (bullet), or one-to-nine ASCII digits followed by `.` or `)` and a
/// space (ordered). The required trailing space keeps `*bold*` (no space) and
/// a bare `-` out of the list kind. Returns the rendered display string — a
/// `•` bullet for unordered items, the original `N.`/`N)` marker kept for
/// ordered ones (the number is semantic) — so the marker renders as a list
/// without the renderer needing a per-kind prefix. `None` for non-list lines.
fn list_item_body(s: &str) -> Option<String> {
    let b = s.as_bytes();
    // Unordered: marker + space.
    if b.len() >= 2 && matches!(b[0], b'-' | b'*' | b'+') && b[1] == b' ' {
        return Some(format!("• {}", &s[2..]));
    }
    // Ordered: digits + '.'|')' + space.
    let n = b.iter().take_while(|&&c| c.is_ascii_digit()).count();
    if (1..=9).contains(&n)
        && matches!(b.get(n), Some(&b'.') | Some(&b')'))
        && b.get(n + 1) == Some(&b' ')
    {
        let marker = &s[..n + 1]; // "1." or "12)"
        let body = &s[n + 2..];
        return Some(format!("{marker} {body}"));
    }
    None
}

/// A blockquote line: the first char is `>` (CommonMark allows `>` with or
/// without a following space, and `>>` nesting). Generous on purpose — a
/// leading `>` in reasoning prose is a quote, not a comparison (which would
/// not sit at the start of a trimmed line).
fn is_blockquote(s: &str) -> bool {
    s.starts_with('>')
}

/// Strip a blockquote's leading `>` markers and the spaces between them, so
/// `> text`, `>>nested`, and `> > spaced` all render their content under the
/// shared indent. Nested quotes collapse to one level — the transient window
/// does not model quote depth.
fn strip_blockquote(s: &str) -> String {
    s.trim_start_matches(['>', ' ']).to_string()
}

/// The [`Style`] for a [`LineKind`], or `None` for the plain passthrough
/// ([`LineKind::Normal`]). Pure routing — split from [`style_kind`] so the
/// per-kind mapping is unit-testable independently of TTY-gated ANSI emission.
fn kind_style(kind: LineKind) -> Option<Style> {
    match kind {
        LineKind::Normal => None,
        LineKind::Heading => Some(Style::new().bold()),
        LineKind::Code => Some(Style::new().fg(Color::Cyan)),
        LineKind::CodeFence => Some(Style::new().dim()),
        LineKind::ListItem => None,
        LineKind::Blockquote => Some(Style::new().dim()),
    }
}

/// Apply the kind's ANSI style to an already-wrapped piece. `Normal` is a
/// no-op (plain text); the others go through `console`, which strips the ANSI
/// on a non-TTY so piped output stays clean. Styling is applied *after* wrap
/// so the width math in [`crate::layout::wrap_line`] never counts escape bytes.
fn style_kind(piece: &str, kind: LineKind) -> String {
    match kind_style(kind) {
        Some(style) => style.apply_to(piece).to_string(),
        None => piece.to_string(),
    }
}

/// Inline markdown span within a single reasoning line. Parsed line-locally
/// (never crosses `\n` — callers feed one line at a time) and only for
/// [`LineKind::Normal`] prose: headings are already bold, and fenced code is
/// raw. Styled: `**bold**`/`***bold italic***` (bold), `` `code` `` (cyan);
/// italic, underline, strike and footnote superscripts are parsed but shown
/// plain, and link/image URLs are dropped — the transient window shows text.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Span {
    /// Unstyled run — Normal prose has no line-kind style to inherit.
    Plain,
    /// `**bold**`.
    Bold,
    /// `` `code` `` — the same cyan as a fenced block, so inline and block
    /// code read the same.
    Code,
}

/// The [`Style`] for an inline [`Span`], or `None` for [`Span::Plain`]. Mirrors
/// [`kind_style`]'s split so the routing is unit-testable apart from emission.
fn span_style(sp: Span) -> Option<Style> {
    match sp {
        Span::Plain => None,
        Span::Bold => Some(Style::new().bold()),
        Span::Code => Some(Style::new().fg(Color::Cyan)),
    }
}

/// Resolve the effective [`Style`] for a span on a line whose [`Span::Plain`]
/// runs carry `base` — the line-kind style (dim for a blockquote, bold for a
/// heading, `None` for Normal/list). `Bold`/`Code` override the base with their
/// own span style so emphasis stands out; `Plain` inherits the base, so a
/// blockquote stays dim between its bold spans and a heading stays bold around
/// a code span.
fn resolve_style(sp: Span, base: Option<&Style>) -> Option<Style> {
    match sp {
        Span::Plain => base.cloned(),
        Span::Bold | Span::Code => span_style(sp),
    }
}

/// Apply a style via `console`, which strips the ANSI on a non-TTY so piped
/// output stays clean; `None` is a verbatim passthrough. Per-run (not per-row)
/// granularity is what lets [`wrap_styled`] re-open a span that a wrap break
/// split across two rows.
fn paint(text: &str, style: Option<Style>) -> String {
    match style {
        Some(s) => s.apply_to(text).to_string(),
        None => text.to_string(),
    }
}

/// Parse inline markdown in a single reasoning line via `streamdown_parser`
/// and map it to aic's [`Span`] stream for [`wrap_styled`]. A fresh
/// `InlineParser` is built per call; streamdown resets its formatting state at
/// the end of each `parse()`, so emphasis is line-local (it never crosses
/// `\n`), and an unclosed opener is rendered **optimistically** — a
/// half-streamed `**bold` shows bold before the closer arrives (the partial
/// line is re-parsed next delta, so it self-corrects). aic keeps the
/// wrap-reopen work in [`wrap_styled`]: streamdown parses, aic wraps.
///
/// Mapping: `Bold`/`BoldItalic` → [`Span::Bold`], `Code` → [`Span::Code`];
/// italic, underline, strikeout and footnote superscripts collapse to plain
/// (terminal italic is unreliable and aic committed to bold + code), and
/// link/image URLs are dropped — the transient window shows the text, not the
/// target. Returns ordered `(text, Span)` segments; never empty.
fn parse_inline(line: &str) -> Vec<(String, Span)> {
    use streamdown_parser::InlineElement;
    let mut parser = streamdown_parser::InlineParser::new();
    let mut segs: Vec<(String, Span)> = parser
        .parse(line)
        .into_iter()
        .map(|el| match el {
            InlineElement::Bold(s) | InlineElement::BoldItalic(s) => (s, Span::Bold),
            InlineElement::Code(s) => (s, Span::Code),
            InlineElement::Text(s)
            | InlineElement::Italic(s)
            | InlineElement::Underline(s)
            | InlineElement::Strikeout(s)
            | InlineElement::Footnote(s) => (s, Span::Plain),
            InlineElement::Link { text, .. } => (text, Span::Plain),
            InlineElement::Image { alt, .. } => (alt, Span::Plain),
        })
        .collect();
    if segs.is_empty() {
        segs.push((String::new(), Span::Plain));
    }
    segs
}

/// Greedy word-wrap of a tagged line to `width` display columns (counted in
/// `char`s, CJK-safe), re-opening each tag's ANSI at the start of any row that
/// begins mid-tag and letting `console` close it at the row end. Width math
/// stays on plain `char`s: the break geometry is delegated to
/// [`crate::layout::wrap_words`] (shared with [`crate::layout::wrap_line`], so
/// a fix to the greedy/hard-break policy reaches both — ADR 0013), and ANSI is
/// emitted only by [`render_runs`] *after* the breaks are chosen, so escape
/// bytes never count toward width.
///
/// `style_of` turns a tag into its [`Style`] (or `None` for a plain run); `plain`
/// is the tag for the single inter-word space a wrap break inserts. The
/// re-open property falls out of rendering each row independently: a row
/// starting mid-tag re-opens that tag's ANSI.
fn wrap_runs<T: PartialEq + Clone>(
    segments: &[(String, T)],
    width: usize,
    plain: T,
    style_of: impl Fn(&T) -> Option<Style>,
) -> Vec<String> {
    if width == 0 {
        return vec![render_runs(&flat_runs(segments), &style_of)];
    }
    let words = tokenize_runs(segments);
    wrap_words(&words, width)
        .into_iter()
        .map(|row| {
            let mut chars: Vec<(char, T)> = Vec::new();
            for (i, slice) in row.iter().enumerate() {
                if i > 0 {
                    chars.push((' ', plain.clone()));
                }
                chars.extend(slice.iter().cloned());
            }
            render_runs(&chars, &style_of)
        })
        .collect()
}

/// The inline-prose wrapper around [`wrap_runs`]: tags are [`Span`]s, the
/// inter-word space is [`Span::Plain`], and each span's style is resolved
/// against `base` (the line-kind style that [`Span::Plain`] inherits) via
/// [`resolve_style`].
fn wrap_inline(segments: &[(String, Span)], width: usize, base: Option<Style>) -> Vec<String> {
    wrap_runs(segments, width, Span::Plain, move |sp| {
        resolve_style(*sp, base.as_ref())
    })
}

/// Flatten `(text, tag)` segments into the `(char, tag)` stream the wrap packs
/// and [`render_runs`] paints.
fn flat_runs<T: Clone>(segments: &[(String, T)]) -> Vec<(char, T)> {
    segments
        .iter()
        .flat_map(|(s, t)| s.chars().map(|c| (c, t.clone())))
        .collect()
}

/// Split a `(char, tag)` stream into whitespace-delimited words (any run of
/// whitespace separates, mirroring [`crate::layout::wrap_line`]'s
/// `split_whitespace`), each word carrying its chars' tags.
fn tokenize_runs<T: Clone>(segments: &[(String, T)]) -> Vec<Vec<(char, T)>> {
    let mut out = Vec::new();
    let mut cur = Vec::new();
    for (c, t) in flat_runs(segments) {
        if c.is_whitespace() {
            if !cur.is_empty() {
                out.push(std::mem::take(&mut cur));
            }
        } else {
            cur.push((c, t));
        }
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

/// Render one wrapped row of `(char, tag)` pairs to an ANSI-styled string:
/// consecutive same-tag chars become one styled run via [`paint`] (the style
/// from `style_of`), so a row starting mid-tag re-opens that tag's ANSI and the
/// row ends clean.
fn render_runs<T: PartialEq>(
    chars: &[(char, T)],
    style_of: &impl Fn(&T) -> Option<Style>,
) -> String {
    let mut out = String::new();
    let mut run = String::new();
    let mut cur: Option<&T> = None;
    for (c, t) in chars {
        if cur.is_some_and(|prev| prev != t) && !run.is_empty() {
            out.push_str(&paint(&run, style_of(cur.unwrap())));
            run.clear();
        }
        cur = Some(t);
        run.push(*c);
    }
    if let Some(t) = cur
        && !run.is_empty()
    {
        out.push_str(&paint(&run, style_of(t)));
    }
    out
}

/// Bundled syntax + theme sets, initialised once on first code block (the
/// ~MiB dump decode is a one-time cost paid when reasoning first streams
/// fenced code, never at startup). `base16-eighties.dark` reads on a default
/// terminal and tokens away from the prose colour.
static SYNTAXES: LazyLock<SyntaxSet> = LazyLock::new(SyntaxSet::load_defaults_newlines);
static HIGHLIGHT_THEME: LazyLock<hl::Theme> = LazyLock::new(|| {
    ThemeSet::load_defaults()
        .themes
        .get("base16-eighties.dark")
        .cloned()
        .expect("base16-eighties.dark ships with default-themes")
});

/// The theme's default foreground — regions matching it are left unstyled so
/// plain code text falls back to the terminal default rather than the theme's
/// light grey (which would vanish on a light background). Tokens keep their
/// colour.
fn default_fg() -> Option<hl::Color> {
    HIGHLIGHT_THEME.settings.foreground
}

/// Highlight one code line into `(text, Option<Style>)` runs for [`wrap_runs`]:
/// syntect's per-token foreground colour becomes a `TrueColor` console style,
/// with multi-line string/comment state carried across lines by the borrowed
/// `HighlightLines`. Regions matching the theme default are `None` (plain).
fn highlight_code_line(
    h: &mut HighlightLines,
    line: &str,
    default_fg: Option<hl::Color>,
) -> Vec<(String, Option<Style>)> {
    h.highlight_line(line, &SYNTAXES)
        .unwrap_or_default()
        .into_iter()
        .map(|(st, text)| {
            let style = (Some(st.foreground) != default_fg).then(|| {
                Style::new().fg(Color::TrueColor(
                    st.foreground.r,
                    st.foreground.g,
                    st.foreground.b,
                ))
            });
            (text.to_string(), style)
        })
        .collect()
}

/// The language token from a fence line like ``` ```rust ```, or `None` for a
/// bare ``` ``` ``` / closer. syntect resolves it against its syntax set.
fn fence_lang(fence: &str) -> Option<&str> {
    let lang = fence.trim_start_matches(['`', '~']).trim();
    (!lang.is_empty()).then_some(lang)
}

/// Build the visual rows for one reasoning frame: the spinner+label row on
/// top, then each retained reasoning line greedy-wrapped under the shared
/// `│ ` indent and styled by its markdown kind (bold headings, coloured code
/// blocks) — and every prose kind (Normal/Heading/ListItem/Blockquote) gets
/// inline `**bold**`/`code` spans re-opened across wrap breaks. Pure (no I/O)
/// so the layout is unit-testable; the renderer paints exactly what this returns.
///
/// `feed_width` is the per-piece wrap budget. `max_rows` is the rendered-row
/// cap (from [`crate::cursor::reasoning_window_rows`]); the newest `max_rows` rows below the
/// spinner are kept and the oldest drop out — a long line that wraps to
/// several rows still cannot grow the region, so the top row may start
/// mid-line, like a terminal tail window. An empty `window` yields just the
/// spinner row. `in_code_start` is the markdown fence state entering the
/// window's first line, as tracked by [`crate::progress::ThinkingView`] over the whole stream
/// — never assumed `false` here, or a code block whose opener scrolled out of
/// the window would classify its closer as an opener and colour the following
/// prose as code.
pub(crate) fn reasoning_rows(
    glyph: &str,
    label: &str,
    window: &[String],
    feed_width: usize,
    elapsed: Option<Duration>,
    max_rows: usize,
    in_code_start: bool,
) -> Vec<String> {
    let mut rows = Vec::with_capacity(window.len() + 1);
    // Spinner row: when `elapsed` is `Some` (the live feed is ticking), append
    // a rising seconds count so the wait is visible even while the model is
    // silent between deltas — the same feedback the loading frame gives, kept
    // through the transition into reasoning so the user never loses the clock.
    // `None` is the tests' stable snapshot (no clock).
    let spinner = match elapsed {
        Some(e) => format!("{MARGIN}{glyph} {label}… {}s", e.as_secs()),
        None => format!("{MARGIN}{glyph} {label}"),
    };
    rows.push(spinner);
    // Running fence state, carried from the stream (see the doc) and advanced
    // line-by-line across the window.
    let mut in_code = in_code_start;
    // A per-block highlighter, started on a ```lang opener and dropped on the
    // closer. When the window begins mid-block the opener has scrolled out, so
    // no highlighter exists and the tail renders in the uniform code colour —
    // the known limit of highlighting only blocks whose opener is in view.
    let mut highlighter: Option<HighlightLines> = None;
    let theme_default = default_fg();
    for line in window {
        let was_in_code = in_code;
        let (kind, display, next) = classify_line(line, in_code);
        in_code = next;
        // Prose kinds get inline `**bold**`/`code` parsing (Bold/Code override
        // the line-kind base style Plain inherits); code is raw so its text is
        // never reinterpreted as inline markdown. A fence sits flush; the code
        // body is indented two columns (the wrap budget shrinks by the same
        // two so the total line width is unchanged).
        if kind == LineKind::CodeFence {
            highlighter = if was_in_code {
                None // closer
            } else {
                // opener: highlight for the fence's language, if syntect knows it
                fence_lang(&display)
                    .and_then(|lang| SYNTAXES.find_syntax_by_token(lang))
                    .map(|syntax| HighlightLines::new(syntax, &HIGHLIGHT_THEME))
            };
            for piece in wrap_line(&display, feed_width) {
                rows.push(format!("{MARGIN}│ {}", style_kind(&piece, kind)));
            }
        } else if kind == LineKind::Code {
            if let Some(h) = highlighter.as_mut() {
                let runs = highlight_code_line(h, &display, theme_default);
                for piece in wrap_runs(
                    &runs,
                    feed_width.saturating_sub(2),
                    None,
                    |s: &Option<Style>| (*s).clone(),
                ) {
                    rows.push(format!("{MARGIN}│   {piece}"));
                }
            } else {
                for piece in wrap_line(&display, feed_width.saturating_sub(2)) {
                    rows.push(format!("{MARGIN}│   {}", style_kind(&piece, kind)));
                }
            }
        } else {
            let segments = parse_inline(&display);
            for piece in wrap_inline(&segments, feed_width, kind_style(kind)) {
                rows.push(format!("{MARGIN}│ {piece}"));
            }
        }
    }
    // The load-bearing rendered-row cap: the line window alone can't bound the
    // on-screen region, because one long line wraps to many rows. Keep the
    // newest `max_rows` rows below the spinner row.
    let start = 1 + (rows.len() - 1).saturating_sub(max_rows);
    rows.drain(1..start);
    rows
}

/// Which explanatory notice (if any) to show under the spinner row of a
/// loading frame. The choice is the caller's: a streaming-capable backend
/// that has not yet produced reasoning is in a cold start (hooks/MCP/TTFT),
/// not a capability gap, so it must not be labeled non-streaming; a plain
/// backend silent past grace is the case the "does not support streaming"
/// notice was written for.
#[derive(Clone, PartialEq, Eq, Debug)]
pub(crate) enum LoadingNotice {
    /// Pre-grace, or a backend that has not been classified: just spinner +
    /// elapsed, no notice row.
    None,
    /// A streaming-capable backend (claude `stream-json`, pi `--mode json`)
    /// past grace with no reasoning yet — a cold start, not a capability gap.
    /// Carries the CLI's own program name so the notice labels the actual
    /// backend (`pi`, `claude`, …), never a hardcoded one. Worded to explain
    /// the wait (hooks/MCP/first-token) without claiming the CLI cannot stream.
    ColdStart(String),
    /// A plain backend past grace with no output: assumed non-streaming.
    Silent,
}

/// Build the explanatory notice for the [`LoadingNotice::ColdStart`] case: a
/// streaming-capable CLI that has not yet emitted reasoning. Interpolates the
/// CLI's own `program` name so the wait is attributed to the right backend
/// (`pi`, `claude`, …), not a hardcoded "Claude" — a pi/opencode/custom run
/// must not be mislabeled. Never claims the CLI cannot stream — it can, it is
/// just paying its cold-start cost (SessionStart hooks, MCP handshakes,
/// first-token latency).
fn cold_start_notice(program: &str) -> String {
    format!(
        "{program} is starting up — first reasoning line takes several seconds \
         while hooks and MCP servers initialize"
    )
}

/// The explanatory notice shown under the spinner row once a plain backend
/// has been silent past [`crate::progress::LOADING_GRACE`]. Worded for the CLI-agent case (the
/// common silent-backend source) but accurate for any backend that returns a
/// full answer with no reasoning deltas — it never claims the *model* cannot
/// think, only that aic is not receiving a stream to show.
const SILENT_NOTICE: &str =
    "This CLI agent does not stream its thinking process — aic is waiting for the final answer";

/// Build the visual rows for one loading frame: the spinner+label row on top
/// annotated with `elapsed` seconds, then — depending on `notice` — an
/// explanatory row greedy-wrapped under the shared `│ ` indent. Pure (no
/// I/O) so the layout is unit-testable; [`crate::progress::ReasoningRenderer::paint_loading`]
/// paints exactly what this returns. Mirrors [`reasoning_rows`]'s shape so a
/// loading frame and a reasoning frame hand off cleanly through draw_rows
/// (same spinner row prefix, same indent for body rows).
pub(crate) fn loading_rows(
    glyph: &str,
    label: &str,
    elapsed: Duration,
    notice: LoadingNotice,
    feed_width: usize,
) -> Vec<String> {
    let secs = elapsed.as_secs();
    let mut rows = Vec::with_capacity(2);
    rows.push(format!("{MARGIN}{glyph} {label}… {secs}s"));
    let text: String = match &notice {
        LoadingNotice::None => return rows,
        LoadingNotice::ColdStart(program) => cold_start_notice(program),
        LoadingNotice::Silent => SILENT_NOTICE.to_string(),
    };
    for piece in wrap_line(&text, feed_width) {
        rows.push(format!("{MARGIN}│ {piece}"));
    }
    rows
}
#[cfg(test)]
mod tests {
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
            render_runs(&[('a', Span::Bold)], &|sp| resolve_style(*sp, None))
                .starts_with("\x1b[1m")
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
}
