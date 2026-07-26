use console::{Style, Term};

/// Clean line-based terminal output — no panels, no box-drawing.
///
/// Every method writes to stderr. Color-aware: when colors are disabled
/// (piped output, NO_COLOR, non-TTY) output is plain text with no ANSI
/// escapes.
///
/// Write errors are intentionally ignored: this is fire-and-forget
/// status output to stderr (e.g. a closed pipe), never load-bearing.
pub struct Display {
    term: Term,
    colors: bool,
}

impl Display {
    pub fn new() -> Self {
        let term = Term::stderr();
        let colors = console::colors_enabled_stderr();
        Self { term, colors }
    }

    // ------------------------------------------------------------------
    // Helpers
    // ------------------------------------------------------------------

    /// Apply a console `Style` to text. Returns plain text when colors
    /// are disabled (piped output, NO_COLOR, non-TTY).
    fn styled(&self, text: &str, s: Style) -> String {
        if self.colors {
            s.apply_to(text).to_string()
        } else {
            text.to_string()
        }
    }

    /// Write a line to stderr, ignoring errors.
    fn writeln(&self, line: &str) {
        let _ = self.term.write_line(line);
    }

    // ------------------------------------------------------------------
    // Public rendering entry points
    // ------------------------------------------------------------------

    /// Compact notice after formatting Rust files.
    pub fn formatted_notice(&self, count: usize) {
        let word = if count == 1 { "file" } else { "files" };
        let msg = self.styled(
            &format!("  Formatted {} Rust {}", count, word),
            Style::new().dim(),
        );
        self.writeln(&msg);
    }

    /// Batch plan summary shown when unstaged changes are split into
    /// logical commits.
    pub fn batch_summary(&self, batches: &[BatchSummary<'_>]) {
        let count = batches.len();
        if count == 0 {
            return;
        }

        let label = match count {
            1 => "1 commit planned:".to_string(),
            n => format!("{n} commits planned:"),
        };
        self.writeln(&label);

        for (i, b) in batches.iter().enumerate() {
            let reason_part = b.reason.map(|r| format!("[{r}] ")).unwrap_or_default();
            let file_part = format_files_preview(b.files);
            let line = format!("  {}. {}{}", i + 1, reason_part, file_part);
            self.writeln(&line);
        }

        self.writeln(""); // blank separator
    }

    /// Commit-completion line — shown after each commit.
    ///
    /// `prefix` is prepended for batch progress (e.g. `[1/3]`);
    /// pass `""` for single-commit or staged workflows.
    pub fn commit_line(&self, hash: &str, message: &str, body: Option<&str>, prefix: &str) {
        let green_bold = Style::new().green().bold();
        let dim = Style::new().dim();

        // Main line: [prefix] ✓ <hash> <message>
        let check = self.styled("\u{2713}", green_bold.clone());
        let hash_styled = self.styled(hash, Style::new().cyan());
        let msg_styled = self.styled(message, green_bold);
        let pre = if prefix.is_empty() {
            String::new()
        } else {
            format!("{prefix} ")
        };
        self.writeln(&format!("{pre}{check} {hash_styled} {msg_styled}"));

        // Optional body — indented, dim
        if let Some(b) = body {
            let trimmed = b.trim();
            if !trimmed.is_empty() {
                for bline in trimmed.lines() {
                    self.writeln(&format!("  {}", self.styled(bline, dim.clone())));
                }
            }
        }
    }
}

impl Default for Display {
    fn default() -> Self {
        Self::new()
    }
}

/// A batch's files and optional reason, passed to [`Display::batch_summary`].
pub struct BatchSummary<'a> {
    pub files: &'a [String],
    pub reason: Option<&'a str>,
}

// ------------------------------------------------------------------
// Internal formatting helpers
// ------------------------------------------------------------------

/// Compact one-file preview for batch-summary lines.
fn format_files_preview(files: &[String]) -> String {
    if files.is_empty() {
        return String::new();
    }
    if files.len() == 1 {
        return files[0].clone();
    }
    format!("{} (+{} more)", files[0], files.len() - 1)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_preview_empty() {
        assert_eq!(format_files_preview(&[]), "");
    }

    #[test]
    fn file_preview_one() {
        assert_eq!(format_files_preview(&["foo.rs".into()]), "foo.rs");
    }

    #[test]
    fn file_preview_many() {
        let files: Vec<String> = ["a.rs", "b.rs", "c.rs"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        assert_eq!(format_files_preview(&files), "a.rs (+2 more)");
    }
}
