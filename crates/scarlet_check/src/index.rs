//! What the editor needs from one checked module: every name it defines,
//! every place a name is used, and the type at each name. The checker builds
//! it as a value alongside the typed tree. The reference graph is made from
//! these, one per module.

use scarlet_syntax::span::Span;
use scarlet_types::name::ModuleKey;
use scarlet_types::ty::Type;

/// What kind of thing a definition is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum EntityKind {
    /// A local value, bound by a binding, a parameter or a pattern.
    Value,
    /// A module-level `fn`, `@vm` ones included.
    Function,
    Constant,
    Constructor,
    Type,
    /// The name an `import` binds.
    ModuleAlias,
    Field,
}

impl EntityKind {
    /// The noun for this kind, for hover and the unused-code hint.
    pub fn noun(self) -> &'static str {
        match self {
            EntityKind::Value => "value",
            EntityKind::Function => "function",
            EntityKind::Constant => "constant",
            EntityKind::Constructor => "constructor",
            EntityKind::Type => "type",
            EntityKind::ModuleAlias => "module",
            EntityKind::Field => "field",
        }
    }

    /// Whether go-to-definition should jump to a target of this kind. A
    /// module alias spans the whole `import`, so jumping to it goes nowhere.
    pub fn is_navigable(self) -> bool {
        !matches!(self, EntityKind::ModuleAlias)
    }
}

/// A definition's identity: its module, the span of its declaring name, and
/// its kind.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Site {
    pub module: ModuleKey,
    pub span: Span,
    pub entity: EntityKind,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DefKind {
    Value,
    Function {
        params: Vec<String>,
    },
    Constant,
    Constructor {
        /// The type that declares it. Using the constructor keeps the type
        /// alive.
        of: Site,
        fields: Vec<String>,
    },
    Type,
    ModuleAlias {
        /// The whole `import` declaration.
        decl: Span,
        /// The module it imports.
        imports: ModuleKey,
    },
    Field,
}

impl DefKind {
    pub(crate) fn entity(&self) -> EntityKind {
        match self {
            DefKind::Value => EntityKind::Value,
            DefKind::Function { .. } => EntityKind::Function,
            DefKind::Constant => EntityKind::Constant,
            DefKind::Constructor { .. } => EntityKind::Constructor,
            DefKind::Type => EntityKind::Type,
            DefKind::ModuleAlias { .. } => EntityKind::ModuleAlias,
            DefKind::Field => EntityKind::Field,
        }
    }
}

#[derive(Debug, Clone)]
pub struct Def {
    pub site: Site,
    pub name: String,
    pub doc: Option<String>,
    pub public: bool,
    pub kind: DefKind,
}

/// How a name is used where it occurs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum UseKind {
    /// `module::member`.
    Qualified,
    /// The `module` half of `module::member`. It doesn't evaluate anything,
    /// but it does use the import.
    Qualifier,
    /// A bare name.
    Unqualified,
    /// The last module segment in `import a/b`.
    Import,
    /// The `c` in `import a/b as c`.
    Alias,
    /// A definition's own name.
    Definition,
}

impl UseKind {}

#[derive(Debug, Clone)]
pub struct Use {
    pub span: Span,
    pub kind: UseKind,
    pub target: Site,
    /// The module-level definition the use is written inside, which is the
    /// edge dead-code analysis follows. `None` for a module statement.
    pub owner: Option<Site>,
}

/// The type of the name at `span`, for hover.
#[derive(Debug, Clone)]
pub struct Hover {
    pub span: Span,
    pub ty: Type,
}

#[derive(Debug, Clone, Default)]
pub struct SourceIndex {
    pub defs: Vec<Def>,
    pub uses: Vec<Use>,
    pub hovers: Vec<Hover>,
    /// The module's leading `/** */` comment.
    pub doc: Option<String>,
}
