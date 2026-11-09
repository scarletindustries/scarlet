use crate::type_def::Type;
use indexmap::IndexSet;
use scarlet_syntax::ast;
use scarlet_syntax::span::Span;
use smallvec::SmallVec;
use std::borrow::Cow;
use std::cell::OnceCell;
use std::rc::Rc;

/// Per-check constructor-name interner, covering real variants and the
/// synthetic `lit:`/`#bin:`/`range:`/`#tuple` names. Ids 0 and 1 are reserved
/// for the array constructors, so `new` must insert `[]` then `::` first.
#[derive(Debug)]
struct Interner(IndexSet<String>);

const EMPTY_LIST_ID: u32 = 0;
const CONS_ID: u32 = 1;

impl Interner {
    fn new() -> Self {
        let mut set = IndexSet::new();
        set.insert("[]".to_string());
        set.insert("::".to_string());
        Interner(set)
    }

    fn intern(&mut self, name: &str) -> u32 {
        match self.0.get_index_of(name) {
            Some(i) => i as u32,
            None => self.0.insert_full(name.to_string()).0 as u32,
        }
    }

    fn name(&self, id: u32) -> &str {
        &self.0[id as usize]
    }
}

/// Bitset over interner ids, built once per recursion level.
#[derive(Debug, Default)]
struct CtorIdSet {
    words: SmallVec<[u64; 1]>,
}

impl CtorIdSet {
    fn insert(&mut self, id: u32) {
        let w = (id / 64) as usize;
        if w >= self.words.len() {
            self.words.resize(w + 1, 0);
        }
        self.words[w] |= 1u64 << (id % 64);
    }

    fn contains(&self, id: u32) -> bool {
        self.words
            .get((id / 64) as usize)
            .is_some_and(|w| w & (1u64 << (id % 64)) != 0)
    }
}

/// What the checker knows about one checked pattern node, as much as
/// exhaustiveness needs. The checker has already resolved every constructor,
/// so a variant comes by its index in its type's declaration order.
pub enum PatShape<'p, P> {
    /// Matches anything: a name, `_`, a field the source left out.
    Wild,
    /// A variant of the type at this position. `fields` has one pattern per
    /// declared field, in declared order.
    Variant {
        index: usize,
        fields: &'p [P],
    },
    Tuple(&'p [P]),
    /// `[a, b]`, or `[a, b, ..rest]` when `open`.
    Array {
        prefix: &'p [P],
        open: bool,
    },
    Number(&'p ast::NumberLiteral),
    String(&'p str),
    Range(&'p ast::NumberLiteral, &'p ast::NumberLiteral),
    /// `<<..>>` with at least one segment. A bare `<<..rest>>` is `Wild`.
    Binary {
        segs: Vec<BinSegShape<'p>>,
        open: bool,
    },
    Or(&'p [P]),
}

/// One `<<..>>` segment: what it matches, and how wide it is.
pub struct BinSegShape<'p> {
    pub value: BinPart<'p>,
    pub unit: BinUnit,
    /// The size, when the segment names one.
    pub size: Option<BinPart<'p>>,
}

/// A segment's value or size, as far as telling two segments apart goes.
pub enum BinPart<'p> {
    /// A name or `_`: any value.
    Any,
    Int(&'p ast::NumberLiteral),
    Str(&'p str),
    /// Anything else. The position keeps it apart from every other one.
    At(Span),
}

pub enum BinUnit {
    Bits,
    Bytes,
    Utf8,
}

/// A checked pattern exhaustiveness can read.
pub trait PatternShape: Sized {
    fn shape(&self) -> PatShape<'_, Self>;
}

/// How many levels of a recursive type's variant tables exhaustiveness must
/// see for `p`: the deepest constructor nesting, with names costing nothing.
pub fn unroll_depth<P: PatternShape>(p: &P) -> usize {
    let deepest = |ps: &[P]| ps.iter().map(unroll_depth).max().unwrap_or(0);
    match p.shape() {
        PatShape::Wild => 0,
        PatShape::Variant { fields, .. } => 1 + deepest(fields),
        PatShape::Tuple(ps) | PatShape::Array { prefix: ps, .. } => 1 + deepest(ps),
        PatShape::Or(ps) => deepest(ps),
        PatShape::Number(_)
        | PatShape::String(_)
        | PatShape::Range(..)
        | PatShape::Binary { .. } => 1,
    }
}

/// Lowered pattern used by the matrix recursion. Sub-pattern lists are
/// `Rc`-shared so the row clones in `specialize`/`default_matrix` are refcount
/// bumps, not deep copies.
#[derive(Debug, Clone)]
pub enum Pat {
    Wildcard,
    Ctor { id: u32, args: Rc<[Pat]> },
    Or { patterns: Rc<[Pat]> },
}

#[derive(Debug, Clone)]
struct CtorInfo {
    id: u32,
    arity: usize,
    types: Vec<RcType>,
}

impl CtorInfo {
    /// Constructor absent from the subject type's table (literals, binary
    /// shapes, ranges, type-error fallout). Payload types are unknown, so
    /// every column is `Infinite`; `types` must stay as long as `arity`, which
    /// `specialize` asserts.
    fn opaque(id: u32, arity: usize) -> Self {
        CtorInfo {
            id,
            arity,
            types: vec![RcType::Infinite; arity],
        }
    }
}

#[derive(Debug, Clone)]
struct TypeCtors {
    ctors: Vec<CtorInfo>,
    infinite: bool,
}

impl TypeCtors {
    fn find(&self, id: u32) -> Option<&CtorInfo> {
        self.ctors.iter().find(|c| c.id == id)
    }
}

/// Synthetic constructor name for an n-ary tuple: one ctor per tuple arity.
fn tuple_ctor_name(n: usize) -> String {
    format!("#tuple{}", n)
}

/// Element type plus a lazily-built `[[], ::]` constructor table. The `::`
/// tail slot holds a fresh `ArrayType` rather than a self-reference, so there
/// is no `Rc` cycle.
#[derive(Debug)]
struct ArrayType {
    element: RcType,
    ctors: OnceCell<TypeCtors>,
}

/// Projection of `type_def::Type` carrying only what the usefulness matrix
/// needs: a type's constructor set, shared behind `Rc` so `get_type_ctors` can
/// borrow it at every recursion level. Lowered once in `UsefulnessMatrix::new`.
///
/// `type_def::Type` cannot hold the `Rc` (the inferencer builds it with struct
/// literals), so this stays local to the module.
#[derive(Debug, Clone)]
enum RcType {
    /// No finite constructor set: primitives, functions, type variables,
    /// opaque nominal types, and sub-patterns whose type did not resolve. A
    /// wildcard arm is always required for these.
    Infinite,
    Array(Rc<ArrayType>),
    /// Constructor table in declaration order. Non-empty: an empty variant
    /// table lowers to `Infinite`.
    Named(Rc<TypeCtors>),
    /// Single synthetic `#tupleN` ctor whose `types` are the element types.
    Tuple(Rc<TypeCtors>),
}

/// Lower a `&Type` into the shared `RcType` graph. `Type::Named.variants`
/// already has field types substituted for the concrete `type_args`, so no
/// environment lookup is needed.
fn rc_type(t: &Type, interner: &mut Interner) -> RcType {
    match t {
        Type::Primitive { .. } | Type::Function { .. } | Type::Var { .. } => RcType::Infinite,
        Type::Array { element } => RcType::Array(Rc::new(ArrayType {
            element: rc_type(element, interner),
            ctors: OnceCell::new(),
        })),
        Type::Tuple { elements } => {
            let types: Vec<RcType> = elements.iter().map(|e| rc_type(e, interner)).collect();
            let ctor = CtorInfo {
                id: interner.intern(&tuple_ctor_name(types.len())),
                arity: types.len(),
                types,
            };
            RcType::Tuple(Rc::new(TypeCtors {
                ctors: vec![ctor],
                infinite: false,
            }))
        }
        Type::Named { variants, .. } => {
            if variants.is_empty() {
                RcType::Infinite
            } else {
                let ctors: Vec<CtorInfo> = variants
                    .iter()
                    .map(|(name, fields)| CtorInfo {
                        id: interner.intern(name),
                        arity: fields.len(),
                        types: fields.iter().map(|f| rc_type(&f.ty, interner)).collect(),
                    })
                    .collect();
                RcType::Named(Rc::new(TypeCtors {
                    ctors,
                    infinite: false,
                }))
            }
        }
    }
}

fn get_type_ctors(t: &RcType) -> Cow<'_, TypeCtors> {
    match t {
        RcType::Infinite => Cow::Owned(TypeCtors {
            ctors: vec![],
            infinite: true,
        }),
        RcType::Named(ctors) | RcType::Tuple(ctors) => Cow::Borrowed(ctors.as_ref()),
        RcType::Array(arr) => Cow::Borrowed(arr.ctors.get_or_init(|| TypeCtors {
            ctors: vec![
                CtorInfo {
                    id: EMPTY_LIST_ID,
                    arity: 0,
                    types: vec![],
                },
                CtorInfo {
                    id: CONS_ID,
                    arity: 2,
                    types: vec![
                        arr.element.clone(),
                        RcType::Array(Rc::new(ArrayType {
                            element: arr.element.clone(),
                            ctors: OnceCell::new(),
                        })),
                    ],
                },
            ],
            infinite: false,
        })),
    }
}

/// Persistent singly-linked stack: cloning is a refcount bump, so
/// `specialize`/`default_matrix` prepend O(arity) nodes onto a shared tail
/// instead of copying an O(width) suffix per recursion step.
#[derive(Debug)]
struct Stack<T>(Option<Rc<StackNode<T>>>);

#[derive(Debug)]
struct StackNode<T> {
    head: T,
    tail: Stack<T>,
}

impl<T> Clone for Stack<T> {
    fn clone(&self) -> Self {
        Stack(self.0.clone())
    }
}

impl<T> Default for Stack<T> {
    fn default() -> Self {
        Stack(None)
    }
}

impl<T> Stack<T> {
    fn one(head: T) -> Self {
        Self::cons(head, Stack(None))
    }

    fn cons(head: T, tail: Self) -> Self {
        Stack(Some(Rc::new(StackNode { head, tail })))
    }

    fn split(&self) -> Option<(&T, &Stack<T>)> {
        self.0.as_deref().map(|n| (&n.head, &n.tail))
    }
}

impl<T: Clone> Stack<T> {
    /// `items ++ tail`.
    fn prepend(items: &[T], tail: &Self) -> Self {
        items
            .iter()
            .rev()
            .fold(tail.clone(), |acc, x| Self::cons(x.clone(), acc))
    }

    /// `[item; n] ++ tail`.
    fn prepend_n(item: T, n: usize, tail: &Self) -> Self {
        (0..n).fold(tail.clone(), |acc, _| Self::cons(item.clone(), acc))
    }
}

/// A matrix row: the pattern for each remaining column. Every row in a matrix
/// shares the same column types, so those are carried once alongside the query
/// rather than per row.
type PatStack = Stack<Pat>;
type TypeStack = Stack<RcType>;

#[derive(Debug, Clone, Default)]
struct PatternMatrix {
    rows: Vec<PatStack>,
}

impl PatternMatrix {
    fn is_empty(&self) -> bool {
        self.rows.is_empty()
    }

    fn first_col_ctors(&self) -> CtorIdSet {
        fn collect(p: &Pat, seen: &mut CtorIdSet) {
            match p {
                Pat::Ctor { id, .. } => seen.insert(*id),
                Pat::Or { patterns } => {
                    for inner in patterns.iter() {
                        collect(inner, seen);
                    }
                }
                Pat::Wildcard => {}
            }
        }
        let mut seen = CtorIdSet::default();
        for row in &self.rows {
            if let Some((first, _)) = row.split() {
                collect(first, &mut seen);
            }
        }
        seen
    }

    /// Visit every row's first column with Or-patterns flattened, so `f` only
    /// ever sees `Wildcard` or `Ctor` heads.
    fn for_each_head(&self, mut f: impl FnMut(&Pat, &PatStack)) {
        fn go(p: &Pat, rest: &PatStack, f: &mut impl FnMut(&Pat, &PatStack)) {
            match p {
                Pat::Or { patterns } => {
                    for a in patterns.iter() {
                        go(a, rest, f);
                    }
                }
                Pat::Wildcard | Pat::Ctor { .. } => f(p, rest),
            }
        }
        for row in &self.rows {
            if let Some((first, rest)) = row.split() {
                go(first, rest, &mut f);
            }
        }
    }

    fn specialize(&self, ctor: &CtorInfo) -> PatternMatrix {
        // Callers prepend `ctor.types` onto the type stack in lockstep with
        // the columns pushed here; a mismatch misaligns the two stacks.
        debug_assert!(ctor.arity == ctor.types.len());
        let mut result = PatternMatrix::default();
        self.for_each_head(|head, rest| match head {
            Pat::Wildcard => result
                .rows
                .push(PatStack::prepend_n(Pat::Wildcard, ctor.arity, rest)),
            Pat::Ctor { id, args } if *id == ctor.id => {
                result.rows.push(PatStack::prepend(args, rest))
            }
            Pat::Ctor { .. } | Pat::Or { .. } => {}
        });
        result
    }

    fn default_matrix(&self) -> PatternMatrix {
        let mut result = PatternMatrix::default();
        self.for_each_head(|head, rest| {
            if matches!(head, Pat::Wildcard) {
                result.rows.push(rest.clone());
            }
        });
        result
    }
}

fn is_complete(seen_ctors: &CtorIdSet, type_ctors: &TypeCtors) -> bool {
    !type_ctors.infinite && type_ctors.ctors.iter().all(|c| seen_ctors.contains(c.id))
}

fn is_useful(m: &PatternMatrix, pats: &PatStack, types: &TypeStack) -> bool {
    let Some((first_pat, rest_pats)) = pats.split() else {
        return m.is_empty();
    };
    let Some((first_type, rest_types)) = types.split() else {
        return true;
    };

    let type_ctors = get_type_ctors(first_type);
    let seen_ctors = m.first_col_ctors();

    match first_pat {
        Pat::Wildcard => {
            if is_complete(&seen_ctors, &type_ctors) {
                for ctor in &type_ctors.ctors {
                    let specialized_m = m.specialize(ctor);
                    let new_pats = PatStack::prepend_n(Pat::Wildcard, ctor.arity, rest_pats);
                    let new_types = TypeStack::prepend(&ctor.types, rest_types);
                    if is_useful(&specialized_m, &new_pats, &new_types) {
                        return true;
                    }
                }
                false
            } else {
                is_useful(&m.default_matrix(), rest_pats, rest_types)
            }
        }
        Pat::Ctor { id, args } => {
            // A constructor absent from the type's table gets an opaque
            // fallback with one `Infinite` type per arg, which keeps the
            // pattern and type stacks column-aligned.
            let fallback;
            let ctor_info = match type_ctors.find(*id) {
                Some(c) => c,
                None => {
                    fallback = CtorInfo::opaque(*id, args.len());
                    &fallback
                }
            };
            let specialized_m = m.specialize(ctor_info);
            let new_pats = PatStack::prepend(args, rest_pats);
            let new_types = TypeStack::prepend(&ctor_info.types, rest_types);
            is_useful(&specialized_m, &new_pats, &new_types)
        }
        Pat::Or { patterns } => patterns
            .iter()
            .any(|p| is_useful(m, &PatStack::cons(p.clone(), rest_pats.clone()), types)),
    }
}

/// The variants a wildcard in `pats` stands in for: a `_` (or bare name) in a
/// column of a type with named variants, where the rows of `m` name some of
/// them but not all. A wildcard over a whole value nobody split, or over a
/// type whose values cannot be listed (numbers, strings, array lengths), is
/// not one.
fn catch_all(m: &PatternMatrix, pats: &PatStack, types: &TypeStack) -> Option<Vec<CtorInfo>> {
    let (first_pat, rest_pats) = pats.split()?;
    let (first_type, rest_types) = types.split()?;
    let type_ctors = get_type_ctors(first_type);
    match first_pat {
        Pat::Wildcard => {
            let seen = m.first_col_ctors();
            let complete = is_complete(&seen, &type_ctors);
            if complete {
                // Every variant is named somewhere, so look inside each.
                return type_ctors.ctors.iter().find_map(|ctor| {
                    let new_pats = PatStack::prepend_n(Pat::Wildcard, ctor.arity, rest_pats);
                    let new_types = TypeStack::prepend(&ctor.types, rest_types);
                    catch_all(&m.specialize(ctor), &new_pats, &new_types)
                });
            }
            let named = !m.rows.is_empty() && type_ctors.ctors.iter().any(|c| seen.contains(c.id));
            if named && matches!(first_type, RcType::Named(_)) {
                return Some(
                    type_ctors
                        .ctors
                        .iter()
                        .filter(|c| !seen.contains(c.id))
                        .cloned()
                        .collect(),
                );
            }
            catch_all(&m.default_matrix(), rest_pats, rest_types)
        }
        Pat::Ctor { id, args } => {
            let fallback;
            let ctor_info = match type_ctors.find(*id) {
                Some(c) => c,
                None => {
                    fallback = CtorInfo::opaque(*id, args.len());
                    &fallback
                }
            };
            let new_pats = PatStack::prepend(args, rest_pats);
            let new_types = TypeStack::prepend(&ctor_info.types, rest_types);
            catch_all(&m.specialize(ctor_info), &new_pats, &new_types)
        }
        Pat::Or { patterns } => patterns
            .iter()
            .find_map(|p| catch_all(m, &PatStack::cons(p.clone(), rest_pats.clone()), types)),
    }
}

fn find_witness_vec(m: &PatternMatrix, types: &TypeStack) -> Option<Vec<Pat>> {
    let Some((first_type, rest_types)) = types.split() else {
        return if m.is_empty() { Some(vec![]) } else { None };
    };

    let type_ctors = get_type_ctors(first_type);
    let seen_ctors = m.first_col_ctors();

    if is_complete(&seen_ctors, &type_ctors) {
        for ctor in &type_ctors.ctors {
            let specialized_m = m.specialize(ctor);
            let sub_types = TypeStack::prepend(&ctor.types, rest_types);
            if let Some(witness_vec) = find_witness_vec(&specialized_m, &sub_types) {
                let args: Rc<[Pat]> = (0..ctor.arity)
                    .map(|i| witness_vec.get(i).cloned().unwrap_or(Pat::Wildcard))
                    .collect();
                let head = Pat::Ctor { id: ctor.id, args };
                let tail = witness_vec.get(ctor.arity..).unwrap_or(&[]);
                return Some(std::iter::once(head).chain(tail.iter().cloned()).collect());
            }
        }
        None
    } else {
        // Maranget: recurse on the default matrix for the remaining columns
        // FIRST. A wildcard row may still constrain later columns, so padding
        // them with `_` yields a witness that overlaps an existing arm.
        let rest = find_witness_vec(&m.default_matrix(), rest_types)?;
        let head = type_ctors
            .ctors
            .iter()
            .find(|c| !seen_ctors.contains(c.id))
            .map(|c| Pat::Ctor {
                id: c.id,
                args: vec![Pat::Wildcard; c.arity].into(),
            })
            .unwrap_or(Pat::Wildcard);
        Some(std::iter::once(head).chain(rest).collect())
    }
}

/// Canonical key for a `<<>>` pattern's shape. Two binary patterns share a key
/// only if they match the same set of `Binary` values, which is what lets the
/// matrix flag exact-shape duplicates as unreachable. Dynamic sizes and
/// non-literal segment values are keyed by span so they never collide.
///
/// The encoding must stay INJECTIVE. String segment values can contain the
/// delimiters (`,`, the kind chars `i`/`b`/`u`, digits), so they are
/// length-prefixed; without that, two different patterns share a key and one
/// is falsely reported unreachable.
fn push_int_key(key: &mut String, n: &ast::NumberLiteral) {
    match n.as_int() {
        Some(i) => key.push_str(&i.to_string()),
        None => key.push_str(&n.digits()),
    }
}

fn push_bin_part(key: &mut String, part: &BinPart<'_>, at: char) {
    use std::fmt::Write;
    match part {
        BinPart::Any => key.push('_'),
        BinPart::Int(n) => push_int_key(key, n),
        BinPart::Str(s) => {
            let _ = write!(key, "s{}:{}", s.len(), s);
        }
        BinPart::At(sp) => {
            let _ = write!(key, "{at}{}:{}", sp.start_line, sp.start_column);
        }
    }
}

fn bin_pattern_key(segments: &[BinSegShape<'_>], open: bool) -> String {
    let mut key = String::from("#bin:");
    for seg in segments {
        push_bin_part(&mut key, &seg.value, '@');
        // The kind char also fixes the size's unit ('i' bits, 'b' bytes), so
        // `<<x:16>>` and `<<x:bytes(16)>>` cannot share a key.
        key.push(match seg.unit {
            BinUnit::Bits => 'i',
            BinUnit::Bytes => 'b',
            BinUnit::Utf8 => 'u',
        });
        if let Some(size) = &seg.size {
            push_bin_part(&mut key, size, '?');
        }
        key.push(',');
    }
    if open {
        key.push_str("..");
    }
    key
}

fn pat_to_string(p: &Pat, t: &RcType, interner: &Interner) -> String {
    match p {
        Pat::Wildcard => "_".to_string(),
        Pat::Ctor { id, args } => {
            if *id == EMPTY_LIST_ID {
                return "[]".to_string();
            }
            if *id == CONS_ID {
                let elem_t: &RcType = match t {
                    RcType::Array(arr) => &arr.element,
                    RcType::Infinite | RcType::Named(..) | RcType::Tuple(..) => &RcType::Infinite,
                };
                let mut heads = Vec::new();
                let mut tail = p;
                loop {
                    match tail {
                        Pat::Ctor { id, args } if *id == CONS_ID && args.len() == 2 => {
                            heads.push(pat_to_string(&args[0], elem_t, interner));
                            tail = &args[1];
                        }
                        Pat::Ctor { id, .. } if *id == EMPTY_LIST_ID => {
                            return format!("[{}]", heads.join(", "));
                        }
                        // Guard fall-through: any non-list pattern renders
                        // as the open tail, whatever kind it is.
                        #[allow(unknown_lints, wildcard_over_own_enum)]
                        _ => {
                            heads.push("..".to_string());
                            return format!("[{}]", heads.join(", "));
                        }
                    }
                }
            }
            let name = interner.name(*id);
            let type_ctors = get_type_ctors(t);
            let ctor_types = type_ctors
                .find(*id)
                .map(|c| c.types.as_slice())
                .unwrap_or(&[]);
            let parts: Vec<String> = args
                .iter()
                .enumerate()
                .map(|(i, a)| {
                    pat_to_string(a, ctor_types.get(i).unwrap_or(&RcType::Infinite), interner)
                })
                .collect();
            if name.starts_with("#tuple") {
                return format!("({})", parts.join(", "));
            }
            if parts.is_empty() {
                return name.to_string();
            }
            format!("{}({})", name, parts.join(", "))
        }
        Pat::Or { patterns } => {
            let parts: Vec<String> = patterns
                .iter()
                .map(|pp| pat_to_string(pp, t, interner))
                .collect();
            parts.join(" | ")
        }
    }
}

fn lower_pattern<P: PatternShape>(p: &P, t: &RcType, interner: &mut Interner) -> Pat {
    fn nullary(id: u32) -> Pat {
        Pat::Ctor {
            id,
            args: Rc::from([]),
        }
    }
    match p.shape() {
        PatShape::Wild => Pat::Wildcard,
        // Numeric identity, not spelling: `0xFF` and `255` are one arm.
        PatShape::Number(n) => {
            let key = match n.as_int() {
                Some(i) => format!("lit:{i}"),
                None => format!("lit:{}", n.digits()),
            };
            nullary(interner.intern(&key))
        }
        PatShape::String(s) => nullary(interner.intern(&format!("lit:'{s}'"))),
        PatShape::Range(start, end) => {
            nullary(interner.intern(&format!("range:{}..{}", start.value, end.value)))
        }
        PatShape::Binary { segs, open } => nullary(interner.intern(&bin_pattern_key(&segs, open))),
        PatShape::Variant { index, fields } => {
            let type_ctors = get_type_ctors(t);
            match type_ctors.ctors.get(index) {
                Some(ctor) => Pat::Ctor {
                    id: ctor.id,
                    args: fields
                        .iter()
                        .zip(&ctor.types)
                        .map(|(f, ft)| lower_pattern(f, ft, interner))
                        .collect(),
                },
                // Past the depth the type was unrolled to, the variant table
                // is gone; the variant still has an identity of its own.
                None => Pat::Ctor {
                    id: interner.intern(&format!("#variant{index}")),
                    args: fields
                        .iter()
                        .map(|f| lower_pattern(f, &RcType::Infinite, interner))
                        .collect(),
                },
            }
        }
        PatShape::Tuple(elements) => {
            let elem_types: &[RcType] = if let RcType::Tuple(ctors) = t {
                &ctors.ctors[0].types
            } else {
                &[]
            };
            let args = elements
                .iter()
                .enumerate()
                .map(|(i, e)| {
                    lower_pattern(e, elem_types.get(i).unwrap_or(&RcType::Infinite), interner)
                })
                .collect();
            Pat::Ctor {
                id: interner.intern(&tuple_ctor_name(elements.len())),
                args,
            }
        }
        PatShape::Array { prefix, open } => {
            let elem_type: &RcType = if let RcType::Array(arr) = t {
                &arr.element
            } else {
                &RcType::Infinite
            };
            // Cons-list, right to left. A rest ends the chain in a wildcard
            // tail; otherwise it ends in `[]`.
            let mut acc = if open {
                Pat::Wildcard
            } else {
                nullary(EMPTY_LIST_ID)
            };
            for el in prefix.iter().rev() {
                acc = Pat::Ctor {
                    id: CONS_ID,
                    args: Rc::from([lower_pattern(el, elem_type, interner), acc]),
                };
            }
            acc
        }
        PatShape::Or(alts) => Pat::Or {
            patterns: alts.iter().map(|a| lower_pattern(a, t, interner)).collect(),
        },
    }
}

/// Incremental usefulness checker for one match expression. The compiler
/// pushes each unguarded arm once after checking it, so N arms cost O(N) row
/// constructions rather than rebuilding the matrix per arm.
#[derive(Debug)]
pub struct UsefulnessMatrix {
    matrix: PatternMatrix,
    subject_type: RcType,
    interner: Interner,
}

impl UsefulnessMatrix {
    pub fn new(subject_type: Type) -> Self {
        let mut interner = Interner::new();
        let subject_type = rc_type(&subject_type, &mut interner);
        Self {
            matrix: PatternMatrix::default(),
            subject_type,
            interner,
        }
    }

    /// Lower a checked pattern against this checker's subject type. Ctor
    /// names are interned here, once.
    pub fn lower<P: PatternShape>(&mut self, p: &P) -> Pat {
        lower_pattern(p, &self.subject_type, &mut self.interner)
    }

    pub fn is_useful(&self, pat: &Pat) -> bool {
        let pats = PatStack::one(pat.clone());
        let types = TypeStack::one(self.subject_type.clone());
        is_useful(&self.matrix, &pats, &types)
    }

    pub fn push(&mut self, pat: &Pat) {
        self.matrix.rows.push(PatStack::one(pat.clone()));
    }

    /// The variants `pat` covers with a wildcard although `earlier` arms name
    /// others of the same value, rendered as patterns (`Absent`, `Present(..)`).
    /// `earlier` is every arm before `pat`, guarded or not: a guarded arm
    /// still names the variant it is about.
    pub fn catch_all<'a>(
        &self,
        earlier: impl IntoIterator<Item = &'a Pat>,
        pat: &Pat,
    ) -> Option<Vec<String>> {
        let mut matrix = PatternMatrix::default();
        for p in earlier {
            matrix.rows.push(PatStack::one(p.clone()));
        }
        let types = TypeStack::one(self.subject_type.clone());
        let missing = catch_all(&matrix, &PatStack::one(pat.clone()), &types)?;
        Some(
            missing
                .iter()
                .map(|c| {
                    let name = self.interner.name(c.id);
                    if c.arity == 0 {
                        name.to_string()
                    } else {
                        format!("{name}(..)")
                    }
                })
                .collect(),
        )
    }

    /// A rendered witness pattern the given arms fail to cover, or `None` if
    /// they are exhaustive. The arms passed here form their own matrix; this
    /// checker's incremental matrix is not consulted.
    pub fn find_missing<'a>(&self, patterns: impl IntoIterator<Item = &'a Pat>) -> Option<String> {
        let mut matrix = PatternMatrix::default();
        for p in patterns {
            matrix.rows.push(PatStack::one(p.clone()));
        }
        let types = TypeStack::one(self.subject_type.clone());
        if !is_useful(&matrix, &PatStack::one(Pat::Wildcard), &types) {
            return None;
        }
        let witness = find_witness_vec(&matrix, &types)
            .and_then(|v| v.into_iter().next())
            .unwrap_or(Pat::Wildcard);
        Some(pat_to_string(&witness, &self.subject_type, &self.interner))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::type_def::{self, FieldDef, TypeId, t_int, t_named, t_tuple};

    /// Test-only string-keyed pattern, interned into a `Pat` before use.
    #[derive(Debug, Clone)]
    enum SPat {
        Wildcard,
        Ctor { name: String, args: Vec<SPat> },
        Or { patterns: Vec<SPat> },
    }

    fn intern_spat(p: &SPat, interner: &mut Interner) -> Pat {
        match p {
            SPat::Wildcard => Pat::Wildcard,
            SPat::Ctor { name, args } => Pat::Ctor {
                id: interner.intern(name),
                args: args.iter().map(|a| intern_spat(a, interner)).collect(),
            },
            SPat::Or { patterns } => Pat::Or {
                patterns: patterns.iter().map(|a| intern_spat(a, interner)).collect(),
            },
        }
    }

    impl UsefulnessMatrix {
        fn intern(&mut self, p: &SPat) -> Pat {
            intern_spat(p, &mut self.interner)
        }
        fn push_s(&mut self, p: &SPat) {
            let ip = self.intern(p);
            self.push(&ip);
        }
        fn is_useful_s(&mut self, p: &SPat) -> bool {
            let ip = self.intern(p);
            self.is_useful(&ip)
        }
    }

    fn check_exhaustiveness(pats: &[SPat], t: &Type) -> Option<String> {
        let mut um = UsefulnessMatrix::new(t.clone());
        let ipats: Vec<Pat> = pats.iter().map(|p| um.intern(p)).collect();
        um.find_missing(&ipats)
    }

    #[track_caller]
    fn assert_witness(pats: &[SPat], t: &Type, w: &str) {
        assert_eq!(check_exhaustiveness(pats, t).as_deref(), Some(w));
    }

    #[track_caller]
    fn assert_exhaustive(pats: &[SPat], t: &Type) {
        assert_eq!(check_exhaustiveness(pats, t), None);
    }

    /// Build a `Named` enum type from a `(variant, [(field, ty)])` table.
    fn t_enum(id: i32, name: &str, variants: Vec<(&str, Vec<(&str, Type)>)>) -> Type {
        let variants = variants
            .into_iter()
            .map(|(v, fs)| {
                let fs = fs
                    .into_iter()
                    .map(|(label, ty)| FieldDef {
                        label: label.into(),
                        ty,
                    })
                    .collect();
                (v.to_string(), fs)
            })
            .collect();
        t_named(TypeId(id), name, vec![], variants)
    }

    /// `Bool` is not a primitive: the same `Named` shape the prelude produces.
    fn t_bool() -> Type {
        t_enum(99, "Bool", vec![("True", vec![]), ("False", vec![])])
    }

    fn ctor(name: &str, args: Vec<SPat>) -> SPat {
        SPat::Ctor {
            name: name.to_string(),
            args,
        }
    }

    fn t_option(inner: Type) -> Type {
        t_enum(
            1,
            "Option",
            vec![("Some", vec![("value", inner)]), ("None", vec![])],
        )
    }

    fn t_result(ok: Type, err: Type) -> Type {
        t_enum(
            2,
            "Result",
            vec![("Ok", vec![("value", ok)]), ("Err", vec![("error", err)])],
        )
    }

    fn enum3() -> Type {
        t_enum(
            10,
            "Letter",
            vec![("A", vec![]), ("B", vec![]), ("C", vec![])],
        )
    }

    /// What the last of `pats` stands for, given the arms before it.
    fn catch_all_of(pats: &[SPat], t: &Type) -> Option<Vec<String>> {
        let mut um = UsefulnessMatrix::new(t.clone());
        let ipats: Vec<Pat> = pats.iter().map(|p| um.intern(p)).collect();
        let (last, earlier) = ipats.split_last()?;
        um.catch_all(earlier, last)
    }

    #[test]
    fn a_wildcard_after_a_named_variant_stands_for_the_rest() {
        let pats = [ctor("A", vec![]), SPat::Wildcard];
        assert_eq!(
            catch_all_of(&pats, &enum3()),
            Some(vec!["B".into(), "C".into()])
        );
    }

    #[test]
    fn a_lone_wildcard_stands_for_nothing_named() {
        assert_eq!(catch_all_of(&[SPat::Wildcard], &enum3()), None);
    }

    #[test]
    fn naming_every_variant_leaves_no_catch_all() {
        let rest = SPat::Or {
            patterns: vec![ctor("B", vec![]), ctor("C", vec![])],
        };
        assert_eq!(catch_all_of(&[ctor("A", vec![]), rest], &enum3()), None);
    }

    /// `Some(_)` ignores the payload; nobody split it, so it is not a catch-all.
    #[test]
    fn a_wildcard_inside_an_unsplit_field_is_fine() {
        let t = t_option(enum3());
        let pats = [ctor("None", vec![]), ctor("Some", vec![SPat::Wildcard])];
        assert_eq!(catch_all_of(&pats, &t), None);
    }

    #[test]
    fn a_wildcard_inside_a_split_field_stands_for_the_rest() {
        let t = t_option(enum3());
        let pats = [
            ctor("Some", vec![ctor("A", vec![])]),
            ctor("Some", vec![SPat::Wildcard]),
        ];
        assert_eq!(catch_all_of(&pats, &t), Some(vec!["B".into(), "C".into()]));
    }

    /// The rule follows each tuple column the arms split.
    #[test]
    fn a_tuple_wildcard_stands_for_the_columns_split_before_it() {
        let t = t_tuple(vec![t_bool(), t_int()]);
        let tn = tuple_ctor_name(2);
        let pats = [
            ctor(&tn, vec![ctor("True", vec![]), SPat::Wildcard]),
            SPat::Wildcard,
        ];
        assert_eq!(catch_all_of(&pats, &t), Some(vec!["False".into()]));
    }

    /// Values that cannot be listed keep their `_`.
    #[test]
    fn an_int_wildcard_after_literals_is_fine() {
        let pats = [ctor("1", vec![]), SPat::Wildcard];
        assert_eq!(catch_all_of(&pats, &t_int()), None);
    }

    /// An array's `[]` and `[_, ..]` are lengths, which cannot be listed.
    #[test]
    fn an_array_wildcard_after_the_empty_array_is_fine() {
        let t = type_def::t_array(t_bool());
        let pats = [ctor("[]", vec![]), SPat::Wildcard];
        assert_eq!(catch_all_of(&pats, &t), None);
    }

    #[test]
    fn a_payload_variant_renders_with_its_dots() {
        let t = t_option(t_int());
        let pats = [ctor("None", vec![]), SPat::Wildcard];
        assert_eq!(catch_all_of(&pats, &t), Some(vec!["Some(..)".into()]));
    }

    #[test]
    fn bool_missing_false() {
        assert_witness(&[ctor("True", vec![])], &t_bool(), "False");
    }

    #[test]
    fn bool_exhaustive() {
        assert_exhaustive(&[ctor("True", vec![]), ctor("False", vec![])], &t_bool());
    }

    #[test]
    fn bool_wildcard_exhaustive() {
        assert_exhaustive(&[SPat::Wildcard], &t_bool());
    }

    #[test]
    fn enum_missing_third_variant() {
        assert_witness(&[ctor("A", vec![]), ctor("B", vec![])], &enum3(), "C");
    }

    #[test]
    fn enum_all_variants_exhaustive() {
        let pats = [ctor("A", vec![]), ctor("B", vec![]), ctor("C", vec![])];
        assert_exhaustive(&pats, &enum3());
    }

    #[test]
    fn tuple_bool_bool_missing_fourth() {
        let t = t_tuple(vec![t_bool(), t_bool()]);
        let tn = tuple_ctor_name(2);
        let pats = vec![
            ctor(&tn, vec![ctor("True", vec![]), ctor("True", vec![])]),
            ctor(&tn, vec![ctor("True", vec![]), ctor("False", vec![])]),
            ctor(&tn, vec![ctor("False", vec![]), ctor("True", vec![])]),
        ];
        assert_witness(&pats, &t, "(False, False)");
    }

    #[test]
    fn tuple_bool_bool_exhaustive() {
        let t = t_tuple(vec![t_bool(), t_bool()]);
        let tn = tuple_ctor_name(2);
        let pats = vec![
            ctor(&tn, vec![ctor("True", vec![]), ctor("True", vec![])]),
            ctor(&tn, vec![ctor("True", vec![]), ctor("False", vec![])]),
            ctor(&tn, vec![ctor("False", vec![]), SPat::Wildcard]),
        ];
        assert_exhaustive(&pats, &t);
    }

    /// `(True, _)` and `(_, True)` miss exactly `(False, False)`, not the
    /// overlapping `(False, _)`.
    #[test]
    fn tuple_witness_recurses_default_matrix() {
        let t = t_tuple(vec![t_bool(), t_bool()]);
        let tn = tuple_ctor_name(2);
        let pats = vec![
            ctor(&tn, vec![ctor("True", vec![]), SPat::Wildcard]),
            ctor(&tn, vec![SPat::Wildcard, ctor("True", vec![])]),
        ];
        assert_witness(&pats, &t, "(False, False)");
    }

    /// The witness's cons chain is walked so the head renders, not `[_, ..]`.
    #[test]
    fn array_witness_renders_cons_head() {
        let t = type_def::t_array(t_bool());
        let pats = vec![
            ctor("[]", vec![]),
            ctor("::", vec![ctor("True", vec![]), SPat::Wildcard]),
        ];
        assert_witness(&pats, &t, "[False]");
    }

    #[test]
    fn nested_option_option_int_missing_some_none() {
        let t = t_option(t_option(t_int()));
        let pats = vec![
            ctor("Some", vec![ctor("Some", vec![SPat::Wildcard])]),
            ctor("None", vec![]),
        ];
        assert_witness(&pats, &t, "Some(None)");
    }

    #[test]
    fn nested_option_option_int_exhaustive() {
        let t = t_option(t_option(t_int()));
        let pats = vec![
            ctor("Some", vec![ctor("Some", vec![SPat::Wildcard])]),
            ctor("Some", vec![ctor("None", vec![])]),
            ctor("None", vec![]),
        ];
        assert_exhaustive(&pats, &t);
    }

    #[test]
    fn option_missing_none() {
        assert_witness(
            &[ctor("Some", vec![SPat::Wildcard])],
            &t_option(t_int()),
            "None",
        );
    }

    #[test]
    fn result_missing_err() {
        let t = t_result(t_int(), type_def::t_string());
        assert_witness(&[ctor("Ok", vec![SPat::Wildcard])], &t, "Err(_)");
    }

    #[test]
    fn array_missing_empty() {
        let t = type_def::t_array(t_int());
        assert_witness(
            &[ctor("::", vec![SPat::Wildcard, SPat::Wildcard])],
            &t,
            "[]",
        );
    }

    #[test]
    fn array_missing_nonempty() {
        assert_witness(
            &[ctor("[]", vec![])],
            &type_def::t_array(t_int()),
            "[_, ..]",
        );
    }

    #[test]
    fn int_literal_never_exhaustive() {
        assert_witness(
            &[ctor("lit:1", vec![]), ctor("lit:2", vec![])],
            &t_int(),
            "_",
        );
    }

    #[test]
    fn or_pattern_useful_if_any_alt_useful() {
        let mut m = UsefulnessMatrix::new(t_bool());
        m.push_s(&ctor("True", vec![]));
        let new = SPat::Or {
            patterns: vec![ctor("True", vec![]), ctor("False", vec![])],
        };
        assert!(m.is_useful_s(&new));
    }

    #[test]
    fn or_pattern_not_useful_if_all_covered() {
        let mut m = UsefulnessMatrix::new(t_bool());
        m.push_s(&ctor("True", vec![]));
        m.push_s(&ctor("False", vec![]));
        let new = SPat::Or {
            patterns: vec![ctor("True", vec![]), ctor("False", vec![])],
        };
        assert!(!m.is_useful_s(&new));
    }

    #[test]
    fn usefulness_redundant_after_wildcard() {
        let mut m = UsefulnessMatrix::new(t_bool());
        m.push_s(&SPat::Wildcard);
        assert!(!m.is_useful_s(&ctor("True", vec![])));
    }

    #[test]
    fn usefulness_distinct_ctor() {
        let mut m = UsefulnessMatrix::new(t_bool());
        m.push_s(&ctor("True", vec![]));
        assert!(m.is_useful_s(&ctor("False", vec![])));
    }

    #[test]
    fn enum_with_payload_witness() {
        let t = t_enum(
            11,
            "Tree",
            vec![
                ("Leaf", vec![]),
                ("Node", vec![("left", t_int()), ("right", t_int())]),
            ],
        );
        assert_witness(&[ctor("Leaf", vec![])], &t, "Node(_, _)");
    }

    /// Opaque prelude `Binary`: Named with no variants, so `get_type_ctors`
    /// reports it infinite.
    fn t_binary() -> Type {
        t_enum(98, "Binary", vec![])
    }

    /// A checked binary pattern, as the tests write one.
    struct TBin {
        segs: Vec<(TPart, Option<ast::NumberLiteral>)>,
        open: bool,
    }

    enum TPart {
        Any,
        Int(ast::NumberLiteral),
        Str(&'static str),
    }

    fn num(v: &str) -> ast::NumberLiteral {
        ast::NumberLiteral {
            value: v.to_string(),
            span: Span::DUMMY,
        }
    }

    impl PatternShape for TBin {
        fn shape(&self) -> PatShape<'_, Self> {
            if self.segs.is_empty() && self.open {
                return PatShape::Wild;
            }
            let segs = self
                .segs
                .iter()
                .map(|(v, size)| BinSegShape {
                    value: match v {
                        TPart::Any => BinPart::Any,
                        TPart::Int(n) => BinPart::Int(n),
                        TPart::Str(s) => BinPart::Str(s),
                    },
                    unit: BinUnit::Bits,
                    size: size.as_ref().map(BinPart::Int),
                })
                .collect();
            PatShape::Binary {
                segs,
                open: self.open,
            }
        }
    }

    fn bin(segs: Vec<(TPart, Option<&str>)>, open: bool) -> TBin {
        let segs = segs.into_iter().map(|(v, sz)| (v, sz.map(num))).collect();
        TBin { segs, open }
    }

    #[test]
    fn binary_fixed_segments_not_exhaustive() {
        let mut um = UsefulnessMatrix::new(t_binary());
        let p = um.lower(&bin(vec![(TPart::Any, None), (TPart::Any, None)], false));
        assert_eq!(um.find_missing(&[p]).as_deref(), Some("_"));
    }

    #[test]
    fn binary_empty_literal_not_exhaustive() {
        let mut um = UsefulnessMatrix::new(t_binary());
        let p = um.lower(&bin(vec![], false));
        assert_eq!(um.find_missing(&[p]).as_deref(), Some("_"));
    }

    #[test]
    fn binary_rest_only_is_exhaustive() {
        let mut um = UsefulnessMatrix::new(t_binary());
        let p = um.lower(&bin(vec![], true));
        assert!(matches!(p, Pat::Wildcard));
        assert_eq!(um.find_missing(&[p]), None);
    }

    #[test]
    fn binary_segments_with_rest_not_exhaustive() {
        let mut um = UsefulnessMatrix::new(t_binary());
        let p = um.lower(&bin(vec![(TPart::Any, None)], true));
        assert_eq!(um.find_missing(&[p]).as_deref(), Some("_"));
    }

    #[test]
    fn binary_fixed_then_else_exhaustive() {
        let mut um = UsefulnessMatrix::new(t_binary());
        let p1 = um.lower(&bin(vec![(TPart::Any, None), (TPart::Any, None)], false));
        assert_eq!(um.find_missing(&[p1, Pat::Wildcard]), None);
    }

    #[test]
    fn binary_same_shape_redundant() {
        let mut m = UsefulnessMatrix::new(t_binary());
        let p1 = m.lower(&bin(vec![(TPart::Any, Some("8"))], false));
        let p2 = m.lower(&bin(vec![(TPart::Any, Some("8"))], false));
        m.push(&p1);
        assert!(!m.is_useful(&p2));
    }

    #[test]
    fn binary_different_literal_useful() {
        let mut m = UsefulnessMatrix::new(t_binary());
        let p1 = m.lower(&bin(vec![(TPart::Int(num("1")), None)], false));
        let p2 = m.lower(&bin(vec![(TPart::Int(num("2")), None)], false));
        m.push(&p1);
        assert!(m.is_useful(&p2));
    }

    /// Segment string values can contain the key's delimiter chars, so two
    /// different patterns must not share a key.
    #[test]
    fn binary_string_key_is_injective() {
        let mut m = UsefulnessMatrix::new(t_binary());
        let p1 = m.lower(&bin(vec![(TPart::Str("a'i8,'b"), None)], false));
        let p2 = m.lower(&bin(
            vec![(TPart::Str("a"), Some("8")), (TPart::Str("b"), None)],
            false,
        ));
        m.push(&p1);
        assert!(m.is_useful(&p2));
    }

    /// An unknown constructor with args must keep the pattern and type stacks
    /// column-aligned, or the type stack runs out early and every duplicate
    /// arm looks useful.
    #[test]
    fn unknown_ctor_with_args_duplicate_is_redundant() {
        let mut m = UsefulnessMatrix::new(t_int());
        m.push_s(&ctor("Foo", vec![SPat::Wildcard]));
        assert!(!m.is_useful_s(&ctor("Foo", vec![SPat::Wildcard])));
        assert!(m.is_useful_s(&ctor("Bar", vec![SPat::Wildcard])));
    }
}
