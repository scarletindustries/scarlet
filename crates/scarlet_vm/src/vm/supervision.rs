//! The supervision tree: supervisors, the workers declared under them, and
//! factories of workers made on demand. This is `scarlet/process`'s
//! `supervisor` / `worker` / `factory` family and the introspection over it.
//!
//! # Shape
//!
//! Every entry here is created by a running process — its *creator* — and
//! sits under either another entry or, for a top-level supervisor, under the
//! creator itself; a process's entries are torn down when it ends, so the
//! whole tree is bounded by process lifetimes exactly as spawned children
//! are. Only the creator may declare into an entry: a worker's start function
//! that declared children into a supervisor it captured would declare them
//! again on every restart, so that is refused at the point of declaration and
//! the rule needs no cleanup logic anywhere else.
//!
//! A worker entry is a *slot*: its address (a durable subject, see
//! [`super::mailbox`]) and a retained copy of its start closure. The process
//! running in it at any moment is an *incarnation*, an ordinary process whose
//! record points back at the slot ([`super::processes`]); when it ends, the
//! slot decides — by its policy — whether to retire or to ask its owner for a
//! restart, and a restart copies the recipe again and hands the same address
//! to the new process. So restarts are invisible to everyone holding the
//! address, which is what lets a whole tree be declared as a sequence of
//! plain bindings, and it is why nothing here is ever re-run: a supervisor
//! restarting means re-incarnating the slots it already has, in the order
//! they were declared.
//!
//! # Restarts and budgets
//!
//! An owner (supervisor or factory) has a budget: more than `restarts`
//! restart events inside `within` and it gives up — stops everything under it
//! and reports itself dead to *its* owner, which handles that like any child
//! death (a supervisor is `Permanent` in its parent), up to a plain process,
//! which is killed: an unhandled failure still ends the program, as with
//! links, and every policy in between only attenuates that. The strategies
//! are OTP's: one-for-one re-incarnates the dead slot; one-for-all stops the
//! others too and re-incarnates all; rest-for-one does that for the slots
//! declared after the dead one.
//!
//! # Stopping is asynchronous
//!
//! Stopping a worker means killing its incarnation — or, for a worker
//! declared with a stop message, starting a small *stopper* process that
//! sends the message, waits the grace period and kills — and a kill lands
//! whenever the holding scheduler next looks. So anything that must stop
//! several things in order (one-for-all, rest-for-one, giving up, teardown)
//! is a small state machine, [`InFlight`], parked on the owner and advanced
//! by the deaths it is waiting for; each step hands back [`Action`]s for the
//! scheduler that observed the death to carry out once the lock is released.
//! Supervisors stop their children one at a time in reverse declaration
//! order, as OTP does, so a dependent is gone before its dependency is asked
//! to stop; a factory's members have no order and are stopped together.
//!
//! One lock covers the tree. Everything here is off the fast path — a
//! declaration, a death, a factory lookup — except that factory lookups are
//! what a program keyed on factories does per message; per-factory locking
//! is the obvious refinement when that shows up.

use std::collections::{HashMap, HashSet, VecDeque};
use std::fmt;
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};

use crate::bytecode::{Value, hash_value, values_equal};
use crate::heap::ProcHeap;

use super::mailbox::SubjectPlace;
use super::processes::{Exit, PidHash};
use super::sched::{Runtime, Seed};
use super::{Crash, IO_REDUCTION_COST, VM, VmError, VmResult, lock};
use crate::abi::AbiSlot;

/// The tree. Ids are minted from the pid counter, so an entry id and a pid
/// never collide and a `Supervised` value in the program can name either.
#[derive(Default)]
pub(super) struct Tree {
    entries: HashMap<u64, Entry, PidHash>,
    /// Top-level entries by the process that owns them; what a process's
    /// death tears down and what `children` of a process reports.
    owned: HashMap<u64, Vec<u64>, PidHash>,
    /// Recipes and keys retired under the lock, freed by whoever next
    /// releases it (`Runtime::with_tree`); a graph can be arbitrarily large.
    garbage: Vec<Value>,
    /// Factories freed under the lock; their key indexes are unpublished
    /// and dropped after it.
    retired_factories: Vec<u64>,
    /// Program-unique watch ids.
    next_watch: u64,
    /// The watches each process holds, as (entry, watch id), so a watcher's
    /// death releases them.
    holders: HashMap<u64, Vec<(u64, u64)>, PidHash>,
}

// Retained recipes are exclusively owned graphs touched only under the tree
// lock, so the tree crosses threads like a mailbox does.
const _: () = crate::assert_send::<Tree>();

struct Entry {
    parent: Parent,
    creator: u64,
    kind: Kind,
    /// Who wants to hear each time what is here exits (see `Watch`).
    watches: Vec<Watch>,
}

/// A registration made by `process.watch`: a closure, copied like a
/// message, started as a process of its own — applied to a description of
/// the exit — each time the entry's occupant exits, and again when the
/// entry itself is removed. Unlike a monitor it survives restarts: it is on
/// the entry, not on any one incarnation.
struct Watch {
    id: u64,
    holder: u64,
    closure: Value,
}

/// What a watch is being told.
pub(super) enum Ended {
    /// The occupant exited thus.
    Exited(Exit),
    /// The supervisor or factory here exhausted its budget.
    GaveUp { restarts: u32, within_ms: u64 },
    /// The entry itself was removed (torn down with something above it).
    Removed,
    /// There was nothing there to watch by the time the watch was placed.
    AlreadyGone,
}

/// The `status` a watch reports alongside an `Ended`: the stdlib's
/// `Status` constructor order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum After {
    Running,
    Restarting,
    Gone,
}

impl After {
    fn code(self) -> i64 {
        match self {
            After::Running => 0,
            After::Restarting => 1,
            After::Gone => 2,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Parent {
    Entry(u64),
    Process(u64),
}

enum Kind {
    Supervisor(Supervisor),
    Worker(Worker),
    Factory(Factory),
}

/// The restart policy of a worker slot. Codes are the stdlib's `Policy`
/// constructor order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Policy {
    Permanent,
    Transient,
    Temporary,
}

impl Policy {
    fn decode(code: i64) -> Option<Policy> {
        match code {
            0 => Some(Policy::Permanent),
            1 => Some(Policy::Transient),
            2 => Some(Policy::Temporary),
            _ => None,
        }
    }

    fn code(self) -> i64 {
        match self {
            Policy::Permanent => 0,
            Policy::Transient => 1,
            Policy::Temporary => 2,
        }
    }

    fn restarts_after(self, exit: &Exit) -> bool {
        match self {
            Policy::Permanent => true,
            Policy::Transient => match exit {
                Exit::Normal => false,
                Exit::Killed | Exit::Crashed(_) => true,
            },
            Policy::Temporary => false,
        }
    }
}

/// Codes are the stdlib's `Strategy` constructor order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Strategy {
    OneForOne,
    OneForAll,
    RestForOne,
}

impl Strategy {
    fn decode(code: i64) -> Option<Strategy> {
        match code {
            0 => Some(Strategy::OneForOne),
            1 => Some(Strategy::OneForAll),
            2 => Some(Strategy::RestForOne),
            _ => None,
        }
    }

    fn code(self) -> i64 {
        match self {
            Strategy::OneForOne => 0,
            Strategy::OneForAll => 1,
            Strategy::RestForOne => 2,
        }
    }
}

struct Budget {
    restarts: u32,
    within: Duration,
    /// Times of the restart events still inside the window.
    events: VecDeque<Instant>,
}

impl Budget {
    fn new(restarts: i64, within_ms: i64) -> Budget {
        Budget {
            restarts: restarts.clamp(0, u32::MAX as i64) as u32,
            within: Duration::from_millis(within_ms.max(0) as u64),
            events: VecDeque::new(),
        }
    }

    /// Record one restart event; false when it is one more than allowed.
    fn charge(&mut self, now: Instant) -> bool {
        while let Some(&t) = self.events.front() {
            if now.duration_since(t) > self.within {
                self.events.pop_front();
            } else {
                break;
            }
        }
        if self.events.len() >= self.restarts as usize {
            return false;
        }
        self.events.push_back(now);
        true
    }
}

struct Supervisor {
    strategy: Strategy,
    budget: Budget,
    /// Declaration order: start order, and the reverse is stop order.
    children: Vec<u64>,
    in_flight: Option<InFlight>,
    /// Restart requests that arrived while something was in flight.
    deferred: Vec<u64>,
}

struct Factory {
    /// `None` for disposable members: they are never restarted, and the
    /// factory has no budget to keep.
    restart: Option<Budget>,
    template: Value,
    members: HashSet<u64, PidHash>,
    /// The key index, shared with [`Runtime::factory_keys`] so lookups need
    /// not take the tree lock; the tree only removes from it, as members
    /// retire. Lock order: tree, then index.
    keys: Keys,
    in_flight: Option<InFlight>,
    deferred: Vec<u64>,
}

struct Worker {
    policy: Policy,
    address: u64,
    /// The start closure (a supervisor's worker) or the factory member's
    /// key or argument, which is applied to the factory's template.
    recipe: Value,
    stopper: Option<Value>,
    /// The scheduler every incarnation is placed on, for the acceptors of a
    /// server; anything else starts wherever its restart is observed.
    pin: Option<u32>,
    /// For a keyed factory member, the hash its reservation sits under in
    /// the factory's index, so retiring it can find the reservation.
    key_hash: Option<u64>,
    current: Option<u64>,
    /// Processes started in this slot so far; restarts are one fewer.
    incarnations: u32,
    /// An in-flight operation on the owner is waiting for the current
    /// incarnation to end; its death is then a step of that, not an event
    /// of its own.
    stopping: bool,
}

impl Worker {
    fn new(
        policy: Policy,
        address: u64,
        recipe: Value,
        stopper: Option<Value>,
        pin: Option<u32>,
        key_hash: Option<u64>,
    ) -> Worker {
        Worker {
            policy,
            address,
            recipe,
            stopper,
            pin,
            key_hash,
            current: None,
            incarnations: 0,
            stopping: false,
        }
    }
}

/// A factory's members by key. Its own lock, published in
/// [`Runtime::factory_keys`], so the per-message path of a program keyed on
/// factories — `lookup`, and `lookup_or_start` of an existing member — takes
/// one shared read lock and one factory-local mutex and never contends with
/// the tree. A miss reserves the key here, under this lock, before the
/// member is declared in the tree: that reservation is what makes "one
/// member per key" hold when several processes ask at once.
#[derive(Default)]
pub(super) struct KeyIndex {
    /// Buckets by key hash; a bucket is probed with `values_equal`.
    buckets: HashMap<u64, Vec<KeyEntry>, PidHash>,
}

struct KeyEntry {
    /// The index's own copy of the key.
    key: Value,
    address: u64,
    slot: u64,
}

type Keys = Arc<Mutex<KeyIndex>>;

/// The published indexes of the live factories, by factory id.
pub(super) type FactoryKeys = RwLock<HashMap<u64, Keys, PidHash>>;

impl KeyIndex {
    fn find(&self, hash: u64, key: &Value) -> Option<u64> {
        self.buckets
            .get(&hash)?
            .iter()
            .find(|e| values_equal(&e.key, key))
            .map(|e| e.address)
    }

    fn insert(&mut self, hash: u64, entry: KeyEntry) {
        self.buckets.entry(hash).or_default().push(entry);
    }

    /// Remove `slot`'s reservation, handing back its key for freeing.
    fn remove_slot(&mut self, hash: u64, slot: u64) -> Option<Value> {
        let bucket = self.buckets.get_mut(&hash)?;
        let i = bucket.iter().position(|e| e.slot == slot)?;
        let removed = bucket.swap_remove(i);
        if bucket.is_empty() {
            self.buckets.remove(&hash);
        }
        Some(removed.key)
    }
}

/// An operation on an owner that is waiting for incarnations to end.
struct InFlight {
    mode: Mode,
    /// Entries still to be stopped, in stop order (taken from the back).
    pending: Vec<u64>,
    /// Entries whose stop has been requested and not yet observed.
    awaiting: HashSet<u64, PidHash>,
    then: Then,
}

/// Whether the entries being stopped are kept (empty, to be re-incarnated
/// or left for the owner's owner to decide) or freed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    Stop,
    Retire,
}

enum Then {
    /// Re-incarnate these entries, in this order.
    Restart(Vec<u64>),
    /// The owner gave up: report its death to its own owner.
    Escalate,
    /// A parent entry's operation is waiting for this one.
    Notify(u64),
    /// Nothing more: the entries were being freed.
    Done,
}

/// What an owner decided to do about a child that needs restarting.
enum Decision {
    Deferred,
    Retire,
    Start(Vec<u64>),
    GiveUp {
        report: String,
        restarts: u32,
        within_ms: u64,
    },
    StopThenStart {
        victims: Vec<u64>,
        restart: Vec<u64>,
    },
}

/// Work for the caller to do once the tree lock is released. Ordered: a
/// batch's starts must happen in the order they were produced.
pub(super) enum Action {
    Kill(u64),
    /// A top-level supervisor gave up: kill the process that declared it,
    /// or, if that process has since returned, record the failure for the
    /// program's exit status and free what it left behind.
    FailOwner(u64),
    /// Start `stopper` (a fresh copy) applied to the address and the pid.
    FireStopper {
        stopper: Value,
        address: u64,
        pid: u64,
    },
    /// Re-incarnate a worker slot.
    Start(u64),
    CloseAddress(u64),
    Report(String),
    /// Start `closure` (a fresh copy) applied to a description of what
    /// happened at `entry`.
    Notify {
        closure: Value,
        entry: u64,
        ended: Ended,
        after: After,
    },
}

pub(super) type Actions = Vec<Action>;

/// Why a declaration was refused; becomes `Crash::Supervision`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Refusal {
    NotACreator,
    NoSuchSupervisor,
    NoSuchFactory,
    BadCode,
}

impl fmt::Display for Refusal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Refusal::NotACreator => {
                "only the process that created a supervisor or factory may declare into it"
            }
            Refusal::NoSuchSupervisor => "the supervisor no longer exists",
            Refusal::NoSuchFactory => "the factory no longer exists",
            Refusal::BadCode => "malformed policy or strategy",
        })
    }
}

/// What the program sees of an entry, before it is turned into values.
struct Info {
    /// 0 process, 1 worker, 2 supervisor, 3 factory.
    pub kind: i64,
    /// The worker's policy code, or the supervisor's strategy code; a
    /// factory reports 1 if its members restart, else 0.
    pub detail: i64,
    pub restarts: i64,
    pub within_ms: i64,
    /// 0 running, 1 present but with nothing running in it, 2 gone.
    pub status: i64,
    pub restarted: i64,
    pub pid: Option<u64>,
}

impl Tree {
    fn entry(&self, id: u64) -> Option<&Entry> {
        self.entries.get(&id)
    }

    fn supervisor_mut(&mut self, id: u64) -> Option<&mut Supervisor> {
        match self.entries.get_mut(&id).map(|e| &mut e.kind) {
            Some(Kind::Supervisor(s)) => Some(s),
            Some(Kind::Worker(_) | Kind::Factory(_)) | None => None,
        }
    }

    fn worker_mut(&mut self, id: u64) -> Option<&mut Worker> {
        match self.entries.get_mut(&id).map(|e| &mut e.kind) {
            Some(Kind::Worker(w)) => Some(w),
            Some(Kind::Supervisor(_) | Kind::Factory(_)) | None => None,
        }
    }

    /// The creator check every declaration goes through.
    fn owner_for_declaration(
        &self,
        owner: u64,
        by: u64,
        want_factory: bool,
    ) -> Result<(), Refusal> {
        match self.entry(owner) {
            Some(e) => {
                let right_kind = match (&e.kind, want_factory) {
                    (Kind::Supervisor(_), false) | (Kind::Factory(_), true) => true,
                    (Kind::Supervisor(_), true)
                    | (Kind::Factory(_), false)
                    | (Kind::Worker(_), true | false) => false,
                };
                if !right_kind {
                    return Err(if want_factory {
                        Refusal::NoSuchFactory
                    } else {
                        Refusal::NoSuchSupervisor
                    });
                }
                if e.creator != by {
                    return Err(Refusal::NotACreator);
                }
                Ok(())
            }
            None => Err(if want_factory {
                Refusal::NoSuchFactory
            } else {
                Refusal::NoSuchSupervisor
            }),
        }
    }

    fn factory_exists(&self, id: u64) -> Result<(), Refusal> {
        match self.entry(id).map(|e| &e.kind) {
            Some(Kind::Factory(_)) => Ok(()),
            Some(Kind::Supervisor(_) | Kind::Worker(_)) | None => Err(Refusal::NoSuchFactory),
        }
    }

    // ---- declaration -------------------------------------------------------------

    fn add_supervisor(
        &mut self,
        id: u64,
        parent: Parent,
        creator: u64,
        strategy: Strategy,
        budget: Budget,
    ) {
        self.entries.insert(
            id,
            Entry {
                parent,
                creator,
                watches: Vec::new(),
                kind: Kind::Supervisor(Supervisor {
                    strategy,
                    budget,
                    children: Vec::new(),
                    in_flight: None,
                    deferred: Vec::new(),
                }),
            },
        );
        self.attach(id, parent);
    }

    fn add_factory(
        &mut self,
        id: u64,
        parent: u64,
        creator: u64,
        restart: Option<Budget>,
        template: Value,
        keys: Keys,
    ) {
        self.entries.insert(
            id,
            Entry {
                parent: Parent::Entry(parent),
                creator,
                watches: Vec::new(),
                kind: Kind::Factory(Factory {
                    restart,
                    template,
                    members: HashSet::default(),
                    keys,
                    in_flight: None,
                    deferred: Vec::new(),
                }),
            },
        );
        self.attach(id, Parent::Entry(parent));
    }

    fn add_worker(&mut self, id: u64, parent: u64, creator: u64, worker: Worker) {
        self.entries.insert(
            id,
            Entry {
                parent: Parent::Entry(parent),
                creator,
                kind: Kind::Worker(worker),
                watches: Vec::new(),
            },
        );
        match self.entries.get_mut(&parent).map(|e| &mut e.kind) {
            Some(Kind::Supervisor(s)) => s.children.push(id),
            Some(Kind::Factory(f)) => {
                f.members.insert(id);
            }
            // Checked by the caller under this same lock.
            Some(Kind::Worker(_)) | None => {}
        }
    }

    fn attach(&mut self, id: u64, parent: Parent) {
        match parent {
            Parent::Process(pid) => self.owned.entry(pid).or_default().push(id),
            Parent::Entry(p) => {
                if let Some(s) = self.supervisor_mut(p) {
                    s.children.push(id);
                }
            }
        }
    }

    // ---- deaths ------------------------------------------------------------------

    /// The incarnation of `slot` ended. Returns what to do about it.
    fn incarnation_exited(&mut self, slot: u64, exit: &Exit, now: Instant) -> Actions {
        let mut actions = Actions::new();
        let (owner, was_stopping, policy) = match self.entries.get_mut(&slot) {
            Some(Entry {
                parent: Parent::Entry(owner),
                kind: Kind::Worker(w),
                ..
            }) => {
                w.current = None;
                (*owner, std::mem::take(&mut w.stopping), w.policy)
            }
            // Workers always live under an entry; a slot retired before its
            // incarnation's death was observed has nothing left to decide.
            Some(Entry {
                parent: Parent::Process(_),
                ..
            })
            | Some(Entry {
                kind: Kind::Supervisor(_) | Kind::Factory(_),
                ..
            })
            | None => return actions,
        };
        // The watches come off the slot for the duration, so that if this
        // exit retires it, `free_entry` does not also announce a removal:
        // the watchers hear one event, with the real reason and the final
        // status.
        let watches = self
            .entries
            .get_mut(&slot)
            .map(|e| std::mem::take(&mut e.watches))
            .unwrap_or_default();
        if was_stopping {
            self.child_stopped(owner, slot, &mut actions);
        } else if policy.restarts_after(exit) {
            self.request_restart(owner, slot, &exit.to_string(), now, &mut actions);
        } else {
            self.retire(slot, &mut actions);
        }
        if !watches.is_empty() {
            let after = match self.entries.get_mut(&slot) {
                None => After::Gone,
                Some(_)
                    if actions
                        .iter()
                        .any(|a| matches!(a, Action::Start(s) if *s == slot)) =>
                {
                    After::Running
                }
                Some(_) => After::Restarting,
            };
            for w in &watches {
                actions.push(Action::Notify {
                    closure: ProcHeap::spawn(&w.closure).1,
                    entry: slot,
                    ended: Ended::Exited(exit.clone()),
                    after,
                });
            }
            match self.entries.get_mut(&slot) {
                Some(e) => e.watches = watches,
                None => self.release_watches(slot, watches),
            }
        }
        actions
    }

    /// A process the tree was interested in ended. If it failed, everything
    /// it declared goes, in order; a normal return leaves that standing.
    fn process_exited(&mut self, pid: u64, failed: bool) -> Actions {
        let mut actions = Actions::new();
        for (entry, id) in self.holders.remove(&pid).unwrap_or_default() {
            if let Some(e) = self.entries.get_mut(&entry)
                && let Some(i) = e.watches.iter().position(|w| w.id == id)
            {
                let removed = e.watches.swap_remove(i);
                self.garbage.push(removed.closure);
            }
        }
        if failed {
            for id in self.owned.remove(&pid).unwrap_or_default() {
                self.begin(id, Mode::Retire, Then::Done, &mut actions);
            }
        }
        actions
    }

    /// Free everything `pid` declared, whether or not it is still running:
    /// what a give-up that found nobody to kill does with the leftovers.
    fn retire_owned(&mut self, pid: u64) -> Actions {
        let mut actions = Actions::new();
        for id in self.owned.remove(&pid).unwrap_or_default() {
            self.begin(id, Mode::Retire, Then::Done, &mut actions);
        }
        actions
    }

    /// `child` (a worker slot, or a nested supervisor/factory that gave up)
    /// under `owner` needs restarting. The owner's budget and strategy
    /// decide what happens; the decision is taken under a short borrow and
    /// acted on afterwards.
    fn request_restart(
        &mut self,
        owner: u64,
        child: u64,
        why: &str,
        now: Instant,
        actions: &mut Actions,
    ) {
        let decision = match self.entries.get_mut(&owner).map(|e| &mut e.kind) {
            Some(Kind::Supervisor(s)) => {
                if let Some(op) = &s.in_flight {
                    if !matches!(&op.then, Then::Restart(list) if list.contains(&child)) {
                        s.deferred.push(child);
                    }
                    Decision::Deferred
                } else if !s.budget.charge(now) {
                    Decision::GiveUp {
                        report: format!(
                            "supervisor gave up: more than {} restarts in {}ms (last: {why})",
                            s.budget.restarts,
                            s.budget.within.as_millis()
                        ),
                        restarts: s.budget.restarts,
                        within_ms: s.budget.within.as_millis() as u64,
                    }
                } else {
                    let children = &s.children;
                    let position = children.iter().position(|&c| c == child);
                    match (s.strategy, position) {
                        (Strategy::OneForOne, _) | (_, None) => Decision::Start(vec![child]),
                        (Strategy::OneForAll, Some(_)) => Decision::StopThenStart {
                            victims: children.iter().copied().filter(|&c| c != child).collect(),
                            restart: children.clone(),
                        },
                        (Strategy::RestForOne, Some(i)) => Decision::StopThenStart {
                            victims: children[i + 1..].to_vec(),
                            restart: children[i..].to_vec(),
                        },
                    }
                }
            }
            Some(Kind::Factory(f)) => match f.restart.as_mut() {
                None => Decision::Retire,
                Some(_) if f.in_flight.is_some() => {
                    f.deferred.push(child);
                    Decision::Deferred
                }
                Some(budget) => {
                    if budget.charge(now) {
                        Decision::Start(vec![child])
                    } else {
                        Decision::GiveUp {
                            report: format!(
                                "factory gave up: more than {} member restarts in {}ms (last: {why})",
                                budget.restarts,
                                budget.within.as_millis()
                            ),
                            restarts: budget.restarts,
                            within_ms: budget.within.as_millis() as u64,
                        }
                    }
                }
            },
            Some(Kind::Worker(_)) | None => Decision::Retire,
        };
        match decision {
            Decision::Deferred => {}
            Decision::Retire => self.retire(child, actions),
            Decision::Start(list) => self.start_all(&list, actions),
            Decision::GiveUp {
                report,
                restarts,
                within_ms,
            } => {
                actions.push(Action::Report(report));
                self.notify(
                    owner,
                    || Ended::GaveUp {
                        restarts,
                        within_ms,
                    },
                    After::Restarting,
                    actions,
                );
                self.begin(owner, Mode::Stop, Then::Escalate, actions);
            }
            Decision::StopThenStart { victims, restart } => {
                if let Some(s) = self.supervisor_mut(owner) {
                    // Victims are stopped in reverse declaration order:
                    // `pending` is taken from the back.
                    s.in_flight = Some(InFlight {
                        mode: Mode::Stop,
                        pending: victims,
                        awaiting: HashSet::default(),
                        then: Then::Restart(restart),
                    });
                }
                self.advance(owner, actions);
            }
        }
    }

    /// Start an operation that stops everything under `owner`. Completes
    /// synchronously — running `then` before returning — when there is
    /// nothing to wait for.
    fn begin(&mut self, owner: u64, mode: Mode, then: Then, actions: &mut Actions) {
        let Some(entry) = self.entries.get_mut(&owner) else {
            if let Then::Notify(parent) = then {
                let child = owner;
                self.child_stopped(parent, child, actions);
            }
            return;
        };
        match &mut entry.kind {
            Kind::Supervisor(s) => {
                if let Some(op) = s.in_flight.as_mut() {
                    // A retire supersedes whatever was happening; a stop
                    // while stopping only replaces the continuation. Entries
                    // already stopped stay stopped, so restarting the walk
                    // over all children is correct: stopped ones pass through.
                    op.mode = match (op.mode, mode) {
                        (Mode::Retire, _) | (_, Mode::Retire) => Mode::Retire,
                        (Mode::Stop, Mode::Stop) => Mode::Stop,
                    };
                    op.then = then;
                    op.pending = s.children.clone();
                    return;
                }
                s.in_flight = Some(InFlight {
                    mode,
                    pending: s.children.clone(),
                    awaiting: HashSet::default(),
                    then,
                });
            }
            Kind::Factory(f) => {
                if let Some(op) = f.in_flight.as_mut() {
                    op.mode = match (op.mode, mode) {
                        (Mode::Retire, _) | (_, Mode::Retire) => Mode::Retire,
                        (Mode::Stop, Mode::Stop) => Mode::Stop,
                    };
                    op.then = then;
                    return;
                }
                let members: Vec<u64> = f.members.iter().copied().collect();
                f.in_flight = Some(InFlight {
                    mode,
                    pending: members,
                    awaiting: HashSet::default(),
                    then,
                });
            }
            // Workers are stopped by their owner's operation
            // (`request_stop`), never begun on their own.
            Kind::Worker(_) => return,
        }
        self.advance(owner, actions);
    }

    /// Move `owner`'s operation along: request the next stop(s), and when
    /// nothing is left to wait for, complete it.
    fn advance(&mut self, owner: u64, actions: &mut Actions) {
        loop {
            let (mode, next): (Mode, Vec<u64>) = {
                let Some(entry) = self.entries.get_mut(&owner) else {
                    return;
                };
                let (op, concurrent) = match &mut entry.kind {
                    Kind::Supervisor(s) => (s.in_flight.as_mut(), false),
                    Kind::Factory(f) => (f.in_flight.as_mut(), true),
                    Kind::Worker(_) => (None, false),
                };
                let Some(op) = op else {
                    return;
                };
                if !op.awaiting.is_empty() {
                    return;
                }
                if op.pending.is_empty() {
                    break;
                }
                let next = if concurrent {
                    std::mem::take(&mut op.pending)
                } else {
                    op.pending.pop().into_iter().collect()
                };
                for &n in &next {
                    op.awaiting.insert(n);
                }
                (op.mode, next)
            };
            for child in next {
                self.request_stop(owner, child, mode, actions);
            }
            // `request_stop` may have observed synchronous completions and
            // emptied `awaiting` again; loop to take the next one.
        }
        self.complete(owner, actions);
    }

    /// Ask one child of `owner` to stop. A child with nothing running is
    /// stopped at once; otherwise the owner waits for its death.
    fn request_stop(&mut self, owner: u64, child: u64, mode: Mode, actions: &mut Actions) {
        let Some(entry) = self.entries.get_mut(&child) else {
            self.child_stopped(owner, child, actions);
            return;
        };
        match &mut entry.kind {
            Kind::Worker(w) => match w.current {
                Some(pid) => {
                    w.stopping = true;
                    match &w.stopper {
                        Some(stopper) => {
                            let (_heap, copy) = ProcHeap::spawn(stopper);
                            actions.push(Action::FireStopper {
                                stopper: copy,
                                address: w.address,
                                pid,
                            });
                        }
                        None => actions.push(Action::Kill(pid)),
                    }
                }
                None => self.child_stopped(owner, child, actions),
            },
            Kind::Supervisor(_) | Kind::Factory(_) => {
                self.begin(child, mode, Then::Notify(owner), actions);
            }
        }
    }

    /// `child`, which `owner`'s operation was waiting on, has stopped.
    fn child_stopped(&mut self, owner: u64, child: u64, actions: &mut Actions) {
        let mode = {
            let Some(entry) = self.entries.get_mut(&owner) else {
                // The owner is gone (retired while this was in flight); the
                // child has nothing to belong to.
                self.retire(child, actions);
                return;
            };
            let op = match &mut entry.kind {
                Kind::Supervisor(s) => s.in_flight.as_mut(),
                Kind::Factory(f) => f.in_flight.as_mut(),
                Kind::Worker(_) => None,
            };
            let Some(op) = op else {
                // Not waiting for anything: a worker whose stop was requested
                // by an operation that has since been superseded. Its slot
                // simply sits empty; a deferred restart may pick it up.
                return;
            };
            op.awaiting.remove(&child);
            op.mode
        };
        // A stopped disposable factory member has no way back, whatever the
        // mode; everything else is freed only when retiring.
        let disposable = matches!(
            self.entries[&owner].kind,
            Kind::Factory(Factory { restart: None, .. })
        );
        if mode == Mode::Retire || disposable {
            self.retire(child, actions);
        }
        self.advance(owner, actions);
    }

    /// `owner`'s operation has nothing left to wait for.
    fn complete(&mut self, owner: u64, actions: &mut Actions) {
        let (mode, then, deferred) = {
            let Some(entry) = self.entries.get_mut(&owner) else {
                return;
            };
            let (op, deferred) = match &mut entry.kind {
                Kind::Supervisor(s) => (s.in_flight.take(), std::mem::take(&mut s.deferred)),
                Kind::Factory(f) => (f.in_flight.take(), std::mem::take(&mut f.deferred)),
                Kind::Worker(_) => (None, Vec::new()),
            };
            let Some(op) = op else {
                return;
            };
            (op.mode, op.then, deferred)
        };
        match then {
            Then::Restart(list) => {
                self.start_all(&list, actions);
                // Deaths that arrived mid-operation of slots not covered by
                // the restart are ordinary restart requests now.
                let now = Instant::now();
                for child in deferred {
                    let still_down = match self.entry(child).map(|e| &e.kind) {
                        Some(Kind::Worker(w)) => w.current.is_none(),
                        Some(Kind::Supervisor(_) | Kind::Factory(_)) => true,
                        None => false,
                    };
                    if still_down {
                        self.request_restart(owner, child, "died during a restart", now, actions);
                    }
                }
            }
            Then::Escalate => self.escalate(owner, actions),
            Then::Notify(parent) => {
                if mode == Mode::Retire {
                    self.free_entry(owner, actions);
                }
                let child = owner;
                self.child_stopped(parent, child, actions);
            }
            Then::Done => {
                if mode == Mode::Retire {
                    self.free_entry(owner, actions);
                }
            }
        }
    }

    /// `owner` gave up (its children are stopped and kept). Its own owner
    /// treats that as the death of a permanent child.
    fn escalate(&mut self, owner: u64, actions: &mut Actions) {
        let parent = self.entries[&owner].parent;
        match parent {
            Parent::Entry(p) => {
                let now = Instant::now();
                let child = owner;
                self.request_restart(p, child, "a child supervisor gave up", now, actions);
            }
            Parent::Process(pid) => {
                // Nothing above can restart it: the failure has reached a
                // plain process, which is killed as a linked child's failure
                // would kill it. Its entries are freed by its death — or,
                // if it had already returned, right away (`FailOwner`).
                actions.push(Action::FailOwner(pid));
            }
        }
    }

    /// Emit starts for `entries` in order; a supervisor or factory entry
    /// starts everything in it, recursively, in its own order.
    fn start_all(&mut self, entries: &[u64], actions: &mut Actions) {
        for &id in entries {
            let Some(entry) = self.entry(id) else {
                continue;
            };
            match &entry.kind {
                Kind::Worker(w) => {
                    if w.current.is_none() {
                        actions.push(Action::Start(id));
                    }
                }
                Kind::Supervisor(s) => {
                    let children = s.children.clone();
                    self.start_all(&children, actions);
                }
                Kind::Factory(f) => {
                    let members: Vec<u64> = f.members.iter().copied().collect();
                    self.start_all(&members, actions);
                }
            }
        }
    }

    /// Take a worker slot out of the tree for good: its address closes, its
    /// owner forgets it, its recipe is freed. Non-worker entries are retired
    /// through `begin(.., Mode::Retire, ..)`, which frees them once empty.
    fn retire(&mut self, id: u64, actions: &mut Actions) {
        let Some(entry) = self.entries.get(&id) else {
            return;
        };
        match &entry.kind {
            Kind::Worker(w) => {
                actions.push(Action::CloseAddress(w.address));
                let parent = entry.parent;
                if let Parent::Entry(p) = parent {
                    self.forget_child(p, id);
                }
                self.free_entry(id, actions);
            }
            Kind::Supervisor(_) | Kind::Factory(_) => {
                self.begin(id, Mode::Retire, Then::Done, actions);
            }
        }
    }

    fn forget_child(&mut self, owner: u64, child: u64) {
        let key_hash = match self.entries.get(&child).map(|e| &e.kind) {
            Some(Kind::Worker(w)) => w.key_hash,
            Some(Kind::Supervisor(_) | Kind::Factory(_)) | None => None,
        };
        match self.entries.get_mut(&owner).map(|e| &mut e.kind) {
            Some(Kind::Supervisor(s)) => {
                s.children.retain(|&c| c != child);
                s.deferred.retain(|&c| c != child);
                if let Some(op) = s.in_flight.as_mut() {
                    op.pending.retain(|&c| c != child);
                    if let Then::Restart(list) = &mut op.then {
                        list.retain(|&c| c != child);
                    }
                }
            }
            Some(Kind::Factory(f)) => {
                f.members.remove(&child);
                f.deferred.retain(|&c| c != child);
                if let Some(op) = f.in_flight.as_mut() {
                    op.pending.retain(|&c| c != child);
                }
                let freed_key = key_hash.and_then(|h| lock(&f.keys).remove_slot(h, child));
                self.garbage.extend(freed_key);
            }
            Some(Kind::Worker(_)) | None => {}
        }
    }

    /// Remove an entry whose contents are already gone, freeing what it
    /// retained. Its owner's reference is removed too.
    fn free_entry(&mut self, id: u64, actions: &mut Actions) {
        let Some(entry) = self.entries.remove(&id) else {
            return;
        };
        for w in &entry.watches {
            actions.push(Action::Notify {
                closure: ProcHeap::spawn(&w.closure).1,
                entry: id,
                ended: Ended::Removed,
                after: After::Gone,
            });
        }
        self.release_watches(id, entry.watches);
        match entry.parent {
            Parent::Process(pid) => {
                if let Some(list) = self.owned.get_mut(&pid) {
                    list.retain(|&c| c != id);
                    if list.is_empty() {
                        self.owned.remove(&pid);
                    }
                }
            }
            Parent::Entry(p) => {
                // `forget_child` needs the entry to compute a member's key
                // hash, so workers go through `retire`, which calls it before
                // this; here only non-workers remain to be forgotten.
                if let Some(Kind::Supervisor(s)) = self.entries.get_mut(&p).map(|e| &mut e.kind) {
                    s.children.retain(|&c| c != id);
                }
            }
        }
        match entry.kind {
            Kind::Worker(w) => {
                self.garbage.push(w.recipe);
                self.garbage.extend(w.stopper);
            }
            Kind::Factory(f) => {
                self.garbage.push(f.template);
                self.retired_factories.push(id);
                // The index's remaining reservations (a member declared
                // while the factory was going) are freed with it, after the
                // lock, by whoever unpublishes it.
                drop(f.keys);
            }
            Kind::Supervisor(_) => {}
        }
    }

    // ---- watches -----------------------------------------------------------------

    /// Place a watch on `entry` for `holder`. `Err` hands the closure back
    /// when there is nothing there, for the caller to fire at once.
    fn watch(&mut self, entry: u64, holder: u64, closure: Value) -> Result<u64, Value> {
        let Some(e) = self.entries.get_mut(&entry) else {
            return Err(closure);
        };
        self.next_watch += 1;
        let id = self.next_watch;
        e.watches.push(Watch {
            id,
            holder,
            closure,
        });
        self.holders.entry(holder).or_default().push((entry, id));
        Ok(id)
    }

    /// Cancel a watch. A notification already produced may still arrive,
    /// as with a demonitored monitor; an unknown watch is a no-op.
    fn unwatch(&mut self, entry: u64, id: u64) {
        let Some(e) = self.entries.get_mut(&entry) else {
            return;
        };
        let Some(i) = e.watches.iter().position(|w| w.id == id) else {
            return;
        };
        let removed = e.watches.swap_remove(i);
        self.forget_holding(removed.holder, entry, id);
        self.garbage.push(removed.closure);
    }

    /// Announce something about `entry` to its watchers, leaving them in
    /// place: what a give-up is.
    fn notify(
        &mut self,
        entry: u64,
        ended: impl Fn() -> Ended,
        after: After,
        actions: &mut Actions,
    ) {
        let Some(e) = self.entries.get(&entry) else {
            return;
        };
        for w in &e.watches {
            actions.push(Action::Notify {
                closure: ProcHeap::spawn(&w.closure).1,
                entry,
                ended: ended(),
                after,
            });
        }
    }

    /// Drop watches whose entry is gone: unindex them and free their
    /// closures.
    fn release_watches(&mut self, entry: u64, watches: Vec<Watch>) {
        for w in watches {
            self.forget_holding(w.holder, entry, w.id);
            self.garbage.push(w.closure);
        }
    }

    fn forget_holding(&mut self, holder: u64, entry: u64, id: u64) {
        if let Some(list) = self.holders.get_mut(&holder) {
            list.retain(|&(e, i)| (e, i) != (entry, id));
            if list.is_empty() {
                self.holders.remove(&holder);
            }
        }
    }

    // ---- introspection -----------------------------------------------------------

    fn info(&self, id: u64) -> Option<Info> {
        let entry = self.entry(id)?;
        Some(match &entry.kind {
            Kind::Worker(w) => Info {
                kind: 1,
                detail: w.policy.code(),
                restarts: 0,
                within_ms: 0,
                status: if w.current.is_some() { 0 } else { 1 },
                restarted: w.incarnations.saturating_sub(1) as i64,
                pid: w.current,
            },
            Kind::Supervisor(s) => Info {
                kind: 2,
                detail: s.strategy.code(),
                restarts: s.budget.restarts as i64,
                within_ms: s.budget.within.as_millis() as i64,
                status: 0,
                restarted: 0,
                pid: None,
            },
            Kind::Factory(f) => Info {
                kind: 3,
                detail: f.restart.is_some() as i64,
                restarts: f.restart.as_ref().map_or(0, |b| b.restarts as i64),
                within_ms: f
                    .restart
                    .as_ref()
                    .map_or(0, |b| b.within.as_millis() as i64),
                status: 0,
                restarted: 0,
                pid: None,
            },
        })
    }

    fn children_of(&self, id: u64) -> Vec<u64> {
        match self.entry(id).map(|e| &e.kind) {
            Some(Kind::Supervisor(s)) => s.children.clone(),
            Some(Kind::Factory(f)) => {
                let mut members: Vec<u64> = f.members.iter().copied().collect();
                members.sort_unstable();
                members
            }
            Some(Kind::Worker(_)) => Vec::new(),
            None => self.owned.get(&id).cloned().unwrap_or_default(),
        }
    }

    fn count_of(&self, id: u64) -> usize {
        match self.entry(id).map(|e| &e.kind) {
            Some(Kind::Supervisor(s)) => s.children.len(),
            Some(Kind::Factory(f)) => f.members.len(),
            Some(Kind::Worker(_)) => 0,
            None => self.owned.get(&id).map_or(0, Vec::len),
        }
    }

    fn parent_of(&self, id: u64) -> Option<Parent> {
        self.entry(id).map(|e| e.parent)
    }
}

impl Runtime {
    /// Run `f` under the tree lock, then free what it retired — recipes,
    /// keys, factory indexes — outside it, where a large graph costs nobody
    /// else anything. Every use of the tree goes through here.
    fn with_tree<T>(&self, f: impl FnOnce(&mut Tree) -> T) -> T {
        let (result, garbage, retired) = {
            let mut tree = lock(&self.supervision);
            let result = f(&mut tree);
            (
                result,
                std::mem::take(&mut tree.garbage),
                std::mem::take(&mut tree.retired_factories),
            )
        };
        if !retired.is_empty() {
            let mut published = write(&self.factory_keys);
            for id in retired {
                drop(published.remove(&id));
            }
        }
        drop(garbage);
        result
    }

    /// The incarnation of `slot` — process `pid` — ended with `exit`.
    pub(super) fn slot_incarnation_exited(&self, slot: u64, exit: &Exit) -> Actions {
        let now = Instant::now();
        self.with_tree(|tree| tree.incarnation_exited(slot, exit, now))
    }

    /// Process `pid`, which the tree had asked to hear about, ended.
    pub(super) fn tree_process_exited(&self, pid: u64, failed: bool) -> Actions {
        self.with_tree(|tree| tree.process_exited(pid, failed))
    }

    /// A factory's key index, if the factory is live.
    fn keys_of(&self, factory: u64) -> Option<Keys> {
        read(&self.factory_keys).get(&factory).cloned()
    }
}

fn read<T>(l: &RwLock<T>) -> std::sync::RwLockReadGuard<'_, T> {
    l.read().unwrap_or_else(|e| e.into_inner())
}

fn write<T>(l: &RwLock<T>) -> std::sync::RwLockWriteGuard<'_, T> {
    l.write().unwrap_or_else(|e| e.into_inner())
}

/// What [`VM::mint_slot`] makes for a worker about to be declared.
struct MintedSlot {
    slot: u64,
    address: u64,
}

impl VM {
    /// Carry out what the tree asked for. Starts happen here, before the
    /// caller counts the dead process as finished, so a program consisting
    /// of one supervised worker cannot be observed as over between the
    /// worker's death and its restart.
    pub(super) fn run_supervision_actions(&mut self, actions: Actions) -> VmResult<()> {
        for action in actions {
            match action {
                Action::Kill(pid) => {
                    self.runtime.kill(pid);
                }
                Action::FailOwner(pid) => {
                    if !self.runtime.kill(pid) {
                        self.runtime.note_unsupervised_failure();
                        let leftovers = self.runtime.with_tree(|tree| tree.retire_owned(pid));
                        self.run_supervision_actions(leftovers)?;
                    }
                }
                Action::FireStopper {
                    stopper,
                    address,
                    pid,
                } => {
                    self.runtime.process_started();
                    self.spawn_process_with_heap_args(
                        ProcHeap::new(),
                        stopper,
                        &[Value::subject(address), Value::pid(pid)],
                    );
                }
                Action::Start(slot) => self.incarnate(slot)?,
                Action::CloseAddress(id) => self.runtime.subject_close(id),
                Action::Report(why) => eprintln!("{why}"),
                Action::Notify {
                    closure,
                    entry,
                    ended,
                    after,
                } => self.fire_watch(closure, entry, &ended, after)?,
            }
        }
        Ok(())
    }

    /// Bring a slot to life: copy its recipe, mint the incarnation's pid,
    /// point the address at it, and queue it — here, or on the scheduler it
    /// is pinned to. One critical section, so a slot retired concurrently is
    /// simply found gone and nothing starts.
    fn incarnate(&mut self, slot: u64) -> VmResult<()> {
        let here = self.scheduler_index;
        let runtime = Arc::clone(&self.runtime);
        let started = runtime.with_tree(|tree| {
            let (owner, address) = match tree.entry(slot) {
                Some(Entry {
                    parent: Parent::Entry(owner),
                    kind: Kind::Worker(w),
                    ..
                }) => (*owner, w.address),
                Some(Entry {
                    parent: Parent::Process(_),
                    ..
                })
                | Some(Entry {
                    kind: Kind::Supervisor(_) | Kind::Factory(_),
                    ..
                })
                | None => return None,
            };
            let template = match tree.entry(owner).map(|e| &e.kind) {
                Some(Kind::Factory(f)) => Some(ProcHeap::spawn(&f.template).1),
                Some(Kind::Supervisor(_)) => None,
                Some(Kind::Worker(_)) | None => return None,
            };
            let w = tree.worker_mut(slot)?;
            if w.current.is_some() {
                return None;
            }
            let (_heap, recipe) = ProcHeap::spawn(&w.recipe);
            let target = match w.pin {
                Some(i) if runtime.is_live_scheduler(i as usize) => i as usize,
                Some(_) | None => here,
            };
            let pid = runtime.alloc_incarnation(target, slot);
            w.current = Some(pid);
            w.incarnations = w.incarnations.saturating_add(1);
            Some((pid, address, template, recipe, target))
        });
        let Some((pid, address, template, recipe, target)) = started else {
            return Ok(());
        };
        self.runtime.subject_rehome(address, pid);
        // A factory member runs template(inbox, key); a supervisor's worker
        // runs start(inbox).
        let (closure, args) = match template {
            Some(template) => (template, vec![Value::subject(address), recipe]),
            None => (recipe, vec![Value::subject(address)]),
        };
        if target == here {
            self.runtime.process_started();
            self.start_process(pid, ProcHeap::new(), closure, &args);
        } else {
            // The graphs were copied on this thread and are handed over
            // whole; `submit_to` counts the process as started.
            self.runtime.submit_to(
                target,
                Seed {
                    pid,
                    root: closure,
                    heap: ProcHeap::new(),
                    args,
                },
            );
        }
        Ok(())
    }

    /// Declare a worker slot under `owner` and start it. The address is
    /// minted by the caller (a keyed member's is already reserved in the
    /// factory's index by the time the tree hears of it). The creator rule
    /// applies to supervisors' workers, not to members, which any process
    /// may make.
    fn declare_worker(
        &mut self,
        owner: u64,
        into_factory: bool,
        slot: u64,
        worker: Worker,
    ) -> VmResult<()> {
        let creator = self.current_pid;
        let address = worker.address;
        let outcome = self.runtime.with_tree(|tree| {
            let check = if into_factory {
                tree.factory_exists(owner)
            } else {
                tree.owner_for_declaration(owner, creator, false)
            };
            match check {
                Ok(()) => {
                    tree.add_worker(slot, owner, creator, worker);
                    Ok(())
                }
                Err(r) => {
                    tree.garbage.push(worker.recipe);
                    tree.garbage.extend(worker.stopper);
                    Err(r)
                }
            }
        });
        match outcome {
            Ok(()) => self.incarnate(slot),
            Err(r) => {
                self.runtime.subject_close(address);
                Err(refused(r))
            }
        }
    }

    /// Start a watch's closure as a process, applied to the raw description
    /// `(kind, reason, restarts, within_ms, status, entry)` that the stdlib
    /// turns into its `Exit`: kind 0 is an exit with `reason` (`NoProcess` for a
    /// watch placed on nothing, `Killed` for an entry removed from under its
    /// watchers), kind 1 a give-up, whose reason field is a placeholder.
    fn fire_watch(
        &mut self,
        closure: Value,
        entry: u64,
        ended: &Ended,
        after: After,
    ) -> VmResult<()> {
        let (kind, reason, restarts, within_ms) = match ended {
            Ended::Exited(exit) => (0, self.exit_reason(exit)?, 0, 0),
            Ended::Removed => (0, self.abi_nullary(AbiSlot::ExitKilled)?, 0, 0),
            Ended::AlreadyGone => (0, self.abi_nullary(AbiSlot::ExitNoProcess)?, 0, 0),
            Ended::GaveUp {
                restarts,
                within_ms,
            } => (
                1,
                self.abi_nullary(AbiSlot::ExitKilled)?,
                *restarts as i64,
                *within_ms as i64,
            ),
        };
        let fields = [
            Value::small_int(kind),
            reason,
            Value::int_in(&mut self.heap, restarts),
            Value::int_in(&mut self.heap, within_ms),
            Value::small_int(after.code()),
            Value::small_int(entry as i64),
        ];
        let description = Value::tuple_in(&mut self.heap, &fields);
        self.runtime.process_started();
        self.spawn_process_with_heap_args(ProcHeap::new(), closure, &[description]);
        Ok(())
    }

    /// A slot id and a durable address for a worker about to be declared.
    fn mint_slot(&self) -> MintedSlot {
        let slot = self.runtime.alloc_entry_id();
        let address = self.runtime.subject_create_durable(0, slot);
        MintedSlot { slot, address }
    }

    // ---- ops ---------------------------------------------------------------------

    /// `Op::SupervisorNew`: `[strategy_code, restarts, within_ms, parent] ->
    /// Int` — a supervisor entry. `parent` is a supervisor's id to nest
    /// under, or a non-Int (Nil) for a top-level supervisor owned by the
    /// calling process.
    pub(super) fn supervisor_new(&mut self) -> VmResult<()> {
        let parent_v = self.pop()?;
        let within = self.pop_int("process.supervisor")?;
        let restarts = self.pop_int("process.supervisor")?;
        let code = self.pop_int("process.supervisor")?;
        let strategy = Strategy::decode(code).ok_or_else(|| refused(Refusal::BadCode))?;
        let creator = self.current_pid;
        let id = self.runtime.alloc_entry_id();
        let parent = match parent_v.as_int() {
            Some(p) => Parent::Entry(p as u64),
            None => Parent::Process(creator),
        };
        if let Parent::Process(_) = parent {
            self.runtime.note_in_tree(creator);
        }
        self.runtime.with_tree(|tree| {
            if let Parent::Entry(p) = parent {
                tree.owner_for_declaration(p, creator, false)
                    .map_err(refused)?;
            }
            tree.add_supervisor(id, parent, creator, strategy, Budget::new(restarts, within));
            Ok::<(), VmError>(())
        })?;
        self.stack.push(Value::small_int(id as i64));
        Ok(())
    }

    /// `Op::SupervisorWorker`: `[supervisor, policy_code, stopper, start] ->
    /// Subject`. `stopper` is a two-argument closure or anything else for
    /// "just kill it". Declares the slot and starts its first incarnation.
    /// Charged like a spawn: two graph copies.
    pub(super) fn supervisor_worker(&mut self, reds: &mut i32) -> VmResult<()> {
        *reds -= 2 * IO_REDUCTION_COST;
        let start = self.pop()?;
        let stopper_v = self.pop()?;
        let code = self.pop_int("process.worker")?;
        let owner = self.pop_int("process.worker")? as u64;
        let policy = Policy::decode(code).ok_or_else(|| refused(Refusal::BadCode))?;
        self.check_arity(&start, 1, "a worker's start function takes its inbox")?;
        let stopper = if stopper_v.as_closure().is_some() {
            self.check_arity(&stopper_v, 2, "a stopper takes the address and the pid")?;
            Some(ProcHeap::spawn(&stopper_v).1)
        } else {
            None
        };
        let (_heap, recipe) = ProcHeap::spawn(&start);
        let MintedSlot { slot, address } = self.mint_slot();
        self.declare_worker(
            owner,
            false,
            slot,
            Worker::new(policy, address, recipe, stopper, None, None),
        )?;
        self.stack.push(Value::subject(address));
        Ok(())
    }

    /// `Op::SupervisorWorkerOnEach`: `[supervisor, policy_code, start] ->
    /// Nil` — one slot per scheduler, each pinned there. The addresses are
    /// not returned: these workers are driven by what they capture.
    pub(super) fn supervisor_worker_on_each(&mut self, reds: &mut i32) -> VmResult<()> {
        let start = self.pop()?;
        let code = self.pop_int("process.worker_per_scheduler")?;
        let owner = self.pop_int("process.worker_per_scheduler")? as u64;
        let policy = Policy::decode(code).ok_or_else(|| refused(Refusal::BadCode))?;
        self.check_arity(&start, 1, "a worker's start function takes its inbox")?;
        // Every scheduler must exist before anything is pinned to it.
        self.runtime.ensure_workers();
        for i in 0..self.runtime.scheduler_count() {
            *reds -= 2 * IO_REDUCTION_COST;
            let (_heap, recipe) = ProcHeap::spawn(&start);
            let MintedSlot { slot, address } = self.mint_slot();
            self.declare_worker(
                owner,
                false,
                slot,
                Worker::new(policy, address, recipe, None, Some(i as u32), None),
            )?;
        }
        let nil = self.make_nil()?;
        self.stack.push(nil);
        Ok(())
    }

    /// `Op::FactoryNew`: `[supervisor, restarts, within_ms, template] -> Int`.
    /// `restarts < 0` declares disposable members. The key index is
    /// published before the entry exists, so a lookup can never find the
    /// factory without its index.
    pub(super) fn factory_new(&mut self, reds: &mut i32) -> VmResult<()> {
        *reds -= IO_REDUCTION_COST;
        let template = self.pop()?;
        let within = self.pop_int("process.factory")?;
        let restarts = self.pop_int("process.factory")?;
        let owner = self.pop_int("process.factory")? as u64;
        self.check_arity(
            &template,
            2,
            "a factory template takes the inbox and the key",
        )?;
        let (_heap, template) = ProcHeap::spawn(&template);
        let creator = self.current_pid;
        let id = self.runtime.alloc_entry_id();
        let keys: Keys = Arc::default();
        write(&self.runtime.factory_keys).insert(id, Arc::clone(&keys));
        let declared = self.runtime.with_tree(|tree| {
            tree.owner_for_declaration(owner, creator, false)?;
            let restart = (restarts >= 0).then(|| Budget::new(restarts, within));
            tree.add_factory(id, owner, creator, restart, template, keys);
            Ok(())
        });
        if let Err(r) = declared {
            write(&self.runtime.factory_keys).remove(&id);
            return Err(refused(r));
        }
        self.stack.push(Value::small_int(id as i64));
        Ok(())
    }

    /// `Op::FactoryLookupOrStart`: `[factory, key] -> Subject`. One member per
    /// key: the probe and the reservation happen under the index's lock, so
    /// of several processes asking at once, one declares the member and the
    /// rest get its address. Any process may call this — members are the
    /// dynamic part of the tree — so there is no creator check.
    pub(super) fn factory_lookup_or_start(&mut self, reds: &mut i32) -> VmResult<()> {
        *reds -= IO_REDUCTION_COST;
        let key = self.pop()?;
        let factory = self.pop_int("process.lookup_or_start")? as u64;
        let keys = self
            .runtime
            .keys_of(factory)
            .ok_or_else(|| refused(Refusal::NoSuchFactory))?;
        let hash = hash_value(&key);
        // The common case — the member exists — takes the index lock and
        // nothing else; a miss holds it only long enough to reserve.
        let reserved = {
            let mut index = lock(&keys);
            match index.find(hash, &key) {
                Some(address) => Err(address),
                None => {
                    let MintedSlot { slot, address } = self.mint_slot();
                    let (_heap, stored) = ProcHeap::spawn(&key);
                    index.insert(
                        hash,
                        KeyEntry {
                            key: stored,
                            address,
                            slot,
                        },
                    );
                    Ok((slot, address))
                }
            }
        };
        let (slot, address) = match reserved {
            Err(existing) => {
                self.stack.push(Value::subject(existing));
                return Ok(());
            }
            Ok(fresh) => fresh,
        };
        let policy = self.member_policy(factory)?;
        let (_heap, recipe) = ProcHeap::spawn(&key);
        let declared = self.declare_worker(
            factory,
            true,
            slot,
            Worker::new(policy, address, recipe, None, None, Some(hash)),
        );
        if let Err(e) = declared {
            // The factory went away between the reservation and the
            // declaration; take the reservation back so a later factory of
            // the same id (impossible) or a concurrent looker sees nothing.
            let freed = lock(&keys).remove_slot(hash, slot);
            drop(freed);
            return Err(e);
        }
        self.stack.push(Value::subject(address));
        Ok(())
    }

    /// `Op::FactorySpawn`: `[factory, arg] -> Subject` — an unkeyed member.
    pub(super) fn factory_spawn(&mut self, reds: &mut i32) -> VmResult<()> {
        *reds -= IO_REDUCTION_COST;
        let arg = self.pop()?;
        let factory = self.pop_int("process.start_in")? as u64;
        let policy = self.member_policy(factory)?;
        let (_heap, recipe) = ProcHeap::spawn(&arg);
        let MintedSlot { slot, address } = self.mint_slot();
        self.declare_worker(
            factory,
            true,
            slot,
            Worker::new(policy, address, recipe, None, None, None),
        )?;
        self.stack.push(Value::subject(address));
        Ok(())
    }

    /// The policy a new member of `factory` gets: restartable members are
    /// transient (a member that returns is done), disposable ones temporary.
    fn member_policy(&self, factory: u64) -> VmResult<Policy> {
        self.runtime
            .with_tree(|tree| match tree.entry(factory).map(|e| &e.kind) {
                Some(Kind::Factory(f)) => Ok(if f.restart.is_some() {
                    Policy::Transient
                } else {
                    Policy::Temporary
                }),
                Some(Kind::Supervisor(_) | Kind::Worker(_)) | None => {
                    Err(refused(Refusal::NoSuchFactory))
                }
            })
    }

    /// `Op::FactoryLookup`: `[factory, key] -> Option(Subject)`. A factory
    /// that is gone has no members, which is `None`, not an error.
    pub(super) fn factory_lookup(&mut self) -> VmResult<()> {
        let key = self.pop()?;
        let factory = self.pop_int("process.lookup")? as u64;
        let found = self
            .runtime
            .keys_of(factory)
            .and_then(|keys| lock(&keys).find(hash_value(&key), &key));
        let v = match found {
            Some(address) => self.make_some(Value::subject(address))?,
            None => self.make_none()?,
        };
        self.stack.push(v);
        Ok(())
    }

    /// `Op::WatchNew`: `[entry, notice fn(description) Nil] -> Int` — the
    /// watch id, or a fresh notice started at once if there is nothing at
    /// `entry` (a plain process is watched through `monitor`; the stdlib
    /// routes that before reaching here). Charged like a monitor.
    pub(super) fn watch_new(&mut self, reds: &mut i32) -> VmResult<()> {
        *reds -= IO_REDUCTION_COST;
        let closure = self.pop()?;
        let entry = self.pop_int("process.watch")? as u64;
        self.check_arity(&closure, 1, "a watch's notice function takes one argument")?;
        let (_heap, copy) = ProcHeap::spawn(&closure);
        let holder = self.current_pid;
        self.runtime.note_in_tree(holder);
        let id = match self
            .runtime
            .with_tree(|tree| tree.watch(entry, holder, copy))
        {
            Ok(id) => id,
            Err(copy) => {
                self.fire_watch(copy, entry, &Ended::AlreadyGone, After::Gone)?;
                0
            }
        };
        self.stack.push(Value::small_int(id as i64));
        Ok(())
    }

    /// `Op::WatchCancel`: `[entry, watch id] -> Nil`.
    pub(super) fn watch_cancel(&mut self) -> VmResult<()> {
        let id = self.pop_int("process.unwatch")? as u64;
        let entry = self.pop_int("process.unwatch")? as u64;
        self.runtime.with_tree(|tree| tree.unwatch(entry, id));
        let nil = self.make_nil()?;
        self.stack.push(nil);
        Ok(())
    }

    /// `Op::SupervisedOf`: `[subject] -> Int` — the entry a subject is the
    /// address of, or the process that owns a plain subject; a dead subject
    /// reads as its own id, which every introspection op reports as gone.
    pub(super) fn supervised_of(&mut self) -> VmResult<()> {
        let subj = self.pop()?;
        let Some(id) = subj.as_subject() else {
            return Err(VmError::type_mismatch(
                "process.supervised",
                "Subject",
                &subj,
            ));
        };
        let place = match self.runtime.subject_place(id) {
            Some(SubjectPlace::Worker(w)) => w,
            Some(SubjectPlace::Process(pid)) => pid,
            None => id,
        };
        self.stack.push(Value::small_int(place as i64));
        Ok(())
    }

    /// `Op::SupervisedParent`: `[id] -> Int`. Total: an entry's owner, an
    /// incarnation's slot, a plain process's spawner, and — for anything
    /// with nothing above it, or gone — itself.
    pub(super) fn supervised_parent(&mut self) -> VmResult<()> {
        let id = self.pop_int("process.parent")? as u64;
        let parent = match self.runtime.with_tree(|tree| tree.parent_of(id)) {
            Some(Parent::Entry(p)) | Some(Parent::Process(p)) => p,
            None => self.runtime.process_parent(id).unwrap_or(id),
        };
        self.stack.push(Value::small_int(parent as i64));
        Ok(())
    }

    /// `Op::SupervisedChildren`: `[id] -> Array(Int)`.
    pub(super) fn supervised_children(&mut self) -> VmResult<()> {
        let id = self.pop_int("process.children")? as u64;
        let children = self.runtime.with_tree(|tree| tree.children_of(id));
        let items: Vec<Value> = children
            .into_iter()
            .map(|c| Value::small_int(c as i64))
            .collect();
        let arr = Value::array_in(&mut self.heap, &items);
        self.stack.push(arr);
        Ok(())
    }

    /// `Op::SupervisedCount`: `[id] -> Int`.
    pub(super) fn supervised_count(&mut self) -> VmResult<()> {
        let id = self.pop_int("process.count")? as u64;
        let n = self.runtime.with_tree(|tree| tree.count_of(id));
        self.stack.push(Value::small_int(n as i64));
        Ok(())
    }

    /// `Op::SupervisedInfo`: `[id] -> (kind, detail, restarts, within_ms,
    /// status, restarted, Option(Pid))` — raw fields the stdlib turns into
    /// its `Info`, so the VM constructs none of those types.
    pub(super) fn supervised_info(&mut self) -> VmResult<()> {
        let id = self.pop_int("process.info")? as u64;
        let info = self
            .runtime
            .with_tree(|tree| tree.info(id))
            .unwrap_or_else(|| {
                let alive = self.runtime.process_is_live(id);
                Info {
                    kind: 0,
                    detail: 0,
                    restarts: 0,
                    within_ms: 0,
                    status: if alive { 0 } else { 2 },
                    restarted: 0,
                    pid: alive.then_some(id),
                }
            });
        let pid = match info.pid {
            Some(pid) => self.make_some(Value::pid(pid))?,
            None => self.make_none()?,
        };
        let fields = [
            Value::small_int(info.kind),
            Value::small_int(info.detail),
            Value::int_in(&mut self.heap, info.restarts),
            Value::int_in(&mut self.heap, info.within_ms),
            Value::small_int(info.status),
            Value::int_in(&mut self.heap, info.restarted),
            pid,
        ];
        let tuple = Value::tuple_in(&mut self.heap, &fields);
        self.stack.push(tuple);
        Ok(())
    }

    fn check_arity(&self, f: &Value, arity: i32, what: &'static str) -> VmResult<()> {
        let Some(cl) = f.as_closure() else {
            return Err(VmError::internal(what));
        };
        if self.program.functions[cl.func_idx() as usize].arity != arity {
            return Err(VmError::internal(what));
        }
        Ok(())
    }
}

fn refused(r: Refusal) -> VmError {
    VmError::Crash(Crash::Supervision(r))
}

pub(super) fn new_tree() -> Mutex<Tree> {
    Mutex::new(Tree::default())
}

#[cfg(test)]
mod tests {
    //! The state machine, driven directly: strategies pick the right
    //! victims in the right order, budgets give up, escalation reaches the
    //! owning process, retire frees everything. Program-level behaviour is
    //! `tests/programs/supervisors.scrl` and `tests/vm_supervision.rs`.

    use super::*;

    fn now() -> Instant {
        Instant::now()
    }

    fn worker(address: u64) -> Worker {
        let mut w = Worker::new(
            Policy::Permanent,
            address,
            Value::small_int(0),
            None,
            None,
            None,
        );
        w.incarnations = 1;
        w
    }

    /// A supervisor under process 1 with `n` running workers, ids 10, 11, ..
    fn tree_with(strategy: Strategy, n: u64) -> Tree {
        let mut t = Tree::default();
        t.add_supervisor(2, Parent::Process(1), 1, strategy, Budget::new(3, 5_000));
        for i in 0..n {
            let id = 10 + i;
            let mut w = worker(100 + i);
            w.current = Some(1000 + i);
            t.add_worker(id, 2, 1, w);
        }
        t
    }

    fn starts(actions: &Actions) -> Vec<u64> {
        actions
            .iter()
            .filter_map(|a| match a {
                Action::Start(s) => Some(*s),
                Action::Kill(_)
                | Action::FailOwner(_)
                | Action::FireStopper { .. }
                | Action::CloseAddress(_)
                | Action::Report(_)
                | Action::Notify { .. } => None,
            })
            .collect()
    }

    fn kills(actions: &Actions) -> Vec<u64> {
        actions
            .iter()
            .filter_map(|a| match a {
                Action::Kill(p) => Some(*p),
                Action::Start(_)
                | Action::FailOwner(_)
                | Action::FireStopper { .. }
                | Action::CloseAddress(_)
                | Action::Report(_)
                | Action::Notify { .. } => None,
            })
            .collect()
    }

    #[test]
    fn one_for_one_restarts_only_the_dead_slot() {
        let mut t = tree_with(Strategy::OneForOne, 3);
        let a = t.incarnation_exited(11, &Exit::Killed, now());
        assert_eq!(starts(&a), vec![11]);
        assert!(kills(&a).is_empty());
    }

    #[test]
    fn temporary_and_returning_transient_workers_retire() {
        let mut t = tree_with(Strategy::OneForOne, 2);
        t.worker_mut(10).unwrap().policy = Policy::Temporary;
        t.worker_mut(11).unwrap().policy = Policy::Transient;
        let a = t.incarnation_exited(10, &Exit::Crashed(Crash::ForeignReceive), now());
        assert!(starts(&a).is_empty());
        assert!(matches!(a[..], [Action::CloseAddress(100)]));
        let a = t.incarnation_exited(11, &Exit::Normal, now());
        assert!(matches!(a[..], [Action::CloseAddress(101)]));
        assert!(!t.entries.contains_key(&10) && !t.entries.contains_key(&11));
        assert!(t.supervisor_mut(2).unwrap().children.is_empty());
    }

    #[test]
    fn rest_for_one_stops_later_siblings_in_reverse_then_restarts_in_order() {
        let mut t = tree_with(Strategy::RestForOne, 4);
        // 11 dies: 13 is stopped first, then 12; then 11, 12, 13 start.
        let a = t.incarnation_exited(11, &Exit::Killed, now());
        assert_eq!(kills(&a), vec![1003]);
        assert!(starts(&a).is_empty());
        let a = t.incarnation_exited(13, &Exit::Killed, now());
        assert_eq!(
            kills(&a),
            vec![1002],
            "next victim only after the last is gone"
        );
        let a = t.incarnation_exited(12, &Exit::Killed, now());
        assert_eq!(starts(&a), vec![11, 12, 13]);
        assert!(t.supervisor_mut(2).unwrap().in_flight.is_none());
        // 10 was never touched.
        assert_eq!(t.worker_mut(10).unwrap().current, Some(1000));
    }

    #[test]
    fn one_for_all_stops_everyone_else_and_restarts_all() {
        let mut t = tree_with(Strategy::OneForAll, 3);
        let a = t.incarnation_exited(10, &Exit::Killed, now());
        assert_eq!(kills(&a), vec![1002]);
        let a = t.incarnation_exited(12, &Exit::Killed, now());
        assert_eq!(kills(&a), vec![1001]);
        let a = t.incarnation_exited(11, &Exit::Killed, now());
        assert_eq!(starts(&a), vec![10, 11, 12]);
    }

    #[test]
    fn a_death_during_an_operation_is_folded_in_or_deferred() {
        let mut t = tree_with(Strategy::RestForOne, 3);
        // 11 dies: victim 12; restart set {11, 12}.
        let a = t.incarnation_exited(11, &Exit::Killed, now());
        assert_eq!(kills(&a), vec![1002]);
        // 10 (outside the restart set) dies spontaneously meanwhile: deferred.
        let a = t.incarnation_exited(10, &Exit::Killed, now());
        assert!(a.is_empty());
        // 12's death completes the operation; 11 and 12 start, then the
        // deferred 10 is handled as its own event (rest-for-one from 10 stops
        // 11 and 12 again — they have no incarnation yet, so it starts all).
        let a = t.incarnation_exited(12, &Exit::Killed, now());
        assert_eq!(starts(&a), vec![11, 12, 10, 11, 12]);
    }

    #[test]
    fn budget_exhaustion_stops_everything_and_kills_the_owning_process() {
        let mut t = tree_with(Strategy::OneForOne, 2);
        let t0 = now();
        for _ in 0..3 {
            let a = t.incarnation_exited(10, &Exit::Killed, t0);
            assert_eq!(starts(&a), vec![10]);
            t.worker_mut(10).unwrap().current = Some(1000);
        }
        // Fourth inside the window: give up. 11 (the last child) is stopped
        // first; 10 has just died, so it needs nothing.
        let a = t.incarnation_exited(10, &Exit::Killed, t0);
        assert!(matches!(a[0], Action::Report(_)));
        assert_eq!(kills(&a), vec![1001]);
        assert!(starts(&a).is_empty());
        let a = t.incarnation_exited(11, &Exit::Killed, t0);
        assert!(
            matches!(a[..], [Action::FailOwner(1)]),
            "escalation reaches the owning process"
        );
        // The entries are kept (stopped), awaiting the process's death.
        assert!(t.entries.contains_key(&2) && t.entries.contains_key(&11));
        let a = t.process_exited(1, true);
        let mut closed: Vec<u64> = a
            .iter()
            .filter_map(|x| match x {
                Action::CloseAddress(id) => Some(*id),
                Action::Kill(_)
                | Action::FailOwner(_)
                | Action::FireStopper { .. }
                | Action::Start(_)
                | Action::Report(_)
                | Action::Notify { .. } => None,
            })
            .collect();
        closed.sort_unstable();
        assert_eq!(closed, vec![100, 101]);
        assert!(t.entries.is_empty() && t.owned.is_empty());
    }

    #[test]
    fn budget_window_expires() {
        let mut b = Budget::new(1, 1_000);
        let t0 = now();
        assert!(b.charge(t0));
        assert!(!b.charge(t0));
        assert!(b.charge(t0 + Duration::from_millis(1_500)));
    }

    #[test]
    fn a_nested_supervisor_giving_up_is_restarted_by_its_parent() {
        let mut t = Tree::default();
        t.add_supervisor(
            2,
            Parent::Process(1),
            1,
            Strategy::OneForOne,
            Budget::new(3, 5_000),
        );
        t.add_supervisor(
            3,
            Parent::Entry(2),
            1,
            Strategy::OneForOne,
            Budget::new(0, 5_000),
        );
        let mut w = worker(100);
        w.current = Some(1000);
        t.add_worker(10, 3, 1, w);
        // Budget of zero: the first death is exhaustion. Nothing else to
        // stop, so it escalates at once; 2 restarts 3, i.e. starts 10.
        let a = t.incarnation_exited(10, &Exit::Killed, now());
        assert!(matches!(a[0], Action::Report(_)));
        assert_eq!(starts(&a), vec![10]);
        assert_eq!(t.supervisor_mut(2).unwrap().budget.events.len(), 1);
    }

    #[test]
    fn retiring_an_owner_stops_children_in_reverse_and_frees_when_empty() {
        let mut t = tree_with(Strategy::OneForOne, 2);
        let a = t.process_exited(1, true);
        assert_eq!(kills(&a), vec![1001]);
        let a = t.incarnation_exited(11, &Exit::Killed, now());
        assert!(matches!(
            a[..],
            [Action::CloseAddress(101), Action::Kill(1000)]
        ));
        let a = t.incarnation_exited(10, &Exit::Killed, now());
        assert!(matches!(a[..], [Action::CloseAddress(100)]));
        assert!(t.entries.is_empty());
        assert_eq!(t.garbage.len(), 2, "both recipes handed back for freeing");
    }

    #[test]
    fn factory_members_are_keyed_and_stopped_together() {
        let mut t = Tree::default();
        let keys: Keys = Arc::default();
        t.add_supervisor(
            2,
            Parent::Process(1),
            1,
            Strategy::OneForOne,
            Budget::new(3, 5_000),
        );
        t.add_factory(3, 2, 1, None, Value::small_int(0), Arc::clone(&keys));
        for i in 0..2u64 {
            let key = Value::small_int(i as i64);
            let hash = hash_value(&key);
            lock(&keys).insert(
                hash,
                KeyEntry {
                    key: key.clone(),
                    address: 100 + i,
                    slot: 10 + i,
                },
            );
            let mut w = Worker::new(Policy::Temporary, 100 + i, key, None, None, Some(hash));
            w.current = Some(1000 + i);
            t.add_worker(10 + i, 3, 1, w);
        }
        let probe = |k: i64| {
            let key = Value::small_int(k);
            lock(&keys).find(hash_value(&key), &key)
        };
        assert_eq!(probe(1), Some(101));
        assert_eq!(probe(7), None);
        assert_eq!(t.count_of(3), 2);
        // A member ending retires itself and releases its key.
        let a = t.incarnation_exited(10, &Exit::Normal, now());
        assert!(matches!(a[..], [Action::CloseAddress(100)]));
        assert_eq!(probe(0), None);
        assert_eq!(t.garbage.len(), 2, "its recipe and its index key");
        // Retiring the tree stops the remaining member and retires the
        // factory, whose index is handed back for unpublishing.
        let a = t.process_exited(1, true);
        assert_eq!(kills(&a), vec![1001]);
        let a = t.incarnation_exited(11, &Exit::Killed, now());
        assert!(a.iter().any(|x| matches!(x, Action::CloseAddress(101))));
        assert!(t.entries.is_empty());
        assert_eq!(t.retired_factories, vec![3]);
    }

    #[test]
    fn a_normal_return_leaves_the_tree_standing() {
        let mut t = tree_with(Strategy::OneForOne, 1);
        assert!(t.process_exited(1, false).is_empty());
        assert_eq!(t.count_of(2), 1);
        // A later give-up with nobody to kill retires it explicitly.
        let a = t.retire_owned(1);
        assert_eq!(kills(&a), vec![1000]);
    }

    fn notices(actions: &Actions) -> Vec<(u64, &Ended, After)> {
        actions
            .iter()
            .filter_map(|a| match a {
                Action::Notify {
                    entry,
                    ended,
                    after,
                    ..
                } => Some((*entry, ended, *after)),
                Action::Kill(_)
                | Action::FailOwner(_)
                | Action::FireStopper { .. }
                | Action::Start(_)
                | Action::CloseAddress(_)
                | Action::Report(_) => None,
            })
            .collect()
    }

    #[test]
    fn a_watch_hears_every_exit_and_survives_restarts() {
        let mut t = tree_with(Strategy::OneForOne, 1);
        let id = t.watch(10, 1, Value::small_int(0)).expect("live entry");
        let a = t.incarnation_exited(10, &Exit::Killed, now());
        assert_eq!(starts(&a), vec![10]);
        let n = notices(&a);
        assert!(matches!(
            n[..],
            [(10, Ended::Exited(Exit::Killed), After::Running)]
        ));
        // Still registered: a second exit is reported too.
        let a = t.incarnation_exited(10, &Exit::Killed, now());
        assert_eq!(notices(&a).len(), 1);
        t.unwatch(10, id);
        let a = t.incarnation_exited(10, &Exit::Killed, now());
        assert!(notices(&a).is_empty());
        assert!(t.holders.is_empty(), "the holder index is cleaned up");
    }

    #[test]
    fn a_retiring_exit_reports_gone_exactly_once() {
        let mut t = tree_with(Strategy::OneForOne, 1);
        t.worker_mut(10).unwrap().policy = Policy::Temporary;
        t.watch(10, 1, Value::small_int(0)).unwrap();
        let a = t.incarnation_exited(10, &Exit::Normal, now());
        let n = notices(&a);
        assert!(matches!(
            n[..],
            [(10, Ended::Exited(Exit::Normal), After::Gone)]
        ));
        assert!(t.holders.is_empty());
        assert!(
            t.watch(10, 1, Value::small_int(0)).is_err(),
            "nothing left to watch"
        );
    }

    #[test]
    fn give_ups_and_removals_reach_watchers_of_the_owner() {
        let mut t = Tree::default();
        t.add_supervisor(
            2,
            Parent::Process(1),
            1,
            Strategy::OneForOne,
            Budget::new(0, 5_000),
        );
        let mut w = worker(100);
        w.current = Some(1000);
        t.add_worker(10, 2, 1, w);
        // Watched by some other process (9): the owner's own watches would
        // be released by its death before the teardown announced anything.
        t.watch(2, 9, Value::small_int(0)).unwrap();
        // Budget of zero: the first restart-worthy exit is a give-up, which
        // stops nothing (10 has just died) and escalates to process 1.
        let a = t.incarnation_exited(10, &Exit::Killed, now());
        let n = notices(&a);
        assert!(matches!(
            n[..],
            [(
                2,
                Ended::GaveUp {
                    restarts: 0,
                    within_ms: 5_000
                },
                After::Restarting
            )]
        ));
        assert!(a.iter().any(|x| matches!(x, Action::FailOwner(1))));
        // The owner failing removes the supervisor: one last notice.
        let a = t.process_exited(1, true);
        let n = notices(&a);
        assert!(matches!(n[..], [(2, Ended::Removed, After::Gone)]));
        assert!(t.holders.is_empty());
    }

    #[test]
    fn a_watchers_death_releases_its_watches_silently() {
        let mut t = tree_with(Strategy::OneForOne, 2);
        t.watch(10, 7, Value::small_int(0)).unwrap();
        t.watch(11, 7, Value::small_int(0)).unwrap();
        t.watch(11, 8, Value::small_int(0)).unwrap();
        let a = t.process_exited(7, false);
        assert!(a.is_empty());
        assert_eq!(t.entries[&10].watches.len(), 0);
        assert_eq!(t.entries[&11].watches.len(), 1);
        assert_eq!(t.garbage.len(), 2, "the released closures are freed");
        let a = t.incarnation_exited(11, &Exit::Killed, now());
        assert_eq!(notices(&a).len(), 1, "process 8's watch is untouched");
    }

    #[test]
    fn declarations_are_creator_only() {
        let t = tree_with(Strategy::OneForOne, 0);
        assert_eq!(t.owner_for_declaration(2, 1, false), Ok(()));
        assert_eq!(
            t.owner_for_declaration(2, 9, false),
            Err(Refusal::NotACreator)
        );
        assert_eq!(
            t.owner_for_declaration(2, 1, true),
            Err(Refusal::NoSuchFactory)
        );
        assert_eq!(
            t.owner_for_declaration(77, 1, false),
            Err(Refusal::NoSuchSupervisor)
        );
    }

    #[test]
    fn introspection_reads_the_tree() {
        let t = tree_with(Strategy::OneForAll, 2);
        assert_eq!(t.children_of(1), vec![2], "a process's top-level entries");
        assert_eq!(t.children_of(2), vec![10, 11]);
        assert_eq!(t.parent_of(10), Some(Parent::Entry(2)));
        assert_eq!(t.parent_of(2), Some(Parent::Process(1)));
        let info = t.info(2).unwrap();
        assert_eq!((info.kind, info.detail, info.restarts), (2, 1, 3));
        let info = t.info(10).unwrap();
        assert_eq!((info.kind, info.status, info.pid), (1, 0, Some(1000)));
        assert!(t.info(99).is_none());
    }
}
