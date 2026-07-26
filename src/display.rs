use console::{Style, Term, measure_text_width};

const MAX_VISIBLE_FILES: usize = 3;

/// Panel-based terminal output for the commit workflow.
///
/// Every method writes to stderr. Panels use Unicode box-drawing when
/// colors are enabled, and ASCII (`+`, `-`, `|`) otherwise. Emoji
/// are only rendered in color mode; non-TTY output is plain text.
pub struct Display {
    term: Term,
    colors: bool,
    width: usize,
}

impl Display {
    pub fn new() -> Self {
        let term = Term::stderr();
        let colors = console::colors_enabled_stderr();
        // Dynamic width, clamped for readability.
        let width = (term.size().1 as usize).clamp(60, 100);
        Self {
            term,
            colors,
            width,
        }
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

    /// Return the set of box-drawing characters for the current mode.
    fn box_chars(&self) -> BoxChars {
        if self.colors {
            BoxChars {
                tl: "┌",
                tr: "┐",
                bl: "└",
                br: "┘",
                h: "─",
                v: "│",
            }
        } else {
            BoxChars {
                tl: "+",
                tr: "+",
                bl: "+",
                br: "+",
                h: "-",
                v: "|",
            }
        }
    }

    /// Render a bordered panel to stderr.
    ///
    /// `header` is centered in the top border. `body` lines are already
    /// fully styled strings — the panel only adds border chars and
    /// right-padding.
    fn panel(&self, header: &str, body: &[String]) {
        let bc = self.box_chars();
        let inner = self.width.saturating_sub(2); // space between borders
        let dim = Style::new().dim();

        // --- top border with centered header ---
        let hw = measure_text_width(header);
        let pad = inner.saturating_sub(hw);
        let left = pad / 2;
        let right = pad - left;
        let top = format!(
            "{}{}{}{}{}",
            self.styled(bc.tl, dim.clone()),
            self.styled(&bc.h.repeat(left), dim.clone()),
            header,
            self.styled(&bc.h.repeat(right), dim.clone()),
            self.styled(bc.tr, dim.clone()),
        );
        let _ = self.term.write_line(&top);

        // --- body ---
        for line in body {
            let vw = measure_text_width(line);
            let fill = if vw < inner {
                " ".repeat(inner - vw)
            } else {
                String::new()
            };
            let _ = self.term.write_line(&format!(
                "{v}{line}{fill}{v}",
                v = self.styled(bc.v, dim.clone()),
            ));
        }

        // --- bottom border ---
        let bottom = format!(
            "{}{}{}",
            self.styled(bc.bl, dim.clone()),
            self.styled(&bc.h.repeat(inner), dim.clone()),
            self.styled(bc.br, dim),
        );
        let _ = self.term.write_line(&bottom);
    }

    // ------------------------------------------------------------------
    // Public rendering entry points
    // ------------------------------------------------------------------

    /// Compact notice after formatting Rust files (no panel).
    pub fn formatted_notice(&self, count: usize) {
        let word = if count == 1 { "file" } else { "files" };
        let msg = self.styled(
            &format!("  Formatted {} Rust {}", count, word),
            Style::new().dim(),
        );
        let _ = self.term.write_line(&msg);
    }

    /// Summary panel shown when unstaged changes are split into batches.
    pub fn batch_summary(&self, batches: &[(&[String], Option<&str>)]) {
        let count = batches.len();
        let header = match count {
            0 => return,
            1 => "1 commit".to_string(),
            n => format!("{n} commits"),
        };

        let mut body: Vec<String> = Vec::new();
        body.push(String::new());
        for (i, (files, reason)) in batches.iter().enumerate() {
            let reason_part = reason.map(|r| format!("[{r}] ")).unwrap_or_default();
            let file_part = format_files_preview(files);
            let line = format!("  {}. {}{}", i + 1, reason_part, file_part);
            body.push(line);
        }
        body.push(String::new());

        self.panel(&header, &body);
    }

    /// Commit-completion panel — shown after the commit is created.
    pub fn commit_panel(
        &self,
        hash: &str,
        message: &str,
        body_text: Option<&str>,
        files: &[String],
    ) {
        let green_bold = Style::new().green().bold();
        let dim = Style::new().dim();
        let cyan = Style::new().cyan();
        let header = format!("Committed {hash}");

        let mut lines: Vec<String> = Vec::new();
        lines.push(String::new()); // spacer

        // Subject
        lines.push(format!("  {}", self.styled(message, green_bold)));

        // Body (optional)
        if let Some(b) = body_text {
            let trimmed = b.trim();
            if !trimmed.is_empty() {
                lines.push(String::new());
                for bline in trimmed.lines() {
                    lines.push(format!("  {}", self.styled(bline, dim.clone())));
                }
            }
        }

        // File list
        lines.push(String::new());
        let file_str = format_files_list(files, MAX_VISIBLE_FILES);
        lines.push(format!(
            "  {}",
            self.styled(&format!("Files: {file_str}"), cyan),
        ));

        lines.push(String::new()); // spacer
        self.panel(&header, &lines);
    }
}

impl Default for Display {
    fn default() -> Self {
        Self::new()
    }
}

// ------------------------------------------------------------------
// Internal formatting helpers
// ------------------------------------------------------------------

struct BoxChars {
    tl: &'static str,
    tr: &'static str,
    bl: &'static str,
    br: &'static str,
    h: &'static str,
    v: &'static str,
}

/// Collapse a long file list: first N names, then "(+M more)".
fn format_files_list(files: &[String], max: usize) -> String {
    let visible: Vec<&str> = files.iter().take(max).map(|s| s.as_str()).collect();
    let mut s = visible.join(", ");
    let rem = files.len().saturating_sub(max);
    if rem > 0 {
        s.push_str(&format!(" (+{rem} more)"));
    }
    s
}

/// Compact file preview for batch-summary lines.
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
    fn file_list_single() {
        let f = |s: &str| s.to_string();
        assert_eq!(format_files_list(&[f("src/main.rs")], 3), "src/main.rs");
    }

    #[test]
    fn file_list_within_max() {
        let f = |s: &str| s.to_string();
        assert_eq!(format_files_list(&[f("a.rs"), f("b.rs")], 3), "a.rs, b.rs");
    }

    #[test]
    fn file_list_collapse() {
        let f = |s: &str| s.to_string();
        assert_eq!(
            format_files_list(&[f("a.rs"), f("b.rs"), f("c.rs"), f("d.rs"), f("e.rs")], 3),
            "a.rs, b.rs, c.rs (+2 more)"
        );
    }

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
