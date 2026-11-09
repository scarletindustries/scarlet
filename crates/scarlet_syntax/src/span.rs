#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Span {
    pub start_line: i32,
    pub start_column: i32,
    pub end_line: i32,
    pub end_column: i32,
}

impl Span {
    /// A placeholder span for generated, prelude or test AST that has no
    /// source location. It's empty (`start == end`), so it contains no point
    /// and can't win a hover, goto or references query at the start of a file.
    pub const DUMMY: Span = Span {
        start_line: 0,
        start_column: 0,
        end_line: 0,
        end_column: 0,
    };

    /// A span one column wide at `(line, column)`, ending at `column + 1`.
    pub fn point(line: i32, column: i32) -> Span {
        Span {
            start_line: line,
            start_column: column,
            end_line: line,
            end_column: column + 1,
        }
    }

    /// A single-line span `[c0, c1)` on `line`.
    pub fn single_line(line: i32, c0: i32, c1: i32) -> Span {
        Span {
            start_line: line,
            start_column: c0,
            end_line: line,
            end_column: c1,
        }
    }

    /// `(start, end)` as `(line, column)` pairs, compared lexicographically.
    fn endpoints(&self) -> ((i32, i32), (i32, i32)) {
        (
            (self.start_line, self.start_column),
            (self.end_line, self.end_column),
        )
    }

    /// Whether the point is in the half-open range `[start, end)`, in
    /// (line, column) order.
    pub fn contains(&self, line: i32, col: i32) -> bool {
        let (start, end) = self.endpoints();
        let p = (line, col);
        start <= p && p < end
    }

    /// A key for picking the tightest of several spans that contain a point.
    /// Only the ordering matters, not the numbers.
    pub fn width(&self) -> (i32, i32) {
        (
            self.end_line - self.start_line,
            self.end_column - self.start_column,
        )
    }

    /// Whether `self` lies fully inside `outer`. This tells where an import
    /// binds a name (inside the `import` declaration's span) apart from a real
    /// use of that name elsewhere.
    pub fn within(&self, outer: &Span) -> bool {
        let (i_start, i_end) = self.endpoints();
        let (o_start, o_end) = outer.endpoints();
        o_start <= i_start && i_end <= o_end
    }

    /// The smallest `Span` covering both, in either order.
    pub(crate) fn union(&self, other: &Span) -> Span {
        let (a_start, a_end) = self.endpoints();
        let (b_start, b_end) = other.endpoints();
        let (start_line, start_column) = a_start.min(b_start);
        let (end_line, end_column) = a_end.max(b_end);
        Span {
            start_line,
            start_column,
            end_line,
            end_column,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::Span;

    #[test]
    fn dummy_contains_no_point() {
        assert!(!Span::DUMMY.contains(0, 0));
    }

    #[test]
    fn union_is_order_independent() {
        let a = Span::single_line(1, 2, 5);
        let b = Span::single_line(3, 0, 4);
        assert_eq!(a.union(&b), b.union(&a));
        assert_eq!(
            a.union(&b),
            Span {
                start_line: 1,
                start_column: 2,
                end_line: 3,
                end_column: 4,
            }
        );
    }
}
