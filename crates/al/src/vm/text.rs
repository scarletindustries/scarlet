//! The string and binary builtins: `strings.*`, `binary.*`, the binary
//! pattern-match segment ops, and the HTTP/1.1 protocol scanners — the
//! typed stack layer over [`super::binary`] (bit math) and
//! [`super::http`] (byte scanning).
//!
//! Layering: this file owns the *opcode contract* — operand order, typed
//! pops, and result shape (`Ok`/`Err`/`Option` wrappers, zero-copy views)
//! — while the byte work itself lives in the VM-free helpers it calls.
//! Nothing here parks a process and nothing
//! touches `ip`: every op is a pure stack transformation, one method per
//! opcode, called directly from the dispatch loop's exhaustive match.
//!
//! The byte-oriented ASCII builtins and the HTTP protocol ops are marked
//! cold + never-inline so their bodies (and the int<->ASCII helpers they
//! call) stay out of the central dispatch loop's codegen and leave the hot
//! integer arms undisturbed.
//!
//! Binary results prefer views over copies: slice/take/rest build
//! sub-views sharing the operand's backing (`Value::binary_view_in`),
//! so a parse loop walking a buffer allocates only constant-size boxes.

use al_core::bytecode::Value;
use al_core::bytecode::bits::copy_bits;
use smallvec::SmallVec;

use super::{VM, VmError, VmResult, bin_ref, binary, http, str_ref};

impl VM {
    // --- String builtins -------------------------------------------------------

    pub(super) fn str_split(&mut self) -> VmResult<()> {
        let delim_v = self.pop_str("strings.split")?;
        let s_v = self.pop_str("strings.split")?;
        let (delim, s) = (str_ref(&delim_v), str_ref(&s_v));
        let mut parts: Vec<Value> = Vec::new();
        if delim.is_empty() {
            let mut buf = [0u8; 4];
            for c in s.chars() {
                let part = Value::str_in(&mut self.heap, c.encode_utf8(&mut buf));
                parts.push(part);
            }
        } else {
            for part in s.split(delim) {
                let part = Value::str_in(&mut self.heap, part);
                parts.push(part);
            }
        }
        let v = Value::array_in(&mut self.heap, &parts);
        self.stack.push(v);
        Ok(())
    }

    pub(super) fn str_len(&mut self) -> VmResult<()> {
        let s_v = self.pop_str("strings.length")?;
        let n = str_ref(&s_v).chars().count() as i64;
        self.push_int(n);
        Ok(())
    }

    pub(super) fn str_contains(&mut self) -> VmResult<()> {
        let n_v = self.pop_str("strings.contains")?;
        let h_v = self.pop_str("strings.contains")?;
        self.stack
            .push(Value::bool(str_ref(&h_v).contains(str_ref(&n_v))));
        Ok(())
    }

    pub(super) fn str_trim(&mut self) -> VmResult<()> {
        let s_v = self.pop_str("strings.trim")?;
        let trimmed = str_ref(&s_v).trim();
        let v = Value::str_in(&mut self.heap, trimmed);
        self.stack.push(v);
        Ok(())
    }

    pub(super) fn int_to_string(&mut self) -> VmResult<()> {
        // i64::MIN renders in 20 bytes; that is the ceiling.
        let n = self.pop_int("int.to_string")?;
        let digits = int_to_ascii(n, Radix::Dec);
        let v = Value::str_in(&mut self.heap, digits.as_str());
        self.stack.push(v);
        Ok(())
    }

    // --- Binary builtins -------------------------------------------------------

    pub(super) fn bin_from_string(&mut self) -> VmResult<()> {
        // The bytes are copied off-heap; only the box is arena.
        let s_v = self.pop_str("binary.from_string")?;
        let v = Value::binary_from_slice_in(&mut self.heap, str_ref(&s_v).as_bytes());
        self.stack.push(v);
        Ok(())
    }

    pub(super) fn bin_to_string(&mut self) -> VmResult<()> {
        // Ok(Str) of the binary's byte length, or Err(Nil).
        let bin_v = self.pop_binary("binary.to_string")?;
        let bin = bin_ref(&bin_v);
        let v = if !bin.bit_len().is_multiple_of(8) {
            self.make_err_nil()
        } else {
            let bytes = bin.full_bytes();
            match std::str::from_utf8(&bytes) {
                Ok(s) => {
                    let s = Value::str_in(&mut self.heap, s);
                    self.make_ok(s)
                }
                Err(_) => self.make_err_nil(),
            }
        };
        self.stack.push(v);
        Ok(())
    }

    pub(super) fn bin_bit_size(&mut self) -> VmResult<()> {
        let bin_v = self.pop_binary("binary.bit_size")?;
        let n = bin_ref(&bin_v).bit_len() as i64;
        self.push_int(n);
        Ok(())
    }

    pub(super) fn bin_byte_size(&mut self) -> VmResult<()> {
        let bin_v = self.pop_binary("binary.byte_size")?;
        let n = bin_ref(&bin_v).bit_len().div_ceil(8) as i64;
        self.push_int(n);
        Ok(())
    }

    pub(super) fn bin_slice(&mut self) -> VmResult<()> {
        // Ok(view box) or Err(Nil); the bytes are shared.
        let take = self.pop_int("binary.slice")?;
        let at = self.pop_int("binary.slice")?;
        let bin_v = self.pop_binary("binary.slice")?;
        let bin = bin_ref(&bin_v);
        let v = if at < 0 || take < 0 || (at as u64) + (take as u64) > bin.bit_len() {
            self.make_err_nil()
        } else {
            // O(1): a sub-view sharing the backing, no byte copy.
            let (backing, off) = (bin.backing_arc(), bin.bit_offset() + at as u64);
            let view = Value::binary_view_in(&mut self.heap, backing, off, take as u64);
            self.make_ok(view)
        };
        self.stack.push(v);
        Ok(())
    }

    pub(super) fn bin_append(&mut self) -> VmResult<()> {
        // `binary.append` is the 2-ary case of `Op::BinConcatN`: same
        // byte-aligned fast path, same bit-unaligned slow path, one code path.
        self.bin_concat_n(2)
    }

    /// `Op::BinConcatN` — pop the top `n` binaries, push their concatenation.
    /// Emitted for multi-segment `<<>>` literals: the bit length is summed
    /// first, the backing is allocated once, and every source byte is copied
    /// exactly once — the binary mirror of the `StrConcatN` arm, replacing the
    /// `BinAppend` chain that re-copied the accumulated prefix per segment.
    pub(super) fn bin_concat_n(&mut self, n: usize) -> VmResult<()> {
        let base = self.operand_base(n)?;
        let mut total_bits = 0u64;
        let mut aligned = true;
        for (i, v) in self.stack[base..].iter().enumerate() {
            if v.as_binary().is_none() {
                return Err(VmError::internal(
                    "binary construction requires Binary segments",
                ));
            }
            let b = bin_ref(v);
            // The fast path needs every fragment to start on a byte boundary
            // and all but the last to be whole bytes, so the result is the
            // byte windows laid back to back.
            if !b.bit_offset().is_multiple_of(8) || (i + 1 < n && !b.bit_len().is_multiple_of(8)) {
                aligned = false;
            }
            total_bits += b.bit_len();
        }
        // ensure() may move the operands, but the stack roots them, so the
        // slots hold valid pointers here; the constructors only allocate —
        // never collect — so the fragment bytes can be read while building.
        let v = if aligned {
            let mut parts: SmallVec<[&[u8]; 8]> = SmallVec::with_capacity(n);
            for v in &self.stack[base..] {
                let b = bin_ref(v);
                let start = (b.bit_offset() / 8) as usize;
                let len = b.bit_len().div_ceil(8) as usize;
                parts.push(&b.backing()[start..start + len]);
            }
            Value::binary_concat_parts_in(&mut self.heap, &parts, total_bits)
        } else {
            // Bit-unaligned slow path: OR each fragment's logical bits into a
            // zeroed buffer at a running bit cursor. Still O(total bits) —
            // no fragment is ever re-copied.
            let mut out = vec![0u8; total_bits.div_ceil(8) as usize];
            let mut at = 0u64;
            for v in &self.stack[base..] {
                let b = bin_ref(v);
                copy_bits(&mut out, at, b.backing(), b.bit_offset(), b.bit_len());
                at += b.bit_len();
            }
            Value::binary_bits_in(&mut self.heap, out, total_bits)
        };
        self.stack.truncate(base);
        self.stack.push(v);
        Ok(())
    }

    /// `<<v:n>>` — encode the low `n` bits of an integer.
    pub(super) fn bin_from_int(&mut self) -> VmResult<()> {
        let num_bits = self.pop_int("binary segment width")?;
        let value = self.pop_int("binary segment value")?;
        let nb = num_bits.max(0) as u64;
        let v = Value::binary_bits_in(&mut self.heap, binary::from_int(value, nb), nb);
        self.stack.push(v);
        Ok(())
    }

    /// `<<a:n>>` pattern — decode `n` bits at an offset as an Int.
    pub(super) fn bin_read_int(&mut self) -> VmResult<()> {
        let num_bits = self.pop_int("binary segment width")?;
        let at = self.pop_int("binary segment offset")?;
        let bin_v = self.pop_binary("binary pattern")?;
        let bin = bin_ref(&bin_v);
        // Read from the shared backing at the view's absolute
        // offset; the limit is the view's logical end, so reads past
        // it return zero (the view never sees neighbouring bits).
        let v = binary::read_int(
            bin.backing(),
            bin.bit_offset() + bin.bit_len(),
            bin.bit_offset() + at.max(0) as u64,
            num_bits.max(0) as u64,
        );
        self.push_int(v);
        Ok(())
    }

    /// `<<x:bytes(n)>>` — splice the first `min(n, len)` bits.
    pub(super) fn bin_take(&mut self) -> VmResult<()> {
        let n = self.pop_int("binary segment width")?;
        let bin_v = self.pop_binary("binary segment")?;
        let bin = bin_ref(&bin_v);
        let take = (n.max(0) as u64).min(bin.bit_len());
        // O(1): a prefix view sharing the backing, no byte copy.
        let (backing, off) = (bin.backing_arc(), bin.bit_offset());
        let v = Value::binary_view_in(&mut self.heap, backing, off, take);
        self.stack.push(v);
        Ok(())
    }

    /// `<<c:utf8>>` pattern — decode one codepoint; pushes the
    /// codepoint and the bits consumed (0,0 on decode failure).
    pub(super) fn bin_read_utf8(&mut self) -> VmResult<()> {
        let at = self.pop_int("binary segment offset")?;
        let bin_v = self.pop_binary("binary pattern")?;
        let bin = bin_ref(&bin_v);
        let (cp, nbits) = binary::read_utf8(
            bin.backing(),
            bin.bit_offset() + bin.bit_len(),
            bin.bit_offset() + at.max(0) as u64,
        )
        .map_or((0, 0), |(c, n)| (c as i64, n as i64));
        self.stack.push(Value::small_int(cp));
        self.stack.push(Value::small_int(nbits));
        Ok(())
    }

    /// `<<'literal', ..>>` pattern — does the binary, at a bit
    /// offset, start with a constant prefix? One compare for the
    /// whole literal; out-of-range is false, never an error.
    pub(super) fn bin_match_prefix(&mut self) -> VmResult<()> {
        let prefix_v = self.pop_binary("binary pattern")?;
        let at = self.pop_int("binary pattern")?;
        let bin_v = self.pop_binary("binary pattern")?;
        let matches = bin_ref(&bin_v).starts_with_at(at.max(0) as u64, &bin_ref(&prefix_v));
        self.stack.push(Value::bool(matches));
        Ok(())
    }

    /// `<<x:bytes(n), ..rest>>` pattern — sub-view at [at, at+len).
    /// The compiler emits a bounds check first, so this is total:
    /// the range is clamped, never an error and never a Result.
    pub(super) fn bin_view(&mut self) -> VmResult<()> {
        let len = self.pop_int("binary pattern")?;
        let at = self.pop_int("binary pattern")?;
        let bin_v = self.pop_binary("binary pattern")?;
        let bin = bin_ref(&bin_v);
        let at = (at.max(0) as u64).min(bin.bit_len());
        let len = (len.max(0) as u64).min(bin.bit_len() - at);
        let (backing, off) = (bin.backing_arc(), bin.bit_offset() + at);
        let v = Value::binary_view_in(&mut self.heap, backing, off, len);
        self.stack.push(v);
        Ok(())
    }

    // --- Binary ASCII builtins -----------------------------------------------
    //
    // All operate on the byte window (the whole bytes of a byte-aligned
    // binary); a non-aligned trailing partial byte is excluded.

    #[cold]
    #[inline(never)]
    pub(super) fn bin_index_of(&mut self) -> VmResult<()> {
        // Some(Int) wrapper at most.
        let from = self.pop_int("binary.index_of")?;
        let needle_v = self.pop_binary("binary.index_of")?;
        let haystack_v = self.pop_binary("binary.index_of")?;
        let hay = bin_ref(&haystack_v).full_bytes();
        let ndl = bin_ref(&needle_v).full_bytes();
        let start = from.clamp(0, hay.len() as i64) as usize;
        // Byte offsets always fit the small-int payload.
        let found = if ndl.is_empty() {
            Some(start as i64)
        } else {
            // SIMD substring search; the single-byte needle case is a
            // straight memchr.
            memchr::memmem::find(&hay[start..], &ndl).map(|rel| (start + rel) as i64)
        };
        drop(hay);
        let v = match found {
            Some(i) => self.make_some(Value::small_int(i)),
            None => self.make_none(),
        };
        self.stack.push(v);
        Ok(())
    }

    #[cold]
    #[inline(never)]
    pub(super) fn bin_byte_at(&mut self) -> VmResult<()> {
        let i = self.pop_int("binary.byte_at")?;
        let bin_v = self.pop_binary("binary.byte_at")?;
        let bytes = bin_ref(&bin_v).full_bytes();
        let v = if i >= 0 && (i as usize) < bytes.len() {
            bytes[i as usize] as i64
        } else {
            -1
        };
        drop(bytes);
        self.stack.push(Value::small_int(v));
        Ok(())
    }

    #[cold]
    #[inline(never)]
    pub(super) fn bin_parse_int(&mut self) -> VmResult<()> {
        let radix = self.pop_radix("binary.parse_int")?;
        let b_v = self.pop_binary("binary.parse_int")?;
        let parsed = parse_uint_ascii(&bin_ref(&b_v).full_bytes(), radix);
        let v = match parsed {
            Some(n) => {
                let n = self.boxed_int(n);
                self.make_some(n)
            }
            None => self.make_none(),
        };
        self.stack.push(v);
        Ok(())
    }

    #[cold]
    #[inline(never)]
    pub(super) fn bin_eq_ignore_ascii_case(&mut self) -> VmResult<()> {
        let b_v = self.pop_binary("binary.eq_ignore_ascii_case")?;
        let a_v = self.pop_binary("binary.eq_ignore_ascii_case")?;
        let aw = bin_ref(&a_v).full_bytes();
        let bw = bin_ref(&b_v).full_bytes();
        let eq = aw.eq_ignore_ascii_case(&bw);
        drop((aw, bw));
        self.stack.push(Value::bool(eq));
        Ok(())
    }

    #[cold]
    #[inline(never)]
    pub(super) fn bin_to_ascii_lower(&mut self) -> VmResult<()> {
        // One fresh box; the lowered bytes live off-heap.
        let b_v = self.pop_binary("binary.to_ascii_lower")?;
        let mut out = bin_ref(&b_v).full_bytes().into_owned();
        out.make_ascii_lowercase();
        let v = Value::binary_in(&mut self.heap, out);
        self.stack.push(v);
        Ok(())
    }

    #[cold]
    #[inline(never)]
    pub(super) fn bin_from_int_ascii(&mut self) -> VmResult<()> {
        let radix = self.pop_radix("binary.from_int_ascii")?;
        let n = self.pop_int("binary.from_int_ascii")?;
        let v = Value::binary_from_slice_in(&mut self.heap, int_to_ascii(n, radix).as_bytes());
        self.stack.push(v);
        Ok(())
    }

    /// Pop an `al/binary.Radix` enum value. Once past the AL type-checker only
    /// `Dec`/`Hex` can appear on the stack, so the `_` arm is a broken-invariant
    /// guard, not a user-reachable error.
    fn pop_radix(&mut self, op: &'static str) -> VmResult<Radix> {
        let v = self.pop()?;
        match v.as_enum().map(|e| e.variant_name()) {
            Some("Dec") => Ok(Radix::Dec),
            Some("Hex") => Ok(Radix::Hex),
            _ => Err(VmError::type_mismatch(op, "Radix", &v)),
        }
    }

    // --- HTTP protocol builtins ------------------------------------------------
    //
    // `al/http/h1` and `al/http/headers` hot paths; see `vm::http`.

    #[cold]
    #[inline(never)]
    pub(super) fn http_parse_head(&mut self) -> VmResult<()> {
        let off = self.pop_int("h1.parse_request")?;
        let buf_v = self.pop_binary("h1.parse_request")?;
        let v = http::parse_head(&self.templates, &mut self.heap, &bin_ref(&buf_v), off);
        self.stack.push(v);
        Ok(())
    }

    #[cold]
    #[inline(never)]
    pub(super) fn http_framing(&mut self) -> VmResult<()> {
        // Length(Int) is the only fresh allocation; the other
        // framings are prebuilt templates.
        let headers = self.pop()?;
        let v = http::framing(&self.templates, &mut self.heap, &headers)?;
        self.stack.push(v);
        Ok(())
    }

    #[cold]
    #[inline(never)]
    pub(super) fn http_chunk_decode(&mut self) -> VmResult<()> {
        let max = self.pop_int("h1.chunk_decode")?;
        let off = self.pop_int("h1.chunk_decode")?;
        let buf_v = self.pop_binary("h1.chunk_decode")?;
        let v = http::chunk_decode(&self.templates, &mut self.heap, &bin_ref(&buf_v), off, max);
        self.stack.push(v);
        Ok(())
    }

    #[cold]
    #[inline(never)]
    pub(super) fn http_header_get(&mut self) -> VmResult<()> {
        // Some(view clone) at most — the view itself is shared.
        let name_v = self.pop_binary("headers.get")?;
        let headers = self.pop()?;
        let v = http::header_get(&self.templates, &mut self.heap, &headers, &bin_ref(&name_v))?;
        self.stack.push(v);
        Ok(())
    }

    #[cold]
    #[inline(never)]
    pub(super) fn http_headers_valid(&mut self) -> VmResult<()> {
        let headers = self.pop()?;
        let v = http::headers_valid(&headers)?;
        self.stack.push(v);
        Ok(())
    }

    #[cold]
    #[inline(never)]
    pub(super) fn http_header_has(&mut self) -> VmResult<()> {
        let name_v = self.pop_binary("headers.has")?;
        let headers = self.pop()?;
        let v = http::header_has(&headers, &bin_ref(&name_v))?;
        self.stack.push(v);
        Ok(())
    }

    #[cold]
    #[inline(never)]
    pub(super) fn http_serialize_head(&mut self) -> VmResult<()> {
        // One fresh box over the serialized bytes (off-heap).
        let headers = self.pop()?;
        let reason_v = self.pop_binary("h1.serialize_head")?;
        let code = self.pop_int("h1.serialize_head")?;
        let v = http::serialize_head(&mut self.heap, code, &bin_ref(&reason_v), &headers)?;
        self.stack.push(v);
        Ok(())
    }
}

/// The two bases the ASCII int helpers support. Mirrors `al/binary.Radix`
/// exactly, so an unsupported base is unrepresentable on both sides — no
/// silent base-10 fallback and no `_ => None` arm to keep in sync.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum Radix {
    Dec,
    Hex,
}

impl Radix {
    #[inline]
    fn base(self) -> i64 {
        match self {
            Radix::Dec => 10,
            Radix::Hex => 16,
        }
    }
}

/// Parse ASCII bytes as a non-negative integer in the given [`Radix`] (both
/// hex cases). Digits only, no sign — [`int_to_ascii`] round-trips only for
/// `n >= 0`. Returns `None` for an empty input, any non-digit byte, or on
/// overflow — the multiply/add are checked so a value that would wrap (e.g.
/// an oversized `Content-Length`) is rejected rather than silently truncated
/// by AL's wrapping arithmetic.
pub(super) fn parse_uint_ascii(bytes: &[u8], radix: Radix) -> Option<i64> {
    if bytes.is_empty() {
        return None;
    }
    let base = radix.base();
    let mut acc: i64 = 0;
    for &c in bytes {
        let digit = match c {
            b'0'..=b'9' => (c - b'0') as i64,
            b'a'..=b'f' if radix == Radix::Hex => (c - b'a' + 10) as i64,
            b'A'..=b'F' if radix == Radix::Hex => (c - b'A' + 10) as i64,
            _ => return None,
        };
        acc = acc.checked_mul(base)?.checked_add(digit)?;
    }
    Some(acc)
}

/// Stack-rendered ASCII digits of an `Int`; `i64::MIN` in base 10 is the
/// 20-byte ceiling. Avoids a heap allocation per conversion.
pub(super) struct IntAscii {
    buf: [u8; 20],
    start: usize,
}

impl IntAscii {
    pub(super) fn as_bytes(&self) -> &[u8] {
        &self.buf[self.start..]
    }

    #[allow(clippy::expect_used)]
    pub(super) fn as_str(&self) -> &str {
        std::str::from_utf8(self.as_bytes()).expect("int_to_ascii writes only ASCII")
    }
}

/// Render an `Int` as ASCII digits in the given [`Radix`] (lowercase hex),
/// without a `to_string` round-trip or heap allocation. Handles zero,
/// negatives, and `i64::MIN` (via `unsigned_abs`).
pub(super) fn int_to_ascii(n: i64, radix: Radix) -> IntAscii {
    let base = radix.base() as u64;
    let mut buf = [0u8; 20];
    let mut start = buf.len();
    if n == 0 {
        start -= 1;
        buf[start] = b'0';
        return IntAscii { buf, start };
    }
    let negative = n < 0;
    let mut mag = n.unsigned_abs();
    // Fill from the back so the digits land in order without a reverse.
    while mag > 0 {
        let d = (mag % base) as u8;
        start -= 1;
        buf[start] = if d < 10 { b'0' + d } else { b'a' + (d - 10) };
        mag /= base;
    }
    if negative {
        start -= 1;
        buf[start] = b'-';
    }
    IntAscii { buf, start }
}
