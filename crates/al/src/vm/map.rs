//! The `al/map` builtins over the opaque [`Map`](ValueView::Map) value.
//!
//! A `Map(k, v)` is a heap value whose representation is chosen by its
//! [`MapBacking`]:
//!
//! - [`MapBacking::Env`] — a zero-copy live view of the host process
//!   environment, typed `Map(String, String)`. Reads ([`get`](VM::map_get),
//!   [`has`](VM::map_has), …) go straight to `std::env`; nothing is
//!   materialized until — and only what — a lookup asks for. A *write*
//!   (`set`/`delete`) has nowhere to go in the environment, so it first
//!   materializes the whole environment into a HAMT and updates that.
//! - [`MapBacking::Hamt`] — an in-memory persistent hash array mapped trie
//!   ([`al_core::bytecode::hamt`]). `set`/`delete` path-copy the trie and share
//!   every untouched subtree, so the prior map stays valid.
//!
//! Every op here is a pure stack transformation: it never parks and never
//! touches `ip`. Allocation is reference-counted — an op pops its operands and
//! builds its result; HAMT writes path-copy and share untouched subtrees, and
//! the environment view materializes a host-side snapshot whose strings live
//! off-arena.
//!
//! Environment entries that are not valid UTF-8 (key or value) are invisible
//! to every op: an AL `String` is UTF-8, so a non-representable entry simply
//! does not belong to the `Map(String, String)` view. This keeps the read ops
//! and the materialized-on-write copy mutually consistent.

use al_core::bytecode::{MapBacking, Value, ValueView, hamt, hash_value};

use super::{VM, VmError, VmResult};

impl VM {
    /// `map.new` — push a fresh empty HAMT map.
    #[inline(never)]
    pub(super) fn map_new(&mut self) -> VmResult<()> {
        let v = hamt::empty(&mut self.heap);
        self.stack.push(v);
        Ok(())
    }

    /// `process.env` — push the environment-backed map. Allocates only the
    /// two-word handle; no environment data is copied.
    #[inline(never)]
    pub(super) fn env_map(&mut self) -> VmResult<()> {
        let v = Value::env_map_in(&mut self.heap);
        self.stack.push(v);
        Ok(())
    }

    /// `map.get(m, key) -> Option(v)`. Map at depth 1, key on top.
    #[inline(never)]
    pub(super) fn map_get(&mut self) -> VmResult<()> {
        let found = match self.map_backing_at(1, "map.get")? {
            MapBacking::Env => {
                let val = self.peek_env_lookup(0);
                let _key = self.pop()?;
                let _map = self.pop()?;
                // Build the result string fresh from the host snapshot.
                val.map(|s| Value::str_in(&mut self.heap, &s))
            }
            MapBacking::Hamt => {
                let hash = hash_value(self.peek_at_or(0)?);
                let key = self.pop()?;
                let map = self.pop()?;
                // The stored value already lives in the arena; `Some` shares it.
                hamt::get(&map, &key, hash)
            }
        };
        let result = match found {
            Some(v) => self.make_some(v),
            None => self.make_none(),
        };
        self.stack.push(result);
        Ok(())
    }

    /// `map.has(m, key) -> Bool`. Map at depth 1, key on top.
    #[inline(never)]
    pub(super) fn map_has(&mut self) -> VmResult<()> {
        let present = match self.map_backing_at(1, "map.has")? {
            MapBacking::Env => self.peek_env_lookup(0).is_some(),
            MapBacking::Hamt => {
                let key = self.peek_at_or(0)?.clone();
                let hash = hash_value(&key);
                let map = self.peek_at_or(1)?.clone();
                hamt::get(&map, &key, hash).is_some()
            }
        };
        let _key = self.pop()?;
        let _map = self.pop()?;
        self.stack.push(Value::bool(present));
        Ok(())
    }

    /// `map.keys(m) -> Array(k)`. Map on top.
    #[inline(never)]
    pub(super) fn map_keys(&mut self) -> VmResult<()> {
        self.map_collect("map.keys", Proj::Keys)
    }

    /// `map.values(m) -> Array(v)`. Map on top.
    #[inline(never)]
    pub(super) fn map_values(&mut self) -> VmResult<()> {
        self.map_collect("map.values", Proj::Values)
    }

    /// `map.to_list(m) -> Array((k, v))`. Map on top.
    #[inline(never)]
    pub(super) fn map_to_list(&mut self) -> VmResult<()> {
        self.map_collect("map.to_list", Proj::Pairs)
    }

    /// Shared body of `keys`/`values`/`to_list`: project each entry per
    /// `proj` into an `Array`. HAMT entries are shared; environment strings
    /// are built fresh. Only the array spine (and, for `Pairs`, one tuple per
    /// entry) is new.
    fn map_collect(&mut self, op: &'static str, proj: Proj) -> VmResult<()> {
        let items: Vec<Value> = match self.map_backing_at(0, op)? {
            MapBacking::Env => {
                let entries = env_entries();
                let _map = self.pop()?;
                entries
                    .into_iter()
                    .map(|(k, v)| match proj {
                        Proj::Keys => Value::str_in(&mut self.heap, &k),
                        Proj::Values => Value::str_in(&mut self.heap, &v),
                        Proj::Pairs => {
                            let kv = Value::str_in(&mut self.heap, &k);
                            let vv = Value::str_in(&mut self.heap, &v);
                            Value::tuple_in(&mut self.heap, &[kv, vv])
                        }
                    })
                    .collect()
            }
            MapBacking::Hamt => {
                let map = self.pop()?;
                let mut items = Vec::with_capacity(hamt::size(&map));
                hamt::for_each(&map, |k, v| {
                    items.push(match proj {
                        Proj::Keys => k,
                        Proj::Values => v,
                        Proj::Pairs => Value::tuple_in(&mut self.heap, &[k, v]),
                    });
                });
                items
            }
        };
        let arr = Value::array_in(&mut self.heap, &items);
        self.stack.push(arr);
        Ok(())
    }

    /// `map.size(m) -> Int`. Map on top.
    #[inline(never)]
    pub(super) fn map_size(&mut self) -> VmResult<()> {
        let n = match self.map_backing_at(0, "map.size")? {
            MapBacking::Env => env_entries().len() as i64,
            MapBacking::Hamt => hamt::size(self.peek_at_or(0)?) as i64,
        };
        let _map = self.pop()?;
        self.push_int(n);
        Ok(())
    }

    /// `map.set(m, key, value) -> Map`. Map at depth 2, key at 1, value on top.
    #[inline(never)]
    pub(super) fn map_set(&mut self) -> VmResult<()> {
        let backing = self.map_backing_at(2, "map.set")?;
        let hash = hash_value(self.peek_at_or(1)?);
        let value = self.pop()?;
        let key = self.pop()?;
        let map = self.pop()?;
        // A write to the environment view has nowhere to go, so materialize the
        // whole environment into a HAMT and update that instead.
        let base = match backing {
            MapBacking::Hamt => map,
            MapBacking::Env => self.build_env_hamt(&env_entries()),
        };
        let next = hamt::insert(&mut self.heap, &base, key, value, hash);
        self.stack.push(next);
        Ok(())
    }

    /// `map.delete(m, key) -> Map`. Map at depth 1, key on top.
    #[inline(never)]
    pub(super) fn map_delete(&mut self) -> VmResult<()> {
        let backing = self.map_backing_at(1, "map.delete")?;
        let hash = hash_value(self.peek_at_or(0)?);
        let key = self.pop()?;
        let map = self.pop()?;
        let base = match backing {
            MapBacking::Hamt => map,
            MapBacking::Env => self.build_env_hamt(&env_entries()),
        };
        let next = hamt::remove(&mut self.heap, &base, &key, hash);
        self.stack.push(next);
        Ok(())
    }

    /// Build a HAMT holding `entries` (an environment snapshot), allocating the
    /// entry strings and the trie.
    fn build_env_hamt(&mut self, entries: &[(String, String)]) -> Value {
        let mut map = hamt::empty(&mut self.heap);
        for (k, v) in entries {
            let kv = Value::str_in(&mut self.heap, k);
            let vv = Value::str_in(&mut self.heap, v);
            let hash = hash_value(&kv);
            map = hamt::insert(&mut self.heap, &map, kv, vv, hash);
        }
        map
    }

    /// The backing of the map operand `d` slots below the top, read without
    /// popping.
    fn map_backing_at(&self, d: usize, op: &'static str) -> VmResult<MapBacking> {
        let v = self.peek_at_or(d)?;
        match v.kind() {
            ValueView::Map(m) => Ok(m.backing()),
            _ => Err(VmError::type_mismatch(op, "Map", v)),
        }
    }

    /// Look up the string key `d` slots below the top against the process
    /// environment, returning the value if both the key and its value are
    /// valid UTF-8. Used by the `Env` read ops.
    fn peek_env_lookup(&self, d: usize) -> Option<String> {
        let key = self.peek_at(d).and_then(|v| v.as_str())?;
        std::env::var(key).ok()
    }
}

/// What [`VM::map_collect`] pulls out of each entry.
#[derive(Clone, Copy)]
enum Proj {
    Keys,
    Values,
    Pairs,
}

/// Snapshot the process environment as the UTF-8 `(key, value)` pairs that the
/// `Map(String, String)` view exposes. `std::env::vars` would panic on a
/// non-UTF-8 entry, so walk `vars_os` and drop anything not representable as an
/// AL `String`.
fn env_entries() -> Vec<(String, String)> {
    std::env::vars_os()
        .filter_map(|(k, v)| Some((k.into_string().ok()?, v.into_string().ok()?)))
        .collect()
}
