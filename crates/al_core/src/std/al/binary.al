@vm(binary__from_string)
pub fn from_string(s String) Binary

@vm(binary__to_string)
pub fn to_string(b Binary) Result(String, Nil)

@vm(binary__bit_size)
pub fn bit_size(b Binary) Int

@vm(binary__byte_size)
pub fn byte_size(b Binary) Int

@vm(binary__slice)
pub fn slice(b Binary, at_bit Int, take_bits Int) Result(Binary, Nil)

// Byte-oriented slice of `b[start .. start+len]` (offsets and length in bytes;
// `slice` itself takes bits). O(1): shares `b`'s backing and copies nothing.
// An out-of-range request yields an empty binary, so a parser can take field
// views by offset without per-call error handling.
pub fn slice_bytes(b Binary, start Int, len Int) Binary {
	match slice(b, start * 8, len * 8) {
		Ok(v) -> v
		Err(_) -> from_string('')
	}
}

@vm(binary__append)
pub fn append(a Binary, b Binary) Binary

@vm(binary__index_of)
pub fn index_of(haystack Binary, needle Binary, from Int) Option(Int)

@vm(binary__parse_int)
pub fn parse_int(b Binary, radix Int) Option(Int)

@vm(binary__eq_ignore_ascii_case)
pub fn eq_ignore_ascii_case(a Binary, b Binary) Bool

@vm(binary__to_ascii_lower)
pub fn to_ascii_lower(b Binary) Binary

@vm(binary__from_int_ascii)
pub fn from_int_ascii(n Int, radix Int) Binary
