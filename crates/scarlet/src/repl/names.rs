//! What the REPL knows how to complete: the language's fixed vocabulary plus
//! whatever the running session has defined or imported.
//!
//! Harvested from the AST of each accepted entry rather than from the
//! reference graph, so a name becomes completable exactly when the entry that
//! bound it was accepted into the session's replay source.

use std::collections::BTreeMap;

use crate::ast;
use crate::bytecode::{Export, IncrementalSession};
use crate::module::{self, ModuleKey, ModulePath};
use crate::token::Keyword;

/// A completion candidate and how to describe it in the completion list.
pub struct Candidate {
    pub name: String,
    /// A function's parameter names, rendered after the name: `add(a, b)`.
    pub params: Vec<String>,
}

impl Candidate {
    fn plain(name: impl Into<String>) -> Self {
        Candidate {
            name: name.into(),
            params: Vec::new(),
        }
    }

    fn from_export(e: Export) -> Self {
        Candidate {
            name: e.name,
            params: e.params,
        }
    }

    /// What the completion list shows: the name, plus a call shape for a
    /// function so `map` and `map(seq, f)` are distinguishable at a glance.
    pub fn display(&self) -> String {
        if self.params.is_empty() {
            self.name.clone()
        } else {
            format!("{}({})", self.name, self.params.join(", "))
        }
    }
}

/// Names the session has introduced. Rebuilt by [`Names::reset`] and extended
/// per accepted entry, so it tracks the replayed prelude exactly.
#[derive(Default)]
pub struct Names {
    defined: Vec<String>,
    /// Import alias to module path, for `alias.` completion. Only stdlib
    /// modules resolve: a relative import's exports live in a file this index
    /// never reads.
    aliases: BTreeMap<String, ModulePath>,
    /// Where stdlib exports are read from. Built on the first completion that
    /// needs one, since it compiles the prelude; it caches every module it
    /// compiles after that, so each module compiles once per REPL.
    stdlib: Option<IncrementalSession>,
}

impl Names {
    pub fn reset(&mut self) {
        self.defined.clear();
        self.aliases.clear();
    }

    /// Record every top-level name an accepted entry bound.
    pub fn observe(&mut self, program: &ast::BlockExpression) {
        for node in &program.body {
            let ast::Node::Statement(stmt) = node else {
                continue;
            };
            match stmt.as_ref() {
                ast::Statement::Declaration { decl, .. } => self.observe_declaration(decl),
                ast::Statement::VariableBinding(b) => self.define(&b.identifier.name),
                ast::Statement::ImportDeclaration(i) => self.observe_import(i),
                _ => {}
            }
        }
    }

    fn observe_declaration(&mut self, decl: &ast::Declaration) {
        match decl {
            ast::Declaration::Const(c) => self.define(&c.identifier.name),
            ast::Declaration::Function(f) => self.define(&f.identifier.name),
            ast::Declaration::Type(t) => {
                self.define(&t.identifier.name);
                if let ast::TypeBody::Variants { ctors, .. } = &t.body {
                    for ctor in ctors {
                        self.define(&ctor.identifier.name);
                    }
                }
            }
        }
    }

    fn observe_import(&mut self, import: &ast::ImportDeclaration) {
        for item in &import.items {
            self.define(&item.alias.as_ref().unwrap_or(&item.name).name);
        }
        let Some(last) = import.path.names.last() else {
            return;
        };
        let alias = match &import.alias {
            Some(a) => a.name.clone(),
            None => last.clone(),
        };
        // Only a canonical (non-relative) path names a stdlib module;
        // anything else completes to nothing.
        if import.path.leading.is_empty() {
            self.aliases.insert(alias, import.path.names.clone());
        } else {
            self.aliases.remove(&alias);
        }
    }

    fn define(&mut self, name: &str) {
        if !self.defined.iter().any(|n| n == name) {
            self.defined.push(name.to_string());
        }
    }

    /// Candidates for a bare word: keywords, the implicitly imported prelude,
    /// module aliases, and the session's own definitions.
    pub fn bare(&mut self, prefix: &str) -> Vec<Candidate> {
        let mut out: Vec<Candidate> = Vec::new();
        out.extend(
            Keyword::ALL
                .iter()
                .map(|k| Candidate::plain(k.text()))
                .filter(|c| c.name.starts_with(prefix)),
        );
        out.extend(
            self.exports(&module::scarlet_prelude())
                .into_iter()
                .filter(|e| e.name.starts_with(prefix))
                .map(Candidate::from_export),
        );
        out.extend(
            self.aliases
                .keys()
                .chain(self.defined.iter())
                .filter(|n| n.starts_with(prefix))
                .map(Candidate::plain),
        );
        out.sort_by(|a, b| a.name.cmp(&b.name));
        out.dedup_by(|a, b| a.name == b.name);
        out
    }

    /// Whether `name`, typed alone, is an imported module rather than a value
    /// the session defined.
    pub fn is_module(&self, name: &str) -> bool {
        self.aliases.contains_key(name) && !self.defined.iter().any(|d| d == name)
    }

    /// Candidates after `qualifier.`: that module's public exports.
    pub fn qualified(&mut self, qualifier: &str, prefix: &str) -> Vec<Candidate> {
        let Some(path) = self.aliases.get(qualifier).cloned() else {
            return Vec::new();
        };
        let mut out: Vec<Candidate> = self
            .exports(&path)
            .into_iter()
            .filter(|e| e.name.starts_with(prefix))
            .map(Candidate::from_export)
            .collect();
        out.sort_by(|a, b| a.name.cmp(&b.name));
        out
    }

    /// Every importable stdlib module path, for completing `import scarlet/…`.
    pub fn module_paths(prefix: &str) -> Vec<Candidate> {
        module::stdlib_modules()
            .iter()
            .map(|path| ModuleKey::of(path).to_string())
            .filter(|k| k.starts_with(prefix))
            .map(Candidate::plain)
            .collect()
    }

    fn exports(&mut self, path: &ModulePath) -> Vec<Export> {
        self.stdlib
            .get_or_insert_with(IncrementalSession::new)
            .exports(path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{parser, scanner};

    fn observed(src: &str) -> Names {
        let mut scanner = scanner::new_scanner(src.to_string());
        let program = parser::new_parser(&mut scanner).parse_program();
        let mut names = Names::default();
        names.observe(&program.ast);
        names
    }

    fn labels(candidates: &[Candidate]) -> Vec<String> {
        candidates.iter().map(|c| c.name.clone()).collect()
    }

    #[test]
    fn the_prelude_is_completable_with_no_session_at_all() {
        let mut names = Names::default();
        assert!(labels(&names.bare("printl")).contains(&"println".to_string()));
        assert!(
            labels(&names.bare("f")).contains(&"fn".to_string()),
            "keyword"
        );
    }

    #[test]
    fn a_definition_becomes_completable() {
        let mut names = observed("fn triple(n Int) Int { n * 3 }\nconst k = 1\n");
        assert_eq!(labels(&names.bare("tri")), vec!["triple"]);
        assert_eq!(labels(&names.bare("k")), vec!["k"]);
    }

    #[test]
    fn a_type_declaration_offers_its_constructors() {
        let mut names = observed("type Shape {\n\tCircle(r Int)\n\tSquare(s Int)\n}\n");
        assert_eq!(labels(&names.bare("Circ")), vec!["Circle"]);
        assert_eq!(labels(&names.bare("Shape")), vec!["Shape"]);
    }

    #[test]
    fn an_import_completes_the_modules_exports_behind_its_alias() {
        let mut names = observed("import scarlet/string\n");
        let items = labels(&names.qualified("string", ""));
        assert!(!items.is_empty(), "no exports for scarlet/string");
        assert!(names.qualified("nope", "").is_empty(), "unknown qualifier");
    }

    /// `http.Post` is a constructor of an exported type, not a value export:
    /// the name list has to reach into the type to find it.
    #[test]
    fn a_constructor_completes_behind_its_modules_alias() {
        let mut names = observed("import scarlet/http\n");
        assert_eq!(labels(&names.qualified("http", "Po")), vec!["Post"]);
        assert_eq!(labels(&names.qualified("http", "Del")), vec!["Delete"]);
        assert!(labels(&names.qualified("http", "Met")).contains(&"Method".to_string()));
    }

    #[test]
    fn an_aliased_import_completes_under_the_alias_only() {
        let mut names = observed("import scarlet/string as str\n");
        assert!(!names.qualified("str", "").is_empty());
        assert!(names.qualified("string", "").is_empty());
        assert!(labels(&names.bare("st")).contains(&"str".to_string()));
    }

    #[test]
    fn a_function_candidate_shows_its_parameters() {
        let println = names_candidate("println");
        assert_eq!(println.display(), "println(x)");
    }

    fn names_candidate(name: &str) -> Candidate {
        let mut names = Names::default();
        names
            .bare(name)
            .into_iter()
            .find(|c| c.name == name)
            .unwrap_or_else(|| panic!("{name} is not a candidate"))
    }
}
