use std::collections::{HashMap, HashSet};

use super::environment::{DefinitionLocation, TypeEnv, TypeInfo};
use crate::diagnostic::{Diagnostic, Severity};
use crate::span::Span;
use crate::type_def::{
    PrimitiveKind, Type, t_array, t_bool, t_float, t_int, t_none, t_option, t_string, t_tuple,
    t_var,
};

// ============================================================================
// Constraints (Elm-style constrained type variables)
// ============================================================================

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Constraint {
    Addable,
    Numeric,
}

impl Constraint {
    pub fn allowed_types(&self) -> Vec<&'static str> {
        match self {
            Constraint::Addable => vec!["Int", "Float", "String"],
            Constraint::Numeric => vec!["Int", "Float"],
        }
    }

    pub fn name(&self) -> &'static str {
        match self {
            Constraint::Addable => "addable",
            Constraint::Numeric => "numeric",
        }
    }
}

// ============================================================================
// InferType - the type representation used during inference
// ============================================================================

#[derive(Debug, Clone)]
pub enum InferType {
    IVar {
        id: i32,
    },
    ICon {
        name: String,
        args: Vec<InferType>,
    },
    IFun {
        params: Vec<InferType>,
        ret: Box<InferType>,
        err: Option<Box<InferType>>,
    },
    ITuple {
        elements: Vec<InferType>,
    },
}

// ============================================================================
// TyVarState - union-find backing store
// ============================================================================

#[derive(Debug, Clone)]
enum TyVarState {
    Unbound {
        level: i32,
        constraint: Option<Constraint>,
    },
    Link {
        ty: InferType,
    },
}

// ============================================================================
// Scheme - polymorphic type with quantified variables
// ============================================================================

#[derive(Debug, Clone)]
pub struct Scheme {
    pub quantified: Vec<i32>,
    pub ty: InferType,
    pub def: Option<DefinitionLocation>,
}

pub fn mono(ty: InferType) -> Scheme {
    Scheme {
        quantified: vec![],
        ty,
        def: None,
    }
}

// ============================================================================
// InferEngine - the HM inference engine
// ============================================================================

#[derive(Debug)]
pub struct InferEngine {
    vars: Vec<TyVarState>,
    next_var_id: i32,
    current_level: i32,
    var_names: HashMap<i32, String>,
    pub diagnostics: Vec<Diagnostic>,
}

pub fn new_engine() -> InferEngine {
    InferEngine {
        vars: Vec::new(),
        next_var_id: 0,
        current_level: 0,
        var_names: HashMap::new(),
        diagnostics: Vec::new(),
    }
}

impl InferEngine {
    // --- Constructors for common types ---

    pub fn icon_int(&self) -> InferType {
        InferType::ICon {
            name: "Int".to_string(),
            args: vec![],
        }
    }

    pub fn icon_float(&self) -> InferType {
        InferType::ICon {
            name: "Float".to_string(),
            args: vec![],
        }
    }

    pub fn icon_string(&self) -> InferType {
        InferType::ICon {
            name: "String".to_string(),
            args: vec![],
        }
    }

    pub fn icon_bool(&self) -> InferType {
        InferType::ICon {
            name: "Bool".to_string(),
            args: vec![],
        }
    }

    pub fn icon_none(&self) -> InferType {
        InferType::ICon {
            name: "None".to_string(),
            args: vec![],
        }
    }

    pub fn icon_array(&self, elem: InferType) -> InferType {
        InferType::ICon {
            name: "Array".to_string(),
            args: vec![elem],
        }
    }

    pub fn icon_option(&self, inner: InferType) -> InferType {
        InferType::ICon {
            name: "Option".to_string(),
            args: vec![inner],
        }
    }

    pub fn icon_result(&self, success: InferType, err: InferType) -> InferType {
        InferType::ICon {
            name: "Result".to_string(),
            args: vec![success, err],
        }
    }

    pub fn icon(&self, name: impl Into<String>, args: Vec<InferType>) -> InferType {
        InferType::ICon {
            name: name.into(),
            args,
        }
    }

    // --- Annotation name tracking ---

    pub fn name_var(&mut self, ty: &InferType, name: impl Into<String>) {
        if let InferType::IVar { id } = ty {
            self.var_names.insert(*id, name.into());
        }
    }

    // --- Fresh variables ---

    pub fn fresh_var(&mut self) -> InferType {
        let id = self.next_var_id;
        self.vars.push(TyVarState::Unbound {
            level: self.current_level,
            constraint: None,
        });
        self.next_var_id += 1;
        InferType::IVar { id }
    }

    pub fn fresh_constrained_var(&mut self, c: Constraint) -> InferType {
        let id = self.next_var_id;
        self.vars.push(TyVarState::Unbound {
            level: self.current_level,
            constraint: Some(c),
        });
        self.next_var_id += 1;
        InferType::IVar { id }
    }

    // --- Union-find: find with path compression ---

    pub fn find(&mut self, ty: &InferType) -> InferType {
        if let InferType::IVar { id } = ty {
            let id = *id;
            let state = self.vars[id as usize].clone();
            if let TyVarState::Link { ty: linked } = state {
                let resolved = self.find(&linked);
                self.vars[id as usize] = TyVarState::Link {
                    ty: resolved.clone(),
                };
                return resolved;
            }
            return ty.clone();
        }
        ty.clone()
    }

    // --- Level management ---

    pub fn enter_level(&mut self) {
        self.current_level += 1;
    }

    pub fn leave_level(&mut self) {
        self.current_level -= 1;
    }

    // --- Occurs check + level adjustment ---
    //
    // BUGFIX: The V implementation only performed an occurs check here. For
    // sound level-based generalization (Rémy/Didier style), when binding a
    // var at level L to a type T we must also lower the level of every unbound
    // var inside T to min(its_level, L). Otherwise an inner var that becomes
    // observable from an outer scope can be wrongly quantified at the inner
    // level. The V code happened not to trip this on the example suite because
    // of unify's argument-order bias, but it is unsound in general.

    fn occurs_and_adjust(&mut self, var_id: i32, var_level: i32, ty: &InferType) -> bool {
        let resolved = self.find(ty);
        match &resolved {
            InferType::IVar { id } => {
                if *id == var_id {
                    return true;
                }
                if let TyVarState::Unbound { level, constraint } = &self.vars[*id as usize]
                    && *level > var_level
                {
                    let constraint = *constraint;
                    self.vars[*id as usize] = TyVarState::Unbound {
                        level: var_level,
                        constraint,
                    };
                }
                false
            }
            InferType::ICon { args, .. } => {
                for arg in args {
                    if self.occurs_and_adjust(var_id, var_level, arg) {
                        return true;
                    }
                }
                false
            }
            InferType::IFun { params, ret, err } => {
                for param in params {
                    if self.occurs_and_adjust(var_id, var_level, param) {
                        return true;
                    }
                }
                if self.occurs_and_adjust(var_id, var_level, ret) {
                    return true;
                }
                if let Some(e) = err
                    && self.occurs_and_adjust(var_id, var_level, e)
                {
                    return true;
                }
                false
            }
            InferType::ITuple { elements } => {
                for elem in elements {
                    if self.occurs_and_adjust(var_id, var_level, elem) {
                        return true;
                    }
                }
                false
            }
        }
    }

    // --- Unification ---

    pub fn unify(&mut self, a: &InferType, b: &InferType, s: Span) -> bool {
        let fa = self.find(a);
        let fb = self.find(b);

        if let (InferType::IVar { id: ia }, InferType::IVar { id: ib }) = (&fa, &fb)
            && ia == ib
        {
            return true;
        }

        if let InferType::IVar { id } = &fa {
            return self.unify_var(*id, &fb, s);
        }
        if let InferType::IVar { id } = &fb {
            return self.unify_var(*id, &fa, s);
        }

        if let (InferType::ICon { name: na, args: aa }, InferType::ICon { name: nb, args: ab }) =
            (&fa, &fb)
        {
            if na != nb {
                self.type_error(&fa, &fb, s);
                return false;
            }
            if aa.len() != ab.len() {
                self.type_error(&fa, &fb, s);
                return false;
            }
            for (arg_a, arg_b) in aa.iter().zip(ab.iter()) {
                if !self.unify(arg_a, arg_b, s) {
                    return false;
                }
            }
            return true;
        }

        if let (
            InferType::IFun {
                params: pa,
                ret: ra,
                err: ea,
            },
            InferType::IFun {
                params: pb,
                ret: rb,
                err: eb,
            },
        ) = (&fa, &fb)
        {
            if pa.len() != pb.len() {
                self.type_error(&fa, &fb, s);
                return false;
            }
            for (param_a, param_b) in pa.iter().zip(pb.iter()) {
                if !self.unify(param_a, param_b, s) {
                    return false;
                }
            }
            if !self.unify(ra, rb, s) {
                return false;
            }
            match (ea, eb) {
                (Some(ea), Some(eb)) => return self.unify(ea, eb, s),
                (Some(_), None) => {
                    self.type_error(&fa, &fb, s);
                    return false;
                }
                (None, Some(_)) => {
                    self.type_error(&fa, &fb, s);
                    return false;
                }
                (None, None) => return true,
            }
        }

        if let (InferType::ITuple { elements: ea }, InferType::ITuple { elements: eb }) = (&fa, &fb)
        {
            if ea.len() != eb.len() {
                self.type_error(&fa, &fb, s);
                return false;
            }
            for (elem_a, elem_b) in ea.iter().zip(eb.iter()) {
                if !self.unify(elem_a, elem_b, s) {
                    return false;
                }
            }
            return true;
        }

        self.type_error(&fa, &fb, s);
        false
    }

    fn unify_var(&mut self, var_id: i32, ty: &InferType, s: Span) -> bool {
        let state = self.vars[var_id as usize].clone();
        let TyVarState::Unbound { level, constraint } = state else {
            return false;
        };

        if self.occurs_and_adjust(var_id, level, ty) {
            self.error_at_span("Infinite type detected".to_string(), s);
            return false;
        }

        if let Some(c) = constraint {
            return self.unify_constrained(var_id, c, level, ty, s);
        }
        self.vars[var_id as usize] = TyVarState::Link { ty: ty.clone() };
        true
    }

    fn unify_constrained(
        &mut self,
        var_id: i32,
        constraint: Constraint,
        level: i32,
        ty: &InferType,
        s: Span,
    ) -> bool {
        let resolved = self.find(ty);
        match &resolved {
            InferType::IVar { id } => {
                let other_id = *id;
                let other_state = self.vars[other_id as usize].clone();
                if let TyVarState::Unbound {
                    level: other_level,
                    constraint: other_constraint,
                } = other_state
                {
                    if let Some(other_c) = other_constraint {
                        let allowed_a = constraint.allowed_types();
                        let allowed_b = other_c.allowed_types();
                        let mut intersection: Vec<&str> = Vec::new();
                        for t in &allowed_a {
                            if allowed_b.contains(t) {
                                intersection.push(t);
                            }
                        }
                        if intersection.is_empty() {
                            self.error_at_span(
                                format!(
                                    "No type satisfies both '{}' and '{}'",
                                    constraint.name(),
                                    other_c.name()
                                ),
                                s,
                            );
                            return false;
                        }
                        let winner = if allowed_a.len() <= allowed_b.len() {
                            constraint
                        } else {
                            other_c
                        };
                        self.vars[other_id as usize] = TyVarState::Unbound {
                            level: if other_level < level {
                                other_level
                            } else {
                                level
                            },
                            constraint: Some(winner),
                        };
                        self.vars[var_id as usize] = TyVarState::Link {
                            ty: resolved.clone(),
                        };
                    } else {
                        self.vars[other_id as usize] = TyVarState::Unbound {
                            level: other_level,
                            constraint: Some(constraint),
                        };
                        self.vars[var_id as usize] = TyVarState::Link {
                            ty: resolved.clone(),
                        };
                    }
                    return true;
                }
                self.vars[var_id as usize] = TyVarState::Link {
                    ty: resolved.clone(),
                };
                true
            }
            InferType::ICon { name, .. } => {
                let allowed = constraint.allowed_types();
                if allowed.contains(&name.as_str()) {
                    self.vars[var_id as usize] = TyVarState::Link {
                        ty: resolved.clone(),
                    };
                    return true;
                }
                self.error_at_span(
                    format!(
                        "Type '{}' is not {} (expected one of: {})",
                        name,
                        constraint.name(),
                        allowed.join(", ")
                    ),
                    s,
                );
                false
            }
            _ => {
                self.error_at_span(format!("Type is not {}", constraint.name()), s);
                false
            }
        }
    }

    // --- Generalization ---
    //
    // Assigns stable display names (A, B, C, ...) to quantified type variables.
    // Names from source annotations (set via name_var) are preserved.

    pub fn generalize(&mut self, ty: &InferType) -> Scheme {
        let mut quantified: Vec<i32> = Vec::new();
        self.collect_generalizable(ty, &mut quantified);

        let mut taken: HashSet<String> = HashSet::new();
        for qvar in &quantified {
            if let Some(name) = self.var_names.get(qvar) {
                taken.insert(name.clone());
            }
        }

        let mut next_letter: u8 = b'A';
        for qvar in &quantified {
            if self.var_names.contains_key(qvar) {
                continue;
            }
            while next_letter <= b'Z' {
                let candidate = (next_letter as char).to_string();
                next_letter += 1;
                if !taken.contains(&candidate) {
                    taken.insert(candidate.clone());
                    self.var_names.insert(*qvar, candidate);
                    break;
                }
            }
        }

        Scheme {
            quantified,
            ty: ty.clone(),
            def: None,
        }
    }

    fn collect_generalizable(&mut self, ty: &InferType, quantified: &mut Vec<i32>) {
        let resolved = self.find(ty);
        match &resolved {
            InferType::IVar { id } => {
                let state = self.vars[*id as usize].clone();
                if let TyVarState::Unbound { level, .. } = state
                    && level > self.current_level
                    && !quantified.contains(id)
                {
                    quantified.push(*id);
                }
            }
            InferType::ICon { args, .. } => {
                for arg in args {
                    self.collect_generalizable(arg, quantified);
                }
            }
            InferType::IFun { params, ret, err } => {
                for param in params {
                    self.collect_generalizable(param, quantified);
                }
                self.collect_generalizable(ret, quantified);
                if let Some(e) = err {
                    self.collect_generalizable(e, quantified);
                }
            }
            InferType::ITuple { elements } => {
                for elem in elements {
                    self.collect_generalizable(elem, quantified);
                }
            }
        }
    }

    // --- Instantiation ---

    pub fn instantiate(&mut self, scheme: &Scheme) -> InferType {
        if scheme.quantified.is_empty() {
            return scheme.ty.clone();
        }

        let mut mapping: HashMap<i32, InferType> = HashMap::new();
        for qvar in &scheme.quantified {
            let state = self.vars[*qvar as usize].clone();
            let new_var = if let TyVarState::Unbound {
                constraint: Some(c),
                ..
            } = state
            {
                self.fresh_constrained_var(c)
            } else {
                self.fresh_var()
            };
            if let Some(name) = self.var_names.get(qvar).cloned() {
                self.name_var(&new_var, name);
            }
            mapping.insert(*qvar, new_var);
        }

        self.substitute_vars(&scheme.ty, &mapping)
    }

    fn substitute_vars(&mut self, ty: &InferType, mapping: &HashMap<i32, InferType>) -> InferType {
        let resolved = self.find(ty);
        match &resolved {
            InferType::IVar { id } => {
                if let Some(replacement) = mapping.get(id) {
                    return replacement.clone();
                }
                resolved
            }
            InferType::ICon { name, args } => {
                if args.is_empty() {
                    return resolved.clone();
                }
                let new_args: Vec<InferType> = args
                    .iter()
                    .map(|a| self.substitute_vars(a, mapping))
                    .collect();
                InferType::ICon {
                    name: name.clone(),
                    args: new_args,
                }
            }
            InferType::IFun { params, ret, err } => {
                let new_params: Vec<InferType> = params
                    .iter()
                    .map(|p| self.substitute_vars(p, mapping))
                    .collect();
                let new_ret = self.substitute_vars(ret, mapping);
                let new_err = err
                    .as_ref()
                    .map(|e| Box::new(self.substitute_vars(e, mapping)));
                InferType::IFun {
                    params: new_params,
                    ret: Box::new(new_ret),
                    err: new_err,
                }
            }
            InferType::ITuple { elements } => {
                let new_elements: Vec<InferType> = elements
                    .iter()
                    .map(|e| self.substitute_vars(e, mapping))
                    .collect();
                InferType::ITuple {
                    elements: new_elements,
                }
            }
        }
    }

    // --- Resolution: InferType -> type_def::Type ---
    //
    // Converts an InferType to the concrete Type used elsewhere in the compiler.
    // Unbound type variables resolve to TypeVar using display names assigned
    // during generalization (stored in var_names).

    pub fn resolve(&mut self, ty: &InferType, env: Option<&TypeEnv>) -> Type {
        let resolved = self.find(ty);
        match &resolved {
            InferType::IVar { id } => {
                let state = self.vars[*id as usize].clone();
                if let TyVarState::Unbound { constraint, .. } = state {
                    if let Some(c) = constraint {
                        return t_var(c.name());
                    }
                    if let Some(name) = self.var_names.get(id) {
                        return t_var(name.clone());
                    }
                    return t_var(format!("'t{}", id));
                }
                if let Some(name) = self.var_names.get(id) {
                    return t_var(name.clone());
                }
                t_var(format!("'t{}", id))
            }
            InferType::ICon { name, args } => self.resolve_icon(name, args, env),
            InferType::IFun { params, ret, err } => {
                let params: Vec<Type> = params.iter().map(|p| self.resolve(p, env)).collect();
                let ret_t = self.resolve(ret, env);
                let err_type = err.as_ref().map(|e| Box::new(self.resolve(e, env)));
                Type::Function {
                    params,
                    ret: Box::new(ret_t),
                    error_type: err_type,
                }
            }
            InferType::ITuple { elements } => {
                let elements: Vec<Type> = elements.iter().map(|e| self.resolve(e, env)).collect();
                t_tuple(elements)
            }
        }
    }

    fn resolve_icon(&mut self, name: &str, args: &[InferType], env: Option<&TypeEnv>) -> Type {
        if let Some(env) = env
            && let Some(info) = env.lookup_type_info(name)
        {
            let resolved_args: Vec<Type> =
                args.iter().map(|a| self.resolve(a, Some(env))).collect();
            match info {
                TypeInfo::Struct(si) => {
                    return Type::Struct {
                        id: si.id,
                        name: name.to_string(),
                        type_params: si.type_params,
                        type_args: resolved_args,
                        fields: si.fields,
                    };
                }
                TypeInfo::Enum(ei) => {
                    return Type::Enum {
                        id: ei.id,
                        name: name.to_string(),
                        type_params: ei.type_params,
                        type_args: resolved_args,
                        variants: ei.variants,
                    };
                }
            }
        }

        match name {
            "Int" => t_int(),
            "Float" => t_float(),
            "String" => t_string(),
            "Bool" => t_bool(),
            "None" => t_none(),
            "Array" => {
                if !args.is_empty() {
                    t_array(self.resolve(&args[0], env))
                } else {
                    t_array(t_var("T"))
                }
            }
            "Option" => {
                if !args.is_empty() {
                    t_option(self.resolve(&args[0], env))
                } else {
                    t_option(t_var("T"))
                }
            }
            "Result" => {
                if args.len() >= 2 {
                    Type::Result {
                        success: Box::new(self.resolve(&args[0], env)),
                        error: Box::new(self.resolve(&args[1], env)),
                    }
                } else {
                    Type::Result {
                        success: Box::new(t_var("T")),
                        error: Box::new(t_var("E")),
                    }
                }
            }
            _ => Type::Var {
                name: name.to_string(),
            },
        }
    }

    // --- Error helpers ---

    pub fn error_at_span(&mut self, message: String, s: Span) {
        self.diagnostics.push(Diagnostic {
            span: s,
            severity: Severity::Error,
            message,
        });
    }

    fn type_error(&mut self, a: &InferType, b: &InferType, s: Span) {
        let a_str = self.type_to_str(a);
        let b_str = self.type_to_str(b);
        self.error_at_span(
            format!("Type mismatch: expected '{}', got '{}'", a_str, b_str),
            s,
        );
    }

    pub fn type_to_str(&mut self, ty: &InferType) -> String {
        let resolved = self.find(ty);
        match &resolved {
            InferType::IVar { id } => {
                let state = self.vars[*id as usize].clone();
                if let TyVarState::Unbound { constraint, .. } = state {
                    if let Some(c) = constraint {
                        return c.name().to_string();
                    }
                    if let Some(name) = self.var_names.get(id) {
                        return name.clone();
                    }
                    return format!("'t{}", id);
                }
                if let Some(name) = self.var_names.get(id) {
                    return name.clone();
                }
                format!("'t{}", id)
            }
            InferType::ICon { name, args } => {
                if args.is_empty() {
                    return name.clone();
                }
                let args: Vec<String> = args.iter().map(|a| self.type_to_str(a)).collect();
                format!("{}({})", name, args.join(", "))
            }
            InferType::IFun { params, ret, err } => {
                let params: Vec<String> = params.iter().map(|p| self.type_to_str(p)).collect();
                let mut result = format!("fn({}) {}", params.join(", "), self.type_to_str(ret));
                if let Some(e) = err {
                    result.push_str(&format!("!{}", self.type_to_str(e)));
                }
                result
            }
            InferType::ITuple { elements } => {
                let elements: Vec<String> = elements.iter().map(|e| self.type_to_str(e)).collect();
                format!("({})", elements.join(", "))
            }
        }
    }

    pub fn type_to_infer_with_vars(
        &mut self,
        t: &Type,
        var_mapping: &HashMap<String, InferType>,
    ) -> InferType {
        match t {
            Type::Var { name } => {
                if let Some(ivar) = var_mapping.get(name) {
                    return ivar.clone();
                }
                self.fresh_var()
            }
            Type::Primitive { kind } => match kind {
                PrimitiveKind::Int => self.icon_int(),
                PrimitiveKind::Float => self.icon_float(),
                PrimitiveKind::String => self.icon_string(),
                PrimitiveKind::Bool => self.icon_bool(),
            },
            Type::Array { element } => {
                let e = self.type_to_infer_with_vars(element, var_mapping);
                self.icon_array(e)
            }
            Type::Option { inner } => {
                let i = self.type_to_infer_with_vars(inner, var_mapping);
                self.icon_option(i)
            }
            Type::Function {
                params,
                ret,
                error_type,
            } => {
                let params: Vec<InferType> = params
                    .iter()
                    .map(|p| self.type_to_infer_with_vars(p, var_mapping))
                    .collect();
                let err = error_type
                    .as_ref()
                    .map(|e| Box::new(self.type_to_infer_with_vars(e, var_mapping)));
                InferType::IFun {
                    params,
                    ret: Box::new(self.type_to_infer_with_vars(ret, var_mapping)),
                    err,
                }
            }
            Type::Result { success, error } => {
                let s = self.type_to_infer_with_vars(success, var_mapping);
                let e = self.type_to_infer_with_vars(error, var_mapping);
                self.icon_result(s, e)
            }
            Type::Tuple { elements } => {
                let elements: Vec<InferType> = elements
                    .iter()
                    .map(|e| self.type_to_infer_with_vars(e, var_mapping))
                    .collect();
                InferType::ITuple { elements }
            }
            Type::Struct {
                name,
                type_params,
                type_args,
                ..
            } => {
                let mut args: Vec<InferType> = type_args
                    .iter()
                    .map(|a| self.type_to_infer_with_vars(a, var_mapping))
                    .collect();
                if args.is_empty() {
                    for param in type_params {
                        if let Some(ivar) = var_mapping.get(param) {
                            args.push(ivar.clone());
                        }
                    }
                }
                InferType::ICon {
                    name: name.clone(),
                    args,
                }
            }
            Type::Enum {
                name,
                type_params,
                type_args,
                ..
            } => {
                let mut args: Vec<InferType> = type_args
                    .iter()
                    .map(|a| self.type_to_infer_with_vars(a, var_mapping))
                    .collect();
                if args.is_empty() {
                    for param in type_params {
                        if let Some(ivar) = var_mapping.get(param) {
                            args.push(ivar.clone());
                        }
                    }
                }
                InferType::ICon {
                    name: name.clone(),
                    args,
                }
            }
            Type::None => self.icon_none(),
        }
    }
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::span::point_span;

    fn dummy_span() -> Span {
        point_span(1, 1)
    }

    #[test]
    fn identity_generalizes_to_forall_a() {
        let mut e = new_engine();
        e.enter_level();
        let x = e.fresh_var();
        let id_ty = InferType::IFun {
            params: vec![x.clone()],
            ret: Box::new(x.clone()),
            err: None,
        };
        e.leave_level();
        let scheme = e.generalize(&id_ty);
        assert_eq!(scheme.quantified.len(), 1);
        assert_eq!(e.type_to_str(&scheme.ty), "fn(A) A");
    }

    #[test]
    fn let_polymorphism() {
        // let id = \x. x in (id 1, id "a") — id should work at both Int and String.
        let mut e = new_engine();
        e.enter_level();
        let x = e.fresh_var();
        let id_ty = InferType::IFun {
            params: vec![x.clone()],
            ret: Box::new(x.clone()),
            err: None,
        };
        e.leave_level();
        let scheme = e.generalize(&id_ty);

        let inst1 = e.instantiate(&scheme);
        let int_t = e.icon_int();
        if let InferType::IFun { params, ret, .. } = &inst1 {
            assert!(e.unify(&params[0], &int_t, dummy_span()));
            assert_eq!(e.type_to_str(ret), "Int");
        } else {
            panic!("expected IFun");
        }

        let inst2 = e.instantiate(&scheme);
        let str_t = e.icon_string();
        if let InferType::IFun { params, ret, .. } = &inst2 {
            assert!(e.unify(&params[0], &str_t, dummy_span()));
            assert_eq!(e.type_to_str(ret), "String");
        } else {
            panic!("expected IFun");
        }

        assert!(e.diagnostics.is_empty());
    }

    #[test]
    fn occurs_check_rejects_infinite_type() {
        // \x. x x  —  x : a, requires a = a -> b, infinite.
        let mut e = new_engine();
        let a = e.fresh_var();
        let b = e.fresh_var();
        let fn_ty = InferType::IFun {
            params: vec![a.clone()],
            ret: Box::new(b.clone()),
            err: None,
        };
        let ok = e.unify(&a, &fn_ty, dummy_span());
        assert!(!ok);
        assert_eq!(e.diagnostics.len(), 1);
        assert!(e.diagnostics[0].message.contains("Infinite type"));
    }

    #[test]
    fn numeric_constraint_rejects_string() {
        let mut e = new_engine();
        let n = e.fresh_constrained_var(Constraint::Numeric);
        let s = e.icon_string();
        let ok = e.unify(&n, &s, dummy_span());
        assert!(!ok);
        assert_eq!(e.diagnostics.len(), 1);
        assert!(e.diagnostics[0].message.contains("not numeric"));
    }

    #[test]
    fn numeric_constraint_accepts_int() {
        let mut e = new_engine();
        let n = e.fresh_constrained_var(Constraint::Numeric);
        let i = e.icon_int();
        assert!(e.unify(&n, &i, dummy_span()));
        assert!(e.diagnostics.is_empty());
        assert_eq!(e.type_to_str(&n), "Int");
    }

    #[test]
    fn addable_intersect_numeric_yields_numeric() {
        let mut e = new_engine();
        let a = e.fresh_constrained_var(Constraint::Addable);
        let n = e.fresh_constrained_var(Constraint::Numeric);
        assert!(e.unify(&a, &n, dummy_span()));
        // After intersection, the surviving constraint should be the narrower one.
        assert_eq!(e.type_to_str(&a), "numeric");
    }

    #[test]
    fn unify_mismatched_cons_produces_error() {
        let mut e = new_engine();
        let i = e.icon_int();
        let s = e.icon_string();
        assert!(!e.unify(&i, &s, dummy_span()));
        assert_eq!(e.diagnostics.len(), 1);
    }

    #[test]
    fn instantiate_gives_fresh_vars() {
        let mut e = new_engine();
        e.enter_level();
        let x = e.fresh_var();
        e.leave_level();
        let scheme = e.generalize(&x);
        let i1 = e.instantiate(&scheme);
        let i2 = e.instantiate(&scheme);
        let int_t = e.icon_int();
        let str_t = e.icon_string();
        assert!(e.unify(&i1, &int_t, dummy_span()));
        assert!(e.unify(&i2, &str_t, dummy_span()));
        assert!(e.diagnostics.is_empty());
    }

    #[test]
    fn resolve_primitives() {
        let mut e = new_engine();
        let i = e.icon_int();
        assert_eq!(e.resolve(&i, None), t_int());
        let arr = e.icon_array(e.icon_string());
        assert_eq!(e.resolve(&arr, None), t_array(t_string()));
    }

    #[test]
    fn unify_adjusts_levels_to_prevent_unsound_generalization() {
        // Regression test for the level-adjustment bugfix in unify_var.
        //
        //   level 1: outer = fresh()           -- outer at level 1
        //     level 2: inner = fresh()         -- inner at level 2
        //              unify(inner, outer)     -- must lower outer? No: must lower
        //                                         whichever ends up free to level 1.
        //   leave to level 1
        //   generalize(inner)                  -- must NOT quantify, since the var
        //                                         is observable at level 1.
        let mut e = new_engine();
        e.enter_level(); // level 1
        let outer = e.fresh_var();
        e.enter_level(); // level 2
        let inner = e.fresh_var();
        assert!(e.unify(&inner, &outer, dummy_span()));
        e.leave_level(); // back to level 1
        let scheme = e.generalize(&inner);
        assert_eq!(
            scheme.quantified.len(),
            0,
            "var tied to outer scope must not be generalized"
        );
    }

    #[test]
    fn unify_adjusts_levels_through_constructors() {
        // Same as above but the inner var is wrapped in a type constructor,
        // exercising the recursive walk in occurs_and_adjust.
        let mut e = new_engine();
        e.enter_level();
        let outer = e.fresh_var();
        e.enter_level();
        let inner = e.fresh_var();
        let arr_inner = e.icon_array(inner.clone());
        let arr_outer = e.icon_array(outer.clone());
        assert!(e.unify(&arr_inner, &arr_outer, dummy_span()));
        e.leave_level();
        let scheme = e.generalize(&inner);
        assert_eq!(scheme.quantified.len(), 0);
    }
}
