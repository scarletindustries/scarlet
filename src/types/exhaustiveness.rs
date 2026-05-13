use crate::ast;
use crate::type_def::{self, PrimitiveKind, Type};

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
    types: Vec<Type>,
}

#[derive(Debug, Clone, Default)]
struct TypeCtors {
    ctors: Vec<CtorInfo>,
    infinite: bool,
}

fn get_type_ctors(t: &Type) -> TypeCtors {
    match t {
        Type::Primitive { kind } => {
            if *kind == PrimitiveKind::Bool {
                return TypeCtors {
                    ctors: vec![
                        CtorInfo {
                            name: "true".to_string(),
                            arity: 0,
                            types: vec![],
                        },
                        CtorInfo {
                            name: "false".to_string(),
                            arity: 0,
                            types: vec![],
                        },
                    ],
                    infinite: false,
                };
            }
            TypeCtors {
                ctors: vec![],
                infinite: true,
            }
        }
        Type::Enum { variants, .. } => {
            let mut ctors = Vec::new();
            for (name, payload_types) in variants {
                ctors.push(CtorInfo {
                    name: name.clone(),
                    arity: payload_types.len(),
                    types: payload_types.clone(),
                });
            }
            TypeCtors {
                ctors,
                infinite: false,
            }
        }
        Type::Option { inner } => TypeCtors {
            ctors: vec![
                CtorInfo {
                    name: "some".to_string(),
                    arity: 1,
                    types: vec![(**inner).clone()],
                },
                CtorInfo {
                    name: "none".to_string(),
                    arity: 0,
                    types: vec![],
                },
            ],
            infinite: false,
        },
        Type::Result { success, error } => TypeCtors {
            ctors: vec![
                CtorInfo {
                    name: "ok".to_string(),
                    arity: 1,
                    types: vec![(**success).clone()],
                },
                CtorInfo {
                    name: "err".to_string(),
                    arity: 1,
                    types: vec![(**error).clone()],
                },
            ],
            infinite: false,
        },
        Type::Array { element } => TypeCtors {
            ctors: vec![
                CtorInfo {
                    name: "[]".to_string(),
                    arity: 0,
                    types: vec![],
                },
                CtorInfo {
                    name: "[..]".to_string(),
                    arity: 2,
                    types: vec![(**element).clone(), t.clone()],
                },
            ],
            infinite: false,
        },
        Type::Tuple { elements } => TypeCtors {
            ctors: vec![CtorInfo {
                name: "tuple".to_string(),
                arity: elements.len(),
                types: elements.clone(),
            }],
            infinite: false,
        },
        Type::Struct { name, fields, .. } => {
            let field_types: Vec<Type> = fields.values().cloned().collect();
            TypeCtors {
                ctors: vec![CtorInfo {
                    name: name.clone(),
                    arity: field_types.len(),
                    types: field_types,
                }],
                infinite: false,
            }
        }
        Type::None => TypeCtors {
            ctors: vec![CtorInfo {
                name: "none".to_string(),
                arity: 0,
                types: vec![],
            }],
            infinite: false,
        },
        _ => TypeCtors {
            ctors: vec![],
            infinite: true,
        },
    }
}

#[derive(Debug, Clone)]
struct PatternRow {
    pats: Vec<Pat>,
    types: Vec<Type>,
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
        let mut seen: Vec<String> = Vec::new();
        for row in &self.rows {
            if !row.pats.is_empty()
                && let Pat::Ctor { name, .. } = &row.pats[0]
                && !seen.contains(name)
            {
                seen.push(name.clone());
            }
        }
        seen
    }

    fn specialize(&self, ctor: &CtorInfo) -> PatternMatrix {
        let mut result = PatternMatrix::default();
        for row in &self.rows {
            if row.pats.is_empty() {
                continue;
            }
            let first = &row.pats[0];
            let rest_pats = &row.pats[1..];
            let rest_types = &row.types[1..];

            match first {
                Pat::Wildcard => {
                    let mut new_pats: Vec<Pat> = Vec::new();
                    for _ in 0..ctor.arity {
                        new_pats.push(Pat::Wildcard);
                    }
                    new_pats.extend_from_slice(rest_pats);
                    let mut new_types = ctor.types.clone();
                    new_types.extend_from_slice(rest_types);
                    result.rows.push(PatternRow {
                        pats: new_pats,
                        types: new_types,
                    });
                }
                Pat::Ctor { name, args } => {
                    if *name == ctor.name {
                        let mut new_pats = args.clone();
                        new_pats.extend_from_slice(rest_pats);
                        let mut new_types = ctor.types.clone();
                        new_types.extend_from_slice(rest_types);
                        result.rows.push(PatternRow {
                            pats: new_pats,
                            types: new_types,
                        });
                    }
                }
                Pat::Or { patterns } => {
                    for p in patterns {
                        let mut expanded_row = vec![p.clone()];
                        expanded_row.extend_from_slice(rest_pats);
                        let temp_matrix = PatternMatrix {
                            rows: vec![PatternRow {
                                pats: expanded_row,
                                types: row.types.clone(),
                            }],
                        };
                        let specialized = temp_matrix.specialize(ctor);
                        result.rows.extend(specialized.rows);
                    }
                }
            }
        }
        result
    }

    fn default_matrix(&self) -> PatternMatrix {
        let mut result = PatternMatrix::default();
        for row in &self.rows {
            if row.pats.is_empty() {
                continue;
            }
            let first = &row.pats[0];
            let rest_pats = row.pats[1..].to_vec();
            let rest_types = row.types[1..].to_vec();

            match first {
                Pat::Wildcard => {
                    result.rows.push(PatternRow {
                        pats: rest_pats,
                        types: rest_types,
                    });
                }
                Pat::Or { patterns } => {
                    for p in patterns {
                        if let Pat::Wildcard = p {
                            result.rows.push(PatternRow {
                                pats: rest_pats,
                                types: rest_types,
                            });
                            break;
                        }
                    }
                }
                _ => {}
            }
        }
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
                    let mut new_pats: Vec<Pat> = Vec::new();
                    for _ in 0..ctor.arity {
                        new_pats.push(Pat::Wildcard);
                    }
                    new_pats.extend_from_slice(&row.pats[1..]);
                    let mut new_types = ctor.types.clone();
                    new_types.extend_from_slice(&row.types[1..]);
                    if is_useful(
                        &specialized_m,
                        &PatternRow {
                            pats: new_pats,
                            types: new_types,
                        },
                    ) {
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
            let mut ctor_info = CtorInfo {
                name: name.clone(),
                arity: args.len(),
                types: vec![],
            };
            for c in &type_ctors.ctors {
                if c.name == *name {
                    ctor_info = c.clone();
                    break;
                }
            }
            let specialized_m = m.specialize(&ctor_info);
            let mut new_pats = args.clone();
            new_pats.extend_from_slice(&row.pats[1..]);
            let mut new_types = ctor_info.types.clone();
            new_types.extend_from_slice(&row.types[1..]);
            is_useful(
                &specialized_m,
                &PatternRow {
                    pats: new_pats,
                    types: new_types,
                },
            )
        }
        Pat::Or { patterns } => {
            for p in patterns {
                let mut new_row = vec![p.clone()];
                new_row.extend_from_slice(&row.pats[1..]);
                if is_useful(
                    m,
                    &PatternRow {
                        pats: new_row,
                        types: row.types.clone(),
                    },
                ) {
                    return true;
                }
            }
            false
        }
    }
}

fn find_witness_vec(m: &PatternMatrix, types: &[Type]) -> Option<Vec<Pat>> {
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
                let mut args: Vec<Pat> = Vec::new();
                for i in 0..ctor.arity {
                    if i < witness_vec.len() {
                        args.push(witness_vec[i].clone());
                    } else {
                        args.push(Pat::Wildcard);
                    }
                }
                let mut result: Vec<Pat> = Vec::new();
                result.push(Pat::Ctor {
                    name: ctor.name.clone(),
                    args,
                });
                if witness_vec.len() > ctor.arity {
                    result.extend_from_slice(&witness_vec[ctor.arity..]);
                }
                return Some(result);
            }
        }
        None
    } else {
        for ctor in &type_ctors.ctors {
            if !seen_ctors.contains(&ctor.name) {
                let mut wildcards: Vec<Pat> = Vec::new();
                for _ in 0..ctor.arity {
                    wildcards.push(Pat::Wildcard);
                }
                let mut result: Vec<Pat> = Vec::new();
                result.push(Pat::Ctor {
                    name: ctor.name.clone(),
                    args: wildcards,
                });
                for _ in 1..types.len() {
                    result.push(Pat::Wildcard);
                }
                return Some(result);
            }
        }
        if type_ctors.infinite {
            let default_m = m.default_matrix();
            if let Some(witness_vec) = find_witness_vec(&default_m, &types[1..]) {
                let mut result: Vec<Pat> = Vec::new();
                result.push(Pat::Wildcard);
                result.extend(witness_vec);
                return Some(result);
            }
        }
        None
    }
}

fn find_witness(m: &PatternMatrix, types: &[Type]) -> Option<Pat> {
    if let Some(witness_vec) = find_witness_vec(m, types) {
        if !witness_vec.is_empty() {
            return Some(witness_vec[0].clone());
        }
        return Some(Pat::Wildcard);
    }
    None
}

fn pat_to_string(p: &Pat, t: &Type) -> String {
    match p {
        Pat::Wildcard => "_".to_string(),
        Pat::Ctor { name, args } => {
            let type_ctors = get_type_ctors(t);
            let mut ctor_types: Vec<Type> = Vec::new();
            for c in &type_ctors.ctors {
                if c.name == *name {
                    ctor_types = c.types.clone();
                    break;
                }
            }

            if name == "true" || name == "false" {
                return name.clone();
            }
            if name == "none" {
                return "none".to_string();
            }
            if name == "some" {
                if !args.is_empty() && !ctor_types.is_empty() {
                    return format!("some({})", pat_to_string(&args[0], &ctor_types[0]));
                }
                return "some(_)".to_string();
            }
            if name == "ok" {
                if !args.is_empty() && !ctor_types.is_empty() {
                    return format!("ok({})", pat_to_string(&args[0], &ctor_types[0]));
                }
                return "ok(_)".to_string();
            }
            if name == "err" {
                if !args.is_empty() && !ctor_types.is_empty() {
                    return format!("err({})", pat_to_string(&args[0], &ctor_types[0]));
                }
                return "err(_)".to_string();
            }
            if name == "[]" {
                return "[]".to_string();
            }
            if name == "[..]" {
                return "[_, ..]".to_string();
            }
            if name == "tuple" {
                if args.is_empty() {
                    return "()".to_string();
                }
                let mut parts: Vec<String> = Vec::new();
                for (i, arg) in args.iter().enumerate() {
                    let arg_type = if i < ctor_types.len() {
                        ctor_types[i].clone()
                    } else {
                        type_def::t_none()
                    };
                    parts.push(pat_to_string(arg, &arg_type));
                }
                return format!("({})", parts.join(", "));
            }
            if args.is_empty() {
                return name.clone();
            }
            let mut parts: Vec<String> = Vec::new();
            for (i, arg) in args.iter().enumerate() {
                let arg_type = if i < ctor_types.len() {
                    ctor_types[i].clone()
                } else {
                    type_def::t_none()
                };
                parts.push(pat_to_string(arg, &arg_type));
            }
            format!("{}({})", name, parts.join(", "))
        }
        Pat::Or { patterns } => {
            let parts: Vec<String> = patterns.iter().map(|pp| pat_to_string(pp, t)).collect();
            parts.join(" | ")
        }
    }
}

pub fn ast_pattern_to_pat(pattern: &ast::Expression, t: &Type) -> Pat {
    match pattern {
        ast::Expression::WildcardPattern(_) => Pat::Wildcard,
        ast::Expression::Identifier(_) => Pat::Wildcard,
        ast::Expression::BooleanLiteral(b) => Pat::Ctor {
            name: if b.value { "true" } else { "false" }.to_string(),
            args: vec![],
        },
        ast::Expression::NumberLiteral(n) => Pat::Ctor {
            name: format!("lit:{}", n.value),
            args: vec![],
        },
        ast::Expression::StringLiteral(s) => Pat::Ctor {
            name: format!("lit:'{}'", s.value),
            args: vec![],
        },
        ast::Expression::NoneExpression(_) => Pat::Ctor {
            name: "none".to_string(),
            args: vec![],
        },
        ast::Expression::TupleExpression(tup) => {
            let element_types: Vec<Type> = if let Type::Tuple { elements } = t {
                elements.clone()
            } else {
                vec![]
            };
            let mut args: Vec<Pat> = Vec::new();
            for (i, elem) in tup.elements.iter().enumerate() {
                let elem_type = if i < element_types.len() {
                    element_types[i].clone()
                } else {
                    type_def::t_none()
                };
                args.push(ast_pattern_to_pat(elem, &elem_type));
            }
            Pat::Ctor {
                name: "tuple".to_string(),
                args,
            }
        }
        ast::Expression::ArrayExpression(arr) => {
            if arr.elements.is_empty() {
                return Pat::Ctor {
                    name: "[]".to_string(),
                    args: vec![],
                };
            }
            let last = &arr.elements[arr.elements.len() - 1];
            if let ast::ArrayElement::SpreadElement(_) = last {
                let elem_type = if let Type::Array { element } = t {
                    (**element).clone()
                } else {
                    type_def::t_none()
                };
                if arr.elements.len() == 1 {
                    return Pat::Ctor {
                        name: "[..]".to_string(),
                        args: vec![Pat::Wildcard, Pat::Wildcard],
                    };
                }
                let first_elem = &arr.elements[0];
                let first_pat = if let ast::ArrayElement::Expression(e) = first_elem {
                    ast_pattern_to_pat(e, &elem_type)
                } else {
                    Pat::Wildcard
                };
                return Pat::Ctor {
                    name: "[..]".to_string(),
                    args: vec![first_pat, Pat::Wildcard],
                };
            }
            let elem_type = if let Type::Array { element } = t {
                (**element).clone()
            } else {
                type_def::t_none()
            };
            let first_elem = &arr.elements[0];
            let first_pat = if let ast::ArrayElement::Expression(e) = first_elem {
                ast_pattern_to_pat(e, &elem_type)
            } else {
                Pat::Wildcard
            };
            if arr.elements.len() == 1 {
                return Pat::Ctor {
                    name: "[..]".to_string(),
                    args: vec![
                        first_pat,
                        Pat::Ctor {
                            name: "[]".to_string(),
                            args: vec![],
                        },
                    ],
                };
            }
            let rest_expr = ast::Expression::ArrayExpression(ast::ArrayExpression {
                elements: arr.elements[1..].to_vec(),
                span: arr.span,
            });
            let rest_pat = ast_pattern_to_pat(&rest_expr, t);
            Pat::Ctor {
                name: "[..]".to_string(),
                args: vec![first_pat, rest_pat],
            }
        }
        ast::Expression::OrPattern(or) => {
            let pats: Vec<Pat> = or
                .patterns
                .iter()
                .map(|p| ast_pattern_to_pat(p, t))
                .collect();
            Pat::Or { patterns: pats }
        }
        ast::Expression::RangeExpression(r) => {
            let start_str = if let ast::Expression::NumberLiteral(n) = r.start.as_ref() {
                n.value.clone()
            } else {
                "_".to_string()
            };
            let end_str = if let ast::Expression::NumberLiteral(n) = r.end.as_ref() {
                n.value.clone()
            } else {
                "_".to_string()
            };
            Pat::Ctor {
                name: format!("range:{}..{}", start_str, end_str),
                args: vec![],
            }
        }
        ast::Expression::FunctionCallExpression(call) => {
            let name = call.identifier.name.clone();
            let type_ctors = get_type_ctors(t);
            let mut arg_types: Vec<Type> = Vec::new();
            for c in &type_ctors.ctors {
                if c.name == name {
                    arg_types = c.types.clone();
                    break;
                }
            }
            let mut args: Vec<Pat> = Vec::new();
            for (i, arg) in call.arguments.iter().enumerate() {
                let arg_type = if i < arg_types.len() {
                    arg_types[i].clone()
                } else {
                    type_def::t_none()
                };
                args.push(ast_pattern_to_pat(arg, &arg_type));
            }
            Pat::Ctor { name, args }
        }
        ast::Expression::PropertyAccessExpression(prop) => match prop.right.as_ref() {
            ast::Expression::Identifier(ident) => Pat::Ctor {
                name: ident.name.clone(),
                args: vec![],
            },
            ast::Expression::FunctionCallExpression(call) => {
                let name = call.identifier.name.clone();
                let type_ctors = get_type_ctors(t);
                let mut arg_types: Vec<Type> = Vec::new();
                for c in &type_ctors.ctors {
                    if c.name == name {
                        arg_types = c.types.clone();
                        break;
                    }
                }
                let mut args: Vec<Pat> = Vec::new();
                for (i, arg) in call.arguments.iter().enumerate() {
                    let arg_type = if i < arg_types.len() {
                        arg_types[i].clone()
                    } else {
                        type_def::t_none()
                    };
                    args.push(ast_pattern_to_pat(arg, &arg_type));
                }
                Pat::Ctor { name, args }
            }
            _ => Pat::Wildcard,
        },
        ast::Expression::UnaryExpression(u) => {
            if let ast::Expression::NumberLiteral(num) = u.expression.as_ref() {
                return Pat::Ctor {
                    name: format!("lit:-{}", num.value),
                    args: vec![],
                };
            }
            Pat::Ctor {
                name: format!("cond:{}:{}", u.span.start_line, u.span.start_column),
                args: vec![],
            }
        }
        ast::Expression::BinaryExpression(b) => Pat::Ctor {
            name: format!("cond:{}:{}", b.span.start_line, b.span.start_column),
            args: vec![],
        },
        ast::Expression::ArrayIndexExpression(_)
        | ast::Expression::BlockExpression(_)
        | ast::Expression::ErrorExpression(_)
        | ast::Expression::ErrorNode(_)
        | ast::Expression::FunctionExpression(_)
        | ast::Expression::IfExpression(_)
        | ast::Expression::InterpolatedString(_)
        | ast::Expression::MatchExpression(_)
        | ast::Expression::OrExpression(_)
        | ast::Expression::StructInitExpression(_)
        | ast::Expression::TypeIdentifier(_) => Pat::Wildcard,
    }
}

pub fn check_pattern_useful(existing: &[Pat], new_pattern: &Pat, subject_type: &Type) -> bool {
    let mut matrix = PatternMatrix::default();
    for p in existing {
        matrix.rows.push(PatternRow {
            pats: vec![p.clone()],
            types: vec![subject_type.clone()],
        });
    }

    let new_row = PatternRow {
        pats: vec![new_pattern.clone()],
        types: vec![subject_type.clone()],
    };

    is_useful(&matrix, &new_row)
}

pub fn check_exhaustiveness(patterns: &[Pat], subject_type: &Type) -> Option<String> {
    let mut matrix = PatternMatrix::default();
    for p in patterns {
        matrix.rows.push(PatternRow {
            pats: vec![p.clone()],
            types: vec![subject_type.clone()],
        });
    }

    let wildcard_row = PatternRow {
        pats: vec![Pat::Wildcard],
        types: vec![subject_type.clone()],
    };
    if is_useful(&matrix, &wildcard_row) {
        if let Some(witness) = find_witness(&matrix, std::slice::from_ref(subject_type)) {
            return Some(pat_to_string(&witness, subject_type));
        }
        return Some("_".to_string());
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::type_def::{t_bool, t_int, t_option, t_tuple};
    use indexmap::IndexMap;

    fn ctor(name: &str, args: Vec<Pat>) -> Pat {
        Pat::Ctor {
            name: name.to_string(),
            args,
        }
    }

    fn enum3() -> Type {
        let mut variants: IndexMap<String, Vec<Type>> = IndexMap::new();
        variants.insert("A".to_string(), vec![]);
        variants.insert("B".to_string(), vec![]);
        variants.insert("C".to_string(), vec![]);
        Type::Enum {
            id: 0,
            name: "Letter".to_string(),
            type_params: vec![],
            type_args: vec![],
            variants,
        }
    }

    #[test]
    fn bool_missing_false() {
        let pats = vec![ctor("true", vec![])];
        let result = check_exhaustiveness(&pats, &t_bool());
        assert_eq!(result, Some("false".to_string()));
    }

    #[test]
    fn bool_exhaustive() {
        let pats = vec![ctor("true", vec![]), ctor("false", vec![])];
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
        let pats = vec![
            ctor("tuple", vec![ctor("true", vec![]), ctor("true", vec![])]),
            ctor("tuple", vec![ctor("true", vec![]), ctor("false", vec![])]),
            ctor("tuple", vec![ctor("false", vec![]), ctor("true", vec![])]),
        ];
        let result = check_exhaustiveness(&pats, &t);
        assert_eq!(result, Some("(false, false)".to_string()));
    }

    #[test]
    fn tuple_bool_bool_exhaustive() {
        let t = t_tuple(vec![t_bool(), t_bool()]);
        let pats = vec![
            ctor("tuple", vec![ctor("true", vec![]), ctor("true", vec![])]),
            ctor("tuple", vec![ctor("true", vec![]), ctor("false", vec![])]),
            ctor("tuple", vec![ctor("false", vec![]), Pat::Wildcard]),
        ];
        let result = check_exhaustiveness(&pats, &t);
        assert_eq!(result, None);
    }

    #[test]
    fn nested_option_option_int_missing_some_none() {
        // ?(?Int): cover some(some(_)) and none → missing some(none)
        let t = t_option(t_option(t_int()));
        let pats = vec![
            ctor("some", vec![ctor("some", vec![Pat::Wildcard])]),
            ctor("none", vec![]),
        ];
        let result = check_exhaustiveness(&pats, &t);
        assert_eq!(result, Some("some(none)".to_string()));
    }

    #[test]
    fn nested_option_option_int_exhaustive() {
        let t = t_option(t_option(t_int()));
        let pats = vec![
            ctor("some", vec![ctor("some", vec![Pat::Wildcard])]),
            ctor("some", vec![ctor("none", vec![])]),
            ctor("none", vec![]),
        ];
        let result = check_exhaustiveness(&pats, &t);
        assert_eq!(result, None);
    }

    #[test]
    fn option_missing_none() {
        let t = t_option(t_int());
        let pats = vec![ctor("some", vec![Pat::Wildcard])];
        let result = check_exhaustiveness(&pats, &t);
        assert_eq!(result, Some("none".to_string()));
    }

    #[test]
    fn result_missing_err() {
        let t = Type::Result {
            success: Box::new(t_int()),
            error: Box::new(type_def::t_string()),
        };
        let pats = vec![ctor("ok", vec![Pat::Wildcard])];
        let result = check_exhaustiveness(&pats, &t);
        assert_eq!(result, Some("err(_)".to_string()));
    }

    #[test]
    fn array_missing_empty() {
        let t = type_def::t_array(t_int());
        let pats = vec![ctor("[..]", vec![Pat::Wildcard, Pat::Wildcard])];
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
        let existing = vec![ctor("true", vec![])];
        let new = Pat::Or {
            patterns: vec![ctor("true", vec![]), ctor("false", vec![])],
        };
        assert!(check_pattern_useful(&existing, &new, &t_bool()));
    }

    #[test]
    fn or_pattern_not_useful_if_all_covered() {
        let existing = vec![ctor("true", vec![]), ctor("false", vec![])];
        let new = Pat::Or {
            patterns: vec![ctor("true", vec![]), ctor("false", vec![])],
        };
        assert!(!check_pattern_useful(&existing, &new, &t_bool()));
    }

    #[test]
    fn check_pattern_useful_redundant() {
        let existing = vec![Pat::Wildcard];
        let new = ctor("true", vec![]);
        assert!(!check_pattern_useful(&existing, &new, &t_bool()));
    }

    #[test]
    fn check_pattern_useful_useful() {
        let existing = vec![ctor("true", vec![])];
        let new = ctor("false", vec![]);
        assert!(check_pattern_useful(&existing, &new, &t_bool()));
    }

    #[test]
    fn enum_with_payload_witness() {
        let mut variants: IndexMap<String, Vec<Type>> = IndexMap::new();
        variants.insert("Leaf".to_string(), vec![]);
        variants.insert("Node".to_string(), vec![t_int(), t_int()]);
        let t = Type::Enum {
            id: 0,
            name: "Tree".to_string(),
            type_params: vec![],
            type_args: vec![],
            variants,
        };
        let pats = vec![ctor("Leaf", vec![])];
        let result = check_exhaustiveness(&pats, &t);
        assert_eq!(result, Some("Node(_, _)".to_string()));
    }
}
