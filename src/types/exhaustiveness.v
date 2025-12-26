module types

import ast
import type_def { Type, TypeArray, TypeEnum, TypeNone, TypeOption, TypePrimitive, TypeResult, TypeStruct, TypeTuple }

type Pat = PatWildcard | PatCtor | PatOr

struct PatWildcard {}

struct PatCtor {
	name string
	args []Pat
}

struct PatOr {
	patterns []Pat
}

struct CtorInfo {
	name  string
	arity int
	types []Type
}

struct TypeCtors {
	ctors    []CtorInfo
	infinite bool
}

fn get_type_ctors(t Type) TypeCtors {
	match t {
		TypePrimitive {
			if t.kind == .t_bool {
				return TypeCtors{
					ctors: [
						CtorInfo{
							name:  'true'
							arity: 0
							types: []
						},
						CtorInfo{
							name:  'false'
							arity: 0
							types: []
						},
					]
				}
			}
			return TypeCtors{
				infinite: true
			}
		}
		TypeEnum {
			mut ctors := []CtorInfo{}
			for name, payload_types in t.variants {
				ctors << CtorInfo{
					name:  name
					arity: payload_types.len
					types: payload_types
				}
			}
			return TypeCtors{
				ctors: ctors
			}
		}
		TypeOption {
			return TypeCtors{
				ctors: [
					CtorInfo{
						name:  'some'
						arity: 1
						types: [t.inner]
					},
					CtorInfo{
						name:  'none'
						arity: 0
						types: []
					},
				]
			}
		}
		TypeResult {
			return TypeCtors{
				ctors: [
					CtorInfo{
						name:  'ok'
						arity: 1
						types: [t.success]
					},
					CtorInfo{
						name:  'err'
						arity: 1
						types: [t.error]
					},
				]
			}
		}
		TypeArray {
			return TypeCtors{
				ctors: [
					CtorInfo{
						name:  '[]'
						arity: 0
						types: []
					},
					CtorInfo{
						name:  '[..]'
						arity: 2
						types: [t.element, t]
					},
				]
			}
		}
		TypeTuple {
			return TypeCtors{
				ctors: [
					CtorInfo{
						name:  'tuple'
						arity: t.elements.len
						types: t.elements
					},
				]
			}
		}
		TypeStruct {
			mut field_types := []Type{}
			for _, ft in t.fields {
				field_types << ft
			}
			return TypeCtors{
				ctors: [
					CtorInfo{
						name:  t.name
						arity: field_types.len
						types: field_types
					},
				]
			}
		}
		TypeNone {
			return TypeCtors{
				ctors: [
					CtorInfo{
						name:  'none'
						arity: 0
						types: []
					},
				]
			}
		}
		else {
			return TypeCtors{
				infinite: true
			}
		}
	}
}

struct PatternRow {
	pats  []Pat
	types []Type
}

struct PatternMatrix {
mut:
	rows []PatternRow
}

fn (m PatternMatrix) is_empty() bool {
	return m.rows.len == 0
}

fn (m PatternMatrix) first_col_ctors() []string {
	mut seen := map[string]bool{}
	mut result := []string{}
	for row in m.rows {
		if row.pats.len > 0 {
			if row.pats[0] is PatCtor {
				ctor := row.pats[0] as PatCtor
				if ctor.name !in seen {
					seen[ctor.name] = true
					result << ctor.name
				}
			}
		}
	}
	return result
}

fn (m PatternMatrix) specialize(ctor CtorInfo) PatternMatrix {
	mut result := PatternMatrix{}
	for row in m.rows {
		if row.pats.len == 0 {
			continue
		}
		first := row.pats[0]
		rest_pats := row.pats[1..]
		rest_types := row.types[1..]

		match first {
			PatWildcard {
				mut new_pats := []Pat{}
				for _ in 0 .. ctor.arity {
					new_pats << PatWildcard{}
				}
				new_pats << rest_pats
				mut new_types := ctor.types.clone()
				new_types << rest_types
				result.rows << PatternRow{
					pats:  new_pats
					types: new_types
				}
			}
			PatCtor {
				if first.name == ctor.name {
					mut new_pats := first.args.clone()
					new_pats << rest_pats
					mut new_types := ctor.types.clone()
					new_types << rest_types
					result.rows << PatternRow{
						pats:  new_pats
						types: new_types
					}
				}
			}
			PatOr {
				for p in first.patterns {
					mut expanded_row := [p]
					expanded_row << rest_pats
					temp_matrix := PatternMatrix{
						rows: [
							PatternRow{
								pats:  expanded_row
								types: row.types
							},
						]
					}
					specialized := temp_matrix.specialize(ctor)
					result.rows << specialized.rows
				}
			}
		}
	}
	return result
}

fn (m PatternMatrix) default_matrix() PatternMatrix {
	mut result := PatternMatrix{}
	for row in m.rows {
		if row.pats.len == 0 {
			continue
		}
		first := row.pats[0]
		rest_pats := row.pats[1..]
		rest_types := row.types[1..]

		match first {
			PatWildcard {
				result.rows << PatternRow{
					pats:  rest_pats
					types: rest_types
				}
			}
			PatOr {
				for p in first.patterns {
					if p is PatWildcard {
						result.rows << PatternRow{
							pats:  rest_pats
							types: rest_types
						}
						break
					}
				}
			}
			else {}
		}
	}
	return result
}

fn is_complete(seen_ctors []string, type_ctors TypeCtors) bool {
	if type_ctors.infinite {
		return false
	}
	for c in type_ctors.ctors {
		if c.name !in seen_ctors {
			return false
		}
	}
	return true
}

fn is_useful(m PatternMatrix, row PatternRow) bool {
	if row.pats.len == 0 {
		return m.is_empty()
	}

	if row.types.len == 0 {
		return true
	}

	first_type := row.types[0]
	type_ctors := get_type_ctors(first_type)
	seen_ctors := m.first_col_ctors()

	first_pat := row.pats[0]
	match first_pat {
		PatWildcard {
			if is_complete(seen_ctors, type_ctors) {
				for ctor in type_ctors.ctors {
					specialized_m := m.specialize(ctor)
					mut new_pats := []Pat{}
					for _ in 0 .. ctor.arity {
						new_pats << PatWildcard{}
					}
					new_pats << row.pats[1..]
					mut new_types := ctor.types.clone()
					new_types << row.types[1..]
					if is_useful(specialized_m, PatternRow{ pats: new_pats, types: new_types }) {
						return true
					}
				}
				return false
			} else {
				return is_useful(m.default_matrix(), PatternRow{
					pats:  row.pats[1..]
					types: row.types[1..]
				})
			}
		}
		PatCtor {
			mut ctor_info := CtorInfo{
				name:  first_pat.name
				arity: first_pat.args.len
			}
			for c in type_ctors.ctors {
				if c.name == first_pat.name {
					ctor_info = c
					break
				}
			}
			specialized_m := m.specialize(ctor_info)
			mut new_pats := first_pat.args.clone()
			new_pats << row.pats[1..]
			mut new_types := ctor_info.types.clone()
			new_types << row.types[1..]
			return is_useful(specialized_m, PatternRow{ pats: new_pats, types: new_types })
		}
		PatOr {
			for p in first_pat.patterns {
				mut new_row := [p]
				new_row << row.pats[1..]
				if is_useful(m, PatternRow{ pats: new_row, types: row.types }) {
					return true
				}
			}
			return false
		}
	}
}

fn find_witness_vec(m PatternMatrix, types []Type) ?[]Pat {
	if types.len == 0 {
		if m.is_empty() {
			return []Pat{}
		}
		return none
	}

	first_type := types[0]
	type_ctors := get_type_ctors(first_type)
	seen_ctors := m.first_col_ctors()

	if is_complete(seen_ctors, type_ctors) {
		for ctor in type_ctors.ctors {
			specialized_m := m.specialize(ctor)
			mut sub_types := ctor.types.clone()
			sub_types << types[1..]
			if witness_vec := find_witness_vec(specialized_m, sub_types) {
				mut args := []Pat{}
				for i in 0 .. ctor.arity {
					if i < witness_vec.len {
						args << witness_vec[i]
					} else {
						args << PatWildcard{}
					}
				}
				mut result := []Pat{}
				result << PatCtor{
					name: ctor.name
					args: args
				}
				if witness_vec.len > ctor.arity {
					result << witness_vec[ctor.arity..]
				}
				return result
			}
		}
		return none
	} else {
		for ctor in type_ctors.ctors {
			if ctor.name !in seen_ctors {
				mut wildcards := []Pat{}
				for _ in 0 .. ctor.arity {
					wildcards << PatWildcard{}
				}
				mut result := []Pat{}
				result << PatCtor{
					name: ctor.name
					args: wildcards
				}
				for _ in 1 .. types.len {
					result << PatWildcard{}
				}
				return result
			}
		}
		if type_ctors.infinite {
			default_m := m.default_matrix()
			if witness_vec := find_witness_vec(default_m, types[1..]) {
				mut result := []Pat{}
				result << PatWildcard{}
				result << witness_vec
				return result
			}
		}
		return none
	}
}

fn find_witness(m PatternMatrix, types []Type) ?Pat {
	if witness_vec := find_witness_vec(m, types) {
		if witness_vec.len > 0 {
			return witness_vec[0]
		}
		return PatWildcard{}
	}
	return none
}

fn pat_to_string(p Pat, t Type) string {
	match p {
		PatWildcard {
			return '_'
		}
		PatCtor {
			type_ctors := get_type_ctors(t)
			mut ctor_types := []Type{}
			for c in type_ctors.ctors {
				if c.name == p.name {
					ctor_types = c.types.clone()
					break
				}
			}

			if p.name == 'true' || p.name == 'false' {
				return p.name
			}
			if p.name == 'none' {
				return 'none'
			}
			if p.name == 'some' {
				if p.args.len > 0 && ctor_types.len > 0 {
					return 'some(${pat_to_string(p.args[0], ctor_types[0])})'
				}
				return 'some(_)'
			}
			if p.name == 'ok' {
				if p.args.len > 0 && ctor_types.len > 0 {
					return 'ok(${pat_to_string(p.args[0], ctor_types[0])})'
				}
				return 'ok(_)'
			}
			if p.name == 'err' {
				if p.args.len > 0 && ctor_types.len > 0 {
					return 'err(${pat_to_string(p.args[0], ctor_types[0])})'
				}
				return 'err(_)'
			}
			if p.name == '[]' {
				return '[]'
			}
			if p.name == '[..]' {
				return '[_, ..]'
			}
			if p.name == 'tuple' {
				if p.args.len == 0 {
					return '()'
				}
				mut parts := []string{}
				for i, arg in p.args {
					arg_type := if i < ctor_types.len { ctor_types[i] } else { type_def.t_none() }
					parts << pat_to_string(arg, arg_type)
				}
				return '(${parts.join(', ')})'
			}
			if p.args.len == 0 {
				return p.name
			}
			mut parts := []string{}
			for i, arg in p.args {
				arg_type := if i < ctor_types.len { ctor_types[i] } else { type_def.t_none() }
				parts << pat_to_string(arg, arg_type)
			}
			return '${p.name}(${parts.join(', ')})'
		}
		PatOr {
			mut parts := []string{}
			for pp in p.patterns {
				parts << pat_to_string(pp, t)
			}
			return parts.join(' | ')
		}
	}
}

pub fn ast_pattern_to_pat(pattern ast.Expression, t Type) Pat {
	match pattern {
		ast.WildcardPattern {
			return PatWildcard{}
		}
		ast.Identifier {
			return PatWildcard{}
		}
		ast.BooleanLiteral {
			return PatCtor{
				name: if pattern.value { 'true' } else { 'false' }
				args: []
			}
		}
		ast.NumberLiteral {
			return PatCtor{
				name: 'lit:${pattern.value}'
				args: []
			}
		}
		ast.StringLiteral {
			return PatCtor{
				name: "lit:'${pattern.value}'"
				args: []
			}
		}
		ast.NoneExpression {
			return PatCtor{
				name: 'none'
				args: []
			}
		}
		ast.TupleExpression {
			mut args := []Pat{}
			element_types := if t is TypeTuple { t.elements } else { []Type{} }
			for i, elem in pattern.elements {
				elem_type := if i < element_types.len { element_types[i] } else { type_def.t_none() }
				args << ast_pattern_to_pat(elem, elem_type)
			}
			return PatCtor{
				name: 'tuple'
				args: args
			}
		}
		ast.ArrayExpression {
			if pattern.elements.len == 0 {
				return PatCtor{
					name: '[]'
					args: []
				}
			}
			last := pattern.elements.last()
			if last is ast.SpreadElement {
				elem_type := if t is TypeArray { t.element } else { type_def.t_none() }
				if pattern.elements.len == 1 {
					return PatCtor{
						name: '[..]'
						args: [PatWildcard{}, PatWildcard{}]
					}
				}
				first_elem := pattern.elements[0]
				first_pat := if first_elem is ast.Expression {
					ast_pattern_to_pat(first_elem, elem_type)
				} else {
					PatWildcard{}
				}
				return PatCtor{
					name: '[..]'
					args: [first_pat, PatWildcard{}]
				}
			}
			elem_type := if t is TypeArray { t.element } else { type_def.t_none() }
			first_elem := pattern.elements[0]
			first_pat := if first_elem is ast.Expression {
				ast_pattern_to_pat(first_elem, elem_type)
			} else {
				PatWildcard{}
			}
			if pattern.elements.len == 1 {
				return PatCtor{
					name: '[..]'
					args: [first_pat, PatCtor{
						name: '[]'
						args: []
					}]
				}
			}
			rest_expr := ast.ArrayExpression{
				elements: pattern.elements[1..]
				span:     pattern.span
			}

			rest_pat := ast_pattern_to_pat(rest_expr, t)
			return PatCtor{
				name: '[..]'
				args: [first_pat, rest_pat]
			}
		}
		ast.OrPattern {
			mut pats := []Pat{}
			for p in pattern.patterns {
				pats << ast_pattern_to_pat(p, t)
			}
			return PatOr{
				patterns: pats
			}
		}
		ast.RangeExpression {
			mut start_str := '_'
			mut end_str := '_'
			if pattern.start is ast.NumberLiteral {
				start_str = pattern.start.value
			}
			if pattern.end is ast.NumberLiteral {
				end_str = pattern.end.value
			}
			return PatCtor{
				name: 'range:${start_str}..${end_str}'
				args: []
			}
		}
		ast.FunctionCallExpression {
			name := pattern.identifier.name
			mut args := []Pat{}
			type_ctors := get_type_ctors(t)
			mut arg_types := []Type{}
			for c in type_ctors.ctors {
				if c.name == name {
					arg_types = c.types.clone()
					break
				}
			}

			for i, arg in pattern.arguments {
				arg_type := if i < arg_types.len { arg_types[i] } else { type_def.t_none() }
				args << ast_pattern_to_pat(arg, arg_type)
			}
			return PatCtor{
				name: name
				args: args
			}
		}
		ast.PropertyAccessExpression {
			if pattern.right is ast.Identifier {
				return PatCtor{
					name: pattern.right.name
					args: []
				}
			} else if pattern.right is ast.FunctionCallExpression {
				call := pattern.right as ast.FunctionCallExpression
				name := call.identifier.name
				mut args := []Pat{}

				type_ctors := get_type_ctors(t)
				mut arg_types := []Type{}
				for c in type_ctors.ctors {
					if c.name == name {
						arg_types = c.types.clone()
						break
					}
				}

				for i, arg in call.arguments {
					arg_type := if i < arg_types.len { arg_types[i] } else { type_def.t_none() }
					args << ast_pattern_to_pat(arg, arg_type)
				}
				return PatCtor{
					name: name
					args: args
				}
			}
			return PatWildcard{}
		}
		ast.UnaryExpression {
			if pattern.expression is ast.NumberLiteral {
				num := pattern.expression as ast.NumberLiteral
				return PatCtor{
					name: 'lit:-${num.value}'
					args: []
				}
			}
			// Boolean expressions as patterns (match true { !cond -> ... })
			// Cannot statically determine coverage, so treat each as unique/non-overlapping.
			// This means: no "unreachable pattern" warnings, and `else` is always required.
			return PatCtor{
				name: 'cond:${pattern.span.start_line}:${pattern.span.start_column}'
				args: []
			}
		}
		ast.BinaryExpression {
			// Boolean expressions as patterns (match true { cond -> ... })
			// Cannot statically determine coverage, so treat each as unique/non-overlapping.
			// This means: no "unreachable pattern" warnings, and `else` is always required.
			return PatCtor{
				name: 'cond:${pattern.span.start_line}:${pattern.span.start_column}'
				args: []
			}
		}
		ast.ArrayIndexExpression, ast.BlockExpression, ast.ErrorExpression, ast.ErrorNode,
		ast.FunctionExpression, ast.IfExpression, ast.InterpolatedString, ast.MatchExpression,
		ast.OrExpression, ast.StructInitExpression, ast.TypeIdentifier {
			return PatWildcard{}
		}
	}
}

pub fn check_pattern_useful(existing []Pat, new_pattern Pat, subject_type Type) bool {
	mut matrix := PatternMatrix{}
	for p in existing {
		matrix.rows << PatternRow{
			pats:  [p]
			types: [subject_type]
		}
	}

	new_row := PatternRow{
		pats:  [new_pattern]
		types: [subject_type]
	}

	return is_useful(matrix, new_row)
}

pub fn check_exhaustiveness(patterns []Pat, subject_type Type) ?string {
	mut matrix := PatternMatrix{}
	for p in patterns {
		matrix.rows << PatternRow{
			pats:  [p]
			types: [subject_type]
		}
	}

	wildcard_row := PatternRow{
		pats:  [PatWildcard{}]
		types: [subject_type]
	}
	if is_useful(matrix, wildcard_row) {
		if witness := find_witness(matrix, [subject_type]) {
			return pat_to_string(witness, subject_type)
		}
		return '_'
	}
	return none
}
