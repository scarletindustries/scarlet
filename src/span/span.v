module span

pub struct Span {
pub:
	start_line   int @[required]
	start_column int @[required]
	end_line     int @[required]
	end_column   int @[required]
}

pub fn point_span(line int, column int) Span {
	return Span{
		start_line:   line
		start_column: column
		end_line:     line
		end_column:   column + 1
	}
}

pub fn range_span(line int, start_column int, end_column int) Span {
	return Span{
		start_line:   line
		start_column: start_column
		end_line:     line
		end_column:   end_column
	}
}
