//! Pure row builders for the Steps timeline (ADR 0018).
//!
//! Everything here is a pure function of a step-state model → pre-styled
//! `Vec<String>` rows, byte-exact-testable exactly like the `display` and
//! `progress` suites (ADR 0018). Nothing writes to a terminal: the frame the
//! builders produce is repainted by the generalized
//! [`frame_bytes`](crate::render::progress) painter as the bottom-most region
//! (ADR 0018 §2 — no CUP, no scroll regions, ADR 0013 intact).
//!
//! One layout rule ([`timeline_frame`], ADR 0018): completed step lines
//! oldest-first, the active step line, the active step's live output, a blank
//! separator (only if pending steps exist), pending step lines. A failed step
//! renders `✗` + detail and every later step is absent from the frame — never
//! rendered, not even as pending.
//!
//! Marker/color vocabulary is fixed by the spec (ADR 0018): `·` muted
//! (pending), `⠋` pending (active — first glyph of the existing braille set),
//! `✓` success, `✗` failure, `?` caution, `›` menu cursor. Only existing
//! Palette roles; no new colors, no new dependencies.

use std::time::Duration;

use console::Style;

use crate::render::layout::{MARGIN, char_display_width, clamp_to_width, display_width};
use crate::render::palette;

pub(crate) mod driver;
#[cfg(test)]
mod tests;

/// Marker column geometry: the step marker sits at column 2 (past the shared
/// [`MARGIN`]), sub-rows and live-output content at column 4 (ADR 0018).
const SUB_INDENT: &str = "  ";

/// One styled-or-plain text segment of a row. Rendering concatenates segments
/// and truncates at the row's display-width budget so no row ever wraps or
/// carries trailing spaces (ADR 0018); styling is applied per segment after
/// truncation, which keeps width math ANSI-blind (ADR 0013).
struct Seg {
    text: String,
    style: Option<Style>,
}

/// Construct an unstyled segment.
fn seg(text: impl Into<String>) -> Seg {
    Seg {
        text: text.into(),
        style: None,
    }
}

/// Construct a styled segment.
fn styled(text: impl Into<String>, style: Style) -> Seg {
    Seg {
        text: text.into(),
        style: Some(style),
    }
}

/// Styling policy + row-width budget for the row builders — the pair every
/// builder needs, bundled so call sites carry one value instead of a
/// clumping `(width, &Theme)`. `colors == false` (non-TTY, NO_COLOR) renders
/// plain text, matching [`Display::styled`](crate::render::display::Display)'s
/// contract on the panel engine.
pub struct Theme {
    /// Whether ANSI styling is emitted at all.
    pub colors: bool,
    /// Row width budget (display columns) — the whole row including the
    /// leading [`MARGIN`].
    pub width: usize,
}

impl Theme {
    fn paint(&self, text: &str, style: Option<Style>) -> String {
        match style {
            Some(s) if self.colors => s.apply_to(text).to_string(),
            _ => text.to_string(),
        }
    }
}

/// Render segments into one row, truncated (never wrapped) at the theme's
/// width budget — for the whole row including the leading [`MARGIN`].
/// Empty segments emit nothing (a styled empty string would embed stray
/// escape pairs mid-row), so no trailing spaces are ever produced.
fn render(segs: &[Seg], theme: &Theme) -> String {
    let mut out = String::new();
    let mut used = 0;
    for s in segs {
        if used >= theme.width {
            break;
        }
        let budget = theme.width - used;
        let mut piece = String::new();
        let mut w = 0;
        for c in s.text.chars() {
            let cw = char_display_width(c);
            if w + cw > budget {
                break;
            }
            w += cw;
            piece.push(c);
        }
        used += w;
        if piece.is_empty() {
            continue;
        }
        out.push_str(&theme.paint(&piece, s.style.clone()));
    }
    out
}

/// Lifecycle state of one Run Step (ADR 0018 §3).
#[derive(Default, Debug, Clone, Copy, PartialEq, Eq)]
pub enum StepState {
    #[default]
    Pending,
    /// Running — `⠋` marker, live output streams under the row.
    Active,
    /// Finished — collapsed to one `✓` summary line (+ durable rows).
    Completed,
    /// Failed — `✗` + error detail persists; later steps are not rendered.
    Failed,
}
/// The failure record for a failed step: the headline reason joins the step
/// label (`✗ Commit batches — failed on batch 2 of 3`); `detail` rows render
/// under the headline — the first in `failure`, the rest as muted remediation
/// hints (ADR 0018), capped to 3 rows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Failure {
    /// Short reason appended to the label after ` — `.
    pub reason: String,
    /// Detail lines: first = the error, subsequent = remediation.
    pub detail: Vec<String>,
}

impl Failure {
    /// A failure whose one detail row is the whole error chain — the shape
    /// behind every generic error exit (`reason: "failed"`) and the
    /// Redaction Gate refusals.
    pub fn from_error(reason: impl Into<String>, e: &anyhow::Error) -> Self {
        Self {
            reason: reason.into(),
            detail: vec![format!("{e:#}")],
        }
    }
}

/// One Run Step of the timeline (CONTEXT.md: Run Step). `live_rows` are the
/// active step's pre-styled live output (reasoning-window gutter rows,
/// sub-rows, menus); `durable_rows` persist under a completed step's summary
/// line (Commit entries, resolve verdicts, fallback notices — ADR 0018).
#[derive(Default, Debug, Clone)]
pub struct Step {
    /// Skeleton label, shown from frame one (`Scan changes`, `Plan batches`).
    pub label: String,
    /// Lifecycle state.
    pub state: StepState,
    /// Completion summary headline (`Planned 3 batches`); `None` falls back
    /// to the label.
    pub summary: Option<String>,
    /// Wall time of a completed step; shown only when ≥ 1 s (ADR 0018).
    pub duration: Option<Duration>,
    /// Live elapsed counter for the active step's label (`⠋ Plan batches · 12s`,
    /// integer seconds — ADR 0018).
    pub active_elapsed: Option<Duration>,
    /// Live output under the active step's row (pre-styled, ephemeral).
    pub live_rows: Vec<String>,
    /// Rows that persist under a completed step's summary line (pre-styled).
    pub durable_rows: Vec<String>,
    /// The completed step's block has been peeled to permanent scrollback
    /// (chronological collapse, ADR 0018 amendment 3): the frame no longer
    /// renders it — its ✓ block lives above the frame as plain history.
    pub peeled: bool,
    /// The failure record once the step failed.
    pub failure: Option<Failure>,
}

impl Step {
    /// A pending skeleton step with the given label — the frame-one shape.
    pub fn pending(label: impl Into<String>) -> Self {
        Self {
            label: label.into(),
            ..Default::default()
        }
    }

    /// An active step with the given label (test construction; production
    /// code flips `state` via the driver).
    #[cfg(test)]
    pub fn active(label: impl Into<String>) -> Self {
        Self {
            state: StepState::Active,
            ..Self::pending(label)
        }
    }

    /// A completed step with the given summary headline (test construction).
    #[cfg(test)]
    pub(crate) fn completed(label: impl Into<String>, summary: impl Into<String>) -> Self {
        Self {
            state: StepState::Completed,
            summary: Some(summary.into()),
            ..Self::pending(label)
        }
    }

    /// A failed step with the given failure record (test construction).
    #[cfg(test)]
    pub(crate) fn failed(
        label: impl Into<String>,
        reason: impl Into<String>,
        detail: Vec<String>,
    ) -> Self {
        Self {
            state: StepState::Failed,
            failure: Some(Failure {
                reason: reason.into(),
                detail,
            }),
            ..Self::pending(label)
        }
    }
}

/// Index of one Run Step in the timeline skeleton. Newtyped so a sub-row
/// number or a row count can never be passed where a Step goes — every
/// [`RunFrame`](driver::RunFrame) method that addresses a Step takes this.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct StepIdx(pub(crate) usize);

impl StepIdx {
    /// The always-first step (Scan / Detect) of every skeleton.
    pub(crate) const FIRST: Self = Self(0);
}

/// Allocates [`StepIdx`] values in display order for a skeleton whose
/// conditional steps are decided one at a time — the shared shape of the
/// commit Run's and the resolve flow's skeleton bookkeeping, so the label
/// list and the index bookkeeping cannot drift apart in shape.
#[derive(Debug, Clone, Copy)]
pub(crate) struct StepIx(usize);

impl StepIx {
    /// Start allocating after the caller's [`StepIdx::FIRST`] step.
    pub(crate) const AFTER_FIRST: Self = Self(1);

    /// The next step index.
    pub(crate) fn take(&mut self) -> StepIdx {
        let i = StepIdx(self.0);
        self.0 += 1;
        i
    }

    /// The next index only when the conditional step runs.
    pub(crate) fn take_if(&mut self, on: bool) -> Option<StepIdx> {
        on.then(|| self.take())
    }
}

/// Duration formatting rule (ADR 0018): shown only when a step took ≥ 1 s,
/// one decimal (`6.4s`); sub-second steps show no duration.
pub fn format_duration(d: Duration) -> Option<String> {
    if d < Duration::from_secs(1) {
        None
    } else {
        Some(format!("{:.1}s", d.as_secs_f64()))
    }
}

/// The active step's live elapsed counter: integer seconds (`12s`), ADR 0018
/// — coarser than [`format_duration`] because it ticks every frame. The
/// join-at-≥1s rule lives in [`step_line`]; this only formats.
pub fn format_elapsed(d: Duration) -> String {
    format!("{}s", d.as_secs())
}

/// The plain completion headline of a completed step: summary (or label),
/// plus ` · <duration>` when it took ≥ 1 s. Shared verbatim by the styled
/// [`step_line`] path and the non-TTY [`plain_step_lines`] path (ADR 0018:
/// same strings, no styling).
fn completed_text(step: &Step) -> String {
    let headline = step.summary.as_deref().unwrap_or(&step.label);
    match step.duration.as_ref().and_then(|d| format_duration(*d)) {
        Some(d) => format!("{headline} · {d}"),
        None => headline.to_string(),
    }
}

/// The failed step's headline: `label — reason`, or the bare label when the
/// failure record is absent or its reason empty. Shared verbatim by the
/// styled [`step_line`] path and the non-TTY [`plain_step_lines`] path.
fn failed_headline(step: &Step) -> String {
    let reason = step
        .failure
        .as_ref()
        .map(|f| f.reason.as_str())
        .unwrap_or_default();
    if reason.is_empty() {
        step.label.clone()
    } else {
        format!("{} — {reason}", step.label)
    }
}

/// One step row (ADR 0018): `  <marker> <label>`, truncated never
/// wrapped, no trailing spaces. All four states:
///
/// - pending — `·` muted marker + muted label;
/// - active — `⠋` pending marker + default-fg bold label + optional
///   ` · <n>s` elapsed detail in `neutral_gray`. The glyph alone carries
///   the yellow: a whole yellow-bold row would be the loudest thing on
///   the screen, outshouting the durable record (motion already says
///   "working"); yellow stays reserved for "needs your attention".
/// - completed — `✓` success marker, default-fg summary, `neutral_gray` detail;
/// - failed — `✗` failure marker + `label — reason` headline in `failure`.
pub fn step_line(step: &Step, theme: &Theme) -> String {
    let mut segs = vec![seg(MARGIN)];
    match step.state {
        StepState::Pending => {
            segs.push(styled("· ", palette::muted()));
            segs.push(styled(step.label.clone(), palette::muted()));
        }
        StepState::Active => {
            segs.push(styled("⠋ ", palette::pending()));
            // Glyph-only yellow — see the state list above.
            segs.push(styled(step.label.clone(), palette::emphasis()));
            // The live elapsed joins only from 1 s — matching the completed
            // state's < 1 s rule, so a fast step never flashes `· 0s`.
            if let Some(e) = step.active_elapsed
                && e.as_secs() >= 1
            {
                segs.push(styled(
                    format!(" · {}", format_elapsed(e)),
                    palette::neutral_gray(),
                ));
            }
        }
        StepState::Completed => {
            segs.push(styled("✓ ", palette::success()));
            let headline = step.summary.clone().unwrap_or_else(|| step.label.clone());
            segs.push(seg(headline));
            if let Some(d) = step.duration.as_ref().and_then(|d| format_duration(*d)) {
                segs.push(styled(format!(" · {d}"), palette::neutral_gray()));
            }
        }
        StepState::Failed => {
            let headline = failed_headline(step);
            segs.push(styled("✗ ", palette::failure()));
            segs.push(styled(headline, palette::failure()));
        }
    }
    render(&segs, theme)
}

/// The failed step's detail rows under the headline (ADR 0018): first line
/// in `failure`, subsequent lines muted remediation, capped to 3 rows,
/// truncated never wrapped.
fn failure_detail_rows(step: &Step, theme: &Theme) -> Vec<String> {
    let Some(failure) = step.failure.as_ref() else {
        return Vec::new();
    };
    failure
        .detail
        .iter()
        .take(3)
        .enumerate()
        .map(|(i, line)| {
            let style = if i == 0 {
                palette::failure()
            } else {
                palette::muted()
            };
            render(
                &[seg(MARGIN), seg(SUB_INDENT), styled(line.clone(), style)],
                theme,
            )
        })
        .collect()
}

/// The whole visible frame (ADR 0018, the one layout rule): completed step
/// lines oldest-first (tight), the active step line, its live output, one
/// blank separator (only if pending steps exist), pending step lines. A
/// durable row block under a completed step keeps one blank row after it
/// before the next step line (ADR 0018). A failed step ends the frame —
/// every later step is absent, never rendered as pending.
pub fn timeline_frame(steps: &[Step], theme: &Theme) -> Vec<String> {
    let mut rows = Vec::new();
    for (i, step) in steps.iter().enumerate() {
        if step.peeled {
            continue;
        }
        match step.state {
            StepState::Pending => rows.push(step_line(step, theme)),
            StepState::Active => {
                rows.push(step_line(step, theme));
                rows.extend(step.live_rows.iter().cloned());
                let pending_below = steps[i + 1..].iter().any(|s| s.state == StepState::Pending);
                if pending_below {
                    rows.push(String::new());
                }
            }
            StepState::Completed => {
                rows.push(step_line(step, theme));
                let before = rows.len();
                rows.extend(step.durable_rows.iter().cloned());
                if rows.len() > before && i + 1 < steps.len() {
                    rows.push(String::new());
                }
            }
            StepState::Failed => {
                rows.push(step_line(step, theme));
                rows.extend(failure_detail_rows(step, theme));
                return rows;
            }
        }
    }
    rows
}

/// The body of one live sub-row (ADR 0018): an in-flight label
/// (`⠋ Drafting f2.rs`), a done row (`✓ [text] [· duration]`), or a landed
/// commit echo (`✓ a1b2c3d feat: first`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SubBody {
    /// In-flight: spinner glyph + label in `pending`.
    InFlight(String),
    /// Done: `✓` in `success`, optional default-fg text (file path, hash
    /// line), optional duration shown only when ≥ 1 s.
    Done(String, Option<Duration>),
    /// A landed commit's in-frame echo: `✓` in `success`, the short hash in
    /// amber bold, then the subject through the shared
    /// [`palette::subject_segments`] — the same grammar the flushed Commit
    /// entry renders, so the live row and the durable row of one batch read
    /// identically (the pre-flush echo used to lose every color anchor).
    Landed { hash: String, subject: String },
}

/// Clamp live-row text after its fixed chrome (ADR 0018's no-wrap
/// contract): every live row spends the inset (4) plus its marker columns
/// (`⠋ `, `✓` + space, `› `, `? `) before the text, and the painter's
/// whole-frame repaint assumes one physical row per row — so the text
/// clamps to `width − chrome` with `…` marking the cut. `width` is the
/// budget the surface owns: frame rows the theme's prose width, sub-row
/// callers the raw terminal width (flushed table rows budget to the real
/// edge).
pub(crate) fn clamp_row_text(text: &str, width: usize, chrome: usize) -> String {
    clamp_to_width(text, width.saturating_sub(chrome))
}

/// One live sub-row at column 4 (ADR 0018): an in-flight label, a done row,
/// or a landed-commit echo. No `[i/N]` numbering — rows land in happening
/// order and every row of the step shares one shape: `⠋ Drafting f2.rs` /
/// `✓ f2.rs · 6.2s` / `✓ a1b2c3d feat: first`. Text clamps to `width` (the
/// terminal's raw column count; `…` marks the cut) so a live row can never
/// wrap to two physical rows: sub-row text is the one frame content with no
/// builder-side budget (commit subjects are prompt-capped at 72 chars but
/// nothing validates that), and the painter's whole-frame repaint assumes
/// one physical row per row.
pub fn sub_row(body: &SubBody, width: usize, theme: &Theme) -> String {
    let mut segs = vec![seg(MARGIN), seg(SUB_INDENT)];
    match body {
        SubBody::InFlight(label) => {
            segs.push(styled("⠋ ", palette::pending()));
            // Glyph-only yellow, matching the active step row; the text
            // clamps to the no-wrap budget (inset 4 + `⠋ ` 2).
            segs.push(styled(clamp_row_text(label, width, 6), palette::emphasis()));
        }
        SubBody::Done(text, duration) => {
            segs.push(styled("✓", palette::success()));
            let dur = duration.as_ref().and_then(|d| format_duration(*d));
            // The duration suffix (joiner + `6.2s`) rides the row after the
            // text, so it spends into the text's budget first — a clamped
            // subject must not push its own duration past the edge.
            let suffix = match &dur {
                Some(d) => (if text.is_empty() { 1 } else { 3 }) + display_width(d),
                None => 0,
            };
            // Inset (4) + `✓` (1) + the text's leading space (1).
            let text = clamp_row_text(text, width, 6 + suffix);
            if !text.is_empty() {
                segs.push(seg(format!(" {text}")));
            }
            if let Some(d) = dur {
                let joiner = if text.is_empty() { " " } else { " · " };
                segs.push(styled(format!("{joiner}{d}"), palette::neutral_gray()));
            }
        }
        SubBody::Landed { hash, subject } => {
            segs.push(styled("✓", palette::success()));
            segs.push(styled(
                format!(" {hash}"),
                palette::commit_id_color().bold(),
            ));
            // Inset (4) + ✓ (1) + the hash's leading space (1) + the hash
            // itself + the joiner space (1) — the same no-wrap contract as
            // the other variants: the subject is the one live-row text with
            // no builder-side budget (prompt-capped at 72 chars, nothing
            // validates that). Clamping the plain subject before
            // `subject_segments` keeps the budget ANSI-blind.
            let subject = clamp_row_text(subject, width, 7 + display_width(hash));
            if !subject.is_empty() {
                segs.push(seg(" "));
                for (text, style) in palette::subject_segments(&subject) {
                    segs.push(styled(text, style));
                }
            }
        }
    }
    render(&segs, theme)
}

/// A bare options menu (the commit-confirm four: Commit / Re-generate /
/// Edit / Abort; the review trio: Continue / Merge… / Abort): `› ` marks the
/// selected option (ADR 0018 — replaces inquire's own cursor); other
/// options indent 2 past it. Returns the rows plus the index of the row the
/// `›` cursor sits on, so key handling can move it without re-deriving
/// geometry.
pub fn menu_rows(options: &[&str], selected: usize, theme: &Theme) -> (Vec<String>, usize) {
    // The menu rides the live frame, where a wrapped row desyncs the
    // whole-frame repaint — options clamp to the theme's width (the
    // resolved prose width, always ≤ the raw terminal): inset (4) + the
    // cursor/indent column pair (2) precede every option.
    let rows = options
        .iter()
        .enumerate()
        .map(|(i, opt)| {
            let opt = clamp_row_text(opt, theme.width, 6);
            if i == selected {
                render(
                    &[
                        seg(MARGIN),
                        seg(SUB_INDENT),
                        styled("› ", palette::emphasis()),
                        seg(opt),
                    ],
                    theme,
                )
            } else {
                render(&[seg(MARGIN), seg(SUB_INDENT), seg("  "), seg(opt)], theme)
            }
        })
        .collect();
    (rows, selected.min(options.len().saturating_sub(1)))
}

/// The Batch Review menu frame rows (ADR 0018): a `?` ask headline, the plan
/// listing (`#i <entry>`), then the options menu with the `›` cursor.
/// Returns rows plus the cursor row index (into the returned rows).
pub fn review_menu_rows(
    headline: &str,
    entries: &[String],
    options: &[&str],
    selected: usize,
    theme: &Theme,
) -> (Vec<String>, usize) {
    let mut rows = vec![render(
        &[
            seg(MARGIN),
            seg(SUB_INDENT),
            styled("? ", palette::caution()),
            styled(headline, palette::caution()),
        ],
        theme,
    )];
    for e in entries {
        rows.push(render(
            &[
                seg(MARGIN),
                seg(SUB_INDENT),
                seg("  "),
                styled(e.clone(), palette::neutral_gray()),
            ],
            theme,
        ));
    }
    let (menu, cursor) = menu_rows(options, selected, theme);
    rows.extend(menu);
    (rows, 1 + entries.len() + cursor)
}

/// A one-row interactive ask (y/n prompts, ADR 0018): `? ` + question in
/// `caution`, at the sub-row column.
pub fn ask_row(question: &str, theme: &Theme) -> String {
    // Same no-wrap contract as `menu_rows`: inset (4) + `? ` (2) precede
    // the question.
    let question = clamp_row_text(question, theme.width, 6);
    render(
        &[
            seg(MARGIN),
            seg(SUB_INDENT),
            styled("? ", palette::caution()),
            styled(question, palette::caution()),
        ],
        theme,
    )
}

/// One transient live note row under the active step (ADR 0018's Re-generate
/// loop): margin + sub-row indent + spinner glyph + label — glyph-only
/// yellow like every active-work row.
pub fn note_row(label: &str, theme: &Theme) -> String {
    render(
        &[
            seg(MARGIN),
            seg(SUB_INDENT),
            styled("⠋ ", palette::pending()),
            styled(label.to_string(), palette::emphasis()),
        ],
        theme,
    )
}

/// The non-TTY ledger lines for one step (ADR 0018): pending and active steps
/// never print; a completed step appends its plain `✓` summary (same strings
/// as the styled path, no styling); a failed step appends `✗` headline plus
/// its detail lines (capped like the TTY path). Commit entries and other
/// durable rows are printed by their owners, not here.
pub fn plain_step_lines(step: &Step) -> Vec<String> {
    match step.state {
        StepState::Pending | StepState::Active => Vec::new(),
        StepState::Completed => vec![format!("✓ {}", completed_text(step))],
        StepState::Failed => {
            let headline = failed_headline(step);
            let mut rows = vec![format!("✗ {headline}")];
            if let Some(f) = step.failure.as_ref() {
                rows.extend(f.detail.iter().take(3).cloned());
            }
            rows
        }
    }
}

/// Below this many rows the live window is suppressed entirely (ADR 0018):
/// the spinner label + elapsed alone carry the wait.
pub const MIN_WINDOW_ROWS: usize = 3;

/// The live window's height budget (ADR 0018):
/// `window_rows = min(max_rows, terminal_height − timeline_fixed_rows − 2)`
/// where `timeline_fixed_rows` is every fixed row of the frame (all step
/// rows, the separator, already-durable rows). A result below
/// [`MIN_WINDOW_ROWS`] suppresses the window (returns 0). The
/// `WindowSizing.max_rows` cap (40 fit / 12 scroll) still applies on top via
/// the `max_rows` argument. Pure.
pub fn window_height_budget(
    max_rows: usize,
    terminal_height: usize,
    timeline_fixed_rows: usize,
) -> usize {
    let rows = max_rows.min(terminal_height.saturating_sub(timeline_fixed_rows + 2));
    if rows < MIN_WINDOW_ROWS { 0 } else { rows }
}

// ----------------------------------------------------------------------
// Step-summary formatting — the shared vocabulary of every step's `✓`
// headline (counts, plurals, deltas). Lives with the timeline so both
// flows (run, resolve) and the Review gate import one source.
// ----------------------------------------------------------------------

/// `"{n} {word}"` with English pluralization — the ledger's step summaries
/// (`✓ Scanned 1 file`, `✓ Planned 2 batches`).
pub(crate) fn plural(n: usize, word: &str) -> String {
    let plural = if word.ends_with("ch") || word.ends_with("sh") || word.ends_with(['s', 'x', 'z'])
    {
        format!("{word}es")
    } else {
        format!("{word}s")
    };
    if n == 1 {
        format!("{n} {word}")
    } else {
        format!("{n} {plural}")
    }
}

/// Compact count for delta summaries (`2.1k`, `15k`, `1.2m`) — above a
/// thousand the exact number no longer earns its columns (ADR 0018
/// redesign). Below it, the plain number.
fn compact_count(n: usize) -> String {
    let unit = |n: usize, divisor: usize, suffix: &str| {
        let tenths = (n * 10 + divisor / 2) / divisor; // round half up
        let (whole, frac) = (tenths / 10, tenths % 10);
        if frac == 0 || whole >= 10 {
            format!("{whole}{suffix}")
        } else {
            format!("{whole}.{frac}{suffix}")
        }
    };
    match n {
        0..=999 => n.to_string(),
        // 999_950+ rounds up to 1000k — roll into `m` instead of "1000k".
        // (The `m` arm has the same wart at ~1e9 lines; unreachable, left.)
        1_000..=999_999 if n >= 999_950 => unit(n, 1_000_000, "m"),
        1_000..=999_999 => unit(n, 1_000, "k"),
        _ => unit(n, 1_000_000, "m"),
    }
}

/// The Scan summary's aggregate text-line delta (` · +12 −3`), summed from
/// per-file stats (unstaged Runs read the workdir diff, staged Runs the
/// index). Always present: a zero delta still tells the reader the scan saw
/// no text-line churn (binary-only or mode-only changes).
pub(crate) fn delta_suffix(stats: &[crate::git::FileStats]) -> String {
    let added: usize = stats.iter().map(|s| s.added).sum();
    let deleted: usize = stats.iter().map(|s| s.deleted).sum();
    format!(" · +{} −{}", compact_count(added), compact_count(deleted))
}

/// One muted help row under a frame menu's options — the key hints the
/// inquire fallback shows via `with_help_message`, so the frame path keeps
/// parity (`↑/↓ select · Enter confirm · Esc or Ctrl-C aborts`). Clamped
/// like every other live row (inset 4 chrome) — a hint past the frame
/// width would wrap and desync the whole-frame repaint.
pub(crate) fn help_row(help: &str, theme: &Theme) -> String {
    render(
        &[
            seg(MARGIN),
            seg(SUB_INDENT),
            styled(clamp_row_text(help, theme.width, 4), palette::muted()),
        ],
        theme,
    )
}

/// The full row set one frame menu paints per key read (ADR 0018): the
/// optional ask row, the option rows ([`menu_rows`]), and the help row —
/// exactly what `frame_menu` hands the painter each loop, so the built
/// rows and the painted rows cannot drift (the original menu bug built
/// rows every loop but never painted them; e2e's non-TTY buffer cannot
/// see frame painting, so this assembly is the unit-testable seam).
pub(crate) fn menu_frame_rows(
    ask: Option<&str>,
    options: &[&str],
    selected: usize,
    help: &str,
    theme: &Theme,
) -> Vec<String> {
    let mut rows = ask.map(|q| vec![ask_row(q, theme)]).unwrap_or_default();
    let (menu, _) = menu_rows(options, selected, theme);
    rows.extend(menu);
    rows.push(help_row(help, theme));
    rows
}
