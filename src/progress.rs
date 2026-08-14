//! The run's live progress surface: an in-place spinner for one-shot waits
//! ([`with_spinner`]) and a flicker-free streaming reasoning feed
//! ([`ReasoningRenderer`] + [`ThinkingView`]).
//!
//! Split out of `display.rs` (AIC-19): `display` is the *static* panel engine
//! — commit lines, conflict summaries, the things that print once and stay.
//! This module is the *moving* surface — glyphs that spin, rows that redraw in
//! place and then vanish so nothing lingers in the scrollback. The two share
//! only the geometry primitives in [`crate::layout`] (the [`crate::layout::MARGIN`]
//! inset, [`crate::layout::wrap_line`], [`crate::layout::terminal_width`]); everything
//! that animates lives here.
//! The cursor-row probe that sizes the reasoning window to the rows below the
//! prompt lives in [`crate::cursor`]; the renderer here takes its result as
//! plain row numbers.

use console::{Color, Style, Term};
use std::collections::VecDeque;
use std::future::Future;
use std::sync::LazyLock;
use std::time::Duration;

use syntect::easy::HighlightLines;
use syntect::highlighting::{self as hl, ThemeSet};
use syntect::parsing::SyntaxSet;

use crate::layout::{MARGIN, terminal_height, terminal_width, wrap_line, wrap_words};

/// Minimum usable width for in-place progress rendering: the spinner glyph +
/// its label need at least this much room, so a pathologically narrow terminal
/// (or a misreported size) doesn't crush the spinner. The progress surface's
/// own policy on top of the shared [`terminal_width`]: the panel engine instead
/// subtracts its margins (see [`crate::display`]).
const MIN_PROGRESS_WIDTH: usize = 20;

/// Animation frame rate for every spinner in the run: the braille tick
/// advances once per tick, so a shorter interval is a smoother spin. 50 ms ≈
/// 20 fps, down from the previous 80 ms.
pub(crate) const SPINNER_TICK: Duration = Duration::from_millis(50);

/// Grace window before the silent-CLI notice appears in the loading frame.
/// Streaming CLIs (claude `-p` w/ `stream-json`, codex exec, pi) usually emit
/// their first reasoning line within 1–3 s of first token, but **first token
/// itself** is gated by the CLI's cold start — claude loads SessionStart
/// hooks, an `init` payload, MCP handshakes, then pays a network TTFT, often
/// 6–10 s in total before any `thinking_delta`. A backend still entirely
/// silent past this deadline is assumed to be a non-streaming CLI (or a
/// wedged one) and the loading frame gains an explanatory notice so the user
/// is not left staring at a bare spinner. Streaming-capable backends
/// ([`Encoding::ClaudeStreamJson`]) get a different, cold-start notice past
/// the same deadline — they ARE streaming, just not reasoning yet. Tunable:
/// the only effect of changing it is how long a slow-to-first-token backend
/// lingers in the generic loading state before the notice appears — the first
/// delta cancels loading regardless.
pub(crate) const LOADING_GRACE: Duration = Duration::from_secs(5);
/// While reasoning is actively streaming, idle-tick repaints are suppressed:
/// the flowing text is itself the motion, so a tick repaint would only
/// re-flash the stable rows (the original "chunky" cause). Once the model
/// goes silent for longer than this, ticks resume repainting so the spinner
/// keeps animating and the elapsed count keeps rising — the stall feedback.
/// Roughly three tick periods: short enough that a stall is noticed promptly,
/// long enough that a normal inter-delta gap (TTFT between tokens) never trips
/// it.
// ponytail: tuned by eye; raise if streaming still flashes, lower if the
// spinner feels sluggish to restart after a stall.
pub(crate) const ACTIVE_THRESHOLD: Duration = Duration::from_millis(150);

/// Per-row stagger of the dissolution at [`ReasoningRenderer::finish`]: each
/// row is erased one tick apart so the block visibly *dissolves* row-by-row
/// (oldest last, bottom-up) rather than blinking out wholesale. Total dissolve
/// ≈ `rows × DISSOLVE_STEP`, bounded because the row cap is bounded.
// ponytail: visual tuning — raise for a slower, more legible dissolve, lower
// if it drags the end of a fast commit.
const DISSOLVE_STEP: Duration = Duration::from_millis(70);

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

/// A rolling window over the model's streamed reasoning, sized to the caller's
/// `cap` — the rendered-row budget from [`crate::cursor::reasoning_window_rows`], reused as
/// the line-storage bound. Each line renders to at least one row, so at most
/// `cap` lines are ever visible at once; that makes `cap` an exact upper
/// bound on retained lines, not an approximation. The post-wrap rendered-row
/// cap (one line can wrap to several rows) is applied separately in
/// [`reasoning_rows`]. The caller renders [`push`](Self::push)'s returned
/// window into the spinner's in-place multi-line message, so the block redraws
/// in place — never printed as permanent lines that linger in the scrollback —
/// and is erased once thinking ends ([`ReasoningRenderer::finish`]), leaving
/// the terminal clean for the rest of the run. The cap keeps the block bounded
/// while it streams, so no unbounded region ever accumulates.
///
/// `cap` is sized by the caller from [`crate::cursor::reasoning_window_rows`], so a larger
/// terminal retains more reasoning instead of aging it out at a fixed rate.
/// Completed lines (terminated by `\n`) roll into the window; the in-progress
/// partial line (no trailing `\n` yet) is shown as the window's last line while
/// it builds and counts against the same budget. Blank lines are dropped to
/// keep the feed information-dense.
///
/// The view also tracks the markdown fence state of the **whole stream**, not
/// just the visible window: each stored line remembers whether it started
/// inside a fenced code block. [`push`](Self::push) reports the state entering
/// the window it returns, so [`reasoning_rows`] classifies the visible lines
/// exactly as a full-stream scan would — a code block whose opener scrolled
/// out of the window still colours its content, and a closer seen after the
/// opener left the window still closes instead of flipping the state open
/// again (which would render following prose as code).
pub(crate) struct ThinkingView {
    /// Completed lines, each with the fence state that was current when the
    /// line STARTED (before its own classification ran).
    lines: VecDeque<(String, bool)>,
    cur: String,
    cap: usize,
    /// Running fence state after the last completed line — the state entering
    /// the next line, and the state entering the in-progress partial.
    in_code: bool,
}

impl ThinkingView {
    pub(crate) fn new(cap: usize) -> Self {
        Self {
            lines: VecDeque::with_capacity(cap),
            cur: String::new(),
            cap,
            in_code: false,
        }
    }

    /// Ingest a reasoning delta (may be a partial line, many lines, or empty)
    /// and return the current window — the newest completed lines plus the
    /// in-progress partial line, oldest-first, capped to `cap` lines — along
    /// with the fence state entering the window's first line (see the type
    /// doc). A delta that ends mid-line leaves the partial buffered and shown
    /// as the window's last line until the next `\n`.
    pub(crate) fn push(&mut self, delta: &str) -> (Vec<String>, bool) {
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
        (self.window(), self.window_start_in_code())
    }

    /// Append a completed line: classify it against the running fence state,
    /// store the entering state with the line, then bound *storage* to `cap`
    /// — not the visible window. This stops a long chain-of-thought from
    /// retaining every completed line forever; the visible cap lives in
    /// [`window`](Self::window), which also accounts for the in-progress
    /// partial row.
    fn push_line(&mut self, line: String) {
        let entering = self.in_code;
        let (_, _, next) = classify_line(&line, entering);
        self.in_code = next;
        self.lines.push_back((line, entering));
        if self.lines.len() > self.cap {
            self.lines.pop_front();
        }
    }

    /// The line-level window: the newest `cap` lines, completed lines
    /// oldest->newest then the in-progress partial as the last line. The
    /// partial counts against the same budget, so a full window trims its
    /// oldest completed line here to make room. This bounds retention
    /// (memory); the *visible* row cap is applied by [`reasoning_rows`] after
    /// wrapping, since one line can render to several rows.
    fn window(&self) -> Vec<String> {
        let mut rows: Vec<String> = self.lines.iter().map(|(l, _)| l.clone()).collect();
        if !self.cur.trim().is_empty() {
            rows.push(self.cur.clone());
        }
        let start = rows.len().saturating_sub(self.cap);
        rows[start..].to_vec()
    }

    /// The fence state entering the current window's first line: the state
    /// stored with that line (captured when it was pushed, i.e. after all
    /// lines before it — including ones that have since rolled out). A window
    /// that is only the in-progress partial enters at the running state.
    fn window_start_in_code(&self) -> bool {
        let rows = self.lines.len() + usize::from(!self.cur.trim().is_empty());
        let start = rows.saturating_sub(self.cap);
        match self.lines.get(start) {
            Some((_, entering)) => *entering,
            None => self.in_code,
        }
    }
}

/// Braille frames for the analysis spinner. Advanced once per reasoning
/// delta (see [`ReasoningRenderer`]) so the glyph spins while reasoning flows
/// and freezes when it stalls — incidental animation that needs no background
/// ticker, since a steady tick is exactly what forced indicatif's flickering
/// full-block repaints.
const REASONING_GLYPHS: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

/// Markdown render kind for one reasoning line. Streaming markdown can't be
/// fully parsed (the input is partial), so classification is line-local with a
/// running fence state — robust where it matters (headings, fenced code
/// blocks). The state is carried across the whole stream by
/// [`ThinkingView`], so a code block whose opener has already scrolled out of
/// the window still colours its content correctly, and a closer after a long
/// block still closes it. Inline `**bold**` / `` `code` `` *are* parsed for
/// [`LineKind::Normal`] prose (see [`parse_inline`] + [`wrap_styled`]), which
/// re-opens a span that a wrap break split across two rows; other kinds stay
/// on the wrap-safe single-style-per-line path.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum LineKind {
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
fn classify_line(line: &str, in_code: bool) -> (LineKind, String, bool) {
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
/// window's first line, as tracked by [`ThinkingView`] over the whole stream
/// — never assumed `false` here, or a code block whose opener scrolled out of
/// the window would classify its closer as an opener and colour the following
/// prose as code.
fn reasoning_rows(
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
/// has been silent past [`LOADING_GRACE`]. Worded for the CLI-agent case (the
/// common silent-backend source) but accurate for any backend that returns a
/// full answer with no reasoning deltas — it never claims the *model* cannot
/// think, only that aic is not receiving a stream to show.
const SILENT_NOTICE: &str =
    "This CLI agent does not stream its thinking process — aic is waiting for the final answer";

/// Build the visual rows for one loading frame: the spinner+label row on top
/// annotated with `elapsed` seconds, then — depending on `notice` — an
/// explanatory row greedy-wrapped under the shared `│ ` indent. Pure (no
/// I/O) so the layout is unit-testable; [`ReasoningRenderer::paint_loading`]
/// paints exactly what this returns. Mirrors [`reasoning_rows`]'s shape so a
/// loading frame and a reasoning frame hand off cleanly through [`draw_rows`]
/// (same spinner row prefix, same indent for body rows).
fn loading_rows(
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
    /// Rendered-row cap for the reasoning window, resolved once by the caller
    /// from the real terminal height ([`crate::cursor::reasoning_window_rows`]) so the view
    /// and the renderer share one notion of the window size; a resize
    /// mid-stream only changes wrap widths, never the row budget.
    max_rows: usize,
    /// Terminal height in rows (same 0→24 fallback as
    /// [`crate::layout::terminal_height`]) — the bottom margin past which a
    /// descent scrolls instead of clamping.
    height: usize,
    /// Absolute 1-based row of the cursor at the start of the next paint —
    /// the frame's bottom row after the previous paint, or the DSR-reported
    /// row before the first. `None` when the cursor could not be queried:
    /// frames then never scroll (legacy behavior).
    row: Option<usize>,
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
    /// The last frame's rows, for incremental repaint. On a typical delta only
    /// the in-progress partial line (the bottom row) grows; everything above is
    /// byte-identical, so redrawing just that one row — instead of clearing and
    /// rewriting the whole block — is what makes streaming read as fluid growth
    /// rather than a flashing block. Structural changes (a line completing and
    /// the window rolling, a row expiring, a height change) fall back to a full
    /// [`frame_bytes`] repaint.
    prev_rows: Vec<String>,
    /// Frozen spinner elapsed-time shown while streaming is active. The spinner
    /// must NOT change between deltas (or the bottom-row-only diff would see a
    /// changed top row and fall back to a full repaint, reintroducing the
    /// flash), so it advances only on a stall tick via [`Self::refresh`].
    shown_elapsed: Duration,
    /// Wall-clock of the last content delta. While `Instant::now() - this` <
    /// [`ACTIVE_THRESHOLD`], [`Self::refresh`] is a no-op — the stream is the
    /// motion, a tick repaint would only flash stable rows. `None` until the
    /// first paint.
    last_delta: Option<std::time::Instant>,
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
/// `start_row` is the absolute 1-based terminal row of the cursor when the
/// paint begins (`None` when the DSR query failed — the legacy no-scroll
/// behavior, since the bottom margin is then unknowable), and `height` the
/// terminal's row count. Descending **past** the bottom margin is the one
/// move the terminal won't make for us: cursor-down clamps at the margin
/// instead of scrolling, which is exactly the collapse that made tall frames
/// overwrite the bottom row. So a descent from the last row emits `\n`
/// instead — a scroll — shifting the screen up one row and giving the frame
/// a fresh bottom row. The scroll shifts the frame's own top row up with
/// everything else, so the erase contract still holds: [`clear_frame_bytes`]
/// walks up `prev_height - 1` rows from the frame's bottom and lands on the
/// frame's top, which is wherever the prompt (and the frame) drifted to.
/// Returns the assembled bytes plus the cursor's absolute row afterwards
/// (`None` when the start was unknown), which the renderer feeds back as the
/// next paint's `start_row`.
///
/// Cursor visibility is a stream-spanning concern, not a per-frame one. The
/// first repaint emits [`HIDE`] (when `prev_height == 0`) and the caret stays
/// hidden for the whole stream; only [`clear_frame_bytes`] at
/// [`ReasoningRenderer::finish`] restores it ([`SHOW`]). Mid-stream repaints
/// emit neither — the caret stays hidden, so where it sits during the traversal
/// is irrelevant and can never smear across the repainted rows.
fn frame_bytes(
    rows: &[String],
    prev_height: usize,
    start_row: Option<usize>,
    height: usize,
) -> (String, Option<usize>) {
    let height = height.max(1);
    // The frame's top row: where the cursor is now, walked up by the repaint
    // preamble. `prev_height == 0` has no preamble — the frame starts at the
    // cursor's own row.
    let top = start_row.map(|r| r.min(height).saturating_sub(prev_height.saturating_sub(1)));
    let frame_height = rows.len();
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
    // Absolute row of the cursor as we descend; tracked only when known.
    let mut row = top;
    for (i, r) in rows.iter().enumerate() {
        out.push('\r');
        out.push_str(CLR_LINE);
        out.push_str(r);
        if i + 1 < frame_height {
            match row {
                Some(cur) if cur >= height => {
                    // At the bottom margin, cursor-down would clamp (the
                    // collapse) — scroll the screen up instead so the frame
                    // gains a fresh bottom row. The cursor stays on the last
                    // row, everything above it shifted up one.
                    out.push('\n');
                }
                _ => {
                    out.push_str(DOWN);
                }
            }
            if let Some(cur) = row.as_mut()
                && *cur < height
            {
                *cur += 1;
            }
        }
    }
    if frame_height < prev_height {
        // Stale tail below the shorter frame: blank each row and walk back.
        // The walk ends at or above the frame's own bottom (the old frame
        // never painted past the new one's bottom unless both hit the bottom
        // margin, in which case there is no tail), so it never scrolls.
        for _ in frame_height..prev_height {
            out.push_str(DOWN);
            out.push('\r');
            out.push_str(CLR_LINE);
        }
        for _ in frame_height..prev_height {
            out.push_str(UP);
        }
    }
    let end_row = top.map(|t| (t + frame_height - 1).min(height));
    (out, end_row)
}

/// Assemble the byte sequence to erase a `prev_height`-row frame, clearing
/// each row from the BOTTOM up so the traversal naturally ends on the frame's
/// TOP row — exactly where the first frame began painting (the cursor's line
/// at stream start, which scrolled frames carry upward with them, so it is
/// still the prompt's row when the stream ends). So once thinking ends the
/// cursor sits back on that line, the block is gone, and the next line of
/// stderr overwrites the blank region from the top with no gap trapped above
/// or below.
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

/// Whether `new` differs from `prev` only in its last row — same length, same
/// every row above. This is the common streaming case: one token grew the
/// in-progress partial line (the bottom row) and nothing else moved. When true,
/// [`ReasoningRenderer::draw_rows`] rewrites just that bottom row in place
/// instead of clearing and rewriting the whole block, which is what makes the
/// feed read as fluid character growth rather than a flashing block. A roll
/// (a line completed and the window shifted), a row count change, or any change
/// above the bottom row all return `false` → full [`frame_bytes`] repaint.
fn incremental_bottom(new: &[String], prev: &[String]) -> bool {
    !new.is_empty()
        && new.len() == prev.len()
        && prev[new.len() - 1] != new[new.len() - 1]
        && prev[..new.len() - 1] == new[..new.len() - 1]
}
impl ReasoningRenderer {
    /// Bind a renderer to stderr with `label` on the spinner row. `max_rows`
    /// is the reasoning window's rendered-row cap and `cursor_row` the
    /// DSR-reported cursor row (`None` if the query failed — scrolling is
    /// then disabled), both from [`crate::cursor::reasoning_window_rows`]. `feed_width` is
    /// resolved once from the shared terminal geometry ([`terminal_width`]),
    /// floored at the progress surface's [`MIN_PROGRESS_WIDTH`] so the
    /// spinner and its label keep room. A resize mid-stream only changes
    /// wrap widths, never correctness.
    pub(crate) fn new(label: &'static str, max_rows: usize, cursor_row: Option<usize>) -> Self {
        Self {
            term: Term::stderr(),
            label,
            feed_width: terminal_width().max(MIN_PROGRESS_WIDTH).saturating_sub(6),
            max_rows,
            height: terminal_height(),
            row: cursor_row,
            glyph: 0,
            prev_height: 0,
            active: false,
            cursor_hidden: false,
            prev_rows: Vec::new(),
            shown_elapsed: Duration::ZERO,
            last_delta: None,
        }
    }

    /// Paint one frame for a content delta. The spinner is **frozen** here —
    /// neither the glyph nor its elapsed count advances — so between tokens
    /// only the in-progress line grows, and [`draw_rows`] can rewrite just that
    /// one bottom row. That single-row update (vs. clearing and rewriting the
    /// whole block every token) is the difference between fluid character
    /// growth and the flashing "block-by-block" feed. The spinner animates
    /// instead on stall ticks via [`Self::refresh`]. A no-op off a terminal.
    pub(crate) fn paint(&mut self, window: &[String], in_code_start: bool) {
        if !self.term.is_term() {
            return;
        }
        let glyph = REASONING_GLYPHS[self.glyph % REASONING_GLYPHS.len()];
        let rows = reasoning_rows(
            glyph,
            self.label,
            window,
            self.feed_width,
            Some(self.shown_elapsed),
            self.max_rows,
            in_code_start,
        );
        self.draw_rows(&rows);
        self.last_delta = Some(std::time::Instant::now());
    }

    /// Idle-tick repaint. While streaming is active (`last_delta` within
    /// [`ACTIVE_THRESHOLD`]) this is a no-op: the flowing text is itself the
    /// motion, and a repaint would only re-flash the stable rows above it — the
    /// original chunky symptom. Once the model falls silent past the threshold,
    /// the spinner advances and the elapsed count rises so the wait stays
    /// visible across a long TTFT gap without the feed ever freezing. A no-op
    /// off a terminal.
    pub(crate) fn refresh(&mut self, window: &[String], in_code_start: bool, elapsed: Duration) {
        if !self.term.is_term() {
            return;
        }
        if let Some(t) = self.last_delta
            && t.elapsed() < ACTIVE_THRESHOLD
        {
            return;
        }
        self.glyph = self.glyph.wrapping_add(1);
        self.shown_elapsed = elapsed;
        let glyph = REASONING_GLYPHS[self.glyph % REASONING_GLYPHS.len()];
        let rows = reasoning_rows(
            glyph,
            self.label,
            window,
            self.feed_width,
            Some(elapsed),
            self.max_rows,
            in_code_start,
        );
        self.draw_rows(&rows);
    }

    /// Paint one loading frame for the silent/cold-start backend state: the
    /// spinner row annotated with `elapsed` seconds, plus — once `notice` is
    /// past [`LOADING_GRACE`]/classified — an explanatory row. Used by the
    /// [`reasoning_feed`](crate::reasoning_feed) driver between stream start
    /// and the first reasoning delta so a non-streaming or cold-starting
    /// backend is not a silent dead zone: the user always sees motion (the spinning
    /// glyph) and a rising elapsed count. The first delta swaps this frame for
    /// the normal reasoning window via [`paint`](Self::paint); both go through
    /// [`draw_rows`], so the in-place repaint handles the height transition
    /// with no special-casing. A no-op off a terminal.
    pub(crate) fn paint_loading(&mut self, elapsed: Duration, notice: LoadingNotice) {
        if !self.term.is_term() {
            return;
        }
        let glyph = REASONING_GLYPHS[self.glyph % REASONING_GLYPHS.len()];
        self.glyph = self.glyph.wrapping_add(1);
        let rows = loading_rows(glyph, self.label, elapsed, notice, self.feed_width);
        self.draw_rows(&rows);
    }

    /// Repaint `rows` in place. The common streaming case — only the bottom
    /// row grew — rewrites that one row where the cursor already sits, skipping
    /// the full clear+rewrite entirely. Structural changes (first frame, a line
    /// completing so the window rolls, a row expiring, a height change) fall
    /// back to [`frame_bytes`]'s anti-flicker full repaint. Pure byte assembly
    /// lives in [`frame_bytes`] / [`incremental_bottom`] so it stays unit-testable.
    fn draw_rows(&mut self, rows: &[String]) {
        let first_frame = self.prev_rows.is_empty();
        // Identical frame (a whitespace-only delta ThinkingView dropped, or a
        // stall tick whose window hadn't moved): writing it would only flash.
        if !first_frame && rows == self.prev_rows.as_slice() {
            return;
        }
        // Record cursor-hidden BEFORE the write: the flag mirrors exactly when
        // frame_bytes emits HIDE (first frame), so tracking it ahead of the
        // side effect means a hidden cursor can never be stranded — even a
        // panic between here and the write leaves Drop seeing the owed SHOW.
        self.cursor_hidden |= first_frame;
        if !first_frame && incremental_bottom(rows, &self.prev_rows) {
            // The cursor already rests on the bottom row (every paint ends
            // there), so a bare CR + clear + rewrite of just that row is the
            // entire update — no ascent, no descent, stable rows untouched.
            let last = rows.last().expect("non-empty by incremental_bottom");
            let mut buf = String::from("\r");
            buf.push_str(CLR_LINE);
            buf.push_str(last);
            let _ = self.term.write_str(&buf);
            let _ = self.term.flush();
        } else {
            let (bytes, end_row) = frame_bytes(rows, self.prev_height, self.row, self.height);
            let _ = self.term.write_str(&bytes);
            let _ = self.term.flush();
            self.row = end_row;
        }
        self.prev_rows.clear();
        self.prev_rows.extend_from_slice(rows);
        self.prev_height = rows.len();
        self.active = true;
    }

    /// End the reasoning stream by **dissolving** the frame one row at a time,
    /// each erased [`DISSOLVE_STEP`] apart, so the block visibly retreats
    /// row-by-row instead of blinking out wholesale — the "linger then exit
    /// one-by-one" close. The erase runs bottom-up (matching the terminal's
    /// scroll direction) and, like the old one-shot clear, ends on the frame's
    /// top row — exactly where the first frame began — so the cursor is
    /// restored to the prompt's line and the next stderr output continues with
    /// no gap above or below. Idempotent.
    ///
    /// The staggered waits are decoration at the *end* of analysis (the stream
    /// is already complete), replacing the old fixed read-tail hold-then-erase:
    /// the dissolve itself is the readability window. An *aborted* stream takes
    /// the fast path in [`Drop`] instead, never blocking on decoration.
    pub(crate) fn finish(&mut self) {
        if !self.active {
            return;
        }
        let n = self.prev_height;
        for i in 0..n {
            // Erase the lowest remaining row, flush so it shows, then ascend to
            // the next — ending on the top row after n-1 ascents.
            let mut buf = String::from("\r");
            buf.push_str(CLR_LINE);
            let _ = self.term.write_str(&buf);
            let _ = self.term.flush();
            if i + 1 < n {
                let _ = self.term.write_str(UP);
                let _ = self.term.flush();
                std::thread::sleep(DISSOLVE_STEP);
            }
        }
        if self.cursor_hidden {
            let _ = self.term.write_str(SHOW);
            let _ = self.term.flush();
        }
        self.prev_rows.clear();
        self.prev_height = 0;
        self.active = false;
        self.cursor_hidden = false;
    }

    /// Fast wholesale erase — the [`Drop`] backstop for a stream that aborted
    /// before [`finish`](Self::finish). Same byte sequence as the legacy
    /// one-shot clear (bottom-up, ending on the top row, [`SHOW`] if the cursor
    /// was hidden) but with no staggered waits, so an error or panic path never
    /// hangs on decoration.
    fn erase_frame(&mut self) {
        let bytes = clear_frame_bytes(self.prev_height, self.cursor_hidden);
        let _ = self.term.write_str(&bytes);
        let _ = self.term.flush();
        self.prev_rows.clear();
        self.prev_height = 0;
        self.active = false;
        self.cursor_hidden = false;
    }
}

/// Production [`reasoning_feed::ReasoningSink`]. Each method forwards to the
/// inherent method of the same name in `impl ReasoningRenderer` above — Rust's
/// method resolution prefers inherent over trait, so a `self.paint` body here
/// is *not* a recursive call. The trait seam exists so the driver loop can run
/// against a recording fake in tests; this is the only production sink.
impl crate::reasoning_feed::ReasoningSink for ReasoningRenderer {
    fn paint(&mut self, window: &[String], in_code_start: bool) {
        self.paint(window, in_code_start)
    }
    fn paint_loading(&mut self, elapsed: Duration, notice: LoadingNotice) {
        self.paint_loading(elapsed, notice)
    }
    fn refresh(&mut self, window: &[String], in_code_start: bool, elapsed: Duration) {
        self.refresh(window, in_code_start, elapsed)
    }
    fn finish(&mut self) {
        self.finish()
    }
}

impl Drop for ReasoningRenderer {
    fn drop(&mut self) {
        // Backstop: if the stream aborted before finish(), erase the frame fast
        // (no staggered dissolve) so the rest of the run's stderr is clean.
        if self.active {
            self.erase_frame();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// [`ThinkingView::push`] returns the current window: completed lines
    /// (blank ones dropped) in arrival order, oldest-first.
    #[test]
    fn thinking_view_window_shows_completed_lines_and_drops_blanks() {
        let mut v = ThinkingView::new(12);
        let (window, in_code) = v.push("line 1\n\nline 2\n");
        assert_eq!(window, vec!["line 1", "line 2"]);
        assert!(!in_code); // plain prose: no fence state
    }

    /// A partial line with no trailing `\n` is the window's last row while it
    /// builds, then collapses into a completed row when the `\n` arrives.
    #[test]
    fn thinking_view_partial_is_last_window_row_until_newline() {
        let mut v = ThinkingView::new(12);
        assert_eq!(v.push("in progress").0, vec!["in progress"]);
        let (window, _) = v.push(" done\n");
        assert_eq!(window, vec!["in progress done"]);
    }

    /// One logical line split across several deltas assembles into a single
    /// window row, shown live as it grows.
    #[test]
    fn thinking_view_assembles_split_chunks() {
        let mut v = ThinkingView::new(12);
        assert_eq!(v.push("hel").0, vec!["hel"]);
        assert_eq!(v.push("lo").0, vec!["hello"]);
        assert_eq!(v.push(" world\n").0, vec!["hello world"]);
    }

    /// A delta containing several `\n`-separated lines yields a window with
    /// each one, in order.
    #[test]
    fn thinking_view_many_lines_one_delta() {
        let mut v = ThinkingView::new(12);
        assert_eq!(v.push("a\nb\nc\n").0, vec!["a", "b", "c"]);
    }

    /// The window rolls: past the cap, the oldest completed
    /// line is dropped, so the window stays capped and always shows the newest
    /// rows.
    #[test]
    fn thinking_view_rolls_at_capacity() {
        let mut v = ThinkingView::new(12);
        for i in 1..=15 {
            v.push(&format!("line {i}\n"));
        }
        let (window, _) = v.push("");
        assert_eq!(window.len(), 12);
        assert_eq!(window.first(), Some(&"line 4".to_string()));
        assert_eq!(window.last(), Some(&"line 15".to_string()));
    }

    /// The in-progress partial line counts against the same budget: with the
    /// window full of completed rows, a partial drops the oldest completed row
    /// so the window never exceeds the cap.
    #[test]
    fn thinking_view_partial_counts_against_budget() {
        let mut v = ThinkingView::new(12);
        for i in 1..=12 {
            v.push(&format!("line {i}\n"));
        }
        let (window, _) = v.push("in progress");
        assert_eq!(window.len(), 12);
        // oldest completed row rolled out to make room for the partial
        assert_eq!(window.first(), Some(&"line 2".to_string()));
        assert_eq!(window.last(), Some(&"in progress".to_string()));
    }

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

    /// First paint (no previous frame) hides the cursor and clears+writes its
    /// single row in place at the cursor's current line — no leading newline
    /// (which would reserve an unreclaimable blank line above the block) and
    /// no cursor-up preamble. With the cursor unknown, the paint never
    /// scrolls (the legacy behavior).
    #[test]
    fn frame_bytes_first_frame_paints_in_place() {
        let (out, end_row) = frame_bytes(&["only".to_string()], 0, None, 24);
        assert_eq!(out, format!("{HIDE}\r{CLR_LINE}only"));
        // the first frame never emits a newline: blank lines are unreclaimable.
        assert!(!out.contains('\n'), "first frame must not emit a newline");
        // unknown start row → unknown end row, and no scroll bytes.
        assert_eq!(end_row, None);
    }

    /// The load-bearing anti-flicker property: a same-height repaint clears
    /// each row and rewrites it *immediately* before descending — never the
    /// "blank every row, then rewrite every row" two-phase that flickered. The
    /// full byte sequence is asserted, so any regression to a clear-all-then-
    /// write-all repaint fails here.
    #[test]
    fn frame_bytes_repaint_is_interleaved_clear_then_write() {
        let rows = vec!["a".to_string(), "b".to_string(), "c".to_string()];
        let (out, end_row) = frame_bytes(&rows, 3, None, 24);
        // up to top (2 rows), then per row: CR + clear + write, descending.
        let expected = format!("{UP}{UP}\r{CLR_LINE}a{DOWN}\r{CLR_LINE}b{DOWN}\r{CLR_LINE}c");
        assert_eq!(out, expected);
        assert_eq!(end_row, None);
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
        let (out, end_row) = frame_bytes(&["a".to_string()], 3, None, 24);
        let expected = format!("{UP}{UP}\r{CLR_LINE}a{DOWN}\r{CLR_LINE}{DOWN}\r{CLR_LINE}{UP}{UP}");
        assert_eq!(out, expected);
        assert_eq!(end_row, None);
    }

    /// A taller new frame descends into fresh rows below the previous frame —
    /// each new row cleared then written in place as it appears.
    #[test]
    fn frame_bytes_taller_frame_descends_into_new_rows() {
        let rows = vec!["a".to_string(), "b".to_string(), "c".to_string()];
        let (out, end_row) = frame_bytes(&rows, 1, None, 24);
        let expected = format!("\r{CLR_LINE}a{DOWN}\r{CLR_LINE}b{DOWN}\r{CLR_LINE}c");
        assert_eq!(out, expected);
        assert_eq!(end_row, None);
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
    /// The incremental-repaint guard: only a lone change to the bottom row
    /// (the in-progress line growing by one token) qualifies for the cheap
    /// single-row rewrite. A roll (window shifted), a height change, a change
    /// above the bottom, or identical frames all fall back to a full repaint —
    /// the cases that would corrupt the screen if redrawn as a single row.
    #[test]
    fn incremental_bottom_only_when_lone_bottom_row_grew() {
        let row = |s: &str| s.to_string();
        // Bottom row grew, everything above identical → incremental.
        assert!(incremental_bottom(
            &[row("a"), row("b"), row("grow")],
            &[row("a"), row("b"), row("gr")]
        ));
        // Identical → not incremental (a no-op write, handled before this fn,
        // but the predicate must not claim a phantom bottom change either).
        assert!(!incremental_bottom(
            &[row("a"), row("b")],
            &[row("a"), row("b")]
        ));
        // A row ABOVE the bottom changed (e.g. the spinner unfroze) → full
        // repaint, never a lone bottom rewrite.
        assert!(!incremental_bottom(
            &[row("A"), row("b"), row("c")],
            &[row("a"), row("b"), row("c")]
        ));
        // Height changed (a line completed, window rolled/grew) → full repaint.
        assert!(!incremental_bottom(&[row("a"), row("b")], &[row("a")]));
        assert!(!incremental_bottom(&[row("a")], &[row("a"), row("b")]));
        // Empty is never incremental.
        assert!(!incremental_bottom(&[], &[row("a")]));
        assert!(!incremental_bottom(&[row("a")], &[]));
    }

    /// Regression: the reasoning stream must leave NO trace — neither a blank
    /// gap nor a reserved blank line — between the output that preceded the
    /// block and the output that follows it. The frame paints in place at the
    /// cursor's current line (no opening newline), so its top row is the
    /// cursor's start row; `finish` must restore the cursor to that same row
    /// and the whole stream must emit zero newlines (a `\n` is an unreclaimable
    /// blank line in a terminal, the original cause of the lingering empty
    /// area). The old design opened with a `\n` and parked the cursor on the
    /// cleared region's bottom row, leaving several blank rows
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

        for height in [1usize, 2, 3, 12, 13] {
            let rows: Vec<String> = (0..height).map(|_| "x".to_string()).collect();
            // Cursor unknown (`None`): the legacy no-scroll geometry — a
            // mid-screen stream that never touches the bottom margin.
            let (paint, _) = frame_bytes(&rows, 0, None, 24);
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
        let (first, _) = frame_bytes(&["only".to_string()], 0, None, 24);
        assert!(first.starts_with(HIDE), "first frame must hide the cursor");
        assert!(
            !first.contains(SHOW),
            "first frame must not restore mid-stream"
        );

        // A mid-stream repaint touches neither: the caret stays hidden, so its
        // position during the traversal can't smear across the repainted rows.
        let (repaint, _) = frame_bytes(&["a".to_string(), "b".to_string()], 2, None, 24);
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

    /// The load-bearing scroll: descending **past** the bottom margin is the
    /// one move the terminal refuses (cursor-down clamps — the collapse that
    /// made tall frames overwrite the bottom row), so the renderer emits `\n`
    /// — a scroll that shifts the screen up and grants the frame a fresh
    /// bottom row. A first frame at the very bottom of the screen (the usual
    /// shell-prompt position) thus paints every row, scrolling once per row
    /// past the first, instead of collapsing.
    #[test]
    fn frame_bytes_scrolls_when_descending_past_bottom_margin() {
        let rows = vec!["a".to_string(), "b".to_string(), "c".to_string()];
        let (out, end_row) = frame_bytes(&rows, 0, Some(24), 24);
        let expected = format!("{HIDE}\r{CLR_LINE}a\n\r{CLR_LINE}b\n\r{CLR_LINE}c");
        assert_eq!(out, expected);
        // The frame's bottom row is the screen's last row; the cursor stays
        // there for the next paint (the top drifted up with the scrolls).
        assert_eq!(end_row, Some(24));
        assert_eq!(
            out.matches('\n').count(),
            2,
            "one scroll per descent past the margin"
        );
    }

    /// A frame that fits in the rows below the cursor never scrolls — the
    /// screen stays put and the cursor ends on the frame's bottom row.
    #[test]
    fn frame_bytes_fits_without_scroll_when_room_below() {
        let rows = vec!["a".to_string(), "b".to_string(), "c".to_string()];
        let (out, end_row) = frame_bytes(&rows, 0, Some(10), 24);
        let expected = format!("{HIDE}\r{CLR_LINE}a{DOWN}\r{CLR_LINE}b{DOWN}\r{CLR_LINE}c");
        assert_eq!(out, expected);
        assert_eq!(end_row, Some(12));
    }

    /// A frame already pinned to the bottom margin grows by scrolling exactly
    /// once for the new row — the repaint preamble still reaches the (drifted)
    /// frame top, and the cursor reports the bottom row for the next paint.
    #[test]
    fn frame_bytes_growth_from_bottom_scrolls_once() {
        let rows = vec![
            "a".to_string(),
            "b".to_string(),
            "c".to_string(),
            "d".to_string(),
        ];
        let (out, end_row) = frame_bytes(&rows, 3, Some(24), 24);
        let expected =
            format!("{UP}{UP}\r{CLR_LINE}a{DOWN}\r{CLR_LINE}b{DOWN}\r{CLR_LINE}c\n\r{CLR_LINE}d");
        assert_eq!(out, expected);
        assert_eq!(end_row, Some(24));
    }

    /// A frame that shrinks after the screen scrolled clears its stale tail
    /// and reports the new (higher) bottom row, so the next paint's preamble
    /// lands on the correct — drifted — frame top.
    #[test]
    fn frame_bytes_shrink_after_scroll_clears_tail_and_reports_row() {
        let (out, end_row) = frame_bytes(&["a".to_string()], 3, Some(24), 24);
        let expected = format!("{UP}{UP}\r{CLR_LINE}a{DOWN}\r{CLR_LINE}{DOWN}\r{CLR_LINE}{UP}{UP}");
        assert_eq!(out, expected);
        assert_eq!(end_row, Some(22));
    }

    /// The erase contract holds after scrolling: the frame's top row drifts
    /// up with the scrolls (it is wherever the prompt line drifted to), and
    /// [`clear_frame_bytes`]'s bottom-up walk — from the frame's bottom row,
    /// `prev_height - 1` ascents — lands exactly on that drifted top. The
    /// next line of stderr therefore continues at the prompt, with the whole
    /// reasoning region erased above it: no gap, nothing lingering.
    #[test]
    fn scrolled_stream_erase_ends_at_drifted_frame_top() {
        let height = 24usize;
        let rows: Vec<String> = (0..13).map(|_| "x".to_string()).collect();
        let (paint, end) = frame_bytes(&rows, 0, Some(height), height);
        assert_eq!(end, Some(24));
        // 12 scrolls shift everything — including the prompt and the frame
        // top — up 12 rows; the frame still fits entirely on screen.
        assert_eq!(paint.matches('\n').count(), 12);
        // Erase from the frame's bottom row (24): 13 clears, 12 ascents →
        // the drifted top row — exactly `height − (rows − 1)`.
        let drifted_top = height - (rows.len() - 1);
        assert_eq!(drifted_top, 12);
        let erase = clear_frame_bytes(13, true);
        // Bottom-up walk: clear current row, ascend, repeat, ending on the
        // drifted top; SHOW restores the caret.
        let expected = format!("\r{CLR_LINE}{UP}").repeat(12) + &format!("\r{CLR_LINE}{SHOW}");
        assert_eq!(erase, expected);
        // The next line of stderr writes at the drifted top — the prompt's
        // new row — with the whole reasoning region erased above it.
        assert_eq!(end.unwrap() - (13 - 1), drifted_top);
    }

    /// The fence state survives window rolls: [`ThinkingView`] tracks it over
    /// the whole stream and reports the state entering the window's first
    /// line, so a code block whose opener scrolled out of the window keeps
    /// its colour (and, crucially, its closer still closes).
    #[test]
    fn thinking_view_fence_state_persists_across_window_rolls() {
        let mut v = ThinkingView::new(3);
        // Stream: opener, 4 code lines, closer, prose — cap 3 keeps only the
        // tail, so the opener is long gone by the time the closer arrives.
        v.push("```rust\n");
        v.push("code 1\ncode 2\ncode 3\ncode 4\n");
        // Window is [code 2, code 3, code 4] — all entered inside the block.
        let (window, in_code) = v.push("");
        assert_eq!(window, vec!["code 2", "code 3", "code 4"]);
        assert!(
            in_code,
            "window starts inside the block, opener already rolled out"
        );
        // The closer arrives; the window now starts even deeper in the block
        // and the closer must still be classified as a closer.
        let (window, in_code) = v.push("```\n");
        assert_eq!(window, vec!["code 3", "code 4", "```"]);
        assert!(in_code);
        // Prose after the closer: the window now starts even deeper in the
        // block — `code 4` entered inside it — so its entering state is still
        // `true`. The closer is IN the window, so a scan from the true state
        // closes the block and the prose classifies as Normal. This is the
        // regression: a per-frame scan starting at `false` would classify the
        // closer as an OPENER (the state resets made it look unopened) and
        // colour the following prose as code.
        let (window, in_code) = v.push("plain prose\n");
        assert_eq!(window, vec!["code 4", "```", "plain prose"]);
        assert!(
            in_code,
            "window starts mid-block (code 4 entered inside it)"
        );
        // Full-stream equivalence: scanning the window from its true entering
        // state yields exactly what a scan of the whole stream would.
        let mut s = in_code;
        let kinds: Vec<LineKind> = window
            .iter()
            .map(|line| {
                let (kind, _, next) = classify_line(line, s);
                s = next;
                kind
            })
            .collect();
        assert_eq!(
            kinds,
            vec![LineKind::Code, LineKind::CodeFence, LineKind::Normal],
            "the closer must close and the prose must not be code"
        );
    }

    /// A partial line being typed inside a code block is classified as code
    /// even before its `\n` arrives: the window's scan reaches it with the
    /// running state (opened by the in-window fence).
    #[test]
    fn thinking_view_partial_enters_at_running_fence_state() {
        let mut v = ThinkingView::new(12);
        v.push("```\n");
        let (window, in_code) = v.push("let x = ");
        assert_eq!(window, vec!["```", "let x = "]);
        // The window starts at the fence line (entering state false); the
        // scan then opens the block, so the partial — the window's last line
        // — classifies as code.
        assert!(!in_code);
        let mut s = in_code;
        let kinds: Vec<LineKind> = window
            .iter()
            .map(|line| {
                let (kind, _, next) = classify_line(line, s);
                s = next;
                kind
            })
            .collect();
        assert_eq!(kinds, vec![LineKind::CodeFence, LineKind::Code]);
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
