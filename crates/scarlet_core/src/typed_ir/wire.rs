//! `wire`'s descriptor builder: a resolved type in, the descriptor that
//! `wire.encode`/`wire.decode` walk out, or a refusal naming the sub-type that
//! cannot cross and the path down to it.
//!
//! The descriptor is a **finite table** describing a value that is not: a
//! `List(Int)` is unbounded, but its descriptor is two nodes, and `Cons.tail`
//! is an index back at the node it sits in. That closure is bought by memoising
//! a nominal node under [`Key::Nominal`] *before* its fields are walked, so the
//! recursive occurrence finds the node already under construction. The memo is
//! keyed on the arguments' **descriptor nodes**, never on their [`RTy`]s:
//! substitution mints a fresh `RTy` for `Cons.tail`'s `List(Int)` every time,
//! so an `RTy`-keyed memo would never hit and the walk would not terminate.
//!
//! Encoding refuses exactly what decoding refuses. The asymmetry is tempting —
//! bytes can be written for things that cannot be read back — and it is
//! precisely what must not be allowed: a program that can write what it can
//! never read has been handed a corruption it will find later, elsewhere.
//!
//! Every value is encodable. The one refusal is [`Reason::TypeVariable`], a
//! fact about inference rather than about a value. An opaque type from
//! another module is described by its constructors like any other type.
//! `Pid`, `Subject`, `Connection`, `TlsConnection` and `net.Server` are
//! described as an [`Node::Identity`], which the runtime writes as the run
//! that minted the handle, its kind and its number. A function type is a
//! [`Node::Closure`] over its parameter and return types, which the runtime
//! writes as the run that made the closure, its function index and its
//! captures — each capture carrying its own tag, because the static type
//! fixes neither how many there are nor what they hold. A type no value has —
//! a `pub type Name` with no body that is none of the five handles — is a
//! [`Node::Uninhabited`]: nothing can be written or read for it, and it is
//! met only in a position the walk never takes, a phantom argument or a field
//! of a constructor no value is, so `Tagged(Native)` over an `Int` field
//! describes and its bytes are the `Int`.
//!
//! Type identity is always [`TypeId`], never `Con.name`: a user's `type Parsed`
//! and `scarlet/http/h1.Parsed` share a name and are different types.

use std::collections::HashMap;

use smallvec::SmallVec;

pub use scarlet_ir::wire::HandleKind;
use scarlet_ir::wire::{Ctor as RtCtor, Desc as RtDesc, Node as RtNode, NodeIdx as RtIdx};

use super::elaborate::subst_rty;
use super::rty::{RTy, ResolvedNode, ResolvedPool};
use crate::core_ir::{VariantRef, WireOp};
use crate::type_def::TypeId;
use crate::types::StrId;

/// Abort: the checker admitted a type whose shape the descriptor builder
/// cannot read, e.g. an `Array` with no element type. The span-free counterpart
/// to [`super::elaborate::elaborator_bug`] — a resolved type carries none.
///
/// No program reaches this. Inventing a descriptor instead would encode a value
/// the peer then rebuilds wrong, which is the one failure this module exists to
/// make impossible.
#[allow(clippy::panic)]
#[cold]
#[inline(never)]
pub(crate) fn wire_bug(why: &'static str) -> ! {
    panic!(
        "internal compiler error: {why} is well-typed but has no wire descriptor. \
         Report this as a compiler bug."
    )
}

/// Index into [`Desc::nodes`]. Only meaningful for the `Desc` that minted it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct NodeIdx(u32);

impl std::fmt::Display for NodeIdx {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// One field of one constructor: the label a decoder rebuilds it under, and the
/// descriptor of its type.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WireField {
    label: StrId,
    node: NodeIdx,
}

/// One constructor a decoder may build.
///
/// Carries the `VariantRef` a decoder rebuilds it by, plus the declared name
/// and labels, which the fingerprint folds as text and a refusal path quotes.
/// The runtime descriptor keeps only the `VariantRef`: a name there would be a
/// second identity for a constructor that already has one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WireVariant {
    variant: VariantRef,
    /// Declaration name. Fingerprint and refusal paths mix this as text.
    name: StrId,
    /// Declared order, which is the order a decoder fills the payload in.
    fields: Vec<WireField>,
}

/// One node of a [`Desc`]. Children are [`NodeIdx`] rather than nested nodes,
/// which is what lets a recursive type be a finite table.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Node {
    Int,
    Float,
    String,
    Binary,
    Array(NodeIdx),
    Map(NodeIdx, NodeIdx),
    Tuple(Vec<NodeIdx>),
    /// Constructors in declared order: a variant's position here is the tag the
    /// peer receives, so reordering a type's constructors is a wire break.
    Data(Vec<WireVariant>),
    /// A host-backed handle: the kind its static type names, and the nodes of
    /// its type arguments. The arguments are on the wire nowhere — a
    /// `Subject`'s bytes are its mailbox id whatever it carries — but they are
    /// part of the SHAPE: a `Subject(Int)` decoded from `Subject(String)`
    /// bytes would be a typed door onto a mailbox of the other type, so the
    /// fingerprint folds them and the runtime node drops them.
    Identity(HandleKind, Vec<NodeIdx>),
    /// `fn(params...) ret`. The parameter and return nodes are part of the
    /// SHAPE — `fn(Int) Int` and `fn(String) Int` must not share a
    /// fingerprint, or a decoded function would be a typed door onto a body
    /// expecting the other argument — and a parameter type that cannot cross
    /// refuses through them. The bytes hold none of it: a closure is written
    /// as function index plus self-described captures, and the runtime node
    /// keeps only the parameter count, which a decoder checks against the
    /// function the index names.
    Closure(Vec<NodeIdx>, NodeIdx),
    /// A type no value has ([`Nominal::Uninhabited`]). One node for every
    /// such type, as `Int` is one node: the fingerprint excludes names and
    /// there is no structure to tell two apart. Never written or read — it
    /// stands where the walk never goes.
    Uninhabited,
}

/// The descriptor of one type: a node table, the node the type itself is, and
/// the fingerprint of that shape.
///
/// `fingerprint` is not recomputable from the other two fields: it folds
/// constructor and field names as **text**, and a [`Desc`] holds only the
/// [`StrId`]s. It is minted in [`build_desc`], which has the [`WireCtx`] that
/// can resolve them, and there is no second way to arrive at one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Desc {
    nodes: Vec<Node>,
    root: NodeIdx,
    fingerprint: u64,
}

impl Desc {
    /// The node the described type itself is.
    // Elaboration builds a descriptor and pools its fingerprint; nothing in
    // the library walks the node table yet, so these three accessors still
    // have only the tests below as readers. They are marked one at a time
    // rather than on the `impl` block, so that a method added here later is
    // not silently covered too.
    #[allow(dead_code)]
    fn root(&self) -> NodeIdx {
        self.root
    }

    /// The 64-bit hash of this type's shape. Two peers agree they are talking
    /// about the same shape by comparing these; `wire.scrl`'s module doc
    /// specifies how it is computed and what moves it.
    fn fingerprint(&self) -> u64 {
        self.fingerprint
    }

    /// The node at `i`, or `None` for an index from another `Desc`.
    #[allow(dead_code)]
    fn node(&self, i: NodeIdx) -> Option<&Node> {
        self.nodes.get(i.0 as usize)
    }

    #[allow(dead_code)]
    fn len(&self) -> usize {
        self.nodes.len()
    }

    /// This descriptor as the one the program carries, for `Program.wire`.
    ///
    /// **Every name is dropped**, and that is the point rather than an
    /// economy: the VM reads structure, and rebuilds a constructor through
    /// `Program::types`, which is keyed on the identity [`VariantRef`] already
    /// carries. A name here would be a second identity for a constructor that
    /// has one.
    ///
    /// The fingerprint travels because it cannot be recomputed on the other
    /// side: it folds constructor and field names as text, and the runtime
    /// descriptor holds none.
    ///
    /// Index-preserving. `NodeIdx` and the runtime's own are the same numbers
    /// over the same table, which is what lets a recursive type stay finite
    /// across the conversion — `Cons.tail` still points at the node it sits
    /// in.
    pub(crate) fn to_runtime(&self) -> RtDesc {
        let at = |n: NodeIdx| RtIdx(n.0);
        let nodes = self
            .nodes
            .iter()
            .map(|n| match n {
                Node::Int => RtNode::Int,
                Node::Float => RtNode::Float,
                Node::String => RtNode::String,
                Node::Binary => RtNode::Binary,
                Node::Array(e) => RtNode::Array(at(*e)),
                Node::Map(k, v) => RtNode::Map(at(*k), at(*v)),
                Node::Tuple(es) => RtNode::Tuple(es.iter().map(|e| at(*e)).collect()),
                // Declared order is preserved, because a variant's position in
                // this list is the tag the peer receives.
                Node::Data(vs) => RtNode::Data(
                    vs.iter()
                        .map(|v| RtCtor {
                            type_id: v.variant.type_id,
                            variant_idx: v.variant.variant_idx,
                            fields: v.fields.iter().map(|f| at(f.node)).collect(),
                        })
                        .collect(),
                ),
                // The arguments are already folded into the fingerprint that
                // travels with this; the walk needs only the kind.
                Node::Identity(k, _) => RtNode::Identity(*k),
                // Likewise the parameter and return types; the decoder needs
                // the count alone, to check the function the bytes name.
                Node::Closure(params, _) => RtNode::Closure {
                    arity: params.len() as u32,
                },
                Node::Uninhabited => RtNode::Uninhabited,
            })
            .collect();
        RtDesc::new(nodes, at(self.root), self.fingerprint())
    }
}

/// A scalar or container the wire format writes directly, rather than as a
/// tagged constructor.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Builtin {
    Int,
    Float,
    String,
    Binary,
    /// `Array(elem)`.
    Array,
    /// `Map(key, value)`.
    Map,
}

/// One constructor as declared, before instantiation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CtorDecl {
    variant: VariantRef,
    /// `variants[variant_idx].name`. Fingerprint and refusal paths mix this
    /// as text; it is not a dispatch key.
    name: StrId,
    /// Declared order. Types are **closed** over the owning type's parameters:
    /// `Bound(i)` is the `i`th argument of the `Con` being described, which is
    /// the form `close_body` stores a `VariantField` in.
    fields: Vec<(StrId, RTy)>,
}

impl CtorDecl {
    /// One declared constructor, for a [`WireCtx`] answering [`Nominal::Data`].
    ///
    /// `fields` must be closed over the owning type's parameters — `Bound(i)`
    /// for the `i`th, which is the form `close_body` leaves a `VariantField`
    /// in. A field carrying a live inference variable instead would be
    /// described as a rigid parameter and refuse.
    pub(crate) fn new(variant: VariantRef, name: StrId, fields: Vec<(StrId, RTy)>) -> CtorDecl {
        CtorDecl {
            variant,
            name,
            fields,
        }
    }
}

/// What a [`TypeId`] denotes to `wire`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Nominal {
    Builtin(Builtin),
    /// `type Name(params) = Target`. Closed over the type's parameters, and
    /// looked through rather than refused — an alias to an encodable type is
    /// encodable.
    Alias(RTy),
    /// Declared constructors, walked wherever the type is met. Visibility is
    /// not consulted: an opaque type from another module crosses whenever its
    /// fields do, and a decoder rebuilds it by constructor. Its invariants
    /// are that module's to re-check.
    Data {
        ctors: Vec<CtorDecl>,
    },
    /// One of the five stdlib handle types, which the runtime can hand back
    /// by identity: `Pid`, `Subject`, `Connection`, `TlsConnection` and
    /// `net.Server`. Resolved by module identity in `Compiler::nominal`, never
    /// by name, so a user's `type Pid` is not one.
    Handle(HandleKind),
    /// `pub type Name` with no body that is not a [`Nominal::Handle`]. No
    /// Scarlet expression builds a value of it — it declares no constructor
    /// and no VM table hands one out — so there is nothing to write and
    /// nothing to read, and it describes as [`Node::Uninhabited`].
    Uninhabited,
}

/// What the descriptor builder must ask about a nominal type.
///
/// A [`ResolvedPool`] carries a type's shape but not its declaration, and
/// `ElabCtx` deliberately exposes no inference engine, so this is the one seam
/// between the two. Same split, and for the same reason, as
/// [`super::elaborate_pat::PatCtx`]: every type crossing it is an [`RTy`], so
/// the builder is exercised against hand-built types without an engine.
pub(crate) trait WireCtx {
    /// What `id` denotes. Field and alias-target types come back closed over
    /// the declaring type's own parameters.
    ///
    /// The pool is passed in because a real declaration table holds inference
    /// types, not [`RTy`]s: answering at all means interning a variant's field
    /// types into the pool the walk is building against. It must be that pool
    /// and no other — an [`RTy`] is an index and means nothing anywhere else.
    fn nominal(&mut self, pool: &mut ResolvedPool, id: TypeId) -> Nominal;

    /// Display text for an interned name, for a refusal's path.
    fn name(&self, s: StrId) -> String;

    /// Display text for a nominal type, for a refusal's rendered type. A
    /// resolved `Con` carries only its [`TypeId`], so the name a reader wrote
    /// is the declaration's, not the node's.
    fn type_name(&self, id: TypeId) -> String;
}

/// One step from the described type down towards the sub-type that refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Step {
    /// The variant is named as well as the label because two constructors of
    /// one type may each have a field called `value`, and a path that said only
    /// `value` would not say which.
    Field {
        variant: StrId,
        label: StrId,
    },
    Element,
    Key,
    Value,
    TupleElem(usize),
    /// The `i`th parameter of a function type.
    Param(usize),
    /// A function type's return.
    Return,
}

/// Why a type cannot cross the wire.
///
/// One arm, and it is a fact about inference rather than about a value: the
/// descriptor is a function of the static type, and a polymorphic type has
/// none. No arm is about a value, because every value encodes: a closure's
/// captures are not fixed by its type, so each is described inline; a type
/// no value has is a node the walk never reaches, not a reason to refuse the
/// record around it. The criterion is reconstructibility, never whether a
/// byte representation exists — Erlang writes funs (`NEW_FUN_EXT`) and pids
/// (`NEW_PID_EXT`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reason {
    TypeVariable,
}

impl Reason {
    /// The prose a diagnostic carries. Present tense, naming the property that
    /// fails rather than the syntax that has it.
    fn describe(self) -> &'static str {
        match self {
            Reason::TypeVariable => {
                "the type is still polymorphic here, so its representation is not known"
            }
        }
    }
}

/// A type that cannot cross the wire, and where it was found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WireRefusal {
    /// The offending sub-type — not the type the call named. Rendering it is
    /// the caller's job; it holds the pool.
    ty: RTy,
    reason: Reason,
    /// Outermost first. Empty when the described type is itself the refusal.
    path: Vec<Step>,
}

impl WireRefusal {
    /// The path text a diagnostic quotes: `Outer.mid -> Middle.inner ->
    /// Inner.f`. Empty when the refusal is the described type itself.
    ///
    /// This is the half of the diagnostic that makes it useful: a refusal three
    /// levels down a record that says only "cannot encode" leaves the reader to
    /// find which field it meant.
    fn path_text<C: WireCtx + ?Sized>(&self, cx: &C) -> String {
        self.path
            .iter()
            .map(|s| match *s {
                Step::Field { variant, label } => {
                    format!("{}.{}", cx.name(variant), cx.name(label))
                }
                Step::Element => "[element]".to_string(),
                Step::Key => "[key]".to_string(),
                Step::Value => "[value]".to_string(),
                Step::TupleElem(i) => format!("[{i}]"),
                Step::Param(i) => format!("[param {i}]"),
                Step::Return => "[return]".to_string(),
            })
            .collect::<Vec<_>>()
            .join(" -> ")
    }

    /// The diagnostic this refusal reports at a `wire` call site.
    ///
    /// An unresolved type at the root is worded as an instruction rather
    /// than as a property: the programmer clears it by writing an annotation
    /// instead of by not putting the type on the wire. Reached through a
    /// path — `decode` at `Box(a)` with `a` unknown, or a generic function
    /// encoding `Outer(a)` — the refusal names the property and the path
    /// says where the unknown is, because a refusal three levels into a
    /// record that says only "cannot encode" leaves the reader to find
    /// which field it meant.
    pub(crate) fn message<C: WireCtx + ?Sized>(
        &self,
        cx: &C,
        pool: &ResolvedPool,
        op: WireOp,
    ) -> String {
        if self.reason == Reason::TypeVariable && self.path.is_empty() {
            return match op {
                WireOp::Encode => {
                    "the type `wire.encode` is given here is not known; annotate the binding"
                        .to_string()
                }
                WireOp::Decode => {
                    "the type `wire.decode` produces here is not known; annotate the binding"
                        .to_string()
                }
            };
        }
        let head = format!(
            "`{}` cannot {} `{}`: {}",
            call_text(op),
            verb_text(op),
            render_rty(cx, pool, self.ty),
            self.reason.describe()
        );
        if self.path.is_empty() {
            head
        } else {
            // Not "at": the driver appends the call's own source location to
            // every diagnostic, and two "at"s in one line read as one place.
            format!("{head} (through {})", self.path_text(cx))
        }
    }
}

/// The call a refusal names. Both ops refuse exactly the same types — that
/// symmetry is the module's whole point — so this only chooses the sentence,
/// never the verdict.
fn call_text(op: WireOp) -> &'static str {
    match op {
        WireOp::Encode => "wire.encode",
        WireOp::Decode => "wire.decode",
    }
}

fn verb_text(op: WireOp) -> &'static str {
    match op {
        WireOp::Encode => "encode",
        WireOp::Decode => "decode",
    }
}

/// A resolved type as source-like text, for a diagnostic.
///
/// The display name comes from the context rather than the node: a resolved
/// `Con` carries only the [`TypeId`] that decides identity, and this is the one
/// place in the module that wants the name a reader wrote instead.
fn render_rty<C: WireCtx + ?Sized>(cx: &C, pool: &ResolvedPool, t: RTy) -> String {
    let list = |xs: &[RTy]| {
        xs.iter()
            .map(|&x| render_rty(cx, pool, x))
            .collect::<Vec<_>>()
            .join(", ")
    };
    match pool.node(t) {
        ResolvedNode::Bound(i) => bound_name(i),
        ResolvedNode::Con { id, args } => {
            let args = pool.children(args);
            if args.is_empty() {
                cx.type_name(id)
            } else {
                format!("{}({})", cx.type_name(id), list(args))
            }
        }
        ResolvedNode::Fun { params, ret } => {
            format!(
                "fn({}) {}",
                list(pool.children(params)),
                render_rty(cx, pool, ret)
            )
        }
        ResolvedNode::Tuple { elems } => format!("({})", list(pool.children(elems))),
    }
}

/// A quantified parameter as the letter a signature would spell it with.
/// Past `z` the index is written out, which no real signature reaches but
/// which keeps the function total.
fn bound_name(i: u32) -> String {
    match u8::try_from(i) {
        Ok(n) if i < 26 => char::from(b'a' + n).to_string(),
        _ => format!("t{i}"),
    }
}

/// What makes two descriptor nodes the same node.
///
/// A nominal is keyed on its `TypeId` and its arguments' *nodes*, not on the
/// variant list it will expand into — that is what lets the key exist before
/// the variants do, which is what closes recursion.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
enum Key {
    Int,
    Float,
    String,
    Binary,
    Array(NodeIdx),
    Map(NodeIdx, NodeIdx),
    Tuple(SmallVec<[NodeIdx; 4]>),
    Nominal(TypeId, SmallVec<[NodeIdx; 4]>),
    /// Keyed on the kind rather than the `TypeId`: two programs' `Pid`s are
    /// one shape, exactly as their `Int`s are.
    Handle(HandleKind, SmallVec<[NodeIdx; 4]>),
    /// Parameters then return, so `fn(Int) Int` twice is one node.
    Fun(SmallVec<[NodeIdx; 4]>, NodeIdx),
    /// Every type no value has, as one node: there is nothing to key on.
    Uninhabited,
}

/// Version of the fingerprint algorithm, folded in first so that a change to
/// what is hashed cannot produce a value a peer would mistake for the old one.
///
/// **Bumping this is bumping the format version byte**, and `wire.scrl`'s
/// module doc says so as a rule rather than as a note: the fingerprint travels
/// in every message, so an algorithm change that kept the version would show up
/// at a peer as a `SchemaMismatch` on a type nobody had touched.
///
/// The wire format's own version byte is this same number. A change bumps it
/// when it alters a fingerprint some peer already holds; a change that only
/// gives a fingerprint to a type that had none — kind tag 14, an uninhabited
/// node, is one — does not.
///
/// Still 3, the algorithm `docs/wire-format.md` specifies for that version.
/// Version 4 changes what a closure node contributes, so the bump belongs with
/// the work that builds closures rather than here, where nothing writes a byte
/// and no peer holds a number.
const FINGERPRINT_VERSION: u64 = 3;

const FINGERPRINT_BASIS: u64 = 0xcbf2_9ce4_8422_2325;
const FINGERPRINT_PRIME: u64 = 0x0000_0100_0000_01b3;

/// FNV-1a over one `u64` at a time. The same construction the VM's value hash
/// uses, restated here because these two hashes must be free to move
/// independently: that one is an equality fast-reject inside one program and
/// samples long inputs, this one is a compatibility surface between two
/// programs and may never sample anything.
#[inline]
fn mix(h: u64, v: u64) -> u64 {
    (h ^ v).wrapping_mul(FINGERPRINT_PRIME)
}

/// Length first, then the UTF-8 bytes — so `("ab", "c")` and `("a", "bc")` do
/// not fold to the same hash.
fn mix_str(h: u64, s: &str) -> u64 {
    let mut h = mix(h, s.len() as u64);
    for b in s.as_bytes() {
        h = mix(h, u64::from(*b));
    }
    h
}

/// Fold the node table in index order.
///
/// A child is folded as its **index**, never by descending into it, which is
/// what lets a recursive type hash without recursing: `Cons.tail` is the number
/// 1, and node 1's own row is folded when the loop reaches it. That makes the
/// fingerprint a hash of the table as [`build_desc`] canonically lays it out —
/// so a change to the walk order is a change to the algorithm, and takes
/// [`FINGERPRINT_VERSION`] with it.
///
/// What is folded, and the reasoning, is specified for a second implementation
/// in `wire.scrl`'s module doc; this function is that spec in Rust.
fn fingerprint_of<C: WireCtx + ?Sized>(cx: &C, nodes: &[Node], root: NodeIdx) -> u64 {
    let mut h = mix(FINGERPRINT_BASIS, FINGERPRINT_VERSION);
    h = mix(h, nodes.len() as u64);
    h = mix(h, u64::from(root.0));

    for n in nodes {
        h = match n {
            Node::Int => mix(h, 1),
            Node::Float => mix(h, 2),
            Node::String => mix(h, 3),
            Node::Binary => mix(h, 4),
            Node::Array(e) => mix(mix(h, 5), u64::from(e.0)),
            Node::Map(k, v) => mix(mix(mix(h, 6), u64::from(k.0)), u64::from(v.0)),
            Node::Tuple(es) => {
                let mut h = mix(mix(h, 7), es.len() as u64);
                for e in es {
                    h = mix(h, u64::from(e.0));
                }
                h
            }
            Node::Data(vs) => {
                let mut h = mix(mix(h, 8), vs.len() as u64);
                for v in vs {
                    // The tag the peer receives, and the constructor name. The
                    // owning type's id and NAME are deliberately absent: two
                    // programs that declare this shape independently, under
                    // whatever name and in whatever module, must agree.
                    h = mix(h, u64::from(v.variant.variant_idx));
                    h = mix_str(h, &cx.name(v.name));
                    h = mix(h, v.fields.len() as u64);
                    for f in &v.fields {
                        h = mix_str(h, &cx.name(f.label));
                        h = mix(h, u64::from(f.node.0));
                    }
                }
                h
            }
            // The kind byte is folded, not just the tag: the fingerprint
            // excludes names, so without it every handle type would be one
            // shape and `decode` at `Pid` would accept `Connection` bytes.
            // Then the arguments, as a `Data` node folds its fields — a
            // `Subject(Int)` and a `Subject(String)` are two doors onto two
            // kinds of mailbox.
            Node::Identity(k, args) => {
                let mut h = mix(mix(h, 9), u64::from(*k as u8));
                h = mix(h, args.len() as u64);
                for a in args {
                    h = mix(h, u64::from(a.0));
                }
                h
            }
            // Parameter count, each parameter, then the return — as a
            // `Tuple` folds its elements. The count is folded even though the
            // indices imply it, so `fn(Int) Int` and `fn(Int, Int) Int` differ
            // by more than one mixed word.
            Node::Closure(params, ret) => {
                let mut h = mix(mix(h, 10), params.len() as u64);
                for p in params {
                    h = mix(h, u64::from(p.0));
                }
                mix(h, u64::from(ret.0))
            }
            // The tag alone: an uninhabited type has no structure. 14 and
            // not 11, because 11 to 13 were version 3's capture tags for Nil,
            // Bool and Range. They stay reserved so no number is ever reused
            // (`docs/wire-format.md`).
            Node::Uninhabited => mix(h, 14),
        };
    }
    h
}

/// Build the descriptor of `root`, or refuse.
///
/// Total over the checker's output: every resolved type either describes or
/// refuses, and the refusal names the sub-type and the path to it.
///
/// `pool` is taken mutably because instantiating a variant's field types at the
/// type's arguments mints nodes — `Option(Int)`'s `Some.value` is an `Int` that
/// the declared body, which says `Bound(0)`, does not contain.
pub(crate) fn build_desc<C: WireCtx + ?Sized>(
    pool: &mut ResolvedPool,
    cx: &mut C,
    root: RTy,
) -> Result<Desc, WireRefusal> {
    let mut b = Build {
        pool,
        cx,
        nodes: Vec::new(),
        memo: HashMap::new(),
    };
    let mut path = Vec::new();
    let root = b.walk(root, &mut path)?;
    let fingerprint = fingerprint_of(b.cx, &b.nodes, root);
    Ok(Desc {
        nodes: b.nodes,
        root,
        fingerprint,
    })
}

struct Build<'a, C: WireCtx + ?Sized> {
    pool: &'a mut ResolvedPool,
    cx: &'a mut C,
    nodes: Vec<Node>,
    memo: HashMap<Key, NodeIdx>,
}

impl<C: WireCtx + ?Sized> Build<'_, C> {
    /// The node for `key`, minting `mk()` on first ask.
    fn intern(&mut self, key: Key, mk: impl FnOnce() -> Node) -> NodeIdx {
        if let Some(&i) = self.memo.get(&key) {
            return i;
        }
        let i = NodeIdx(self.nodes.len() as u32);
        self.nodes.push(mk());
        self.memo.insert(key, i);
        i
    }

    fn refuse(&self, ty: RTy, reason: Reason, path: &[Step]) -> WireRefusal {
        WireRefusal {
            ty,
            reason,
            path: path.to_vec(),
        }
    }

    /// Descend into `t` with `step` appended to the path.
    ///
    /// The step is not popped on the error path: the refusal has already copied
    /// the path it needs, and every frame above returns immediately.
    fn child(&mut self, t: RTy, step: Step, path: &mut Vec<Step>) -> Result<NodeIdx, WireRefusal> {
        path.push(step);
        let n = self.walk(t, path)?;
        path.pop();
        Ok(n)
    }

    fn walk(&mut self, t: RTy, path: &mut Vec<Step>) -> Result<NodeIdx, WireRefusal> {
        match self.pool.node(t) {
            ResolvedNode::Bound(_) => Err(self.refuse(t, Reason::TypeVariable, path)),
            // Described like a tuple of its parameters and return: each is
            // part of the shape and a level of structure a refusal can be
            // reached through. The bytes carry none of them.
            ResolvedNode::Fun { params, ret } => {
                let params: SmallVec<[RTy; 4]> = self.pool.children(params).into();
                let mut kids: SmallVec<[NodeIdx; 4]> = SmallVec::new();
                for (i, p) in params.iter().enumerate() {
                    kids.push(self.child(*p, Step::Param(i), path)?);
                }
                let r = self.child(ret, Step::Return, path)?;
                Ok(self.intern(Key::Fun(kids.clone(), r), || {
                    Node::Closure(kids.to_vec(), r)
                }))
            }
            ResolvedNode::Tuple { elems } => {
                let elems: SmallVec<[RTy; 4]> = self.pool.children(elems).into();
                let mut kids: SmallVec<[NodeIdx; 4]> = SmallVec::new();
                for (i, e) in elems.iter().enumerate() {
                    kids.push(self.child(*e, Step::TupleElem(i), path)?);
                }
                Ok(self.intern(Key::Tuple(kids.clone()), || Node::Tuple(kids.to_vec())))
            }
            ResolvedNode::Con { id, args, .. } => {
                let args: SmallVec<[RTy; 4]> = self.pool.children(args).into();
                self.con(id, &args, path)
            }
        }
    }

    fn con(
        &mut self,
        id: TypeId,
        args: &[RTy],
        path: &mut Vec<Step>,
    ) -> Result<NodeIdx, WireRefusal> {
        match self.cx.nominal(self.pool, id) {
            // No value, so no structure and no arguments worth walking: a
            // `pub type Name` has none to be applied to.
            Nominal::Uninhabited => Ok(self.intern(Key::Uninhabited, || Node::Uninhabited)),

            // The arguments are walked for the same reason `data` walks a
            // phantom parameter: encodability is a property of the type, and
            // `Subject(a)` with `a` unknown refuses at the `a` like
            // `Phantom(a)` does. No path step — a handle is a leaf the way
            // `Int` is.
            Nominal::Handle(kind) => {
                let mut arg_nodes: SmallVec<[NodeIdx; 4]> = SmallVec::new();
                for a in args {
                    arg_nodes.push(self.walk(*a, path)?);
                }
                let key = Key::Handle(kind, arg_nodes.clone());
                Ok(self.intern(key, || Node::Identity(kind, arg_nodes.to_vec())))
            }

            Nominal::Builtin(b) => self.builtin(b, args, path),

            // Looked through without a path step: an alias is a spelling, not a
            // level of structure, and naming it in the path would point the
            // reader at a field that does not exist.
            Nominal::Alias(target) => {
                let at = self.instantiate(target, args);
                self.walk(at, path)
            }

            Nominal::Data { ctors } => self.data(id, args, ctors, path),
        }
    }

    fn builtin(
        &mut self,
        b: Builtin,
        args: &[RTy],
        path: &mut Vec<Step>,
    ) -> Result<NodeIdx, WireRefusal> {
        match b {
            Builtin::Int => Ok(self.intern(Key::Int, || Node::Int)),
            Builtin::Float => Ok(self.intern(Key::Float, || Node::Float)),
            Builtin::String => Ok(self.intern(Key::String, || Node::String)),
            Builtin::Binary => Ok(self.intern(Key::Binary, || Node::Binary)),
            Builtin::Array => {
                let Some(&elem) = args.first() else {
                    wire_bug("an array type with no element type")
                };
                let e = self.child(elem, Step::Element, path)?;
                Ok(self.intern(Key::Array(e), || Node::Array(e)))
            }
            Builtin::Map => {
                let (Some(&k), Some(&v)) = (args.first(), args.get(1)) else {
                    wire_bug("a map type without both a key and a value type")
                };
                let kn = self.child(k, Step::Key, path)?;
                let vn = self.child(v, Step::Value, path)?;
                Ok(self.intern(Key::Map(kn, vn), || Node::Map(kn, vn)))
            }
        }
    }

    /// The node for `id(args...)`, expanding its constructors.
    ///
    /// The node is minted and memoised *before* the fields are walked, and
    /// filled in afterwards. A recursive occurrence hits the memo and gets this
    /// index, which is the whole of why the table is finite.
    fn data(
        &mut self,
        id: TypeId,
        args: &[RTy],
        ctors: Vec<CtorDecl>,
        path: &mut Vec<Step>,
    ) -> Result<NodeIdx, WireRefusal> {
        let mut arg_nodes: SmallVec<[NodeIdx; 4]> = SmallVec::new();
        for a in args {
            // A type argument is described even when no constructor mentions
            // it. `Phantom(a)` with `a` unknown still refuses at `a`, so the
            // same type is encodable or not regardless of which constructors
            // it happens to have — the alternative makes encodability depend
            // on a body the caller cannot see. `Phantom(Native)` describes:
            // the argument is a node no value reaches, not a refusal.
            arg_nodes.push(self.walk(*a, path)?);
        }

        let key = Key::Nominal(id, arg_nodes);
        if let Some(&i) = self.memo.get(&key) {
            return Ok(i);
        }
        let idx = NodeIdx(self.nodes.len() as u32);
        self.nodes.push(Node::Data(Vec::new()));
        self.memo.insert(key, idx);

        let mut variants = Vec::with_capacity(ctors.len());
        for c in ctors {
            let mut fields = Vec::with_capacity(c.fields.len());
            for (label, declared) in c.fields {
                let at = self.instantiate(declared, args);
                let step = Step::Field {
                    variant: c.name,
                    label,
                };
                fields.push(WireField {
                    label,
                    node: self.child(at, step, path)?,
                });
            }
            variants.push(WireVariant {
                variant: c.variant,
                name: c.name,
                fields,
            });
        }

        let Some(slot) = self.nodes.get_mut(idx.0 as usize) else {
            wire_bug("a descriptor node that was minted and then lost")
        };
        *slot = Node::Data(variants);
        Ok(idx)
    }

    /// A declared type, closed over its owner's parameters, at `args`.
    ///
    /// `Bound(i)` is the `i`th parameter, which is the form `close_body` stores
    /// a `VariantField` in, so the map is positional. The rewrite itself is the
    /// elaborator's — a second implementation of substitution is a second thing
    /// to keep in step with the pool's node kinds.
    fn instantiate(&mut self, declared: RTy, args: &[RTy]) -> RTy {
        if args.is_empty() {
            return declared;
        }
        let m: HashMap<u32, RTy> = args
            .iter()
            .enumerate()
            .map(|(i, &a)| (i as u32, a))
            .collect();
        subst_rty(self.pool, declared, &m)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::PrimIds;

    const INT: TypeId = TypeId(1);
    const FLOAT: TypeId = TypeId(2);
    const STRING: TypeId = TypeId(3);
    const ARRAY: TypeId = TypeId(4);
    const BINARY: TypeId = TypeId(5);
    const MAP: TypeId = TypeId(6);
    const NATIVE: TypeId = TypeId(7);

    /// A hand-built type world: no inference engine, no module table, so a test
    /// states exactly the declarations it is about.
    struct Fixture {
        names: Vec<String>,
        types: HashMap<TypeId, Nominal>,
        type_names: HashMap<TypeId, StrId>,
        pool: ResolvedPool,
    }

    impl Fixture {
        fn new() -> Fixture {
            let mut f = Fixture {
                names: Vec::new(),
                types: HashMap::new(),
                type_names: HashMap::new(),
                pool: ResolvedPool::new(PrimIds {
                    int: INT,
                    float: FLOAT,
                    string: STRING,
                    array: ARRAY,
                }),
            };
            f.types.insert(INT, Nominal::Builtin(Builtin::Int));
            f.types.insert(FLOAT, Nominal::Builtin(Builtin::Float));
            f.types.insert(STRING, Nominal::Builtin(Builtin::String));
            f.types.insert(BINARY, Nominal::Builtin(Builtin::Binary));
            f.types.insert(ARRAY, Nominal::Builtin(Builtin::Array));
            f.types.insert(MAP, Nominal::Builtin(Builtin::Map));
            f
        }

        fn intern(&mut self, s: &str) -> StrId {
            if let Some(i) = self.names.iter().position(|n| n == s) {
                return StrId(i as u32);
            }
            self.names.push(s.to_string());
            StrId((self.names.len() - 1) as u32)
        }

        /// `Name(args...)`. A resolved `Con` carries only its id, so the
        /// display name is recorded beside it for `WireCtx::type_name`.
        fn con(&mut self, id: TypeId, name: &str, args: &[RTy]) -> RTy {
            let n = self.intern(name);
            self.type_names.insert(id, n);
            self.pool.mk_con(id, args)
        }

        fn int(&mut self) -> RTy {
            self.con(INT, "Int", &[])
        }

        fn string(&mut self) -> RTy {
            self.con(STRING, "String", &[])
        }

        fn bound(&mut self, i: u32) -> RTy {
            self.pool.mk_bound(i)
        }

        /// `pub type Native` — a user-declared bodiless type, which no VM
        /// table backs and no expression builds. Described as uninhabited.
        fn native(&mut self) -> RTy {
            self.types.insert(NATIVE, Nominal::Uninhabited);
            self.con(NATIVE, "Native", &[])
        }

        /// Declare `type Name(params) { ... }`.
        fn data(&mut self, id: TypeId, ctors: Vec<CtorDecl>) {
            self.types.insert(id, Nominal::Data { ctors });
        }

        fn ctor(
            &mut self,
            ty: TypeId,
            ty_name: &str,
            idx: u16,
            name: &str,
            fields: &[(&str, RTy)],
        ) -> CtorDecl {
            let type_name = self.intern(ty_name);
            self.type_names.insert(ty, type_name);
            let variant_name = self.intern(name);
            let fields = fields
                .iter()
                .map(|(l, t)| (self.intern(l), *t))
                .collect::<Vec<_>>();
            CtorDecl {
                variant: VariantRef {
                    type_id: ty,
                    variant_idx: idx,
                },
                name: variant_name,
                fields,
            }
        }

        fn build(&mut self, root: RTy) -> Result<Desc, WireRefusal> {
            // Split the borrow: the walk needs the pool mutably and the
            // declarations immutably, and they live in one fixture.
            let mut cx = Decls {
                names: &self.names,
                type_names: &self.type_names,
                types: &self.types,
            };
            build_desc(&mut self.pool, &mut cx, root)
        }
    }

    struct Decls<'a> {
        names: &'a [String],
        types: &'a HashMap<TypeId, Nominal>,
        type_names: &'a HashMap<TypeId, StrId>,
    }

    impl WireCtx for Decls<'_> {
        // The declarations are already `RTy`s the fixture minted, so this
        // implementation has no use for the pool. A real one does.
        fn nominal(&mut self, _pool: &mut ResolvedPool, id: TypeId) -> Nominal {
            match self.types.get(&id) {
                Some(n) => n.clone(),
                // A real compile aborts on an undeclared id (`wire_bug`); a
                // test that reaches one has a typo, and is told so.
                None => panic!("the fixture declares no type {id:?}"),
            }
        }

        fn name(&self, s: StrId) -> String {
            self.names
                .get(s.0 as usize)
                .cloned()
                .unwrap_or_else(|| format!("?{}", s.0))
        }

        fn type_name(&self, id: TypeId) -> String {
            match self.type_names.get(&id) {
                Some(&s) => self.name(s),
                None => format!("<type {}>", id.0),
            }
        }
    }

    fn refusal(r: Result<Desc, WireRefusal>) -> WireRefusal {
        match r {
            Ok(d) => panic!("expected a refusal, got a descriptor of {} nodes", d.len()),
            Err(e) => e,
        }
    }

    fn desc(r: Result<Desc, WireRefusal>) -> Desc {
        match r {
            Ok(d) => d,
            Err(e) => panic!(
                "expected a descriptor, refused: {:?} at {:?}",
                e.reason, e.path
            ),
        }
    }

    // --- the encodability table, accepting direction ---

    #[test]
    fn the_scalars_describe_as_themselves() {
        let mut f = Fixture::new();
        for (id, name, want) in [
            (INT, "Int", Node::Int),
            (FLOAT, "Float", Node::Float),
            (STRING, "String", Node::String),
            (BINARY, "Binary", Node::Binary),
        ] {
            let t = f.con(id, name, &[]);
            let d = desc(f.build(t));
            assert_eq!(d.node(d.root()), Some(&want), "{name}");
            assert_eq!(d.len(), 1, "{name} is one node");
        }
    }

    #[test]
    fn an_array_describes_its_element() {
        let mut f = Fixture::new();
        let int = f.int();
        let arr = f.con(ARRAY, "Array", &[int]);
        let d = desc(f.build(arr));
        let Some(&Node::Array(e)) = d.node(d.root()) else {
            panic!("root is not an Array: {:?}", d.node(d.root()))
        };
        assert_eq!(d.node(e), Some(&Node::Int));
    }

    #[test]
    fn a_map_describes_key_and_value_separately() {
        let mut f = Fixture::new();
        let int = f.int();
        let s = f.string();
        let m = f.con(MAP, "Map", &[s, int]);
        let d = desc(f.build(m));
        let Some(&Node::Map(k, v)) = d.node(d.root()) else {
            panic!("root is not a Map: {:?}", d.node(d.root()))
        };
        assert_eq!(d.node(k), Some(&Node::String));
        assert_eq!(d.node(v), Some(&Node::Int));
    }

    #[test]
    fn a_tuple_describes_its_elements_in_order() {
        let mut f = Fixture::new();
        let int = f.int();
        let s = f.string();
        let t = f.pool.mk_tuple(&[int, s]);
        let d = desc(f.build(t));
        let Some(Node::Tuple(es)) = d.node(d.root()) else {
            panic!("root is not a Tuple: {:?}", d.node(d.root()))
        };
        let es = es.clone();
        assert_eq!(es.len(), 2);
        assert_eq!(d.node(es[0]), Some(&Node::Int));
        assert_eq!(d.node(es[1]), Some(&Node::String));
    }

    #[test]
    fn a_data_type_describes_one_variant_per_constructor() {
        let mut f = Fixture::new();
        let int = f.int();
        let colour = TypeId(20);
        let red = f.ctor(colour, "Colour", 0, "Red", &[]);
        let shade = f.ctor(colour, "Colour", 1, "Shade", &[("level", int)]);
        f.data(colour, vec![red, shade]);

        let t = f.con(colour, "Colour", &[]);
        let d = desc(f.build(t));
        let Some(Node::Data(vs)) = d.node(d.root()) else {
            panic!("root is not Data: {:?}", d.node(d.root()))
        };
        let vs = vs.clone();
        assert_eq!(vs.len(), 2);
        assert_eq!(vs[0].variant.variant_idx, 0);
        assert!(vs[0].fields.is_empty(), "Red is nullary");
        assert_eq!(vs[1].variant.variant_idx, 1);
        assert_eq!(vs[1].fields.len(), 1);
        assert_eq!(d.node(vs[1].fields[0].node), Some(&Node::Int));
    }

    /// A closure's captures are not fixed by its type, so each is described
    /// inline at encode time: the type is a node over its signature, and the
    /// signature is the whole of what the descriptor holds.
    #[test]
    fn a_function_describes_as_a_closure_over_its_signature() {
        let mut f = Fixture::new();
        let int = f.int();
        let s = f.string();
        let fun = f.pool.mk_fun(&[int, s], int);
        let d = desc(f.build(fun));
        let Some(Node::Closure(params, ret)) = d.node(d.root()) else {
            panic!("root is not a Closure: {:?}", d.node(d.root()))
        };
        let (params, ret) = (params.clone(), *ret);
        assert_eq!(params.len(), 2);
        assert_eq!(d.node(params[0]), Some(&Node::Int));
        assert_eq!(d.node(params[1]), Some(&Node::String));
        assert_eq!(d.node(ret), Some(&Node::Int));
        assert_eq!(params[0], ret, "Int is one node, referenced twice");
        assert_eq!(d.len(), 3, "Int, String and the closure");

        // Interned on its signature, as a tuple is on its elements.
        let again = f.pool.mk_fun(&[int, s], int);
        let pair = f.pool.mk_tuple(&[fun, again]);
        let d = desc(f.build(pair));
        assert_eq!(d.len(), 4, "one closure node for two spellings of it");
    }

    /// The signature is walked the way a tuple's elements are: a parameter or
    /// return that cannot cross refuses, with a path step naming which. The
    /// unknown type is the one refusal left to reach them through.
    #[test]
    fn a_closure_refuses_through_its_parameter_and_return_with_a_path() {
        let mut f = Fixture::new();
        let int = f.int();
        let a = f.bound(0);

        let bad_param = f.pool.mk_fun(&[int, a], int);
        let at_param = refusal(f.build(bad_param));
        assert_eq!(at_param.reason, Reason::TypeVariable);
        assert_eq!(at_param.ty, a, "the refusal names the parameter's type");
        assert_eq!(at_param.path, vec![Step::Param(1)]);

        let bad_ret = f.pool.mk_fun(&[int], a);
        let at_ret = refusal(f.build(bad_ret));
        assert_eq!(at_ret.reason, Reason::TypeVariable);
        assert_eq!(at_ret.path, vec![Step::Return]);

        let cx = Decls {
            names: &f.names,
            type_names: &f.type_names,
            types: &f.types,
        };
        assert_eq!(at_param.path_text(&cx), "[param 1]");
        assert_eq!(at_ret.path_text(&cx), "[return]");

        // An uninhabited parameter is not a refusal: the closure describes,
        // with the node where the parameter is.
        let native = f.native();
        let phantom = f.pool.mk_fun(&[native], int);
        let d = desc(f.build(phantom));
        let Some(Node::Closure(params, _)) = d.node(d.root()) else {
            panic!("root is not a Closure: {:?}", d.node(d.root()))
        };
        assert_eq!(d.node(params[0]), Some(&Node::Uninhabited));
    }

    // --- the encodability table, refusing direction ---

    #[test]
    fn a_type_variable_is_refused() {
        let mut f = Fixture::new();
        let a = f.bound(0);
        let e = refusal(f.build(a));
        assert_eq!(e.reason, Reason::TypeVariable);
        assert!(
            e.path.is_empty(),
            "the described type is itself the refusal"
        );
    }

    /// A bodiless type that is none of the five handles: no expression builds
    /// a value of it and no table hands one back, so it is a node no value
    /// reaches rather than a refusal. Two such types are one node, as two
    /// `Int`s are: the fingerprint excludes names and there is no structure
    /// to tell them apart.
    #[test]
    fn a_bodiless_type_that_is_not_a_handle_describes_as_uninhabited() {
        let mut f = Fixture::new();
        let native = f.native();
        let d = desc(f.build(native));
        assert_eq!(d.node(d.root()), Some(&Node::Uninhabited));
        assert_eq!(d.len(), 1);

        let other = TypeId(30);
        f.types.insert(other, Nominal::Uninhabited);
        let other_t = f.con(other, "Other", &[]);
        let pair = f.pool.mk_tuple(&[native, other_t]);
        let d = desc(f.build(pair));
        assert_eq!(d.len(), 2, "one uninhabited node and the tuple");
    }

    /// `Tagged(Native)` over an `Int` field: the descriptor builds, the
    /// argument is a node in the table, and the record's one field is the
    /// `Int`. This is the program the old refusal turned away — its bytes
    /// are the `Int`'s, and the argument is never written or read.
    #[test]
    fn a_record_over_a_phantom_uninhabited_argument_builds_a_descriptor() {
        let mut f = Fixture::new();
        let int = f.int();
        let native = f.native();
        let tagged = TypeId(31);
        let c = f.ctor(tagged, "Tagged", 0, "Tagged", &[("value", int)]);
        f.data(tagged, vec![c]);
        let t = f.con(tagged, "Tagged", &[native]);
        let d = desc(f.build(t));
        let Some(Node::Data(vs)) = d.node(d.root()) else {
            panic!("root is not Data: {:?}", d.node(d.root()))
        };
        assert_eq!(vs.len(), 1);
        assert_eq!(vs[0].fields.len(), 1);
        assert_eq!(d.node(vs[0].fields[0].node), Some(&Node::Int));
        assert_eq!(d.len(), 3, "Uninhabited, the record and Int");
        assert!(
            d.nodes.contains(&Node::Uninhabited),
            "the argument is in the table, on no path the walk takes"
        );

        // And it is its own shape: `Tagged(Native)` is not `Tagged(Int)`.
        let at_int = f.con(tagged, "Tagged", &[int]);
        assert_ne!(d.fingerprint(), desc(f.build(at_int)).fingerprint());
    }

    /// A handle is a leaf node of its kind, one per kind per type-argument
    /// list — `Pid` twice is one node, like `Int` twice is.
    #[test]
    fn a_handle_type_describes_as_an_identity_of_its_kind() {
        let mut f = Fixture::new();
        let pid = TypeId(30);
        f.types.insert(pid, Nominal::Handle(HandleKind::Pid));
        let t = f.con(pid, "Pid", &[]);
        let d = desc(f.build(t));
        assert_eq!(
            d.node(d.root()),
            Some(&Node::Identity(HandleKind::Pid, Vec::new()))
        );
        assert_eq!(d.len(), 1);

        let again = f.con(pid, "Pid", &[]);
        let pair = f.pool.mk_tuple(&[t, again]);
        let d = desc(f.build(pair));
        assert_eq!(d.len(), 2, "one Pid node and the tuple");
    }

    /// `Subject(msg)`'s argument is described, for the reason a phantom
    /// parameter is: it is part of the shape, and a `Subject` of something
    /// unencodable is refused at that something.
    #[test]
    fn a_handle_describes_its_type_arguments_and_refuses_through_them() {
        let mut f = Fixture::new();
        let subj = TypeId(31);
        f.types.insert(subj, Nominal::Handle(HandleKind::Subject));
        let s = f.string();
        let t = f.con(subj, "Subject", &[s]);
        let d = desc(f.build(t));
        let Some(Node::Identity(HandleKind::Subject, args)) = d.node(d.root()) else {
            panic!("root is not a Subject identity: {:?}", d.node(d.root()))
        };
        assert_eq!(args.len(), 1);
        assert_eq!(d.node(args[0]), Some(&Node::String));

        let a = f.bound(0);
        let t = f.con(subj, "Subject", &[a]);
        let e = refusal(f.build(t));
        assert_eq!(e.reason, Reason::TypeVariable);
        assert!(
            e.path.is_empty(),
            "a handle is a leaf: no path step for its argument"
        );
    }

    /// `Decimal { units Int, scale Int }` is `opaque` in `scarlet/decimal`, and
    /// this is the shape it reaches the builder in from ANY module: a
    /// [`Nominal::Data`] carries its constructors and nothing about who may
    /// call them, so a refusal on visibility is not expressible here. What
    /// this witnesses is the descriptor that results — one constructor, both
    /// fields — and that it is the same one the declaring module gets. The
    /// end-to-end half, a real `Decimal` crossing from outside
    /// `scarlet/decimal`, is `type_errors.rs::opaque_from_another_module`.
    #[test]
    fn an_opaque_type_is_described_by_its_constructors_from_any_module() {
        let mut f = Fixture::new();
        let int = f.int();
        let dec = TypeId(31);
        let c = f.ctor(
            dec,
            "Decimal",
            0,
            "Decimal",
            &[("units", int), ("scale", int)],
        );
        f.data(dec, vec![c]);
        let t = f.con(dec, "Decimal", &[]);
        let d = desc(f.build(t));
        let Some(Node::Data(vs)) = d.node(d.root()) else {
            panic!("an opaque record must be described as a Data node");
        };
        assert_eq!(vs.len(), 1, "one constructor");
        assert_eq!(vs[0].fields.len(), 2, "both fields walked");
    }

    /// `Socket { conn Connection, peer Address }` is a public record over a
    /// host-backed field: the field is an identity node and the record is a
    /// `Data` node over it, so the whole record crosses.
    #[test]
    fn a_public_struct_over_a_handle_field_describes_the_field_as_an_identity() {
        let mut f = Fixture::new();
        let conn_id = TypeId(40);
        f.types
            .insert(conn_id, Nominal::Handle(HandleKind::Connection));
        let conn = f.con(conn_id, "Connection", &[]);

        let sock = TypeId(41);
        let c = f.ctor(sock, "Socket", 0, "Socket", &[("conn", conn)]);
        f.data(sock, vec![c]);

        let t = f.con(sock, "Socket", &[]);
        let d = desc(f.build(t));
        let Some(Node::Data(vs)) = d.node(d.root()) else {
            panic!("root is not Data: {:?}", d.node(d.root()))
        };
        assert_eq!(vs.len(), 1);
        assert_eq!(vs[0].fields.len(), 1);
        assert_eq!(
            d.node(vs[0].fields[0].node),
            Some(&Node::Identity(HandleKind::Connection, Vec::new()))
        );
    }

    // --- instantiation ---

    #[test]
    fn the_same_type_at_two_instantiations_gets_two_descriptors() {
        let mut f = Fixture::new();
        let a = f.bound(0);
        let opt = TypeId(50);
        let none = f.ctor(opt, "Option", 0, "None", &[]);
        let some = f.ctor(opt, "Option", 1, "Some", &[("value", a)]);
        f.data(opt, vec![none, some]);

        let int = f.int();
        let s = f.string();
        let opt_int = f.con(opt, "Option", &[int]);
        let opt_str = f.con(opt, "Option", &[s]);

        let di = desc(f.build(opt_int));
        let ds = desc(f.build(opt_str));

        let field_node = |d: &Desc| {
            let Some(Node::Data(vs)) = d.node(d.root()) else {
                panic!("not Data")
            };
            let n = vs[1].fields[0].node;
            d.node(n).cloned()
        };
        assert_eq!(field_node(&di), Some(Node::Int));
        assert_eq!(field_node(&ds), Some(Node::String));
        assert_ne!(di, ds, "Option(Int) and Option(String) are different types");
    }

    #[test]
    fn a_nested_generic_instantiates_through_the_spine() {
        let mut f = Fixture::new();
        let a = f.bound(0);
        let opt = TypeId(50);
        let none = f.ctor(opt, "Option", 0, "None", &[]);
        let some = f.ctor(opt, "Option", 1, "Some", &[("value", a)]);
        f.data(opt, vec![none, some]);

        let int = f.int();
        let arr_int = f.con(ARRAY, "Array", &[int]);
        let t = f.con(opt, "Option", &[arr_int]);

        let d = desc(f.build(t));
        let Some(Node::Data(vs)) = d.node(d.root()) else {
            panic!("not Data")
        };
        let inner = vs[1].fields[0].node;
        let Some(&Node::Array(e)) = d.node(inner) else {
            panic!("Some.value is not an Array: {:?}", d.node(inner))
        };
        assert_eq!(d.node(e), Some(&Node::Int));
    }

    // --- recursion and interning ---

    #[test]
    fn a_recursive_type_points_back_at_its_own_node() {
        let mut f = Fixture::new();
        let a = f.bound(0);
        let list = TypeId(60);
        let list_a = f.con(list, "List", &[a]);
        let nil = f.ctor(list, "List", 0, "Nil", &[]);
        let cons = f.ctor(list, "List", 1, "Cons", &[("head", a), ("tail", list_a)]);
        f.data(list, vec![nil, cons]);

        let int = f.int();
        let list_int = f.con(list, "List", &[int]);
        let d = desc(f.build(list_int));

        let Some(Node::Data(vs)) = d.node(d.root()) else {
            panic!("not Data")
        };
        let tail = vs[1].fields[1].node;
        assert_eq!(
            tail,
            d.root(),
            "Cons.tail must be the List(Int) node itself, not a copy"
        );
        // Two nodes only: Int and List(Int). An unrolled descriptor would grow
        // without bound.
        assert_eq!(d.len(), 2);
    }

    #[test]
    fn identical_types_share_one_node() {
        let mut f = Fixture::new();
        let int = f.int();
        // Two distinct RTys that denote the same type: the pool does not
        // hash-cons, so this is the case interning has to catch.
        let int_again = f.int();
        assert_ne!(int, int_again, "the pool minted two nodes");
        let t = f.pool.mk_tuple(&[int, int_again]);
        let d = desc(f.build(t));
        let Some(Node::Tuple(es)) = d.node(d.root()) else {
            panic!("not a Tuple")
        };
        assert_eq!(es[0], es[1], "one Int node, referenced twice");
        assert_eq!(d.len(), 2, "Int and the tuple");
    }

    // --- the path a refusal carries ---
    //
    // The unknown type is the one refusal, so it is what every path here is
    // reached through, and the diagnostic the path serves is "where the
    // unknown is". One limit, stated so nobody looks for the program that
    // reaches it: `data` walks a type's ARGUMENTS before its
    // fields, and a field can only hold a variable the type was applied to,
    // so a real compile refuses `Outer(a)` at the argument with no path. A
    // `Field` step is therefore reached only by this fixture, which can
    // declare a non-generic constructor over a bound variable; the
    // positional steps — element, key, value, tuple, parameter, return — are
    // the ones a program produces (`type_errors.rs`, `wire_descriptor.rs`).

    #[test]
    fn a_refusal_three_levels_down_names_the_full_field_path() {
        let mut f = Fixture::new();
        let bad = f.bound(0);

        let inner = TypeId(70);
        let ic = f.ctor(inner, "Inner", 0, "Inner", &[("f", bad)]);
        f.data(inner, vec![ic]);
        let inner_t = f.con(inner, "Inner", &[]);

        let middle = TypeId(71);
        let mc = f.ctor(middle, "Middle", 0, "Middle", &[("inner", inner_t)]);
        f.data(middle, vec![mc]);
        let middle_t = f.con(middle, "Middle", &[]);

        let outer = TypeId(72);
        let oc = f.ctor(outer, "Outer", 0, "Outer", &[("mid", middle_t)]);
        f.data(outer, vec![oc]);
        let outer_t = f.con(outer, "Outer", &[]);

        let e = refusal(f.build(outer_t));
        assert_eq!(e.reason, Reason::TypeVariable);
        assert_eq!(e.ty, bad, "the refusal names the offending sub-type");

        let cx = Decls {
            names: &f.names,
            type_names: &f.type_names,
            types: &f.types,
        };
        assert_eq!(e.path_text(&cx), "Outer.mid -> Middle.inner -> Inner.f");
    }

    #[test]
    fn a_refusal_inside_a_container_names_the_position() {
        let mut f = Fixture::new();
        let int = f.int();
        let bad = f.bound(0);
        let arr = f.con(ARRAY, "Array", &[bad]);
        let m = f.con(MAP, "Map", &[int, arr]);
        let t = f.pool.mk_tuple(&[int, m]);

        let e = refusal(f.build(t));
        assert_eq!(e.reason, Reason::TypeVariable);
        let cx = Decls {
            names: &f.names,
            type_names: &f.type_names,
            types: &f.types,
        };
        assert_eq!(e.path_text(&cx), "[1] -> [value] -> [element]");
    }

    /// Two constructors of one type may each have a field called `value`, so
    /// the path names the variant as well as the label.
    #[test]
    fn a_path_step_names_its_variant() {
        let mut f = Fixture::new();
        let int = f.int();
        let bad = f.bound(0);
        let e_id = TypeId(80);
        let good = f.ctor(e_id, "Either", 0, "Left", &[("value", int)]);
        let evil = f.ctor(e_id, "Either", 1, "Right", &[("value", bad)]);
        f.data(e_id, vec![good, evil]);
        let t = f.con(e_id, "Either", &[]);

        let e = refusal(f.build(t));
        let cx = Decls {
            names: &f.names,
            type_names: &f.type_names,
            types: &f.types,
        };
        assert_eq!(e.path_text(&cx), "Right.value");
    }

    // --- aliases ---

    #[test]
    fn an_alias_is_looked_through_and_adds_no_path_step() {
        let mut f = Fixture::new();
        let int = f.int();
        let name = TypeId(90);
        f.types.insert(name, Nominal::Alias(int));
        let t = f.con(name, "Name", &[]);
        let d = desc(f.build(t));
        assert_eq!(d.node(d.root()), Some(&Node::Int));
    }

    /// `type Id(a) = a` at an unknown `a` refuses at the target; an alias to
    /// an uninhabited type is looked through to the node, as an alias to
    /// anything else is to its target.
    #[test]
    fn an_alias_to_an_unencodable_type_refuses_at_the_target() {
        let mut f = Fixture::new();
        let a = f.bound(0);
        let id = TypeId(91);
        f.types.insert(id, Nominal::Alias(a));
        let t = f.con(id, "Id", &[a]);
        let e = refusal(f.build(t));
        assert_eq!(e.reason, Reason::TypeVariable);
        assert!(e.path.is_empty(), "an alias is a spelling, not a level");

        let native = f.native();
        let handle = TypeId(92);
        f.types.insert(handle, Nominal::Alias(native));
        let t = f.con(handle, "Handle", &[]);
        let d = desc(f.build(t));
        assert_eq!(d.node(d.root()), Some(&Node::Uninhabited));
    }

    /// An alias to a function type is looked through to a closure node, as
    /// an alias to anything else is to its target.
    #[test]
    fn an_alias_to_a_function_type_describes_as_a_closure() {
        let mut f = Fixture::new();
        let int = f.int();
        let fun = f.pool.mk_fun(&[int], int);
        let handler = TypeId(91);
        f.types.insert(handler, Nominal::Alias(fun));
        let t = f.con(handler, "Handler", &[]);
        let d = desc(f.build(t));
        assert!(
            matches!(d.node(d.root()), Some(Node::Closure(..))),
            "{:?}",
            d.node(d.root())
        );
    }

    #[test]
    fn a_generic_alias_instantiates_its_target() {
        let mut f = Fixture::new();
        let a = f.bound(0);
        let arr_a = f.con(ARRAY, "Array", &[a]);
        let bag = TypeId(92);
        f.types.insert(bag, Nominal::Alias(arr_a));
        let s = f.string();
        let t = f.con(bag, "Bag", &[s]);
        let d = desc(f.build(t));
        let Some(&Node::Array(e)) = d.node(d.root()) else {
            panic!("not an Array")
        };
        assert_eq!(d.node(e), Some(&Node::String));
    }

    // --- the fingerprint ---

    /// Declare `type <ty_name> { ctors... }` at `id` and return its
    /// fingerprint.
    ///
    /// Every test below is two calls to this with **exactly one argument
    /// different**, so what moved the fingerprint is named by the call and not
    /// by a comment. Each property gets its own test: a single "different
    /// shapes differ" assertion would pass on a fingerprint that ignored names
    /// entirely.
    fn fp(f: &mut Fixture, id: TypeId, ty_name: &str, ctors: &[(&str, &[(&str, RTy)])]) -> u64 {
        let mut decls = Vec::new();
        for (i, (name, fields)) in ctors.iter().enumerate() {
            decls.push(f.ctor(id, ty_name, i as u16, name, fields));
        }
        f.data(id, decls);
        let t = f.con(id, ty_name, &[]);
        desc(f.build(t)).fingerprint()
    }

    /// The shape the sensitivity tests vary away from:
    /// `type Colour { Red, Shade(level Int) }`.
    fn colour(f: &mut Fixture, id: TypeId, ty_name: &str) -> u64 {
        let int = f.int();
        fp(
            f,
            id,
            ty_name,
            &[("Red", &[]), ("Shade", &[("level", int)])],
        )
    }

    const C1: TypeId = TypeId(100);
    const C2: TypeId = TypeId(101);

    /// The stability half of the DONE-WHEN, one layer below `dis`: the
    /// fingerprint may not depend on how the names happened to be interned.
    ///
    /// This is what a two-compile `dis` test would catch and the only part of
    /// it reachable before elaboration emits the constant (ticket 7). A second
    /// compile meets identifiers in a different order, so `StrId(4)` is a
    /// different string in each — a fingerprint folding `StrId.0` rather than
    /// the name text passes every other test in this file and fails here.
    #[test]
    fn the_fingerprint_survives_a_different_interning_order() {
        let mut a = Fixture::new();
        let fa = colour(&mut a, C1, "Colour");

        let mut b = Fixture::new();
        for junk in ["parse", "render", "Shade", "level", "Red"] {
            b.intern(junk);
        }
        let fb = colour(&mut b, C1, "Colour");

        // Without this the test is vacuous: if both compiles gave the names the
        // same ids, a StrId-folding fingerprint would pass it too.
        assert_ne!(
            a.intern("Shade"),
            b.intern("Shade"),
            "premise: the two compiles must disagree about Shade's StrId"
        );
        assert_eq!(fa, fb, "the same shape, interned differently");
    }

    #[test]
    fn the_fingerprint_is_the_same_on_a_second_build_of_one_type() {
        let mut f = Fixture::new();
        assert_eq!(colour(&mut f, C1, "Colour"), colour(&mut f, C1, "Colour"));
    }

    #[test]
    fn renaming_a_constructor_changes_the_fingerprint() {
        let mut f = Fixture::new();
        let int = f.int();
        let before = fp(&mut f, C1, "Colour", &[("Shade", &[("level", int)])]);
        let after = fp(&mut f, C1, "Colour", &[("Tint", &[("level", int)])]);
        assert_ne!(before, after);
    }

    #[test]
    fn reordering_variants_changes_the_fingerprint() {
        let mut f = Fixture::new();
        let int = f.int();
        let before = fp(
            &mut f,
            C1,
            "Colour",
            &[("Red", &[]), ("Shade", &[("level", int)])],
        );
        let after = fp(
            &mut f,
            C1,
            "Colour",
            &[("Shade", &[("level", int)]), ("Red", &[])],
        );
        assert_ne!(before, after);
    }

    #[test]
    fn adding_a_field_changes_the_fingerprint() {
        let mut f = Fixture::new();
        let int = f.int();
        let before = fp(&mut f, C1, "Colour", &[("Shade", &[("level", int)])]);
        let after = fp(
            &mut f,
            C1,
            "Colour",
            &[("Shade", &[("level", int), ("alpha", int)])],
        );
        assert_ne!(before, after);
    }

    #[test]
    fn changing_a_field_type_changes_the_fingerprint() {
        let mut f = Fixture::new();
        let int = f.int();
        let s = f.string();
        let before = fp(&mut f, C1, "Colour", &[("Shade", &[("level", int)])]);
        let after = fp(&mut f, C1, "Colour", &[("Shade", &[("level", s)])]);
        assert_ne!(before, after);
    }

    #[test]
    fn renaming_a_field_changes_the_fingerprint() {
        let mut f = Fixture::new();
        let int = f.int();
        let before = fp(&mut f, C1, "Colour", &[("Shade", &[("level", int)])]);
        let after = fp(&mut f, C1, "Colour", &[("Shade", &[("lvl", int)])]);
        assert_ne!(before, after);
    }

    /// The type's own name is not part of its shape, so renaming it is not a
    /// schema change and peers keep talking.
    #[test]
    fn renaming_the_type_does_not_change_the_fingerprint() {
        let mut f = Fixture::new();
        let before = colour(&mut f, C1, "Colour");
        let after = colour(&mut f, C1, "Hue");
        assert_eq!(before, after);
    }

    /// Moving a declaration to another module mints a new `TypeId`. The
    /// fingerprint must not notice — this is the property that keeps a module
    /// reshuffle from reading as a wire break at every peer.
    #[test]
    fn moving_the_type_to_another_module_does_not_change_the_fingerprint() {
        let mut f = Fixture::new();
        let before = colour(&mut f, C1, "Colour");
        let after = colour(&mut f, C2, "Colour");
        assert_eq!(before, after);
    }

    /// The interoperability claim in the module doc, asserted: two programs
    /// that never shared a declaration agree, because they agree on the shape.
    #[test]
    fn the_same_shape_declared_twice_gets_one_fingerprint() {
        let mut mine = Fixture::new();
        let a = colour(&mut mine, C1, "Colour");

        let mut theirs = Fixture::new();
        let b = colour(&mut theirs, C2, "Paint");

        assert_eq!(a, b, "same constructors, same labels, same field types");
    }

    /// The headline claim of the algorithm: children are folded as indices, so
    /// a cyclic descriptor terminates — a fingerprint that descended into
    /// `Cons.tail` would not return from this test at all.
    ///
    /// Terminating is half of it. The other half is that the element type is
    /// still reached, which `List(Int)` against `List(String)` is what asserts:
    /// a hash that stopped at the cycle would give these two one value.
    #[test]
    fn a_recursive_type_fingerprints_without_recursing() {
        let mut f = Fixture::new();
        let a = f.bound(0);
        let list = TypeId(110);
        let list_a = f.con(list, "List", &[a]);
        let nil = f.ctor(list, "List", 0, "Nil", &[]);
        let cons = f.ctor(list, "List", 1, "Cons", &[("head", a), ("tail", list_a)]);
        f.data(list, vec![nil, cons]);

        let int = f.int();
        let s = f.string();
        let li = f.con(list, "List", &[int]);
        let ls = f.con(list, "List", &[s]);

        let fi = desc(f.build(li)).fingerprint();
        assert_eq!(fi, desc(f.build(li)).fingerprint(), "stable");
        assert_ne!(
            fi,
            desc(f.build(ls)).fingerprint(),
            "List(Int) vs List(String)"
        );
    }

    /// The fingerprint excludes names, so the kind byte is what keeps the
    /// handle types apart — a fold of the tag alone would make `decode` at
    /// `Pid` accept `Connection` bytes — and the argument fold is what keeps
    /// `Subject(Int)` from being a typed door onto a `Subject(String)`
    /// mailbox. Each pair differs in exactly one thing.
    #[test]
    fn each_handle_kind_and_argument_gets_its_own_fingerprint() {
        let mut f = Fixture::new();
        let (pid, subj, conn) = (TypeId(30), TypeId(31), TypeId(32));
        f.types.insert(pid, Nominal::Handle(HandleKind::Pid));
        f.types.insert(subj, Nominal::Handle(HandleKind::Subject));
        f.types
            .insert(conn, Nominal::Handle(HandleKind::Connection));
        let s = f.string();
        let i = f.int();
        let fp_of = |f: &mut Fixture, t: RTy| desc(f.build(t)).fingerprint();

        let pid_t = f.con(pid, "Pid", &[]);
        let conn_t = f.con(conn, "Connection", &[]);
        let subj_s = f.con(subj, "Subject", &[s]);
        let subj_i = f.con(subj, "Subject", &[i]);

        let (fp_pid, fp_conn) = (fp_of(&mut f, pid_t), fp_of(&mut f, conn_t));
        let (fp_s, fp_i) = (fp_of(&mut f, subj_s), fp_of(&mut f, subj_i));
        assert_ne!(
            fp_pid, fp_conn,
            "Pid vs Connection: same arguments, other kind"
        );
        assert_ne!(fp_pid, fp_s, "Pid vs Subject(String)");
        assert_ne!(
            fp_s, fp_i,
            "Subject(String) vs Subject(Int): same kind, other argument"
        );
        assert_eq!(fp_pid, fp_of(&mut f, pid_t), "stable");
    }

    /// A phantom parameter still has to be encodable: encodability is a
    /// property of the type, not of which parameters its constructors read.
    /// `Phantom(a)` with `a` unknown refuses at `a`, with no path step — an
    /// argument is not a field, and it is walked before any field is.
    #[test]
    fn an_unused_type_argument_is_still_described() {
        let mut f = Fixture::new();
        let int = f.int();
        let a = f.bound(0);
        let ph = TypeId(93);
        let c = f.ctor(ph, "Phantom", 0, "Phantom", &[("n", int)]);
        f.data(ph, vec![c]);
        let t = f.con(ph, "Phantom", &[a]);
        let e = refusal(f.build(t));
        assert_eq!(e.reason, Reason::TypeVariable);
        assert!(e.path.is_empty());
    }

    /// The fingerprint excludes names, so the signature is what keeps
    /// function types apart: a fold of the tag alone would make `decode` at
    /// `fn(Int) Int` accept bytes written at `fn(String) Int`, and the
    /// decoded closure would be a typed door onto a body expecting the other
    /// argument. Each pair differs in exactly one thing.
    #[test]
    fn each_function_signature_gets_its_own_fingerprint() {
        let mut f = Fixture::new();
        let int = f.int();
        let s = f.string();
        let nil_id = TypeId(94);
        let nil_c = f.ctor(nil_id, "Nil", 0, "Nil", &[]);
        f.data(nil_id, vec![nil_c]);
        let nil = f.con(nil_id, "Nil", &[]);
        let fp_of = |f: &mut Fixture, t: RTy| desc(f.build(t)).fingerprint();

        let int_int = f.pool.mk_fun(&[int], int);
        let str_int = f.pool.mk_fun(&[s], int);
        let int_str = f.pool.mk_fun(&[int], s);
        let two_int = f.pool.mk_fun(&[int, int], int);
        let nullary_nil = f.pool.mk_fun(&[], nil);

        let (a, b) = (fp_of(&mut f, int_int), fp_of(&mut f, str_int));
        assert_ne!(a, b, "fn(Int) Int vs fn(String) Int: other parameter");
        assert_ne!(
            a,
            fp_of(&mut f, int_str),
            "fn(Int) Int vs fn(Int) String: other return"
        );
        assert_ne!(
            a,
            fp_of(&mut f, two_int),
            "fn(Int) Int vs fn(Int, Int) Int: other count"
        );
        assert_ne!(a, fp_of(&mut f, nullary_nil), "fn(Int) Int vs fn() Nil");
        assert_ne!(
            a,
            fp_of(&mut f, int),
            "fn(Int) Int vs Int: a closure is not its return"
        );
        assert_eq!(a, fp_of(&mut f, int_int), "stable");

        // The sharp case: behind a tuple that interns Int and String first,
        // `fn(Int) String` and `fn(String) Int` have IDENTICAL node tables
        // and differ only in which index is a parameter and which the
        // return. A fold of the closure's tag and count alone — without its
        // indices — gives these two one fingerprint.
        let to_str = f.pool.mk_fun(&[int], s);
        let to_int = f.pool.mk_fun(&[s], int);
        let t1 = f.pool.mk_tuple(&[int, s, to_str]);
        let t2 = f.pool.mk_tuple(&[int, s, to_int]);
        assert_eq!(
            desc(f.build(t1)).len(),
            desc(f.build(t2)).len(),
            "premise: the two tables are the same size"
        );
        assert_ne!(
            fp_of(&mut f, t1),
            fp_of(&mut f, t2),
            "(Int, String, fn(Int) String) vs (Int, String, fn(String) Int): same table, other indices"
        );
    }

    /// The fingerprint excludes names, so the tag is what keeps an
    /// uninhabited type apart from the scalars and from `Nil` — a
    /// one-constructor `Data` type that also occupies zero bytes on the
    /// wire. Same table size, other tag.
    #[test]
    fn an_uninhabited_type_gets_its_own_fingerprint() {
        let mut f = Fixture::new();
        let native = f.native();
        let int = f.int();
        let nil_id = TypeId(94);
        let nil_c = f.ctor(nil_id, "Nil", 0, "Nil", &[]);
        f.data(nil_id, vec![nil_c]);
        let nil = f.con(nil_id, "Nil", &[]);
        let fp_of = |f: &mut Fixture, t: RTy| desc(f.build(t)).fingerprint();

        let fp_native = fp_of(&mut f, native);
        assert_ne!(fp_native, fp_of(&mut f, int), "Native vs Int");
        assert_ne!(
            fp_native,
            fp_of(&mut f, nil),
            "Native vs Nil: both zero bytes, other tag"
        );
        assert_eq!(fp_native, fp_of(&mut f, native), "stable");
    }

    /// The number itself, computed from `wire.scrl`'s specification by hand
    /// rather than through `mix`: the basis mixed with version 3, then one
    /// node, then root 0, then tag 14 alone. A second implementation
    /// produces this, and `wire_uninhabited.rs` writes it into a header
    /// from Scarlet and decodes against it — so the two instruments agree.
    #[test]
    fn the_uninhabited_fingerprint_is_the_tag_alone() {
        let mut f = Fixture::new();
        let native = f.native();
        assert_eq!(desc(f.build(native)).fingerprint(), 0xf5c9_9787_f8eb_c4ff);
    }

    /// The diagnostic the path serves: at the root it is an instruction, and
    /// through a path it names the unknown and where it is.
    #[test]
    fn a_type_variable_through_a_path_is_reported_with_the_path() {
        let mut f = Fixture::new();
        let int = f.int();
        let a = f.bound(0);
        let t = f.pool.mk_tuple(&[int, a]);
        let through = refusal(f.build(t));
        let root = refusal(f.build(a));
        let cx = Decls {
            names: &f.names,
            type_names: &f.type_names,
            types: &f.types,
        };
        assert_eq!(
            through.message(&cx, &f.pool, WireOp::Decode),
            "`wire.decode` cannot decode `a`: the type is still polymorphic here, so its \
             representation is not known (through [1])"
        );
        assert_eq!(
            root.message(&cx, &f.pool, WireOp::Decode),
            "the type `wire.decode` produces here is not known; annotate the binding"
        );
    }

    /// THE CRITERION, asserted over every reason rather than one at a time.
    ///
    /// A golden pins the text a refusal has today; this pins the property all
    /// of them must have, so a refusal added next year is covered without
    /// anyone remembering to widen a test. The property: **the criterion is
    /// reconstructibility, never the existence of a byte encoding.** Erlang
    /// writes funs (`NEW_FUN_EXT`) and pids (`NEW_PID_EXT`); they fail at
    /// the far end, not at the wire.
    ///
    /// WHAT THIS DOES NOT WITNESS, said plainly rather than implied: it cannot
    /// prove `ALL` lists every variant. The `match` below is exhaustive, so a
    /// new `Reason` stops this file compiling until someone edits here — but
    /// they could satisfy the compiler without extending `ALL`. The match is a
    /// tripwire that brings the author to this test, not a proof of coverage.
    #[test]
    fn no_refusal_claims_the_type_has_no_representation() {
        const ALL: [Reason; 1] = [Reason::TypeVariable];
        for r in ALL {
            match r {
                Reason::TypeVariable => {}
            }
            let text = r.describe();
            assert!(
                !text.contains("no representation"),
                "{r:?} claims the type has no representation, which is not the criterion: {text}"
            );
            assert!(
                !text.contains("cannot be represented"),
                "{r:?} claims the type cannot be represented: {text}"
            );
            assert!(
                !text.is_empty(),
                "{r:?} must carry prose a diagnostic can quote"
            );
        }
    }

    /// [`Desc::to_runtime`] carries the fingerprint and preserves every index.
    ///
    /// Asserted here, directly, because **a round trip cannot see either of
    /// them**. Dropping the fingerprint in the conversion was planted and
    /// `wire.encode`/`wire.decode` still agreed end to end: the encoder writes
    /// whatever it is handed into the header and the decoder compares against
    /// that same value, so the fault is symmetric across the two halves and
    /// cancels exactly. That is the checksum-through-the-mechanism-it-checks
    /// shape, and it is the concrete reason T-341 pins a *printed* encoding
    /// rather than only a round trip.
    ///
    /// The index half does show up in a round trip — shifting it by one takes
    /// the `wire.encode` walk red — but it is asserted here too, because that
    /// failure depends on the descriptor having nodes of different kinds and a
    /// table that happened to be uniform would hide it.
    #[test]
    fn to_runtime_carries_the_fingerprint_and_preserves_indices() {
        let d = Desc {
            nodes: vec![
                Node::Array(NodeIdx(1)),
                Node::Int,
                Node::Identity(HandleKind::Subject, vec![NodeIdx(1)]),
                Node::Closure(vec![NodeIdx(1), NodeIdx(1)], NodeIdx(2)),
                Node::Uninhabited,
            ],
            root: NodeIdx(0),
            fingerprint: 0xdead_beef_0bad_f00d,
        };
        // Structural equality rather than accessor-by-accessor: the runtime
        // descriptor's readers are crate-private to `scarlet_vm`, and widening
        // them so a test could call them would add public surface for nothing.
        // The identity's argument and the closure's signature are dropped:
        // they live in the fingerprint. The parameter count survives, for the
        // decoder's check against the function the bytes name.
        let want = RtDesc::new(
            vec![
                RtNode::Array(RtIdx(1)),
                RtNode::Int,
                RtNode::Identity(HandleKind::Subject),
                RtNode::Closure { arity: 2 },
                RtNode::Uninhabited,
            ],
            RtIdx(0),
            0xdead_beef_0bad_f00d,
        );
        assert_eq!(d.to_runtime(), want);
    }
}
