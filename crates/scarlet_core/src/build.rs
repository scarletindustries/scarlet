//! A build: the modules a program or an editor session reaches, each checked
//! once and kept while some entry still reaches it.
//!
//! Each [`Build::check`] is one [`CheckRun`]. The run loads the entry's
//! imports depth first, reuses every cached check that is still fresh, and
//! checks the rest. It gives back an [`Analysis`]: the modules that one check
//! reached, which is what every consumer reads.

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::rc::Rc;

use scarlet_check::{CheckInput, CheckedModule, Interfaces, Role, UnusedBindings};
use scarlet_syntax::ast;
use scarlet_syntax::diagnostic::{Diagnostic, DiagnosticCode, has_errors};
use scarlet_syntax::module_path::{ModuleKey, ModulePath, ResolveError};
use scarlet_syntax::source::SourceFile;
use scarlet_syntax::span::Span;
use scarlet_types::interface::Interface;
use scarlet_types::known::prelude_key;

use crate::module::{self, ModuleSource};
use crate::sources::{Revision, Sources, Text};

/// Where a module's text came from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Origin {
    /// The stdlib copy built into the compiler.
    Embedded,
    File(PathBuf),
    /// Text handed over directly: the entry of a `scarlet run`, an editor
    /// buffer, a REPL line.
    Given,
}

/// Everything besides its text and imports that decides a module's check:
/// which module it is and what it is checked as. A file open as a program and
/// imported as a library is checked once as each.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct CheckSpec {
    key: ModuleKey,
    role: Role,
    unused: UnusedBindings,
}

impl CheckSpec {
    /// How every import is checked.
    fn library(key: ModuleKey) -> Self {
        CheckSpec {
            key,
            role: Role::Library,
            unused: UnusedBindings::Report,
        }
    }
}

/// One checked module.
#[derive(Debug)]
pub struct Module {
    pub spec: CheckSpec,
    pub origin: Origin,
    revision: Revision,
    /// Each import's module, in source order, or `None` when it didn't
    /// resolve.
    imports: Vec<Option<ModuleKey>>,
    /// The imports as written, so a module whose text hasn't changed can have
    /// its imports looked at again without parsing it.
    written: Vec<(ast::ImportPath, Span)>,
    /// Every interface the check read, as it read it.
    read: Vec<(ModuleKey, Interface)>,
    /// What the parser and the import resolution reported.
    load_diagnostics: Vec<Diagnostic>,
    pub checked: CheckedModule,
}

impl Module {
    pub fn key(&self) -> &ModuleKey {
        &self.spec.key
    }

    /// Everything reported about this module.
    pub fn diagnostics(&self) -> impl Iterator<Item = &Diagnostic> {
        self.load_diagnostics
            .iter()
            .chain(&self.checked.diagnostics)
    }
}

#[derive(Debug, PartialEq, Eq)]
enum Freshness {
    Fresh,
    Stale,
}

/// Whether a cached check still holds for a module now. It does when the
/// module has the same text from the same place, its imports resolve to the
/// same modules, and every interface the check read is the one loaded now.
fn freshness(
    cached: &Module,
    origin: &Origin,
    revision: Revision,
    imports: &[Option<ModuleKey>],
    loaded: &Loaded,
) -> Freshness {
    let same = cached.origin == *origin
        && cached.revision == revision
        && cached.imports == imports
        && cached.read.iter().all(|(k, iface)| {
            loaded
                .by_key
                .get(k)
                .is_some_and(|m| m.checked.interface == *iface)
        });
    if same {
        Freshness::Fresh
    } else {
        Freshness::Stale
    }
}

/// A module to check whose text the caller has.
pub struct Entry<'a> {
    /// The module's text. Its key is the module's identity.
    pub source: Rc<SourceFile>,
    pub ast: &'a ast::BlockExpression,
    pub doc: Option<String>,
    /// What the parser reported about the text.
    pub parse: Vec<Diagnostic>,
    /// The directory `./` imports are relative to.
    pub base_dir: Option<&'a Path>,
    pub role: Role,
    pub unused: UnusedBindings,
}

impl Entry<'_> {
    pub fn spec(&self) -> CheckSpec {
        CheckSpec {
            key: self.source.key.clone(),
            role: self.role,
            unused: self.unused,
        }
    }
}

/// How much work one check did, for tests of what gets done again.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Counts {
    /// Modules checked.
    pub checks: usize,
    /// Imported modules parsed.
    pub parses: usize,
}

/// What one check reached: the entry, and every module it imports, directly
/// or not. Within one analysis a key names one module.
#[derive(Debug, Clone)]
pub struct Analysis {
    /// Each before every module that imports it, the prelude first.
    imports: Vec<Rc<Module>>,
    entry: Rc<Module>,
    counts: Counts,
}

impl Analysis {
    pub fn entry(&self) -> &Module {
        &self.entry
    }

    /// Every module, each before the modules that import it, and the entry
    /// last.
    pub fn modules(&self) -> impl DoubleEndedIterator<Item = &Module> {
        self.imports
            .iter()
            .chain(std::iter::once(&self.entry))
            .map(|m| &**m)
    }

    pub fn module(&self, key: &ModuleKey) -> Option<&Module> {
        self.modules().find(|m| m.key() == key)
    }

    /// Everything reported about every module.
    pub fn diagnostics(&self) -> Vec<Diagnostic> {
        self.modules()
            .flat_map(|m| m.diagnostics().cloned())
            .collect()
    }

    pub fn counts(&self) -> Counts {
        self.counts
    }

    fn specs(&self) -> Vec<CheckSpec> {
        self.modules().map(|m| m.spec.clone()).collect()
    }
}

pub struct Build {
    /// The stdlib's source directory, when the stdlib itself is being edited:
    /// its files on disk win over the copy built in.
    stdlib_root: Option<PathBuf>,
    stdlib_modules: Vec<ModulePath>,
    modules: HashMap<CheckSpec, Rc<Module>>,
    /// Each entry checked, and what its latest analysis reached. A module no
    /// entry reaches is dropped.
    entries: HashMap<CheckSpec, Vec<CheckSpec>>,
}

impl Default for Build {
    fn default() -> Self {
        Build::new(None)
    }
}

impl Build {
    pub fn new(stdlib_root: Option<PathBuf>) -> Self {
        Build {
            stdlib_root,
            stdlib_modules: module::stdlib_modules(),
            modules: HashMap::new(),
            entries: HashMap::new(),
        }
    }

    /// Check `entry`, and every module it imports that needs it, reading
    /// module text from `sources`.
    pub fn check(&mut self, sources: &mut dyn Sources, entry: Entry<'_>) -> Analysis {
        let spec = entry.spec();
        let mut run = CheckRun {
            cache: &self.modules,
            stdlib_root: self.stdlib_root.as_deref(),
            stdlib_modules: &self.stdlib_modules,
            sources,
            loading: Vec::new(),
            loaded: Loaded::default(),
            counts: Counts::default(),
        };
        let given = Given {
            ast: entry.ast,
            doc: entry.doc,
            parse: entry.parse,
        };
        let dir = entry.base_dir.map(Path::to_path_buf);
        let revision = Revision::of(&entry.source.text);
        let checked = run.settle(
            spec.clone(),
            Origin::Given,
            entry.source,
            revision,
            dir,
            Some(given),
        );
        let analysis = Analysis {
            imports: run.loaded.order,
            entry: checked,
            counts: run.counts,
        };
        for m in analysis.imports.iter().chain([&analysis.entry]) {
            self.modules.insert(m.spec.clone(), m.clone());
        }
        self.entries.insert(spec, analysis.specs());
        self.drop_unreached();
        analysis
    }

    /// Forget the entry `spec`, and every module only it reached.
    pub fn forget(&mut self, spec: &CheckSpec) {
        self.entries.remove(spec);
        self.drop_unreached();
    }

    fn drop_unreached(&mut self) {
        let live: HashSet<&CheckSpec> = self.entries.values().flatten().collect();
        self.modules.retain(|spec, _| live.contains(spec));
    }
}

/// The modules one run has loaded, by key and in the order they finished.
#[derive(Default)]
struct Loaded {
    by_key: HashMap<ModuleKey, Rc<Module>>,
    order: Vec<Rc<Module>>,
}

impl Loaded {
    fn add(&mut self, m: Rc<Module>) {
        self.by_key.insert(m.key().clone(), m.clone());
        self.order.push(m);
    }
}

/// The interfaces a check may read, which notes each one it does read: a
/// cached check holds only while those are unchanged.
struct Reading<'l> {
    loaded: &'l Loaded,
    read: RefCell<Vec<ModuleKey>>,
}

impl Interfaces for Reading<'_> {
    fn interface(&self, key: &ModuleKey) -> Option<&Interface> {
        let m = self.loaded.by_key.get(key)?;
        self.read.borrow_mut().push(key.clone());
        Some(&m.checked.interface)
    }
}

/// The parse an entry arrives with.
struct Given<'a> {
    ast: &'a ast::BlockExpression,
    doc: Option<String>,
    parse: Vec<Diagnostic>,
}

/// The tree a module is checked from: the one its entry arrived with, or
/// one parsed here.
enum Tree<'a> {
    Given(Given<'a>),
    Parsed(Parsed),
}

/// One check of an entry. It lives for that check only.
struct CheckRun<'r> {
    /// What earlier checks made, to reuse what is still fresh.
    cache: &'r HashMap<CheckSpec, Rc<Module>>,
    stdlib_root: Option<&'r Path>,
    stdlib_modules: &'r [ModulePath],
    sources: &'r mut dyn Sources,
    /// The modules being loaded, innermost last, for finding import cycles.
    loading: Vec<ModuleKey>,
    loaded: Loaded,
    counts: Counts,
}

impl CheckRun<'_> {
    /// The checked module for `source`: the cached check when it is still
    /// fresh, or a new one. Its imports are loaded first.
    fn settle(
        &mut self,
        spec: CheckSpec,
        origin: Origin,
        source: Rc<SourceFile>,
        revision: Revision,
        dir: Option<PathBuf>,
        given: Option<Given<'_>>,
    ) -> Rc<Module> {
        let cached = self.cache.get(&spec).cloned();
        // The imports come from the parse, or from a cached check of the
        // same text, which saves parsing it.
        let mut parsed = None;
        let written = match (&given, &cached) {
            (Some(g), _) => written_imports(g.ast),
            (None, Some(c)) if c.revision == revision => c.written.clone(),
            (None, _) => {
                let p = self.parse(&source.text);
                let w = written_imports(&p.ast);
                parsed = Some(p);
                w
            }
        };
        self.loading.push(spec.key.clone());
        let (imports, import_errors) = self.load_imports(&written, dir.as_deref());
        self.loading.pop();
        if let Some(c) = cached
            && freshness(&c, &origin, revision, &imports, &self.loaded) == Freshness::Fresh
        {
            return c;
        }
        let parsed = match (given, parsed) {
            (Some(g), _) => Tree::Given(g),
            (None, Some(p)) => Tree::Parsed(p),
            (None, None) => Tree::Parsed(self.parse(&source.text)),
        };
        let owned;
        let (ast, doc, mut load_diagnostics) = match parsed {
            Tree::Given(g) => (g.ast, g.doc, g.parse),
            Tree::Parsed(Parsed {
                ast,
                doc,
                diagnostics,
            }) => {
                owned = ast;
                (&owned, doc, diagnostics)
            }
        };
        load_diagnostics.extend(import_errors);
        for d in &mut load_diagnostics {
            d.source = Some(source.clone());
        }
        let parsed_cleanly = !has_errors(&load_diagnostics);
        let (checked, read) = self.check(&source, ast, doc, &imports, &spec, parsed_cleanly);
        Rc::new(Module {
            spec,
            origin,
            revision,
            imports,
            written,
            read,
            load_diagnostics,
            checked,
        })
    }

    fn parse(&mut self, text: &str) -> Parsed {
        self.counts.parses += 1;
        parse(text)
    }

    /// Check one module, against the interfaces loaded so far. Returns the
    /// result, and each interface it read.
    fn check(
        &mut self,
        source: &Rc<SourceFile>,
        ast: &ast::BlockExpression,
        doc: Option<String>,
        imports: &[Option<ModuleKey>],
        spec: &CheckSpec,
        parsed_cleanly: bool,
    ) -> (CheckedModule, Vec<(ModuleKey, Interface)>) {
        self.counts.checks += 1;
        let reading = Reading {
            loaded: &self.loaded,
            read: RefCell::new(Vec::new()),
        };
        let input = CheckInput {
            source,
            module: ast,
            imports,
            interfaces: &reading,
            role: spec.role,
            unused: spec.unused,
            doc,
            parsed_cleanly,
            stdlib_modules: self.stdlib_modules,
        };
        let checked = scarlet_check::check_module(&input);
        let mut seen = HashSet::new();
        let read = reading
            .read
            .into_inner()
            .into_iter()
            .filter(|k| seen.insert(k.clone()))
            .filter_map(|k| {
                let iface = self.loaded.by_key.get(&k)?.checked.interface.clone();
                Some((k, iface))
            })
            .collect();
        (checked, read)
    }

    /// Load the prelude and every module `written` names. Returns each
    /// import's key, in source order, and the errors the imports raised.
    fn load_imports(
        &mut self,
        written: &[(ast::ImportPath, Span)],
        base_dir: Option<&Path>,
    ) -> (Vec<Option<ModuleKey>>, Vec<Diagnostic>) {
        if !self.loading.contains(&prelude_key()) {
            let path = ast::ImportPath::canonical(module::scarlet_prelude());
            // The prelude is part of the compiler, so it always loads.
            let _ = self.load(&path, Span::DUMMY, None);
        }
        let mut keys = Vec::new();
        let mut errors = Vec::new();
        for (path, span) in written {
            match self.load(path, *span, base_dir) {
                Ok(k) => keys.push(Some(k)),
                Err(d) => {
                    errors.push(d);
                    keys.push(None);
                }
            }
        }
        (keys, errors)
    }

    /// Load the module `path` names, as a library.
    fn load(
        &mut self,
        path: &ast::ImportPath,
        at: Span,
        base_dir: Option<&Path>,
    ) -> Result<ModuleKey, Diagnostic> {
        let error = |msg: String| Diagnostic::error(at, DiagnosticCode::ModuleError, msg);
        let resolved = module::resolve_with(path, base_dir, self.stdlib_root).map_err(|e| match e {
            ResolveError::BareName(p) => error(format!(
                "Unknown module '{p}'. Package imports aren't supported yet, so use a relative path like `./{p}`"
            )),
            ResolveError::NoBaseDir => {
                error("Relative imports don't work without a file, such as in the REPL".to_string())
            }
            e => error(e.to_string()),
        })?;
        let key = resolved.key.clone();
        if self.loaded.by_key.contains_key(&key) {
            return Ok(key);
        }
        if self.loading.contains(&key) {
            return Err(error(format!(
                "Import cycle at module '{key}'. Modules can't import each other in a loop"
            )));
        }
        let (origin, dir, file, text) = match &resolved.source {
            ModuleSource::Embedded(s) => (Origin::Embedded, None, None, Text::new(*s)),
            ModuleSource::File(p) => {
                let text = self
                    .sources
                    .read(p)
                    .map_err(|e| error(format!("Couldn't read module '{key}': {e}")))?;
                (
                    Origin::File(p.clone()),
                    p.parent().map(Path::to_path_buf),
                    Some(p.clone()),
                    text,
                )
            }
        };
        let source = SourceFile::new(key.clone(), file, text.text);
        let m = self.settle(
            CheckSpec::library(key.clone()),
            origin,
            source,
            text.revision,
            dir,
            None,
        );
        self.loaded.add(m);
        Ok(key)
    }
}

/// A module's source, parsed and with its sugar removed.
pub struct Parsed {
    pub ast: ast::BlockExpression,
    pub doc: Option<String>,
    pub diagnostics: Vec<Diagnostic>,
}

pub fn parse(text: &str) -> Parsed {
    let mut sc = scarlet_syntax::scanner::new_scanner(text.to_string());
    let parser = scarlet_syntax::parser::new_parser(&mut sc);
    let mut result = parser.parse_program();
    scarlet_syntax::desugar::desugar_program(&mut result.ast);
    Parsed {
        ast: result.ast,
        doc: result.doc,
        diagnostics: result.diagnostics,
    }
}

/// Each `import` in `block`, as written, with its span.
fn written_imports(block: &ast::BlockExpression) -> Vec<(ast::ImportPath, Span)> {
    block
        .body
        .iter()
        .filter_map(|n| match n {
            ast::Node::Statement(s) => match s.as_ref() {
                ast::Statement::ImportDeclaration(d) => Some((d.path.clone(), d.span)),
                ast::Statement::Declaration { .. }
                | ast::Statement::Binding(_)
                | ast::Statement::Backpass(_) => None,
            },
            ast::Node::Expression(_) => None,
        })
        .collect()
}
