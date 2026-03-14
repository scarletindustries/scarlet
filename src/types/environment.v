module types

import type_def { Type }

// ============================================================================
// DefinitionLocation - where a name was defined
// ============================================================================

pub struct DefinitionLocation {
pub:
	line    int @[required]
	column  int @[required]
	end_col int @[required]
}

// ============================================================================
// TypeInfo - information about a registered type (struct or enum)
// ============================================================================

pub type TypeInfo = StructInfo | EnumInfo

pub struct StructInfo {
pub:
	id          int @[required]
	type_params []string
	fields      map[string]Type
}

pub struct EnumInfo {
pub:
	id          int @[required]
	type_params []string
	variants    map[string][]Type
}

// ============================================================================
// TypeEnv - the type environment
// ============================================================================

pub struct TypeEnv {
pub mut:
	scopes       []map[string]Scheme
	type_info    map[string]TypeInfo
	definitions  map[string]DefinitionLocation
	docs         map[string]string
	next_type_id int
}

pub fn new_env() TypeEnv {
	return TypeEnv{
		scopes:       [map[string]Scheme{}]
		type_info:    map[string]TypeInfo{}
		definitions:  map[string]DefinitionLocation{}
		docs:         map[string]string{}
		next_type_id: 1
	}
}

// --- Scope management ---

pub fn (mut e TypeEnv) push_scope() {
	e.scopes << map[string]Scheme{}
}

pub fn (mut e TypeEnv) pop_scope() {
	if e.scopes.len > 1 {
		e.scopes.pop()
	}
}

pub fn (e TypeEnv) scope_depth() int {
	return e.scopes.len
}

// --- Variable bindings ---

pub fn (mut e TypeEnv) define(name string, scheme Scheme) {
	if e.scopes.len > 0 {
		e.scopes[e.scopes.len - 1][name] = scheme
	}
}

pub fn (mut e TypeEnv) define_at(name string, scheme Scheme, loc DefinitionLocation) {
	e.define(name, Scheme{
		quantified: scheme.quantified
		ty:         scheme.ty
		def:        loc
	})
}

pub fn (e TypeEnv) lookup(name string) ?Scheme {
	for i := e.scopes.len - 1; i >= 0; i-- {
		if scheme := e.scopes[i][name] {
			return scheme
		}
	}
	return none
}

pub fn (mut e TypeEnv) store_doc(name string, doc string) {
	e.docs[name] = doc
}

pub fn (e TypeEnv) lookup_doc(name string) ?string {
	return e.docs[name] or { return none }
}

pub fn (e TypeEnv) lookup_definition(name string) ?DefinitionLocation {
	// First check scoped variable bindings (inner scopes shadow outer).
	for i := e.scopes.len - 1; i >= 0; i-- {
		if scheme := e.scopes[i][name] {
			if def := scheme.def {
				return def
			}
		}
	}
	// Then check global names (struct/enum/variant).
	return e.definitions[name] or { return none }
}

// --- Type registration ---

pub fn (mut e TypeEnv) register_struct(name string, type_params []string, fields map[string]Type) StructInfo {
	info := StructInfo{
		id:          e.next_type_id
		type_params: type_params
		fields:      fields
	}
	e.next_type_id++
	e.type_info[name] = info
	return info
}

pub fn (mut e TypeEnv) register_enum(name string, type_params []string, variants map[string][]Type) EnumInfo {
	info := EnumInfo{
		id:          e.next_type_id
		type_params: type_params
		variants:    variants
	}
	e.next_type_id++
	e.type_info[name] = info
	return info
}

pub fn (e TypeEnv) lookup_type_info(name string) ?TypeInfo {
	return e.type_info[name] or { return none }
}

// Find an enum with a variant of this name. Returns the enum name + info if
// exactly one enum has it; returns none if zero or multiple (ambiguous).
pub fn (e TypeEnv) find_variant_owner(variant_name string) ?(string, EnumInfo) {
	mut found_name := ''
	mut found_info := ?EnumInfo(none)
	mut count := 0
	for enum_name, info in e.type_info {
		if info is EnumInfo && variant_name in info.variants {
			found_name = enum_name
			found_info = info
			count++
		}
	}
	if count == 1 {
		if fi := found_info {
			return found_name, fi
		}
	}
	return none
}

// --- Name suggestions (for "did you mean X?") ---

fn (e TypeEnv) all_names() []string {
	mut names := []string{}
	for scope in e.scopes {
		for name, _ in scope {
			if name !in names {
				names << name
			}
		}
	}
	return names
}

fn levenshtein(a string, b string) int {
	if a.len == 0 {
		return b.len
	}
	if b.len == 0 {
		return a.len
	}

	mut prev := []int{len: b.len + 1}
	mut curr := []int{len: b.len + 1}

	for j in 0 .. b.len + 1 {
		prev[j] = j
	}

	for i in 1 .. a.len + 1 {
		curr[0] = i
		for j in 1 .. b.len + 1 {
			cost := if a[i - 1] == b[j - 1] { 0 } else { 1 }
			del := prev[j] + 1
			ins := curr[j - 1] + 1
			sub := prev[j - 1] + cost
			mut min := del
			if ins < min {
				min = ins
			}
			if sub < min {
				min = sub
			}
			curr[j] = min
		}
		prev = curr.clone()
	}

	return prev[b.len]
}

pub fn (e TypeEnv) suggest_name(name string) ?string {
	all := e.all_names()
	mut best := ''
	mut best_dist := 4 // max distance threshold

	for candidate in all {
		dist := levenshtein(name, candidate)
		if dist < best_dist {
			best_dist = dist
			best = candidate
		}
	}

	if best.len > 0 {
		return best
	}
	return none
}
