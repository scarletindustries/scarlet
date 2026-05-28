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

@vm(binary__append)
pub fn append(a Binary, b Binary) Binary
