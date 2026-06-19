import al/map.{Map}

// The program's command-line arguments. The first element is the path to the
// entrypoint that was run; the rest are the arguments passed after it, in order.
@vm(process__argv)
pub fn argv() Array(String)

// The process environment as a `Map(String, String)`. The map is a zero-copy
// live view: no variables are read or copied until you look one up, so this is
// cheap to call even when the environment is large.
@vm(process__env)
pub fn env() Map(String, String)

// Look up a single environment variable, returning `Some(value)` if it is set
// (and valid UTF-8) and `None` otherwise.
pub fn get_env(key String) Option(String) {
	map.get(env(), key)
}
