//! Lowering a whole program: every module's typed tree, in dependency order,
//! turned into one [`Program`]. This is where the ids the VM uses are handed
//! out, once each: a [`TypeId`] for every nominal type, a [`FuncIdx`] for
//! every function, a [`GlobalSlot`] for every module-level value, and a
//! [`ConstId`] for every constant.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::rc::Rc;

use scarlet_check::CleanModule;
use scarlet_check::typed::BinderId;
use scarlet_ir::TypeId;
use scarlet_ir::core_ir::{
    Abi, Const, ConstId, FuncIdx, GlobalSlot, HttpTypes, IoErrors, JsonTypes, LoweredFn,
    ParseReasons, Program, Radix, TypeNames, VariantNames, VariantRef,
};
use scarlet_ir::rty::{PrimIds, RTy, ResolvedPool};
use scarlet_ir::tivec::TiVec;
use scarlet_types::def::TypeDef;
use scarlet_types::known::Known;
use scarlet_types::name::{ModuleKey, TypeName, ValueName};
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
    /// A prelude type the compiler relies on is missing or has the wrong
    /// shape.
    Prelude(String),
}

/// The ids handed out so far, and the bodies made so far.
pub(crate) struct Builder<'a> {
    pub defs: &'a dyn TypeDefs,
    pub known: Known,
    pub pool: ResolvedPool,
    type_ids: HashMap<TypeName, TypeId>,
    type_names: Vec<TypeName>,
    rtys: HashMap<Type, RTy>,
    consts: Vec<Const>,
    const_ids: HashMap<Const, ConstId>,
    /// Every function body, by `FuncIdx`. A slot is reserved before its body
    /// is made, so a function can call itself and those after it.
    pub fns: Vec<Option<(String, String, TypedFn)>>,
    pub fn_of: HashMap<ValueName, FuncIdx>,
    pub const_slot: HashMap<ValueName, GlobalSlot>,
    pub binder_slot: HashMap<(ModuleKey, BinderId), GlobalSlot>,
    globals: u32,
    pub temps: TempTys,
    pub ctors: PreludeCtors,
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

impl<'a> Builder<'a> {
    fn new(defs: &'a dyn TypeDefs) -> Result<Self, LinkError> {
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
        let mut b = Builder {
            defs,
            known: known.clone(),
            pool: ResolvedPool::new(prims),
            type_ids,
            type_names,
            rtys: HashMap::new(),
            consts: Vec::new(),
            const_ids: HashMap::new(),
            fns: Vec::new(),
            fn_of: HashMap::new(),
            const_slot: HashMap::new(),
            binder_slot: HashMap::new(),
            globals: 0,
            temps: TempTys {
                int: RTy(0),
                bool: RTy(0),
                string: RTy(0),
                binary: RTy(0),
                int_pair: RTy(0),
            },
            ctors: PreludeCtors {
                some: VariantRef {
                    type_id: TypeId::NONE,
                    variant_idx: 0,
                },
                none: VariantRef {
                    type_id: TypeId::NONE,
                    variant_idx: 0,
                },
                ok: VariantRef {
                    type_id: TypeId::NONE,
                    variant_idx: 0,
                },
                err: VariantRef {
                    type_id: TypeId::NONE,
                    variant_idx: 0,
                },
                true_: VariantRef {
                    type_id: TypeId::NONE,
                    variant_idx: 0,
                },
                false_: VariantRef {
                    type_id: TypeId::NONE,
                    variant_idx: 0,
                },
                nil: VariantRef {
                    type_id: TypeId::NONE,
                    variant_idx: 0,
                },
            },
        };
        let int = b.rty(&Type::named(known.int.clone(), vec![]));
        let boolean = b.rty(&Type::named(known.bool.clone(), vec![]));
        let string = b.rty(&Type::named(known.string.clone(), vec![]));
        let binary = b.rty(&Type::named(known.binary.clone(), vec![]));
        let int_pair = b.pool.mk_tuple(&[int, int]);
        b.temps = TempTys {
            int,
            bool: boolean,
            string,
            binary,
            int_pair,
        };
        let variant = |b: &mut Builder<'_>, t: &TypeName, v: &str| {
            b.variant(t, v).ok_or_else(|| {
                LinkError::Prelude(format!("prelude: constructor '{v}' is required"))
            })
        };
        b.ctors = PreludeCtors {
            some: variant(&mut b, &known.option, "Some")?,
            none: variant(&mut b, &known.option, "None")?,
            ok: variant(&mut b, &known.result, "Ok")?,
            err: variant(&mut b, &known.result, "Err")?,
            true_: variant(&mut b, &known.bool, "True")?,
            false_: variant(&mut b, &known.bool, "False")?,
            nil: variant(&mut b, &known.nil, "Nil")?,
        };
        Ok(b)
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

    pub fn constant(&mut self, c: Const) -> ConstId {
        if let Some(&id) = self.const_ids.get(&c) {
            return id;
        }
        let id = ConstId(self.consts.len() as u32);
        self.consts.push(c.clone());
        self.const_ids.insert(c, id);
        id
    }

    pub fn reserve_fn(&mut self) -> FuncIdx {
        self.fns.push(None);
        FuncIdx(self.fns.len() as u32 - 1)
    }

    pub fn fill_fn(&mut self, idx: FuncIdx, module: &ModuleKey, name: &str, f: TypedFn) {
        if let Some(slot) = self.fns.get_mut(idx.0 as usize) {
            *slot = Some((module.as_str().to_string(), name.to_string(), f));
        }
    }

    pub fn global(&mut self) -> GlobalSlot {
        let g = GlobalSlot(self.globals as i32);
        self.globals += 1;
        g
    }

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

/// Build the program whose modules are `modules`, imports first and the
/// entry last. `main` is the entry's function to start at, when it runs one.
pub fn build_program(
    modules: &[&CleanModule],
    defs: &dyn TypeDefs,
    main: Option<&ValueName>,
) -> Result<Program, LinkError> {
    let mut b = Builder::new(defs)?;
    let mut tops: Vec<(String, TypedFn)> = Vec::with_capacity(modules.len());
    for m in modules {
        crate::translate::register(&mut b, m.module());
    }
    for m in modules {
        let top = crate::translate::module(&mut b, m.module());
        tops.push((m.module().key.as_str().to_string(), top));
    }

    let pool = Rc::new(b.pool.clone());
    let im = Immediates {
        true_: b.ctors.true_,
        false_: b.ctors.false_,
        nil: b.ctors.nil,
    };
    let finish = |f: &TypedFn, toplevel: bool| {
        let mut core = lower::lower_fn(b.temps, f);
        immediates::rewrite(&mut core, &im);
        let core = perceus::perceus(&pool, core);
        if toplevel {
            perceus::keep_globals(core)
        } else {
            core
        }
    };
    let mut fns = TiVec::new();
    for slot in &b.fns {
        let Some((module, name, f)) = slot else {
            continue;
        };
        fns.push(LoweredFn::new(
            module.clone(),
            name.clone(),
            finish(f, false),
            Rc::clone(&pool),
        ));
    }
    // The entry's top level is named for what it is; each import's, for its
    // module.
    let last = tops.len().saturating_sub(1);
    let mut inits: Vec<LoweredFn> = tops
        .iter()
        .enumerate()
        .map(|(i, (module, f))| {
            let name = if i == last {
                "__main__".to_string()
            } else {
                module.clone()
            };
            LoweredFn::new(module.clone(), name, finish(f, true), Rc::clone(&pool))
        })
        .collect();
    let toplevel = match inits.pop() {
        Some(t) => t,
        None => {
            let empty = TypedFn {
                params: Vec::new(),
                ret: b.temps.int,
                body: crate::typed_ir::TypedExpr::Nil { ty: b.temps.int },
                binds: 0,
            };
            LoweredFn::new(
                "main".into(),
                "main".into(),
                finish(&empty, true),
                Rc::clone(&pool),
            )
        }
    };
    let main = main.and_then(|m| b.fn_of.get(m).copied());
    let abi = Abi {
        some: b.ctors.some,
        none: b.ctors.none,
        ok: b.ctors.ok,
        err: b.ctors.err,
        radix: radix(&mut b),
        io: io_errors(&mut b),
        json: json_types(&mut b),
        http: http_types(&mut b),
    };
    let mut ids: BTreeSet<TypeId> = abi.variants().iter().map(|v| v.type_id).collect();
    for f in (&fns).into_iter().chain(&inits).chain([&toplevel]) {
        f.core.body.for_each_variant(|v| {
            ids.insert(v.type_id);
        });
    }
    let types: BTreeMap<TypeId, TypeNames> =
        ids.into_iter().map(|id| (id, b.names_of(id))).collect();
    Ok(Program {
        fns,
        consts: b.consts.clone(),
        inits,
        toplevel,
        main,
        globals: b.globals,
        types,
        abi,
    })
}

/// The constructors of the stdlib type `module.name`, by name, when the
/// program reaches that module.
fn stdlib_variants<'b, 'd>(
    b: &'b mut Builder<'d>,
    module: &[&str],
    name: &str,
) -> Option<impl FnMut(&str) -> Option<VariantRef> + use<'b, 'd>> {
    let path: Vec<String> = module.iter().map(|s| s.to_string()).collect();
    let t = TypeName::new(ModuleKey::for_stdlib(&path), name.into());
    b.defs.type_def(&t)?;
    Some(move |v: &str| b.variant(&t, v))
}

fn radix(b: &mut Builder<'_>) -> Option<Radix> {
    let mut v = stdlib_variants(b, &["scarlet", "binary"], "Radix")?;
    Some(Radix {
        dec: v("Dec")?,
        hex: v("Hex")?,
    })
}

fn io_errors(b: &mut Builder<'_>) -> Option<IoErrors> {
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

fn json_types(b: &mut Builder<'_>) -> Option<JsonTypes> {
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

fn http_types(b: &mut Builder<'_>) -> Option<HttpTypes> {
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
