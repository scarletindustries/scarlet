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
//! touches `ip`. Worst-case `ensure` budgets are computed while the operands
//! are still rooted, per the rooting rule — for the HAMT ops from
//! [`hamt::insert_cost`]/[`hamt::remove_cost`], for the environment from a
//! host-side snapshot whose strings live off-arena.
//!
//! Environment entries that are not valid UTF-8 (key or value) are invisible
//! to every op: an AL `String` is UTF-8, so a non-representable entry simply
//! does not belong to the `Map(String, String)` view. This keeps the read ops
//! and the materialized-on-write copy mutually consistent.

use al_core::bytecode::{MapBacking, Value, ValueView, hamt, hash_value};

use super::{VM, VmResult, cost};

impl VM {
    /// `map.new` — push a fresh empty HAMT map.
    pub(super) fn map_new(&mut self) -> VmResult<()> {
        self.ensure(hamt::EMPTY_WORDS);
        let v = hamt::empty(&mut self.heap);
        self.stack.push(v);
        Ok(())
    }

    /// `process.env` — push the environment-backed map. Allocates only the
    /// two-word handle; no environment data is copied.
    pub(super) fn env_map(&mut self) -> VmResult<()> {
        self.ensure(cost::MAP);
        let v = Value::env_map_in(&mut self.heap);
        self.stack.push(v);
        Ok(())
    }

    /// `map.get(m, key) -> Option(v)`. Map at depth 1, key on top.
    pub(super) fn map_get(&mut self) -> VmResult<()> {
        let found = match self.map_backing_at(1, "map.get")? {
            MapBacking::Env => {
                let val = self.peek_env_lookup(0);
                self.ensure(cost::WRAP + val.as_deref().map_or(0, |s| cost::str(s.len())));
                let _key = self.pop()?;
                let _map = self.pop()?;
                // Build the result string fresh from the host snapshot.
                val.map(|s| Value::str_in(&mut self.heap, &s))
            }
            MapBacking::Hamt => {
                self.ensure(cost::WRAP);
                let key = self.peek_at(0).copied();
                let hash = key.map_or(0, |k| hash_value(&k));
                let key = self.pop()?;
                let map = self.pop()?;
                // The stored value already lives in the arena; `Some` shares it.
                hamt::get(map, &key, hash)
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
    pub(super) fn map_has(&mut self) -> VmResult<()> {
        let present = match self.map_backing_at(1, "map.has")? {
            MapBacking::Env => self.peek_env_lookup(0).is_some(),
            MapBacking::Hamt => {
                let key = self.peek_at(0).copied();
                let hash = key.map_or(0, |k| hash_value(&k));
                let map = self.at(1)?;
                key.is_some_and(|k| hamt::get(map, &k, hash).is_some())
            }
        };
        let _key = self.pop()?;
        let _map = self.pop()?;
        self.stack.push(Value::bool(present));
        Ok(())
    }

    /// `map.keys(m) -> Array(k)`. Map on top.
    pub(super) fn map_keys(&mut self) -> VmResult<()> {
        self.map_collect("map.keys", true)
    }

    /// `map.values(m) -> Array(v)`. Map on top.
    pub(super) fn map_values(&mut self) -> VmResult<()> {
        self.map_collect("map.values", false)
    }

    /// Shared body of `keys`/`values`: build an `Array` of the key (or value)
    /// of every entry. The keys/values already live in the arena (a HAMT) or
    /// are built fresh (the environment); either way only the array spine is
    /// new, so the budget is `seq_build(n)` plus, for the environment, the
    /// strings.
    fn map_collect(&mut self, op: &str, keys: bool) -> VmResult<()> {
        match self.map_backing_at(0, op)? {
            MapBacking::Env => {
                let entries = env_entries();
                let pick = |(k, v): &(String, String)| if keys { k.clone() } else { v.clone() };
                let need = cost::seq_build(entries.len())
                    + entries
                        .iter()
                        .map(|e| cost::str(pick(e).len()))
                        .sum::<usize>();
                self.ensure(need);
                let _map = self.pop()?;
                let items: Vec<Value> = entries
                    .iter()
                    .map(|e| Value::str_in(&mut self.heap, &pick(e)))
                    .collect();
                let arr = Value::array_in(&mut self.heap, &items);
                self.stack.push(arr);
            }
            MapBacking::Hamt => {
                let map = self.at(0)?;
                self.ensure(cost::seq_build(hamt::size(map)));
                let map = self.pop()?;
                let items: Vec<Value> = hamt::collect_entries(map)
                    .into_iter()
                    .map(|(k, v)| if keys { k } else { v })
                    .collect();
                let arr = Value::array_in(&mut self.heap, &items);
                self.stack.push(arr);
            }
        }
        Ok(())
    }

    /// `map.size(m) -> Int`. Map on top.
    pub(super) fn map_size(&mut self) -> VmResult<()> {
        let n = match self.map_backing_at(0, "map.size")? {
            MapBacking::Env => env_entries().len() as i64,
            MapBacking::Hamt => hamt::size(self.at(0)?) as i64,
        };
        let _map = self.pop()?;
        self.push_int(n);
        Ok(())
    }

    /// `map.to_list(m) -> Array((k, v))`. Map on top. One 3-word tuple per
    /// entry plus the array spine; for the environment, the entry strings too.
    pub(super) fn map_to_list(&mut self) -> VmResult<()> {
        match self.map_backing_at(0, "map.to_list")? {
            MapBacking::Env => {
                let entries = env_entries();
                let need = cost::seq_build(entries.len())
                    + entries
                        .iter()
                        .map(|(k, v)| cost::tuple(2) + cost::str(k.len()) + cost::str(v.len()))
                        .sum::<usize>();
                self.ensure(need);
                let _map = self.pop()?;
                let items: Vec<Value> = entries
                    .iter()
                    .map(|(k, v)| {
                        let kv = Value::str_in(&mut self.heap, k);
                        let vv = Value::str_in(&mut self.heap, v);
                        Value::tuple_in(&mut self.heap, &[kv, vv])
                    })
                    .collect();
                let arr = Value::array_in(&mut self.heap, &items);
                self.stack.push(arr);
            }
            MapBacking::Hamt => {
                let map = self.at(0)?;
                self.ensure(cost::seq_build(hamt::size(map)) + hamt::size(map) * cost::tuple(2));
                let map = self.pop()?;
                let items: Vec<Value> = hamt::collect_entries(map)
                    .into_iter()
                    .map(|(k, v)| Value::tuple_in(&mut self.heap, &[k, v]))
                    .collect();
                let arr = Value::array_in(&mut self.heap, &items);
                self.stack.push(arr);
            }
        }
        Ok(())
    }

    /// `map.set(m, key, value) -> Map`. Map at depth 2, key at 1, value on top.
    pub(super) fn map_set(&mut self) -> VmResult<()> {
        match self.map_backing_at(2, "map.set")? {
            MapBacking::Hamt => {
                let map = self.at(2)?;
                let key = self.at(1)?;
                let hash = hash_value(&key);
                self.ensure(hamt::insert_cost(hamt::size(map)));
                let value = self.pop()?;
                let key = self.pop()?;
                let map = self.pop()?;
                let next = hamt::insert(&mut self.heap, map, key, value, hash);
                self.stack.push(next);
            }
            MapBacking::Env => {
                // Materialize the environment into a HAMT, then insert. The new
                // key/value are already arena values; only the env snapshot's
                // strings and the trie are fresh.
                let entries = env_entries();
                let key = self.at(1)?;
                let hash = hash_value(&key);
                self.ensure(env_build_cost(&entries) + hamt::insert_cost(entries.len()));
                let value = self.pop()?;
                let key = self.pop()?;
                let _map = self.pop()?;
                let base = self.build_env_hamt(&entries);
                let next = hamt::insert(&mut self.heap, base, key, value, hash);
                self.stack.push(next);
            }
        }
        Ok(())
    }

    /// `map.delete(m, key) -> Map`. Map at depth 1, key on top.
    pub(super) fn map_delete(&mut self) -> VmResult<()> {
        match self.map_backing_at(1, "map.delete")? {
            MapBacking::Hamt => {
                let hash = hash_value(&self.at(0)?);
                self.ensure(hamt::remove_cost());
                let key = self.pop()?;
                let map = self.pop()?;
                let next = hamt::remove(&mut self.heap, map, &key, hash);
                self.stack.push(next);
            }
            MapBacking::Env => {
                let entries = env_entries();
                let key = self.at(0)?;
                let hash = hash_value(&key);
                self.ensure(env_build_cost(&entries) + hamt::remove_cost());
                let key = self.pop()?;
                let _map = self.pop()?;
                let base = self.build_env_hamt(&entries);
                let next = hamt::remove(&mut self.heap, base, &key, hash);
                self.stack.push(next);
            }
        }
        Ok(())
    }

    /// Build a HAMT holding `entries` (an environment snapshot). The caller
    /// must have reserved [`env_build_cost`] plus whatever its own follow-up
    /// op needs; this allocates the entry strings and trie without collecting.
    fn build_env_hamt(&mut self, entries: &[(String, String)]) -> Value {
        let mut map = hamt::empty(&mut self.heap);
        for (k, v) in entries {
            let kv = Value::str_in(&mut self.heap, k);
            let vv = Value::str_in(&mut self.heap, v);
            let hash = hash_value(&kv);
            map = hamt::insert(&mut self.heap, map, kv, vv, hash);
        }
        map
    }

    /// Copy of the operand `d` slots below the top. Used by the map ops after
    /// [`map_backing_at`] has confirmed the operands are present, so a `None`
    /// here is a compiler bug, reported rather than panicked.
    fn at(&self, d: usize) -> VmResult<Value> {
        self.peek_at(d)
            .copied()
            .ok_or_else(|| "Stack underflow. This is likely a compiler bug.".to_string())
    }

    /// The backing of the map operand `d` slots below the top, read without
    /// popping so the operand stays rooted across the budget `ensure`.
    fn map_backing_at(&self, d: usize, op: &str) -> VmResult<MapBacking> {
        match self.peek_at(d).map(Value::kind) {
            Some(ValueView::Map(m)) => Ok(m.backing()),
            _ => Err(format!("{op} requires a Map")),
        }
    }

    /// Look up the string key `d` slots below the top against the process
    /// environment, returning the value if both the key and its value are
    /// valid UTF-8. Used by the `Env` read ops for their pre-pop budget.
    fn peek_env_lookup(&self, d: usize) -> Option<String> {
        let key = self.peek_at(d).and_then(|v| v.as_str())?;
        std::env::var(key).ok()
    }
}

/// Arena words to build a HAMT from an environment snapshot: each entry's two
/// strings plus its incremental insert.
fn env_build_cost(entries: &[(String, String)]) -> usize {
    hamt::EMPTY_WORDS
        + entries
            .iter()
            .enumerate()
            .map(|(i, (k, v))| cost::str(k.len()) + cost::str(v.len()) + hamt::insert_cost(i))
            .sum::<usize>()
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
