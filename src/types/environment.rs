use indexmap::IndexMap;

use super::infer::Scheme;
use crate::type_def::Type;

#[derive(Debug, Clone, Copy)]
pub struct DefinitionLocation {
    pub line: i32,
    pub column: i32,
    pub end_col: i32,
}

#[derive(Debug, Clone)]
pub struct StructInfo {
    pub id: i32,
    pub type_params: Vec<String>,
    pub fields: IndexMap<String, Type>,
}

#[derive(Debug, Clone)]
pub struct EnumInfo {
    pub id: i32,
    pub type_params: Vec<String>,
    pub variants: IndexMap<String, Vec<Type>>,
}

#[derive(Debug, Clone)]
pub enum TypeInfo {
    Struct(StructInfo),
    Enum(EnumInfo),
}

#[derive(Debug, Clone)]
pub struct TypeEnv {
    pub scopes: Vec<IndexMap<String, Scheme>>,
    pub type_info: IndexMap<String, TypeInfo>,
    pub definitions: IndexMap<String, DefinitionLocation>,
    pub docs: IndexMap<String, String>,
    pub next_type_id: i32,
}

pub fn new_env() -> TypeEnv {
    TypeEnv {
        scopes: vec![IndexMap::new()],
        type_info: IndexMap::new(),
        definitions: IndexMap::new(),
        docs: IndexMap::new(),
        next_type_id: 1,
    }
}

impl TypeEnv {
    pub fn push_scope(&mut self) {
        self.scopes.push(IndexMap::new());
    }

    pub fn pop_scope(&mut self) {
        if self.scopes.len() > 1 {
            self.scopes.pop();
        }
    }

    pub fn scope_depth(&self) -> usize {
        self.scopes.len()
    }

    pub fn define(&mut self, name: &str, scheme: Scheme) {
        if !self.scopes.is_empty() {
            let last = self.scopes.len() - 1;
            self.scopes[last].insert(name.to_string(), scheme);
        }
    }

    pub fn define_at(&mut self, name: &str, scheme: Scheme, loc: DefinitionLocation) {
        self.define(
            name,
            Scheme {
                quantified: scheme.quantified,
                ty: scheme.ty,
                def: Some(loc),
            },
        );
    }

    pub fn lookup(&self, name: &str) -> Option<Scheme> {
        for scope in self.scopes.iter().rev() {
            if let Some(scheme) = scope.get(name) {
                return Some(scheme.clone());
            }
        }
        None
    }

    pub fn store_doc(&mut self, name: &str, doc: String) {
        self.docs.insert(name.to_string(), doc);
    }

    pub fn lookup_doc(&self, name: &str) -> Option<String> {
        self.docs.get(name).cloned()
    }

    pub fn lookup_definition(&self, name: &str) -> Option<DefinitionLocation> {
        for scope in self.scopes.iter().rev() {
            if let Some(scheme) = scope.get(name)
                && let Some(def) = scheme.def
            {
                return Some(def);
            }
        }
        self.definitions.get(name).copied()
    }

    pub fn register_struct(
        &mut self,
        name: &str,
        type_params: Vec<String>,
        fields: IndexMap<String, Type>,
    ) -> StructInfo {
        let info = StructInfo {
            id: self.next_type_id,
            type_params,
            fields,
        };
        self.next_type_id += 1;
        self.type_info
            .insert(name.to_string(), TypeInfo::Struct(info.clone()));
        info
    }

    pub fn register_enum(
        &mut self,
        name: &str,
        type_params: Vec<String>,
        variants: IndexMap<String, Vec<Type>>,
    ) -> EnumInfo {
        let info = EnumInfo {
            id: self.next_type_id,
            type_params,
            variants,
        };
        self.next_type_id += 1;
        self.type_info
            .insert(name.to_string(), TypeInfo::Enum(info.clone()));
        info
    }

    pub fn lookup_type_info(&self, name: &str) -> Option<TypeInfo> {
        self.type_info.get(name).cloned()
    }

    pub fn find_variant_owner(&self, variant_name: &str) -> Option<(String, EnumInfo)> {
        let mut found_name = String::new();
        let mut found_info: Option<EnumInfo> = None;
        let mut count = 0;
        for (enum_name, info) in &self.type_info {
            if let TypeInfo::Enum(ei) = info
                && ei.variants.contains_key(variant_name)
            {
                found_name = enum_name.clone();
                found_info = Some(ei.clone());
                count += 1;
            }
        }
        if count == 1
            && let Some(fi) = found_info
        {
            return Some((found_name, fi));
        }
        None
    }

    fn all_names(&self) -> Vec<String> {
        let mut names: Vec<String> = Vec::new();
        for scope in &self.scopes {
            for name in scope.keys() {
                if !names.contains(name) {
                    names.push(name.clone());
                }
            }
        }
        names
    }

    pub fn suggest_name(&self, name: &str) -> Option<String> {
        let all = self.all_names();
        let mut best = String::new();
        let mut best_dist = 4usize;

        for candidate in &all {
            let dist = levenshtein(name, candidate);
            if dist < best_dist {
                best_dist = dist;
                best = candidate.clone();
            }
        }

        if !best.is_empty() { Some(best) } else { None }
    }
}

fn levenshtein(a: &str, b: &str) -> usize {
    let a = a.as_bytes();
    let b = b.as_bytes();
    if a.is_empty() {
        return b.len();
    }
    if b.is_empty() {
        return a.len();
    }

    let mut prev: Vec<usize> = vec![0; b.len() + 1];
    let mut curr: Vec<usize> = vec![0; b.len() + 1];

    for (j, p) in prev.iter_mut().enumerate() {
        *p = j;
    }

    for i in 1..=a.len() {
        curr[0] = i;
        for j in 1..=b.len() {
            let cost = if a[i - 1] == b[j - 1] { 0 } else { 1 };
            let del = prev[j] + 1;
            let ins = curr[j - 1] + 1;
            let sub = prev[j - 1] + cost;
            let mut min = del;
            if ins < min {
                min = ins;
            }
            if sub < min {
                min = sub;
            }
            curr[j] = min;
        }
        std::mem::swap(&mut prev, &mut curr);
    }

    prev[b.len()]
}
