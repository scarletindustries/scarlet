//! The VM's resolved view of [`Program::abi`](crate::bytecode::Program::abi):
//! one [`EnumTemplate`] per bound slot, cloned out at VM init.
//!
//! Unbound slots stay `None`. Reaching one at runtime means the front end
//! emitted an op it did not bind, so it surfaces as [`VmError::Internal`].

use std::borrow::Cow;

use crate::abi::AbiSlot;
use crate::bytecode::Program;
use crate::template::EnumTemplate;

use super::{VmError, VmResult};

/// Every bound constructor, indexed by slot.
pub(super) struct Templates {
    slots: Vec<Option<EnumTemplate>>,
    /// The HTTP/1 group, resolved whole so the scanners stay infallible.
    pub(super) h1: Option<H1>,
    /// The all-false `HeadFlags`, pre-built in the frozen area so the common
    /// parse copies a word instead of allocating.
    pub(super) head_flags_none: Option<crate::bytecode::Value>,
}

/// The constructors the `vm::http` scanners build.
pub(super) struct H1 {
    pub(super) header: EnumTemplate,
    pub(super) http10: EnumTemplate,
    pub(super) http11: EnumTemplate,
    pub(super) head_flags: EnumTemplate,
    pub(super) parsed_done: EnumTemplate,
    pub(super) parsed_need_more: EnumTemplate,
    pub(super) parsed_bad: EnumTemplate,
    pub(super) framing_no_body: EnumTemplate,
    pub(super) framing_length: EnumTemplate,
    pub(super) framing_chunked: EnumTemplate,
    pub(super) framing_invalid: EnumTemplate,
    pub(super) chunked_done: EnumTemplate,
    pub(super) chunked_need_more: EnumTemplate,
    pub(super) chunked_bad: EnumTemplate,
}

impl Templates {
    pub(super) fn resolve(program: &Program, fb: &mut crate::frozen::FrozenBuilder) -> Self {
        let get = |slot: AbiSlot| -> Option<EnumTemplate> {
            program.abi.get(slot).map(|i| program.templates[i].clone())
        };
        let slots: Vec<Option<EnumTemplate>> = AbiSlot::ALL.iter().map(|&s| get(s)).collect();

        let h1 = (|| {
            Some(H1 {
                header: get(AbiSlot::H1Header)?,
                http10: get(AbiSlot::H1Http10)?,
                http11: get(AbiSlot::H1Http11)?,
                head_flags: get(AbiSlot::H1HeadFlags)?,
                parsed_done: get(AbiSlot::H1ParsedDone)?,
                parsed_need_more: get(AbiSlot::H1ParsedNeedMore)?,
                parsed_bad: get(AbiSlot::H1ParsedBad)?,
                framing_no_body: get(AbiSlot::H1FramingNoBody)?,
                framing_length: get(AbiSlot::H1FramingLength)?,
                framing_chunked: get(AbiSlot::H1FramingChunked)?,
                framing_invalid: get(AbiSlot::H1FramingInvalid)?,
                chunked_done: get(AbiSlot::H1ChunkedDone)?,
                chunked_need_more: get(AbiSlot::H1ChunkedNeedMore)?,
                chunked_bad: get(AbiSlot::H1ChunkedBad)?,
            })
        })();

        let head_flags_none = h1.as_ref().map(|h| {
            // Immediates, so the frozen record owns no mortal children.
            h.head_flags.instantiate(
                fb,
                &[
                    crate::bytecode::Value::bool(false),
                    crate::bytecode::Value::bool(false),
                    crate::bytecode::Value::bool(false),
                ],
            )
        });

        Templates {
            slots,
            h1,
            head_flags_none,
        }
    }

    #[inline]
    pub(super) fn get(&self, slot: AbiSlot) -> VmResult<&EnumTemplate> {
        match &self.slots[slot.index()] {
            Some(t) => Ok(t),
            None => Err(unbound(slot)),
        }
    }

    #[inline]
    pub(super) fn h1(&self) -> VmResult<&H1> {
        match &self.h1 {
            Some(h) => Ok(h),
            None => Err(unbound(AbiSlot::H1ParsedDone)),
        }
    }
}

fn unbound(slot: AbiSlot) -> VmError {
    VmError::Internal(Cow::Owned(format!(
        "ABI slot {} is unbound: the front end emitted an op that constructs it",
        slot.name()
    )))
}
