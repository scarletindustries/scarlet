import al/binary

@vm(io__read_file)
pub fn read_file(path String) Result(Binary, String)

@vm(io__write_file)
pub fn write_file(path String, data Binary) Result(Nil, String)

pub fn read_text(path String) Result(String, String) {
	match read_file(path) {
		Ok(b) -> match binary.to_string(b) {
			Ok(s) -> Ok(s)
			Err(Nil) -> Err('file is not valid UTF-8')
		}
		Err(e) -> Err(e)
	}
}

pub fn write_text(path String, s String) Result(Nil, String) {
	write_file(path, binary.from_string(s))
}
