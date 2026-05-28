pub fn map(xs Array(a), f fn(a) b) Array(b) {
	match xs {
		[] -> []
		[h, ..t] -> [f(h), ..map(t, f)]
	}
}

pub fn filter(xs Array(a), p fn(a) Bool) Array(a) {
	match xs {
		[] -> []
		[h, ..t] -> if p(h) { [h, ..filter(t, p)] } else { filter(t, p) }
	}
}

pub fn fold(xs Array(a), init b, f fn(b, a) b) b {
	match xs {
		[] -> init
		[h, ..t] -> fold(t, f(init, h), f)
	}
}

pub fn reverse(xs Array(a)) Array(a) {
	fold(xs, [], fn(acc, x) [x, ..acc])
}

pub fn length(xs Array(a)) Int {
	fold(xs, 0, fn(n, _) n + 1)
}

pub fn contains(xs Array(a), target a) Bool {
	match xs {
		[] -> False
		[h, ..t] -> if h == target { True } else { contains(t, target) }
	}
}

pub fn find(xs Array(a), p fn(a) Bool) Option(a) {
	match xs {
		[] -> None
		[h, ..t] -> if p(h) { Some(h) } else { find(t, p) }
	}
}

pub fn any(xs Array(a), p fn(a) Bool) Bool {
	match xs {
		[] -> False
		[h, ..t] -> if p(h) { True } else { any(t, p) }
	}
}

pub fn all(xs Array(a), p fn(a) Bool) Bool {
	match xs {
		[] -> True
		[h, ..t] -> if p(h) { all(t, p) } else { False }
	}
}
