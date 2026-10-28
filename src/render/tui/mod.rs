//! The TUI substrate (ADR 0020): the shared full-screen interactive
//! primitives every surface composes — the terminal guard (raw mode + alt
//! screen + panic restore), the palette→ratatui style bridge, the
//! filterable list picker, and the text input. Rendering is whole-frame and
//! diffed (ratatui), so a screen is one atomic repaint and leftover-output
//! bugs are structurally impossible — the class of defect the opt-in
//! per-screen `clear_frame` calls kept reproducing (three consecutive
//! patch-fixes before ADR 0020).
//!
//! Scope note (ADR 0020): the Run timeline keeps its raw-ANSI whole-frame
//! painter (ADR 0018) until its own migration phase; this module is the
//! home the migrated surfaces grow into.

pub(crate) mod picker;
pub(crate) mod style;
pub(crate) mod terminal;
pub(crate) mod text;
