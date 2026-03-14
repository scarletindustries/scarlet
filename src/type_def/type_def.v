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
	kind PrimitiveKind @[required]
}

pub struct TypeArray {
pub:
	element Type @[required]
}

pub struct TypeOption {
pub:
	inner Type @[required]
}

pub struct TypeFunction {
pub:
	params     []Type
	ret        Type @[required]
	error_type ?Type
}

pub struct TypeStruct {
pub:
	id          int    @[required]
	name        string @[required]
	type_params []string
	type_args   []Type
	fields      map[string]Type
}

pub struct TypeEnum {
pub:
	id          int    @[required]
	name        string @[required]
	type_params []string
	type_args   []Type
	variants    map[string][]Type
}

pub struct TypeNone {}

pub struct TypeVar {
pub:
	name string @[required]
}

pub struct TypeResult {
pub:
	success Type @[required]
	error   Type @[required]
}

pub struct TypeTuple {
pub:
	elements []Type @[required]
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
