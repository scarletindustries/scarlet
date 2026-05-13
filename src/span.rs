#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Span {
    pub start_line: i32,
    pub start_column: i32,
    pub end_line: i32,
    pub end_column: i32,
}

pub fn point_span(line: i32, column: i32) -> Span {
    Span {
        start_line: line,
        start_column: column,
        end_line: line,
        end_column: column + 1,
    }
}

pub fn range_span(line: i32, start_column: i32, end_column: i32) -> Span {
    Span {
        start_line: line,
        start_column,
        end_line: line,
        end_column,
    }
}
