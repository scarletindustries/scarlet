//! What a decision tree is made of: places in the matched value, and what is asked of
//! them.

use std::collections::BTreeMap;

use crate::program::InternalLinkError;
use crate::typed_ir::{RTy, TypedBinPatSeg, TypedBind};
use crate::{ConstId, FloatKey, VariantRef};

use super::R;

/// The decision tree of a match.
pub(crate) struct Tree<'p> {
    pub(crate) root: Node<'p>,
    pub(crate) places: Places,
    /// How many ways through the tree come to each arm.
    pub(crate) leaves: Vec<u32>,
    /// How many places in the tree go on at each [`RestId`].
    pub(crate) rests: Vec<u32>,
    /// How many [`Node::Any`]s there are.
    pub(crate) anys: usize,
}

/// Something in the matched value.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub(crate) struct Place(pub(super) u32);

impl Place {
    pub(crate) fn index(self) -> usize {
        self.0 as usize
    }
}

/// The matched value itself.
pub(crate) const ROOT: Place = Place(0);

/// Where rows are tried that come after others.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct RestId(pub(super) u32);

impl RestId {
    pub(crate) fn index(self) -> usize {
        self.0 as usize
    }
}

/// An or-pattern whose alternatives come together again.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct AnyId(pub(super) u32);

impl AnyId {
    pub(crate) fn index(self) -> usize {
        self.0 as usize
    }
}

/// What a place is in the place it is a part of.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub(crate) enum Part {
    /// A field of the constructor that is `variant` of its type.
    Field { variant: u16, index: u16 },
    /// An element of a tuple.
    Element(u16),
    /// How many elements an array has.
    Length,
    /// How many bits a binary has.
    Bits,
    /// Where the reading of a binary starts: 0, as the constant.
    Start(ConstId),
    /// What a binary that has `total` bits has from bit `cursor` on.
    From { cursor: Place, total: Place },
}

/// What a [`Node::Cut`] finds in an array.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub(crate) enum Piece {
    /// The element at this index.
    Element(u32),
    /// The array of what is behind the first so many elements.
    Tail(u32),
}

/// How the value of a place is come by.
#[derive(Debug, Clone, Copy)]
pub(crate) enum Source {
    /// It is there where the tree begins, or a node of the tree gives it.
    Given,
    /// It is read out of another.
    Part { of: Place, part: Part },
    /// A [`Node::Cut`] of another finds it.
    Found { of: Place, piece: Piece },
}

impl Source {
    /// The place that it is come by from.
    pub(super) fn of(self) -> Option<Place> {
        match self {
            Source::Given => None,
            Source::Part { of, .. } | Source::Found { of, .. } => Some(of),
        }
    }
}

/// Every place of a match.
#[derive(Debug, Default)]
pub(crate) struct Places {
    all: Vec<(Source, RTy)>,
    /// The parts of each place that there are, and the pieces.
    parts: Vec<BTreeMap<Part, Place>>,
    pieces: Vec<BTreeMap<Piece, Place>>,
}

impl Places {
    pub(crate) fn len(&self) -> usize {
        self.all.len()
    }

    pub(crate) fn get(&self, p: Place) -> Option<(Source, RTy)> {
        self.all.get(p.index()).copied()
    }

    pub(super) fn add(&mut self, source: Source, ty: RTy) -> R<Place> {
        let at = self.all.len();
        let p = u32::try_from(at).map_err(|_| InternalLinkError::IndexTooWide(at))?;
        self.all.push((source, ty));
        self.parts.push(BTreeMap::new());
        self.pieces.push(BTreeMap::new());
        Ok(Place(p))
    }

    pub(super) fn part(&mut self, of: Place, part: Part, ty: RTy) -> R<Place> {
        let known = self.parts.get(of.index());
        let known = known.ok_or(InternalLinkError::NoPlace(of.index()))?;
        if let Some(p) = known.get(&part) {
            return Ok(*p);
        }
        let p = self.add(Source::Part { of, part }, ty)?;
        if let Some(parts) = self.parts.get_mut(of.index()) {
            parts.insert(part, p);
        }
        Ok(p)
    }

    pub(super) fn piece(&mut self, of: Place, piece: Piece, ty: RTy) -> R<Place> {
        let known = self.pieces.get(of.index());
        let known = known.ok_or(InternalLinkError::NoPlace(of.index()))?;
        if let Some(p) = known.get(&piece) {
            return Ok(*p);
        }
        let p = self.add(Source::Found { of, piece }, ty)?;
        if let Some(pieces) = self.pieces.get_mut(of.index()) {
            pieces.insert(piece, p);
        }
        Ok(p)
    }

    /// The place of a piece that something asks for.
    pub(super) fn found(&self, of: Place, piece: Piece) -> Option<Place> {
        self.pieces.get(of.index())?.get(&piece).copied()
    }

    /// The places of the elements of `of` that something asks for.
    pub(super) fn elements(&self, of: Place) -> impl Iterator<Item = Place> {
        let pieces = self.pieces.get(of.index()).into_iter().flatten();
        pieces.filter_map(|(piece, p)| match piece {
            Piece::Element(_) => Some(*p),
            Piece::Tail(_) => None,
        })
    }

    /// The place that `p` is come by from.
    pub(super) fn parent(&self, p: Place) -> Option<Place> {
        self.get(p)?.0.of()
    }

    /// The part of `of` that `p` is, or is somewhere in.
    pub(super) fn part_of(&self, of: Place, mut p: Place) -> Option<Place> {
        loop {
            let parent = self.parent(p)?;
            if parent == of {
                return Some(p);
            }
            p = parent;
        }
    }
}

/// A value that a case is for. Those of one switch are of one kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum Key {
    Variant(VariantRef),
    Int(i64),
    Float(FloatKey),
    Text(ConstId),
}

/// Where a way through the tree goes on when nothing it has tried fits: at rows
/// that come after, or nowhere, which the checker has proved never happens.
pub(crate) type Fail = Option<RestId>;

/// What the tree does next.
#[derive(Debug)]
pub(crate) enum Node<'p> {
    /// No row is left.
    Nothing,
    /// Go on at the rows that are there.
    Rest(RestId),
    /// Every check of a row holds.
    Leaf(Leaf),
    /// Go on by what `on` is.
    Switch {
        on: Place,
        branches: Vec<Branch<'p>>,
        default: Option<Box<Node<'p>>>,
    },
    /// Ask for pieces of an array, and go on with `no` when it does not have the length.
    /// That is nothing when it is known to.
    Cut {
        cut: Box<Cut>,
        yes: Box<Node<'p>>,
        no: Box<Node<'p>>,
    },
    /// Read the length of an array, so that nothing in `then` need have the array to ask
    /// for it.
    Measure { length: Place, then: Box<Node<'p>> },
    /// Whether the Int `on` is at least the first constant and less than the second.
    Within {
        on: Place,
        range: [ConstId; 2],
        yes: Box<Node<'p>>,
        fail: Fail,
    },
    /// Read one segment of the binary `on`.
    Segment(Box<Segment<'p>>),
    /// Whether a binary that has `total` bits ends at bit `cursor`.
    Ends {
        total: Place,
        cursor: Place,
        yes: Box<Node<'p>>,
        fail: Fail,
    },
    /// Whether some alternative of an or-pattern fits, and then `then`.
    Any {
        id: AnyId,
        /// The names bound in front of it, which a size in it may read.
        binds: Vec<(TypedBind, Place)>,
        /// The names it binds, each with the place its value is at in `then`.
        names: Vec<(TypedBind, Place)>,
        first: Box<Node<'p>>,
        then: Box<Node<'p>>,
    },
    /// Try each link, and where nothing in it fits go on after it. Whatever goes on
    /// at a link's id does so after that link.
    Chain {
        links: Vec<(RestId, Node<'p>)>,
        last: Box<Node<'p>>,
    },
}

/// What the rows do that a way through the tree has come to the end of.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Goal {
    /// Run this arm.
    Arm(usize),
    /// Go on with what comes after the or-pattern.
    Any(AnyId),
}

impl Node<'_> {
    /// Whether some way through here reads something of the matched value.
    pub(crate) fn reads(&self) -> bool {
        match self {
            Node::Nothing | Node::Rest(_) => false,
            Node::Leaf(leaf) => !leaf.binds.is_empty(),
            Node::Switch { .. }
            | Node::Cut { .. }
            | Node::Measure { .. }
            | Node::Within { .. }
            | Node::Segment(_)
            | Node::Ends { .. }
            | Node::Any { .. } => true,
            Node::Chain { links, last } => {
                links.iter().any(|(_, node)| node.reads()) || last.reads()
            }
        }
    }
}

#[derive(Debug)]
pub(crate) struct Leaf {
    pub(crate) goal: Goal,
    /// The arm's names, each with where its value is. A name that nothing reads is
    /// not here.
    pub(crate) binds: Vec<(TypedBind, Place)>,
    /// The names of tails that are only taken once the arm's guard has held.
    pub(crate) late: Vec<(TypedBind, Place)>,
    /// Where it goes on when the arm has a guard and it fails.
    pub(crate) fail: Fail,
}

/// A case of a [`Node::Switch`].
#[derive(Debug)]
pub(crate) struct Branch<'p> {
    pub(crate) keys: Vec<Key>,
    /// The fields that a row below needs, when nothing below needs the value they
    /// are in. Read all at once, the value may be taken apart.
    pub(crate) apart: Vec<Place>,
    pub(crate) node: Node<'p>,
}

/// How many elements an array is to have.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Length {
    pub(crate) n: u32,
    /// Whether it may have more.
    pub(crate) or_more: bool,
}

/// A question of an array, and the pieces of it that are found where the answer is yes.
#[derive(Debug)]
pub(crate) struct Cut {
    pub(crate) on: Place,
    /// What is read in front of it, so that nothing that holds the array is held for
    /// it any longer.
    pub(crate) before: Vec<Place>,
    pub(crate) length: Length,
    /// The places of its elements from `from` on, of those that are needed.
    pub(crate) from: u32,
    pub(crate) elements: Vec<Option<Place>>,
    /// The place of what is behind the first `length.n` elements, when it is needed.
    pub(crate) tail: Option<Place>,
}

#[derive(Debug)]
pub(crate) struct Segment<'p> {
    pub(crate) on: Place,
    /// Where it starts.
    pub(crate) cursor: Place,
    /// How many bits the binary has.
    pub(crate) total: Place,
    /// 0, as the constant.
    pub(crate) zero: ConstId,
    pub(crate) seg: &'p TypedBinPatSeg,
    /// The names bound so far, which a size may read.
    pub(crate) binds: Vec<(TypedBind, Place)>,
    /// Where what is read goes, when the segment reads a value.
    pub(crate) value: Option<Place>,
    /// Where the next segment starts.
    pub(crate) next: Place,
    pub(crate) yes: Node<'p>,
    pub(crate) fail: Fail,
}

/// Counts the ways through a tree.
pub(super) struct Ways<'t> {
    pub(super) leaves: &'t mut [u32],
    pub(super) rests: &'t mut [u32],
}

impl Ways<'_> {
    fn fail(&mut self, fail: Fail) {
        if let Some(n) = fail.and_then(|id| self.rests.get_mut(id.index())) {
            *n = n.saturating_add(1);
        }
    }

    pub(super) fn walk(&mut self, node: &Node<'_>) {
        match node {
            Node::Nothing => {}
            Node::Rest(id) => self.fail(Some(*id)),
            Node::Leaf(leaf) => {
                let leaves = match leaf.goal {
                    Goal::Arm(i) => self.leaves.get_mut(i),
                    Goal::Any(_) => None,
                };
                if let Some(n) = leaves {
                    *n = n.saturating_add(1);
                }
                self.fail(leaf.fail);
            }
            Node::Switch {
                branches, default, ..
            } => {
                branches.iter().for_each(|b| self.walk(&b.node));
                default.iter().for_each(|node| self.walk(node));
            }
            Node::Cut { yes, no, .. } => {
                self.walk(yes);
                self.walk(no);
            }
            Node::Measure { then, .. } => self.walk(then),
            Node::Within { yes, fail, .. } | Node::Ends { yes, fail, .. } => {
                self.walk(yes);
                self.fail(*fail);
            }
            Node::Segment(segment) => {
                self.walk(&segment.yes);
                self.fail(segment.fail);
            }
            Node::Any { first, then, .. } => {
                self.walk(first);
                self.walk(then);
            }
            // Nothing after a link that nothing goes on after is ever tried.
            Node::Chain { links, last } => {
                for (id, node) in links {
                    self.walk(node);
                    if self.rests.get(id.index()) == Some(&0) {
                        return;
                    }
                }
                self.walk(last);
            }
        }
    }
}
