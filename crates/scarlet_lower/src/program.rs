//! Lowering a whole program: every module's typed tree, in dependency order,
//! turned into one [`Program`]. It runs in steps, each making a value the
//! later ones read:
//!
//! 1. [`Ids`] hands out a [`TypeId`] for every nominal type, an [`RTy`] for
//!    every type, and a [`ConstId`] for every constant, as translation asks.
//! 2. [`Prelude`] finds the types and constructors lowering builds itself,
//!    before anything else, so none of them is ever a placeholder.
//! 3. [`Symbols`] gives every module function its [`FuncIdx`] and every
//!    module-level value its [`GlobalSlot`], before any body is made, so a
//!    body can name anything.
//! 4. Each body is translated. A lambda or wrapper it makes is [`Minted`]
//!    when it is finished, and takes the index that gives it.
//! 5. The pool is frozen, and every body is lowered against it.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fmt;

use scarlet_check::CleanModule;
use scarlet_check::typed::{BinderId, Expr, FnBody, Function, TypedModule};
use scarlet_ir::TypeId;
use scarlet_ir::core_ir::{
    Abi, AddressTypes, Const, ConstId, FuncIdx, GlobalSlot, HttpTypes, IoErrors, JoinId, JsonTypes,
    LocalId, LoweredFn, NetTypes, ParseReasons, Program, Radix, TlsTypes, TypeNames, VariantNames,
    VariantRef,
};
use scarlet_ir::rty::{ForeignRTy, PrimIds, RTy, Repr, ResolvedPool};
use scarlet_ir::tivec::TiVec;
use scarlet_types::def::{TypeBody, TypeDef};
use scarlet_types::engine::NotInEngine;
use scarlet_types::known::Known;
use scarlet_types::name::{CtorName, ModuleKey, Name, TypeName, ValueName};
use scarlet_types::ty::Type;

use crate::immediates::Immediates;
use crate::perceus::Dropped;
use crate::typed_ir::{BindingId, TempTys, TypedFn};
use crate::{immediates, lower, perceus, tail_args};

/// Where lowering reads a type's definition.
pub trait TypeDefs {
    fn type_def(&self, name: &TypeName) -> Option<&TypeDef>;
}

/// Why a program can't be built.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LinkError {
    /// The prelude has no constructor `ctor` of `ty`, which lowering builds
    /// values of itself.
    MissingPreludeCtor { ty: TypeName, ctor: &'static str },
    /// The program has more of something than a Core IR id can name.
    TooMany(Table),
    /// Lowering met something a checked program can't hold. It is a bug in
    /// the compiler, never in the program.
    Internal(InternalLinkError),
}

/// A table whose ids are 32 bits wide in Core IR.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Table {
    /// Named types, by `TypeId`.
    TypeNames,
    /// Type nodes in the program's pool, by `RTy`.
    Types,
    /// Constants, by `ConstId`.
    Constants,
    /// One function's locals, by `LocalId`.
    Locals,
    /// Functions, by `FuncIdx`.
    Functions,
    /// One function's join points, by `JoinId`.
    Joins,
    /// Global slots, by `GlobalSlot`.
    Globals,
    /// One closure's captures, by `CaptureIdx`.
    Captures,
}

impl fmt::Display for Table {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Table::TypeNames => "named types",
            Table::Types => "types",
            Table::Constants => "constants",
            Table::Locals => "locals in one function",
            Table::Functions => "functions",
            Table::Joins => "join points in one function",
            Table::Globals => "globals",
            Table::Captures => "captures in one closure",
        })
    }
}

/// What lowering found that a checked program can't hold.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InternalLinkError {
    /// A body names a function that has no index.
    NoFunction(ValueName),
    /// A body reads a const that has no slot.
    NoConst(ValueName),
    /// The program has no modules, so no entry.
    NoEntryModule,
    /// Something called or made as a function has a type that isn't a
    /// function's.
    NotAFunction(RTy),
    /// A binder is bound in a function that doesn't bind it.
    UnownedBinder(Name),
    /// A clean module holds a node the checker marked as an error.
    ErrorNode,
    /// A spread fills a field slot its constructor doesn't have.
    NoFieldSlot { ctor: CtorName, slot: usize },
    /// A spread sits after more arguments than its constructor was given.
    NoSpreadPlace { ctor: CtorName, at: usize },
    /// A named type that no module of the program defines, or that is only
    /// another name for a type. Nothing says what its values are.
    NoRepr(TypeName),
    /// A field is read from a value whose type isn't a named type.
    FieldOfUnnamed(Type),
    /// A named type has no variant at this index.
    NoVariant { ty: TypeName, variant: usize },
    /// An `or` on a `Result` whose type has no error argument.
    ResultWithoutError(RTy),
    /// An array pattern on a value whose type isn't an array.
    NotAnArray(RTy),
    /// Core IR reads a local `lower` never gave a type.
    UntypedLocal(LocalId),
    /// A `Goto` jumps to a join no enclosing `LetCont` declares.
    UnboundJoin(JoinId),
    /// A body reads a binding where no binding of it is in scope.
    UnboundBinding(BindingId),
    /// A body names a binding past the end of its function's binder table.
    BindingOutOfRange(BindingId),
    /// A field or element index too wide for an instruction to carry.
    IndexTooWide(usize),
    /// A body names a binder its module never made.
    UnknownBinder(BinderId),
    /// A type that isn't in the pool lowering builds.
    ForeignRTy(ForeignRTy),
    /// A type that isn't in its module's frozen types.
    NotInEngine(NotInEngine),
}

impl From<ForeignRTy> for InternalLinkError {
    fn from(e: ForeignRTy) -> Self {
        InternalLinkError::ForeignRTy(e)
    }
}

impl From<NotInEngine> for InternalLinkError {
    fn from(e: NotInEngine) -> Self {
        InternalLinkError::NotInEngine(e)
    }
}

impl From<InternalLinkError> for LinkError {
    fn from(e: InternalLinkError) -> Self {
        LinkError::Internal(e)
    }
}

impl fmt::Display for LinkError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            LinkError::MissingPreludeCtor { ctor, .. } => {
                write!(f, "prelude: constructor '{ctor}' is required")
            }
            LinkError::TooMany(t) => {
                write!(f, "the program has more {t} than a 32-bit id can name")
            }
            LinkError::Internal(e) => write!(f, "internal compiler error: {e}"),
        }
    }
}

impl fmt::Display for InternalLinkError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        use InternalLinkError as E;
        match self {
            E::NoFunction(v) => write!(f, "no function `{}::{}`", v.module, v.name),
            E::NoConst(v) => write!(f, "no const `{}::{}`", v.module, v.name),
            E::NoEntryModule => write!(f, "a program with no entry module"),
            E::NotAFunction(t) => write!(f, "type :{} isn't a function's", t.0),
            E::UnownedBinder(n) => write!(f, "`{n}` is bound in a function that doesn't own it"),
            E::ErrorNode => write!(f, "a clean module holds an error node"),
            E::NoFieldSlot { ctor, slot } => write!(
                f,
                "a spread fills field {slot} of variant {} of `{}`, which has no such field",
                ctor.variant,
                ctor.type_name.name()
            ),
            E::NoSpreadPlace { ctor, at } => write!(
                f,
                "a spread sits after {at} arguments of variant {} of `{}`, which was given fewer",
                ctor.variant,
                ctor.type_name.name()
            ),
            E::NoRepr(t) => write!(f, "nothing says what a value of `{}` is", t.name()),
            E::FieldOfUnnamed(t) => write!(f, "a field read from a value of type `{t}`"),
            E::NoVariant { ty, variant } => {
                write!(f, "`{}` has no variant {variant}", ty.name())
            }
            E::ResultWithoutError(t) => {
                write!(f, "an `or` on a Result, type :{}, with no error type", t.0)
            }
            E::NotAnArray(t) => {
                write!(f, "an array pattern on type :{}, which isn't an array", t.0)
            }
            E::UntypedLocal(l) => write!(f, "local {l} has no type"),
            E::UnboundJoin(j) => write!(f, "a goto to join {j}, which nothing declares"),
            E::UnboundBinding(b) => write!(f, "{b} is read where it isn't bound"),
            E::BindingOutOfRange(b) => {
                write!(f, "{b} is past the end of its function's binder table")
            }
            E::IndexTooWide(i) => write!(f, "index {i} is too wide for an instruction"),
            E::UnknownBinder(b) => write!(f, "binder {} isn't in its module", b.0),
            E::ForeignRTy(e) => write!(f, "{e}"),
            E::NotInEngine(e) => write!(f, "{e}"),
        }
    }
}

/// The ids handed out for types and constants, and the pool of types.
pub(crate) struct Ids<'a> {
    pub defs: &'a dyn TypeDefs,
    pool: ResolvedPool,
    /// Each named type's id, and what its values are.
    type_ids: HashMap<TypeName, (TypeId, Repr)>,
    /// Each type's name, by `TypeId`, from 1.
    type_names: Vec<TypeName>,
    rtys: HashMap<Type, RTy>,
    consts: Vec<Const>,
    const_ids: HashMap<Const, ConstId>,
}

impl<'a> Ids<'a> {
    fn new(defs: &'a dyn TypeDefs) -> Self {
        let known = Known::new();
        let mut type_ids = HashMap::new();
        let mut type_names = Vec::new();
        // The four primitives get ids 1 to 4.
        let prims = PrimIds {
            int: TypeId(1),
            float: TypeId(2),
            string: TypeId(3),
            array: TypeId(4),
        };
        for (n, id, repr) in [
            (&known.int, prims.int, Repr::Counted),
            (&known.float, prims.float, Repr::Word),
            (&known.string, prims.string, Repr::Counted),
            (&known.array, prims.array, Repr::Counted),
        ] {
            type_names.push(n.clone());
            type_ids.insert(n.clone(), (id, repr));
        }
        Ids {
            defs,
            pool: ResolvedPool::new(prims),
            type_ids,
            type_names,
            rtys: HashMap::new(),
            consts: Vec::new(),
            const_ids: HashMap::new(),
        }
    }

    /// The pool as it stands, to read a type already made.
    pub fn pool(&self) -> &ResolvedPool {
        &self.pool
    }

    pub fn type_id(&mut self, n: &TypeName) -> Result<TypeId, LinkError> {
        Ok(self.named(n)?.0)
    }

    /// Ids count from 1, so the next one is the table's length plus one.
    fn named(&mut self, n: &TypeName) -> Result<(TypeId, Repr), LinkError> {
        if let Some(&named) = self.type_ids.get(n) {
            return Ok(named);
        }
        let id = i32::try_from(self.type_names.len())
            .ok()
            .and_then(|len| len.checked_add(1))
            .map(TypeId)
            .ok_or(LinkError::TooMany(Table::TypeNames))?;
        let def = self.defs.type_def(n);
        let repr = def
            .and_then(repr_of)
            .ok_or_else(|| InternalLinkError::NoRepr(n.clone()))?;
        self.type_names.push(n.clone());
        self.type_ids.insert(n.clone(), (id, repr));
        Ok((id, repr))
    }

    /// The constructor of `t` named `variant`, or `None` when `t` has none.
    pub fn variant(
        &mut self,
        t: &TypeName,
        variant: &str,
    ) -> Result<Option<VariantRef>, LinkError> {
        let Some(idx) = self
            .defs
            .type_def(t)
            .and_then(TypeDef::variants)
            .and_then(|vs| vs.iter().position(|v| &*v.name == variant))
        else {
            return Ok(None);
        };
        let variant_idx = u16::try_from(idx).map_err(|_| InternalLinkError::IndexTooWide(idx))?;
        Ok(Some(VariantRef {
            type_id: self.type_id(t)?,
            variant_idx,
        }))
    }

    pub fn ctor_ref(&mut self, t: &TypeName, variant: u16) -> Result<VariantRef, LinkError> {
        Ok(VariantRef {
            type_id: self.type_id(t)?,
            variant_idx: variant,
        })
    }

    /// A finished type in the program's pool. An open variable is a `Bound`:
    /// a type nothing knows the representation of.
    pub fn rty(&mut self, t: &Type) -> Result<RTy, LinkError> {
        if let Some(&r) = self.rtys.get(t) {
            return Ok(r);
        }
        let r = match t {
            Type::Var(v) => {
                let idx = u32::try_from(v.id).map_err(|_| LinkError::TooMany(Table::Types))?;
                self.pool.mk_bound(idx)
            }
            Type::Named { name, args } => {
                let args = self.rtys_of(args)?;
                let (id, repr) = self.named(name)?;
                self.pool.mk_con(id, repr, &args)
            }
            Type::Fn { params, ret } => {
                let params = self.rtys_of(params)?;
                let ret = self.rty(ret)?;
                self.pool.mk_fun(&params, ret)
            }
            Type::Tuple(elems) => {
                let elems = self.rtys_of(elems)?;
                self.pool.mk_tuple(&elems)
            }
        }
        .map_err(|_| LinkError::TooMany(Table::Types))?;
        self.rtys.insert(t.clone(), r);
        Ok(r)
    }

    fn rtys_of(&mut self, ts: &[Type]) -> Result<Vec<RTy>, LinkError> {
        ts.iter().map(|t| self.rty(t)).collect()
    }

    pub fn tuple(&mut self, elems: &[RTy]) -> Result<RTy, LinkError> {
        self.pool
            .mk_tuple(elems)
            .map_err(|_| LinkError::TooMany(Table::Types))
    }

    pub fn constant(&mut self, c: Const) -> Result<ConstId, LinkError> {
        if let Some(&id) = self.const_ids.get(&c) {
            return Ok(id);
        }
        let id = u32::try_from(self.consts.len())
            .map(ConstId)
            .map_err(|_| LinkError::TooMany(Table::Constants))?;
        self.consts.push(c.clone());
        self.const_ids.insert(c, id);
        Ok(id)
    }

    /// Everything handed out, once nothing more will be.
    fn freeze(self) -> Frozen<'a> {
        Frozen {
            defs: self.defs,
            pool: self.pool,
            type_names: self.type_names,
            consts: self.consts,
        }
    }
}

/// What a value of `def` is. A constructor with no fields is a word and one
/// with any is a cell. The host's values may be cells. An alias has no values.
fn repr_of(def: &TypeDef) -> Option<Repr> {
    match &def.body {
        TypeBody::Variants { variants, .. } => {
            let plain = !variants.is_empty() && variants.iter().all(|v| v.fields.is_empty());
            Some(if plain { Repr::Word } else { Repr::Counted })
        }
        TypeBody::External { .. } => Some(Repr::Counted),
        TypeBody::Alias(_) => None,
    }
}

/// The ids once lowering stops handing them out.
struct Frozen<'a> {
    defs: &'a dyn TypeDefs,
    pool: ResolvedPool,
    type_names: Vec<TypeName>,
    consts: Vec<Const>,
}

impl Frozen<'_> {
    fn names_of(&self, id: TypeId) -> TypeNames {
        // Ids count from 1.
        let name =
            id.0.checked_sub(1)
                .and_then(|i| usize::try_from(i).ok())
                .and_then(|i| self.type_names.get(i));
        let def = name.and_then(|n| self.defs.type_def(n));
        TypeNames {
            name: name.map_or_else(String::new, |n| n.name().to_string()),
            variants: def
                .and_then(|d| d.variants())
                .unwrap_or_default()
                .iter()
                .map(|v| VariantNames {
                    name: v.name.to_string(),
                    fields: v.fields.iter().map(|f| f.label.to_string()).collect(),
                })
                .collect(),
        }
    }
}

/// The prelude constructors lowering builds itself.
#[derive(Debug, Clone, Copy)]
pub(crate) struct PreludeCtors {
    pub some: VariantRef,
    pub none: VariantRef,
    pub ok: VariantRef,
    pub err: VariantRef,
    pub true_: VariantRef,
    pub false_: VariantRef,
    pub nil: VariantRef,
}

/// The prelude's types and constructors that lowering builds values of
/// itself, each found once, before any body is translated.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Prelude {
    pub temps: TempTys,
    pub ctors: PreludeCtors,
    pub nil: RTy,
}

impl Prelude {
    fn resolve(ids: &mut Ids<'_>) -> Result<Self, LinkError> {
        let known = Known::new();
        let named = |ids: &mut Ids<'_>, n: &TypeName| ids.rty(&Type::named(n.clone(), vec![]));
        let int = named(ids, &known.int)?;
        let temps = TempTys {
            int,
            bool: named(ids, &known.bool)?,
            string: named(ids, &known.string)?,
            binary: named(ids, &known.binary)?,
            int_pair: ids.tuple(&[int, int])?,
        };
        let mut variant = |t: &TypeName, v: &'static str| {
            ids.variant(t, v)?
                .ok_or_else(|| LinkError::MissingPreludeCtor {
                    ty: t.clone(),
                    ctor: v,
                })
        };
        let ctors = PreludeCtors {
            some: variant(&known.option, "Some")?,
            none: variant(&known.option, "None")?,
            ok: variant(&known.result, "Ok")?,
            err: variant(&known.result, "Err")?,
            true_: variant(&known.bool, "True")?,
            false_: variant(&known.bool, "False")?,
            nil: variant(&known.nil, "Nil")?,
        };
        let nil = named(ids, &known.nil)?;
        Ok(Prelude { temps, ctors, nil })
    }
}

/// Where one module function's body is: its module, and its place in the
/// module's functions.
pub(crate) struct Site<'m> {
    pub module: &'m TypedModule,
    pub function: &'m Function,
    /// The function's Scarlet body. Only a function with one gets a site.
    pub body: &'m Expr,
}

/// Every module function's index and every module-level value's slot, all
/// known before any body is translated.
pub(crate) struct Symbols<'m> {
    fns: HashMap<ValueName, FuncIdx>,
    consts: HashMap<ValueName, GlobalSlot>,
    binders: HashMap<(ModuleKey, BinderId), GlobalSlot>,
    /// Each module function, in `FuncIdx` order.
    sites: Vec<Site<'m>>,
    globals: u32,
}

impl<'m> Symbols<'m> {
    fn of(modules: &[&'m TypedModule]) -> Result<Self, LinkError> {
        let mut s = Symbols {
            fns: HashMap::new(),
            consts: HashMap::new(),
            binders: HashMap::new(),
            sites: Vec::new(),
            globals: 0,
        };
        // A slot is an `i32`, and the count of them a `u32`, so both must fit.
        let global = |n: &mut u32| {
            let g = i32::try_from(*n)
                .map(GlobalSlot)
                .map_err(|_| LinkError::TooMany(Table::Globals))?;
            *n = n.checked_add(1).ok_or(LinkError::TooMany(Table::Globals))?;
            Ok::<_, LinkError>(g)
        };
        for &m in modules {
            for f in &m.functions {
                if let FnBody::Expr(body) = &f.body {
                    let idx = u32::try_from(s.sites.len())
                        .map(FuncIdx)
                        .map_err(|_| LinkError::TooMany(Table::Functions))?;
                    s.fns.insert(value_name(&m.key, &f.name), idx);
                    s.sites.push(Site {
                        module: m,
                        function: f,
                        body,
                    });
                }
            }
            for c in &m.consts {
                s.consts
                    .insert(value_name(&m.key, &c.name), global(&mut s.globals)?);
            }
            for (i, b) in m.binders.iter().enumerate() {
                if b.module_scope {
                    s.binders
                        .insert((m.key.clone(), BinderId(i)), global(&mut s.globals)?);
                }
            }
        }
        Ok(s)
    }

    pub fn func(&self, name: &ValueName) -> Result<FuncIdx, LinkError> {
        self.fns
            .get(name)
            .copied()
            .ok_or_else(|| InternalLinkError::NoFunction(name.clone()).into())
    }

    pub fn const_slot(&self, name: &ValueName) -> Result<GlobalSlot, LinkError> {
        self.consts
            .get(name)
            .copied()
            .ok_or_else(|| InternalLinkError::NoConst(name.clone()).into())
    }

    /// The global slot of a module-scope binder. Any other binder is a local
    /// and has none.
    pub fn binder_slot(&self, module: &ModuleKey, b: BinderId) -> Option<GlobalSlot> {
        self.binders.get(&(module.clone(), b)).copied()
    }

    /// How many module functions there are. Every minted function comes
    /// after them.
    fn fn_count(&self) -> Result<u32, LinkError> {
        u32::try_from(self.sites.len()).map_err(|_| LinkError::TooMany(Table::Functions))
    }
}

fn value_name(module: &ModuleKey, name: &scarlet_types::name::Name) -> ValueName {
    ValueName {
        module: module.clone(),
        name: name.clone(),
    }
}

/// A function body with the names a person reads it by.
pub(crate) struct Named {
    pub module: String,
    pub name: String,
    pub body: TypedFn,
}

/// The functions translation makes that the source doesn't declare: lambdas
/// and the wrappers around constructors and builtins used as values. Each is
/// added once its body is finished, and its index is where it went.
pub(crate) struct Minted {
    /// The first index a minted function takes: one past the module
    /// functions.
    base: u32,
    fns: Vec<Named>,
}

impl Minted {
    /// Every function index, minted or declared, fits in a `FuncIdx`, so the
    /// program's function table can name all of them.
    pub fn push(&mut self, f: Named) -> Result<FuncIdx, LinkError> {
        let idx = u32::try_from(self.fns.len())
            .ok()
            .and_then(|n| self.base.checked_add(n))
            .ok_or(LinkError::TooMany(Table::Functions))?;
        self.fns.push(f);
        Ok(FuncIdx(idx))
    }
}

/// Build the program whose modules are `modules`, imports first and the
/// entry last. `main` is the entry's function to start at, when it runs one.
pub fn build_program(
    modules: &[&CleanModule],
    defs: &dyn TypeDefs,
    main: Option<&ValueName>,
) -> Result<Program, LinkError> {
    let modules: Vec<&TypedModule> = modules.iter().map(|m| m.module()).collect();
    let mut ids = Ids::new(defs);
    let prelude = Prelude::resolve(&mut ids)?;
    let symbols = Symbols::of(&modules)?;
    let mut minted = Minted {
        base: symbols.fn_count()?,
        fns: Vec::new(),
    };
    let mut cx = crate::translate::Cx {
        ids: &mut ids,
        prelude: &prelude,
        symbols: &symbols,
        minted: &mut minted,
    };
    let declared: Vec<Named> = symbols
        .sites
        .iter()
        .map(|site| crate::translate::function(&mut cx, site))
        .collect::<Result<_, _>>()?;
    let tops: Vec<TypedFn> = modules
        .iter()
        .map(|m| crate::translate::toplevel(&mut cx, m))
        .collect::<Result<_, _>>()?;
    let main = main.map(|m| symbols.func(m)).transpose()?;
    let abi = Abi {
        some: prelude.ctors.some,
        none: prelude.ctors.none,
        ok: prelude.ctors.ok,
        err: prelude.ctors.err,
        radix: present(radix(&mut ids))?,
        io: present(io_errors(&mut ids))?,
        json: present(json_types(&mut ids))?,
        http: present(http_types(&mut ids))?,
        address: present(address_types(&mut ids))?,
        net: present(net_types(&mut ids))?,
        tls: present(tls_types(&mut ids))?,
    };

    let frozen = ids.freeze();
    let im = Immediates {
        true_: prelude.ctors.true_,
        false_: prelude.ctors.false_,
        nil: prelude.ctors.nil,
    };
    let finish = |f: &TypedFn, module: String, name: String| {
        let Dropped { core, undropped } = core_fn(prelude.temps, &im, &frozen.pool, f)?;
        Ok(LoweredFn::new(module, name, core, undropped))
    };
    let fns: TiVec<FuncIdx, LoweredFn> = declared
        .iter()
        .chain(&minted.fns)
        .map(|n| finish(&n.body, n.module.clone(), n.name.clone()))
        .collect::<Result<_, LinkError>>()?;
    // The entry's top level is named for what it is; each import's, for its
    // module.
    let mut inits: Vec<LoweredFn> = modules
        .iter()
        .zip(&tops)
        .map(|(m, f)| {
            let key = m.key.as_str();
            finish(f, key.to_string(), key.to_string())
        })
        .collect::<Result<_, LinkError>>()?;
    let mut toplevel = inits.pop().ok_or(InternalLinkError::NoEntryModule)?;
    toplevel.name = "__main__".to_string();

    let mut used: BTreeSet<TypeId> = abi.variants().iter().map(|v| v.type_id).collect();
    for f in (&fns).into_iter().chain(&inits).chain([&toplevel]) {
        f.core.body.for_each_variant(|v| {
            used.insert(v.type_id);
        });
    }
    let types: BTreeMap<TypeId, TypeNames> = used
        .into_iter()
        .map(|id| (id, frozen.names_of(id)))
        .collect();
    let Frozen { pool, consts, .. } = frozen;
    Ok(Program {
        fns,
        consts,
        inits,
        toplevel,
        main,
        globals: symbols.globals,
        types,
        abi,
        pool,
    })
}

/// Typed IR to the Core IR a backend runs: `lower`, then `immediates`, then
/// `perceus`, then `tail_args`. This is the only place they run, so they can't
/// run out of order. It comes with whether `perceus` left something undropped.
/// The order of a function's `Let`s then says when that is given up, so
/// `tail_args` leaves it as it is.
fn core_fn(
    temps: TempTys,
    im: &Immediates,
    pool: &ResolvedPool,
    f: &TypedFn,
) -> Result<Dropped, LinkError> {
    let lower::Lowered { core, locals } = lower::lower_fn(temps, f)?;
    let core = immediates::rewrite(core, im);
    let Dropped { core, undropped } = perceus::perceus(pool, &locals, core)?;
    let core = if undropped {
        core
    } else {
        tail_args::schedule(pool, core)?
    };
    Ok(Dropped { core, undropped })
}

/// Why a stdlib type's constructors can't be listed.
enum Absent {
    /// The program doesn't reach the type, or the type lacks a constructor.
    No,
    Link(LinkError),
}

impl From<LinkError> for Absent {
    fn from(e: LinkError) -> Self {
        Absent::Link(e)
    }
}

/// `r`, with a type the program doesn't reach as `None`.
fn present<T>(r: Result<T, Absent>) -> Result<Option<T>, LinkError> {
    match r {
        Ok(t) => Ok(Some(t)),
        Err(Absent::No) => Ok(None),
        Err(Absent::Link(e)) => Err(e),
    }
}

/// The constructors of the stdlib type `module.name`, by name, when the
/// program reaches that module.
fn stdlib_variants<'b, 'd>(
    b: &'b mut Ids<'d>,
    module: &[&str],
    name: &str,
) -> Result<impl FnMut(&str) -> Result<VariantRef, Absent> + use<'b, 'd>, Absent> {
    let path: Vec<String> = module.iter().map(|s| s.to_string()).collect();
    let t = TypeName::new(ModuleKey::for_stdlib(&path), name.into());
    b.defs.type_def(&t).ok_or(Absent::No)?;
    Ok(move |v: &str| b.variant(&t, v)?.ok_or(Absent::No))
}

fn radix(b: &mut Ids<'_>) -> Result<Radix, Absent> {
    let mut v = stdlib_variants(b, &["scarlet", "binary"], "Radix")?;
    Ok(Radix {
        dec: v("Dec")?,
        hex: v("Hex")?,
    })
}

fn io_errors(b: &mut Ids<'_>) -> Result<IoErrors, Absent> {
    let mut v = stdlib_variants(b, &["scarlet", "io"], "IoError")?;
    Ok(IoErrors {
        not_found: v("NotFound")?,
        permission_denied: v("PermissionDenied")?,
        already_exists: v("AlreadyExists")?,
        not_a_directory: v("NotADirectory")?,
        is_a_directory: v("IsADirectory")?,
        read_only_filesystem: v("ReadOnlyFilesystem")?,
        filesystem_loop: v("FilesystemLoop")?,
        file_too_large: v("FileTooLarge")?,
        storage_full: v("StorageFull")?,
        quota_exceeded: v("QuotaExceeded")?,
        unaligned_binary: v("UnalignedBinary")?,
        errno: v("Errno")?,
    })
}

fn address_types(b: &mut Ids<'_>) -> Result<AddressTypes, Absent> {
    let module = ["scarlet", "net", "address"];
    let (v4, v6) = {
        let mut ip = stdlib_variants(b, &module, "IpAddress")?;
        (ip("V4")?, ip("V6")?)
    };
    let socket_address = stdlib_variants(b, &module, "SocketAddress")?("SocketAddress")?;
    Ok(AddressTypes {
        v4,
        v6,
        socket_address,
    })
}

fn net_types(b: &mut Ids<'_>) -> Result<NetTypes, Absent> {
    let socket_module = ["scarlet", "net", "socket"];
    let socket = stdlib_variants(b, &socket_module, "Socket")?("Socket")?;
    let (data, closed) = {
        let mut r = stdlib_variants(b, &socket_module, "Read")?;
        (r("Data")?, r("Closed")?)
    };
    let mut e = stdlib_variants(b, &["scarlet", "net", "error"], "NetError")?;
    Ok(NetTypes {
        timed_out: e("TimedOut")?,
        connection_refused: e("ConnectionRefused")?,
        connection_reset: e("ConnectionReset")?,
        connection_aborted: e("ConnectionAborted")?,
        not_connected: e("NotConnected")?,
        broken_pipe: e("BrokenPipe")?,
        addr_in_use: e("AddrInUse")?,
        addr_not_available: e("AddrNotAvailable")?,
        network_down: e("NetworkDown")?,
        network_unreachable: e("NetworkUnreachable")?,
        host_unreachable: e("HostUnreachable")?,
        permission_denied: e("PermissionDenied")?,
        unexpected_eof: e("UnexpectedEof")?,
        unaligned_binary: e("UnalignedBinary")?,
        invalid_port: e("InvalidPort")?,
        errno: e("Errno")?,
        socket,
        data,
        closed,
    })
}

fn tls_types(b: &mut Ids<'_>) -> Result<TlsTypes, Absent> {
    let module = ["scarlet", "net", "tls"];
    let tls_socket = stdlib_variants(b, &module, "TlsSocket")?("TlsSocket")?;
    let mut e = stdlib_variants(b, &module, "TlsError")?;
    Ok(TlsTypes {
        tls_socket,
        certificate_unknown_issuer: e("CertificateUnknownIssuer")?,
        certificate_expired: e("CertificateExpired")?,
        certificate_not_yet_valid: e("CertificateNotYetValid")?,
        certificate_revoked: e("CertificateRevoked")?,
        hostname_mismatch: e("HostnameMismatch")?,
        bad_certificate: e("BadCertificate")?,
        protocol_error: e("ProtocolError")?,
        handshake_failed: e("HandshakeFailed")?,
        invalid_server_name: e("InvalidServerName")?,
        transport: e("Transport")?,
    })
}

fn json_types(b: &mut Ids<'_>) -> Result<JsonTypes, Absent> {
    let module = ["scarlet", "json"];
    let doc = stdlib_variants(b, &module, "Doc")?("Doc")?;
    let parse_error = stdlib_variants(b, &module, "ParseError")?("ParseError")?;
    let reasons = {
        let mut r = stdlib_variants(b, &module, "ParseReason")?;
        ParseReasons {
            unexpected_end: r("UnexpectedEnd")?,
            unexpected_byte: r("UnexpectedByte")?,
            trailing_data: r("TrailingData")?,
            unterminated_string: r("UnterminatedString")?,
            bad_escape: r("BadEscape")?,
            bad_unicode: r("BadUnicode")?,
            bad_number: r("BadNumber")?,
            not_utf8: r("NotUtf8")?,
            not_whole_bytes: r("NotWholeBytes")?,
            too_large: r("TooLarge")?,
        }
    };
    let mut j = stdlib_variants(b, &module, "Json")?;
    Ok(JsonTypes {
        doc,
        parse_error,
        reasons,
        null: j("Null")?,
        boolean: j("Boolean")?,
        integer: j("Integer")?,
        real: j("Real")?,
        str: j("Str")?,
        list: j("List")?,
        object: j("Object")?,
        number: j("Number")?,
    })
}

fn http_types(b: &mut Ids<'_>) -> Result<HttpTypes, Absent> {
    let h1 = ["scarlet", "http", "h1"];
    let header = stdlib_variants(b, &["scarlet", "http", "headers"], "Header")?("Header")?;
    let (http10, http11) = {
        let mut v = stdlib_variants(b, &h1, "Version")?;
        (v("Http10")?, v("Http11")?)
    };
    let head_flags = stdlib_variants(b, &h1, "HeadFlags")?("HeadFlags")?;
    let (conn_neither, conn_close, conn_keep_alive, conn_both) = {
        let mut c = stdlib_variants(b, &h1, "ConnTokens")?;
        (
            c("ConnNeither")?,
            c("ConnClose")?,
            c("ConnKeepAlive")?,
            c("ConnBoth")?,
        )
    };
    let (parsed_done, parsed_need_more, parsed_bad) = {
        let mut p = stdlib_variants(b, &h1, "Parsed")?;
        (p("Done")?, p("NeedMore")?, p("Bad")?)
    };
    let (response_done, response_need_more, response_bad) = {
        let mut r = stdlib_variants(b, &h1, "ParsedResponse")?;
        (
            r("ResponseDone")?,
            r("ResponseNeedMore")?,
            r("ResponseBad")?,
        )
    };
    let (bad_status_line, bad_version, bad_field, head_too_large, bad_framing) = {
        let mut x = stdlib_variants(b, &h1, "BadResponse")?;
        (
            x("BadStatusLine")?,
            x("BadVersion")?,
            x("BadField")?,
            x("HeadTooLarge")?,
            x("BadFraming")?,
        )
    };
    let (no_body, length, chunked, framing_invalid) = {
        let mut f = stdlib_variants(b, &h1, "Framing")?;
        (f("NoBody")?, f("Length")?, f("Chunked")?, f("Invalid")?)
    };
    let (chunked_done, chunked_need_more, chunked_bad) = {
        let mut c = stdlib_variants(b, &h1, "ChunkBody")?;
        (c("ChunkedDone")?, c("ChunkedNeedMore")?, c("ChunkedBad")?)
    };
    Ok(HttpTypes {
        header,
        http10,
        http11,
        head_flags,
        conn_neither,
        conn_close,
        conn_keep_alive,
        conn_both,
        parsed_done,
        parsed_need_more,
        parsed_bad,
        response_done,
        response_need_more,
        response_bad,
        bad_status_line,
        bad_version,
        bad_field,
        head_too_large,
        bad_framing,
        no_body,
        length,
        chunked,
        framing_invalid,
        chunked_done,
        chunked_need_more,
        chunked_bad,
    })
}

#[cfg(test)]
mod tests;
