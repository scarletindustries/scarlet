//! The descriptor of a type: what `wire.encode` walks a value against and
//! `wire.decode` builds one from.
//!
//! This is **program data**, not interpreter code. A [`Program`] carries a
//! table of these and each [`Atom::Wire`] names one by index; the walks that
//! read them live in the VM.
//!
//! [`Program`]: crate::core_ir::Program
//! [`Atom::Wire`]: crate::core_ir::Atom::Wire
//!
//! # Why a descriptor the VM can read at all
//!
//! The compiler builds a richer one — `scarlet_core::typed_ir::wire::Desc` —
//! which carries type, constructor and field names. This one deliberately
//! carries **less**: the walks need structure, and a decoder rebuilds a
//! constructor through [`Program::types`], keyed on the identity a
//! [`VariantRef`] already has. A name in here would be a second identity for a
//! constructor that has one.
//!
//! [`Program::types`]: crate::core_ir::Program::types
//!
//! The fingerprint is carried rather than recomputed: it folds constructor and
//! field names as text, and this table holds none.

use crate::TypeId;

/// Index into a [`Desc`]'s node table. Only meaningful for the descriptor that
/// minted it.
///
/// Children are indices rather than nested nodes, which is what lets a
/// recursive type be a finite table: `List(Int)` is two nodes, and `Cons.tail`
/// is the index of the node it sits in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NodeIdx(pub u32);

/// One constructor a value of a [`Node::Data`] may be.
///
/// The position of a `Ctor` in its node's list **is** the tag the peer
/// receives, so reordering a type's constructors is a wire break. `variant_idx`
/// is the same number seen from the other side — a constructor's declaration
/// order within its type — and they coincide because the front end lists
/// constructors in declared order. Both are kept: the position is what goes on
/// the wire, and `(type_id, variant_idx)` is what a decoder looks the
/// constructor up by.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Ctor {
    pub type_id: TypeId,
    pub variant_idx: u16,
    /// Field types in declared order, which is the order the payload is
    /// written in.
    pub fields: Vec<NodeIdx>,
}

/// What a host-backed handle on the wire is a handle *to*. Written as one byte
/// after the run identity, and this enumeration is that byte: a **stable wire
/// surface**, so a variant keeps its number forever and a new one takes the
/// next free number. Deliberately not a [`TypeId`], which is minted per compile
/// and means nothing to a peer.
///
/// Sockets are not built yet. Whatever re-introduces them must give a socket's
/// own kind field these same numbers rather than mint a second set, or the byte
/// a value carries and the byte a descriptor names will disagree.
///
/// A descriptor names the kind its *static type* has. `Port` is the one kind no
/// static type names: `scarlet/os/port.Port`'s stream field is a `Connection`,
/// so its descriptor says `Connection` while the value's kind field says
/// `Port`. An encoder therefore writes the kind the VALUE carries and a decoder
/// accepts a `Port` byte under a `Connection` descriptor ([`Self::admits`]);
/// rebuilding from the descriptor's kind instead would hand back a handle the
/// port table does not hold.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum HandleKind {
    Connection = 0,
    Listener = 1,
    Port = 2,
    Tls = 3,
    Pid = 4,
    Subject = 5,
}

/// One node of a [`Desc`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Node {
    Int,
    Float,
    String,
    Binary,
    Array(NodeIdx),
    Map(NodeIdx, NodeIdx),
    Tuple(Vec<NodeIdx>),
    Data(Vec<Ctor>),
    /// A host-backed handle, written as the identity of the run that minted
    /// it, its kind and its number. The kind is the static type's; a
    /// `Subject`'s message type is folded into the fingerprint by the front end
    /// and is not needed here, since the bytes hold only the mailbox id.
    Identity(HandleKind),
    /// A function value. `arity` is the static type's parameter count: a
    /// decoder refuses a function that takes a different number, which is the
    /// one way a name that resolves could hand back a value of another type.
    /// The parameter and return types are folded into the fingerprint by the
    /// front end and are not needed here.
    ///
    /// The captures are not fixed by this type — every lambda of one signature
    /// shares it — so the walks read them against a descriptor the function
    /// itself carries, which is not built yet (`docs/wire-format.md`,
    /// "Closures").
    Closure {
        arity: u32,
    },
    /// A type no value has: a `pub type Name` with no body that is not one of
    /// the handle kinds. It can only stand in a descriptor where nothing is
    /// ever written or read — a phantom type argument, or a constructor field
    /// no value ever reaches — so an encoder has no value to meet it with, and
    /// a decoder steered into one has been handed bytes claiming a value that
    /// cannot exist.
    Uninhabited,
}

/// The descriptor of one type: a node table, the node the type itself is, and
/// the 64-bit hash of that shape.
///
/// Only what the compiler reads is exposed. The rest of the surface the byte
/// walks need — the fingerprint, the table's length, a handle kind from a byte
/// — arrives with the walk that reads it, so nothing here is public before
/// something uses it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Desc {
    nodes: Vec<Node>,
    root: NodeIdx,
    /// Travels with the descriptor because it cannot be recomputed from the
    /// other two: it folds constructor and field names as text, and this table
    /// holds no names. Two peers agree they are talking about the same shape by
    /// comparing these (`docs/wire-format.md`).
    fingerprint: u64,
}

impl Desc {
    /// Build a descriptor. The front end's converter is the only caller: it
    /// holds the richer `Desc` this mirrors and the context that resolved it.
    pub fn new(nodes: Vec<Node>, root: NodeIdx, fingerprint: u64) -> Desc {
        Desc {
            nodes,
            root,
            fingerprint,
        }
    }

    /// The node the described type itself is.
    pub fn root(&self) -> NodeIdx {
        self.root
    }

    /// The node at `i`. `None` only for an index from another descriptor, which
    /// the converter cannot produce.
    pub fn node(&self, i: NodeIdx) -> Option<&Node> {
        self.nodes.get(i.0 as usize)
    }
}
