//! The TUI terminal guard (ADR 0020): raw mode + the alternate screen
//! buffer for the duration of a full-screen interactive surface, restored
//! on every exit path — normal return, `?` propagation, and panic (a hook
//! restores first, then chains to the previous hook so panic output still
//! prints). The alt screen plus ratatui's whole-frame diffed repaint make
//! "leftover output in the terminal" structurally impossible: every frame
//! is painted over the previous one and the buffer dies on exit, so no
//! intermediate frame can ever reach the user's scrollback.
//!
//! Chrome renders to **stderr** (`CrosstermBackend::new(io::stderr())`), so
//! a piped stdout still carries only real results — the same stream split
//! the raw-ANSI surfaces keep.
//!
//! Capability gate mirrors the old `AltScreen`: on Unix any TTY speaks the
//! escapes; on Windows only when the console crate's color path is active
//! (its VT proxy — old conhost would print sequences as text). An incapable
//! terminal means the caller refuses to start rather than emitting garbage.

use std::io;

use anyhow::Context;
use crossterm::cursor::Show;
use crossterm::execute;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use ratatui::{Terminal, backend::CrosstermBackend};

/// Whether a full-screen TUI can run on this terminal: stderr must be a TTY
/// and (Windows) VT processing must be active. Pure capability check — no
/// side effects.
pub(crate) fn tui_capable() -> bool {
    #[cfg(windows)]
    let capable = console::colors_enabled_stderr();
    #[cfg(unix)]
    let capable = true;
    console::Term::stderr().is_term() && capable
}

/// Owns the terminal for one full-screen TUI session. `enter` flips raw mode
/// on and switches to the alternate screen; [`Drop`] undoes both, so every
/// unwinding path — including panics, after the hook restores — lands the
/// user back in their untouched shell.
pub(crate) struct Tui {
    terminal: Terminal<CrosstermBackend<io::Stderr>>,
}

impl Tui {
    /// Take the terminal over. Installs the panic-restore hook first so even
    /// a panic between setup steps leaves a usable shell behind.
    pub(crate) fn enter() -> anyhow::Result<Self> {
        install_panic_restore();
        enable_raw_mode().context("failed to enable raw mode")?;
        let mut term = Terminal::new(CrosstermBackend::new(io::stderr()))
            .context("failed to open the terminal")?;
        term.hide_cursor().context("failed to hide the cursor")?;
        execute!(io::stderr(), EnterAlternateScreen).context("failed to enter alt screen")?;
        Ok(Self { terminal: term })
    }

    /// Paint one whole frame through `render`. ratatui diffs against the
    /// previous frame, so repainting is idempotent and never stacks.
    pub(crate) fn draw<F>(&mut self, render: F) -> anyhow::Result<()>
    where
        F: FnOnce(&mut ratatui::Frame),
    {
        self.terminal
            .draw(render)
            .context("failed to draw a frame")?;
        Ok(())
    }
}

impl Drop for Tui {
    fn drop(&mut self) {
        let _ = execute!(io::stderr(), LeaveAlternateScreen);
        let _ = disable_raw_mode();
        let _ = execute!(io::stderr(), Show);
    }
}

/// Restore the terminal from inside a panic: leave the alt screen and drop
/// raw mode *before* the default hook prints, so panic output lands in the
/// user's normal buffer and the shell is usable afterwards. Idempotent by
/// construction — the `Drop` restore after unwinding is a harmless no-op on
/// an already-restored terminal (the escapes are simply ignored outside the
/// alt screen).
fn install_panic_restore() {
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let _ = execute!(io::stderr(), LeaveAlternateScreen);
        let _ = disable_raw_mode();
        previous(info);
    }));
}

#[cfg(all(unix, test))]
mod tests {
    use super::Tui;
    use crate::render::timeline::driver::pty_tests::{capture_until_exit, fork_on_pty};

    /// ADR 0020's alt-screen contract, pinned on a real PTY: exactly one
    /// `ESC[?1049h`/`ESC[?1049l` pair per session and zero full-screen
    /// `ESC[2J` clears — the whole-frame diffed repaint never clears.
    #[test]
    fn one_alt_screen_pair_and_zero_full_clears_per_session() {
        fn drive() {
            let mut tui = Tui::enter().expect("enter");
            let _ = tui.draw(|f| {
                f.render_widget(ratatui::widgets::Paragraph::new("x"), f.area());
            });
            drop(tui);
        }
        let (pid, master, slave) = fork_on_pty(drive);
        let bytes = capture_until_exit(pid, master, slave);
        assert_eq!(
            bytes.matches("\x1b[?1049h").count(),
            1,
            "one alt-screen enter per session"
        );
        assert_eq!(
            bytes.matches("\x1b[?1049l").count(),
            1,
            "one alt-screen leave per session"
        );
        assert_eq!(
            bytes.matches("\x1b[2J").count(),
            0,
            "diffed repaint must never full-clear"
        );
    }
}
