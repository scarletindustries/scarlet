use super::*;

/// A key is the string `k<n>`, so keys are cells whose references count.
fn key(heap: &mut Heap, n: u64) -> Value {
    Value::cell(heap.string(format!("k{n}").as_bytes()).expect("room"))
}

/// The `n` a [`key`] was made from.
fn number(heap: &Heap, v: Value) -> u64 {
    let mut text = Vec::new();
    if let Some(cell) = v.as_cell() {
        heap.read_string(cell, &mut text);
    }
    String::from_utf8_lossy(&text[1..]).parse().expect("a key")
}

/// A hash where keys differ only in the top bits, which makes long paths
/// of one-child nodes.
fn deep(heap: &Heap, v: Value) -> u64 {
    (number(heap, v) % 13) << 60 | 0x0555_5555_5555_5555
}

/// A hash that puts every key in one of three groups whose whole hashes
/// are equal, which makes collisions.
fn clashing(heap: &Heap, v: Value) -> u64 {
    number(heap, v) % 3
}

fn release(heap: &mut Heap, v: Value) {
    if let Some(cell) = v.as_cell() {
        heap.release(cell).expect("a live cell");
    }
}

/// A small random number generator, the same every run.
struct Lcg(u64);

impl Lcg {
    fn below(&mut self, n: u64) -> u64 {
        self.0 = self
            .0
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1);
        (self.0 >> 33) % n
    }
}

/// Run `set` and `delete` at random, then delete until the map is empty,
/// and check it against a plain list after each step. Every key must find
/// what the list says, the size must agree, and the map must be `==` to
/// one built fresh from the list in another order. That last check holds
/// only if the shape depends on the entries alone. Some versions are kept
/// and checked again at the end, then all are freed, leaving nothing.
fn against_a_list(hash: Hasher) {
    const KEYS: u64 = 120;
    let mut heap = Heap::default();
    let mut rng = Lcg(7);
    let mut model: Vec<Option<u64>> = vec![None; KEYS as usize];
    let mut map = empty(&mut heap).expect("room");
    let mut versions = Vec::new();
    for step in 0..1500 {
        let k = rng.below(KEYS);
        let kv = key(&mut heap, k);
        let before = model.clone();
        // A version kept for later shares its nodes with every map made
        // from it, so a set that gives its map up must not change them.
        let keep = step % 50 == 0;
        let next = if rng.below(3) == 0 {
            model[k as usize] = None;
            if keep {
                delete_by(&mut heap, map, kv, hash).expect("room")
            } else {
                delete_owned_by(&mut heap, map, kv, hash).expect("room")
            }
        } else {
            let v = rng.below(1000);
            model[k as usize] = Some(v);
            let vv = Value::cell(heap.string(format!("v{v}").as_bytes()).expect("room"));
            let next = if keep {
                set_by(&mut heap, map, kv, vv, hash)
            } else {
                set_owned_by(&mut heap, map, kv, vv, hash)
            };
            release(&mut heap, vv);
            next.expect("room")
        };
        release(&mut heap, kv);
        if keep {
            versions.push((map, before));
        }
        map = next;
        check(&mut heap, map, &model, hash);
    }
    // Then empty it, in a random order, so every node shrinks away.
    let mut left: Vec<u64> = (0..KEYS).filter(|&k| model[k as usize].is_some()).collect();
    while !left.is_empty() {
        let k = left.swap_remove(rng.below(left.len() as u64) as usize);
        let kv = key(&mut heap, k);
        let next = delete_owned_by(&mut heap, map, kv, hash).expect("room");
        release(&mut heap, kv);
        map = next;
        model[k as usize] = None;
        check(&mut heap, map, &model, hash);
    }
    assert_eq!(top(&heap, map).bits(), Value::NIL.bits());
    versions.push((map, model));
    for (map, model) in &versions {
        check(&mut heap, *map, model, hash);
    }
    for (map, _) in versions {
        heap.release(map).expect("a live cell");
    }
    assert_eq!(heap.live(), 0, "a map lost track of a reference");
}

fn check(heap: &mut Heap, map: Cell, model: &[Option<u64>], hash: Hasher) {
    let mut text = Vec::new();
    for (k, want) in model.iter().enumerate() {
        let kv = key(heap, k as u64);
        let got = get_by(heap, map, kv, hash).map(|v| {
            text.clear();
            heap.read_string(v.as_cell().expect("a string"), &mut text);
            String::from_utf8_lossy(&text).into_owned()
        });
        assert_eq!(got, want.map(|v| format!("v{v}")), "key k{k}");
        release(heap, kv);
    }
    let present = model.iter().filter(|v| v.is_some()).count() as u64;
    assert_eq!(size(heap, map), present);
    assert_eq!(entries(heap, map).len() as u64, present);

    let mut fresh = empty(heap).expect("room");
    for (k, v) in model.iter().enumerate().rev() {
        if let Some(v) = v {
            let kv = key(heap, k as u64);
            let vv = Value::cell(heap.string(format!("v{v}").as_bytes()).expect("room"));
            let next = set_by(heap, fresh, kv, vv, hash).expect("room");
            release(heap, kv);
            release(heap, vv);
            heap.release(fresh).expect("a live cell");
            fresh = next;
        }
    }
    assert!(
        eq::equal(heap, Value::cell(map), Value::cell(fresh)),
        "equal entries, different shapes"
    );
    heap.release(fresh).expect("a live cell");
}

#[test]
fn a_map_agrees_with_a_list() {
    against_a_list(hash::hash);
}

#[test]
fn a_map_agrees_with_a_list_down_long_paths() {
    against_a_list(deep);
}

#[test]
fn a_map_agrees_with_a_list_when_whole_hashes_clash() {
    against_a_list(clashing);
}

/// A set copies only the path to its key: one new node a level, and the
/// new map's root. Every other node is shared with the old map.
#[test]
fn a_set_copies_only_its_path() {
    let mut heap = Heap::default();
    let mut map = empty(&mut heap).expect("room");
    for n in 0..5000 {
        let kv = key(&mut heap, n);
        let next = set(&mut heap, map, kv, Value::NIL).expect("room");
        release(&mut heap, kv);
        heap.release(map).expect("a live cell");
        map = next;
    }
    let before = heap.live();
    let kv = key(&mut heap, 5000);
    let next = set(&mut heap, map, kv, Value::NIL).expect("room");
    release(&mut heap, kv);
    // 5,000 entries is three levels, so at most three nodes and a root,
    // and the new key's string is the one other cell.
    assert!(
        heap.live() - before <= 5,
        "{} new cells",
        heap.live() - before
    );
    heap.release(next).expect("a live cell");
    heap.release(map).expect("a live cell");
    assert_eq!(heap.live(), 0);
}

/// The owned forms give up the caller's reference, so on a map that was
/// already freed they stop instead of freeing it again.
#[test]
fn an_owned_edit_of_a_freed_map_is_a_dead_cell() {
    let mut heap = Heap::default();
    let map = empty(&mut heap).expect("room");
    heap.release(map).expect("a live cell");
    let k = Value::int(1).expect("small");
    assert_eq!(set_owned(&mut heap, map, k, k), Err(Fault::DeadCell));
    assert_eq!(delete_owned(&mut heap, map, k), Err(Fault::DeadCell));
}

/// A set over a key gives up the value that was there. When that value
/// was freed behind the map's back, the set stops at it.
#[test]
fn a_set_over_a_freed_value_is_a_dead_cell() {
    let mut heap = Heap::default();
    let map = empty(&mut heap).expect("room");
    let k = Value::int(1).expect("small");
    let v = key(&mut heap, 9);
    let map = set_owned(&mut heap, map, k, v).expect("room");
    // One for the test's own reference, and one that was the map's.
    release(&mut heap, v);
    release(&mut heap, v);
    assert_eq!(set_owned(&mut heap, map, k, k), Err(Fault::DeadCell));
}
