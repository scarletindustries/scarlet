//! Mid-flight process migration between schedulers.
//!
//! A process owns its stack, frames and heap, so moving it is a plain
//! [`Process`] move. The exception is its socket fds: `tcp_listeners` and
//! `tcp_connections` are per-scheduler side tables, so those must be re-homed
//! from the donor into the destination. A [`Migrant`] is that pairing.
//!
//! Two invariants the donation path in `scheduler_loop` relies on:
//!
//! - A migrant stays counted in `Runtime::live` for its whole journey. Donation
//!   never decrements and adoption never increments, so [`VM::adopt_migrant`]
//!   must push the run queue directly rather than call `Runtime::submit`, which
//!   would double-count.
//! - [`VM::can_donate_fds`] is read-only and runs first, so [`VM::detach_fds`]
//!   can be infallible.

use std::collections::HashSet;
use std::net::TcpStream;

use crate::bytecode::{SocketValue, Value};

use super::{Process, VM};

/// Connection fds re-homed out of a scheduler's tables, ready to travel with a
/// [`Migrant`] or `Seed`. Listeners are excluded: the listener socket is
/// program-wide (`Runtime.shared_listeners`) and every scheduler registers the
/// same fd on demand (`VM::ensure_listener`).
pub(super) type DetachedFds = Vec<(i32, TcpStream, u64)>;

/// A process in flight between schedulers.
pub(super) struct Migrant {
    pub process: Process,
    /// Moved, so the donor loses them.
    pub connections: DetachedFds,
}

// This assert cannot see everything it needs to. `Send` is structural, and a
// parked native stack (`Process::parked`) is `Send` as a byte buffer no matter
// what its words point at — a raw pointer into the donor scheduler's `VM` would
// pass here and corrupt on resume. That obligation is discharged by the
// process-stack purity invariant in the `vm::stack` module doc. The assert still
// catches ordinary regressions like an `Rc` or a non-Send fd wrapper.
const _: () = crate::assert_send::<Migrant>();

/// Visit every socket reachable from `v`. The one fd-walk shared by spawn
/// seeding, migrant detach, and the donor-side guard.
///
/// It must descend into immortal (frozen) subgraphs: a socket is an immediate,
/// so a frozen tuple can carry one. Pruning on `Value::is_immortal` drops those
/// fds on the floor.
pub(super) fn for_each_socket(v: &Value, visit: &mut impl FnMut(SocketValue)) {
    if let Some(s) = v.as_socket() {
        visit(s);
    }
    v.for_each_child_ref(|c| for_each_socket(c, visit));
}

/// Visit every socket reachable from the process's stack or any frame's
/// closure. Frames of a recursive function share one closure object, so
/// closures are walked once each by address. A socket reachable from several
/// roots is still visited once per root; callers dedup ids.
pub(super) fn for_each_process_socket(p: &Process, visit: &mut impl FnMut(SocketValue)) {
    for v in &p.stack {
        for_each_socket(v, visit);
    }
    let mut seen: HashSet<usize> = HashSet::new();
    for frame in &p.frames {
        if let Some(addr) = frame.captures.object_addr()
            && !seen.insert(addr)
        {
            continue;
        }
        for_each_socket(&frame.captures, visit);
    }
}

/// Every socket id reachable from the process, listeners included, deduplicated.
fn process_socket_ids(p: &Process) -> Vec<i32> {
    let mut ids = Vec::new();
    for_each_process_socket(p, &mut |s| ids.push(s.id));
    ids.sort_unstable();
    ids.dedup();
    ids
}

impl VM {
    /// May `victim`'s connection fds safely leave this scheduler? Read-only,
    /// and must run before any side effect of donation.
    ///
    /// A connection may only move while quiescent here. Donation aborts if a
    /// referenced id is in `pending_connects` (an in-flight connect whose
    /// completion handler looks the socket up in this table) or armed in a
    /// parked sibling's `Wait::Io` fds (that sibling's wake path resolves the
    /// id through this scheduler's tables and would break silently).
    ///
    /// Listener ids need no guarding: the listener socket is shared
    /// program-wide, so the destination re-resolves the same fd.
    pub(super) fn can_donate_fds(&self, victim: &Process) -> bool {
        let mut ids = Vec::new();
        for_each_process_socket(victim, &mut |s| {
            if !s.is_listener {
                ids.push(s.id);
            }
        });
        if ids.is_empty() {
            return true;
        }

        if ids.iter().any(|id| self.pending_connects.contains_key(id)) {
            return false;
        }
        // The victim itself is runnable (in run_queue, not parked), so its own
        // fds cannot appear here — any hit is a sibling's armed fd.
        !ids.iter().any(|id| self.io_waiters.contains_key(id))
    }

    /// Re-home every connection fd a suspended process references out of this
    /// scheduler's tables so they can travel with it.
    pub(super) fn detach_fds(&mut self, p: &Process) -> DetachedFds {
        // Owned-but-unreferenced connections must travel too. The process's
        // death closes them, and death happens on whichever scheduler it ends
        // on, so leaving them here strands them open forever.
        let mut ids = process_socket_ids(p);
        ids.extend(
            self.tcp_connections
                .iter()
                .filter(|&(_, conn)| conn.owner == p.pid)
                .map(|(&id, _)| id),
        );
        ids.sort_unstable();
        ids.dedup();
        self.detach_socket_ids(ids)
    }

    /// Move the connections among `ids` out of this scheduler's table. Shared
    /// by spawn seeding and donation. Listeners in `ids` are left alone, and an
    /// id with no connection entry (a listener, or one an earlier spawn already
    /// moved away) is skipped.
    pub(super) fn detach_socket_ids(&mut self, ids: impl IntoIterator<Item = i32>) -> DetachedFds {
        let mut connections: DetachedFds = Vec::new();
        for id in ids {
            // `evict_connection` also fails anything parked on the id. Donation
            // is guarded by `can_donate_fds`, but `build_seed` is not: a sibling
            // parked on a captured id would be unwakeable forever, its readiness
            // going to a poller that no longer owns the fd.
            if let Some(conn) = self.evict_connection(id) {
                connections.push((id, conn.stream, conn.owner));
            }
        }
        connections
    }

    /// Adopt connection fds arriving with a seed or migrant. Each keeps the
    /// owner it arrived with. Infallible by design: a poller registration
    /// failure is logged and the process still runs, with ops on the
    /// unwatchable socket surfacing as `NetError` at use.
    pub(super) fn adopt_connections(&mut self, connections: DetachedFds) {
        for (id, c, owner) in connections {
            if let Err(e) = self.track_connection(id, c, owner) {
                eprintln!("warning: cannot watch adopted connection {id}: {e}");
            }
        }
    }

    /// Adopt a migrated process: take its fds and queue it runnable.
    ///
    /// The migrant is already counted in `Runtime::live`, so it goes on the run
    /// queue directly, never through `Runtime::submit`, which assumes uncounted
    /// work. The caller must `sync_globals()` first so the top-level bindings
    /// the migrant reads exist here.
    pub(super) fn adopt_migrant(&mut self, m: Migrant) {
        self.adopt_connections(m.connections);
        self.run_queue.push_back(m.process);
    }
}

#[cfg(test)]
mod tests {
    //! The two transports between schedulers: migration is a move, spawn is a
    //! copy. Identity is asserted through `Value::object_addr`, which is stable
    //! under a `Process` move, so a moved value keeps its exact address and a
    //! copied one must not.
    //!
    //! End-to-end coverage lives in `tests/vm_migration.rs`.

    use super::super::{CallFrame, halt_test_vm};
    use super::*;
    use crate::heap::ProcHeap;

    fn test_heap() -> ProcHeap {
        ProcHeap::new()
    }

    fn addr(v: &Value) -> usize {
        v.object_addr().expect("heap-backed value")
    }

    #[test]
    fn move_is_zero_copy_and_preserves_state() {
        // Three frames sharing one closure, the shape a recursive function
        // leaves behind.
        let mut heap = test_heap();
        let cap = Value::str_in(&mut heap, "shared");
        let shared = Value::closure_in(&mut heap, 0, &[Value::small_int(7), cap]);
        let two = Value::str_in(&mut heap, "two");
        let four = Value::str_in(&mut heap, "four");
        let arr = Value::array_in(&mut heap, &[Value::small_int(3), four]);
        let stack = vec![Value::small_int(1), two, arr];

        let shared_addr = addr(&shared);
        let stack_addrs: Vec<Option<usize>> = stack.iter().map(Value::object_addr).collect();

        let frames = vec![
            CallFrame {
                func_idx: 0,
                code_start: 0,
                ip: 3,
                base_slot: 0,
                captures: shared.clone(),
            },
            CallFrame {
                func_idx: 1,
                code_start: 10,
                ip: 5,
                base_slot: 2,
                captures: shared.clone(),
            },
            CallFrame {
                func_idx: 1,
                code_start: 10,
                ip: 7,
                base_slot: 4,
                captures: shared.clone(),
            },
        ];
        let p = Process {
            heap,
            stack,
            frames,
            is_main: false,
            pid: 0,
            native_floor: 0,
            native_reds: 0,
            native_pending: None,
            parked: None,
        };

        let mut donor = halt_test_vm();
        let connections = donor.detach_fds(&p);
        assert!(connections.is_empty());

        let mut dest = halt_test_vm();
        dest.adopt_migrant(Migrant {
            process: p,
            connections,
        });
        let q = dest
            .run_queue
            .pop_back()
            .expect("the migrant must be queued runnable");

        // A move, not a copy: every frame still holds the original closure
        // allocation at its original address.
        assert!(!q.is_main);
        assert_eq!(q.frames.len(), 3);
        assert!(
            q.frames
                .iter()
                .all(|f| f.captures.object_addr() == Some(shared_addr))
        );

        let moved_addrs: Vec<Option<usize>> = q.stack.iter().map(Value::object_addr).collect();
        assert_eq!(moved_addrs, stack_addrs);
        assert_eq!(q.stack[0].as_int(), Some(1));
        assert_eq!(q.stack[1].as_str(), Some("two"));
        let moved_arr = q.stack[2].as_array().expect("array slot");
        assert_eq!(moved_arr.len(), 2);
        assert_eq!(moved_arr.get(0).and_then(|e| e.as_int()), Some(3));
        let elem = moved_arr.get(1).expect("array element");
        assert_eq!(elem.as_str(), Some("four"));

        let meta: Vec<_> = q
            .frames
            .iter()
            .map(|f| (f.func_idx, f.code_start, f.ip, f.base_slot))
            .collect();
        assert_eq!(meta, vec![(0, 0, 3, 0), (1, 10, 5, 2), (1, 10, 7, 4)]);
    }

    /// Collect the distinct heap objects reachable from `v`, keyed by address.
    /// Comparing the source's count with the copy's pins sharing exactly:
    /// duplicating a shared object inflates the count, deduplicating
    /// equal-but-distinct objects deflates it.
    fn distinct_heap_nodes(v: &Value, out: &mut Vec<usize>) {
        let Some(a) = v.object_addr() else { return };
        if out.contains(&a) {
            return;
        }
        out.push(a);
        v.for_each_child_ref(|c| distinct_heap_nodes(c, out));
    }

    #[test]
    fn move_keeps_distinct_capture_allocations_distinct() {
        // Two frames holding equal but distinct closure allocations: the
        // transport moves words and must never canonicalize by value.
        let mut heap = test_heap();
        let cap_a = Value::str_in(&mut heap, "same");
        let a = Value::closure_in(&mut heap, 1, &[cap_a]);
        let cap_b = Value::str_in(&mut heap, "same");
        let b = Value::closure_in(&mut heap, 1, &[cap_b]);
        let a_addr = addr(&a);
        let b_addr = addr(&b);
        assert_ne!(a_addr, b_addr);

        let p = Process {
            heap,
            stack: Vec::new(),
            frames: vec![
                CallFrame {
                    func_idx: 1,
                    code_start: 10,
                    ip: 2,
                    base_slot: 0,
                    captures: a,
                },
                CallFrame {
                    func_idx: 1,
                    code_start: 10,
                    ip: 4,
                    base_slot: 1,
                    captures: b,
                },
            ],
            is_main: false,
            pid: 0,
            native_floor: 0,
            native_reds: 0,
            native_pending: None,
            parked: None,
        };

        let mut donor = halt_test_vm();
        let connections = donor.detach_fds(&p);
        let mut dest = halt_test_vm();
        dest.adopt_migrant(Migrant {
            process: p,
            connections,
        });
        let q = dest.run_queue.pop_back().expect("queued migrant");

        assert_eq!(q.frames[0].captures.object_addr(), Some(a_addr));
        assert_eq!(q.frames[1].captures.object_addr(), Some(b_addr));
        let qa = q.frames[0].captures.as_closure().expect("closure");
        let qb = q.frames[1].captures.as_closure().expect("closure");
        assert_eq!(qa.func_idx(), 1);
        assert_eq!(qb.func_idx(), 1);
        assert_eq!(qa.captures()[0].as_str(), Some("same"));
        assert_eq!(qb.captures()[0].as_str(), Some("same"));
    }

    #[test]
    fn seed_copy_preserves_sharing_without_dedup_or_aliasing() {
        // The graph must live in the spawner's own spaces: `ProcHeap::spawn`
        // classifies pointers by address range and leaves frozen or foreign
        // ones untouched, so a graph allocated elsewhere is skipped, not copied.
        let mut spawner = test_heap();
        let cap = Value::str_in(&mut spawner, "cap");
        let shared = Value::array_in(&mut spawner, &[Value::small_int(1), cap]);
        let twin_cap = Value::str_in(&mut spawner, "cap");
        let twin = Value::array_in(&mut spawner, &[Value::small_int(1), twin_cap]);
        let root = Value::closure_in(&mut spawner, 0, &[shared.clone(), shared.clone(), twin]);

        let mut src_nodes = Vec::new();
        distinct_heap_nodes(&root, &mut src_nodes);

        let (_child_heap, copy) = ProcHeap::spawn(&root);

        // The spawner's graph is untouched: the copy restores every forwarded
        // header.
        let mut src_after = Vec::new();
        distinct_heap_nodes(&root, &mut src_after);
        assert_eq!(src_after, src_nodes);

        let mut copy_nodes = Vec::new();
        distinct_heap_nodes(&copy, &mut copy_nodes);
        assert_eq!(
            copy_nodes.len(),
            src_nodes.len(),
            "the copy must preserve sharing exactly — no duplication, no dedup"
        );

        // Both graphs are live here, so addresses cannot be reused.
        for &a in &copy_nodes {
            assert!(
                !src_nodes.contains(&a),
                "copied node must not alias the source graph"
            );
        }

        // Captures 0 and 1 are one allocation; the equal twin at 2 is another.
        let cl = copy.as_closure().expect("copied closure");
        assert_eq!(cl.func_idx(), 0);
        let caps = cl.captures();
        assert_eq!(addr(&caps[0]), addr(&caps[1]));
        assert_ne!(addr(&caps[0]), addr(&caps[2]));

        for v in [&caps[0], &caps[2]] {
            let arr = v.as_array().expect("array capture");
            assert_eq!(arr.len(), 2);
            assert_eq!(arr.get(0).and_then(|e| e.as_int()), Some(1));
            let s = arr.get(1).expect("capture element");
            assert_eq!(s.as_str(), Some("cap"));
        }
    }

    #[test]
    fn sockets_leave_listeners_put_move_connections_skip_dangling() {
        use crate::bytecode::SocketValue;
        use std::net::{TcpListener, TcpStream};

        let mut donor = halt_test_vm();

        // A listener (stays put), an established connection (moves), and a
        // dangling id 3 whose fd an earlier spawn already moved away.
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let bind_addr = listener.local_addr().unwrap();
        let client = TcpStream::connect(bind_addr).expect("connect");
        let (_server, _) = listener.accept().expect("accept");
        donor.tcp_listeners.insert(1, std::sync::Arc::new(listener));
        donor.tcp_connections.insert(
            2,
            super::super::Conn {
                stream: client,
                owner: 7,
            },
        );

        let socket = |id, is_listener| Value::socket(SocketValue { id, is_listener });
        // Id 2 is on the stack and in a frame's captures: its fd must move once.
        let mut heap = test_heap();
        let captures = Value::closure_in(&mut heap, 0, &[socket(2, false)]);
        let p = Process {
            heap,
            stack: vec![socket(1, true), socket(2, false), socket(3, false)],
            frames: vec![CallFrame {
                func_idx: 0,
                code_start: 0,
                ip: 0,
                base_slot: 0,
                captures,
            }],
            is_main: false,
            pid: 0,
            native_floor: 0,
            native_reds: 0,
            native_pending: None,
            parked: None,
        };

        let connections = donor.detach_fds(&p);

        assert!(donor.tcp_listeners.contains_key(&1));
        assert_eq!(connections.len(), 1);
        assert_eq!(connections[0].0, 2);
        assert!(donor.tcp_connections.is_empty());

        let mut dest = halt_test_vm();
        dest.adopt_migrant(Migrant {
            process: p,
            connections,
        });
        assert!(!dest.tcp_listeners.contains_key(&1));
        assert!(dest.tcp_connections.contains_key(&2));
        assert_eq!(dest.run_queue.len(), 1);

        let q = dest.run_queue.pop_back().expect("queued migrant");
        let cl = q.frames[0]
            .captures
            .as_closure()
            .expect("frame holds a closure");
        assert_eq!(cl.captures().len(), 1);
        assert_eq!(
            cl.captures()[0].as_socket(),
            Some(SocketValue {
                id: 2,
                is_listener: false
            })
        );
    }
}
