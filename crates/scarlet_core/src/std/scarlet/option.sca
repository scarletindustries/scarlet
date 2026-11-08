pub fn map(o Option(a), f fn(a) b) Option(b) {
	match o {
		Some(x) -> Some(f(x))
		None -> None
	}
}

pub fn then(o Option(a), f fn(a) Option(b)) Option(b) {
	match o {
		Some(x) -> f(x)
		None -> None
	}
}

pub fn unwrap(o Option(a), default a) a {
	match o {
		Some(x) -> x
		None -> default
	}
}

pub fn or_else(o Option(a), fallback fn() Option(a)) Option(a) {
	match o {
		Some(_) -> o
		None -> fallback()
	}
}

pub fn is_some(o Option(a)) Bool {
	match o {
		Some(_) -> True
		None -> False
	}
}

pub fn is_none(o Option(a)) Bool {
	match o {
		Some(_) -> False
		None -> True
	}
}
