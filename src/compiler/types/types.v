module types

pub type Type = TypePrimitive
	| TypeArray
	| TypeOption
	| TypeFunction
	| TypeStruct
	| TypeEnum
	| TypeNone
	| TypeVar

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
	name   string
	fields map[string]Type
}

pub struct TypeEnum {
pub:
	name     string
	variants map[string]?Type
}

pub struct TypeNone {}

pub struct TypeVar {
pub:
	name string
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
				return types_equal(a.ret, b.ret)
			}
			return false
		}
		TypeStruct {
			if b is TypeStruct {
				return a.name == b.name
			}
			return false
		}
		TypeEnum {
			if b is TypeEnum {
				return a.name == b.name
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
			return 'fn(${params.join(', ')}) ${type_to_string(t.ret)}'
		}
		TypeStruct {
			return t.name
		}
		TypeEnum {
			return t.name
		}
		TypeNone {
			return 'None'
		}
		TypeVar {
			return t.name
		}
	}
}

pub fn is_numeric(t Type) bool {
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
		else {
			return t
		}
	}
}

