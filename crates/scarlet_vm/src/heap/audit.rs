//! A check of everything the heap relies on and no single operation sees.
//!
//! [`Heap::audit`] walks the whole arena and is told every reference held
//! outside it. A count is right when that many words hold the cell, so the
//! audit finds a count that went wrong while the run can still say which
//! cell it was, and even when the space has been handed out again since. It
//! costs the arena's length each time, so only `scarlet run --audit` asks.

use std::fmt;

use super::{
    ARENA_CELL_WORDS, Cell, FREED, Heap, Index, Kind, LEAST_WORDS, UNCOUNTED, class, count, freed,
    held, scan, size, space,
};
use crate::value::Value;
use crate::word::{high_u32, low_u32, to_usize};

/// What an audit found wrong.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AuditFault {
    /// The arena is not made of whole cells one after another: what should
    /// be one at this index is not.
    NoCell { at: usize },
    /// A reference to something that is not a live cell. It is held by
    /// `holder`, or by the run itself.
    Dangling { holder: Option<Index>, to: Index },
    /// A reference that does not carry the variant of the cell it is to. It is
    /// held by `holder`, or by the run itself.
    WrongVariant {
        holder: Option<Index>,
        to: Index,
        variant: u16,
    },
    /// A cell's class is none, or the rest of its header does not go with its
    /// class: its scan, how many fields its constructor has, or whether its
    /// words are in a box. Or it is an array's and has not the words of one.
    Misshapen { cell: Index },
    /// A closure's cell does not start with a function.
    NoFunction { cell: Index },
    /// An array's cell, or a node's, has no elements. The array with none is no cell.
    EmptyArray { cell: Index },
    /// A cell's count is not the number of references to it.
    Miscounted {
        cell: Index,
        count: i32,
        holders: u32,
    },
    /// The free list for cells of `space` words names a cell that is not a
    /// free one of that size, or names one twice.
    BadLink { space: usize, to: u32 },
    /// A free cell is on no list, so nothing would ever take its space.
    Unlisted { cell: Index },
    /// A long cell's stub names a box that is not there, or that another
    /// stub names.
    NoBox { cell: Index },
    /// The boxes do not add up: one has no stub, or a place is not empty and
    /// spare, or the words counted are not the words there are.
    Boxes,
    /// A constant's cell can be freed.
    CountedConstant { cell: Index },
    /// A cell that is no constant's can never be freed.
    NeverCounted { cell: Index },
}

/// Why an audit did not pass a heap.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum AuditFailure {
    Found(AuditFault),
    /// The system has no memory for the audit's own tables, so it did not
    /// finish. This says nothing about the heap.
    NoRoom,
}

impl From<AuditFault> for AuditFailure {
    fn from(fault: AuditFault) -> AuditFailure {
        AuditFailure::Found(fault)
    }
}

/// What an audit has found at a word of the arena: no cell's header, a free
/// cell's, a constant's, or any other live cell's and how many references to
/// it so far.
const NO_CELL: u32 = u32::MAX;
const FREE: u32 = u32::MAX - 1;
const CONSTANT: u32 = u32::MAX - 2;

impl Heap {
    /// Check that the arena is made of whole cells, that each header says
    /// what a cell of its class is, that no array's cell is empty, that every
    /// reference is to a live cell and carries its variant, that every count is
    /// the number of references to its cell, and that the free lists and the
    /// boxes hold exactly what they should. `roots` is every reference held
    /// outside the heap.
    ///
    /// A constant's cell is never counted, and nothing else may be. That
    /// refuses a cell whose count overflowed as well, but a heap has no room
    /// for that many holders.
    pub(crate) fn audit(&self, roots: impl Iterator<Item = Value>) -> Result<(), AuditFailure> {
        let mut seen = table(self.arena.len(), NO_CELL)?;
        let mut at = 1;
        while let Some(slot) = seen.get_mut(at) {
            let end = self.end_of(at).ok_or(AuditFault::NoCell { at })?;
            *slot = if self.is_free(at) { FREE } else { 0 };
            at = end;
        }

        for cell in self.cells() {
            self.audit_header(cell)?;
        }

        for constant in self.constants.iter().flatten() {
            let to = constant.index();
            match seen.get_mut(to.at()) {
                Some(n) if *n < FREE => *n = CONSTANT,
                _ => return Err(AuditFault::Dangling { holder: None, to }.into()),
            }
            self.audit_variant(None, *constant)?;
        }

        let mut holds = |holder, value: Value| {
            let Some(cell) = value.as_cell() else {
                return Ok(());
            };
            let to = cell.index();
            match seen.get_mut(to.at()) {
                Some(n) if *n < CONSTANT => *n = n.saturating_add(1),
                Some(n) if *n == CONSTANT => {}
                _ => return Err(AuditFault::Dangling { holder, to }),
            }
            self.audit_variant(holder, cell)
        };
        for root in roots {
            holds(None, root)?;
        }
        for cell in self.cells() {
            let data = self.words(cell, 1);
            let values = data.get(held(class(self.header(cell)), data.len()));
            for w in values.unwrap_or(&[]) {
                holds(Some(cell), Value::from_bits(*w))?;
            }
        }

        for cell in self.cells() {
            let count = count(self.header(cell));
            let holders = seen.get(cell.at()).copied().unwrap_or(0);
            let fault = match (holders, count) {
                (CONSTANT, UNCOUNTED) => continue,
                (CONSTANT, _) => AuditFault::CountedConstant { cell },
                (_, UNCOUNTED) => AuditFault::NeverCounted { cell },
                (holders, count) if u32::try_from(count) == Ok(holders) => continue,
                (holders, count) => AuditFault::Miscounted {
                    cell,
                    count,
                    holders,
                },
            };
            return Err(fault.into());
        }
        self.audit_lists(seen)?;
        self.audit_boxes()
    }

    /// The header of `cell`, which is live, says what its class says. A long
    /// cell whose box is missing is left for [`Self::audit_boxes`].
    fn audit_header(&self, cell: Index) -> Result<(), AuditFault> {
        let h = self.header(cell);
        let misshapen = AuditFault::Misshapen { cell };
        let kind = class(h).kind().ok_or(misshapen)?;
        let words = self.words(cell, 1);
        let fits = match self.slot(cell, h) {
            None => scan(h) == kind.scan().of(words.len()),
            Some(number) if !self.has_box(number) => return Ok(()),
            Some(_) => scan(h) == 0 && words.len() >= ARENA_CELL_WORDS,
        };
        let fields = match kind {
            Kind::Ctor => self.ctors.shape(class(h)).map(|(fields, _)| fields),
            Kind::String
            | Kind::BigInt
            | Kind::Range
            | Kind::Binary
            | Kind::Handle
            | Kind::BinarySlice
            | Kind::Tuple
            | Kind::Closure
            | Kind::ArrayLeaf
            | Kind::ArrayRoot
            | Kind::ArrayBranch
            | Kind::Map
            | Kind::MapNode
            | Kind::MapCollision => Some(words.len()),
        };
        if !fits || fields != Some(words.len()) {
            return Err(misshapen);
        }
        if kind == Kind::Closure && self.closure_func(Cell::new(cell, 0)).is_none() {
            return Err(AuditFault::NoFunction { cell });
        }
        // As `crate::array` lays them out.
        let empty = match kind {
            Kind::ArrayRoot => match words {
                [len, _, _, _, _, _] => *len == 0,
                _ => return Err(misshapen),
            },
            Kind::ArrayLeaf => words.is_empty(),
            Kind::ArrayBranch => match words {
                [_, both @ ..] if both.len().is_multiple_of(2) => both.is_empty(),
                _ => return Err(misshapen),
            },
            Kind::Range => match words {
                [start, end] => start.cast_signed() >= end.cast_signed(),
                _ => return Err(misshapen),
            },
            Kind::String
            | Kind::BigInt
            | Kind::Binary
            | Kind::Handle
            | Kind::BinarySlice
            | Kind::Tuple
            | Kind::Closure
            | Kind::Map
            | Kind::MapNode
            | Kind::MapCollision
            | Kind::Ctor => false,
        };
        if empty {
            return Err(AuditFault::EmptyArray { cell });
        }
        Ok(())
    }

    /// Whether place `number` of the table of boxes holds one.
    fn has_box(&self, number: u32) -> bool {
        matches!(self.outside.get(number as usize), Some(Some(_)))
    }

    /// `cell`, which `holder` holds, carries the variant its class says.
    fn audit_variant(&self, holder: Option<Index>, cell: Cell) -> Result<(), AuditFault> {
        let (to, variant) = (cell.index(), cell.variant());
        let shape = self.ctors.shape(class(self.header(to)));
        if variant == shape.map_or(0, |(_, variant)| variant) {
            Ok(())
        } else {
            let wrong = AuditFault::WrongVariant {
                holder,
                to,
                variant,
            };
            Err(wrong)
        }
    }

    /// Each free cell is on the list for its size, once.
    fn audit_lists(&self, mut seen: Vec<u32>) -> Result<(), AuditFault> {
        for (space, head) in self.free.iter().enumerate() {
            let mut to = *head;
            while to != 0 {
                let link = self.arena.get((to as usize).saturating_add(1));
                match (seen.get_mut(to as usize), link) {
                    (Some(slot), Some(link))
                        if *slot == FREE && to_usize(high_u32(*link).into()) == space =>
                    {
                        // Found, so a second link to it is refused.
                        *slot = NO_CELL;
                        to = low_u32(*link);
                    }
                    _ => return Err(AuditFault::BadLink { space, to }),
                }
            }
        }
        match seen.iter().position(|slot| *slot == FREE) {
            Some(at) => Err(match Index::new(at) {
                Some(cell) => AuditFault::Unlisted { cell },
                None => AuditFault::NoCell { at },
            }),
            None => Ok(()),
        }
    }

    /// Each stub has a box of its own, and no box is without one.
    fn audit_boxes(&self) -> Result<(), AuditFailure> {
        let mut named = table(self.outside.len(), false)?;
        let mut words: usize = 0;
        for cell in self.cells() {
            let Some(number) = self.slot(cell, self.header(cell)) else {
                continue;
            };
            let number = number as usize;
            match (self.outside.get(number), named.get_mut(number)) {
                (Some(Some(held)), Some(named)) if !*named => {
                    *named = true;
                    words = words.saturating_add(held.len());
                }
                _ => return Err(AuditFault::NoBox { cell }.into()),
            }
        }
        let empty = self.spare.iter().all(|number| {
            let place = self.outside.get(*number as usize);
            let named = named.get_mut(*number as usize);
            matches!(place, Some(None)) && named.is_some_and(|n| !std::mem::replace(n, true))
        });
        if empty && words == self.boxed && named.iter().all(|named| *named) {
            Ok(())
        } else {
            Err(AuditFault::Boxes.into())
        }
    }

    /// How many cells are not yet freed. An arena that is not made of whole
    /// cells has no such number.
    pub(crate) fn live_cells(&self) -> Result<usize, AuditFault> {
        let (mut at, mut live) = (1, 0_usize);
        while at < self.arena.len() {
            live = live.saturating_add(usize::from(!self.is_free(at)));
            at = self.end_of(at).ok_or(AuditFault::NoCell { at })?;
        }
        Ok(live)
    }

    /// Each cell not yet freed, in the arena's order. It stops at the first
    /// word that should be a cell and is not, so ask only once the arena is
    /// known to be whole.
    fn cells(&self) -> impl Iterator<Item = Index> + '_ {
        std::iter::successors(Some(1), |at| self.end_of(*at))
            .take_while(|at| *at < self.arena.len())
            .filter(|at| !self.is_free(*at))
            .filter_map(Index::new)
    }

    /// Whether the cell whose header is word `at` is free.
    fn is_free(&self, at: usize) -> bool {
        self.arena.get(at).is_none_or(|h| freed(*h))
    }

    /// Where the cell whose header is word `at` ends, or `None` when there is
    /// no whole cell there.
    fn end_of(&self, at: usize) -> Option<usize> {
        let h = *self.arena.get(at)?;
        let words = if h == FREED {
            let link = self.arena.get(at.checked_add(1)?)?;
            to_usize(high_u32(*link).into())
        } else if freed(h) || size(h) > ARENA_CELL_WORDS {
            return None;
        } else {
            space(size(h))
        };
        let end = at.checked_add(words)?;
        (words >= LEAST_WORDS && end <= self.arena.len()).then_some(end)
    }
}

impl fmt::Display for AuditFault {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            AuditFault::NoCell { at } => write!(f, "the heap has no whole cell at word {at}"),
            AuditFault::Dangling {
                holder,
                to: Index(to),
            } => write!(
                f,
                "{} holds cell {to}, which is not a live cell",
                Holder(holder)
            ),
            AuditFault::WrongVariant {
                holder,
                to: Index(to),
                variant,
            } => write!(
                f,
                "{} holds cell {to} as variant {variant}, which it is not",
                Holder(holder)
            ),
            AuditFault::Misshapen { cell: Index(cell) } => {
                write!(
                    f,
                    "the header of cell {cell} is none a cell of its class has"
                )
            }
            AuditFault::NoFunction { cell: Index(cell) } => {
                write!(f, "the closure cell {cell} has no function")
            }
            AuditFault::EmptyArray { cell: Index(cell) } => {
                write!(f, "the array's cell {cell} has no elements")
            }
            AuditFault::Miscounted {
                cell: Index(cell),
                count,
                holders,
            } => write!(
                f,
                "cell {cell} has a count of {count} and {holders} holders"
            ),
            AuditFault::BadLink { space, to } => write!(
                f,
                "the free list for {space} words names cell {to}, which is not a free cell of that size, or names it twice"
            ),
            AuditFault::Unlisted { cell: Index(cell) } => {
                write!(f, "the free cell {cell} is on no free list")
            }
            AuditFault::NoBox { cell: Index(cell) } => {
                write!(f, "the long cell {cell} has no box of its own")
            }
            AuditFault::Boxes => f.write_str("the boxes of long cells do not add up"),
            AuditFault::CountedConstant { cell: Index(cell) } => {
                write!(f, "cell {cell} is a constant's and can be freed")
            }
            AuditFault::NeverCounted { cell: Index(cell) } => {
                write!(f, "cell {cell} can never be freed and is not a constant's")
            }
        }
    }
}

/// What holds a reference, as a fault names it.
struct Holder(Option<Index>);

impl fmt::Display for Holder {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.0 {
            Some(Index(cell)) => write!(f, "cell {cell}"),
            None => f.write_str("the run"),
        }
    }
}

/// A table of `len` entries, each `entry`.
fn table<T: Copy>(len: usize, entry: T) -> Result<Vec<T>, AuditFailure> {
    let mut table = Vec::new();
    table
        .try_reserve_exact(len)
        .map_err(|_| AuditFailure::NoRoom)?;
    table.resize(len, entry);
    Ok(table)
}

#[cfg(test)]
mod tests;
