module types

import type_def { Type, TypeEnum, TypeFunction, TypeStruct, t_bool, t_float, t_int, t_none, t_string }

pub struct TypeEnv {
mut:
	scopes       []map[string]Type
	functions    map[string]TypeFunction
	structs      map[string]TypeStruct
	enums        map[string]TypeEnum
	next_type_id int
}

pub fn new_env() TypeEnv {
	return TypeEnv{
		scopes:       [map[string]Type{}]
		functions:    map[string]TypeFunction{}
		structs:      map[string]TypeStruct{}
		enums:        map[string]TypeEnum{}
		next_type_id: 1
	}
}

pub fn (mut e TypeEnv) push_scope() {
	e.scopes << map[string]Type{}
}

pub fn (mut e TypeEnv) pop_scope() {
	if e.scopes.len > 1 {
		e.scopes.pop()
	}
}

pub fn (mut e TypeEnv) define(name string, t Type) {
	if e.scopes.len > 0 {
		e.scopes[e.scopes.len - 1][name] = t
	}
}

pub fn (e TypeEnv) lookup(name string) ?Type {
	for i := e.scopes.len - 1; i >= 0; i-- {
		if t := e.scopes[i][name] {
			return t
		}
	}
	return none
}

pub fn (mut e TypeEnv) register_struct(s TypeStruct) TypeStruct {
	registered := TypeStruct{
		...s
		id: e.next_type_id
	}
	e.next_type_id++
	e.structs[s.name] = registered
	return registered
}

pub fn (e TypeEnv) lookup_struct(name string) ?TypeStruct {
	return e.structs[name] or { return none }
}

pub fn (mut e TypeEnv) register_enum(en TypeEnum) TypeEnum {
	registered := TypeEnum{
		...en
		id: e.next_type_id
	}
	e.next_type_id++
	e.enums[en.name] = registered
	return registered
}

pub fn (mut e TypeEnv) register_function(name string, f TypeFunction) {
	e.functions[name] = f
}

pub fn (e TypeEnv) lookup_function(name string) ?TypeFunction {
	return e.functions[name] or { return none }
}

pub fn (e TypeEnv) lookup_type(name string) ?Type {
	match name {
		'Int' { return t_int() }
		'Float' { return t_float() }
		'String' { return t_string() }
		'Bool' { return t_bool() }
		'None' { return t_none() }
		else {}
	}

	if s := e.structs[name] {
		return Type(s)
	}

	if en := e.enums[name] {
		return Type(en)
	}

	return none
}

pub fn (e TypeEnv) lookup_enum_by_variant(variant_name string) ?TypeEnum {
	for _, enum_type in e.enums {
		if variant_name in enum_type.variants {
			return enum_type
		}
	}
	return none
}

pub fn (e TypeEnv) all_names() []string {
	mut names := []string{}
	for scope in e.scopes {
		for name, _ in scope {
			if name !in names {
				names << name
			}
		}
	}
	for name, _ in e.functions {
		if name !in names {
			names << name
		}
	}
	return names
}
