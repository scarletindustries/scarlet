//! Binds the VM's [`AbiSlot`]s to AL stdlib constructors at the end of a
//! compile: for each slot whose module this program loaded, intern an
//! [`EnumTemplate`] into the program's frozen area and record it in
//! `program.abi`. This table is the whole of what the runtime knows about
//! AL's stdlib; a slot for a module the program never imported stays unbound,
//! which is fine exactly because the ops that construct it were never emitted
//! (checked below).
//!
//! The `(module, type, constructor)` names here are the ONE compiler-side
//! registry of runtime-constructed stdlib values. A renamed or deleted
//! constructor surfaces as a compile diagnostic naming the slot, not as a
//! silently mis-built value.

use al_vm::abi::AbiSlot;
use al_vm::template::EnumTemplate;
use al_vm::tivec::TiVec;

use crate::module::ModuleKey;
use crate::span::Span;

const ZERO_SPAN: Span = Span {
    start_line: 0,
    start_column: 0,
    end_line: 0,
    end_column: 0,
};

use super::Compiler;

/// `(slot, module path, type name, constructor name)`.
type Binding = (AbiSlot, &'static [&'static str], &'static str, &'static str);

const AL: &[&str] = &["al"];
const IO: &[&str] = &["al", "io"];
const ADDR: &[&str] = &["al", "net", "address"];
const SOCK: &[&str] = &["al", "net", "socket"];
const NET: &[&str] = &["al", "net", "error"];
const HDRS: &[&str] = &["al", "http", "headers"];
const H1: &[&str] = &["al", "http", "h1"];

#[rustfmt::skip]
const BINDINGS: &[Binding] = &[
    (AbiSlot::Unit,               AL,   "Nil",           "Nil"),
    (AbiSlot::OptionSome,         AL,   "Option",        "Some"),
    (AbiSlot::OptionNone,         AL,   "Option",        "None"),
    (AbiSlot::ResultOk,           AL,   "Result",        "Ok"),
    (AbiSlot::ResultErr,          AL,   "Result",        "Err"),
    (AbiSlot::FsEnoent,           IO,   "IoError",       "NotFound"),
    (AbiSlot::FsEacces,           IO,   "IoError",       "PermissionDenied"),
    (AbiSlot::FsEexist,           IO,   "IoError",       "AlreadyExists"),
    (AbiSlot::FsEnotdir,          IO,   "IoError",       "NotADirectory"),
    (AbiSlot::FsEisdir,           IO,   "IoError",       "IsADirectory"),
    (AbiSlot::FsErofs,            IO,   "IoError",       "ReadOnlyFilesystem"),
    (AbiSlot::FsEloop,            IO,   "IoError",       "FilesystemLoop"),
    (AbiSlot::FsEnospc,           IO,   "IoError",       "StorageFull"),
    (AbiSlot::FsEdquot,           IO,   "IoError",       "QuotaExceeded"),
    (AbiSlot::FsEfbig,            IO,   "IoError",       "FileTooLarge"),
    (AbiSlot::FsUnalignedBinary,  IO,   "IoError",       "UnalignedBinary"),
    (AbiSlot::FsErrnoOther,       IO,   "IoError",       "Errno"),
    (AbiSlot::IpV4,               ADDR, "IpAddress",     "V4"),
    (AbiSlot::IpV6,               ADDR, "IpAddress",     "V6"),
    (AbiSlot::SocketAddr,         ADDR, "SocketAddress", "SocketAddress"),
    (AbiSlot::Socket,             SOCK, "Socket",        "Socket"),
    (AbiSlot::ReadData,           SOCK, "Read",          "Data"),
    (AbiSlot::ReadClosed,         SOCK, "Read",          "Closed"),
    (AbiSlot::NetEtimedout,       NET,  "NetError",      "TimedOut"),
    (AbiSlot::NetEconnrefused,    NET,  "NetError",      "ConnectionRefused"),
    (AbiSlot::NetEconnreset,      NET,  "NetError",      "ConnectionReset"),
    (AbiSlot::NetEconnaborted,    NET,  "NetError",      "ConnectionAborted"),
    (AbiSlot::NetEnotconn,        NET,  "NetError",      "NotConnected"),
    (AbiSlot::NetEpipe,           NET,  "NetError",      "BrokenPipe"),
    (AbiSlot::NetEaddrinuse,      NET,  "NetError",      "AddrInUse"),
    (AbiSlot::NetEaddrnotavail,   NET,  "NetError",      "AddrNotAvailable"),
    (AbiSlot::NetEnetdown,        NET,  "NetError",      "NetworkDown"),
    (AbiSlot::NetEnetunreach,     NET,  "NetError",      "NetworkUnreachable"),
    (AbiSlot::NetEhostunreach,    NET,  "NetError",      "HostUnreachable"),
    (AbiSlot::NetEacces,          NET,  "NetError",      "PermissionDenied"),
    (AbiSlot::NetInvalidPort,     NET,  "NetError",      "InvalidPort"),
    (AbiSlot::NetUnalignedBinary, NET,  "NetError",      "UnalignedBinary"),
    (AbiSlot::NetErrnoOther,      NET,  "NetError",      "Errno"),
    (AbiSlot::H1Header,           HDRS, "Header",        "Header"),
    (AbiSlot::H1Http10,           H1,   "Version",       "Http10"),
    (AbiSlot::H1Http11,           H1,   "Version",       "Http11"),
    (AbiSlot::H1HeadFlags,        H1,   "HeadFlags",     "HeadFlags"),
    (AbiSlot::H1ParsedDone,       H1,   "Parsed",        "Done"),
    (AbiSlot::H1ParsedNeedMore,   H1,   "Parsed",        "NeedMore"),
    (AbiSlot::H1ParsedBad,        H1,   "Parsed",        "Bad"),
    (AbiSlot::H1FramingNoBody,    H1,   "Framing",       "NoBody"),
    (AbiSlot::H1FramingLength,    H1,   "Framing",       "Length"),
    (AbiSlot::H1FramingChunked,   H1,   "Framing",       "Chunked"),
    (AbiSlot::H1FramingInvalid,   H1,   "Framing",       "Invalid"),
    (AbiSlot::H1ChunkedDone,      H1,   "ChunkBody",     "ChunkedDone"),
    (AbiSlot::H1ChunkedNeedMore,  H1,   "ChunkBody",     "ChunkedNeedMore"),
    (AbiSlot::H1ChunkedBad,       H1,   "ChunkBody",     "ChunkedBad"),
];

impl Compiler {
    /// Fill `program.templates`/`program.abi` from the modules this compile
    /// loaded, then require a binding for every slot an emitted op constructs.
    /// Rebuilt from scratch each emit so a session's watermark rewind can
    /// never leave a stale `TemplateIdx` behind.
    pub(super) fn bind_abi(&mut self) {
        self.program.templates = TiVec::new();
        self.program.abi = Default::default();

        for &(slot, module, type_name, ctor) in BINDINGS {
            let key = ModuleKey::of(&module.iter().map(|s| s.to_string()).collect());
            // Only modules this program loaded: an unimported module's ops
            // were never emitted, so its slots may stay unbound.
            let Some(iface) = self.module_table.get(&key) else {
                continue;
            };
            // From here on the module is present, so a missing type or
            // constructor is stdlib drift: the AL source no longer matches
            // this registry. Surface it, naming the slot.
            let Some(ti) = iface.types.get(type_name).copied() else {
                self.abi_drift(slot, type_name, ctor, "type not found");
                continue;
            };
            let Some(variants) = ti.variants() else {
                self.abi_drift(slot, type_name, ctor, "type has no constructors");
                continue;
            };
            let found = self.engine.variants[variants.range()]
                .iter()
                .enumerate()
                .find(|(_, v)| self.engine.str(v.name) == ctor)
                .map(|(idx, v)| (idx, v.fields));
            let Some((variant_idx, fields)) = found else {
                self.abi_drift(slot, type_name, ctor, "constructor not found");
                continue;
            };
            let labels: Vec<&str> = self.engine.variant_fields[fields.range()]
                .iter()
                .map(|f| self.engine.str(f.label))
                .collect();
            if labels.len() != slot.arity() {
                self.abi_drift(slot, type_name, ctor, "constructor arity changed");
                continue;
            }
            let tpl = EnumTemplate::build(
                &mut self.frozen,
                ti.id,
                variant_idx as u16,
                type_name,
                ctor,
                &labels,
            );
            let idx = self.program.templates.push(tpl);
            self.program.abi.bind(slot, idx);
        }

        let missing = self
            .program
            .abi
            .unbound_for(self.program.code.iter().map(|i| i.op));
        if !missing.is_empty() {
            let names: Vec<&str> = missing.iter().map(|s| s.name()).collect();
            self.error(
                format!(
                    "internal: emitted ops construct unbound ABI slots: {}",
                    names.join(", ")
                ),
                ZERO_SPAN,
            );
        }
    }

    fn abi_drift(&mut self, slot: AbiSlot, type_name: &str, ctor: &str, what: &str) {
        self.error(
            format!(
                "internal: ABI slot {} binds {type_name}.{ctor}, but {what} — \
                 the stdlib no longer matches the compiler's ABI registry",
                slot.name()
            ),
            ZERO_SPAN,
        );
    }
}
