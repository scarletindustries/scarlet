module types

import type_def {
	Type,
	TypeArray,
	TypeEnum,
	TypeFunction,
	TypeNone,
	TypeOption,
	TypePrimitive,
	TypeResult,
	TypeStruct,
	TypeTuple,
	TypeVar,
	t_array,
	t_bool,
	t_float,
	t_int,
	t_none,
	t_option,
	t_string,
	t_tuple,
	t_var,
}
import diagnostic { Diagnostic }
import span { Span }

// ============================================================================
// Constraints (Elm-style constrained type variables)
// ============================================================================

pub enum Constraint {
	addable // Int, Float, String
	numeric // Int, Float
}

fn (c Constraint) allowed_types() []string {
	return match c {
		.addable { ['Int', 'Float', 'String'] }
		.numeric { ['Int', 'Float'] }
	}
}

fn (c Constraint) name() string {
	return match c {
		.addable { 'addable' }
		.numeric { 'numeric' }
	}
}

// ============================================================================
// InferType - the type representation used during inference
// ============================================================================

pub type InferType = IVar | ICon | IFun | ITuple

pub struct IVar {
pub:
	id int @[required]
}

pub struct ICon {
pub:
	name string @[required]
	args []InferType
}

pub struct IFun {
pub:
	params []InferType
	ret    InferType @[required]
	err    ?InferType
}

pub struct ITuple {
pub:
	elements []InferType @[required]
}

// ============================================================================
// TyVarState - union-find backing store
// ============================================================================

type TyVarState = Unbound | Link

struct Unbound {
	level      int @[required]
	constraint ?Constraint
}

struct Link {
	ty InferType @[required]
}

// ============================================================================
// Scheme - polymorphic type with quantified variables
// ============================================================================

pub struct Scheme {
pub:
	quantified []int
	ty         InferType @[required]
	def        ?DefinitionLocation
}

pub fn mono(ty InferType) Scheme {
	return Scheme{
		quantified: []
		ty:         ty
	}
}

// ============================================================================
// InferEngine - the HM inference engine
// ============================================================================

pub struct InferEngine {
mut:
	vars          []TyVarState
	next_var_id   int
	current_level int
	var_names     map[int]string // display names for type variables, assigned at generalization
pub mut:
	diagnostics []Diagnostic
	type_env    &TypeEnv = unsafe { nil }
}

pub fn new_engine() InferEngine {
	return InferEngine{
		vars:          []
		next_var_id:   0
		current_level: 0
		diagnostics:   []
	}
}

// --- Constructors for common types ---

pub fn (e &InferEngine) icon_int() InferType {
	return ICon{
		name: 'Int'
		args: []
	}
}

pub fn (e &InferEngine) icon_float() InferType {
	return ICon{
		name: 'Float'
		args: []
	}
}

pub fn (e &InferEngine) icon_string() InferType {
	return ICon{
		name: 'String'
		args: []
	}
}

pub fn (e &InferEngine) icon_bool() InferType {
	return ICon{
		name: 'Bool'
		args: []
	}
}

pub fn (e &InferEngine) icon_none() InferType {
	return ICon{
		name: 'None'
		args: []
	}
}

pub fn (e &InferEngine) icon_array(elem InferType) InferType {
	return ICon{
		name: 'Array'
		args: [elem]
	}
}

pub fn (e &InferEngine) icon_option(inner InferType) InferType {
	return ICon{
		name: 'Option'
		args: [inner]
	}
}

pub fn (e &InferEngine) icon_result(success InferType, err InferType) InferType {
	return ICon{
		name: 'Result'
		args: [success, err]
	}
}

pub fn (e &InferEngine) icon(name string, args []InferType) InferType {
	return ICon{
		name: name
		args: args
	}
}

// --- Annotation name tracking ---

pub fn (mut e InferEngine) name_var(ty InferType, name string) {
	if ty is IVar {
		e.var_names[ty.id] = name
	}
}

// --- Fresh variables ---

pub fn (mut e InferEngine) fresh_var() InferType {
	id := e.next_var_id
	e.vars << TyVarState(Unbound{
		level:      e.current_level
		constraint: none
	})
	e.next_var_id++
	return IVar{
		id: id
	}
}

pub fn (mut e InferEngine) fresh_constrained_var(c Constraint) InferType {
	id := e.next_var_id
	e.vars << TyVarState(Unbound{
		level:      e.current_level
		constraint: c
	})
	e.next_var_id++
	return IVar{
		id: id
	}
}

// --- Union-find: find with path compression ---

pub fn (mut e InferEngine) find(ty InferType) InferType {
	if ty is IVar {
		state := e.vars[ty.id]
		if state is Link {
			resolved := e.find(state.ty)
			e.vars[ty.id] = Link{
				ty: resolved
			}
			return resolved
		}
		return ty
	}
	return ty
}

// --- Level management ---

pub fn (mut e InferEngine) enter_level() {
	e.current_level++
}

pub fn (mut e InferEngine) leave_level() {
	e.current_level--
}

// --- Occurs check ---

fn (mut e InferEngine) occurs_in(var_id int, ty InferType) bool {
	resolved := e.find(ty)
	match resolved {
		IVar {
			return resolved.id == var_id
		}
		ICon {
			for arg in resolved.args {
				if e.occurs_in(var_id, arg) {
					return true
				}
			}
			return false
		}
		IFun {
			for param in resolved.params {
				if e.occurs_in(var_id, param) {
					return true
				}
			}
			if e.occurs_in(var_id, resolved.ret) {
				return true
			}
			if err := resolved.err {
				if e.occurs_in(var_id, err) {
					return true
				}
			}
			return false
		}
		ITuple {
			for elem in resolved.elements {
				if e.occurs_in(var_id, elem) {
					return true
				}
			}
			return false
		}
	}
}

// --- Unification ---

pub fn (mut e InferEngine) unify(a InferType, b InferType, s Span) bool {
	fa := e.find(a)
	fb := e.find(b)

	if fa is IVar && fb is IVar {
		if fa.id == fb.id {
			return true
		}
	}

	if fa is IVar {
		return e.unify_var(fa.id, fb, s)
	}
	if fb is IVar {
		return e.unify_var(fb.id, fa, s)
	}

	if fa is ICon && fb is ICon {
		if fa.name != fb.name {
			e.type_error(fa, fb, s)
			return false
		}
		if fa.args.len != fb.args.len {
			e.type_error(fa, fb, s)
			return false
		}
		for i, arg_a in fa.args {
			if !e.unify(arg_a, fb.args[i], s) {
				return false
			}
		}
		return true
	}

	if fa is IFun && fb is IFun {
		if fa.params.len != fb.params.len {
			e.type_error(fa, fb, s)
			return false
		}
		for i, param_a in fa.params {
			if !e.unify(param_a, fb.params[i], s) {
				return false
			}
		}
		if !e.unify(fa.ret, fb.ret, s) {
			return false
		}
		if fa_err := fa.err {
			if fb_err := fb.err {
				return e.unify(fa_err, fb_err, s)
			}
			e.type_error(fa, fb, s)
			return false
		}
		if fb.err != none {
			e.type_error(fa, fb, s)
			return false
		}
		return true
	}

	if fa is ITuple && fb is ITuple {
		if fa.elements.len != fb.elements.len {
			e.type_error(fa, fb, s)
			return false
		}
		for i, elem_a in fa.elements {
			if !e.unify(elem_a, fb.elements[i], s) {
				return false
			}
		}
		return true
	}

	e.type_error(fa, fb, s)
	return false
}

fn (mut e InferEngine) unify_var(var_id int, ty InferType, s Span) bool {
	if e.occurs_in(var_id, ty) {
		e.error_at_span('Infinite type detected', s)
		return false
	}

	state := e.vars[var_id]
	if state is Unbound {
		if constraint := state.constraint {
			return e.unify_constrained(var_id, constraint, state.level, ty, s)
		}
		e.vars[var_id] = Link{
			ty: ty
		}
		return true
	}

	return false
}

fn (mut e InferEngine) unify_constrained(var_id int, constraint Constraint, level int, ty InferType, s Span) bool {
	resolved := e.find(ty)
	match resolved {
		IVar {
			other_state := e.vars[resolved.id]
			if other_state is Unbound {
				if other_constraint := other_state.constraint {
					allowed_a := constraint.allowed_types()
					allowed_b := other_constraint.allowed_types()
					mut intersection := []string{}
					for t in allowed_a {
						if t in allowed_b {
							intersection << t
						}
					}
					if intersection.len == 0 {
						e.error_at_span("No type satisfies both '${constraint.name()}' and '${other_constraint.name()}'",
							s)
						return false
					}
					winner := if allowed_a.len <= allowed_b.len {
						constraint
					} else {
						other_constraint
					}
					e.vars[resolved.id] = Unbound{
						level:      if other_state.level < level { other_state.level } else { level }
						constraint: winner
					}
					e.vars[var_id] = Link{
						ty: resolved
					}
				} else {
					e.vars[resolved.id] = Unbound{
						level:      other_state.level
						constraint: constraint
					}
					e.vars[var_id] = Link{
						ty: resolved
					}
				}
				return true
			}
			e.vars[var_id] = Link{
				ty: resolved
			}
			return true
		}
		ICon {
			allowed := constraint.allowed_types()
			if resolved.name in allowed {
				e.vars[var_id] = Link{
					ty: resolved
				}
				return true
			}
			e.error_at_span("Type '${resolved.name}' is not ${constraint.name()} (expected one of: ${allowed.join(', ')})",
				s)
			return false
		}
		else {
			e.error_at_span('Type is not ${constraint.name()}', s)
			return false
		}
	}
}

// --- Generalization ---
//
// Assigns stable display names (A, B, C, ...) to quantified type variables.
// Names from source annotations (set via name_var) are preserved.

pub fn (mut e InferEngine) generalize(ty InferType) Scheme {
	mut quantified := []int{}
	e.collect_generalizable(ty, mut quantified)

	mut taken := map[string]bool{}
	for qvar in quantified {
		if name := e.var_names[qvar] {
			taken[name] = true
		}
	}

	mut next_letter := u8(`A`)
	for qvar in quantified {
		if _ := e.var_names[qvar] {
			continue
		}
		for next_letter <= u8(`Z`) {
			candidate := [next_letter].bytestr()
			next_letter++
			if candidate !in taken {
				e.var_names[qvar] = candidate
				taken[candidate] = true
				break
			}
		}
	}

	return Scheme{
		quantified: quantified
		ty:         ty
	}
}

fn (mut e InferEngine) collect_generalizable(ty InferType, mut quantified []int) {
	resolved := e.find(ty)
	match resolved {
		IVar {
			state := e.vars[resolved.id]
			if state is Unbound {
				if state.level > e.current_level && resolved.id !in quantified {
					quantified << resolved.id
				}
			}
		}
		ICon {
			for arg in resolved.args {
				e.collect_generalizable(arg, mut quantified)
			}
		}
		IFun {
			for param in resolved.params {
				e.collect_generalizable(param, mut quantified)
			}
			e.collect_generalizable(resolved.ret, mut quantified)
			if err := resolved.err {
				e.collect_generalizable(err, mut quantified)
			}
		}
		ITuple {
			for elem in resolved.elements {
				e.collect_generalizable(elem, mut quantified)
			}
		}
	}
}

// --- Instantiation ---

pub fn (mut e InferEngine) instantiate(scheme Scheme) InferType {
	if scheme.quantified.len == 0 {
		return scheme.ty
	}

	mut mapping := map[int]InferType{}
	for qvar in scheme.quantified {
		state := e.vars[qvar]
		new_var := if state is Unbound {
			if constraint := state.constraint {
				e.fresh_constrained_var(constraint)
			} else {
				e.fresh_var()
			}
		} else {
			e.fresh_var()
		}
		if name := e.var_names[qvar] {
			e.name_var(new_var, name)
		}
		mapping[qvar] = new_var
	}

	return e.substitute_vars(scheme.ty, mapping)
}

fn (mut e InferEngine) substitute_vars(ty InferType, mapping map[int]InferType) InferType {
	resolved := e.find(ty)
	match resolved {
		IVar {
			if replacement := mapping[resolved.id] {
				return replacement
			}
			return resolved
		}
		ICon {
			if resolved.args.len == 0 {
				return resolved
			}
			mut new_args := []InferType{cap: resolved.args.len}
			for arg in resolved.args {
				new_args << e.substitute_vars(arg, mapping)
			}
			return ICon{
				name: resolved.name
				args: new_args
			}
		}
		IFun {
			mut new_params := []InferType{cap: resolved.params.len}
			for param in resolved.params {
				new_params << e.substitute_vars(param, mapping)
			}
			new_ret := e.substitute_vars(resolved.ret, mapping)
			new_err := if err := resolved.err {
				?InferType(e.substitute_vars(err, mapping))
			} else {
				?InferType(none)
			}
			return IFun{
				params: new_params
				ret:    new_ret
				err:    new_err
			}
		}
		ITuple {
			mut new_elements := []InferType{cap: resolved.elements.len}
			for elem in resolved.elements {
				new_elements << e.substitute_vars(elem, mapping)
			}
			return ITuple{
				elements: new_elements
			}
		}
	}
}

// --- Resolution: InferType -> type_def.Type ---
//
// Converts an InferType to the concrete Type used elsewhere in the compiler.
// Unbound type variables resolve to TypeVar using display names assigned
// during generalization (stored in var_names).

pub fn (mut e InferEngine) resolve(ty InferType) Type {
	resolved := e.find(ty)
	match resolved {
		IVar {
			state := e.vars[resolved.id]
			if state is Unbound {
				if constraint := state.constraint {
					return t_var(constraint.name())
				}
				if name := e.var_names[resolved.id] {
					return t_var(name)
				}
				return t_var("'t${resolved.id}")
			}
			if name := e.var_names[resolved.id] {
				return t_var(name)
			}
			return t_var("'t${resolved.id}")
		}
		ICon {
			return e.resolve_icon(resolved)
		}
		IFun {
			mut params := []Type{cap: resolved.params.len}
			for param in resolved.params {
				params << e.resolve(param)
			}
			ret := e.resolve(resolved.ret)
			err_type := if err := resolved.err {
				?Type(e.resolve(err))
			} else {
				?Type(none)
			}
			return TypeFunction{
				params:     params
				ret:        ret
				error_type: err_type
			}
		}
		ITuple {
			mut elements := []Type{cap: resolved.elements.len}
			for elem in resolved.elements {
				elements << e.resolve(elem)
			}
			return t_tuple(elements)
		}
	}
}

fn (mut e InferEngine) resolve_icon(icon ICon) Type {
	if e.type_env != unsafe { nil } {
		if info := e.type_env.lookup_type_info(icon.name) {
			mut resolved_args := []Type{cap: icon.args.len}
			for arg in icon.args {
				resolved_args << e.resolve(arg)
			}
			match info {
				StructInfo {
					return TypeStruct{
						id:          info.id
						name:        icon.name
						type_params: info.type_params
						type_args:   resolved_args
						fields:      info.fields
					}
				}
				EnumInfo {
					return TypeEnum{
						id:          info.id
						name:        icon.name
						type_params: info.type_params
						type_args:   resolved_args
						variants:    info.variants
					}
				}
			}
		}
	}

	match icon.name {
		'Int' {
			return t_int()
		}
		'Float' {
			return t_float()
		}
		'String' {
			return t_string()
		}
		'Bool' {
			return t_bool()
		}
		'None' {
			return t_none()
		}
		'Array' {
			if icon.args.len > 0 {
				return t_array(e.resolve(icon.args[0]))
			}
			return t_array(t_var('T'))
		}
		'Option' {
			if icon.args.len > 0 {
				return t_option(e.resolve(icon.args[0]))
			}
			return t_option(t_var('T'))
		}
		'Result' {
			if icon.args.len >= 2 {
				return TypeResult{
					success: e.resolve(icon.args[0])
					error:   e.resolve(icon.args[1])
				}
			}
			return TypeResult{
				success: t_var('T')
				error:   t_var('E')
			}
		}
		else {
			return TypeVar{
				name: icon.name
			}
		}
	}
}

// --- Error helpers ---

fn (mut e InferEngine) error_at_span(message string, s Span) {
	e.diagnostics << Diagnostic{
		span:     s
		severity: .error
		message:  message
	}
}

fn (mut e InferEngine) type_error(a InferType, b InferType, s Span) {
	a_str := e.type_to_str(a)
	b_str := e.type_to_str(b)
	e.error_at_span("Type mismatch: expected '${a_str}', got '${b_str}'", s)
}

pub fn (mut e InferEngine) type_to_str(ty InferType) string {
	resolved := e.find(ty)
	match resolved {
		IVar {
			state := e.vars[resolved.id]
			if state is Unbound {
				if constraint := state.constraint {
					return constraint.name()
				}
				if name := e.var_names[resolved.id] {
					return name
				}
				return "'t${resolved.id}"
			}
			if name := e.var_names[resolved.id] {
				return name
			}
			return "'t${resolved.id}"
		}
		ICon {
			if resolved.args.len == 0 {
				return resolved.name
			}
			mut args := []string{cap: resolved.args.len}
			for arg in resolved.args {
				args << e.type_to_str(arg)
			}
			return '${resolved.name}(${args.join(', ')})'
		}
		IFun {
			mut params := []string{cap: resolved.params.len}
			for param in resolved.params {
				params << e.type_to_str(param)
			}
			mut result := 'fn(${params.join(', ')}) ${e.type_to_str(resolved.ret)}'
			if err := resolved.err {
				result += '!${e.type_to_str(err)}'
			}
			return result
		}
		ITuple {
			mut elements := []string{cap: resolved.elements.len}
			for elem in resolved.elements {
				elements << e.type_to_str(elem)
			}
			return '(${elements.join(', ')})'
		}
	}
}

pub fn (mut e InferEngine) type_to_infer_with_vars(t Type, var_mapping map[string]InferType) InferType {
	match t {
		TypeVar {
			if ivar := var_mapping[t.name] {
				return ivar
			}
			return e.fresh_var()
		}
		TypePrimitive {
			return match t.kind {
				.t_int { e.icon_int() }
				.t_float { e.icon_float() }
				.t_string { e.icon_string() }
				.t_bool { e.icon_bool() }
			}
		}
		TypeArray {
			return e.icon_array(e.type_to_infer_with_vars(t.element, var_mapping))
		}
		TypeOption {
			return e.icon_option(e.type_to_infer_with_vars(t.inner, var_mapping))
		}
		TypeFunction {
			mut params := []InferType{cap: t.params.len}
			for param in t.params {
				params << e.type_to_infer_with_vars(param, var_mapping)
			}
			err := if e_type := t.error_type {
				?InferType(e.type_to_infer_with_vars(e_type, var_mapping))
			} else {
				?InferType(none)
			}
			return IFun{
				params: params
				ret:    e.type_to_infer_with_vars(t.ret, var_mapping)
				err:    err
			}
		}
		TypeResult {
			return e.icon_result(e.type_to_infer_with_vars(t.success, var_mapping), e.type_to_infer_with_vars(t.error,
				var_mapping))
		}
		TypeTuple {
			mut elements := []InferType{cap: t.elements.len}
			for elem in t.elements {
				elements << e.type_to_infer_with_vars(elem, var_mapping)
			}
			return ITuple{
				elements: elements
			}
		}
		TypeStruct {
			mut args := []InferType{cap: t.type_args.len}
			for arg in t.type_args {
				args << e.type_to_infer_with_vars(arg, var_mapping)
			}
			if args.len == 0 {
				for param in t.type_params {
					if ivar := var_mapping[param] {
						args << ivar
					}
				}
			}
			return ICon{
				name: t.name
				args: args
			}
		}
		TypeEnum {
			mut args := []InferType{cap: t.type_args.len}
			for arg in t.type_args {
				args << e.type_to_infer_with_vars(arg, var_mapping)
			}
			if args.len == 0 {
				for param in t.type_params {
					if ivar := var_mapping[param] {
						args << ivar
					}
				}
			}
			return ICon{
				name: t.name
				args: args
			}
		}
		TypeNone {
			return e.icon_none()
		}
	}
}
