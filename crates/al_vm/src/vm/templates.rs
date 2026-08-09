//! The VM's resolved view of [`Program::abi`](crate::bytecode::Program::abi):
//! one clone of each bound [`EnumTemplate`], indexed by slot. Reaching an
//! unbound slot means the front end emitted an op whose outcomes it did not
//! bind, and gives [`VmError::Internal`].

use std::borrow::Cow;

use crate::abi::{AbiSlot, TemplateIdx};
use crate::bytecode::{Arena, Value};
use crate::frozen::FrozenBuilder;
use crate::template::{AbiTable, EnumTemplate};
use crate::tivec::TiVec;

use super::{VmError, VmResult};

pub(super) struct Templates {
    slots: [Option<EnumTemplate>; AbiSlot::COUNT],
    /// Resolved whole so `vm::http` stays infallible past the op boundary.
    h1: Option<H1>,
}

/// Everything `vm::http` constructs. A nullary outcome is the pre-built frozen
/// value itself, so pushing one is a word copy.
pub(super) struct H1 {
    pub(super) some: EnumTemplate,
    pub(super) none: Value,
    pub(super) header: EnumTemplate,
    pub(super) version_http10: Value,
    pub(super) version_http11: Value,
    pub(super) head_flags: EnumTemplate,
    /// The all-false `HeadFlags`, pre-built so the common parse copies a word
    /// instead of allocating.
    pub(super) head_flags_none: Value,
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

impl Templates {
    /// Clone the bound templates out of the program's tables. `fb` prebuilds
    /// `head_flags_none`.
    pub(super) fn resolve(
        abi: &AbiTable,
        templates: &TiVec<TemplateIdx, EnumTemplate>,
        fb: &mut FrozenBuilder,
    ) -> Self {
        let get =
            |slot: AbiSlot| -> Option<EnumTemplate> { abi.get(slot).map(|i| templates[i].clone()) };
        let nullary = |slot: AbiSlot| -> Option<Value> { get(slot)?.nullary().cloned() };

        let h1 = (|| {
            let head_flags = get(AbiSlot::H1HeadFlags)?;
            let f = Value::bool(false);
            // Immediates, so the frozen record owns no mortal children.
            let head_flags_none = head_flags.instantiate(fb, &[f.clone(), f.clone(), f]);
            Some(H1 {
                some: get(AbiSlot::OptionSome)?,
                none: nullary(AbiSlot::OptionNone)?,
                header: get(AbiSlot::H1Header)?,
                version_http10: nullary(AbiSlot::H1Http10)?,
                version_http11: nullary(AbiSlot::H1Http11)?,
                head_flags,
                head_flags_none,
                parsed_done: get(AbiSlot::H1ParsedDone)?,
                parsed_need_more: nullary(AbiSlot::H1ParsedNeedMore)?,
                parsed_bad: get(AbiSlot::H1ParsedBad)?,
                framing_no_body: nullary(AbiSlot::H1FramingNoBody)?,
                framing_length: get(AbiSlot::H1FramingLength)?,
                framing_chunked: nullary(AbiSlot::H1FramingChunked)?,
                framing_invalid: get(AbiSlot::H1FramingInvalid)?,
                chunked_done: get(AbiSlot::H1ChunkedDone)?,
                chunked_need_more: nullary(AbiSlot::H1ChunkedNeedMore)?,
                chunked_bad: get(AbiSlot::H1ChunkedBad)?,
            })
        })();

        Templates {
            slots: std::array::from_fn(|i| get(AbiSlot::ALL[i])),
            h1,
        }
    }

    #[inline]
    pub(super) fn get(&self, slot: AbiSlot) -> VmResult<&EnumTemplate> {
        self.slots[slot.index()]
            .as_ref()
            .ok_or_else(|| unbound(slot))
    }

    /// The pre-built value of a nullary slot.
    #[inline]
    pub(super) fn nullary(&self, slot: AbiSlot) -> VmResult<Value> {
        match self.get(slot)?.nullary() {
            Some(v) => Ok(v.clone()),
            None => Err(VmError::Internal(Cow::Owned(format!(
                "ABI slot {} bound to a non-nullary constructor",
                slot.name()
            )))),
        }
    }

    #[cfg(test)]
    pub(super) fn h1_owned(self) -> Option<H1> {
        self.h1
    }

    #[inline]
    pub(super) fn h1(&self) -> VmResult<&H1> {
        self.h1
            .as_ref()
            .ok_or_else(|| unbound(AbiSlot::H1ParsedDone))
    }

    /// `IpAddress` (`V4`/`V6`) from a `std::net::IpAddr`, as its text form.
    pub(super) fn ip_address<A: Arena + ?Sized>(
        &self,
        a: &mut A,
        ip: std::net::IpAddr,
    ) -> VmResult<Value> {
        let slot = if ip.is_ipv6() {
            AbiSlot::IpV6
        } else {
            AbiSlot::IpV4
        };
        let text = Value::str_in(a, &ip.to_string());
        Ok(self.get(slot)?.instantiate(a, &[text]))
    }

    pub(super) fn socket_address<A: Arena + ?Sized>(
        &self,
        a: &mut A,
        addr: std::net::SocketAddr,
    ) -> VmResult<Value> {
        let ip = self.ip_address(a, addr.ip())?;
        let t = self.get(AbiSlot::SocketAddr)?;
        Ok(t.instantiate(a, &[ip, Value::small_int(addr.port() as i64)]))
    }

    /// A `Socket` record wrapping `conn` with its peer address.
    pub(super) fn make_socket<A: Arena + ?Sized>(
        &self,
        a: &mut A,
        conn: Value,
        peer: std::net::SocketAddr,
    ) -> VmResult<Value> {
        let peer = self.socket_address(a, peer)?;
        let t = self.get(AbiSlot::Socket)?;
        Ok(t.instantiate(a, &[conn, peer]))
    }
}

fn unbound(slot: AbiSlot) -> VmError {
    VmError::Internal(Cow::Owned(format!(
        "ABI slot {} is unbound but an emitted op constructs it",
        slot.name()
    )))
}
