//! Mid-flight process migration between schedulers
//!
//! A process owns all of its state — operand stack, call frames, and the
//! heap its values live in — so moving it to another scheduler is a plain
//! [`Process`] move; nothing is copied or rebuilt. The one thing
//! a process references that it does NOT own is its socket fds:
//! `tcp_listeners`/`tcp_connections` are per-scheduler side tables, so a
//! migrating process's fds must be re-homed from the donor's tables into the
//! destination's. A [`Migrant`] is exactly that pairing: the moved process
//! plus the fds traveling with it.
//!
//! Protocol invariants (the donation path in `scheduler_loop` relies on these):
//!
//! - **Live count.** A migrant stays counted in `Runtime::live` for its whole
//!   journey: it was counted when it spawned, donation never decrements, and
//!   adoption never increments. This is why [`VM::adopt_migrant`] pushes the
//!   run queue directly instead of going through `Runtime::submit`
//!   (which would double-count), and why a migrant in transit holds
//!   `live > 0` — there is no shutdown race while one is in flight.
//! - **Abort safety.** [`VM::detach_fds`] either succeeds or leaves the
//!   donor's tables exactly as they were (every fd it had already moved is
//!   re-inserted), so the donor can simply re-queue the untouched process.

use std::collections::HashSet;
use std::net::TcpStream;
use std::os::fd::AsRawFd;

use al_core::bytecode::{SocketValue, Value, ValueView};

use super::{Process, VM};

/// A process in flight between schedulers: the suspended [`Process`] moved
/// as-is, alongside the socket fds it references, re-homed out of the donor's
/// per-scheduler tables for insertion into the destination's.
pub(super) struct Migrant {
    /// The suspended process, moved whole — stack, frames, heap untouched.
    pub process: Process,
    /// Connections the process references (moved — the donor loses them).
    /// Listeners do not travel: the destination binds its own reuseport socket
    /// from the shared address on first accept (`VM::ensure_listener`), and the
    /// donor keeps its own entry.
    pub connections: Vec<(i32, TcpStream)>,
}

// A migrant crosses an OS-thread boundary, so it must be `Send`. The fd
// handles are inherently `Send`; the meaningful constraint is
// `Process: Send`, which must hold by construction — a process owns its
// memory outright, nothing in it is shared with the donor scheduler. This
// assert is the compile-time gate for migration soundness: if `Process` ever
// regains a field that cannot move across threads, donation must not build.
const _: () = {
    const fn assert_send<T: Send>() {}
    assert_send::<Migrant>();
};

/// Visit every socket reachable from `v` — the one fd-walk shared by every
/// path that pairs a value graph with the per-scheduler socket tables:
/// seeding a spawn (`VM::build_seed`), detaching a migrant's fds
/// ([`process_socket_ids`]), and the donor-side guard (`VM::can_donate_fds`,
/// which filters to connections).
pub(super) fn for_each_socket(v: &Value, visit: &mut impl FnMut(SocketValue)) {
    match v.kind() {
        ValueView::Array(seq) => {
            for e in seq.iter() {
                for_each_socket(&e, visit);
            }
        }
        ValueView::Tuple(t) => {
            for e in t {
                for_each_socket(e, visit);
            }
        }
        ValueView::Closure(c) => {
            for e in c.captures() {
                for_each_socket(e, visit);
            }
        }
        ValueView::Enum(e) => {
            for p in e.payload() {
                for_each_socket(p, visit);
            }
        }
        ValueView::Socket(s) => visit(s),
        // Leaves that cannot reference a socket: immediates and heap
        // values whose payload holds no `Value` words. Listed
        // explicitly so a future value kind must decide its socket
        // story here instead of being skipped silently — this walk is
        // what keeps migration's fd re-homing sound.
        ValueView::Int(_)
        | ValueView::Float(_)
        | ValueView::Bool(_)
        | ValueView::Nil
        | ValueView::Str(_)
        | ValueView::Range(..)
        | ValueView::Binary(_)
        // A Map's only backing (Env) holds no values, let alone sockets. A
        // future socket-bearing backing must re-home its fds here.
        | ValueView::Map(_) => {}
    }
}

/// Visit every socket reachable from the process's stack or any frame's
/// closure. Frames of a recursive function all share one closure object
/// (see `CallFrame::captures`), so each distinct closure — keyed by its
/// arena address — is walked once. A socket reachable through several
/// roots is still visited once per root; callers dedup ids as needed.
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

/// Every socket id reachable from the process — listeners and connections
/// alike — deduplicated: the fd set [`VM::detach_fds`] re-homes.
fn process_socket_ids(p: &Process) -> Vec<i32> {
    let mut ids = Vec::new();
    for_each_process_socket(p, &mut |s| ids.push(s.id));
    ids.sort_unstable();
    ids.dedup();
    ids
}

/// Connection fds re-homed out of a scheduler's tables, ready to travel with a
/// [`Migrant`] or `Seed`. Listeners are not included: they are a shared,
/// addr-keyed resource each scheduler re-materializes on demand
/// (`VM::ensure_listener`), never moved.
pub(super) type DetachedFds = Vec<(i32, TcpStream)>;

impl VM {
    /// Re-home every connection fd a suspended process references out of the
    /// donor's per-scheduler tables, for donation: the process itself moves
    /// whole; only its connections need detaching so they can travel with it.
    pub(super) fn detach_fds(&mut self, p: &Process) -> DetachedFds {
        self.detach_socket_ids(process_socket_ids(p))
    }

    /// Move the connections among `ids` out of this scheduler's table — the
    /// one fd transfer shared by spawn seeding (`VM::build_seed`) and donation
    /// ([`VM::detach_fds`]). A connection belongs to exactly one scheduler, so
    /// it is removed here (with a defensive poller delete first — a runnable
    /// process has nothing armed, so it is normally a no-op) and travels to the
    /// destination. Listeners among `ids` are left untouched: each scheduler
    /// binds its own reuseport socket from the shared address on first accept,
    /// so a captured listener needs no transfer and the donor keeps accepting.
    /// An id with no connection entry is either a listener or dangling (an
    /// earlier spawn moved it away) and is skipped.
    pub(super) fn detach_socket_ids(&mut self, ids: impl IntoIterator<Item = i32>) -> DetachedFds {
        let mut connections: Vec<(i32, TcpStream)> = Vec::new();
        for id in ids {
            if let Some(c) = self.tcp_connections.remove(&id) {
                // The fd is leaving this scheduler; drop any poller
                // registration before it goes (defensive — see doc comment).
                self.poller_deregister(c.as_raw_fd());
                connections.push((id, c));
            }
        }
        connections
    }

    /// Adopt a bundle of connection fds arriving with a seed or migrant into
    /// this scheduler's tables — the ONE fd-adoption policy shared by both
    /// intake paths (`hydrate_seed`, `adopt_migrant`). A poller registration
    /// failure (e.g. EMFILE) is logged and the process still runs; ops on the
    /// unwatchable socket surface as `NetError` at use. Adoption is thus
    /// infallible: an fd-table hiccup during transport never takes the whole
    /// scheduler down.
    pub(super) fn adopt_connections(&mut self, connections: DetachedFds) {
        for (id, c) in connections {
            if let Err(e) = self.track_connection(id, c) {
                eprintln!("warning: cannot watch adopted connection {id}: {e}");
            }
        }
    }

    /// Adopt a migrated process: take ownership of its fds and queue the
    /// moved process as runnable. Nothing is rebuilt — the process arrives
    /// exactly as it was suspended on the donor.
    ///
    /// The migrant is already counted in `Runtime::live` (counted at its
    /// original spawn; donation never decrements), so it is pushed onto the
    /// run queue directly — never routed through `Runtime::submit`, which
    /// assumes uncounted work.
    ///
    /// The caller must `sync_globals()` before invoking this, exactly as for
    /// seed hydration, so any top-level bindings the migrant reads exist here.
    pub(super) fn adopt_migrant(&mut self, m: Migrant) {
        self.adopt_connections(m.connections);
        // Only non-main runnable processes are eligible for donation, so the
        // adopted process is never main.
        self.run_queue.push_back(m.process);
    }
}

#[cfg(test)]
mod tests {
    //! Coverage for the two transports between schedulers, and the
    //! properties each must preserve:
    //!
    //! - **Migration is a move**: the process arrives holding its original
    //!   allocations in the heap that traveled with it — shared captures stay
    //!   shared, distinct allocations stay distinct, nothing is rebuilt or
    //!   deduplicated.
    //! - **Spawn is a copy** (`ProcHeap::spawn_copy`, the spawn-side graph copy
    //!   entry): sharing in the source graph is sharing in the copy, and the
    //!   copy aliases nothing in the spawner.
    //!
    //! Identity is asserted through `Value::object_addr` (the arena header
    //! address): slab addresses are stable under a `Process`/`ProcHeap` move,
    //! so a moved value must keep its exact address, while a copied value
    //! must land at an address inside the child heap's own spaces.
    //!
    //! Semantic end-to-end coverage (real programs migrating mid-run) lives
    //! in `tests/vm_migration.rs`.

    use super::super::{CallFrame, halt_test_vm};
    use super::*;
    use al_core::heap::ProcHeap;

    /// A process heap with a pre-granted allocation budget: these tests fill
    /// the heap directly, with no VM `ensure()` loop staking budgets.
    fn test_heap() -> ProcHeap {
        ProcHeap::new()
    }

    /// The arena header address of a heap-backed value — the identity the
    /// move/copy assertions compare.
    fn addr(v: &Value) -> usize {
        v.object_addr().expect("heap-backed value")
    }

    #[test]
    fn move_is_zero_copy_and_preserves_state() {
        // Three frames sharing ONE closure, the shape a recursive function
        // leaves behind. Everything is allocated in the process's own heap —
        // the heap that migrates with it — so address identity across the
        // move is exactly the property under test.
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
        };

        let mut donor = halt_test_vm();
        let connections = donor.detach_fds(&p);
        // No sockets referenced: nothing re-homed.
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

        // The transport is a move, not a copy: every frame still holds the
        // ORIGINAL closure allocation, at its original address, inside the
        // heap that traveled with the process.
        assert!(!q.is_main);
        assert_eq!(q.frames.len(), 3);
        assert!(
            q.frames
                .iter()
                .all(|f| f.captures.object_addr() == Some(shared_addr))
        );

        // Each stack slot is the original word: heap-backed slots kept their
        // exact addresses, and the payloads read back intact through them.
        let moved_addrs: Vec<Option<usize>> = q.stack.iter().map(Value::object_addr).collect();
        assert_eq!(moved_addrs, stack_addrs);
        assert_eq!(q.stack[0].as_int(), Some(1));
        assert_eq!(q.stack[1].as_str(), Some("two"));
        let moved_arr = q.stack[2].as_array().expect("array slot");
        assert_eq!(moved_arr.len(), 2);
        assert_eq!(moved_arr.get(0).and_then(|e| e.as_int()), Some(3));
        let elem = moved_arr.get(1).expect("array element");
        assert_eq!(elem.as_str(), Some("four"));

        // Frame metadata survived intact.
        let meta: Vec<_> = q
            .frames
            .iter()
            .map(|f| (f.func_idx, f.code_start, f.ip, f.base_slot))
            .collect();
        assert_eq!(meta, vec![(0, 0, 3, 0), (1, 10, 5, 2), (1, 10, 7, 4)]);
    }

    /// Collect the distinct heap objects reachable from `v`, keyed by arena
    /// address (`Value::object_addr`). Sharing shows up as an object
    /// referenced many times but collected once, so comparing the source's
    /// count with the copy's pins "sharing preserved exactly": duplicating a
    /// shared object inflates the copy's count, deduplicating
    /// equal-but-distinct objects deflates it. The walk follows the
    /// `ValueView` arms, so it counts the objects user values reach (a
    /// `Seq`'s interior nodes are hidden behind its root) — the same
    /// granularity on both sides, which is all the comparison needs. No
    /// address is ever compared *across* the two graphs.
    fn distinct_heap_nodes(v: &Value, out: &mut Vec<usize>) {
        let Some(a) = v.object_addr() else { return };
        if out.contains(&a) {
            return;
        }
        out.push(a);
        match v.kind() {
            ValueView::Array(seq) => {
                for e in seq.iter() {
                    distinct_heap_nodes(&e, out);
                }
            }
            ValueView::Tuple(t) => {
                for e in t {
                    distinct_heap_nodes(e, out);
                }
            }
            ValueView::Closure(c) => {
                for e in c.captures() {
                    distinct_heap_nodes(e, out);
                }
            }
            ValueView::Enum(e) => {
                for p in e.payload() {
                    distinct_heap_nodes(p, out);
                }
            }
            _ => {}
        }
    }

    #[test]
    fn move_keeps_distinct_capture_allocations_distinct() {
        // Two frames holding equal-but-DISTINCT closure allocations: the
        // transport must never canonicalize equal values into one
        // allocation — it moves words, it does not rebuild by value.
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
        };

        let mut donor = halt_test_vm();
        let connections = donor.detach_fds(&p);
        let mut dest = halt_test_vm();
        dest.adopt_migrant(Migrant {
            process: p,
            connections,
        });
        let q = dest.run_queue.pop_back().expect("queued migrant");

        // Each frame still holds its own original allocation, both inside
        // the heap that traveled with the process...
        assert_eq!(q.frames[0].captures.object_addr(), Some(a_addr));
        assert_eq!(q.frames[1].captures.object_addr(), Some(b_addr));
        // ...and the two equal closures were not collapsed into one
        // (`a_addr != b_addr` above; the addresses did not change). The
        // payloads read back intact through the moved handles.
        let qa = q.frames[0].captures.as_closure().expect("closure");
        let qb = q.frames[1].captures.as_closure().expect("closure");
        assert_eq!(qa.func_idx(), 1);
        assert_eq!(qb.func_idx(), 1);
        assert_eq!(qa.captures()[0].as_str(), Some("same"));
        assert_eq!(qb.captures()[0].as_str(), Some("same"));
    }

    #[test]
    fn seed_copy_preserves_sharing_without_dedup_or_aliasing() {
        // The spawn-side copy (`ProcHeap::spawn_copy`, the spawn-side graph copy entry
        // point) is the one cross-scheduler transport that copies a value
        // graph. Its required properties:
        //
        // - a node referenced twice travels as ONE allocation referenced
        //   twice (shared captures stay shared), and
        // - two distinct-but-equal allocations stay DISTINCT (the copy's
        //   `src → dst` map is keyed by identity, never by value).
        //
        // The graph must live in the spawner's own spaces: the copy
        // classifies pointers by address range and leaves frozen/foreign
        // pointers untouched, so a graph allocated anywhere else would be
        // skipped, not copied.
        let mut spawner = test_heap();
        let cap = Value::str_in(&mut spawner, "cap");
        let shared = Value::array_in(&mut spawner, &[Value::small_int(1), cap]);
        let twin_cap = Value::str_in(&mut spawner, "cap");
        let twin = Value::array_in(&mut spawner, &[Value::small_int(1), twin_cap]);
        let root = Value::closure_in(&mut spawner, 0, &[shared.clone(), shared.clone(), twin]);

        let mut src_nodes = Vec::new();
        distinct_heap_nodes(&root, &mut src_nodes);

        let (_child_heap, copy) = spawner.spawn_copy(&root);

        // The spawner's graph is untouched (Clone-mode copy restores every
        // forwarded header): same objects at the same addresses.
        let mut src_after = Vec::new();
        distinct_heap_nodes(&root, &mut src_after);
        assert_eq!(src_after, src_nodes);

        // Sharing preserved exactly: the copy has the same distinct-object
        // count as the source. Losing the sharing of `shared` (referenced by
        // captures 0 and 1) would inflate the count; deduplicating the equal
        // `twin` would deflate it.
        let mut copy_nodes = Vec::new();
        distinct_heap_nodes(&copy, &mut copy_nodes);
        assert_eq!(
            copy_nodes.len(),
            src_nodes.len(),
            "the copy must preserve sharing exactly — no duplication, no dedup"
        );

        // No aliasing: every copied object is a fresh allocation, disjoint
        // from the source graph (both are live here, so addresses don't reuse).
        for &a in &copy_nodes {
            assert!(
                !src_nodes.contains(&a),
                "copied node must not alias the source graph"
            );
        }

        // The sharing structure inside the copy mirrors the source: captures
        // 0 and 1 are one allocation, the equal twin at 2 is another.
        let cl = copy.as_closure().expect("copied closure");
        assert_eq!(cl.func_idx(), 0);
        let caps = cl.captures();
        assert_eq!(addr(&caps[0]), addr(&caps[1]));
        assert_ne!(addr(&caps[0]), addr(&caps[2]));

        // And the payload survived the trip.
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
        use al_core::bytecode::SocketValue;
        use std::net::{TcpListener, TcpStream};

        let mut donor = halt_test_vm();

        // One listener (stays put — not transferred), one established
        // connection (moved), and a dangling id 3 whose fd an earlier spawn
        // already moved away.
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let bind_addr = listener.local_addr().unwrap();
        let client = TcpStream::connect(bind_addr).expect("connect");
        let (_server, _) = listener.accept().expect("accept");
        donor.tcp_listeners.insert(1, listener);
        donor.tcp_connections.insert(2, client);

        let socket = |id, is_listener| Value::socket(SocketValue { id, is_listener });
        // Id 2 appears on the stack AND in a frame's closure captures: the
        // walk must still move its fd exactly once.
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
        };

        let connections = donor.detach_fds(&p);

        // The listener does not travel: the donor keeps its own entry and
        // every scheduler binds its own reuseport socket from the shared
        // address on first accept.
        assert!(donor.tcp_listeners.contains_key(&1));
        // The connection moved exactly once; the donor lost it. The dangling
        // id 3 was skipped entirely.
        assert_eq!(connections.len(), 1);
        assert_eq!(connections[0].0, 2);
        assert!(donor.tcp_connections.is_empty());

        // Adoption takes only the moved connection; no listener is carried.
        let mut dest = halt_test_vm();
        dest.adopt_migrant(Migrant {
            process: p,
            connections,
        });
        assert!(!dest.tcp_listeners.contains_key(&1));
        assert!(dest.tcp_connections.contains_key(&2));
        assert_eq!(dest.run_queue.len(), 1);

        // The moved frame's closure socket is untouched.
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
