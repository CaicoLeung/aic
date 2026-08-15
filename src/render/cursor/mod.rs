//! The reasoning window's geometry: where the cursor is, and how many rows fit
//! below it.
//!
//! [`reasoning_window_rows`] is the one entry point — it probes the cursor's
//! 1-based row over DSR (`ESC[6n`) and returns a [`WindowSizing`] carrying the
//! rendered-row cap (`max_rows`) and the cursor row the in-place renderer paints
//! down from. Everything else here is the implementation of that probe: a
//! raw-termios guard, a byte-by-byte DSR reply reader with an input-safety
//! contract (never consume pending input, never leak a late reply), and the pure
//! row-budget + reply-parse helpers.
//!
//! Split out of `progress.rs`: the cursor probe is self-contained terminal I/O
//! with no shared state with the flicker-free renderer that consumes its result —
//! the renderer takes `max_rows` and `cursor_row` as plain numbers. Colocating
//! the termios/DSR machinery with the painting code it feeds made both harder to
//! read; this module gives the probe one address and one interface.

use console::Term;
use std::time::Duration;

use crate::render::layout::terminal_height;

/// Hard ceiling on the reasoning window. A very tall terminal could otherwise
/// paint dozens of rows, pushing the in-place region toward the bottom edge
/// where further growth would scroll into the scrollback (breaking the discard
/// model — reasoning must never linger). Well past any comfortable reading
/// window, so real terminals are essentially never clamped here in practice.
const MAX_REASONING_ROWS: usize = 40;

/// How many reasoning rows stay visible at once — a *rendered-row* cap, now
/// sized to the space **below the cursor** instead of a fixed 12.
///
/// The old fixed 12 was the root cause of "content scrolls past too fast to
/// read": at 12 rows a line is visible for only ~12 emission-intervals before
/// it rolls off the top of the window. Enlarging the window makes each line
/// linger proportionally longer — the window *is* the "invisible area" content
/// was scrolling out of, so growing it is the direct fix.
///
/// But the window is bounded by a hard geometric constraint: the in-place
/// renderer paints **downward from the cursor**, and cursor-down moves clamp
/// at the bottom margin instead of scrolling. A region taller than the rows
/// left below the cursor therefore **collapses** — several frame rows
/// overwrite the same bottom physical row, so the reasoning is invisible, and
/// the erase at [`ReasoningRenderer::finish`](crate::render::progress::ReasoningRenderer::finish) cannot reclaim what was never
/// cleanly painted. Sizing to the *full* terminal height (`h - 3`) assumed the
/// cursor starts in the top three rows; in reality it sits at the shell prompt
/// — mid-screen or lower — so a full-height window collapsed in the common
/// case. The window therefore queries the cursor's actual row (DSR) and sizes
/// to the rows genuinely available below it — capped at [`MAX_REASONING_ROWS`].
///
/// Two regimes, decided by how much room the cursor actually has below it:
///
/// * **Fit** — the cursor is at least a couple of rows off the bottom
///   (`height - cursor_row >= 2`): the frame (spinner + up to `max_rows`
///   reasoning rows) fits in the rows from the cursor's row to the bottom
///   (`height - cursor_row + 1`), so `max_rows = height - cursor_row` and the
///   screen never moves.
/// * **Scroll** — the cursor sits in the bottom two rows: there is no room to
///   fit a readable window, so the renderer grows the frame past the bottom
///   margin by scrolling the screen (a `\n` at the bottom margin, which the
///   cursor-down clamp would otherwise swallow). The cap is the pre-dynamic
///   fixed budget, clamped so the frame still fits on screen — no reasoning
///   row ever lands in the scrollback (the block is painted in place and
///   erased by [`ReasoningRenderer::finish`](crate::render::progress::ReasoningRenderer::finish)). The scrolls DO shift the
///   screen up one row per descent past the margin, so the user's prior
///   terminal output above the prompt advances into the scrollback by that
///   many rows — an unavoidable cost of painting a window at the bottom of a
///   full screen, and strictly better than the collapse that hid the
///   reasoning entirely. This is the common case: the shell prompt (and
///   therefore the frame's start row) usually sits on the last line of a full
///   screen, and the old fixed-12 window simply collapsed there.
///
/// When the cursor cannot be queried, [`REASONING_FALLBACK_ROWS`] (the
/// pre-dynamic fixed cap) is used and the renderer never scrolls — the
/// user-visible baseline before the sizing regression.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct WindowSizing {
    /// Rendered-row cap for the reasoning window.
    pub(crate) max_rows: usize,
    /// The cursor's 1-based row as reported by the DSR query (`None` when the
    /// query failed) — lets the renderer scroll the frame at the bottom
    /// margin. `None` disables scrolling (legacy behavior).
    pub(crate) cursor_row: Option<usize>,
}

impl WindowSizing {
    /// The no-query fallback: the pre-dynamic fixed cap with scrolling
    /// disabled — the user-visible baseline before the dynamic-window
    /// feature, and the graceful degradation when the cursor row cannot be
    /// queried at all (stdin/stderr not a terminal, query timeout, parse
    /// failure) or when the offloaded query task panicked off the async
    /// runtime. Decoration must never break the commit, so a failure here is
    /// the same as a failure inside [`reasoning_window_rows`].
    pub(crate) fn fallback() -> Self {
        Self {
            max_rows: REASONING_FALLBACK_ROWS,
            cursor_row: None,
        }
    }
}

pub(crate) fn reasoning_window_rows() -> WindowSizing {
    let h = terminal_height();
    let cursor_row = query_cursor_row().filter(|r| *r <= h);
    WindowSizing {
        max_rows: reasoning_rows_for(h, cursor_row),
        cursor_row,
    }
}

/// The window cap for a `height`-row terminal with the cursor on 1-based row
/// `cursor_row` (`None` = unknown). See [`reasoning_window_rows`] for the two
/// regimes: with the cursor known and at least two rows of room below it, the
/// frame fits without scrolling (`max_rows = height - cursor_row`); a cursor
/// in the bottom two rows has no room to fit a readable window, so the
/// renderer scrolls instead and the pre-dynamic budget applies (clamped so
/// the frame never exceeds the screen). With the cursor unknown, fall back to
/// the fixed cap that predates the dynamic window — the user-visible baseline
/// before the sizing regression (12 rows was the original [`ThinkingView`](crate::render::progress::ThinkingView)
/// budget).
fn reasoning_rows_for(height: usize, cursor_row: Option<usize>) -> usize {
    match cursor_row.filter(|r| *r <= height) {
        Some(r) => {
            let space = height - r; // rows below the cursor's own row
            if space >= 2 {
                space.min(MAX_REASONING_ROWS)
            } else {
                REASONING_FALLBACK_ROWS.min(height.saturating_sub(1))
            }
        }
        None => REASONING_FALLBACK_ROWS,
    }
}

/// Fallback window cap when the cursor row cannot be queried (stdin or stderr
/// not a terminal, unresponsive terminal, parse failure): the pre-dynamic
/// fixed budget. Matching the old behavior exactly means a failed query is
/// never *worse* than the experience before the dynamic-window feature.
const REASONING_FALLBACK_ROWS: usize = 12;

/// Query the cursor's current 1-based row via DSR (`ESC[6n`), consuming the
/// `ESC[<row>;<col>R` response byte-by-byte from stdin so nothing leaks into
/// later stdin reads (prompts). Returns `None` on any failure: stdin or stderr
/// not a terminal, no response within 200 ms, or an unparseable reply. The
/// terminal is restored to its prior mode on every path.
#[cfg(unix)]
fn query_cursor_row() -> Option<usize> {
    use std::os::fd::AsRawFd;

    let in_fd = std::io::stdin().as_raw_fd();
    if unsafe { libc::isatty(in_fd) } != 1 {
        return None;
    }
    if !Term::stderr().is_term() {
        return None;
    }
    query_cursor_row_core(in_fd)
}

/// RAII guard that switches a tty fd into raw input mode on construction and
/// restores the original termios on Drop — on every exit path, including
/// unwinding. The cursor-row query must never leave the user's terminal in
/// raw mode (canonical off, echo off — an unrecoverable-from-the-keyboard
/// state the user would have to `stty sane` out of), so the restore is tied
/// to the guard's lifetime rather than a manual `tcsetattr` at the end of
/// [`query_cursor_row_core`]. A panic or an early return inserted between
/// entering raw mode and that final call still unwinds the guard and
/// restores the terminal — the property the bare trailing restore could not
/// guarantee.
#[cfg(unix)]
struct RawTermios {
    fd: std::os::fd::RawFd,
    old: libc::termios,
}

#[cfg(unix)]
impl RawTermios {
    /// Read the current termios, switch the fd to raw input (canonical mode
    /// off so the DSR reply is not buffered until a newline it never
    /// carries), and preserve output processing (OPOST) so stderr stays
    /// translated during the brief raw window. Returns `None` if either
    /// termios call fails (not a tty, or permission denied) — the caller
    /// then skips the query without having entered raw mode.
    fn enter(fd: std::os::fd::RawFd) -> Option<Self> {
        unsafe {
            let mut old: libc::termios = std::mem::zeroed();
            if libc::tcgetattr(fd, &mut old) != 0 {
                return None;
            }
            let mut raw = old;
            libc::cfmakeraw(&mut raw);
            raw.c_oflag = old.c_oflag;
            if libc::tcsetattr(fd, libc::TCSANOW, &raw) != 0 {
                return None;
            }
            Some(Self { fd, old })
        }
    }
}

#[cfg(unix)]
impl Drop for RawTermios {
    fn drop(&mut self) {
        // Best-effort restore: a failure here is ignored (nothing useful
        // can be done from Drop without aborting, and `old` was read
        // successfully by `enter` so the value is sound). TCSANOW, not
        // TCSAFLUSH, so any input the drain left pending is preserved.
        unsafe {
            libc::tcsetattr(self.fd, libc::TCSANOW, &self.old);
        }
    }
}

/// Upper bound on the bytes consumed for one DSR reply (`ESC[<row>;<col>R`).
/// Generous over any real reply, but finite so a runaway/malformed stream
/// can't pin the cursor-row query or its late-reply drain in an unbounded
/// read. Shared by [`query_cursor_row_core`] and [`drain_dsr_reply`] so the
/// two stay in lockstep.
const MAX_DSR_REPLY: usize = 32;

/// Read exactly one byte from `fd`. Returns `None` on EOF or read error. The
/// shared read primitive of the cursor-row query and its late-reply drain —
/// both consume the DSR reply one byte at a time, each read preceded by a
/// bounded `poll` whose timeout and retry policy the caller owns (they differ
/// between the deadline-bounded query and the single-shot drain, so the poll
/// stays at the call sites; only the byte read is shared).
#[cfg(unix)]
fn read_byte(fd: std::os::fd::RawFd) -> Option<u8> {
    let mut byte = 0u8;
    (unsafe { libc::read(fd, (&mut byte as *mut u8).cast(), 1) } == 1).then_some(byte)
}

/// The cursor-row query itself, against an already-validated tty `fd` (the
/// caller checks `isatty` and that stderr is a terminal). Split from
/// [`query_cursor_row`] so the full I/O path — pre-check, raw mode, byte
/// loop, late-reply drain — is testable over a PTY without touching the test
/// process's real stdin/stderr.
///
/// Input-safety contract, in order:
///
/// 1. **Never consume input that was pending before the query.** If anything
///    is already buffered (the user started typing, or a previous read leaked
///    bytes), the query is skipped entirely — those bytes are the user's, not
///    ours to eat.
/// 2. **Bail the moment a byte can't continue a DSR reply.** Raw mode lasts
///    only as long as the reply is actually arriving; a keypress (or Ctrl-C)
///    landing mid-window ends the query immediately and restores the
///    terminal, so the raw window is a few milliseconds, not the full
///    deadline — at most one keystroke can be lost, and only in the
///    millisecond race between the pre-check and the reply.
/// 3. **Drain a late reply.** A reply that straggles past the deadline is
///    consumed from stdin (still in raw mode) so it can never surface as
///    garbage in a later prompt read. The drain only commits once a reply
///    actually shows up — bytes are never read speculatively past the first
///    one, which must be ESC to be a reply at all.
#[cfg(unix)]
fn query_cursor_row_core(fd: std::os::fd::RawFd) -> Option<usize> {
    use std::time::Instant;

    // 1. Skip if input is already pending: consumed bytes are lost, so a
    //    quiet queue is a precondition, not an optimization.
    let mut pfd = libc::pollfd {
        fd,
        events: libc::POLLIN,
        revents: 0,
    };
    if unsafe { libc::poll(&mut pfd, 1, 0) } > 0 {
        return None;
    }
    // The query goes to the same tty the reply comes back on: for a terminal
    // stdin (the only path that reaches here) that is the user's screen.
    let query = b"\x1b[6n";
    if unsafe { libc::write(fd, query.as_ptr().cast(), query.len()) } != query.len() as isize {
        return None;
    }

    // Raw input for the reply, held by `_raw` whose Drop restores termios on
    // every path — including a panic between here and the end of the
    // function — so the terminal can never be stranded in raw mode. See
    // [`RawTermios::enter`] for the mode flags (canonical off, OPOST kept).
    let _raw = RawTermios::enter(fd)?;
    let deadline = Instant::now() + Duration::from_millis(200);
    let mut resp: Vec<u8> = Vec::with_capacity(16);
    let result = loop {
        if Instant::now() >= deadline {
            break None;
        }
        let ready = unsafe { libc::poll(&mut pfd, 1, 50) };
        if ready <= 0 {
            continue;
        }
        let Some(byte) = read_byte(fd) else {
            break None;
        };
        // 2. A byte that can't be part of `ESC[<row>;<col>R` is not the reply
        //    (user input, or garbage): bail immediately instead of waiting
        //    out the deadline in raw mode.
        if !dsr_byte_ok(&resp, byte) {
            break None;
        }
        resp.push(byte);
        if byte == b'R' {
            break parse_dsr(&resp);
        }
        if resp.len() > MAX_DSR_REPLY {
            break None;
        }
    };
    // 3. Late-reply drain, still in raw mode. A reply that straggles past the
    //    deadline would otherwise sit in the kernel queue and surface as
    //    garbage in a later prompt read (canonical mode holds it until a
    //    newline that never comes — the worst kind of leak), so a short grace
    //    waits for its first byte. A partial reply (`resp` holds a real
    //    ESC-led prefix) drains straight to its closing `R`; a query that
    //    never saw its first byte only consumes a byte once one actually
    //    arrives, and only when it is ESC — a non-ESC byte ends the drain
    //    immediately (that one keystroke, typed in the millisecond race
    //    between pre-check and reply, is lost — the alternative, an undrained
    //    reply corrupting the next prompt read, is worse).
    if result.is_none() {
        let drain_deadline = Instant::now() + DRAIN_GRACE;
        if resp.is_empty() {
            while Instant::now() < drain_deadline {
                if unsafe { libc::poll(&mut pfd, 1, 10) } > 0 {
                    if read_byte(fd) == Some(b'\x1b') {
                        drain_dsr_reply(fd);
                    }
                    break;
                }
            }
        } else {
            drain_dsr_reply(fd);
        }
    }
    // `_raw` restores termios as it goes out of scope here — the guard's
    // Drop is the single restore point, so every path (timeout, garbage,
    // drain, panic) unwinds it.
    result
}

/// How long a reply that missed the query deadline is still waited for, so a
/// slow terminal's answer cannot leak into later stdin reads. Covers reply
/// latency up to 200 ms past the deadline (i.e. ~400 ms end to end) — well
/// past any link RTT that would make the reply genuinely late.
const DRAIN_GRACE: Duration = Duration::from_millis(200);

/// Whether `b` can continue a DSR reply already accumulated in `resp`
/// (`ESC[<row>;<col>R`): the first byte must be ESC, the second `[`, then
/// digits / `;` until the closing `R` (the loop stops there, so `R` only
/// passes while empty-accumulated checks for the leading bytes apply).
fn dsr_byte_ok(resp: &[u8], b: u8) -> bool {
    match resp.len() {
        0 => b == b'\x1b',
        1 => b == b'[',
        _ => b.is_ascii_digit() || b == b';' || b == b'R',
    }
}

/// Consume the remainder of a DSR reply from `fd` (still in raw mode) so it
/// cannot leak into later stdin reads: reads until the closing `R`, a reply-
/// shaped byte budget, or a short grace — whichever first. Only called once
/// the reply is known to be in flight (an ESC-led prefix was seen).
#[cfg(unix)]
fn drain_dsr_reply(fd: std::os::fd::RawFd) {
    use std::time::Instant;

    // Same budget as the outer wait-for-first-byte ([`DRAIN_GRACE`]): both
    // phases of the drain — a late reply beginning to arrive, then its body
    // following — are bounded by one "slow terminal" timeout, so a single
    // value covers reply latency past the query deadline end to end.
    let deadline = Instant::now() + DRAIN_GRACE;
    let mut pfd = libc::pollfd {
        fd,
        events: libc::POLLIN,
        revents: 0,
    };
    let mut n = 0usize;
    while Instant::now() < deadline && n < MAX_DSR_REPLY {
        if unsafe { libc::poll(&mut pfd, 1, 10) } <= 0 {
            break;
        }
        let Some(byte) = read_byte(fd) else {
            break;
        };
        n += 1;
        if byte == b'R' {
            break;
        }
    }
}

#[cfg(not(unix))]
fn query_cursor_row() -> Option<usize> {
    None
}

/// Parse a DSR response `ESC[<row>;<col>R` into the 1-based row. Pure (no
/// I/O) so the parse is unit-testable without a terminal.
fn parse_dsr(resp: &[u8]) -> Option<usize> {
    let s = std::str::from_utf8(resp).ok()?;
    let body = s.strip_prefix("\x1b[")?.strip_suffix('R')?;
    let row = body.split(';').next()?.parse::<usize>().ok()?;
    (row > 0).then_some(row)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_dsr_extracts_1based_row() {
        assert_eq!(parse_dsr(b"\x1b[12;1R"), Some(12));
        assert_eq!(parse_dsr(b"\x1b[1;80R"), Some(1));
        assert_eq!(parse_dsr(b"\x1b[40;1R"), Some(40));
    }

    #[test]
    fn parse_dsr_rejects_garbage() {
        assert_eq!(parse_dsr(b"nope"), None);
        assert_eq!(parse_dsr(b"\x1b[R"), None);
        assert_eq!(parse_dsr(b"\x1b[0;1R"), None); // row 0 is never valid
        assert_eq!(parse_dsr(b""), None);
    }

    #[test]
    fn window_sizes_to_space_below_cursor() {
        // 24-row terminal, cursor on row 21 (near the bottom): the frame
        // (spinner + window) must fit rows 21..24 — a 3-row window, never a
        // collapsing full-height one.
        assert_eq!(reasoning_rows_for(24, Some(21)), 3);
        // Cursor at the very top: the full height is available, capped at MAX.
        assert_eq!(reasoning_rows_for(24, Some(1)), 23);
        // Mid-screen cursor on a 40-row terminal: exact-fit case.
        assert_eq!(reasoning_rows_for(40, Some(3)), 37);
        // Cursor in the bottom two rows has no room to fit a readable window:
        // the renderer scrolls instead, and the pre-dynamic budget applies
        // (clamped so the frame still fits on screen — 12 < 40).
        assert_eq!(reasoning_rows_for(40, Some(40)), 12);
        assert_eq!(reasoning_rows_for(40, Some(39)), 12);
        // Two rows of room below the cursor: exact fit again (no scrolling).
        assert_eq!(reasoning_rows_for(40, Some(38)), 2);
        // A tiny terminal clamps the scroll budget so the frame never exceeds
        // the screen: 1 spinner + 4 rows on a 5-row terminal.
        assert_eq!(reasoning_rows_for(5, Some(5)), 4);
        // Unknown cursor: the pre-dynamic fixed cap (never worse than before
        // the dynamic-window feature).
        assert_eq!(reasoning_rows_for(24, None), REASONING_FALLBACK_ROWS);
        // A bogus cursor row past the terminal height falls back too.
        assert_eq!(reasoning_rows_for(24, Some(99)), REASONING_FALLBACK_ROWS);
    }

    /// [`WindowSizing::fallback`] is the graceful-degradation shape used when
    /// the query fails or the offloaded query task panicked: the pre-dynamic
    /// fixed cap and scrolling disabled — never worse than the pre-feature
    /// experience, and (critically) never breaks the commit.
    #[test]
    fn window_sizing_fallback_is_fixed_cap_no_scroll() {
        let f = WindowSizing::fallback();
        assert_eq!(f.max_rows, REASONING_FALLBACK_ROWS);
        assert_eq!(f.cursor_row, None);
    }
}

/// Full-I/O tests for [`query_cursor_row_core`] over a real PTY: a forked
/// child plays the terminal emulator on the pty master (reads the DSR query,
/// answers per script), while the parent runs the query against the slave.
/// This is the committed version of the PR's "deterministic PTY harness"
/// claim — the input-safety contract (never consume pending input, never
/// leak a late reply) is exercised end to end, not just the pure parse.
#[cfg(all(unix, test))]
mod pty_tests {
    use super::*;
    use std::os::fd::RawFd;
    use std::time::{Duration, Instant};

    /// What the fake terminal does on the pty master.
    #[derive(Clone, Copy)]
    enum Script {
        /// Wait for the query, then reply immediately.
        Reply(&'static [u8]),
        /// Wait for the query, sleep `ms`, then reply (a late reply).
        ReplyLate(&'static [u8], u32),
        /// Wait for the query, then stay silent (no reply ever).
        Silent,
        /// Write bytes immediately without waiting for any query.
        PreWrite(&'static [u8]),
    }

    fn open_pty() -> (RawFd, RawFd) {
        let mut master = 0;
        let mut slave = 0;
        let rc = unsafe {
            libc::openpty(
                &mut master,
                &mut slave,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
            )
        };
        assert_eq!(rc, 0, "openpty failed");
        (master, slave)
    }

    /// Child-side terminal emulator. Runs after `fork`, so it must not touch
    /// the allocator: fixed stack buffer, libc calls only.
    fn terminal_script(master: RawFd, script: Script) -> i32 {
        if let Script::PreWrite(bytes) = script {
            let n = unsafe { libc::write(master, bytes.as_ptr().cast(), bytes.len()) };
            return if n == bytes.len() as isize { 0 } else { 1 };
        }
        // Wait for the DSR query (`ESC[6n`) to arrive on the master.
        let query = b"\x1b[6n";
        let mut buf = [0u8; 64];
        let mut len = 0usize;
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut pfd = libc::pollfd {
            fd: master,
            events: libc::POLLIN,
            revents: 0,
        };
        loop {
            if Instant::now() >= deadline {
                return 2; // query never arrived
            }
            let ready = unsafe { libc::poll(&mut pfd, 1, 50) };
            if ready <= 0 {
                continue;
            }
            let n = unsafe { libc::read(master, buf[len..].as_mut_ptr().cast(), buf.len() - len) };
            if n <= 0 {
                return 3;
            }
            len += n as usize;
            if len >= query.len() && buf[..len].windows(query.len()).any(|w| w == query) {
                break;
            }
            if len == buf.len() {
                return 4; // buffer full without ever seeing the query
            }
        }
        match script {
            Script::Silent => 0,
            Script::ReplyLate(bytes, ms) => {
                unsafe { libc::usleep(ms * 1000) };
                let n = unsafe { libc::write(master, bytes.as_ptr().cast(), bytes.len()) };
                if n == bytes.len() as isize { 0 } else { 5 }
            }
            Script::Reply(bytes) => {
                let n = unsafe { libc::write(master, bytes.as_ptr().cast(), bytes.len()) };
                if n == bytes.len() as isize { 0 } else { 5 }
            }
            Script::PreWrite(_) => 0,
        }
    }

    /// Put `fd` in raw (non-canonical) mode, like the query does — without
    /// this, a pty slave buffers input until a newline (canonical mode), so a
    /// single byte like the PreWrite script's `x` would never make poll
    /// report it readable.
    fn make_raw(fd: RawFd) {
        unsafe {
            let mut t: libc::termios = std::mem::zeroed();
            assert_eq!(libc::tcgetattr(fd, &mut t), 0, "tcgetattr failed");
            libc::cfmakeraw(&mut t);
            assert_eq!(
                libc::tcsetattr(fd, libc::TCSANOW, &t),
                0,
                "tcsetattr failed"
            );
        }
    }

    /// Fork a fake terminal running `script`; returns `(master, slave, pid)`
    /// — the parent keeps the master open for the test's lifetime so the
    /// slave never sees EOF/HUP (a closed master would fake "input pending"
    /// on every later poll). Both fds are closed by the test; `pid` is
    /// reaped with [`reap`].
    fn spawn_terminal(script: Script) -> (RawFd, RawFd, i32) {
        let (master, slave) = open_pty();
        make_raw(slave);
        let pid = unsafe { libc::fork() };
        assert!(pid >= 0, "fork failed");
        if pid == 0 {
            unsafe { libc::close(slave) };
            let code = terminal_script(master, script);
            unsafe { libc::_exit(code) };
        }
        (master, slave, pid)
    }

    fn reap(pid: i32) -> i32 {
        let mut status = 0;
        unsafe { libc::waitpid(pid, &mut status, 0) };
        status
    }

    fn poll_ready(fd: RawFd, timeout_ms: u32) -> bool {
        let mut pfd = libc::pollfd {
            fd,
            events: libc::POLLIN,
            revents: 0,
        };
        unsafe { libc::poll(&mut pfd, 1, timeout_ms as libc::c_int) > 0 }
    }

    /// Snapshot a fd's termios, for asserting the query restored it.
    fn termios_of(fd: RawFd) -> libc::termios {
        unsafe {
            let mut t: libc::termios = std::mem::zeroed();
            assert_eq!(libc::tcgetattr(fd, &mut t), 0, "tcgetattr failed");
            t
        }
    }

    /// Like [`spawn_terminal`] but leaves the slave in its default canonical
    /// mode (no [`make_raw`]). The query enters raw mode itself to read the
    /// reply, so canonical mode does not block the Reply/Silent scripts; and
    /// leaving the slave canonical makes a successful termios restore
    /// observable as a return to canonical-with-echo, whereas raw-to-raw
    /// (what [`spawn_terminal`] produces) would hide a missing restore.
    fn spawn_canonical_terminal(script: Script) -> (RawFd, RawFd, i32) {
        let (master, slave) = open_pty();
        let pid = unsafe { libc::fork() };
        assert!(pid >= 0, "fork failed");
        if pid == 0 {
            unsafe { libc::close(slave) };
            let code = terminal_script(master, script);
            unsafe { libc::_exit(code) };
        }
        (master, slave, pid)
    }

    /// The full DSR round trip over a real PTY: the fake terminal sees the
    /// query and answers `ESC[21;1R`; the query reports row 21.
    #[test]
    fn dsr_query_roundtrip_over_pty() {
        let (master, slave, pid) = spawn_terminal(Script::Reply(b"\x1b[21;1R"));
        let row = query_cursor_row_core(slave);
        unsafe {
            libc::close(master);
            libc::close(slave);
        }
        assert_eq!(reap(pid), 0);
        assert_eq!(row, Some(21));
    }

    /// A garbage reply (not ESC-led) bails the byte loop on its first byte —
    /// the query fails fast instead of waiting out the 200 ms deadline.
    #[test]
    fn dsr_rejects_garbage_reply() {
        let (master, slave, pid) = spawn_terminal(Script::Reply(b"hello"));
        let row = query_cursor_row_core(slave);
        unsafe {
            libc::close(master);
            libc::close(slave);
        }
        assert_eq!(reap(pid), 0);
        assert_eq!(row, None);
    }

    /// No reply at all: the query times out and returns `None` (the fallback
    /// path) instead of hanging.
    #[test]
    fn dsr_times_out_without_reply() {
        let (master, slave, pid) = spawn_terminal(Script::Silent);
        let row = query_cursor_row_core(slave);
        unsafe {
            libc::close(master);
            libc::close(slave);
        }
        assert_eq!(reap(pid), 0);
        assert_eq!(row, None);
    }

    /// A reply that arrives after the 200 ms deadline (but inside the drain
    /// grace) is consumed, so it can never sit in the input queue and surface
    /// as garbage in a later prompt read.
    #[test]
    fn dsr_drains_late_reply_so_nothing_leaks() {
        let (master, slave, pid) = spawn_terminal(Script::ReplyLate(b"\x1b[40;1R", 300));
        let row = query_cursor_row_core(slave);
        assert_eq!(row, None, "reply past the deadline must not be reported");
        // Give the drain grace time to finish, then the queue must be empty.
        std::thread::sleep(Duration::from_millis(500));
        assert!(
            !poll_ready(slave, 0),
            "late reply leaked into the input queue — it would corrupt a later prompt read"
        );
        unsafe {
            libc::close(master);
            libc::close(slave);
        }
        assert_eq!(reap(pid), 0);
    }

    /// Input already pending when the query starts is never consumed: the
    /// pre-check skips the query entirely and the byte survives for the next
    /// read.
    #[test]
    fn dsr_skips_query_when_input_pending() {
        let (master, slave, pid) = spawn_terminal(Script::PreWrite(b"x"));
        assert!(poll_ready(slave, 2000), "child never wrote its input");
        let row = query_cursor_row_core(slave);
        assert_eq!(row, None, "a busy stdin must not be queried");
        // The byte is still pending — the pre-check did not consume it.
        assert!(poll_ready(slave, 2000), "pending input was swallowed");
        unsafe {
            libc::close(master);
            libc::close(slave);
        }
        assert_eq!(reap(pid), 0);
    }

    /// termios restore is panic-safe via the RAII guard: the query enters raw
    /// mode to read the reply, and the guard's Drop must restore the original
    /// mode on every path — including the timeout path (the one most likely
    /// to skip cleanup back when restore was a manual trailing `tcsetattr`).
    /// The slave starts canonical (no [`make_raw`]), so a successful restore
    /// is observable as ICANON set afterwards; a stranded-raw regression
    /// leaves ICANON clear.
    #[test]
    fn dsr_restores_termios_even_on_timeout_path() {
        let (master, slave, pid) = spawn_canonical_terminal(Script::Silent);
        let before = termios_of(slave);
        assert!(
            before.c_lflag & libc::ICANON != 0,
            "slave must start canonical for the restore to be observable"
        );
        let row = query_cursor_row_core(slave);
        assert_eq!(row, None, "Silent script times out");
        let after = termios_of(slave);
        unsafe {
            libc::close(master);
            libc::close(slave);
        }
        assert_eq!(reap(pid), 0);
        assert_eq!(
            after.c_lflag & libc::ICANON,
            before.c_lflag & libc::ICANON,
            "ICANON not restored — slave left stranded in raw mode"
        );
        assert_eq!(
            after.c_lflag & libc::ECHO,
            before.c_lflag & libc::ECHO,
            "ECHO not restored"
        );
    }
}
