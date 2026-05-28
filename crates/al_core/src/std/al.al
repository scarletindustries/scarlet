pub type Int

pub type Float

pub type String

pub type Array(a)

pub type Binary

pub type Bool {
	True
	False
}

pub type Nil {
	Nil
}

pub type Option(a) {
	Some(value a)
	None
}

pub type Result(a, e) {
	Ok(value a)
	Err(error e)
}

@vm(println)
pub fn println(x a) Nil
