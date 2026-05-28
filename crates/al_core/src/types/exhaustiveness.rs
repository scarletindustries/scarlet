use crate::ast;
use crate::type_def::Type;
use indexmap::IndexMap;
use std::rc::Rc;

#[derive(Debug, Clone)]
pub enum Pat {
    Wildcard,
    Ctor { name: String, args: Vec<Pat> },
    Or { patterns: Vec<Pat> },
}

#[derive(Debug, Clone)]
struct CtorInfo {
    name: String,
    arity: usize,
    /// Field labels in declaration order (parallel to `types`). Empty for
    /// constructors without named fields (array `::`/`[]`, tuples). Used by
    /// `pattern_to_pat` to slot labeled pattern args into declaration order.
    labels: Vec<String>,
    types: Vec<RcType>,
}

#[derive(Debug, Clone, Default)]
struct TypeCtors {
    ctors: Vec<CtorInfo>,
    infinite: bool,
}

impl TypeCtors {
    fn find(&self, name: &str) -> Option<&CtorInfo> {
        self.ctors.iter().find(|c| c.name == name)
    }
}

/// Synthetic constructor name for an n-ary tuple. Kept in lockstep with the
/// lowering in `pattern_to_pat` so the matrix algorithm sees a single ctor per
/// tuple arity.
fn tuple_ctor_name(n: usize) -> String {
    format!("#tuple{}", n)
}

/// Interned, structurally-shared projection of `type_def::Type` carrying only
/// what the usefulness matrix needs: the constructor set of a nominal/array/
/// tuple type. Lowered from a `&Type` once at each public entry point
/// (`pattern_to_pat`, `check_exhaustiveness`, `UsefulnessMatrix::new`). The
/// compiler calls `pattern_to_pat` once per match arm, so an N-arm match
/// performs N+2 of these O(type-size) lowerings. Each is still strictly
/// cheaper than the pre-interning code: thereafter every clone the Maranget
/// recursion performs (`splice`, `get_type_ctors`, `row.types.clone()`, …) is
/// an `Rc` refcount bump rather than an O(subtree-size) deep copy of the
/// nested `Named` variant table. Collapsing the N+2 to a single lowering —
/// lower once in `compile_match` and thread `&RcType` through — is possible
/// but a cross-cutting API change (these entry points take `&Type`), deferred.
///
/// `type_def::Type` itself can't hold the `Rc` — it is built with struct
/// literals in the inferencer — so the interning is local to this module.
#[derive(Debug, Clone)]
enum RcType {
    /// No finite constructor set: primitives, functions, type variables, and
    /// opaque/unresolved nominal types (empty variant table). `get_type_ctors`
    /// reports these as `infinite`, so a wildcard arm is required.
    Infinite,
    Array(Rc<RcType>),
    /// ctor name -> per-field label + interned type, in declaration order.
    /// Non-empty by construction (an empty variant table lowers to `Infinite`).
    Named(Rc<IndexMap<String, Vec<RcField>>>),
    Tuple(Rc<Vec<RcType>>),
}

/// A constructor field: its declaration label paired with its interned type.
/// The label lets `pattern_to_pat` slot labeled pattern args into
/// field-declaration order, matching `slot_ctor_args` in the compiler.
#[derive(Debug, Clone)]
struct RcField {
    label: String,
    ty: RcType,
}

/// One-time O(type-size) lowering of a `&Type` into the shared `RcType` graph.
/// `Type::Named.variants` already carries field types substituted for the
/// concrete `type_args`, so no environment lookup is needed here.
fn rc_type(t: &Type) -> RcType {
    match t {
        Type::Primitive { .. } | Type::Function { .. } | Type::Var { .. } => RcType::Infinite,
        Type::Array { element } => RcType::Array(Rc::new(rc_type(element))),
        Type::Tuple { elements } => RcType::Tuple(Rc::new(elements.iter().map(rc_type).collect())),
        Type::Named { variants, .. } => {
            if variants.is_empty() {
                RcType::Infinite
            } else {
                let table: IndexMap<String, Vec<RcField>> = variants
                    .iter()
                    .map(|(name, fields)| {
                        (
                            name.clone(),
                            fields
                                .iter()
                                .map(|f| RcField {
                                    label: f.label.clone(),
                                    ty: rc_type(&f.ty),
                                })
                                .collect(),
                        )
                    })
                    .collect();
                RcType::Named(Rc::new(table))
            }
        }
    }
}

fn get_type_ctors(t: &RcType) -> TypeCtors {
    match t {
        RcType::Infinite => TypeCtors {
            ctors: vec![],
            infinite: true,
        },
        RcType::Named(variants) => {
            let mut ctors = Vec::with_capacity(variants.len());
            for (name, fields) in variants.iter() {
                ctors.push(CtorInfo {
                    name: name.clone(),
                    arity: fields.len(),
                    labels: fields.iter().map(|f| f.label.clone()).collect(),
                    // Per-field type clone: one allocation + O(arity) refcount
                    // bumps, not an O(subtree-size) recursive copy.
                    types: fields.iter().map(|f| f.ty.clone()).collect(),
                });
            }
            TypeCtors {
                ctors,
                infinite: false,
            }
        }
        RcType::Array(element) => TypeCtors {
            ctors: vec![
                CtorInfo {
                    name: "[]".to_string(),
                    arity: 0,
                    labels: vec![],
                    types: vec![],
                },
                CtorInfo {
                    name: "::".to_string(),
                    arity: 2,
                    labels: vec![],
                    types: vec![(**element).clone(), t.clone()],
                },
            ],
            infinite: false,
        },
        RcType::Tuple(elements) => TypeCtors {
            ctors: vec![CtorInfo {
                name: tuple_ctor_name(elements.len()),
                arity: elements.len(),
                labels: vec![],
                types: elements.as_ref().clone(),
            }],
            infinite: false,
        },
    }
}

#[derive(Debug, Clone)]
struct PatternRow {
    pats: Vec<Pat>,
    types: Vec<RcType>,
}

impl PatternRow {
    /// Build a row whose pats are `head ++ rest_pats` and whose types are
    /// `ctor_types ++ rest_types` — the splice that `specialize`/`is_useful`
    /// perform when expanding a constructor in the first column.
    fn splice(
        mut head: Vec<Pat>,
        ctor_types: &[RcType],
        rest_pats: &[Pat],
        rest_types: &[RcType],
    ) -> Self {
        head.extend_from_slice(rest_pats);
        let mut types = ctor_types.to_vec();
        types.extend_from_slice(rest_types);
        PatternRow { pats: head, types }
    }
}

#[derive(Debug, Clone, Default)]
struct PatternMatrix {
    rows: Vec<PatternRow>,
}

impl PatternMatrix {
    fn is_empty(&self) -> bool {
        self.rows.is_empty()
    }

    fn first_col_ctors(&self) -> Vec<String> {
        fn collect(p: &Pat, seen: &mut Vec<String>) {
            match p {
                Pat::Ctor { name, .. } => {
                    if !seen.contains(name) {
                        seen.push(name.clone());
                    }
                }
                Pat::Or { patterns } => {
                    for inner in patterns {
                        collect(inner, seen);
                    }
                }
                Pat::Wildcard => {}
            }
        }
        let mut seen: Vec<String> = Vec::new();
        for row in &self.rows {
            if let Some(first) = row.pats.first() {
                collect(first, &mut seen);
            }
        }
        seen
    }

    /// Visit every row's first column with Or-patterns recursively flattened,
    /// so `f` only ever sees `Wildcard` or `Ctor` heads. Shared iteration core
    /// for `specialize` and `default_matrix`.
    fn for_each_head(&self, mut f: impl FnMut(&Pat, &[Pat], &[RcType])) {
        fn go(p: &Pat, rp: &[Pat], rt: &[RcType], f: &mut impl FnMut(&Pat, &[Pat], &[RcType])) {
            match p {
                Pat::Or { patterns } => {
                    for a in patterns {
                        go(a, rp, rt, f);
                    }
                }
                _ => f(p, rp, rt),
            }
        }
        for row in &self.rows {
            if let Some((first, rest)) = row.pats.split_first() {
                go(first, rest, &row.types[1..], &mut f);
            }
        }
    }

    fn specialize(&self, ctor: &CtorInfo) -> PatternMatrix {
        let mut result = PatternMatrix::default();
        self.for_each_head(|head, rest_p, rest_t| match head {
            Pat::Wildcard => result.rows.push(PatternRow::splice(
                vec![Pat::Wildcard; ctor.arity],
                &ctor.types,
                rest_p,
                rest_t,
            )),
            Pat::Ctor { name, args } if *name == ctor.name => result.rows.push(PatternRow::splice(
                args.clone(),
                &ctor.types,
                rest_p,
                rest_t,
            )),
            _ => {}
        });
        result
    }

    fn default_matrix(&self) -> PatternMatrix {
        let mut result = PatternMatrix::default();
        self.for_each_head(|head, rest_p, rest_t| {
            if matches!(head, Pat::Wildcard) {
                result.rows.push(PatternRow {
                    pats: rest_p.to_vec(),
                    types: rest_t.to_vec(),
                });
            }
        });
        result
    }
}

fn is_complete(seen_ctors: &[String], type_ctors: &TypeCtors) -> bool {
    if type_ctors.infinite {
        return false;
    }
    for c in &type_ctors.ctors {
        if !seen_ctors.contains(&c.name) {
            return false;
        }
    }
    true
}

fn is_useful(m: &PatternMatrix, row: &PatternRow) -> bool {
    if row.pats.is_empty() {
        return m.is_empty();
    }

    if row.types.is_empty() {
        return true;
    }

    let first_type = &row.types[0];
    let type_ctors = get_type_ctors(first_type);
    let seen_ctors = m.first_col_ctors();

    let first_pat = &row.pats[0];
    match first_pat {
        Pat::Wildcard => {
            if is_complete(&seen_ctors, &type_ctors) {
                for ctor in &type_ctors.ctors {
                    let specialized_m = m.specialize(ctor);
                    let head = vec![Pat::Wildcard; ctor.arity];
                    let new_row =
                        PatternRow::splice(head, &ctor.types, &row.pats[1..], &row.types[1..]);
                    if is_useful(&specialized_m, &new_row) {
                        return true;
                    }
                }
                false
            } else {
                is_useful(
                    &m.default_matrix(),
                    &PatternRow {
                        pats: row.pats[1..].to_vec(),
                        types: row.types[1..].to_vec(),
                    },
                )
            }
        }
        Pat::Ctor { name, args } => {
            let ctor_info = type_ctors.find(name).cloned().unwrap_or_else(|| CtorInfo {
                name: name.clone(),
                arity: args.len(),
                labels: vec![],
                types: vec![],
            });
            let specialized_m = m.specialize(&ctor_info);
            let new_row = PatternRow::splice(
                args.clone(),
                &ctor_info.types,
                &row.pats[1..],
                &row.types[1..],
            );
            is_useful(&specialized_m, &new_row)
        }
        Pat::Or { patterns } => patterns.iter().any(|p| {
            let mut pats = vec![p.clone()];
            pats.extend_from_slice(&row.pats[1..]);
            is_useful(
                m,
                &PatternRow {
                    pats,
                    types: row.types.clone(),
                },
            )
        }),
    }
}

fn find_witness_vec(m: &PatternMatrix, types: &[RcType]) -> Option<Vec<Pat>> {
    if types.is_empty() {
        if m.is_empty() {
            return Some(vec![]);
        }
        return None;
    }

    let first_type = &types[0];
    let type_ctors = get_type_ctors(first_type);
    let seen_ctors = m.first_col_ctors();

    if is_complete(&seen_ctors, &type_ctors) {
        for ctor in &type_ctors.ctors {
            let specialized_m = m.specialize(ctor);
            let mut sub_types = ctor.types.clone();
            sub_types.extend_from_slice(&types[1..]);
            if let Some(witness_vec) = find_witness_vec(&specialized_m, &sub_types) {
                let args: Vec<Pat> = (0..ctor.arity)
                    .map(|i| witness_vec.get(i).cloned().unwrap_or(Pat::Wildcard))
                    .collect();
                let head = Pat::Ctor {
                    name: ctor.name.clone(),
                    args,
                };
                let tail = witness_vec.get(ctor.arity..).unwrap_or(&[]);
                return Some(std::iter::once(head).chain(tail.iter().cloned()).collect());
            }
        }
        None
    } else {
        for ctor in &type_ctors.ctors {
            if !seen_ctors.contains(&ctor.name) {
                let mut result = vec![Pat::Ctor {
                    name: ctor.name.clone(),
                    args: vec![Pat::Wildcard; ctor.arity],
                }];
                result.resize(types.len(), Pat::Wildcard);
                return Some(result);
            }
        }
        if type_ctors.infinite {
            let default_m = m.default_matrix();
            if let Some(witness_vec) = find_witness_vec(&default_m, &types[1..]) {
                return Some(std::iter::once(Pat::Wildcard).chain(witness_vec).collect());
            }
        }
        None
    }
}

/// Canonical key for a `<<>>` pattern's shape. Two binary patterns lower to the
/// same key iff they are guaranteed to match the same set of `Binary` values, so
/// the matrix algorithm flags exact-shape duplicates as unreachable while still
/// treating `Binary` as infinite (a wildcard / `else` arm is required for
/// exhaustiveness). Dynamic sizes and non-literal segment values are keyed by
/// span so we never emit a false-positive unreachable error.
fn bin_pattern_key(segments: &[ast::BinSegmentPat], has_rest: bool) -> String {
    use std::fmt::Write;
    let mut key = String::from("#bin:");
    for seg in segments {
        match &seg.value {
            ast::Pattern::Wildcard { .. } | ast::Pattern::Var { .. } => key.push('_'),
            ast::Pattern::Literal(ast::PatternLiteral::Number(n)) => key.push_str(&n.value),
            ast::Pattern::Literal(ast::PatternLiteral::String(s)) => {
                let _ = write!(key, "'{}'", s.value);
            }
            other => {
                let sp = other.span();
                let _ = write!(key, "@{}:{}", sp.start_line, sp.start_column);
            }
        }
        match seg.kind {
            ast::BinKind::Int => key.push('i'),
            ast::BinKind::Binary => key.push('b'),
            ast::BinKind::Utf8 => key.push('u'),
        }
        match &seg.size {
            None => {}
            Some(ast::Expression::NumberLiteral(n)) => {
                key.push_str(&n.value);
                if seg.unit == ast::BinUnit::Bytes {
                    key.push('B');
                }
            }
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

/// Placeholder type for sub-patterns whose type could not be resolved from the
/// surrounding context. Falls through to `infinite: true` in `get_type_ctors`.
fn unknown_type() -> RcType {
    RcType::Infinite
}

fn pat_to_string(p: &Pat, t: &RcType) -> String {
    match p {
        Pat::Wildcard => "_".to_string(),
        Pat::Ctor { name, args } => {
            if name == "[]" {
                return "[]".to_string();
            }
            if name == "::" {
                return "[_, ..]".to_string();
            }
            let type_ctors = get_type_ctors(t);
            let ctor_types = type_ctors
                .find(name)
                .map(|c| c.types.as_slice())
                .unwrap_or(&[]);
            let parts: Vec<String> = args
                .iter()
                .enumerate()
                .map(|(i, a)| pat_to_string(a, ctor_types.get(i).unwrap_or(&unknown_type())))
                .collect();
            if name.starts_with("#tuple") {
                return format!("({})", parts.join(", "));
            }
            if parts.is_empty() {
                return name.clone();
            }
            format!("{}({})", name, parts.join(", "))
        }
        Pat::Or { patterns } => {
            let parts: Vec<String> = patterns.iter().map(|pp| pat_to_string(pp, t)).collect();
            parts.join(" | ")
        }
    }
}

/// Lower an `ast::Pattern` into the `Pat` form used by the matrix algorithm.
/// The `t` argument is the type the pattern is being matched against; it is
/// used to recover sub-pattern types for constructor arguments and to pad
/// `Ctor(..)` rest-patterns to the constructor's full arity.
pub fn pattern_to_pat(p: &ast::Pattern, t: &Type) -> Pat {
    pattern_to_pat_rc(p, &rc_type(t))
}

fn pattern_to_pat_rc(p: &ast::Pattern, t: &RcType) -> Pat {
    match p {
        ast::Pattern::Wildcard { .. } | ast::Pattern::Var { .. } => Pat::Wildcard,
        ast::Pattern::Literal(lit) => match lit {
            ast::PatternLiteral::Number(n) => Pat::Ctor {
                name: format!("lit:{}", n.value),
                args: vec![],
            },
            ast::PatternLiteral::String(s) => Pat::Ctor {
                name: format!("lit:'{}'", s.value),
                args: vec![],
            },
        },
        ast::Pattern::Constructor { name, args, .. } => {
            let type_ctors = get_type_ctors(t);
            let pat_args = match type_ctors.find(&name.name) {
                Some(ctor) => {
                    // Slot args into field-DECLARATION order, mirroring the
                    // compiler's `slot_ctor_args`: positional args fill
                    // declaration slots left to right, labeled args land at
                    // their field's declaration index, and any slot left empty
                    // (including those covered by a `..` rest) becomes a
                    // wildcard. Lowering in source order instead would permute
                    // the usefulness matrix relative to real match semantics,
                    // yielding unsound false-exhaustiveness (and false
                    // positives) whenever labels name fields out of order.
                    let arity = ctor.types.len();
                    let mut slots: Vec<Option<Pat>> = vec![None; arity];
                    let mut next_positional = 0usize;
                    for a in args {
                        let (idx, inner) = match a {
                            ast::PatternArg::Positional(p) => {
                                let i = next_positional;
                                next_positional += 1;
                                (i, p)
                            }
                            ast::PatternArg::Labeled { label, pattern } => (
                                ctor.labels
                                    .iter()
                                    .position(|fl| fl == &label.name)
                                    .unwrap_or(arity),
                                pattern,
                            ),
                        };
                        if idx < arity {
                            slots[idx] = Some(pattern_to_pat_rc(inner, &ctor.types[idx]));
                        }
                    }
                    slots
                        .into_iter()
                        .map(|p| p.unwrap_or(Pat::Wildcard))
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
                        pattern_to_pat_rc(inner, &unknown_type())
                    })
                    .collect(),
            };
            Pat::Ctor {
                name: name.name.clone(),
                args: pat_args,
            }
        }
        ast::Pattern::Tuple { elements, .. } => {
            let elem_types: Vec<RcType> = if let RcType::Tuple(ts) = t {
                ts.as_ref().clone()
            } else {
                vec![]
            };
            let mut args: Vec<Pat> = Vec::with_capacity(elements.len());
            for (i, e) in elements.iter().enumerate() {
                let sub_t = if i < elem_types.len() {
                    elem_types[i].clone()
                } else {
                    unknown_type()
                };
                args.push(pattern_to_pat_rc(e, &sub_t));
            }
            Pat::Ctor {
                name: tuple_ctor_name(elements.len()),
                args,
            }
        }
        ast::Pattern::Array { elements, .. } => {
            let elem_type = if let RcType::Array(element) = t {
                (**element).clone()
            } else {
                unknown_type()
            };
            // Build a cons-list right-to-left. A spread element terminates the
            // chain with a wildcard tail (matches any remaining list, including
            // empty); otherwise the chain ends in `[]`.
            let mut acc = Pat::Ctor {
                name: "[]".to_string(),
                args: vec![],
            };
            for el in elements.iter().rev() {
                match el {
                    ast::ArrayPatternElement::Spread { .. } => {
                        acc = Pat::Wildcard;
                    }
                    ast::ArrayPatternElement::Pattern(p) => {
                        acc = Pat::Ctor {
                            name: "::".to_string(),
                            args: vec![pattern_to_pat_rc(p, &elem_type), acc],
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
                Pat::Ctor {
                    name: bin_pattern_key(segments, rest.is_some()),
                    args: vec![],
                }
            }
        }
        ast::Pattern::Or { patterns, .. } => Pat::Or {
            patterns: patterns.iter().map(|p| pattern_to_pat_rc(p, t)).collect(),
        },
        ast::Pattern::Range { start, end, .. } => {
            let bound_str = |b: &ast::Pattern| -> String {
                if let ast::Pattern::Literal(ast::PatternLiteral::Number(n)) = b {
                    n.value.clone()
                } else {
                    "_".to_string()
                }
            };
            Pat::Ctor {
                name: format!("range:{}..{}", bound_str(start), bound_str(end)),
                args: vec![],
            }
        }
    }
}

/// Incremental usefulness checker for a single match expression. The compiler
/// pushes each unguarded arm once after its usefulness check, so N arms cost
/// O(N) row constructions instead of the O(N²) incurred by rebuilding the
/// matrix from a `&[Pat]` slice on every arm.
#[derive(Debug)]
pub struct UsefulnessMatrix {
    matrix: PatternMatrix,
    subject_type: RcType,
}

impl UsefulnessMatrix {
    pub fn new(subject_type: Type) -> Self {
        Self {
            matrix: PatternMatrix::default(),
            subject_type: rc_type(&subject_type),
        }
    }

    pub fn is_useful(&self, pat: &Pat) -> bool {
        let row = PatternRow {
            pats: vec![pat.clone()],
            types: vec![self.subject_type.clone()],
        };
        is_useful(&self.matrix, &row)
    }

    pub fn push(&mut self, pat: Pat) {
        self.matrix.rows.push(PatternRow {
            pats: vec![pat],
            types: vec![self.subject_type.clone()],
        });
    }
}

pub fn check_exhaustiveness(patterns: &[Pat], subject_type: &Type) -> Option<String> {
    let subject = rc_type(subject_type);
    let mut matrix = PatternMatrix::default();
    for p in patterns {
        matrix.rows.push(PatternRow {
            pats: vec![p.clone()],
            types: vec![subject.clone()],
        });
    }

    let wildcard_row = PatternRow {
        pats: vec![Pat::Wildcard],
        types: vec![subject.clone()],
    };
    if is_useful(&matrix, &wildcard_row) {
        if let Some(witness_vec) = find_witness_vec(&matrix, std::slice::from_ref(&subject)) {
            let witness = witness_vec.into_iter().next().unwrap_or(Pat::Wildcard);
            return Some(pat_to_string(&witness, &subject));
        }
        return Some("_".to_string());
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::type_def::{self, FieldDef, t_int, t_named, t_tuple};
    use indexmap::IndexMap;

    /// `Bool` is no longer a primitive; build the same `Named` shape the
    /// real prelude produces so the matrix tests exercise the variant path.
    fn t_bool() -> Type {
        let mut v: IndexMap<String, Vec<FieldDef>> = IndexMap::new();
        v.insert("True".into(), vec![]);
        v.insert("False".into(), vec![]);
        t_named(99, "Bool", vec![], v)
    }

    fn ctor(name: &str, args: Vec<Pat>) -> Pat {
        Pat::Ctor {
            name: name.to_string(),
            args,
        }
    }

    fn t_option(inner: Type) -> Type {
        let mut variants: IndexMap<String, Vec<FieldDef>> = IndexMap::new();
        variants.insert(
            "Some".to_string(),
            vec![FieldDef {
                label: "value".to_string(),
                ty: inner,
            }],
        );
        variants.insert("None".to_string(), vec![]);
        t_named(1, "Option", vec![], variants)
    }

    fn t_result(ok: Type, err: Type) -> Type {
        let mut variants: IndexMap<String, Vec<FieldDef>> = IndexMap::new();
        variants.insert(
            "Ok".to_string(),
            vec![FieldDef {
                label: "value".to_string(),
                ty: ok,
            }],
        );
        variants.insert(
            "Err".to_string(),
            vec![FieldDef {
                label: "error".to_string(),
                ty: err,
            }],
        );
        t_named(2, "Result", vec![], variants)
    }

    fn enum3() -> Type {
        let mut variants: IndexMap<String, Vec<FieldDef>> = IndexMap::new();
        variants.insert("A".to_string(), vec![]);
        variants.insert("B".to_string(), vec![]);
        variants.insert("C".to_string(), vec![]);
        t_named(10, "Letter", vec![], variants)
    }

    #[test]
    fn bool_missing_false() {
        let pats = vec![ctor("True", vec![])];
        let result = check_exhaustiveness(&pats, &t_bool());
        assert_eq!(result, Some("False".to_string()));
    }

    #[test]
    fn bool_exhaustive() {
        let pats = vec![ctor("True", vec![]), ctor("False", vec![])];
        let result = check_exhaustiveness(&pats, &t_bool());
        assert_eq!(result, None);
    }

    #[test]
    fn bool_wildcard_exhaustive() {
        let pats = vec![Pat::Wildcard];
        let result = check_exhaustiveness(&pats, &t_bool());
        assert_eq!(result, None);
    }

    #[test]
    fn enum_missing_third_variant() {
        let t = enum3();
        let pats = vec![ctor("A", vec![]), ctor("B", vec![])];
        let result = check_exhaustiveness(&pats, &t);
        assert_eq!(result, Some("C".to_string()));
    }

    #[test]
    fn enum_all_variants_exhaustive() {
        let t = enum3();
        let pats = vec![ctor("A", vec![]), ctor("B", vec![]), ctor("C", vec![])];
        let result = check_exhaustiveness(&pats, &t);
        assert_eq!(result, None);
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
        let result = check_exhaustiveness(&pats, &t);
        assert_eq!(result, Some("(False, False)".to_string()));
    }

    #[test]
    fn tuple_bool_bool_exhaustive() {
        let t = t_tuple(vec![t_bool(), t_bool()]);
        let tn = tuple_ctor_name(2);
        let pats = vec![
            ctor(&tn, vec![ctor("True", vec![]), ctor("True", vec![])]),
            ctor(&tn, vec![ctor("True", vec![]), ctor("False", vec![])]),
            ctor(&tn, vec![ctor("False", vec![]), Pat::Wildcard]),
        ];
        let result = check_exhaustiveness(&pats, &t);
        assert_eq!(result, None);
    }

    #[test]
    fn nested_option_option_int_missing_some_none() {
        // Option(Option(Int)): cover Some(Some(_)) and None → missing Some(None)
        let t = t_option(t_option(t_int()));
        let pats = vec![
            ctor("Some", vec![ctor("Some", vec![Pat::Wildcard])]),
            ctor("None", vec![]),
        ];
        let result = check_exhaustiveness(&pats, &t);
        assert_eq!(result, Some("Some(None)".to_string()));
    }

    #[test]
    fn nested_option_option_int_exhaustive() {
        let t = t_option(t_option(t_int()));
        let pats = vec![
            ctor("Some", vec![ctor("Some", vec![Pat::Wildcard])]),
            ctor("Some", vec![ctor("None", vec![])]),
            ctor("None", vec![]),
        ];
        let result = check_exhaustiveness(&pats, &t);
        assert_eq!(result, None);
    }

    #[test]
    fn option_missing_none() {
        let t = t_option(t_int());
        let pats = vec![ctor("Some", vec![Pat::Wildcard])];
        let result = check_exhaustiveness(&pats, &t);
        assert_eq!(result, Some("None".to_string()));
    }

    #[test]
    fn result_missing_err() {
        let t = t_result(t_int(), type_def::t_string());
        let pats = vec![ctor("Ok", vec![Pat::Wildcard])];
        let result = check_exhaustiveness(&pats, &t);
        assert_eq!(result, Some("Err(_)".to_string()));
    }

    #[test]
    fn array_missing_empty() {
        let t = type_def::t_array(t_int());
        let pats = vec![ctor("::", vec![Pat::Wildcard, Pat::Wildcard])];
        let result = check_exhaustiveness(&pats, &t);
        assert_eq!(result, Some("[]".to_string()));
    }

    #[test]
    fn array_missing_nonempty() {
        let t = type_def::t_array(t_int());
        let pats = vec![ctor("[]", vec![])];
        let result = check_exhaustiveness(&pats, &t);
        assert_eq!(result, Some("[_, ..]".to_string()));
    }

    #[test]
    fn int_literal_never_exhaustive() {
        let pats = vec![ctor("lit:1", vec![]), ctor("lit:2", vec![])];
        let result = check_exhaustiveness(&pats, &t_int());
        assert_eq!(result, Some("_".to_string()));
    }

    #[test]
    fn or_pattern_useful_if_any_alt_useful() {
        // Or in the test row goes through is_useful's PatOr branch: useful if any alt is.
        let mut m = UsefulnessMatrix::new(t_bool());
        m.push(ctor("True", vec![]));
        let new = Pat::Or {
            patterns: vec![ctor("True", vec![]), ctor("False", vec![])],
        };
        assert!(m.is_useful(&new));
    }

    #[test]
    fn or_pattern_not_useful_if_all_covered() {
        let mut m = UsefulnessMatrix::new(t_bool());
        m.push(ctor("True", vec![]));
        m.push(ctor("False", vec![]));
        let new = Pat::Or {
            patterns: vec![ctor("True", vec![]), ctor("False", vec![])],
        };
        assert!(!m.is_useful(&new));
    }

    #[test]
    fn usefulness_redundant_after_wildcard() {
        let mut m = UsefulnessMatrix::new(t_bool());
        m.push(Pat::Wildcard);
        assert!(!m.is_useful(&ctor("True", vec![])));
    }

    #[test]
    fn usefulness_distinct_ctor() {
        let mut m = UsefulnessMatrix::new(t_bool());
        m.push(ctor("True", vec![]));
        assert!(m.is_useful(&ctor("False", vec![])));
    }

    #[test]
    fn enum_with_payload_witness() {
        let mut variants: IndexMap<String, Vec<FieldDef>> = IndexMap::new();
        variants.insert("Leaf".to_string(), vec![]);
        variants.insert(
            "Node".to_string(),
            vec![
                FieldDef {
                    label: "left".to_string(),
                    ty: t_int(),
                },
                FieldDef {
                    label: "right".to_string(),
                    ty: t_int(),
                },
            ],
        );
        let t = t_named(11, "Tree", vec![], variants);
        let pats = vec![ctor("Leaf", vec![])];
        let result = check_exhaustiveness(&pats, &t);
        assert_eq!(result, Some("Node(_, _)".to_string()));
    }

    /// Opaque prelude `Binary` — Named with no variants → infinite in
    /// `get_type_ctors`, mirroring how the real prelude registers it.
    fn t_binary() -> Type {
        t_named(98, "Binary", vec![], IndexMap::new())
    }

    fn bin_seg(value: ast::Pattern, size: Option<&str>) -> ast::BinSegmentPat {
        ast::BinSegmentPat {
            value,
            size: size.map(|n| {
                ast::Expression::NumberLiteral(ast::NumberLiteral {
                    value: n.to_string(),
                    span: crate::span::point_span(0, 0),
                })
            }),
            unit: ast::BinUnit::Bits,
            kind: ast::BinKind::Int,
            span: crate::span::point_span(0, 0),
        }
    }

    fn bin_pat(segments: Vec<ast::BinSegmentPat>, rest: bool) -> ast::Pattern {
        ast::Pattern::Binary {
            segments,
            rest: rest.then(|| ast::BinaryPatternRest {
                binding: None,
                span: crate::span::point_span(0, 0),
            }),
            span: crate::span::point_span(0, 0),
        }
    }

    fn p_var(name: &str) -> ast::Pattern {
        ast::Pattern::Var {
            name: ast::Identifier {
                name: name.to_string(),
                span: crate::span::point_span(0, 0),
            },
        }
    }

    #[test]
    fn binary_fixed_segments_not_exhaustive() {
        let t = t_binary();
        let p = pattern_to_pat(
            &bin_pat(
                vec![bin_seg(p_var("a"), None), bin_seg(p_var("b"), None)],
                false,
            ),
            &t,
        );
        let result = check_exhaustiveness(&[p], &t);
        assert_eq!(result, Some("_".to_string()));
    }

    #[test]
    fn binary_empty_literal_not_exhaustive() {
        let t = t_binary();
        let p = pattern_to_pat(&bin_pat(vec![], false), &t);
        let result = check_exhaustiveness(&[p], &t);
        assert_eq!(result, Some("_".to_string()));
    }

    #[test]
    fn binary_rest_only_is_exhaustive() {
        let t = t_binary();
        let p = pattern_to_pat(&bin_pat(vec![], true), &t);
        assert!(matches!(p, Pat::Wildcard));
        let result = check_exhaustiveness(&[p], &t);
        assert_eq!(result, None);
    }

    #[test]
    fn binary_segments_with_rest_not_exhaustive() {
        let t = t_binary();
        let p = pattern_to_pat(&bin_pat(vec![bin_seg(p_var("a"), None)], true), &t);
        let result = check_exhaustiveness(&[p], &t);
        assert_eq!(result, Some("_".to_string()));
    }

    #[test]
    fn binary_fixed_then_else_exhaustive() {
        let t = t_binary();
        let p1 = pattern_to_pat(
            &bin_pat(
                vec![bin_seg(p_var("a"), None), bin_seg(p_var("b"), None)],
                false,
            ),
            &t,
        );
        let result = check_exhaustiveness(&[p1, Pat::Wildcard], &t);
        assert_eq!(result, None);
    }

    #[test]
    fn binary_same_shape_redundant() {
        let t = t_binary();
        let mut m = UsefulnessMatrix::new(t.clone());
        let p1 = pattern_to_pat(&bin_pat(vec![bin_seg(p_var("a"), Some("8"))], false), &t);
        let p2 = pattern_to_pat(&bin_pat(vec![bin_seg(p_var("b"), Some("8"))], false), &t);
        m.push(p1);
        assert!(!m.is_useful(&p2));
    }

    #[test]
    fn binary_different_literal_useful() {
        let t = t_binary();
        let mut m = UsefulnessMatrix::new(t.clone());
        let lit = |v: &str| {
            ast::Pattern::Literal(ast::PatternLiteral::Number(ast::NumberLiteral {
                value: v.to_string(),
                span: crate::span::point_span(0, 0),
            }))
        };
        let p1 = pattern_to_pat(&bin_pat(vec![bin_seg(lit("1"), None)], false), &t);
        let p2 = pattern_to_pat(&bin_pat(vec![bin_seg(lit("2"), None)], false), &t);
        m.push(p1);
        assert!(m.is_useful(&p2));
    }

    /// A nullary constructor pattern such as `True`.
    fn p_ctor0(name: &str) -> ast::Pattern {
        ast::Pattern::Constructor {
            name: ast::Identifier {
                name: name.to_string(),
                span: crate::span::point_span(0, 0),
            },
            args: vec![],
            rest: false,
            span: crate::span::point_span(0, 0),
        }
    }

    /// `Name(label: pat, ...)` with all-labeled args and an optional `..` rest.
    fn p_ctor_labeled(name: &str, fields: Vec<(&str, ast::Pattern)>, rest: bool) -> ast::Pattern {
        ast::Pattern::Constructor {
            name: ast::Identifier {
                name: name.to_string(),
                span: crate::span::point_span(0, 0),
            },
            args: fields
                .into_iter()
                .map(|(label, pattern)| ast::PatternArg::Labeled {
                    label: ast::Identifier {
                        name: label.to_string(),
                        span: crate::span::point_span(0, 0),
                    },
                    pattern,
                })
                .collect(),
            rest,
            span: crate::span::point_span(0, 0),
        }
    }

    /// `type Pair { a Bool b Bool }` — `a` is declaration index 0, `b` is 1.
    fn pair_bool_bool() -> Type {
        let mut variants: IndexMap<String, Vec<FieldDef>> = IndexMap::new();
        variants.insert(
            "Pair".to_string(),
            vec![
                FieldDef {
                    label: "a".to_string(),
                    ty: t_bool(),
                },
                FieldDef {
                    label: "b".to_string(),
                    ty: t_bool(),
                },
            ],
        );
        t_named(50, "Pair", vec![], variants)
    }

    /// `pattern_to_pat` must slot labeled constructor args into field-
    /// DECLARATION order, mirroring the compiler's `slot_ctor_args` and the
    /// runtime matcher. Lowering in source order permuted the usefulness
    /// matrix, so a non-exhaustive match looked exhaustive (unsound) and a
    /// genuinely exhaustive one looked incomplete.
    #[test]
    fn pattern_to_pat_slots_labeled_args_in_declaration_order() {
        let pair = pair_bool_bool();

        // `Pair(b: True, ..)` ∪ `Pair(a: False, ..)` MISS (a: True, b: False).
        // Source-order lowering read arm 1 as (a: True, b: _) → false
        // exhaustiveness (this returned `None` before the fix).
        let unsound = vec![
            pattern_to_pat(
                &p_ctor_labeled("Pair", vec![("b", p_ctor0("True"))], true),
                &pair,
            ),
            pattern_to_pat(
                &p_ctor_labeled("Pair", vec![("a", p_ctor0("False"))], true),
                &pair,
            ),
        ];
        assert_eq!(
            check_exhaustiveness(&unsound, &pair),
            Some("Pair(True, _)".to_string())
        );

        // Genuinely exhaustive; the third arm names fields in reverse order.
        // Source-order lowering read it as a duplicate of arm 2 and reported a
        // bogus missing case (this returned `Some(...)` before the fix).
        let exhaustive = vec![
            pattern_to_pat(
                &p_ctor_labeled(
                    "Pair",
                    vec![("a", p_ctor0("True")), ("b", p_ctor0("True"))],
                    false,
                ),
                &pair,
            ),
            pattern_to_pat(
                &p_ctor_labeled(
                    "Pair",
                    vec![("a", p_ctor0("True")), ("b", p_ctor0("False"))],
                    false,
                ),
                &pair,
            ),
            pattern_to_pat(
                &p_ctor_labeled(
                    "Pair",
                    vec![("b", p_ctor0("True")), ("a", p_ctor0("False"))],
                    false,
                ),
                &pair,
            ),
            pattern_to_pat(
                &p_ctor_labeled(
                    "Pair",
                    vec![("a", p_ctor0("False")), ("b", p_ctor0("False"))],
                    false,
                ),
                &pair,
            ),
        ];
        assert_eq!(check_exhaustiveness(&exhaustive, &pair), None);
    }
}
