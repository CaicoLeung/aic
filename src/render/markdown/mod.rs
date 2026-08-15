//! Pure streaming-Markdown painter: line → styled rows.
//!
//! ADR 0013's renderer contract lives here — the line→styled-rows function
//! family ([`reasoning_rows`], [`loading_rows`]) that turns a partial Markdown
//! reasoning window into ANSI-styled terminal rows. Zero I/O: classification
//! ([`classify_line`]), inline `**bold**`/`` `code` `` parsing, syntect code
//! highlighting, and the generic [`wrap_runs`] wrap engine. The *when-to-paint*
//! policy is [`crate::render::reasoning_feed`]; the terminal surface that frames these
//! rows is [`crate::render::progress::ReasoningRenderer`].

use console::{Color, Style};
use std::sync::LazyLock;
use std::time::Duration;
use syntect::easy::HighlightLines;
use syntect::highlighting::{self as hl, ThemeSet};
use syntect::parsing::SyntaxSet;

use crate::render::layout::{MARGIN, wrap_line, wrap_words};

/// Markdown render kind for one reasoning line. Streaming markdown can't be
/// fully parsed (the input is partial), so classification is line-local with a
/// running fence state — robust where it matters (headings, fenced code
/// blocks). The state is carried across the whole stream by
/// [`crate::render::progress::ThinkingView`], so a code block whose opener has already scrolled out of
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
/// so the width math in [`crate::render::layout::wrap_line`] never counts escape bytes.
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
/// [`crate::render::layout::wrap_words`] (shared with [`crate::render::layout::wrap_line`], so
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
/// whitespace separates, mirroring [`crate::render::layout::wrap_line`]'s
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
/// cap (from [`crate::render::cursor::reasoning_window_rows`]); the newest `max_rows` rows below the
/// spinner are kept and the oldest drop out — a long line that wraps to
/// several rows still cannot grow the region, so the top row may start
/// mid-line, like a terminal tail window. An empty `window` yields just the
/// spinner row. `in_code_start` is the markdown fence state entering the
/// window's first line, as tracked by [`crate::render::progress::ThinkingView`] over the whole stream
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
/// has been silent past [`crate::render::progress::LOADING_GRACE`]. Worded for the CLI-agent case (the
/// common silent-backend source) but accurate for any backend that returns a
/// full answer with no reasoning deltas — it never claims the *model* cannot
/// think, only that aic is not receiving a stream to show.
const SILENT_NOTICE: &str =
    "This CLI agent does not stream its thinking process — aic is waiting for the final answer";

/// Build the visual rows for one loading frame: the spinner+label row on top
/// annotated with `elapsed` seconds, then — depending on `notice` — an
/// explanatory row greedy-wrapped under the shared `│ ` indent. Pure (no
/// I/O) so the layout is unit-testable; [`crate::render::progress::ReasoningRenderer::paint_loading`]
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
mod tests;
