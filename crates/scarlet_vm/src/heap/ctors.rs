//! The constructors whose cells a heap holds, and the number each has in a
//! cell's header.
//!
//! A constructor with fields has a class of its own, above the classes of the
//! kinds ([`Kind`]). The loader gives them out, one for each constructor of
//! every type the program names (`code::forms`), and the table of them goes
//! from the loaded program to each heap that runs it. A class is never given
//! to another constructor, so a header's class says which constructor the cell
//! is for as long as the program is loaded.

use std::sync::Arc;

use scarlet_ir::core_ir::VariantRef;

use super::{ARENA_CELL_WORDS, Kind, Make, Scan, Shape, class, scan};
use crate::word::low_u32;

/// What a cell is, as its header holds it: 0 for a freed cell, then the kinds
/// ([`Kind`]), then the program's constructors.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct Class(pub(super) u16);

/// The class of the first constructor. [`Kind::Ctor`] stands for it and every
/// class above.
const FIRST: u16 = Kind::Ctor as u16;

impl Class {
    /// The class of a cell of `kind`, which is no constructor's.
    #[inline(always)]
    pub(super) const fn of(kind: Kind) -> Class {
        Class(kind as u16)
    }

    #[inline(always)]
    fn is_ctor(self) -> bool {
        self.0 >= FIRST
    }

    /// Whether this is the class of a cell of `kind`.
    #[inline(always)]
    pub(super) fn is(self, kind: Kind) -> bool {
        if kind == Kind::Ctor {
            self.is_ctor()
        } else {
            self == Class::of(kind)
        }
    }

    /// The constructor with this class, when it is one's, and with `variant`.
    pub(super) fn ctor(self, variant: u16) -> Option<Ctor> {
        self.is_ctor().then_some(Ctor {
            class: self,
            variant,
        })
    }

    /// The kind of cell this is, or `None` for a class no cell has.
    #[inline]
    pub(super) fn kind(self) -> Option<Kind> {
        match self.0 {
            1 => Some(Kind::String),
            2 => Some(Kind::BigInt),
            3 => Some(Kind::Range),
            4 => Some(Kind::Binary),
            5 => Some(Kind::Handle),
            6 => Some(Kind::BinarySlice),
            7 => Some(Kind::Tuple),
            8 => Some(Kind::Closure),
            9 => Some(Kind::ArrayLeaf),
            10 => Some(Kind::ArrayRoot),
            11 => Some(Kind::ArrayFlat),
            12 => Some(Kind::Map),
            13 => Some(Kind::MapNode),
            14 => Some(Kind::MapCollision),
            15 => Some(Kind::ArrayBranch),
            FIRST.. => Some(Kind::Ctor),
            0 => None,
        }
    }

    /// Which of a cell's words its header says are values.
    pub(super) fn scan(self) -> Scan {
        self.kind().map_or(Scan::Ask, Kind::scan)
    }
}

/// A constructor with fields, as a cell of it is made and told from others.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Ctor {
    class: Class,
    variant: u16,
}

impl Ctor {
    pub(super) fn class(self) -> Class {
        self.class
    }

    /// Which of its type's variants it is.
    pub(crate) fn variant(self) -> u16 {
        self.variant
    }
}

/// What the header of a constructor's cell says, but for its count: its class,
/// how many words it takes and how many of them are values. A long cell's says
/// the class alone.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Layout(u32);

impl Layout {
    /// The layout of a `ctor` of `fields` fields.
    pub(crate) fn new(ctor: Ctor, fields: usize) -> Layout {
        let make = Shape::ctor(ctor).make();
        let header = if fields < ARENA_CELL_WORDS {
            make.header(fields)
        } else {
            make.stub()
        };
        Layout(low_u32(header >> 32))
    }

    /// The header of a cell laid out so that has one holder.
    #[inline(always)]
    pub(super) fn alone(self) -> u64 {
        u64::from(self.0) << 32 | 1
    }

    /// Whether `h` is the header of a cell laid out so.
    #[inline(always)]
    pub(super) fn is_of(self, h: u64) -> bool {
        h >> 32 == u64::from(self.0)
    }

    /// How many fields the cell has in the arena.
    #[inline(always)]
    pub(super) fn scan(self) -> usize {
        scan(self.alone())
    }

    /// Whether the cell is a long one.
    pub(crate) fn is_long(self) -> bool {
        self.scan() == 0
    }

    pub(super) fn class(self) -> Class {
        class(self.alone())
    }

    /// What a new cell laid out so is, and named with `variant`.
    pub(super) fn make(self, variant: u16) -> Make {
        Make {
            class: self.class(),
            scan: Scan::All,
            variant,
        }
    }
}

/// What the program says of a constructor.
#[derive(Debug, Clone, Copy)]
struct Row {
    variant: VariantRef,
    fields: usize,
}

/// A program's constructors that have fields, by class.
#[derive(Debug, Clone)]
pub(crate) struct Ctors(Arc<[Row]>);

impl Ctors {
    fn row(&self, class: Class) -> Option<&Row> {
        self.0.get(usize::from(class.0.checked_sub(FIRST)?))
    }

    /// The name the program knows `ctor` by.
    pub(crate) fn name(&self, ctor: Ctor) -> Option<VariantRef> {
        self.variant(ctor.class)
    }

    /// The name the program knows the constructor laid out as `layout` by.
    pub(crate) fn laid_out(&self, layout: Layout) -> Option<VariantRef> {
        self.variant(layout.class())
    }

    /// The name the program knows the constructor of `class` by.
    pub(super) fn variant(&self, class: Class) -> Option<VariantRef> {
        self.row(class).map(|row| row.variant)
    }

    /// How many fields the constructor of `class` has, and which of its type's
    /// variants it is.
    pub(super) fn shape(&self, class: Class) -> Option<(usize, u16)> {
        self.row(class)
            .map(|row| (row.fields, row.variant.variant_idx))
    }
}

/// [`Ctors`] still being numbered.
#[derive(Debug, Default)]
pub(crate) struct Numbering(Vec<Row>);

/// A program has more constructors with fields than a header can number.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct TooManyCtors;

impl Numbering {
    /// Give constructor `variant`, which has `fields` fields, the next class.
    pub(crate) fn add(&mut self, variant: VariantRef, fields: usize) -> Result<Ctor, TooManyCtors> {
        let class = u16::try_from(self.0.len()).ok();
        let class = class
            .and_then(|n| n.checked_add(FIRST))
            .ok_or(TooManyCtors)?;
        self.0.push(Row { variant, fields });
        Ok(Ctor {
            class: Class(class),
            variant: variant.variant_idx,
        })
    }

    /// Every constructor numbered so far.
    pub(crate) fn finish(self) -> Ctors {
        Ctors(self.0.into())
    }
}
