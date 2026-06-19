import al/list

// An immutable map from keys of type `k` to values of type `v`.
//
// `Map` is opaque: it is a host-backed handle whose storage is chosen by the
// runtime, not a value you can pattern-match. `new` gives an empty in-memory
// map; updates (`set`/`delete`) return a new map and leave the original intact,
// sharing structure between versions. `process.env` returns a special
// `Map(String, String)` that reads through to the process environment without
// copying it — a lookup touches only the key you ask for.
pub type Map(k, v)

// A new empty map.
@vm(map__new)
pub fn new() Map(k, v)

// `m` with `key` bound to `value`, as a new map. The original `m` is unchanged
// (maps are persistent): `next = map.set(m, k, v)` leaves `m` intact.
@vm(map__set)
pub fn set(m Map(k, v), key k, value v) Map(k, v)

// `m` with `key` removed, as a new map. Absent keys leave the map unchanged.
@vm(map__delete)
pub fn delete(m Map(k, v), key k) Map(k, v)

// Look up `key`, returning `Some(value)` if present and `None` otherwise.
@vm(map__get)
pub fn get(m Map(k, v), key k) Option(v)

// Whether `key` is present in the map.
@vm(map__has)
pub fn has(m Map(k, v), key k) Bool

// Every key in the map. The order is the backing's own iteration order and is
// not guaranteed to be stable across calls.
@vm(map__keys)
pub fn keys(m Map(k, v)) Array(k)

// Every value in the map, in the same order as `keys`.
@vm(map__values)
pub fn values(m Map(k, v)) Array(v)

// The number of entries in the map.
@vm(map__size)
pub fn size(m Map(k, v)) Int

// Every entry as a `(key, value)` tuple, in the same order as `keys`.
@vm(map__to_list)
pub fn to_list(m Map(k, v)) Array((k, v))

// Build a map from a list of `(key, value)` tuples. Later entries win on
// duplicate keys.
pub fn from_list(entries Array((k, v))) Map(k, v) {
	list.fold(entries, new(), fn(acc, entry) {
		match entry {
			(key, value) -> set(acc, key, value)
		}
	})
}

// Reduce over the entries: `f` is called with the accumulator, key, and value
// of each entry. Iteration order is the backing's own.
//
// Folds straight over the key and value arrays in lockstep — `keys` and
// `values` share an order — so no intermediate `(k, v)` list is built.
pub fn fold(m Map(k, v), init r, f fn(r, k, v) r) r {
	fold_entries(keys(m), values(m), init, f)
}

fn fold_entries(ks Array(k), vs Array(v), acc r, f fn(r, k, v) r) r {
	match ks {
		[] -> acc
		[key, ..krest] -> match vs {
			[value, ..vrest] -> fold_entries(krest, vrest, f(acc, key, value), f)
			[] -> acc
		}
	}
}

// `a` with every entry of `b` added; on a shared key, `b`'s value wins.
pub fn merge(a Map(k, v), b Map(k, v)) Map(k, v) {
	fold(b, a, fn(acc, key, value) set(acc, key, value))
}

// A new map keeping only the entries whose key/value satisfy `keep`.
pub fn filter(m Map(k, v), keep fn(k, v) Bool) Map(k, v) {
	fold(m, new(), fn(acc, key, value) {
		if keep(key, value) {
			set(acc, key, value)
		} else {
			acc
		}
	})
}
