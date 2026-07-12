//! Precomputed templates for prelude and stdlib enum values — how the VM
//! builds `Some(x)`, `Ok(x)`, `NetError` variants, HTTP `Parsed` values,
//! and the like without ever re-interning a name.
//!
//! An [`EnumTemplate`] is the constant part of one variant, computed once:
//! frozen `Str` values for the enum and variant names, a frozen labels
//! tuple, and the compile-time name-prefix hash. Instantiating it folds
//! the payload into that hash and allocates exactly one enum cell — the
//! names are stored as single reference words into the frozen area
//! (program-lifetime stable, shared by every process; see
//! [`al_core::frozen`]).
//!
//! [`PreludeTemplates`] is the fixed set every VM carries, built at init
//! through the program's frozen builder. Nullary values (`Nil`, `None`,
//! the HTTP `NeedMore`/`NoBody`/`Chunked` markers) are pre-built whole in
//! the frozen area: pushing one is a word copy that allocates nothing.
//! Stdlib variants outside this fixed set (error values) go through
//! `VM::stdlib_template`, which builds and memoizes an [`EnumTemplate`]
//! on first use against the same frozen area.

use al_core::bytecode::{Arena, Value};
use al_core::frozen::FrozenBuilder;
use al_core::static_ir::VariantTemplate;

use crate::stdlib;

/// One prelude/stdlib enum variant, precomputed for cheap construction: the
/// frozen name and label values (program-lifetime stable, shared by every
/// instance) and the compile-time name-prefix hash, completed per instance
/// by folding the payload in. Plain words — copying a template costs
/// nothing and borrows nothing.
#[derive(Clone)]
pub(super) struct EnumTemplate {
    type_id: al_core::TypeId,
    variant_idx: u16,
    /// Frozen `Str` value of the enum type name.
    enum_name: Value,
    /// Frozen `Str` value of the variant name.
    variant_name: Value,
    /// Frozen `Tuple`-of-`Str` value of the field labels.
    labels: Value,
}

impl EnumTemplate {
    /// Build an instance carrying `payload` in `a`. Names and labels are
    /// frozen references, never copied.
    pub(super) fn instantiate<A: Arena + ?Sized>(&self, a: &mut A, payload: &[Value]) -> Value {
        // Lazy: see `EnumRef::hash` — 0 means "computed on first use".
        let hash = 0u64;
        Value::enum_in(
            a,
            self.type_id,
            self.variant_idx,
            hash,
            self.enum_name.clone(),
            self.variant_name.clone(),
            self.labels.clone(),
            payload,
        )
    }
}

/// Precomputed prelude enum templates. The constant-shape parts (frozen
/// names, labels, name-prefix hashes) are built once at VM init into the
/// program's frozen area; hot paths instantiate against them instead of
/// rebuilding names. Variant-only values (Nil, None, NeedMore, NoBody,
/// Chunked) are pre-built IN the frozen area, so pushing one is a single
/// word copy and allocates nothing.
pub(super) struct PreludeTemplates {
    pub(super) nil: Value,
    pub(super) none: Value,
    pub(super) some: EnumTemplate,
    pub(super) ok: EnumTemplate,
    pub(super) err: EnumTemplate,
    pub(super) ip_v4: EnumTemplate,
    pub(super) ip_v6: EnumTemplate,
    pub(super) socket_address: EnumTemplate,
    pub(super) socket: EnumTemplate,
    pub(super) read_data: EnumTemplate,
    pub(super) read_closed: Value,
    // HTTP protocol templates (al/http/h1, al/http/headers), used by the
    // native scanners in `vm::http`.
    pub(super) header: EnumTemplate,
    pub(super) version_http10: Value,
    pub(super) version_http11: Value,
    pub(super) parsed_done: EnumTemplate,
    pub(super) parsed_need_more: Value,
    pub(super) parsed_bad: EnumTemplate,
    pub(super) framing_no_body: Value,
    pub(super) framing_length: EnumTemplate,
    pub(super) framing_chunked: Value,
    pub(super) framing_invalid: EnumTemplate,
    pub(super) chunked_done: EnumTemplate,
    pub(super) chunked_need_more: Value,
    pub(super) chunked_bad: EnumTemplate,
}

/// Convert a build.rs-emitted `stdlib::*` template into the runtime form:
/// intern its names and labels in the frozen area and precompute the
/// name-prefix hash.
pub(super) fn enum_template(fb: &mut FrozenBuilder, t: &VariantTemplate) -> EnumTemplate {
    let labels = t.labels.iter().map(|l| fb.str(l)).collect();
    EnumTemplate {
        type_id: t.type_id,
        variant_idx: t.variant_idx,
        enum_name: fb.str(t.type_name).into_value(),
        variant_name: fb.str(t.variant_name).into_value(),
        labels: fb.tuple(labels).into_value(),
    }
}

/// A nullary stdlib enum value pre-built in the frozen area: pushing it is a
/// word copy, shared by every process for the program lifetime.
fn frozen_enum_value(fb: &mut FrozenBuilder, t: &VariantTemplate) -> Value {
    debug_assert!(
        t.labels.is_empty(),
        "frozen_enum_value requires a nullary variant: {}::{}",
        t.type_name,
        t.variant_name
    );
    let tpl = enum_template(fb, t);
    tpl.instantiate(fb, &[])
}

impl PreludeTemplates {
    pub(super) fn new(fb: &mut FrozenBuilder) -> Self {
        PreludeTemplates {
            nil: frozen_enum_value(fb, &stdlib::prelude::NIL),
            none: frozen_enum_value(fb, &stdlib::prelude::NONE),
            some: enum_template(fb, &stdlib::prelude::SOME),
            ok: enum_template(fb, &stdlib::prelude::OK),
            err: enum_template(fb, &stdlib::prelude::ERR),
            ip_v4: enum_template(fb, &stdlib::net::address::V4),
            ip_v6: enum_template(fb, &stdlib::net::address::V6),
            socket_address: enum_template(fb, &stdlib::net::address::SOCKET_ADDRESS),
            socket: enum_template(fb, &stdlib::net::socket::SOCKET),
            read_data: enum_template(fb, &stdlib::net::socket::DATA),
            read_closed: frozen_enum_value(fb, &stdlib::net::socket::CLOSED),
            header: enum_template(fb, &stdlib::http::headers::HEADER),
            version_http10: frozen_enum_value(fb, &stdlib::http::h1::HTTP10),
            version_http11: frozen_enum_value(fb, &stdlib::http::h1::HTTP11),
            parsed_done: enum_template(fb, &stdlib::http::h1::DONE),
            parsed_need_more: frozen_enum_value(fb, &stdlib::http::h1::NEED_MORE),
            parsed_bad: enum_template(fb, &stdlib::http::h1::BAD),
            framing_no_body: frozen_enum_value(fb, &stdlib::http::h1::NO_BODY),
            framing_length: enum_template(fb, &stdlib::http::h1::LENGTH),
            framing_chunked: frozen_enum_value(fb, &stdlib::http::h1::CHUNKED),
            framing_invalid: enum_template(fb, &stdlib::http::h1::INVALID),
            chunked_done: enum_template(fb, &stdlib::http::h1::CHUNKED_DONE),
            chunked_need_more: frozen_enum_value(fb, &stdlib::http::h1::CHUNKED_NEED_MORE),
            chunked_bad: enum_template(fb, &stdlib::http::h1::CHUNKED_BAD),
        }
    }

    /// Build an `al/net/address.IpAddress` (`V4`/`V6`) from a
    /// `std::net::IpAddr`.
    pub(super) fn ip_address<A: Arena + ?Sized>(&self, a: &mut A, ip: std::net::IpAddr) -> Value {
        let tpl = if ip.is_ipv6() {
            self.ip_v6.clone()
        } else {
            self.ip_v4.clone()
        };
        let text = Value::str_in(a, &ip.to_string());
        tpl.instantiate(a, &[text])
    }

    /// Build an `al/net/address.SocketAddress` from a `std::net::SocketAddr`.
    pub(super) fn socket_address<A: Arena + ?Sized>(
        &self,
        a: &mut A,
        addr: std::net::SocketAddr,
    ) -> Value {
        let ip = self.ip_address(a, addr.ip());
        self.socket_address
            .instantiate(a, &[ip, Value::small_int(addr.port() as i64)])
    }

    /// Build an `al/net.Socket` record wrapping `conn` with its peer address.
    pub(super) fn make_socket<A: Arena + ?Sized>(
        &self,
        a: &mut A,
        conn: Value,
        peer: std::net::SocketAddr,
    ) -> Value {
        let peer = self.socket_address(a, peer);
        self.socket.instantiate(a, &[conn, peer])
    }
}
