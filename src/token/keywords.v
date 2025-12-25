module token

pub const keyword_map = {
	'fn':     Kind.kw_function
	'import': Kind.kw_import
	'from':   Kind.kw_from
	'true':   Kind.kw_true
	'false':  Kind.kw_false
	'enum':   Kind.kw_enum
	'export': Kind.kw_export
	'struct': Kind.kw_struct
	'in':     Kind.kw_in
	'match':  Kind.kw_match
	'none':   Kind.kw_none
	'const':  Kind.kw_const
	'if':     Kind.kw_if
	'else':   Kind.kw_else
	'error':  Kind.kw_error
	'or':     Kind.kw_or
}
