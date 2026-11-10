//! What the REPL knows how to complete: the language's fixed vocabulary plus
//! whatever the running session has defined or imported.
//!
//! We collect names from the AST of each accepted entry, not from the reference graph, so a name
//! becomes completable exactly when the entry that bound it joins the session's replay source.

use std::collections::BTreeMap;

use super::entry::{Parts, split};
use crate::ast;
use crate::build::Build;
use crate::compile::{Export, exports};
use crate::module::{self, ModuleKey, ModulePath};
use crate::sources::DiskSources;
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

    /// What the completion list shows. It's the name, plus a call shape for a function, so you can
    /// tell `map` and `map(seq, f)` apart at a glance.
    pub fn display(&self) -> String {
        if self.params.is_empty() {
            self.name.clone()
        } else {
            format!("{}({})", self.name, self.params.join(", "))
        }
    }
}

/// One entry the session accepted: what it adds to the replayed source, and the names it binds.
pub struct AcceptedEntry {
    /// Its imports. The language needs every import before all other declarations, so a session
    /// replays every entry's imports first.
    imports: String,
    /// Its declarations and bindings, replayed ahead of every later entry.
    definitions: String,
    /// The names it binds at the top level.
    defined: Vec<String>,
    /// The aliases its imports bind, in order: the stdlib module each names, or `None` for a
    /// relative import, whose exports live in a file completion never reads.
    aliases: Vec<(String, Option<ModulePath>)>,
}

impl AcceptedEntry {
    /// The entry `source`, which parsed to `program`.
    pub fn new(source: &str, program: &ast::BlockExpression) -> Self {
        let parts = split(source, program);
        let mut entry = AcceptedEntry {
            imports: parts.imports,
            definitions: parts.definitions,
            defined: Vec::new(),
            aliases: Vec::new(),
        };
        for node in &program.body {
            let ast::Node::Statement(stmt) = node else {
                continue;
            };
            match stmt.as_ref() {
                ast::Statement::Declaration { decl, .. } => entry.declaration(decl),
                ast::Statement::Binding(b) => {
                    b.pattern
                        .for_each_binder(ast::OrAlternatives::All, &mut |binder| {
                            if let ast::PatternBinder::Name(id) = binder {
                                entry.defined.push(id.name.clone());
                            }
                        })
                }
                ast::Statement::ImportDeclaration(i) => entry.import(i),
                ast::Statement::Backpass(_) => {}
            }
        }
        entry
    }

    fn declaration(&mut self, decl: &ast::Declaration) {
        match decl {
            ast::Declaration::Const(c) => self.defined.push(c.identifier.name.clone()),
            ast::Declaration::Function(f) => self.defined.push(f.identifier.name.clone()),
            ast::Declaration::Type(t) => {
                self.defined.push(t.identifier.name.clone());
                if let ast::TypeBody::Variants { ctors, .. } = &t.body {
                    for ctor in ctors {
                        self.defined.push(ctor.identifier.name.clone());
                    }
                }
            }
        }
    }

    fn import(&mut self, import: &ast::ImportDeclaration) {
        let Some(last) = import.path.names.last() else {
            return;
        };
        let alias = match &import.alias {
            Some(a) => a.name.clone(),
            None => last.clone(),
        };
        // Only a canonical (non-relative) path names a stdlib module.
        let module = import
            .path
            .leading
            .is_empty()
            .then(|| import.path.names.clone());
        self.aliases.push((alias, module));
    }
}

/// The entries the session has accepted. The source the session replays and the names it
/// completes both come from this one list, so a reset clears one thing.
#[derive(Default)]
pub struct Accepted {
    entries: Vec<AcceptedEntry>,
    /// Where stdlib exports are read from. We make it on the first completion that needs it,
    /// because that checks the prelude. It keeps every module it checks after that, so each
    /// module is checked once per REPL.
    stdlib: Option<(Build, DiskSources)>,
}

impl Accepted {
    pub fn reset(&mut self) {
        self.entries.clear();
    }

    pub fn accept(&mut self, entry: AcceptedEntry) {
        self.entries.push(entry);
    }

    /// The session's own source: every import, then every definition.
    pub fn replay(&self) -> String {
        self.replay_with(&Parts::default())
    }

    /// The session's source with `next` joined to it: its imports after the session's, its
    /// definitions after the session's, and its expressions last.
    pub fn replay_with(&self, next: &Parts) -> String {
        let mut out = String::new();
        for e in &self.entries {
            out.push_str(&e.imports);
        }
        out.push_str(&next.imports);
        for e in &self.entries {
            out.push_str(&e.definitions);
        }
        out.push_str(&next.definitions);
        out.push_str(&next.expressions);
        out
    }

    /// Every name the session's entries bound, each once.
    fn defined(&self) -> Vec<&str> {
        let mut out: Vec<&str> = Vec::new();
        for n in self.entries.iter().flat_map(|e| &e.defined) {
            if !out.contains(&n.as_str()) {
                out.push(n);
            }
        }
        out
    }

    /// Each import alias in scope, and the stdlib module it names, for `alias::` completion. A
    /// later import of the same alias wins.
    fn aliases(&self) -> BTreeMap<&str, &ModulePath> {
        let mut out = BTreeMap::new();
        for (alias, module) in self.entries.iter().flat_map(|e| &e.aliases) {
            match module {
                Some(m) => out.insert(alias.as_str(), m),
                None => out.remove(alias.as_str()),
            };
        }
        out
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
        let aliases = self.aliases();
        let local: Vec<String> = aliases
            .keys()
            .copied()
            .chain(self.defined())
            .filter(|n| n.starts_with(prefix))
            .map(str::to_string)
            .collect();
        out.extend(local.into_iter().map(Candidate::plain));
        out.sort_by(|a, b| a.name.cmp(&b.name));
        out.dedup_by(|a, b| a.name == b.name);
        out
    }

    /// Whether `name`, typed alone, is an imported module rather than a value
    /// the session defined.
    pub fn is_module(&self, name: &str) -> bool {
        self.aliases().contains_key(name) && !self.defined().contains(&name)
    }

    /// Candidates after `qualifier::`: that module's public exports.
    pub fn qualified(&mut self, qualifier: &str, prefix: &str) -> Vec<Candidate> {
        let Some(path) = self.aliases().get(qualifier).map(|p| (*p).clone()) else {
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
        let (build, sources) = self
            .stdlib
            .get_or_insert_with(|| (Build::default(), DiskSources::new()));
        exports(build, sources, path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{parser, scanner};

    fn observed(src: &str) -> Accepted {
        let mut scanner = scanner::new_scanner(src.to_string());
        let program = parser::new_parser(&mut scanner).parse_program();
        let mut names = Accepted::default();
        names.accept(AcceptedEntry::new(src, &program.ast));
        names
    }

    fn labels(candidates: &[Candidate]) -> Vec<String> {
        candidates.iter().map(|c| c.name.clone()).collect()
    }

    #[test]
    fn the_prelude_is_completable_with_no_session_at_all() {
        let mut names = Accepted::default();
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

    /// `http::Post` is a constructor of an exported type, not a value export, so the name list has
    /// to reach into the type to find it.
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
        let mut names = Accepted::default();
        names
            .bare(name)
            .into_iter()
            .find(|c| c.name == name)
            .unwrap_or_else(|| panic!("{name} is not a candidate"))
    }
}
