use crate::span::Span;
use crate::type_inference::{Solver, TypeError};

/// A parsing/resolution error with span information
#[derive(Debug, Clone)]
pub struct SimpleError {
    pub code: String,
    pub message: String,
    pub span: Span,
}

impl From<String> for SimpleError {
    fn from(s: String) -> Self {
        SimpleError {
            code: "E1000".to_string(),
            message: s,
            span: Span::default(),
        }
    }
}

impl From<&str> for SimpleError {
    fn from(s: &str) -> Self {
        s.to_string().into()
    }
}

/// Severity of a compilation diagnostic.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Severity {
    Error,
    Warning,
    Note,
}

impl Severity {
    pub fn label(&self) -> &str {
        match self {
            Severity::Error => "error",
            Severity::Warning => "warning",
            Severity::Note => "note",
        }
    }
}

/// A labeled span — a location in source with an associated message.
#[derive(Debug, Clone)]
pub struct Label {
    pub span: Span,
    pub message: String,
}

/// A compilation diagnostic with Rust-style structured information.
///
/// Each diagnostic has:
/// - A `code` — machine-readable error code (e.g. "E0001")
/// - A `title` — short human-readable summary
/// - A `severity` — Error, Warning, or Note
/// - A `primary` span — the main location of the problem
/// - Optional `labels` — secondary spans with explanatory messages
/// - Optional `help` — a suggestion or hint
#[derive(Debug, Clone)]
pub struct CompileError {
    pub code: String,
    pub title: String,
    pub severity: Severity,
    pub primary: Label,
    pub labels: Vec<Label>,
    pub help: Option<String>,
}

impl CompileError {
    pub fn error(code: &str, title: &str, span: Span, message: &str) -> Self {
        CompileError {
            code: code.to_string(),
            title: title.to_string(),
            severity: Severity::Error,
            primary: Label {
                span,
                message: message.to_string(),
            },
            labels: vec![],
            help: None,
        }
    }

    pub fn warning(code: &str, title: &str, span: Span, message: &str) -> Self {
        CompileError {
            code: code.to_string(),
            title: title.to_string(),
            severity: Severity::Warning,
            primary: Label {
                span,
                message: message.to_string(),
            },
            labels: vec![],
            help: None,
        }
    }

    pub fn note(code: &str, title: &str, span: Span, message: &str) -> Self {
        CompileError {
            code: code.to_string(),
            title: title.to_string(),
            severity: Severity::Note,
            primary: Label {
                span,
                message: message.to_string(),
            },
            labels: vec![],
            help: None,
        }
    }

    pub fn with_label(mut self, span: Span, message: &str) -> Self {
        self.labels.push(Label {
            span,
            message: message.to_string(),
        });
        self
    }

    pub fn with_help(mut self, help: &str) -> Self {
        self.help = Some(help.to_string());
        self
    }
}

impl CompileError {
    /// Create a CompileError from a TypeError with type display via Solver.
    pub fn from_type_error(err: TypeError, solver: &Solver) -> Self {
        let span = err.span().unwrap_or_else(|| Span::new(0, 0, 0, 0));
        match err {
            TypeError::UnknownIdentifier { name, .. } => CompileError::error(
                "E0425",
                "cannot find value in this scope",
                span,
                &format!("not found in this scope: `{}`", name),
            ),
            TypeError::TypeMismatch {
                expected,
                actual,
                context,
                ..
            } => {
                let title = if context.contains("unification") {
                    "mismatched types".to_string()
                } else {
                    "type mismatch".to_string()
                };
                let mut ce = CompileError::error("E0308", &title, span, &context);
                ce.primary.message = format!(
                    "expected `{}`, found `{}`",
                    expected.display(solver),
                    actual.display(solver)
                );
                ce
            }
            TypeError::OccursCheck { .. } => CompileError::error(
                "E0391",
                "cycle detected in type definition",
                span,
                "recursive type has infinite size",
            ).with_help("consider using a boxed or reference type instead"),
        }
    }
}