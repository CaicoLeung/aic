# ADR 0006: Unify interactive menus on `inquire`, drop `dialoguer`

- **Status:** Accepted
- **Date:** 2026-08-07

## Context

The setup wizard (issue #93) rendered text prompts with `inquire` but its
menus with `dialoguer::Select`, so two prompt libraries drew slightly
different widgets on the same screen. `opt_nav` also carried `dialoguer`-specific
error kind-matching (`dialoguer::Error::IO` → `io::ErrorKind`) instead of
inquire's native cancel variants.

The wizard, `confirm_menu`, and `prompt_shell` together were the only
`dialoguer::Select` call sites; `dialoguer` (and its transitive `shell-words`)
was pulled in for nothing else.

## Decision

Migrate every remaining `dialoguer::Select` call site to `inquire::Select` and
remove `dialoguer` entirely (−1 direct dependency; `shell-words` drops out
transitively). `opt_nav` keeps its index-based dispatch by using
`inquire::Select::raw_prompt()` (which returns the `ListOption` with the
chosen index) and now maps inquire's native variants —
`OperationCanceled` (Esc) → `Back`, `OperationInterrupted` (Ctrl-C) /
closed-stdin `IO` → `Cancel`. The shared `config::is_io_cancel` predicate
covers the hard-cancel (Ctrl-C / EOF) sub-clause for both the wizard's
`opt_nav` and `main.rs`'s `is_graceful_cancel`.

`confirm_menu`'s zero-residue contract (the caller's exact `clear_last(rows)`
preview erase) is preserved height-independently: `DECSC` save-cursor before
the prompt, `DECRC` restore + `clear_to_end_of_screen` after — correct across
submit / Esc / Ctrl-C. The stream choice (`Term::stderr()`) matches inquire's
own default render stream, so the save/restore and the prompt draw share one
terminal stream.

## Consequences

**Positive:** one prompt library, one rendering style, one cancel/error model.
`ColorfulTheme` threading is gone, so every menu is a one-liner
`opt_nav(prompt, &items, default)`. −45 lines net.

**Behavior changes (deliberate, disclosed against the PR's "no behavior change"
framing):** migrating to inquire carries three deltas that cannot be fully
restored without forking the crate:

1. **Type-to-filter is disabled.** inquire's `Select` filters the option list
   as the user types (`filter_input_enabled` defaults to `true`), which would
   add an unfamiliar filter input line and re-bind letter keys. Every
   `Select::new` is built `.without_filtering()` so typing a letter is a clean
   no-op, matching the dialoguer behavior this replaced.
2. **Menu output moved stdout → stderr.** `dialoguer::Select` wrote to stdout;
   inquire 0.7.5 hardcodes `Term::stderr()` in its (private) `terminal` module
   with no public override. On a single TTY this is invisible; it only matters
   if a caller pipes `aic`'s stdout while keeping stderr on the terminal — in
   which case the menu now appears on stderr. Accepted as more correct
   (interactive prompts belong on stderr; data on stdout) and consistent with
   how the text prompts already behave.
3. **`q` no longer quits.** dialoguer bound `q` as a quit key; inquire has no
   public keybinding hook, so `q` is now a no-op. Esc and Ctrl-C still abort
   (`confirm_menu`/`prompt_shell`) / go back (`opt_nav`), which are the
   documented cancel paths.

These are accepted trade-offs for unifying on one prompt library; revisit means
forking `inquire` or re-introducing `dialoguer`, neither warranted for a
cosmetic follow-up.
