module token

pub const keyword_map = {
	'fn':       Kind.kw_function
	'return':   Kind.kw_return
	'break':    Kind.kw_break
	'continue': Kind.kw_continue
	'import':   Kind.kw_import
	'from':     Kind.kw_from
	'true':     Kind.kw_true
	'false':    Kind.kw_false
	'assert':   Kind.kw_assert
	'export':   Kind.kw_export
	'struct':   Kind.kw_struct
	'in':       Kind.kw_in
	'none':     Kind.kw_none
	'const':    Kind.kw_const
	'if':		Kind.kw_if
	'else':		Kind.kw_else
	'throw':	Kind.kw_throw
}
