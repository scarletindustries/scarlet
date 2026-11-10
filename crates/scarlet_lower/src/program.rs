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
use scarlet_check::typed::{BinderId, FnBody, TypedModule};
use scarlet_ir::TypeId;
use scarlet_ir::core_ir::{
    Abi, Const, ConstId, FuncIdx, GlobalSlot, HttpTypes, IoErrors, JsonTypes, LoweredFn,
    ParseReasons, Program, Radix, TypeNames, VariantNames, VariantRef,
};
use scarlet_ir::rty::{PrimIds, RTy, ResolvedPool};
use scarlet_ir::tivec::TiVec;
use scarlet_types::def::TypeDef;
use scarlet_types::known::Known;
use scarlet_types::name::{CtorName, ModuleKey, Name, TypeName, ValueName};
use scarlet_types::ty::Type;

use crate::immediates::Immediates;
use crate::typed_ir::{TempTys, TypedFn};
use crate::{immediates, lower, perceus};

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
    /// Lowering met something a checked program can't hold. It is a bug in
    /// the compiler, never in the program.
    Internal(InternalLinkError),
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
    /// A function's place is past the end of its module's functions.
    NoSuchFunction { module: ModuleKey, index: usize },
    /// A function to translate has no Scarlet body.
    NoBody(ValueName),
    /// Something called or made as a function has a type that isn't a
    /// function's.
    NotAFunction(RTy),
    /// A binder is bound in a function that doesn't bind it.
    UnownedBinder(Name),
    /// A clean module holds a node the checker marked as an error.
    ErrorNode,
    /// A spread fills a field slot its constructor doesn't have.
    NoFieldSlot { ctor: CtorName, slot: u32 },
    /// A field is read from a value whose type isn't a named type.
    FieldOfUnnamed(Type),
    /// A named type has no variant at this index.
    NoVariant { ty: TypeName, variant: u16 },
    /// An `or` on a `Result` whose type has no error argument.
    ResultWithoutError(RTy),
    /// An array pattern on a value whose type isn't an array.
    NotAnArray(RTy),
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
            E::NoSuchFunction { module, index } => {
                write!(f, "module `{module}` has no function {index}")
            }
            E::NoBody(v) => write!(f, "`{}::{}` has no body to translate", v.module, v.name),
            E::NotAFunction(t) => write!(f, "type :{} isn't a function's", t.0),
            E::UnownedBinder(n) => write!(f, "`{n}` is bound in a function that doesn't own it"),
            E::ErrorNode => write!(f, "a clean module holds an error node"),
            E::NoFieldSlot { ctor, slot } => write!(
                f,
                "a spread fills field {slot} of variant {} of `{}`, which has no such field",
                ctor.variant,
                ctor.type_name.name()
            ),
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
        }
    }
}

/// The ids handed out for types and constants, and the pool of types.
pub(crate) struct Ids<'a> {
    pub defs: &'a dyn TypeDefs,
    pool: ResolvedPool,
    type_ids: HashMap<TypeName, TypeId>,
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
        let mut intern = |n: &TypeName| -> TypeId {
            *type_ids.entry(n.clone()).or_insert_with(|| {
                type_names.push(n.clone());
                TypeId(type_names.len() as i32)
            })
        };
        let prims = PrimIds {
            int: intern(&known.int),
            float: intern(&known.float),
            string: intern(&known.string),
            array: intern(&known.array),
        };
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

    pub fn type_id(&mut self, n: &TypeName) -> TypeId {
        if let Some(&id) = self.type_ids.get(n) {
            return id;
        }
        self.type_names.push(n.clone());
        let id = TypeId(self.type_names.len() as i32);
        self.type_ids.insert(n.clone(), id);
        id
    }

    /// The constructor of `t` named `variant`.
    pub fn variant(&mut self, t: &TypeName, variant: &str) -> Option<VariantRef> {
        let idx = self
            .defs
            .type_def(t)?
            .variants()?
            .iter()
            .position(|v| &*v.name == variant)?;
        Some(VariantRef {
            type_id: self.type_id(t),
            variant_idx: u16::try_from(idx).ok()?,
        })
    }

    pub fn ctor_ref(&mut self, t: &TypeName, variant: u16) -> VariantRef {
        VariantRef {
            type_id: self.type_id(t),
            variant_idx: variant,
        }
    }

    /// A finished type in the program's pool. An open variable is a `Bound`:
    /// a type nothing knows the representation of.
    pub fn rty(&mut self, t: &Type) -> RTy {
        if let Some(&r) = self.rtys.get(t) {
            return r;
        }
        let r = match t {
            Type::Var(v) => self.pool.mk_bound(v.id),
            Type::Named { name, args } => {
                let args: Vec<RTy> = args.iter().map(|a| self.rty(a)).collect();
                let id = self.type_id(name);
                self.pool.mk_con(id, &args)
            }
            Type::Fn { params, ret } => {
                let params: Vec<RTy> = params.iter().map(|p| self.rty(p)).collect();
                let ret = self.rty(ret);
                self.pool.mk_fun(&params, ret)
            }
            Type::Tuple(elems) => {
                let elems: Vec<RTy> = elems.iter().map(|e| self.rty(e)).collect();
                self.pool.mk_tuple(&elems)
            }
        };
        self.rtys.insert(t.clone(), r);
        r
    }

    pub fn tuple(&mut self, elems: &[RTy]) -> RTy {
        self.pool.mk_tuple(elems)
    }

    pub fn constant(&mut self, c: Const) -> ConstId {
        if let Some(&id) = self.const_ids.get(&c) {
            return id;
        }
        let id = ConstId(self.consts.len() as u32);
        self.consts.push(c.clone());
        self.const_ids.insert(c, id);
        id
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

/// The ids once lowering stops handing them out.
struct Frozen<'a> {
    defs: &'a dyn TypeDefs,
    pool: ResolvedPool,
    type_names: Vec<TypeName>,
    consts: Vec<Const>,
}

impl Frozen<'_> {
    fn names_of(&self, id: TypeId) -> TypeNames {
        let name = self.type_names.get((id.0 - 1) as usize);
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
        let int = named(ids, &known.int);
        let temps = TempTys {
            int,
            bool: named(ids, &known.bool),
            string: named(ids, &known.string),
            binary: named(ids, &known.binary),
            int_pair: ids.tuple(&[int, int]),
        };
        let mut variant = |t: &TypeName, v: &'static str| {
            ids.variant(t, v)
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
        let nil = named(ids, &known.nil);
        Ok(Prelude { temps, ctors, nil })
    }
}

/// Where one module function's body is: its module, and its place in the
/// module's functions.
struct Site {
    module: usize,
    function: usize,
}

/// Every module function's index and every module-level value's slot, all
/// known before any body is translated.
pub(crate) struct Symbols {
    fns: HashMap<ValueName, FuncIdx>,
    consts: HashMap<ValueName, GlobalSlot>,
    binders: HashMap<(ModuleKey, BinderId), GlobalSlot>,
    /// Each module function, in `FuncIdx` order.
    sites: Vec<Site>,
    globals: u32,
}

impl Symbols {
    fn of(modules: &[&TypedModule]) -> Self {
        let mut s = Symbols {
            fns: HashMap::new(),
            consts: HashMap::new(),
            binders: HashMap::new(),
            sites: Vec::new(),
            globals: 0,
        };
        let global = |n: &mut u32| {
            let g = GlobalSlot(*n as i32);
            *n += 1;
            g
        };
        for (mi, m) in modules.iter().enumerate() {
            for (fi, f) in m.functions.iter().enumerate() {
                if let FnBody::Expr(_) = f.body {
                    let idx = FuncIdx(s.sites.len() as u32);
                    s.fns.insert(value_name(&m.key, &f.name), idx);
                    s.sites.push(Site {
                        module: mi,
                        function: fi,
                    });
                }
            }
            for c in &m.consts {
                s.consts
                    .insert(value_name(&m.key, &c.name), global(&mut s.globals));
            }
            for (i, b) in m.binders.iter().enumerate() {
                if b.module_scope {
                    s.binders
                        .insert((m.key.clone(), BinderId(i as u32)), global(&mut s.globals));
                }
            }
        }
        s
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
    fn fn_count(&self) -> u32 {
        self.sites.len() as u32
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
    pub fn push(&mut self, f: Named) -> FuncIdx {
        self.fns.push(f);
        FuncIdx(self.base + self.fns.len() as u32 - 1)
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
    let symbols = Symbols::of(&modules);
    let mut minted = Minted {
        base: symbols.fn_count(),
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
        .map(|site| crate::translate::function(&mut cx, modules[site.module], site.function))
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
        radix: radix(&mut ids),
        io: io_errors(&mut ids),
        json: json_types(&mut ids),
        http: http_types(&mut ids),
    };

    let frozen = ids.freeze();
    let im = Immediates {
        true_: prelude.ctors.true_,
        false_: prelude.ctors.false_,
        nil: prelude.ctors.nil,
    };
    let finish = |f: &TypedFn| {
        let mut core = lower::lower_fn(prelude.temps, f);
        immediates::rewrite(&mut core, &im);
        perceus::perceus(&frozen.pool, core)
    };
    let fns: TiVec<FuncIdx, LoweredFn> = declared
        .iter()
        .chain(&minted.fns)
        .map(|n| LoweredFn::new(n.module.clone(), n.name.clone(), finish(&n.body)))
        .collect();
    // The entry's top level is named for what it is; each import's, for its
    // module.
    let mut inits: Vec<LoweredFn> = modules
        .iter()
        .zip(&tops)
        .map(|(m, f)| {
            let core = perceus::keep_globals(finish(f));
            let key = m.key.as_str();
            LoweredFn::new(key.to_string(), key.to_string(), core)
        })
        .collect();
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

/// The constructors of the stdlib type `module.name`, by name, when the
/// program reaches that module.
fn stdlib_variants<'b, 'd>(
    b: &'b mut Ids<'d>,
    module: &[&str],
    name: &str,
) -> Option<impl FnMut(&str) -> Option<VariantRef> + use<'b, 'd>> {
    let path: Vec<String> = module.iter().map(|s| s.to_string()).collect();
    let t = TypeName::new(ModuleKey::for_stdlib(&path), name.into());
    b.defs.type_def(&t)?;
    Some(move |v: &str| b.variant(&t, v))
}

fn radix(b: &mut Ids<'_>) -> Option<Radix> {
    let mut v = stdlib_variants(b, &["scarlet", "binary"], "Radix")?;
    Some(Radix {
        dec: v("Dec")?,
        hex: v("Hex")?,
    })
}

fn io_errors(b: &mut Ids<'_>) -> Option<IoErrors> {
    let mut v = stdlib_variants(b, &["scarlet", "io"], "IoError")?;
    Some(IoErrors {
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

fn json_types(b: &mut Ids<'_>) -> Option<JsonTypes> {
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
    Some(JsonTypes {
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

fn http_types(b: &mut Ids<'_>) -> Option<HttpTypes> {
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
    Some(HttpTypes {
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
