use std::fs;
use std::path::Path;
use typhoon_compiler::error::{CompileError, Severity};

mod color {
    pub const RED: &str = "\x1b[31m";
    pub const YELLOW: &str = "\x1b[33m";
    pub const BLUE: &str = "\x1b[34m";
    pub const CYAN: &str = "\x1b[36m";
    pub const BOLD: &str = "\x1b[1m";
    pub const RESET: &str = "\x1b[0m";
}

/// Source file context for rendering error snippets.
struct SourceContext {
    lines: Vec<String>,
}

impl SourceContext {
    fn load(path: &Path) -> Option<Self> {
        let content = fs::read_to_string(path).ok()?;
        let lines = content.lines().map(|s| s.to_string()).collect();
        Some(SourceContext { lines })
    }

    fn get_line(&self, line: usize) -> Option<&str> {
        if line == 0 || line > self.lines.len() {
            return None;
        }
        Some(&self.lines[line - 1])
    }
}

/// Writer for rendering diagnostics in Rust-style format.
pub struct ErrorWriter {
    use_color: bool,
}

impl ErrorWriter {
    pub fn new() -> Self {
        ErrorWriter { use_color: true }
    }

    pub fn with_color(mut self, use_color: bool) -> Self {
        self.use_color = use_color;
        self
    }

    fn colorize(&self, text: &str, color: &str) -> String {
        if self.use_color {
            format!("{}{}{}", color, text, color::RESET)
        } else {
            text.to_string()
        }
    }

    fn colorize_bold(&self, text: &str, color: &str) -> String {
        if self.use_color {
            format!("{}{}{}{}", color::BOLD, color, text, color::RESET)
        } else {
            text.to_string()
        }
    }

    fn severity_color(&self, sev: Severity) -> &str {
        match sev {
            Severity::Error => color::RED,
            Severity::Warning => color::YELLOW,
            Severity::Note => color::BLUE,
        }
    }

    fn severity_label(&self, sev: Severity) -> &str {
        match sev {
            Severity::Error => "error",
            Severity::Warning => "warning",
            Severity::Note => "note",
        }
    }

    /// Render a single diagnostic to stderr.
    pub fn render(&self, err: &CompileError, source_path: &Path) {
        let source = SourceContext::load(source_path);

        // Header: file:line:col: severity[code]: title
        let primary = &err.primary;
        let line = primary.span.line;
        let col = primary.span.col;

        let sev_label = self.colorize_bold(
            self.severity_label(err.severity),
            self.severity_color(err.severity),
        );
        let code = if !err.code.is_empty() {
            self.colorize(
                &format!("[{}]", err.code),
                self.severity_color(err.severity),
            )
        } else {
            String::new()
        };
        let title = self.colorize_bold(&err.title, self.severity_color(err.severity));

        eprintln!(
            "{}:{}:{}: {} {}{}: {}",
            source_path.display(),
            line,
            col,
            sev_label,
            code,
            if !err.code.is_empty() { ":" } else { "" },
            title
        );
        eprintln!("{}", primary.message);

        // Source snippet with underlines
        if let Some(source) = source {
            self.render_snippet(&source, line, primary, &err.labels, err.severity);
        }

        // Help text
        if let Some(help) = &err.help {
            let help_label = self.colorize_bold("help: ", color::CYAN);
            eprintln!("{}  {}", help_label, help);
        }

        eprintln!(); // blank line between errors
    }

    fn render_snippet(
        &self,
        source: &SourceContext,
        center_line: usize,
        primary: &typhoon_compiler::error::Label,
        labels: &[typhoon_compiler::error::Label],
        primary_severity: Severity,
    ) {
        let context = 2; // lines before/after
        let start = center_line.saturating_sub(context);
        let end = (center_line + context).min(source.lines.len());

        // If primary span is invalid (line 0), don't render snippet
        if center_line == 0 || primary.span.line == 0 {
            return;
        }

        // Build line number width
        let max_line_num = end;
        let line_width = max_line_num.to_string().len();

        // Collect all labeled spans per line
        use std::collections::HashMap;
        let mut line_labels: HashMap<usize, Vec<&typhoon_compiler::error::Label>> = HashMap::new();
        // Add primary
        if primary.span.line > 0 && primary.span.line <= source.lines.len() {
            line_labels
                .entry(primary.span.line)
                .or_default()
                .push(primary);
        }
        // Add secondary labels
        for label in labels {
            if label.span.line > 0 && label.span.line <= source.lines.len() {
                line_labels.entry(label.span.line).or_default().push(label);
            }
        }

        // Render each line
        for ln in start..=end {
            let line_content = source.get_line(ln).unwrap_or("");
            let line_num =
                self.colorize(&format!("{:>width$}", ln, width = line_width), color::CYAN);
            let gutter = self.colorize(" |", color::CYAN);

            eprintln!("{} {}{}", line_num, gutter, line_content);

            // If this line has labels, render underlines
            if let Some(labels_on_line) = line_labels.get(&ln) {
                // Find the leftmost column of any label on this line
                let min_col = labels_on_line
                    .iter()
                    .map(|l| l.span.col)
                    .filter(|&c| c > 0)
                    .min()
                    .unwrap_or(1);

                // Build underline string
                let mut underline = String::new();
                underline.push_str(&" ".repeat(line_width + 3)); // line num + " | "
                underline.push_str(&" ".repeat(min_col.saturating_sub(1)));

                // Draw carets for each label
                for label in labels_on_line {
                    let start_col = label.span.col.max(1);
                    let end_col = if label.span.end > label.span.start {
                        // Calculate column span from byte offset
                        let line_content = source.get_line(label.span.line).unwrap_or("");
                        let byte_span = label.span.end.saturating_sub(label.span.start);
                        (start_col + byte_span).min(line_content.len() + 1)
                    } else {
                        start_col + 1
                    };

                    let width = (end_col - start_col).max(1);
                    let caret_color = if label.span.start == primary.span.start
                        && label.span.line == primary.span.line
                    {
                        self.severity_color(primary_severity)
                    } else {
                        color::BLUE
                    };

                    if self.use_color {
                        underline.push_str(caret_color);
                    }
                    underline.push_str(&"^".repeat(width));
                    if self.use_color {
                        underline.push_str(color::RESET);
                    }
                }

                eprintln!("{}", underline);

                // Render label messages on next line
                for label in labels_on_line {
                    let msg_indent = " ".repeat(line_width + 3 + label.span.col.max(1) - 1);
                    let label_color = if label.span.start == primary.span.start
                        && label.span.line == primary.span.line
                    {
                        self.severity_color(primary_severity)
                    } else {
                        color::BLUE
                    };
                    let msg = self.colorize(&label.message, label_color);
                    eprintln!("{}{} {}", msg_indent, label_color, msg);
                }
            }
        }
    }
}
