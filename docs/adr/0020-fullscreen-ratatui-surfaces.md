# ADR 0020: Full-screen ratatui surfaces — whole-frame diffed repaint, staged migration

- **Status:** Accepted
- **Date:** 2026-10-28

## Context

ADR 0007 unified interactive menus on `inquire`, but inquire draws by
printing frames into the normal buffer and optionally clearing per screen.
`aic setup` stacked those frames into the user's scrollback whenever a
clear was missed or an emulator retained alt-screen history (reproduced on
a PTY: six menu renders with zero `ESC[2J` between them). The same defect
class threatens every interactive surface: the confirm menu, the Batch
Review menu, the Run's live viewport. Each hand-rolled patch (opt-in
clears, scrollback purges, DECSC/DECRC save-restore) fights the same root
cause — printing a frame and then erasing it is not a render model.

ratatui inverts the model: the app describes the whole screen, the
terminal diffs it against the previous frame, and the alternate screen
guarantees no intermediate frame ever reaches the user's scrollback. That
is a structural fix, not another clearing convention. ADR 0013 banned TUI
frameworks to keep the streaming renderer dependency-light; ratatui is a
pure diffing backend (no async runtime, no event loop opinions — crossterm
is pulled in for input/raw-mode), and this ADR amends 0013 to permit
exactly this pair for *full-screen* surfaces while the raw-ANSI engine
keeps owning inline (Run-time) rendering.

ADR 0018 froze the Run's inline viewport model; nothing here touches it.

## Decision

Migrate aic's interactive surfaces to ratatui **full-screen apps**, staged:

1. **Setup wizard** (this ADR's cutover). `src/render/tui/` gains the
   substrate: `terminal::Tui` (alt-screen guard, panic restore,
   `tui_capable()` VT probe), `style` (palette role twins — `console`
   keeps owning the inline roles, ratatui reads the same palette triples),
   `picker` (filterable single-choice list with Esc-back semantics), and
   `text` (line editor with submit/back). `workflow/setup/app.rs` is the
   wizard's state machine: one `Screen` enum, whole-frame `draw` per
   state, `on_key` dispatch mirroring the inquire flow graph, and
   background jobs (`std::thread` + mpsc) so probes and model fetches
   never block the render loop. Verify outcomes fold their ✓/✗ marker
   into the owning top-menu row instead of a standalone status line.
   `run_setup` refuses non-TTY stdin and non-VT terminals outright.
2. **Confirm + Batch Review menus** — same substrate, later phase.
3. **Run/resolve inline viewport** — stays raw-ANSI inline (ADR 0018);
   ratatui is only ever adopted there behind an Inline viewport, if at all.
4. **Reasoning widget** — follows the Run decision.
5. **`inquire`/opt-in-clear removal** — the shared-input layer shrinks to
   what non-full-screen flows still need (raw-mode y/n for piped tests).

Cross-cutting rules: setup-time network/probe work runs on plain threads
(nested current-thread runtimes; no `block_in_place` — the caller is not
inside a tokio worker), the wizard's pure logic (steps, labels, cache key,
draft↔config) stays in the existing sub-modules untouched, and every
screen must be re-enterable with its cursor restored (`Picker::new_at`).

`indicatif` is gone in this cutover — its only consumers were the setup
spinner paths the TUI Busy screen replaces.

## Consequences

- Setup frames can no longer leak into scrollback: one `ESC[?1049h`/`l`
  pair per session, zero `ESC[2J` — asserted by the PTY smoke.
- The wizard's interaction parity is preserved verbatim (filter, Esc-back,
  Ctrl-C cancel, dirty-discard confirm, Edit/Retry/Save-anyway) because
  `on_key` mirrors the old flow graph rather than redesigning it.
- `console::Style` cannot be converted to ratatui `Style` (private
  fields), so palette roles have explicit twins in `render::tui::style`
  reading the same theme triples — one source of truth, two projections.
- ADR 0007's inquire unification remains true for the surfaces still on
  it; this ADR supersedes 0007 for full-screen surfaces and amends 0013
  (TUI-framework ban → permitted for full-screen apps via ratatui) and
  0018 (no interaction — Run stays inline).
- Background jobs (model fetch, verify probes) are bounded by hard timeouts
  in `setup::verify::probe`, and the Busy screen is Esc-abortable (returns to
  the screen that started the job) with Ctrl-C canceling from every screen —
  a wedged network call can never trap the wizard.
