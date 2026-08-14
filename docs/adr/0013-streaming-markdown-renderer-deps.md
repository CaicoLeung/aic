# ADR 0013: Streaming-markdown renderer dependencies

- **Status:** Accepted
- **Date:** 2026-08-14

## Context

PR #134 extends the streaming reasoning window (`src/progress.rs`) to render
model reasoning as Markdown — line kinds (heading / list-item / blockquote),
inline `**bold**` and `` `code` `` spans, and per-token syntax highlighting in
fenced code blocks. Two dependencies landed to do it:

- **`streamdown-parser`** (0.1) — parses inline Markdown spans.
- **`syntect`** (5, `default-features = false`) — syntax-highlights fenced
  code blocks.

Both are new to a binary that had previously been a `console` + `indicatif`
raw-ANSI painter with no Markdown or syntax dependency at all. CONTRIBUTING
asks that dependency / architecture decisions be recorded as an ADR, so this
records *why these two* and *why this substrate*.

### The substrate constraint

The reasoning window is a **raw-ANSI in-place painter**, not a TUI framework:
rows are `Vec<String>` of already-styled text repainted via `console::Term`.
The load-bearing invariant is that **width math is ANSI-blind** — line length
is counted in plain `char`s and styling is applied *after* wrap, so escape
bytes never count toward the visible budget. That geometry is shared with the
static panel engine through `layout::wrap_line`, so a Markdown wrap path must
not grow its own divergent width counter.

## Decision

**Inline parsing → `streamdown-parser`.** It is line-oriented and resets its
formatting state at the end of each `parse()` call, so emphasis is line-local
for free — exactly the streaming property aic needs (one partial line is
re-parsed each delta, and an unclosed `**bold` renders optimistically until the
closer arrives, then self-corrects). aic maps its `InlineElement`s onto a
three-variant `Span` (Bold / Code / Plain): italic/underline/strike/footnote
collapse to Plain (terminal italic is unreliable and aic committed to bold +
code), and link/image URLs are dropped (the transient window shows text, not
targets). The mapping is an exhaustive `match` with no wildcard, so a new
`InlineElement` variant is a compile error rather than a silent misrender.

**Syntax highlighting → `syntect`** with `default-features = false` and only
`default-syntaxes`, `default-themes`, `parsing`, `regex-fancy`. The bundled
syntax/theme dumps decode once, lazily, on the first fenced code block (never
at startup), and `base16-eighties.dark` is used. Per-token foreground colours
become `TrueColor` `console` styles; regions matching the theme's default
foreground are left unstyled so plain code text falls back to the terminal
default instead of vanishing into theme grey on a light background.

**Keep the raw-ANSI substrate.** No `ratatui` / `tui-markdown` / `bat`: those
target a TUI or a different paint model and would fight the existing
flicker-free repaint engine. The Markdown renderer stays a pure
`line → styled-rows` function family that the existing engine calls.

**Wrap geometry stays single-source.** The greedy word-wrap that both the
plain (`layout::wrap_line`) and the tagged Markdown paths need is now one
generic core — `layout::wrap_words(words, width) -> rows of word-slices` — so
a fix to the wrap geometry (CJK width, hard-break policy) reaches both paths.
`wrap_line` and the Markdown `wrap_runs` are thin collectors over it; the
ANSI-blind invariant holds because `wrap_words` never sees a style byte.

### Why not the alternatives

- **Hand-roll the inline scanner.** ~20 lines of mapping today, but the
  streaming/optimistic/line-local semantics are subtle (half-streamed `**bold`
  must render bold, not literal) and `streamdown-parser` already encodes them.
  Re-implementing them is a bug farm for a feature that re-parses every token
  delta.
- **A TUI Markdown lib (`tui-markdown`, `ratatui`-based).** Wrong substrate:
  the window is raw ANSI repainted in place, not a retained TUI. Rewriting the
  repaint engine to host a TUI widget is a far larger change for no gain.
- **`bat` for highlighting.** Heavier than `syntect` alone (it is a full
  pager) and pulls a TTY/terminal model aic does not want.
- **`syntect` default features on.** Pulls `default-fnt`/`metadata` and more;
  the lean feature set above is everything aic uses.

## Consequences

- The reasoning window renders Markdown (headings, lists, blockquotes, inline
  bold/code, highlighted fenced code) on the existing raw-ANSI painter — no
  repaint-engine rewrite.
- Two new render deps, both lazy and feature-gated. `syntect`'s ~MiB syntax +
  theme dumps decode once on first fenced code block; this is a one-time cost
  on the reasoning stream, not at startup or on non-reasoning runs.
- `streamdown-parser` is a young `0.x` crate; aic pins `= "0.1"` and the
  `InlineElement → Span` mapping is exhaustive, so a breaking upstream change
  surfaces as a compile error here rather than a silent regression.
- The wrap-geometry single-source (`wrap_words`) means the ANSI-blind width
  invariant is defended in one place; `layout` tests and a visible-width
  invariant test in `progress` guard it.
- Builds on ADR 0010 (CLI-agent backend streams into the same `on_reasoning`
  callback): the Markdown renderer is backend-agnostic — both the API and CLI
  reasoning feeds render identically.
