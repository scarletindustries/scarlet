//! A build: every module a program or an editor session reaches, each checked
//! once. A module is checked again only when its source changes or one of its
//! imports' interfaces does. Nothing is rewound: re-checking a module just
//! replaces its result.

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

use crate::module::{self, ModuleSource};

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

/// One checked module.
#[derive(Debug, Clone)]
pub struct Module {
    pub(crate) key: ModuleKey,
    pub origin: Origin,
    source_hash: u64,
    /// Each import's module, in source order, or `None` when it didn't
    /// resolve.
    imports: Vec<Option<ModuleKey>>,
    /// The interfaces of the imports as this check saw them.
    seen: Vec<(ModuleKey, Interface)>,
    /// The imports as written, and the directory `./` ones are relative to,
    /// so a module whose text hasn't changed can have its imports looked at
    /// again without parsing it.
    written: Vec<(ast::ImportPath, Span)>,
    dir: Option<PathBuf>,
    /// The file's stat when it was read, so an unchanged file needn't be
    /// read again. `None` for text that didn't come from the file.
    stat: Option<FileStat>,
    /// What the parser and the import resolution reported.
    load_diagnostics: Vec<Diagnostic>,
    role: Role,
    unused: UnusedBindings,
    pub checked: CheckedModule,
}

impl Module {
    /// Everything reported about this module.
    pub fn diagnostics(&self) -> impl Iterator<Item = &Diagnostic> {
        self.load_diagnostics
            .iter()
            .chain(&self.checked.diagnostics)
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

pub struct Build {
    /// The stdlib's source directory, when the stdlib itself is being edited:
    /// its files on disk win over the copy built in.
    stdlib_root: Option<PathBuf>,
    /// Editor buffers, which win over the file on disk.
    overlays: HashMap<PathBuf, String>,
    modules: HashMap<ModuleKey, Module>,
    loading: HashSet<ModuleKey>,
    /// The modules this check has already found up to date, so a module
    /// many others import is read once per check.
    verified: HashSet<ModuleKey>,
    stdlib_modules: Vec<ModulePath>,
    /// How many module checks ran, for tests of what gets checked again.
    checks: usize,
    /// How many imported modules were parsed, for the same.
    parses: usize,
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
            overlays: HashMap::new(),
            modules: HashMap::new(),
            loading: HashSet::new(),
            verified: HashSet::new(),
            stdlib_modules: module::stdlib_modules(),
            checks: 0,
            parses: 0,
        }
    }

    pub fn checks(&self) -> usize {
        self.checks
    }

    pub fn parses(&self) -> usize {
        self.parses
    }

    pub fn module(&self, key: &ModuleKey) -> Option<&Module> {
        self.modules.get(key)
    }

    pub(crate) fn modules(&self) -> impl Iterator<Item = &Module> {
        self.modules.values()
    }

    pub fn set_overlay(&mut self, path: PathBuf, text: String) {
        self.overlays.insert(path, text);
    }

    pub fn clear_overlay(&mut self, path: &Path) {
        self.overlays.remove(path);
    }

    /// Say that the file at `path` changed, so the next check reads it even
    /// if its stat looks the same. An edit within one clock tick that keeps
    /// the length and inode would otherwise go unseen.
    pub fn file_changed(&mut self, path: &Path) {
        for m in self.modules.values_mut() {
            if m.origin == Origin::File(path.to_path_buf()) {
                m.stat = None;
            }
        }
    }

    /// Check `entry`, and every module it imports that needs it. Returns the
    /// keys of the modules it reaches, imports first and `entry` last.
    pub fn check_entry(&mut self, entry: Entry<'_>) -> Vec<ModuleKey> {
        let key = entry.source.key.clone();
        let hash = module::source_hash(&entry.source.text);
        self.verified.clear();
        self.loading.insert(key.clone());
        let written = written_imports(entry.ast);
        let imports = self.load_imports(&written, entry.base_dir);
        self.loading.remove(&key);
        let (imports, import_errors) = imports;
        let mut load_diagnostics = entry.parse;
        load_diagnostics.extend(import_errors);
        for d in &mut load_diagnostics {
            d.source = Some(entry.source.clone());
        }
        let fresh = self.modules.get(&key).is_some_and(|m| {
            m.source_hash == hash
                && m.imports == imports
                && m.role == entry.role
                && m.unused == entry.unused
                && self.interfaces_unchanged(m)
        });
        if !fresh {
            let parsed_cleanly = !has_errors(&load_diagnostics);
            let checked = self.run_check(
                &entry.source,
                entry.ast,
                entry.doc,
                &imports,
                (entry.role, entry.unused),
                parsed_cleanly,
            );
            let seen = self.seen(&imports);
            self.modules.insert(
                key.clone(),
                Module {
                    key: key.clone(),
                    origin: Origin::Given,
                    source_hash: hash,
                    imports,
                    seen,
                    written,
                    dir: entry.base_dir.map(Path::to_path_buf),
                    stat: None,
                    load_diagnostics,
                    role: entry.role,
                    unused: entry.unused,
                    checked,
                },
            );
        }
        self.reach(&key)
    }

    /// The modules `key` reaches, imports first and `key` last.
    pub fn reach(&self, key: &ModuleKey) -> Vec<ModuleKey> {
        let mut out = Vec::new();
        let mut seen = HashSet::new();
        self.visit(key, &mut seen, &mut out);
        out
    }

    fn visit(&self, key: &ModuleKey, seen: &mut HashSet<ModuleKey>, out: &mut Vec<ModuleKey>) {
        if !seen.insert(key.clone()) {
            return;
        }
        let Some(m) = self.modules.get(key) else {
            return;
        };
        for k in m.imports.iter().flatten() {
            self.visit(k, seen, out);
        }
        // The prelude comes before everything, though nothing imports it.
        if *key != scarlet_types::known::prelude_key() {
            self.visit(&scarlet_types::known::prelude_key(), seen, out);
        }
        out.push(key.clone());
    }

    /// Every diagnostic of the modules `keys` names.
    pub fn diagnostics(&self, keys: &[ModuleKey]) -> Vec<Diagnostic> {
        keys.iter()
            .filter_map(|k| self.modules.get(k))
            .flat_map(|m| m.diagnostics().cloned())
            .collect()
    }

    fn interfaces_unchanged(&self, m: &Module) -> bool {
        m.seen.iter().all(|(k, iface)| {
            self.modules
                .get(k)
                .is_some_and(|d| d.checked.interface == *iface)
        })
    }

    fn seen(&self, imports: &[Option<ModuleKey>]) -> Vec<(ModuleKey, Interface)> {
        let mut keys: Vec<ModuleKey> = imports.iter().flatten().cloned().collect();
        keys.push(scarlet_types::known::prelude_key());
        keys.into_iter()
            .filter_map(|k| {
                let iface = self.modules.get(&k)?.checked.interface.clone();
                Some((k, iface))
            })
            .collect()
    }

    fn run_check(
        &mut self,
        source: &Rc<SourceFile>,
        ast: &ast::BlockExpression,
        doc: Option<String>,
        imports: &[Option<ModuleKey>],
        (role, unused): (Role, UnusedBindings),
        parsed_cleanly: bool,
    ) -> CheckedModule {
        self.checks += 1;
        let input = CheckInput {
            source,
            module: ast,
            imports,
            interfaces: &*self,
            role,
            unused,
            doc,
            parsed_cleanly,
            stdlib_modules: &self.stdlib_modules,
        };
        scarlet_check::check_module(&input)
    }

    /// Load the prelude and every module `written` names. Returns each
    /// import's key, in source order, and the errors the imports raised.
    fn load_imports(
        &mut self,
        written: &[(ast::ImportPath, Span)],
        base_dir: Option<&Path>,
    ) -> (Vec<Option<ModuleKey>>, Vec<Diagnostic>) {
        let prelude = scarlet_types::known::prelude_key();
        if !self.loading.contains(&prelude) {
            let path = ast::ImportPath::canonical(module::scarlet_prelude());
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

    /// Load the module `path` names, checking it if it hasn't been or needs
    /// to be again.
    fn load(
        &mut self,
        path: &ast::ImportPath,
        at: Span,
        base_dir: Option<&Path>,
    ) -> Result<ModuleKey, Diagnostic> {
        let error = |msg: String| Diagnostic::error(at, DiagnosticCode::ModuleError, msg);
        let resolved = module::resolve_with(path, base_dir, self.stdlib_root.as_deref()).map_err(|e| match e {
            ResolveError::BareName(p) => error(format!(
                "Unknown module '{p}'. Package imports aren't supported yet, so use a relative path like `./{p}`"
            )),
            ResolveError::NoBaseDir => {
                error("Relative imports don't work without a file, such as in the REPL".to_string())
            }
            e => error(e.to_string()),
        })?;
        let key = resolved.key.clone();
        if self.verified.contains(&key) {
            return Ok(key);
        }
        if self.loading.contains(&key) {
            return Err(error(format!(
                "Import cycle at module '{key}'. Modules can't import each other in a loop"
            )));
        }
        let (origin, dir) = match &resolved.source {
            ModuleSource::Embedded(_) => (Origin::Embedded, None),
            ModuleSource::File(p) => (Origin::File(p.clone()), p.parent().map(Path::to_path_buf)),
        };
        let read = |p: &Path| {
            std::fs::read_to_string(p)
                .map_err(|e| error(format!("Couldn't read module '{key}': {e}")))
        };

        // Find out whether the text is what the cached check saw, reading the
        // file only when its stat can't say.
        let cached = self.modules.get(&key);
        let mut text: Option<String> = None;
        let mut stat = None;
        let unchanged = match &resolved.source {
            ModuleSource::Embedded(_) => cached.is_some_and(|m| m.origin == Origin::Embedded),
            ModuleSource::File(p) => match self.overlays.get(p) {
                Some(t) => cached.is_some_and(|m| m.source_hash == module::source_hash(t)),
                None => {
                    stat = file_stat(p);
                    if stat.is_some() && cached.is_some_and(|m| m.stat == stat) {
                        true
                    } else {
                        let t = read(p)?;
                        let same = cached.is_some_and(|m| m.source_hash == module::source_hash(&t));
                        text = Some(t);
                        same
                    }
                }
            },
        };
        if unchanged && let Some(m) = cached {
            let (written, mdir) = (m.written.clone(), m.dir.clone());
            self.loading.insert(key.clone());
            let (imports, _) = self.load_imports(&written, mdir.as_deref());
            self.loading.remove(&key);
            if let Some(m) = self.modules.get(&key)
                && m.imports == imports
                && m.role == Role::Library
                && m.unused == UnusedBindings::Report
                && self.interfaces_unchanged(m)
            {
                if let Some(m) = self.modules.get_mut(&key)
                    && stat.is_some()
                {
                    m.stat = stat;
                }
                self.verified.insert(key.clone());
                return Ok(key);
            }
        }

        let text = match (text, &resolved.source) {
            (Some(t), _) => t,
            (None, ModuleSource::Embedded(s)) => s.to_string(),
            (None, ModuleSource::File(p)) => match self.overlays.get(p) {
                Some(t) => t.clone(),
                None => read(p)?,
            },
        };
        if let ModuleSource::File(p) = &resolved.source
            && self.overlays.contains_key(p)
        {
            stat = None;
        }
        let hash = module::source_hash(&text);
        let source = SourceFile::new(
            key.clone(),
            match &resolved.source {
                ModuleSource::File(p) => Some(p.clone()),
                ModuleSource::Embedded(_) => None,
            },
            text.as_str(),
        );
        self.parses += 1;
        let mut parsed = parse(&text);
        let written = written_imports(&parsed.ast);
        self.loading.insert(key.clone());
        let (imports, mut load_diagnostics) = self.load_imports(&written, dir.as_deref());
        self.loading.remove(&key);
        for d in parsed.diagnostics.iter_mut().chain(&mut load_diagnostics) {
            d.source = Some(source.clone());
        }
        let mut diagnostics = std::mem::take(&mut parsed.diagnostics);
        diagnostics.extend(load_diagnostics);
        let parsed_cleanly = !has_errors(&diagnostics);
        let checked = self.run_check(
            &source,
            &parsed.ast,
            parsed.doc,
            &imports,
            (Role::Library, UnusedBindings::Report),
            parsed_cleanly,
        );
        let seen = self.seen(&imports);
        self.modules.insert(
            key.clone(),
            Module {
                key: key.clone(),
                origin,
                source_hash: hash,
                imports,
                seen,
                written,
                dir,
                stat,
                load_diagnostics: diagnostics,
                role: Role::Library,
                unused: UnusedBindings::Report,
                checked,
            },
        );
        self.verified.insert(key.clone());
        Ok(key)
    }
}

impl Interfaces for Build {
    fn interface(&self, key: &ModuleKey) -> Option<&Interface> {
        self.modules.get(key).map(|m| &m.checked.interface)
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

pub fn source_hash(text: &str) -> u64 {
    module::source_hash(text)
}

/// A file's modification time, length and inode. An editor that saves by
/// replacing the file changes the inode even within the same second.
type FileStat = (std::time::SystemTime, u64, u64);

fn file_stat(path: &Path) -> Option<FileStat> {
    let meta = std::fs::metadata(path).ok()?;
    #[cfg(unix)]
    let ino = std::os::unix::fs::MetadataExt::ino(&meta);
    #[cfg(not(unix))]
    let ino = 0;
    Some((meta.modified().ok()?, meta.len(), ino))
}

/// Each `import` in `block`, as written, with its span.
fn written_imports(block: &ast::BlockExpression) -> Vec<(ast::ImportPath, Span)> {
    import_decls(block)
        .map(|d| (d.path.clone(), d.span))
        .collect()
}

fn import_decls(block: &ast::BlockExpression) -> impl Iterator<Item = &ast::ImportDeclaration> {
    block.body.iter().filter_map(|n| match n {
        ast::Node::Statement(s) => match s.as_ref() {
            ast::Statement::ImportDeclaration(d) => Some(d),
            ast::Statement::Declaration { .. }
            | ast::Statement::Binding(_)
            | ast::Statement::Backpass(_) => None,
        },
        ast::Node::Expression(_) => None,
    })
}
