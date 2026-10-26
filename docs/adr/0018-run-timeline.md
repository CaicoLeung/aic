# ADR 0018: Run timeline — upfront Step list over a whole-frame repaint engine

- **Status:** Accepted
- **Date:** 2026-10-25

## Context

The runtime UI today is two surfaces sharing `layout`: a static panel engine
(`display` — commit lines, previews, footers; printed once, erased only via
`clear_last`) and a live bottom-anchored reasoning window (raw-ANSI
`frame_bytes` repaints). The user learns what a Run did only as panels
appear; nothing states upfront what the Run will do. The reasoning window
ends with a row-by-row dissolve (`DISSOLVE_STEP`, 70 ms per row) — a flourish
that delays the next output by `rows × 70 ms`.

The substrate has a hard gap: `clear_last` erases only rows contiguous with
the cursor, and `frame_bytes` assumes the frame is the bottom-most region.
There is no absolute cursor addressing and no scroll region, so "a fixed
step list at the top, updating while content streams beneath it" cannot be
painted with today's primitives. ADR 0013 bans adopting a TUI framework, so
whatever does this must be built on the raw-ANSI engine.

The redesign goals (user-driven): show every Step of the flow upfront; give
each Step visible loading / output / completed states; replace the dissolve
with a single whole-block erase when thinking ends; keep the visual language
consistent across every command.

## Decision

Flows paint a **Run timeline** (see CONTEXT.md: Run Step, Run timeline, Diff
bar) — the whole vocabulary is normative there; this ADR records the
mechanism.

1. **Fixed skeleton, drawn upfront.** The commit Run and the resolve flow
   each paint their Step list before doing any work. Config-conditional
   Steps (Redaction Gate, Batch Review, Commit confirmation) are included
   exactly when their config says they will run — the skeleton is never
   wrong. Counts and per-Batch sub-rows appear as discovered. The Conflict
   gate runs before drawing, so a Run that hands off to resolve paints the
   resolve timeline from the start.
2. **Whole-frame repaint.** `frame_bytes` generalizes from "the reasoning
   window" to "the live frame": the visible stack of completed Step lines,
   the active Step line with its live output beneath it, and the pending
   Step lines below, repainted as one pre-styled row vector on each tick.
   Completed Step lines are immutable, so rows that scroll off the top into
   scrollback freeze permanently — correct, because they never change
   again. No CUP/CSR escape sequences are introduced.
3. **Step lifecycle.** pending → active (live output streams under the
   Step's row) → completed (collapsed to one summary line, duration shown
   when ≥ 1 s) → failed (`✗` with the error persisted; later Steps are not
   rendered). Instant Steps (Scan, Redact) appear directly as `✓`.
4. **Reasoning window = the active plan Step's live output.** Kept while
   thinking (rolling window, unchanged policy), erased as **one whole
   frame** when thinking ends — the dissolve is deleted. Its row budget
   shrinks while the timeline is visible (terminal height minus timeline
   height).
5. **Draft Step replaces the drafting bars.** One Step, per-Batch sub-rows
   `[i/N]` (loading → ✓); the sub-rows are erased when the Step completes
   and the summary line carries the count. This supersedes ADR 0014's
   `[i/N]` MultiProgress *presentation* only — the concurrency itself is
   untouched. Draft calls get no reasoning window: streaming is not plumbed
   through `generate_commit_message`, and eight interleaved reasoning
   streams would be unreadable in one frame.
6. **Commit entries persist** under the Commit Step — the durable record —
   and are redesigned with the **Diff bar** (Σ-line upgrade: proportional
   +/- cells over text line counts; binary files show a byte-size change in
   their per-file row and never feed the bar).
7. **Non-TTY degrades to a ledger.** No animation: each Step appends its
   plain `✓` summary line as it completes; panels print plain text as today.
8. **No timelines outside Run and resolve.** The one-shot commands
   (`setup`, `use`, `list`, `update`, `undo`, `completion`) adopt the same
   symbols, colors, and spacing — not the timeline.

## Consequences

- **Positive:** the Run states its plan before doing anything; Run and
  resolve share one state vocabulary; a finished Run leaves a readable
  step ledger in scrollback; end-of-thinking no longer costs
  `rows × 70 ms`; the erase contract simplifies to one clear.
- **Negative:** repaint cost scales with the visible frame (bounded by the
  terminal height, pre-styled rows on a 50 ms tick — cheap, but no longer
  proportional to the active window alone); the ~70 exact-text render tests
  and the e2e footer asserts are rewritten against the new frame layout;
  the bottom-margin scroll path must now account for pending-Step rows
  above the frame's bottom (they repaint only while on screen).
- **Neutral:** `DISSOLVE_STEP` and its sleep loop are deleted outright;
  the DSR probe, `MARGIN` geometry, and Palette roles are reused as-is;
  interactive menus keep ADR 0007's zero-residue erase and render beneath
  the active Step.

## Amendment (2026-10-26): bounded frame, flushed entries, neon

First real-terminal use exposed two structural flaws and three polish gaps;
this amendment records the accepted resolutions.

1. **Ledger duplication on TTY (bug).** `complete()`/`fail()` emitted ledger
   lines through the real stderr `Display` even on a terminal, where the
   frame already paints them — double lines and cursor drift under
   `prev_rows`. Fixed: ledger emission is non-TTY only.
2. **Entries ride the frame (bug).** Commit entries lived in the frame as
   durable rows, so a 13-batch Run pushed the frame past terminal height —
   every repaint scrolled a full copy into scrollback. Fixed by **Model A
   flush**: `RunFrame::flush` erases the frame, prints the entry rows as
   permanent scrollback below it, then repaints the frame beneath. The frame
   is bounded by the terminal height again; entries never scroll twice.
   (Trap: the `Display` handed to `flush` must not be frame-sink-backed —
   re-entering the painter's mutex deadlocks.)
3. **Commit entry = one table row per batch.** The subject line carries the
   whole batch delta (`[n/m] ✓ hash subject · +A −D`); the file-stats
   footer stretches to the terminal's real edge (`Display::table_width`,
   raw columns − 4 — the 100-column prose cap applies only to in-frame
   rows); directories render muted with the basename in emphasis. The
   commit body is omitted from the entry — `git log` owns the prose; the
   pre-commit preview still shows it in full.
4. **Scan summary carries the aggregate delta** (`✓ Scanned 3 files ·
   +12 −3`): unstaged Runs sum the workdir diff (new `Git::workdir_stats`),
   staged Runs the index diff. Counts compact above a thousand (`2.1k`).
5. **Neon palette on dark terminals.** The dual-theme tables compromised
   every color to clear 3:1 on both white and dark. An OSC 11 background
   probe (same raw-termios input-safety contract as the DSR probe) runs
   once per process at the head of the Run/resolve workflows; a dark reply
   switches every truecolor palette entry to a neon variant clearing WCAG
   AA Normal (4.5:1) on dark. Light themes, pipes, `NO_COLOR`, and silent
   terminals keep the classic tables.
   Known limits, accepted: the probe is Unix-only, so Windows terminals
   stay classic even where OSC 11 is answered (Windows Terminal does); the
   tty check reads stdin only, so `CLICOLOR_FORCE` with piped output and a
   tty stdin can enable neon on a pipe; and the query is written to stdin's
   fd, mirroring the DSR probe's convention (read-only fd 0 degrades
   gracefully). None of these surface in normal terminal use.
