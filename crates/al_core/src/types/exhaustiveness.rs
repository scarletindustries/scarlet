use crate::ast;
use crate::type_def::Type;
use crate::typed_ir::slots::slot_labeled;
use indexmap::IndexSet;
use smallvec::SmallVec;
use std::borrow::Cow;
use std::cell::OnceCell;
use std::rc::Rc;

/// Per-check constructor-name interner. Every ctor name (real variants plus
/// the synthetic `lit:`/`#bin:`/`range:`/`#tuple` names) is mapped to a dense
/// u32 id once at the public entry points, so the matrix recursion compares
/// and stores integers instead of `String`s and seen-ctor membership is a
/// bitset lookup. Ids 0 and 1 are pre-reserved for the array constructors so
/// `get_type_ctors` and `pat_to_string` can refer to them without a lookup.
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

/// Bitset over interner ids. Ids are dense and small (one per distinct ctor
/// name in the match + subject type), so membership is a single word probe.
/// Built once per `is_useful`/`find_witness_vec` recursion level; the inline
/// `SmallVec` word keeps the ≤64-id case (the norm) allocation-free.
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

/// Lowered pattern used by the matrix recursion: ctor names are interner ids
/// and sub-pattern lists are `Rc`-shared, so the row clones performed by
/// `specialize`/`default_matrix`/`is_useful` are refcount bumps instead of deep
/// copies of the pattern subtree. Built once per arm by `UsefulnessMatrix::lower`
/// and then borrowed by `is_useful`/`push`/`find_missing`.
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
    /// Field labels in declaration order (parallel to `types`). Empty for
    /// constructors without named fields (array `::`/`[]`, tuples). Used by
    /// `lower_pattern` to slot labeled pattern args into declaration order.
    labels: Vec<String>,
    types: Vec<RcType>,
}

impl CtorInfo {
    /// Constructor absent from the subject type's table (literals, binary
    /// shapes, ranges, type-error fallout). Payload types are unknown, so
    /// every column is `Infinite` — keeping `types` parallel to `arity`, the
    /// invariant every table-built `CtorInfo` maintains and `specialize`
    /// asserts.
    fn opaque(id: u32, arity: usize) -> Self {
        CtorInfo {
            id,
            arity,
            labels: vec![],
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

/// Synthetic constructor name for an n-ary tuple. Kept in lockstep with the
/// lowering in `lower_pattern` so the matrix algorithm sees a single ctor per
/// tuple arity.
fn tuple_ctor_name(n: usize) -> String {
    format!("#tuple{}", n)
}

/// Element type plus a lazily-built `[[], ::]` constructor table. The table is
/// filled on the first `get_type_ctors` call and then borrowed on every
/// subsequent recursion level, so an M-arm match over cons-depth-D array
/// patterns builds the table O(D) times instead of O(M×D). The `::` tail slot
/// is a fresh `ArrayType` (same element, empty cell) rather than a
/// self-reference, so no `Rc` cycle is introduced.
#[derive(Debug)]
struct ArrayType {
    element: RcType,
    ctors: OnceCell<TypeCtors>,
}

/// Interned, structurally-shared projection of `type_def::Type` carrying only
/// what the usefulness matrix needs: the constructor set of a nominal/array/
/// tuple type. Lowered from a `&Type` once in `UsefulnessMatrix::new`, with
/// ctor names interned into that checker's `Interner` as part of the
/// lowering. `Named`/`Tuple` store their fully-built `TypeCtors` behind an
/// `Rc`, and `Array` caches its `[[], ::]` table behind a `OnceCell`, so
/// `get_type_ctors` — called once per recursion level — borrows the table
/// instead of re-allocating ctor names/labels/types each time.
///
/// `type_def::Type` itself can't hold the `Rc` — it is built with struct
/// literals in the inferencer — so the interning is local to this module.
#[derive(Debug, Clone)]
enum RcType {
    /// No finite constructor set: primitives, functions, type variables, and
    /// opaque/unresolved nominal types (empty variant table). Also used as the
    /// placeholder for sub-patterns whose type could not be resolved from the
    /// surrounding context. `get_type_ctors` reports these as `infinite`, so a
    /// wildcard arm is required.
    Infinite,
    Array(Rc<ArrayType>),
    /// Constructor table in declaration order. Non-empty by construction (an
    /// empty variant table lowers to `Infinite`).
    Named(Rc<TypeCtors>),
    /// Single synthetic `#tupleN` ctor whose `types` are the element types.
    Tuple(Rc<TypeCtors>),
}

/// One-time O(type-size) lowering of a `&Type` into the shared `RcType` graph.
/// `Type::Named.variants` already carries field types substituted for the
/// concrete `type_args`, so no environment lookup is needed here.
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
                labels: vec![],
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
                        labels: fields.iter().map(|f| f.label.clone()).collect(),
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
                    labels: vec![],
                    types: vec![],
                },
                CtorInfo {
                    id: CONS_ID,
                    arity: 2,
                    labels: vec![],
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

/// Persistent singly-linked stack. Cloning is an `Rc` refcount bump and the
/// tail is structurally shared, so `specialize`/`default_matrix` prepend
/// O(arity) new nodes onto an existing row's tail instead of copying its
/// O(width) suffix into a fresh `Vec` on every recursion step of `is_useful`.
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
    /// `[items[0], items[1], ..] ++ tail`. O(items.len()) node allocs; the
    /// tail is shared by refcount.
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

/// A matrix row: the pattern for each remaining column. Column types are the
/// same for every row in a given matrix (they evolve in lockstep with the
/// query row), so they are carried once alongside the query in `is_useful` /
/// `find_witness_vec` rather than duplicated per row.
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

    /// Visit every row's first column with Or-patterns recursively flattened,
    /// so `f` only ever sees `Wildcard` or `Ctor` heads. Shared iteration core
    /// for `specialize` and `default_matrix`.
    fn for_each_head(&self, mut f: impl FnMut(&Pat, &PatStack)) {
        fn go(p: &Pat, rest: &PatStack, f: &mut impl FnMut(&Pat, &PatStack)) {
            match p {
                Pat::Or { patterns } => {
                    for a in patterns.iter() {
                        go(a, rest, f);
                    }
                }
                _ => f(p, rest),
            }
        }
        for row in &self.rows {
            if let Some((first, rest)) = row.split() {
                go(first, rest, &mut f);
            }
        }
    }

    fn specialize(&self, ctor: &CtorInfo) -> PatternMatrix {
        // Every caller prepends `ctor.types` onto the type stack in lockstep
        // with the `ctor.arity` pattern columns pushed here; a mismatch would
        // silently misalign the two stacks.
        debug_assert!(ctor.arity == ctor.types.len());
        let mut result = PatternMatrix::default();
        self.for_each_head(|head, rest| match head {
            Pat::Wildcard => result
                .rows
                .push(PatStack::prepend_n(Pat::Wildcard, ctor.arity, rest)),
            Pat::Ctor { id, args } if *id == ctor.id => {
                result.rows.push(PatStack::prepend(args, rest))
            }
            _ => {}
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
            // Constructors absent from the type's table (literals, binary
            // shapes, ranges, type-error fallout) get an opaque fallback with
            // one `Infinite` type per arg, keeping the pattern and type
            // stacks column-aligned through the recursion.
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
        // FIRST — a wildcard row in the matrix may still constrain later
        // columns, so padding them with `_` (as an early return would) can
        // yield a witness that overlaps an existing arm.
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

/// Canonical key for a `<<>>` pattern's shape. Two binary patterns lower to the
/// same key iff they are guaranteed to match the same set of `Binary` values, so
/// the matrix algorithm flags exact-shape duplicates as unreachable while still
/// treating `Binary` as infinite (a wildcard / `else` arm is required for
/// exhaustiveness). Dynamic sizes and non-literal segment values are keyed by
/// span so we never emit a false-positive unreachable error.
///
/// The encoding must be INJECTIVE: string segment values can contain the
/// delimiter chars (`,`, kind chars `i`/`b`/`u`, digits), so they are
/// length-prefixed (`s<len>:<value>`) rather than quoted — otherwise two
/// structurally different patterns could share a key and one would be
/// falsely flagged unreachable.
fn bin_pattern_key(segments: &[ast::BinSegmentPat], has_rest: bool) -> String {
    use std::fmt::Write;
    let mut key = String::from("#bin:");
    for seg in segments {
        match &seg.value {
            ast::Pattern::Wildcard { .. } | ast::Pattern::Var { .. } => key.push('_'),
            ast::Pattern::Literal(ast::PatternLiteral::Number(n)) => key.push_str(&n.value),
            ast::Pattern::Literal(ast::PatternLiteral::String(s)) => {
                let _ = write!(key, "s{}:{}", s.value.len(), s.value);
            }
            other => {
                let sp = other.span();
                let _ = write!(key, "@{}:{}", sp.start_line, sp.start_column);
            }
        }
        // The kind char also fixes the size's unit: 'i' widths are bits,
        // 'b' sizes are bytes, so `<<x:16>>` and `<<x:bytes(16)>>` cannot
        // share a key.
        match &seg.spec {
            ast::BinSpec::Int { .. } => key.push('i'),
            ast::BinSpec::Binary { .. } => key.push('b'),
            ast::BinSpec::Utf8 => key.push('u'),
        }
        match seg.spec.size_expr() {
            None => {}
            Some(ast::Expression::NumberLiteral(n)) => key.push_str(&n.value),
            Some(e) => {
                let sp = e.span();
                let _ = write!(key, "?{}:{}", sp.start_line, sp.start_column);
            }
        }
        key.push(',');
    }
    if has_rest {
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
                    _ => &RcType::Infinite,
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

fn lower_pattern(p: &ast::Pattern, t: &RcType, interner: &mut Interner) -> Pat {
    fn nullary(id: u32) -> Pat {
        Pat::Ctor {
            id,
            args: Rc::from([]),
        }
    }
    match p {
        ast::Pattern::Wildcard { .. } | ast::Pattern::Var { .. } => Pat::Wildcard,
        ast::Pattern::Literal(lit) => match lit {
            ast::PatternLiteral::Number(n) => nullary(interner.intern(&format!("lit:{}", n.value))),
            ast::PatternLiteral::String(s) => {
                nullary(interner.intern(&format!("lit:'{}'", s.value)))
            }
        },
        ast::Pattern::Constructor { name, args, .. } => {
            let type_ctors = get_type_ctors(t);
            let id = interner.intern(&name.name);
            let pat_args: Rc<[Pat]> = match type_ctors.find(id) {
                Some(ctor) => {
                    // Slot args into field-DECLARATION order with the same
                    // `slot_labeled` the typechecker and elaborator use: any
                    // slot left empty (including those covered by a `..` rest)
                    // becomes a wildcard. Lowering in source order instead
                    // would permute the usefulness matrix relative to real
                    // match semantics, yielding unsound false-exhaustiveness
                    // (and false positives) whenever labels name fields out of
                    // order. Slotting errors are dropped: the typechecker has
                    // already reported them, and the unplaced arg's slot
                    // degrades to a wildcard here.
                    let fields: Vec<Option<&str>> = (0..ctor.types.len())
                        .map(|i| ctor.labels.get(i).map(String::as_str))
                        .collect();
                    let supplied = args.iter().map(|a| match a {
                        ast::PatternArg::Positional(p) => (None, p),
                        ast::PatternArg::Labeled { label, pattern } => {
                            (Some(label.name.as_str()), pattern)
                        }
                    });
                    let (by_pos, _errors) = slot_labeled(&fields, supplied);
                    by_pos
                        .into_iter()
                        .zip(&ctor.types)
                        .map(|(slot, field_t)| match slot {
                            Some(p) => lower_pattern(p, field_t, interner),
                            None => Pat::Wildcard,
                        })
                        .collect()
                }
                // Unresolved type / unknown constructor: typing already reported
                // the error. Lower args in source order as a best effort.
                None => args
                    .iter()
                    .map(|a| {
                        let inner = match a {
                            ast::PatternArg::Positional(p) => p,
                            ast::PatternArg::Labeled { pattern, .. } => pattern,
                        };
                        lower_pattern(inner, &RcType::Infinite, interner)
                    })
                    .collect(),
            };
            Pat::Ctor { id, args: pat_args }
        }
        ast::Pattern::Tuple { elements, .. } => {
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
        ast::Pattern::Array { elements, .. } => {
            let elem_type: &RcType = if let RcType::Array(arr) = t {
                &arr.element
            } else {
                &RcType::Infinite
            };
            // Build a cons-list right-to-left. A spread element terminates the
            // chain with a wildcard tail (matches any remaining list, including
            // empty); otherwise the chain ends in `[]`.
            let mut acc = nullary(EMPTY_LIST_ID);
            for el in elements.iter().rev() {
                match el {
                    ast::ArrayPatternElement::Spread { .. } => {
                        acc = Pat::Wildcard;
                    }
                    ast::ArrayPatternElement::Pattern(p) => {
                        acc = Pat::Ctor {
                            id: CONS_ID,
                            args: Rc::from([lower_pattern(p, elem_type, interner), acc]),
                        };
                    }
                }
            }
            acc
        }
        ast::Pattern::Binary { segments, rest, .. } => {
            // `<<..rest>>` with no leading segments matches every Binary value.
            // Any other shape (fixed-width segments, or `<<>>`) constrains
            // length and/or content and cannot exhaust the infinite Binary
            // value space on its own — lower it to an opaque ctor so a
            // wildcard / `else` arm is still required.
            if segments.is_empty() && rest.is_some() {
                Pat::Wildcard
            } else {
                nullary(interner.intern(&bin_pattern_key(segments, rest.is_some())))
            }
        }
        ast::Pattern::Or { first, rest, .. } => Pat::Or {
            patterns: std::iter::once(&**first)
                .chain(rest.iter())
                .map(|p| lower_pattern(p, t, interner))
                .collect(),
        },
        ast::Pattern::Range { start, end, .. } => {
            nullary(interner.intern(&format!("range:{}..{}", start.value, end.value)))
        }
    }
}

/// Incremental usefulness checker for a single match expression. The compiler
/// pushes each unguarded arm once after its usefulness check, so N arms cost
/// O(N) row constructions instead of the O(N²) incurred by rebuilding the
/// matrix from a `&[Pat]` slice on every arm. The interner lives for the whole
/// match, so each arm's ctor names are interned exactly once.
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

    /// Lower an `ast::Pattern` into the interned `Pat` form used by the matrix
    /// algorithm, against this checker's subject type. Ctor names are interned
    /// here, once, so `is_useful`/`push`/`find_missing` can borrow the result
    /// without re-walking the tree.
    pub fn lower(&mut self, p: &ast::Pattern) -> Pat {
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

    /// Return a rendered witness pattern the given arms fail to cover, or
    /// `None` if they are exhaustive. Independent of `is_useful`/`push` — the
    /// arms passed here (typically the unguarded subset) form their own
    /// matrix; this checker's incremental matrix is not consulted.
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

    /// Test-only string-keyed pattern; interned into a `Pat` against a
    /// specific `UsefulnessMatrix` before use.
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

    /// `Bool` is no longer a primitive; build the same `Named` shape the
    /// real prelude produces so the matrix tests exercise the variant path.
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

    /// `(True, _)` and `(_, True)` miss exactly `(False, False)`. The
    /// incomplete-signature branch of `find_witness_vec` used to short-circuit
    /// with `(False, _)`, which overlaps arm 2 — recursing on the default
    /// matrix first yields the precise witness.
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

    /// `[]` ∪ `[True, ..]` miss any list whose head is `False`; the witness's
    /// cons chain is walked so the head renders, not a blanket `[_, ..]`.
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
        // Option(Option(Int)): cover Some(Some(_)) and None → missing Some(None)
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
        // Or in the test row goes through is_useful's PatOr branch: useful if any alt is.
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

    /// Opaque prelude `Binary` — Named with no variants → infinite in
    /// `get_type_ctors`, mirroring how the real prelude registers it.
    fn t_binary() -> Type {
        t_enum(98, "Binary", vec![])
    }

    fn bin_seg(value: ast::Pattern, size: Option<&str>) -> ast::BinSegmentPat {
        ast::BinSegmentPat {
            value,
            spec: ast::BinSpec::Int {
                bits: size.map(|n| {
                    ast::Expression::NumberLiteral(ast::NumberLiteral {
                        value: n.to_string(),
                        span: crate::span::Span::DUMMY,
                    })
                }),
            },
            span: crate::span::Span::DUMMY,
        }
    }

    fn bin_pat(segments: Vec<ast::BinSegmentPat>, rest: bool) -> ast::Pattern {
        ast::Pattern::Binary {
            segments,
            rest: rest.then_some(ast::BinaryPatternRest {
                binding: None,
                span: crate::span::Span::DUMMY,
            }),
            span: crate::span::Span::DUMMY,
        }
    }

    fn p_var(name: &str) -> ast::Pattern {
        ast::Pattern::Var {
            name: ast::Identifier {
                name: name.to_string(),
                span: crate::span::Span::DUMMY,
            },
        }
    }

    #[test]
    fn binary_fixed_segments_not_exhaustive() {
        let mut um = UsefulnessMatrix::new(t_binary());
        let p = um.lower(&bin_pat(
            vec![bin_seg(p_var("a"), None), bin_seg(p_var("b"), None)],
            false,
        ));
        assert_eq!(um.find_missing(&[p]).as_deref(), Some("_"));
    }

    #[test]
    fn binary_empty_literal_not_exhaustive() {
        let mut um = UsefulnessMatrix::new(t_binary());
        let p = um.lower(&bin_pat(vec![], false));
        assert_eq!(um.find_missing(&[p]).as_deref(), Some("_"));
    }

    #[test]
    fn binary_rest_only_is_exhaustive() {
        let mut um = UsefulnessMatrix::new(t_binary());
        let p = um.lower(&bin_pat(vec![], true));
        assert!(matches!(p, Pat::Wildcard));
        assert_eq!(um.find_missing(&[p]), None);
    }

    #[test]
    fn binary_segments_with_rest_not_exhaustive() {
        let mut um = UsefulnessMatrix::new(t_binary());
        let p = um.lower(&bin_pat(vec![bin_seg(p_var("a"), None)], true));
        assert_eq!(um.find_missing(&[p]).as_deref(), Some("_"));
    }

    #[test]
    fn binary_fixed_then_else_exhaustive() {
        let mut um = UsefulnessMatrix::new(t_binary());
        let p1 = um.lower(&bin_pat(
            vec![bin_seg(p_var("a"), None), bin_seg(p_var("b"), None)],
            false,
        ));
        assert_eq!(um.find_missing(&[p1, Pat::Wildcard]), None);
    }

    #[test]
    fn binary_same_shape_redundant() {
        let mut m = UsefulnessMatrix::new(t_binary());
        let p1 = m.lower(&bin_pat(vec![bin_seg(p_var("a"), Some("8"))], false));
        let p2 = m.lower(&bin_pat(vec![bin_seg(p_var("b"), Some("8"))], false));
        m.push(&p1);
        assert!(!m.is_useful(&p2));
    }

    #[test]
    fn binary_different_literal_useful() {
        let mut m = UsefulnessMatrix::new(t_binary());
        let lit = |v: &str| {
            ast::Pattern::Literal(ast::PatternLiteral::Number(ast::NumberLiteral {
                value: v.to_string(),
                span: crate::span::Span::DUMMY,
            }))
        };
        let p1 = m.lower(&bin_pat(vec![bin_seg(lit("1"), None)], false));
        let p2 = m.lower(&bin_pat(vec![bin_seg(lit("2"), None)], false));
        m.push(&p1);
        assert!(m.is_useful(&p2));
    }

    /// Segment string values can contain the key's delimiter chars. Before
    /// the length-prefixed encoding, `<<"a'i8,'b":int>>` and
    /// `<<"a":int-8, "b":int>>` both keyed as `#bin:'a'i8,'b'i,`, so the
    /// second (structurally different) pattern was falsely redundant.
    #[test]
    fn binary_string_key_is_injective() {
        let str_lit = |v: &str| {
            ast::Pattern::Literal(ast::PatternLiteral::String(ast::StringLiteral {
                value: v.to_string(),
                span: crate::span::Span::DUMMY,
            }))
        };
        let mut m = UsefulnessMatrix::new(t_binary());
        let p1 = m.lower(&bin_pat(vec![bin_seg(str_lit("a'i8,'b"), None)], false));
        let p2 = m.lower(&bin_pat(
            vec![
                bin_seg(str_lit("a"), Some("8")),
                bin_seg(str_lit("b"), None),
            ],
            false,
        ));
        m.push(&p1);
        assert!(m.is_useful(&p2));
    }

    /// An unknown constructor (type-error fallout) with args must keep the
    /// pattern and type stacks column-aligned: the old empty-`types` fallback
    /// exhausted the type stack early and reported every duplicate arm as
    /// useful.
    #[test]
    fn unknown_ctor_with_args_duplicate_is_redundant() {
        let mut m = UsefulnessMatrix::new(t_int());
        m.push_s(&ctor("Foo", vec![SPat::Wildcard]));
        assert!(!m.is_useful_s(&ctor("Foo", vec![SPat::Wildcard])));
        assert!(m.is_useful_s(&ctor("Bar", vec![SPat::Wildcard])));
    }

    /// A nullary constructor pattern such as `True`.
    fn p_ctor0(name: &str) -> ast::Pattern {
        ast::Pattern::Constructor {
            qualifier: None,
            name: ast::Identifier {
                name: name.to_string(),
                span: crate::span::Span::DUMMY,
            },
            args: vec![],
            rest: false,
            span: crate::span::Span::DUMMY,
        }
    }

    /// `Name(label: pat, ...)` with all-labeled args and an optional `..` rest.
    fn p_ctor_labeled(name: &str, fields: Vec<(&str, ast::Pattern)>, rest: bool) -> ast::Pattern {
        ast::Pattern::Constructor {
            qualifier: None,
            name: ast::Identifier {
                name: name.to_string(),
                span: crate::span::Span::DUMMY,
            },
            args: fields
                .into_iter()
                .map(|(label, pattern)| ast::PatternArg::Labeled {
                    label: ast::Identifier {
                        name: label.to_string(),
                        span: crate::span::Span::DUMMY,
                    },
                    pattern,
                })
                .collect(),
            rest,
            span: crate::span::Span::DUMMY,
        }
    }

    /// `type Pair { a Bool b Bool }` — `a` is declaration index 0, `b` is 1.
    fn pair_bool_bool() -> Type {
        t_enum(
            50,
            "Pair",
            vec![("Pair", vec![("a", t_bool()), ("b", t_bool())])],
        )
    }

    /// `lower` must slot labeled constructor args into field-DECLARATION
    /// order, mirroring the compiler's `slot_ctor_args` and the runtime
    /// matcher. Lowering in source order permuted the usefulness matrix, so a
    /// non-exhaustive match looked exhaustive (unsound) and a genuinely
    /// exhaustive one looked incomplete.
    #[test]
    fn lower_slots_labeled_args_in_declaration_order() {
        let pair = pair_bool_bool();
        let pair_pat = |fields: &[(&str, &str)], rest| {
            let args = fields.iter().map(|&(l, v)| (l, p_ctor0(v))).collect();
            p_ctor_labeled("Pair", args, rest)
        };
        let missing = |arms: &[ast::Pattern]| {
            let mut um = UsefulnessMatrix::new(pair.clone());
            let ipats: Vec<Pat> = arms.iter().map(|p| um.lower(p)).collect();
            um.find_missing(&ipats)
        };

        // `Pair(b: True, ..)` ∪ `Pair(a: False, ..)` MISS (a: True, b: False).
        // Source-order lowering read arm 1 as (a: True, b: _) → false
        // exhaustiveness (this returned `None` before the fix).
        let unsound = [
            pair_pat(&[("b", "True")], true),
            pair_pat(&[("a", "False")], true),
        ];
        assert_eq!(missing(&unsound).as_deref(), Some("Pair(True, False)"));

        // Genuinely exhaustive; the third arm names fields in reverse order.
        // Source-order lowering read it as a duplicate of arm 2 and reported a
        // bogus missing case (this returned `Some(...)` before the fix).
        let exhaustive = [
            pair_pat(&[("a", "True"), ("b", "True")], false),
            pair_pat(&[("a", "True"), ("b", "False")], false),
            pair_pat(&[("b", "True"), ("a", "False")], false),
            pair_pat(&[("a", "False"), ("b", "False")], false),
        ];
        assert_eq!(missing(&exhaustive), None);
    }
}
