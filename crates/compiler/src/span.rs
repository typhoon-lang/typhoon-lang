// src/span.rs

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Span {
    pub start: usize,
    pub end: usize,
    pub line: usize,
    pub col: usize,
}

impl Span {
    pub fn new(start: usize, end: usize, line: usize, col: usize) -> Self {
        Span {
            start,
            end,
            line,
            col,
        }
    }

    pub fn join(&self, other: Span) -> Span {
        let start = self.start.min(other.start);
        let end = self.end.max(other.end);
        // line and col should be from the first one
        let (line, col) = if self.start <= other.start {
            (self.line, self.col)
        } else {
            (other.line, other.col)
        };
        Span {
            start,
            end,
            line,
            col,
        }
    }
}
