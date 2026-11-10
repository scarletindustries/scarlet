//! What every pass of one module check reads and none changes: the input,
//! the prelude, and the module's imports.

use std::collections::HashSet;

use indexmap::IndexMap;
use scarlet_syntax::ast;
use scarlet_syntax::module_path::{self, ModulePath};
use scarlet_syntax::span::Span;
use scarlet_types::interface::{Interface, ValueDef, ValueKind};
use scarlet_types::known::prelude_key;
use scarlet_types::name::{ModuleKey, Name, TypeName};

use crate::CheckInput;
use crate::index::{DefKind, EntityKind, Site, UseKind};
use crate::report::Report;

pub(crate) struct Env<'a> {
    pub input: &'a CheckInput<'a>,
    pub key: ModuleKey,
    path: ModulePath,
    /// The prelude's interface, unless this is the prelude.
    pub prelude: Option<&'a Interface>,
    /// Names user code can't declare again: the prelude's types and
    /// constructors.
    pub reserved: HashSet<Name>,
    /// Types whose parameters a binding never generalises over.
    pub restricted: Vec<TypeName>,
    pub imports: Imports,
}

impl<'a> Env<'a> {
    pub fn new(input: &'a CheckInput<'a>, report: &mut Report) -> Self {
        let key = input.source.key.clone();
        let path: ModulePath = key.as_str().split('/').map(str::to_string).collect();
        let prelude = (key != prelude_key())
            .then(|| input.interfaces.interface(&prelude_key()))
            .flatten();
        let mut reserved = HashSet::new();
        if let Some(p) = prelude {
            reserved.extend(p.types.keys().cloned());
            for (name, v) in &p.values {
                if matches!(v.kind, ValueKind::Constructor { .. }) {
                    reserved.insert(name.clone());
                }
            }
        }
        let process: ModulePath = vec!["scarlet".into(), "process".into()];
        let imports = Imports::bind(input, &key, report);
        Env {
            input,
            key,
            path,
            prelude,
            reserved,
            restricted: vec![TypeName::new(
                ModuleKey::for_stdlib(&process),
                "Subject".into(),
            )],
            imports,
        }
    }

    pub fn in_stdlib(&self) -> bool {
        module_path::is_stdlib(&self.path)
    }

    pub fn is_prelude(&self) -> bool {
        self.key == prelude_key()
    }

    pub fn interface_of(&self, key: &ModuleKey) -> Option<&'a Interface> {
        self.input.interfaces.interface(key)
    }

    pub fn site(&self, span: Span, entity: EntityKind) -> Site {
        Site {
            module: self.key.clone(),
            span,
            entity,
        }
    }

    /// Where another module declares one of its values.
    pub fn foreign_site(key: &ModuleKey, v: &ValueDef) -> Site {
        Site {
            module: key.clone(),
            span: v.span,
            entity: entity_of(&v.kind),
        }
    }

    /// The prelude's value `name`, with the module that declares it.
    pub fn prelude_value(&self, name: &str) -> Option<(&'a ModuleKey, &'a ValueDef)> {
        let p = self.prelude?;
        Some((&p.key, p.values.get(name)?))
    }

    /// How to reach a constructor `name` that isn't in scope: through each
    /// imported module that has one.
    pub fn qualified_spellings(&self, name: &str) -> Vec<String> {
        let mut out: Vec<String> = self
            .imports
            .iter()
            .filter(|(_, imp)| {
                self.interface_of(&imp.key)
                    .and_then(|i| i.values.get(name))
                    .is_some_and(|v| matches!(v.kind, ValueKind::Constructor { .. }))
            })
            .map(|(q, _)| format!("`{q}::{name}`"))
            .collect();
        out.sort();
        out
    }
}

fn entity_of(kind: &ValueKind) -> EntityKind {
    match kind {
        ValueKind::Fn { .. } | ValueKind::Builtin { .. } => EntityKind::Function,
        ValueKind::Const => EntityKind::Constant,
        ValueKind::Constructor { .. } => EntityKind::Constructor,
    }
}

/// One `import` in the module.
pub(crate) struct Import {
    pub key: ModuleKey,
    /// The name it binds, as a definition.
    pub alias: Site,
    /// The path as the import wrote it, for naming the module to a person.
    pub written: String,
}

/// The module's imports, by the qualifier each binds.
pub(crate) struct Imports {
    by_qualifier: IndexMap<Name, Import>,
}

impl Imports {
    fn bind(input: &CheckInput<'_>, key: &ModuleKey, report: &mut Report) -> Self {
        let decls = input.module.body.iter().filter_map(|n| match n {
            ast::Node::Statement(s) => match s.as_ref() {
                ast::Statement::ImportDeclaration(d) => Some(d),
                ast::Statement::Declaration { .. }
                | ast::Statement::Binding(_)
                | ast::Statement::Backpass(_) => None,
            },
            ast::Node::Expression(_) => None,
        });
        let mut by_qualifier: IndexMap<Name, Import> = IndexMap::new();
        for (imp, target) in decls.zip(input.imports) {
            let Some(target) = target else { continue };
            let qualifier: Name = match &imp.alias {
                Some(a) => a.name.as_str().into(),
                None => imp
                    .path
                    .names
                    .last()
                    .map_or_else(|| target.as_str().into(), |n| n.as_str().into()),
            };
            let alias_span = imp.alias.as_ref().map_or(imp.span, |a| a.span);
            let alias = Site {
                module: key.clone(),
                span: alias_span,
                entity: EntityKind::ModuleAlias,
            };
            report.defs.push(crate::index::Def {
                site: alias.clone(),
                name: qualifier.to_string(),
                doc: None,
                public: false,
                kind: DefKind::ModuleAlias {
                    decl: imp.span,
                    imports: target.clone(),
                },
            });
            // The module segment points into the imported module, so
            // go-to-definition on it opens that module.
            report.uses.push(crate::index::Use {
                span: imp.path_span,
                kind: UseKind::Import,
                target: Site {
                    module: target.clone(),
                    span: imp.path_span,
                    entity: EntityKind::ModuleAlias,
                },
                owner: None,
            });
            if let Some(a) = &imp.alias {
                report.uses.push(crate::index::Use {
                    span: a.span,
                    kind: UseKind::Alias,
                    target: alias.clone(),
                    owner: None,
                });
            }
            if let Some(prev) = by_qualifier.get(&qualifier) {
                report.module_error(
                    alias_span,
                    format!(
                        "'{qualifier}' already names the import of `{}`. Give this one another name with `as`",
                        prev.written
                    ),
                );
                continue;
            }
            by_qualifier.insert(
                qualifier,
                Import {
                    key: target.clone(),
                    alias,
                    written: imp.path.to_string(),
                },
            );
        }
        Imports { by_qualifier }
    }

    pub fn get(&self, qualifier: &str) -> Option<&Import> {
        self.by_qualifier.get(qualifier)
    }

    pub fn iter(&self) -> impl Iterator<Item = (&Name, &Import)> {
        self.by_qualifier.iter()
    }

    /// How to name a module in a diagnostic: the path its import wrote, or
    /// its last segment. Never the key, which is an identity, not a name.
    pub fn module_name(&self, key: &ModuleKey) -> String {
        if let Some(imp) = self.by_qualifier.values().find(|i| i.key == *key) {
            return imp.written.clone();
        }
        let k = key.as_str();
        k.rsplit('/').next().unwrap_or(k).to_string()
    }
}
