module type_def

pub type Type = TypePrimitive
	| TypeArray
	| TypeOption
	| TypeFunction
	| TypeStruct
	| TypeEnum
	| TypeNone
	| TypeVar
	| TypeResult
	| TypeTuple

pub enum PrimitiveKind {
	t_int
	t_float
	t_string
	t_bool
}

pub struct TypePrimitive {
pub:
	kind PrimitiveKind
}

pub struct TypeArray {
pub:
	element Type
}

pub struct TypeOption {
pub:
	inner Type
}

pub struct TypeFunction {
pub:
	params     []Type
	ret        Type
	error_type ?Type
}

pub struct TypeStruct {
pub:
	id          int
	name        string
	type_params []string
	type_args   []Type
	fields      map[string]Type
}

pub struct TypeEnum {
pub:
	id          int
	name        string
	type_params []string
	type_args   []Type
	variants    map[string][]Type
}

pub struct TypeNone {}

pub struct TypeVar {
pub:
	name string
}

pub struct TypeResult {
pub:
	success Type // The T in T!E
	error   Type // The E in T!E
}

pub struct TypeTuple {
pub:
	elements []Type
}

pub fn t_int() Type {
	return TypePrimitive{
		kind: .t_int
	}
}

pub fn t_float() Type {
	return TypePrimitive{
		kind: .t_float
	}
}

pub fn t_string() Type {
	return TypePrimitive{
		kind: .t_string
	}
}

pub fn t_bool() Type {
	return TypePrimitive{
		kind: .t_bool
	}
}

pub fn t_none() Type {
	return TypeNone{}
}

pub fn t_var(name string) Type {
	return TypeVar{
		name: name
	}
}

pub fn t_array(element Type) Type {
	return TypeArray{
		element: element
	}
}

pub fn t_option(inner Type) Type {
	return TypeOption{
		inner: inner
	}
}

pub fn t_tuple(elements []Type) Type {
	return TypeTuple{
		elements: elements
	}
}

pub fn types_equal(a Type, b Type) bool {
	match a {
		TypePrimitive {
			if b is TypePrimitive {
				return a.kind == b.kind
			}
			return false
		}
		TypeArray {
			if b is TypeArray {
				return types_equal(a.element, b.element)
			}
			return false
		}
		TypeOption {
			if b is TypeOption {
				return types_equal(a.inner, b.inner)
			}
			return false
		}
		TypeFunction {
			if b is TypeFunction {
				if a.params.len != b.params.len {
					return false
				}
				for i, param in a.params {
					if !types_equal(param, b.params[i]) {
						return false
					}
				}
				if !types_equal(a.ret, b.ret) {
					return false
				}
				a_err := a.error_type
				b_err := b.error_type
				if a_err != none && b_err != none {
					return types_equal(a_err, b_err)
				}
				return a_err == none && b_err == none
			}
			return false
		}
		TypeStruct {
			if b is TypeStruct {
				return a.id == b.id
			}
			return false
		}
		TypeEnum {
			if b is TypeEnum {
				return a.id == b.id
			}
			return false
		}
		TypeNone {
			return b is TypeNone
		}
		TypeVar {
			if b is TypeVar {
				return a.name == b.name
			}
			return false
		}
		TypeResult {
			if b is TypeResult {
				return types_equal(a.success, b.success) && types_equal(a.error, b.error)
			}
			return false
		}
		TypeTuple {
			if b is TypeTuple {
				if a.elements.len != b.elements.len {
					return false
				}
				for i, elem in a.elements {
					if !types_equal(elem, b.elements[i]) {
						return false
					}
				}
				return true
			}
			return false
		}
	}
}

pub fn type_to_string(t Type) string {
	match t {
		TypePrimitive {
			return match t.kind {
				.t_int { 'Int' }
				.t_float { 'Float' }
				.t_string { 'String' }
				.t_bool { 'Bool' }
			}
		}
		TypeArray {
			return '[]${type_to_string(t.element)}'
		}
		TypeOption {
			return '?${type_to_string(t.inner)}'
		}
		TypeFunction {
			mut params := []string{}
			for param in t.params {
				params << type_to_string(param)
			}
			mut result := 'fn(${params.join(', ')}) ${type_to_string(t.ret)}'
			if err := t.error_type {
				result += '!${type_to_string(err)}'
			}
			return result
		}
		TypeStruct {
			if t.type_args.len > 0 {
				mut args := []string{}
				for arg in t.type_args {
					args << type_to_string(arg)
				}
				return '${t.name}(${args.join(', ')})'
			}
			return t.name
		}
		TypeEnum {
			if t.type_args.len > 0 {
				mut args := []string{}
				for arg in t.type_args {
					args << type_to_string(arg)
				}
				return '${t.name}(${args.join(', ')})'
			}
			return t.name
		}
		TypeNone {
			return 'none'
		}
		TypeVar {
			return t.name
		}
		TypeResult {
			return '${type_to_string(t.success)}!${type_to_string(t.error)}'
		}
		TypeTuple {
			mut elems := []string{}
			for elem in t.elements {
				elems << type_to_string(elem)
			}
			return '(${elems.join(', ')})'
		}
	}
}

pub fn is_numeric(t Type) bool {
	if t is TypeVar {
		return true // TypeVar might be numeric, will be constrained later
	}
	if t is TypePrimitive {
		return t.kind == .t_int || t.kind == .t_float
	}
	return false
}

pub fn substitute(t Type, subs map[string]Type) Type {
	match t {
		TypeVar {
			if concrete := subs[t.name] {
				return concrete
			}
			return t
		}
		TypeArray {
			return t_array(substitute(t.element, subs))
		}
		TypeOption {
			return t_option(substitute(t.inner, subs))
		}
		TypeFunction {
			mut new_params := []Type{}
			for param in t.params {
				new_params << substitute(param, subs)
			}
			return TypeFunction{
				params: new_params
				ret:    substitute(t.ret, subs)
			}
		}
		TypeResult {
			return TypeResult{
				success: substitute(t.success, subs)
				error:   substitute(t.error, subs)
			}
		}
		TypeTuple {
			mut new_elements := []Type{}
			for elem in t.elements {
				new_elements << substitute(elem, subs)
			}
			return t_tuple(new_elements)
		}
		TypeStruct {
			mut new_fields := map[string]Type{}
			for name, field_type in t.fields {
				new_fields[name] = substitute(field_type, subs)
			}
			mut new_type_args := []Type{}
			if t.type_args.len > 0 {
				for arg in t.type_args {
					new_type_args << substitute(arg, subs)
				}
			} else {
				for param in t.type_params {
					if concrete := subs[param] {
						new_type_args << concrete
					}
				}
			}
			return TypeStruct{
				id:          t.id
				name:        t.name
				type_params: t.type_params
				type_args:   new_type_args
				fields:      new_fields
			}
		}
		TypeEnum {
			mut new_variants := map[string][]Type{}
			for name, variant_types in t.variants {
				mut new_types := []Type{}
				for vt in variant_types {
					new_types << substitute(vt, subs)
				}
				new_variants[name] = new_types
			}
			mut new_type_args := []Type{}
			if t.type_args.len > 0 {
				for arg in t.type_args {
					new_type_args << substitute(arg, subs)
				}
			} else {
				for param in t.type_params {
					if concrete := subs[param] {
						new_type_args << concrete
					}
				}
			}
			return TypeEnum{
				id:          t.id
				name:        t.name
				type_params: t.type_params
				type_args:   new_type_args
				variants:    new_variants
			}
		}
		else {
			return t
		}
	}
}
