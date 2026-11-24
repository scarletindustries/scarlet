//! Everything a run holds but its registers, and what reaches none of those:
//! the built-ins, and the work of every instruction that is more than a few
//! lines.
//!
//! The interpreter's loop reaches it through one reference ([`Rest`]).

use num_bigint::Sign;
use num_traits::{FromPrimitive, ToPrimitive};
use scarlet_ir::core_ir::{ConstId, FuncIdx, HttpTypes, IoErrors, JsonTypes, VariantRef};
use scarlet_ir::intrinsic::Intrinsic;

use crate::Stop;
use crate::array::{self, End, Seq};
use crate::bigint::{self, Int};
use crate::binary::{self, Bits};
use crate::code::{BinaryId, BitsOp, Code, Entry, Reg, TextId};
use crate::float;
use crate::frame::WINDOW;
use crate::heap::{Cell, DeadCell, Fault, Heap, Kind};
use crate::host::{Host, IoError};
use crate::http;
use crate::int;
use crate::json;
use crate::map;
use crate::ops::{Arith, Compare, NumOp};
use crate::show;
use crate::stop::{Broken, Missing, fault};
use crate::value::{Value, View};

mod net;

// The fields keep this order, with the heap last, so that a change to the
// heap's size cannot move the others. An offset past what a load can hold takes
// an instruction more at each use.
#[repr(C)]
pub(super) struct Rest<'c, 'h> {
    pub(super) code: &'c Code,
    pub(super) host: &'h mut dyn Host,
    pub(super) globals: Vec<Value>,
    /// `os.env` as a map, made the first time it is asked for. The
    /// environment cannot change while the program runs, so every call after
    /// shares this one.
    pub(super) env: Option<Cell>,
    /// How many times each block of a counted load has run, by the number
    /// its `Count` instruction holds. It is empty for an ordinary load,
    /// which has no such instruction.
    pub(super) ran: Vec<u64>,
    /// The values on their way into a frame or a cell, which are the arguments
    /// of a call or the fields of a constructor that has too many to hold. It
    /// is kept between uses so that neither allocates.
    pub(super) passing: Vec<Value>,
    pub(super) heap: Heap,
}

/// The values in the registers an instruction names, which it only looks at.
#[derive(Clone, Copy)]
struct Operands<'a> {
    frame: &'a [Value; WINDOW],
    regs: &'a [Reg],
}

impl Operands<'_> {
    fn get(self, k: usize) -> Value {
        let reg = self.regs.get(k).map(|r| usize::from(r.0));
        let v = reg.and_then(|r| self.frame.get(r));
        v.copied().unwrap_or(Value::NIL)
    }

    fn iter(self) -> impl Iterator<Item = Value> {
        (0..self.regs.len()).map(move |k| self.get(k))
    }
}

impl<'c> Rest<'c, '_> {
    /// What built-in `i` gives for the values in `args`, which it only reads.
    /// The loader checked how many there are (`code::built`). The result
    /// holds its own reference.
    pub(super) fn builtin(
        &mut self,
        i: Intrinsic,
        frame: &[Value; WINDOW],
        regs: &[Reg],
    ) -> Result<Value, Stop> {
        let args = Operands { frame, regs };
        let v = args.get(0);
        match i {
            // Read the count before making this call's own answer, so asking
            // does not change what it says.
            Intrinsic::InternalCellsMade => {
                let n = self.heap.made();
                bigint::int(&mut self.heap, n).map_err(fault)
            }
            Intrinsic::InternalCellsReused => {
                let n = self.heap.reused();
                bigint::int(&mut self.heap, n).map_err(fault)
            }
            Intrinsic::BinaryFromString => {
                let text = self.text_of(v)?;
                let cell = binary::from_bytes(&mut self.heap, &text);
                Ok(Value::cell(cell.map_err(fault)?))
            }
            Intrinsic::BinaryToString => {
                let b = self.binary(v)?;
                let text = binary::bytes(&self.heap, b);
                match std::str::from_utf8(&text) {
                    Ok(_) if b.len % 8 == 0 => {
                        let s = Value::cell(self.heap.string(&text).map_err(fault)?);
                        self.ok(s)
                    }
                    _ => self.err_nil(),
                }
            }
            Intrinsic::BinaryBitSize => {
                let n = self.binary(v)?.len;
                bigint::int(&mut self.heap, n).map_err(fault)
            }
            Intrinsic::BinaryByteSize => {
                let n = self.binary(v)?.len.div_ceil(8);
                bigint::int(&mut self.heap, n).map_err(fault)
            }
            Intrinsic::BinaryAppend => {
                let (a, b) = (self.binary(v)?, self.binary(args.get(1))?);
                let cell = binary::join(&mut self.heap, &[a, b]).map_err(fault)?;
                Ok(Value::cell(cell))
            }
            // The ASCII built-ins read whole bytes only, as the old VM's did.
            // A last byte that is not whole is left out.
            Intrinsic::BinaryByteAt => {
                let b = self.binary(v)?;
                let found = match self.int_of(args.get(1))? {
                    Int::Small(i) => u64::try_from(i)
                        .ok()
                        .and_then(|i| binary::whole_byte(&self.heap, b, i)),
                    Int::Big(_) => None,
                };
                let n = found.map_or(-1, i64::from);
                bigint::int(&mut self.heap, n).map_err(fault)
            }
            // The search starts at `from`, clamped to the binary. An empty
            // needle is found where the search starts.
            Intrinsic::BinaryIndexOf => {
                let hay = binary::whole_bytes(&self.heap, self.binary(v)?);
                let needle = binary::whole_bytes(&self.heap, self.binary(args.get(1))?);
                let from = match self.int_of(args.get(2))? {
                    Int::Small(n) => usize::try_from(n).unwrap_or(0).min(hay.len()),
                    Int::Big(n) if n.sign() == num_bigint::Sign::Minus => 0,
                    Int::Big(_) => hay.len(),
                };
                let found = if needle.is_empty() {
                    Some(from)
                } else {
                    hay.get(from..)
                        .and_then(|rest| rest.windows(needle.len()).position(|w| w == needle))
                        .map(|at| from.saturating_add(at))
                };
                match found {
                    Some(at) => {
                        let at = bigint::int(&mut self.heap, at).map_err(fault)?;
                        self.some(at)
                    }
                    None => Ok(Value::nullary(self.code.abi.none)),
                }
            }
            // This takes digits only, with no sign. Empty input or any other
            // byte gives `None`. An Int has no bounds, so no number is too
            // large.
            Intrinsic::BinaryParseInt => {
                let digits = binary::whole_bytes(&self.heap, self.binary(v)?);
                let base = self.radix(args.get(1))?;
                let valid =
                    !digits.is_empty() && digits.iter().all(|c| char::from(*c).is_digit(base));
                let n = if valid {
                    num_bigint::BigInt::parse_bytes(&digits, base)
                } else {
                    None
                };
                match n {
                    Some(n) => {
                        let n = bigint::value(&mut self.heap, n).map_err(fault)?;
                        self.some(n)
                    }
                    None => Ok(Value::nullary(self.code.abi.none)),
                }
            }
            Intrinsic::BinaryEqIgnoreAsciiCase => {
                let a = binary::whole_bytes(&self.heap, self.binary(v)?);
                let b = binary::whole_bytes(&self.heap, self.binary(args.get(1))?);
                Ok(Value::bool(a.eq_ignore_ascii_case(&b)))
            }
            Intrinsic::BinaryToAsciiLower => {
                let mut bytes = binary::whole_bytes(&self.heap, self.binary(v)?);
                bytes.make_ascii_lowercase();
                Ok(Value::cell(
                    binary::from_bytes(&mut self.heap, &bytes).map_err(fault)?,
                ))
            }
            // This writes lowercase hex, with a `-` before a negative
            // number's digits.
            Intrinsic::BinaryFromIntAscii => {
                let n = match self.int_of(v)? {
                    Int::Small(n) => num_bigint::BigInt::from(n),
                    Int::Big(n) => n,
                };
                let text = n.to_str_radix(self.radix(args.get(1))?);
                let cell = binary::from_bytes(&mut self.heap, text.as_bytes());
                Ok(Value::cell(cell.map_err(fault)?))
            }
            // A negative bound, or one past the end, is `Err(Nil)`, never a
            // short read (`binary.sca`).
            Intrinsic::BinarySliceBits => {
                let b = self.binary(v)?;
                let (at, take) = (self.int_of(args.get(1))?, self.int_of(args.get(2))?);
                let window = match (at, take) {
                    (Int::Small(at), Int::Small(take)) if at >= 0 && take >= 0 => {
                        let (at, take) = (at.unsigned_abs(), take.unsigned_abs());
                        let fits = at.checked_add(take).is_some_and(|end| end <= b.len);
                        fits.then_some((at, take))
                    }
                    _ => None,
                };
                match window {
                    Some((at, take)) => {
                        let cut = binary::slice(&mut self.heap, b, at, take).map_err(fault)?;
                        self.ok(Value::cell(cut))
                    }
                    None => self.err_nil(),
                }
            }
            Intrinsic::StringInspect => {
                let mut text = Vec::new();
                self.show(v, &mut text)?;
                Ok(Value::cell(self.heap.string(&text).map_err(fault)?))
            }
            // Adjacent delimiters give empty pieces, and an empty `on` gives
            // one piece per Unicode scalar value (`string.sca`).
            Intrinsic::StringSplit => {
                let s = self.utf8_of(v)?;
                let on = self.utf8_of(args.get(1))?;
                let pieces: Vec<String> = if on.is_empty() {
                    s.chars().map(String::from).collect()
                } else {
                    s.split(on.as_str()).map(String::from).collect()
                };
                self.strings(&pieces)
            }
            Intrinsic::StringContains => {
                let (s, needle) = (self.utf8_of(v)?, self.utf8_of(args.get(1))?);
                Ok(Value::bool(s.contains(needle.as_str())))
            }
            Intrinsic::StringTrim => {
                let s = self.utf8_of(v)?;
                Ok(Value::cell(
                    self.heap.string(s.trim().as_bytes()).map_err(fault)?,
                ))
            }
            Intrinsic::StringToGraphemes => {
                let s = self.utf8_of(v)?;
                let pieces: Vec<String> =
                    unicode_segmentation::UnicodeSegmentation::graphemes(s.as_str(), true)
                        .map(String::from)
                        .collect();
                self.strings(&pieces)
            }
            // This takes an optional sign, then one or more ASCII digits, and
            // nothing else. An Int has no bounds, so no number of digits is
            // too many.
            Intrinsic::IntFromString => {
                let s = self.utf8_of(v)?;
                let digits = s.strip_prefix(['+', '-']).unwrap_or(&s);
                let valid = !digits.is_empty() && digits.bytes().all(|c| c.is_ascii_digit());
                let n = if valid {
                    num_bigint::BigInt::parse_bytes(s.as_bytes(), 10)
                } else {
                    None
                };
                match n {
                    Some(n) => {
                        let n = bigint::value(&mut self.heap, n).map_err(fault)?;
                        self.some(n)
                    }
                    None => Ok(Value::nullary(self.code.abi.none)),
                }
            }
            // These treat an Int as an endless row of two's-complement bits
            // (`int.sca`).
            Intrinsic::IntBitwiseAnd | Intrinsic::IntBitwiseOr | Intrinsic::IntBitwiseXor => {
                let (a, b) = (self.int_of(v)?, self.int_of(args.get(1))?);
                // Two small Ints give a small answer, since these never widen.
                if let (Int::Small(a), Int::Small(b)) = (&a, &b) {
                    let n = match i {
                        Intrinsic::IntBitwiseAnd => a & b,
                        Intrinsic::IntBitwiseOr => a | b,
                        _ => a ^ b,
                    };
                    return bigint::int(&mut self.heap, n).map_err(fault);
                }
                let (a, b) = (a.big(), b.big());
                let n = match i {
                    Intrinsic::IntBitwiseAnd => a & b,
                    Intrinsic::IntBitwiseOr => a | b,
                    _ => a ^ b,
                };
                bigint::value(&mut self.heap, n).map_err(fault)
            }
            Intrinsic::IntBitwiseNot => match self.int_of(v)? {
                Int::Small(n) => bigint::int(&mut self.heap, !n).map_err(fault),
                Int::Big(n) => bigint::value(&mut self.heap, !n).map_err(fault),
            },
            // A negative count shifts the other way.
            Intrinsic::IntBitwiseShiftLeft | Intrinsic::IntBitwiseShiftRight => {
                let x = self.int_of(v)?.big();
                let count = self.int_of(args.get(1))?.big();
                let left = (i == Intrinsic::IntBitwiseShiftLeft) != (count.sign() == Sign::Minus);
                let n = shift(x, left, &count.magnitude().clone().into())?;
                bigint::value(&mut self.heap, n).map_err(fault)
            }
            Intrinsic::StringLength => {
                let Some(cell) = v.as_cell().filter(|_| self.is_string(v)) else {
                    return Err(Stop::broken(Broken::StringLength(v.view())));
                };
                let mut bytes = Vec::with_capacity(self.heap.string_len(cell));
                self.heap.read_string(cell, &mut bytes);
                let n = match std::str::from_utf8(&bytes) {
                    Ok(s) => s.chars().count(),
                    Err(_) => {
                        return Err(Stop::broken(Broken::NotUtf8));
                    }
                };
                bigint::int(&mut self.heap, n).map_err(fault)
            }
            Intrinsic::ArrayLength => {
                let n = self.seq(v)?.len(&self.heap);
                bigint::int(&mut self.heap, n).map_err(fault)
            }
            // A wait too long for a `u64` of milliseconds is still one nobody
            // outlives, so it waits that long instead.
            Intrinsic::ProcessSleep => {
                let ms = match self.int_of(v)? {
                    Int::Small(n) => u64::try_from(n).unwrap_or(0),
                    Int::Big(n) if n.sign() == Sign::Minus => 0,
                    Int::Big(_) => u64::MAX,
                };
                if ms > 0 {
                    self.host.sleep_ms(ms);
                }
                Ok(Value::NIL)
            }
            Intrinsic::TimeMonotonic => {
                let ms = self.host.monotonic_ms();
                bigint::int(&mut self.heap, ms).map_err(fault)
            }
            Intrinsic::TimeEpochMs => {
                let ms = self.host.epoch_ms();
                bigint::int(&mut self.heap, ms).map_err(fault)
            }
            // We never give zeros or anything else in place of bytes the host
            // did not give.
            Intrinsic::CryptoRandomBytes => {
                let n = match self.int_of(v)? {
                    Int::Small(n) => usize::try_from(n).ok(),
                    Int::Big(_) => None,
                };
                let Some(n) = n else {
                    return self.err_nil();
                };
                if n > binary::MAX_BYTES {
                    return Err(Stop::HeapFull);
                }
                let mut bytes = vec![0u8; n];
                if self.host.random_bytes(&mut bytes).is_err() {
                    return self.err_nil();
                }
                let cell = binary::from_bytes(&mut self.heap, &bytes).map_err(fault)?;
                self.ok(Value::cell(cell))
            }
            // The digests and the tag are over bytes. A binary that is not
            // whole bytes has none, instead of the digest of some bytes near
            // it, which another binary would share.
            Intrinsic::CryptoSha1 | Intrinsic::CryptoSha256 | Intrinsic::CryptoSha512 => {
                let Some(m) = self.whole_bytes(v)? else {
                    return self.err_nil();
                };
                let alg = match i {
                    Intrinsic::CryptoSha1 => &aws_lc_rs::digest::SHA1_FOR_LEGACY_USE_ONLY,
                    Intrinsic::CryptoSha256 => &aws_lc_rs::digest::SHA256,
                    _ => &aws_lc_rs::digest::SHA512,
                };
                let digest = aws_lc_rs::digest::digest(alg, &m);
                self.ok_bytes(digest.as_ref())
            }
            Intrinsic::CryptoHmacSha256 => {
                let (Some(key), Some(msg)) = (self.whole_bytes(v)?, self.whole_bytes(args.get(1))?)
                else {
                    return self.err_nil();
                };
                let key = aws_lc_rs::hmac::Key::new(aws_lc_rs::hmac::HMAC_SHA256, &key);
                let tag = aws_lc_rs::hmac::sign(&key, &msg);
                self.ok_bytes(tag.as_ref())
            }
            // This checks for the same bits, in time that depends on the
            // lengths alone. The last byte of each is padded with zeros the
            // same way, so equal lengths and equal bytes mean equal bits.
            Intrinsic::CryptoConstEq => {
                let (a, b) = (self.binary(v)?, self.binary(args.get(1))?);
                let same = a.len == b.len
                    && aws_lc_rs::constant_time::verify_slices_are_equal(
                        &binary::bytes(&self.heap, a),
                        &binary::bytes(&self.heap, b),
                    )
                    .is_ok();
                Ok(Value::bool(same))
            }
            // A key, message or signature of the wrong shape, whole bytes or
            // not, is a signature that does not check out.
            Intrinsic::CryptoP256Verify | Intrinsic::CryptoEd25519Verify => {
                let alg: &'static dyn aws_lc_rs::signature::VerificationAlgorithm = match i {
                    Intrinsic::CryptoP256Verify => &aws_lc_rs::signature::ECDSA_P256_SHA256_ASN1,
                    _ => &aws_lc_rs::signature::ED25519,
                };
                let parts = (
                    self.whole_bytes(v)?,
                    self.whole_bytes(args.get(1))?,
                    self.whole_bytes(args.get(2))?,
                );
                let valid = match parts {
                    (Some(key), Some(message), Some(sig)) => {
                        aws_lc_rs::signature::UnparsedPublicKey::new(alg, key)
                            .verify(&message, &sig)
                            .is_ok()
                    }
                    _ => false,
                };
                Ok(Value::bool(valid))
            }
            Intrinsic::JsonParseBinary => {
                let src = self.binary(v)?;
                let parsed = if src.len % 8 == 0 {
                    json::parse(&binary::bytes(&self.heap, src))
                } else {
                    Err(json::ParseError {
                        offset: (src.len / 8) as usize,
                        reason: json::Reason::NotWholeBytes,
                    })
                };
                let types = self.json_types()?;
                match parsed {
                    Ok((tape, arena)) => {
                        let arena = binary::from_bytes(&mut self.heap, &arena);
                        let arena = Value::cell(arena.map_err(fault)?);
                        let tape = binary::from_bytes(&mut self.heap, &tape);
                        let tape = Value::cell(tape.map_err(fault)?);
                        let root = Value::int(0).unwrap_or(Value::NIL);
                        let doc = self
                            .heap
                            .ctor(types.doc, &[arena, tape, root])
                            .map_err(fault)?;
                        self.ok(Value::cell(doc))
                    }
                    Err(e) => {
                        let offset = bigint::int(&mut self.heap, e.offset).map_err(fault)?;
                        let reason = Value::nullary(e.reason.variant(&types.reasons));
                        let error = self
                            .heap
                            .ctor(types.parse_error, &[offset, reason])
                            .map_err(fault)?;
                        self.err(Value::cell(error))
                    }
                }
            }
            // This gives -1 for a node the tape does not have, which
            // `scarlet/json` checks for.
            Intrinsic::JsonKind | Intrinsic::JsonLen => {
                let d = self.doc(v)?;
                let t = self.tape(&d)?;
                let n = match i {
                    Intrinsic::JsonKind => t.kind(d.idx),
                    _ => t.len(d.idx).and_then(|n| i64::try_from(n).ok()),
                };
                bigint::int(&mut self.heap, n.unwrap_or(-1)).map_err(fault)
            }
            Intrinsic::JsonField | Intrinsic::JsonIndex => {
                let d = self.doc(v)?;
                let key = args.get(1);
                let found = match i {
                    Intrinsic::JsonField => {
                        let name = self.text_of(key)?;
                        self.tape(&d)?.field(d.idx, &name)
                    }
                    _ => match self.int_of(key)? {
                        Int::Small(n) => usize::try_from(n)
                            .ok()
                            .and_then(|n| self.tape(&d).ok()?.element(d.idx, n)),
                        Int::Big(_) => None,
                    },
                };
                match found {
                    Some(at) => {
                        let doc = self.doc_at(&d, at)?;
                        self.some(doc)
                    }
                    None => Ok(Value::nullary(self.code.abi.none)),
                }
            }
            Intrinsic::JsonEntries => {
                let d = self.doc(v)?;
                let members = self.tape(&d)?.members(d.idx).unwrap_or_default();
                let mut items = Vec::with_capacity(members.len());
                for (key, at) in members {
                    let key = Value::cell(self.heap.string(&key).map_err(fault)?);
                    let doc = self.doc_at(&d, at)?;
                    items.push(Value::cell(self.heap.tuple(&[key, doc]).map_err(fault)?));
                }
                Ok(Value::cell(
                    array::from_values(&mut self.heap, &items).map_err(fault)?,
                ))
            }
            Intrinsic::JsonElements => {
                let d = self.doc(v)?;
                let ats = self.tape(&d)?.elements(d.idx).unwrap_or_default();
                let mut items = Vec::with_capacity(ats.len());
                for at in ats {
                    items.push(self.doc_at(&d, at)?);
                }
                Ok(Value::cell(
                    array::from_values(&mut self.heap, &items).map_err(fault)?,
                ))
            }
            Intrinsic::JsonString => {
                let d = self.doc(v)?;
                let text = self.tape(&d)?.string(d.idx);
                match text.filter(|t| std::str::from_utf8(t).is_ok()) {
                    Some(t) => {
                        let s = Value::cell(self.heap.string(&t).map_err(fault)?);
                        self.some(s)
                    }
                    None => Ok(Value::nullary(self.code.abi.none)),
                }
            }
            Intrinsic::JsonInt | Intrinsic::JsonIntText => {
                let d = self.doc(v)?;
                match self.tape(&d)?.int(d.idx) {
                    Some(n) => {
                        let found = match i {
                            Intrinsic::JsonInt => {
                                bigint::value(&mut self.heap, n).map_err(fault)?
                            }
                            _ => Value::cell(
                                self.heap.string(n.to_string().as_bytes()).map_err(fault)?,
                            ),
                        };
                        self.some(found)
                    }
                    None => Ok(Value::nullary(self.code.abi.none)),
                }
            }
            Intrinsic::JsonFloat => {
                let d = self.doc(v)?;
                match self.tape(&d)?.float(d.idx) {
                    Some(f) => self.some(Value::float(f)),
                    None => Ok(Value::nullary(self.code.abi.none)),
                }
            }
            Intrinsic::JsonBool => {
                let d = self.doc(v)?;
                match self.tape(&d)?.bool(d.idx) {
                    Some(b) => self.some(Value::bool(b)),
                    None => Ok(Value::nullary(self.code.abi.none)),
                }
            }
            Intrinsic::JsonEncode => {
                let text = self.encode_json(v)?;
                Ok(Value::cell(self.heap.string(&text).map_err(fault)?))
            }
            Intrinsic::HttpParseHead | Intrinsic::HttpParseResponseHead => {
                let buf = self.binary(v)?;
                let off = self.offset(args.get(1), buf)?;
                // Take enough to reach past the longest head there could be,
                // and its empty lines.
                const WINDOW: u64 = http::MAX_HEAD as u64 + 16;
                let window = binary::bytes_from(&self.heap, buf, off as u64, WINDOW);
                match i {
                    Intrinsic::HttpParseHead => self.parsed_request(buf, off, &window),
                    _ => self.parsed_response(buf, off, &window),
                }
            }
            Intrinsic::HttpChunkDecode => {
                let buf = self.binary(v)?;
                let off = self.offset(args.get(1), buf)?;
                let max = match self.int_of(args.get(2))? {
                    Int::Small(n) => u64::try_from(n).unwrap_or(0),
                    Int::Big(n) if n.sign() == Sign::Minus => 0,
                    Int::Big(_) => u64::MAX,
                };
                let bytes = binary::bytes_from(&self.heap, buf, off as u64, u64::MAX);
                self.chunk_body(buf, off, &bytes, max)
            }
            Intrinsic::HttpFraming => {
                let types = self.http_types()?;
                let (mut te, mut cl) = (Vec::new(), Vec::new());
                for (name, value) in self.headers(v)? {
                    let name = self.bytes_of(name)?;
                    if name.eq_ignore_ascii_case(b"transfer-encoding") {
                        te.push(self.bytes_of(value)?);
                    } else if name.eq_ignore_ascii_case(b"content-length") {
                        cl.push(self.bytes_of(value)?);
                    }
                }
                let te: Vec<&[u8]> = te.iter().map(Vec::as_slice).collect();
                let cl: Vec<&[u8]> = cl.iter().map(Vec::as_slice).collect();
                match http::framing(&te, &cl) {
                    http::Framing::NoBody => Ok(Value::nullary(types.no_body)),
                    http::Framing::Chunked => Ok(Value::nullary(types.chunked)),
                    http::Framing::Length(n) => {
                        let n = bigint::int(&mut self.heap, n).map_err(fault)?;
                        Ok(Value::cell(
                            self.heap.ctor(types.length, &[n]).map_err(fault)?,
                        ))
                    }
                    http::Framing::Invalid(r) => self.with_status(types.framing_invalid, r),
                }
            }
            Intrinsic::HttpHeaderGet | Intrinsic::HttpHeaderHas => {
                let name = self.bytes_of(args.get(1))?;
                let mut found = None;
                for (n, value) in self.headers(v)? {
                    if self.bytes_of(n)?.eq_ignore_ascii_case(&name) {
                        found = Some(value);
                        break;
                    }
                }
                match (i, found) {
                    (Intrinsic::HttpHeaderHas, found) => Ok(Value::bool(found.is_some())),
                    (_, Some(value)) => {
                        let value = self.share(value)?;
                        self.some(value)
                    }
                    (_, None) => Ok(Value::nullary(self.code.abi.none)),
                }
            }
            Intrinsic::HttpHeadersValid => {
                let mut valid = true;
                for (name, value) in self.headers(v)? {
                    let (name, value) = (self.binary(name)?, self.binary(value)?);
                    valid &= name.len % 8 == 0
                        && value.len % 8 == 0
                        && http::is_token(&binary::bytes(&self.heap, name))
                        && http::is_safe_value(&binary::bytes(&self.heap, value));
                }
                Ok(Value::bool(valid))
            }
            Intrinsic::HttpSerializeHead => {
                let code = self.int_of(v)?.to_string();
                let reason = self.bytes_of(args.get(1))?;
                let fields = self.headers(args.get(2))?;
                let mut out = Vec::new();
                out.extend_from_slice(b"HTTP/1.1 ");
                out.extend_from_slice(code.as_bytes());
                out.push(b' ');
                out.extend_from_slice(&reason);
                out.extend_from_slice(b"\r\n");
                for (name, value) in fields {
                    out.extend_from_slice(&self.bytes_of(name)?);
                    out.extend_from_slice(b": ");
                    out.extend_from_slice(&self.bytes_of(value)?);
                    out.extend_from_slice(b"\r\n");
                }
                out.extend_from_slice(b"\r\n");
                let cell = binary::from_bytes(&mut self.heap, &out).map_err(fault)?;
                Ok(Value::cell(cell))
            }
            Intrinsic::IoReadFile => {
                let path = self.text_of(v)?;
                match self.host.read_file(&path) {
                    Ok(bytes) if bytes.len() > binary::MAX_BYTES => Err(Stop::HeapFull),
                    Ok(bytes) => {
                        let cell = binary::from_bytes(&mut self.heap, &bytes).map_err(fault)?;
                        self.ok(Value::cell(cell))
                    }
                    Err(e) => self.io_error(e, &path),
                }
            }
            Intrinsic::IoWriteFile => {
                let path = self.text_of(v)?;
                let data = self.binary(args.get(1))?;
                if data.len % 8 != 0 {
                    let unaligned = self.io_errors()?.unaligned_binary;
                    return self.err(Value::nullary(unaligned));
                }
                let bytes = binary::bytes(&self.heap, data);
                match self.host.write_file(&path, &bytes) {
                    Ok(()) => self.ok(Value::NIL),
                    Err(e) => self.io_error(e, &path),
                }
            }
            Intrinsic::OsArgv => {
                let mut items = Vec::with_capacity(self.host.argv().len());
                for a in self.host.argv() {
                    items.push(Value::cell(self.heap.string(a.as_bytes()).map_err(fault)?));
                }
                Ok(Value::cell(
                    array::from_values(&mut self.heap, &items).map_err(fault)?,
                ))
            }
            Intrinsic::OsEnv => {
                let env = match self.env {
                    Some(env) => env,
                    None => {
                        let env = self.env_map().map_err(fault)?;
                        self.env = Some(env);
                        env
                    }
                };
                self.heap.share(env)?;
                Ok(Value::cell(env))
            }
            Intrinsic::OsTerminalSize => match self.host.terminal_size() {
                Some((cols, rows)) => {
                    let cols = bigint::int(&mut self.heap, cols).map_err(fault)?;
                    let rows = bigint::int(&mut self.heap, rows).map_err(fault)?;
                    let size = self.heap.tuple(&[cols, rows]).map_err(fault)?;
                    self.ok(Value::cell(size))
                }
                None => self.err_nil(),
            },
            Intrinsic::MapNew => Ok(Value::cell(map::empty(&mut self.heap).map_err(fault)?)),
            Intrinsic::MapSet => {
                let m = self.map(v)?;
                let (key, value) = (args.get(1), args.get(2));
                let cell = map::set(&mut self.heap, m, key, value).map_err(fault)?;
                Ok(Value::cell(cell))
            }
            Intrinsic::MapDelete => {
                let (m, key) = (self.map(v)?, args.get(1));
                let cell = map::delete(&mut self.heap, m, key).map_err(fault)?;
                Ok(Value::cell(cell))
            }
            Intrinsic::MapGet => {
                let m = self.map(v)?;
                match map::get(&self.heap, m, args.get(1)) {
                    Some(found) => {
                        let found = self.share(found)?;
                        self.some(found)
                    }
                    None => Ok(Value::nullary(self.code.abi.none)),
                }
            }
            Intrinsic::MapHas => {
                let m = self.map(v)?;
                Ok(Value::bool(map::get(&self.heap, m, args.get(1)).is_some()))
            }
            Intrinsic::MapSize => {
                let n = map::size(&self.heap, self.map(v)?);
                bigint::int(&mut self.heap, n).map_err(fault)
            }
            Intrinsic::MapKeys | Intrinsic::MapValues | Intrinsic::MapToList => {
                let entries = map::entries(&self.heap, self.map(v)?);
                let mut items = Vec::with_capacity(entries.len());
                for (k, v) in entries {
                    let item = match i {
                        Intrinsic::MapKeys => self.share(k)?,
                        Intrinsic::MapValues => self.share(v)?,
                        _ => {
                            let pair = [self.share(k)?, self.share(v)?];
                            Value::cell(self.heap.tuple(&pair).map_err(fault)?)
                        }
                    };
                    items.push(item);
                }
                Ok(Value::cell(
                    array::from_values(&mut self.heap, &items).map_err(fault)?,
                ))
            }
            Intrinsic::FloatFloor
            | Intrinsic::FloatCeil
            | Intrinsic::FloatRound
            | Intrinsic::FloatTruncate => {
                let f = float::read(v)?;
                let whole = match i {
                    Intrinsic::FloatFloor => f.floor(),
                    Intrinsic::FloatCeil => f.ceil(),
                    Intrinsic::FloatRound => f.round(),
                    _ => f.trunc(),
                };
                // A Float is finite, so every whole one is an exact Int.
                if (i64::MIN as f64..i64::MAX as f64).contains(&whole) {
                    #[expect(
                        clippy::cast_possible_truncation,
                        reason = "`whole` is a whole number inside i64's range, so it converts exactly"
                    )]
                    let exact = whole as i64;
                    return bigint::int(&mut self.heap, exact).map_err(fault);
                }
                let n = num_bigint::BigInt::from_f64(whole).unwrap_or_default();
                bigint::value(&mut self.heap, n).map_err(fault)
            }
            // `units * 10^-scale`, rounded once. Rust's parser rounds to the
            // nearest Float, subnormals included, and `Value::float` stops one
            // too large at the largest Float.
            Intrinsic::FloatFromDecimal => {
                let units = self.int_of(v)?.big();
                let scale = self.int_of(args.get(1))?.big();
                #[expect(
                    clippy::arithmetic_side_effects,
                    reason = "an Int grows instead of overflowing"
                )]
                let exponent = -scale;
                let text = format!("{units}e{exponent}");
                let f = text
                    .parse::<f64>()
                    .map_err(|_| Stop::broken(Broken::NotAFloat { text }))?;
                Ok(Value::float(f))
            }
            Intrinsic::FloatToString => {
                let text = float::text(float::read(v)?);
                Ok(Value::cell(
                    self.heap.string(text.as_bytes()).map_err(fault)?,
                ))
            }
            Intrinsic::IntToString => {
                let text = self.int_of(v)?.to_string();
                Ok(Value::cell(
                    self.heap.string(text.as_bytes()).map_err(fault)?,
                ))
            }
            Intrinsic::AddressParse
            | Intrinsic::NetResolve
            | Intrinsic::NetResolveUntil
            | Intrinsic::NetListen
            | Intrinsic::NetAccept
            | Intrinsic::NetConnect
            | Intrinsic::NetConnectUntil
            | Intrinsic::NetLocalAddr
            | Intrinsic::NetClose
            | Intrinsic::SocketRead
            | Intrinsic::SocketReadUntil
            | Intrinsic::SocketWrite
            | Intrinsic::SocketWriteParts
            | Intrinsic::SocketClose
            | Intrinsic::TlsHandshake
            | Intrinsic::TlsHandshakeUntil
            | Intrinsic::TlsRead
            | Intrinsic::TlsReadUntil
            | Intrinsic::TlsWrite
            | Intrinsic::TlsClose => self.net_builtin(i, args),
            other => Err(Stop::missing(Missing::Intrinsic(other))),
        }
    }

    /// A binary operation, with its arguments in the `PrimOp`'s order. A width
    /// or position below zero counts as 0, and a segment wider than the binary
    /// it takes from takes all of it, as in the old VM.
    #[inline(never)]
    pub(super) fn bits_op(
        &mut self,
        op: BitsOp,
        frame: &[Value; WINDOW],
        regs: &[Reg],
    ) -> Result<Value, Stop> {
        let args = Operands { frame, regs };
        match op {
            BitsOp::FromInt => {
                let n = match self.int_of(args.get(0))? {
                    Int::Small(n) => num_bigint::BigInt::from(n),
                    Int::Big(n) => n,
                };
                let width = self.width(args.get(1))?;
                if width > binary::MAX_BITS {
                    return Err(Stop::HeapFull);
                }
                let bytes = binary::from_int(&n, width);
                Ok(Value::cell(
                    binary::make(&mut self.heap, &bytes, width).map_err(fault)?,
                ))
            }
            BitsOp::Take => {
                let b = self.binary(args.get(0))?;
                let take = self.width(args.get(1))?.min(b.len);
                Ok(Value::cell(
                    binary::slice(&mut self.heap, b, 0, take).map_err(fault)?,
                ))
            }
            BitsOp::FromString => {
                let text = self.text_of(args.get(0))?;
                let cell = binary::from_bytes(&mut self.heap, &text);
                Ok(Value::cell(cell.map_err(fault)?))
            }
            BitsOp::Concat => {
                let parts = args
                    .iter()
                    .map(|v| self.binary(v))
                    .collect::<Result<Vec<Bits>, Stop>>()?;
                Ok(Value::cell(
                    binary::join(&mut self.heap, &parts).map_err(fault)?,
                ))
            }
            BitsOp::BitSize => {
                let n = self.binary(args.get(0))?.len;
                bigint::int(&mut self.heap, n).map_err(fault)
            }
            BitsOp::View => {
                let b = self.binary(args.get(0))?;
                let (at, len) = (self.width(args.get(1))?, self.width(args.get(2))?);
                if at.checked_add(len).is_none_or(|end| end > b.len) {
                    return Err(Stop::broken(Broken::ReadPastEnd {
                        at,
                        len,
                        bits: b.len,
                    }));
                }
                Ok(Value::cell(
                    binary::slice(&mut self.heap, b, at, len).map_err(fault)?,
                ))
            }
            BitsOp::MatchPrefix => {
                let b = self.binary(args.get(0))?;
                let at = self.width(args.get(1))?;
                let prefix = self.binary(args.get(2))?;
                Ok(Value::bool(binary::has_at(&self.heap, b, at, prefix)))
            }
            BitsOp::ReadUtf8 => {
                let b = self.binary(args.get(0))?;
                let at = self.width(args.get(1))?;
                let (cp, took) = binary::read_utf8(&self.heap, b, at).unwrap_or((0, 0));
                let cp = Value::int(i64::from(cp));
                let took = i64::try_from(took).ok().and_then(Value::int);
                let pair = [cp, took];
                let [Some(cp), Some(took)] = pair else {
                    return Err(Stop::broken(Broken::CodePointTooBig));
                };
                Ok(Value::cell(self.heap.tuple(&[cp, took]).map_err(fault)?))
            }
            BitsOp::ReadInt => {
                let b = self.binary(args.get(0))?;
                let (at, width) = (self.width(args.get(1))?, self.width(args.get(2))?);
                if at.checked_add(width).is_none_or(|end| end > b.len) {
                    return Err(Stop::broken(Broken::ReadPastEnd {
                        at,
                        len: width,
                        bits: b.len,
                    }));
                }
                let n = binary::read_uint(&self.heap, b, at, width);
                bigint::value(&mut self.heap, n).map_err(fault)
            }
        }
    }

    /// The binary `v` holds.
    fn binary(&self, v: Value) -> Result<Bits, Stop> {
        v.as_cell()
            .and_then(|cell| binary::bits(&self.heap, cell))
            .ok_or_else(|| Stop::broken(Broken::NotBinary(v.view())))
    }

    /// The Int `v` as a width or position in bits. It is 0 below zero, as in
    /// the old VM, and past the end of any binary when it does not fit in 64
    /// bits.
    fn width(&self, v: Value) -> Result<u64, Stop> {
        Ok(match self.int_of(v)? {
            Int::Small(n) => u64::try_from(n).unwrap_or(0),
            Int::Big(n) if n.sign() == num_bigint::Sign::Minus => 0,
            Int::Big(_) => u64::MAX,
        })
    }

    /// The string `v`, as text. Every string the VM makes is UTF-8.
    fn utf8_of(&self, v: Value) -> Result<String, Stop> {
        String::from_utf8(self.text_of(v)?).map_err(|_| Stop::broken(Broken::NotUtf8))
    }

    /// A new array of new strings holding `pieces`.
    fn strings(&mut self, pieces: &[String]) -> Result<Value, Stop> {
        let mut items = Vec::with_capacity(pieces.len());
        for p in pieces {
            items.push(Value::cell(self.heap.string(p.as_bytes()).map_err(fault)?));
        }
        Ok(Value::cell(
            array::from_values(&mut self.heap, &items).map_err(fault)?,
        ))
    }

    /// The bytes of the string `v`.
    #[inline(never)]
    fn text_of(&self, v: Value) -> Result<Vec<u8>, Stop> {
        match v.as_cell() {
            Some(cell) if self.is_string(v) => {
                let mut text = Vec::with_capacity(self.heap.string_len(cell));
                self.heap.read_string(cell, &mut text);
                Ok(text)
            }
            _ => Err(Stop::broken(Broken::NotString(v.view()))),
        }
    }

    /// The base a `scarlet/binary.Radix` stands for.
    fn radix(&self, v: Value) -> Result<u32, Stop> {
        match (v.view(), self.code.abi.radix) {
            (View::Nullary(r), Some(radix)) if r == radix.dec => Ok(10),
            (View::Nullary(r), Some(radix)) if r == radix.hex => Ok(16),
            _ => Err(Stop::broken(Broken::NotRadix(v.view()))),
        }
    }

    /// `Some(v)`, holding `v`'s reference.
    pub(super) fn some(&mut self, v: Value) -> Result<Value, Stop> {
        let cell = self.heap.ctor(self.code.abi.some, &[v]).map_err(fault)?;
        Ok(Value::cell(cell))
    }

    /// `Ok(v)`, holding `v`'s reference.
    fn ok(&mut self, v: Value) -> Result<Value, Stop> {
        let cell = self.heap.ctor(self.code.abi.ok, &[v]).map_err(fault)?;
        Ok(Value::cell(cell))
    }

    /// `Err(v)`, holding `v`'s reference.
    fn err(&mut self, v: Value) -> Result<Value, Stop> {
        let cell = self.heap.ctor(self.code.abi.err, &[v]).map_err(fault)?;
        Ok(Value::cell(cell))
    }

    fn err_nil(&mut self) -> Result<Value, Stop> {
        let cell = self
            .heap
            .ctor(self.code.abi.err, &[Value::NIL])
            .map_err(fault)?;
        Ok(Value::cell(cell))
    }

    /// The environment as a map. When a name is listed twice, the first
    /// wins, as it does for the OS's own `getenv`.
    fn env_map(&mut self) -> Result<Cell, Fault> {
        let heap = &mut self.heap;
        let mut env = map::empty(heap)?;
        for (name, value) in self.host.env() {
            let k = heap.string(name.as_bytes())?;
            let next = if map::get(heap, env, Value::cell(k)).is_some() {
                None
            } else {
                let v = heap.string(value.as_bytes())?;
                let next = map::set(heap, env, Value::cell(k), Value::cell(v))?;
                heap.release(v)?;
                Some(next)
            };
            heap.release(k)?;
            if let Some(next) = next {
                heap.release(env)?;
                env = next;
            }
        }
        Ok(env)
    }

    fn io_errors(&self) -> Result<IoErrors, Stop> {
        self.code.abi.io.ok_or(Stop::broken(Broken::NoIoTypes))
    }

    /// `Err` of the `IoError` `e` stands for, about `path`.
    fn io_error(&mut self, e: IoError, path: &[u8]) -> Result<Value, Stop> {
        let io = self.io_errors()?;
        let about_path = match e {
            IoError::NotFound => io.not_found,
            IoError::PermissionDenied => io.permission_denied,
            IoError::AlreadyExists => io.already_exists,
            IoError::NotADirectory => io.not_a_directory,
            IoError::IsADirectory => io.is_a_directory,
            IoError::ReadOnlyFilesystem => io.read_only_filesystem,
            IoError::FilesystemLoop => io.filesystem_loop,
            IoError::FileTooLarge => io.file_too_large,
            IoError::StorageFull => return self.err(Value::nullary(io.storage_full)),
            IoError::QuotaExceeded => return self.err(Value::nullary(io.quota_exceeded)),
            IoError::Errno(code) => {
                let code = bigint::int(&mut self.heap, code).map_err(fault)?;
                let error = Value::cell(self.heap.ctor(io.errno, &[code]).map_err(fault)?);
                return self.err(error);
            }
        };
        let path = Value::cell(self.heap.string(path).map_err(fault)?);
        let error = Value::cell(self.heap.ctor(about_path, &[path]).map_err(fault)?);
        self.err(error)
    }

    fn json_types(&self) -> Result<JsonTypes, Stop> {
        self.code.abi.json.ok_or(Stop::broken(Broken::NoJsonTypes))
    }

    /// The parts of the `Doc` `v`. Only `json.parse` makes one, so anything
    /// else is a program the compiler should not have let through.
    fn doc(&self, v: Value) -> Result<Doc, Stop> {
        let types = self.json_types()?;
        let not_a_doc = || Stop::broken(Broken::NotADoc(v.view()));
        let cell = v.as_cell().ok_or_else(not_a_doc)?;
        if self.heap.kind(cell) != Some(Kind::Ctor) || self.heap.variant(cell) != types.doc {
            return Err(not_a_doc());
        }
        let field = |i| self.heap.field(cell, i).ok_or_else(not_a_doc);
        let (arena, tape) = (field(0)?, field(1)?);
        // An index no tape has becomes a node past any tape, so every read of
        // it is `None`.
        let idx = match self.int_of(field(2)?)? {
            Int::Small(n) => usize::try_from(n).unwrap_or(usize::MAX),
            Int::Big(_) => usize::MAX,
        };
        Ok(Doc { arena, tape, idx })
    }

    fn tape(&self, d: &Doc) -> Result<json::Tape<'_>, Stop> {
        Ok(json::Tape {
            heap: &self.heap,
            tape: self.binary(d.tape)?,
            arena: self.binary(d.arena)?,
        })
    }

    /// A `Doc` over the same document as `d`, at node `idx`.
    fn doc_at(&mut self, d: &Doc, idx: usize) -> Result<Value, Stop> {
        let types = self.json_types()?;
        let idx = bigint::int(&mut self.heap, idx).map_err(fault)?;
        let (arena, tape) = (self.share(d.arena)?, self.share(d.tape)?);
        let cell = self
            .heap
            .ctor(types.doc, &[arena, tape, idx])
            .map_err(fault)?;
        Ok(Value::cell(cell))
    }

    /// The `scarlet/json.Json` tree `root`, as JSON text. The walk keeps a
    /// list of what is still to write instead of recursing, so a very deeply
    /// nested tree encodes without overflowing the stack.
    fn encode_json(&self, root: Value) -> Result<Vec<u8>, Stop> {
        enum Step {
            Value(Value),
            Key(Value),
            Text(&'static [u8]),
        }
        let types = self.json_types()?;
        let mut out = Vec::new();
        let mut todo = vec![Step::Value(root)];
        while let Some(step) = todo.pop() {
            let v = match step {
                Step::Text(t) => {
                    out.extend_from_slice(t);
                    continue;
                }
                Step::Key(k) => {
                    json::write_string(&mut out, &String::from_utf8_lossy(&self.text_of(k)?));
                    continue;
                }
                Step::Value(v) => v,
            };
            let not_json = || Stop::broken(Broken::NotJson(v.view()));
            let cell = match v.view() {
                View::Nullary(n) if n == types.null => {
                    out.extend_from_slice(b"null");
                    continue;
                }
                View::Cell(cell) if self.heap.kind(cell) == Some(Kind::Ctor) => cell,
                View::Nullary(_)
                | View::Cell(_)
                | View::Int(_)
                | View::Float(_)
                | View::Nil
                | View::Bool(_)
                | View::Func(_) => return Err(not_json()),
            };
            let variant = self.heap.variant(cell);
            let inner = self.heap.field(cell, 0).ok_or_else(not_json)?;
            if variant == types.boolean {
                let yes = matches!(inner.view(), View::Bool(true));
                out.extend_from_slice(if yes { b"true" } else { b"false" });
            } else if variant == types.integer {
                let n = self.int_of(inner)?.to_string();
                out.extend_from_slice(n.as_bytes());
            } else if variant == types.real {
                json::write_float(&mut out, float::read(inner)?);
            } else if variant == types.str {
                json::write_string(&mut out, &String::from_utf8_lossy(&self.text_of(inner)?));
            } else if variant == types.number {
                // Text that is not a JSON number becomes `null`, instead of
                // bytes that stop the whole document parsing at the far end.
                let digits = self.text_of(inner)?;
                if json::is_number(&digits) {
                    out.extend_from_slice(&digits);
                } else {
                    out.extend_from_slice(b"null");
                }
            } else if variant == types.list || variant == types.object {
                let Seq::Tree(items) = self.seq(inner)? else {
                    return Err(not_json());
                };
                let items = array::elements(&self.heap, items);
                let object = variant == types.object;
                out.push(if object { b'{' } else { b'[' });
                todo.push(Step::Text(if object { b"}" } else { b"]" }));
                // Pushed last to first, so they come off in order, each
                // after the comma before it.
                for (i, item) in items.into_iter().enumerate().rev() {
                    if object {
                        let pair = item
                            .as_cell()
                            .filter(|&c| self.heap.kind(c) == Some(Kind::Tuple));
                        let pair = pair.ok_or_else(not_json)?;
                        let (Some(k), Some(value)) =
                            (self.heap.element(pair, 0), self.heap.element(pair, 1))
                        else {
                            return Err(not_json());
                        };
                        todo.push(Step::Value(value));
                        todo.push(Step::Text(b":"));
                        todo.push(Step::Key(k));
                    } else {
                        todo.push(Step::Value(item));
                    }
                    if i > 0 {
                        todo.push(Step::Text(b","));
                    }
                }
            } else {
                return Err(not_json());
            }
        }
        Ok(out)
    }

    fn http_types(&self) -> Result<HttpTypes, Stop> {
        self.code.abi.http.ok_or(Stop::broken(Broken::NoHttpTypes))
    }

    /// The bytes of the binary `v`, the last one padded with zero bits.
    fn bytes_of(&self, v: Value) -> Result<Vec<u8>, Stop> {
        Ok(binary::bytes(&self.heap, self.binary(v)?))
    }

    /// The byte offset `v` into `buf`, clamped to the buffer as `h1`'s
    /// offsets are.
    fn offset(&self, v: Value, buf: Bits) -> Result<usize, Stop> {
        let whole = usize::try_from(buf.len / 8).unwrap_or(usize::MAX);
        Ok(match self.int_of(v)? {
            Int::Small(n) => usize::try_from(n).unwrap_or(0).min(whole),
            Int::Big(n) if n.sign() == Sign::Minus => 0,
            Int::Big(_) => whole,
        })
    }

    /// The name and value of each `Header` in the `Headers` array `v`.
    fn headers(&self, v: Value) -> Result<Vec<(Value, Value)>, Stop> {
        let types = self.http_types()?;
        let not_headers = || Stop::broken(Broken::NotHeaders(v.view()));
        let items = match self.seq(v)? {
            Seq::Tree(t) => array::elements(&self.heap, t),
            Seq::Range { start, end } if start >= end => Vec::new(),
            Seq::Range { .. } => return Err(not_headers()),
        };
        let mut out = Vec::with_capacity(items.len());
        for h in items {
            let cell = h.as_cell().ok_or_else(not_headers)?;
            if self.heap.kind(cell) != Some(Kind::Ctor) || self.heap.variant(cell) != types.header {
                return Err(not_headers());
            }
            let (Some(name), Some(value)) = (self.heap.field(cell, 0), self.heap.field(cell, 1))
            else {
                return Err(not_headers());
            };
            out.push((name, value));
        }
        Ok(out)
    }

    /// Bytes `range` of the head at byte `off` of `buf`, as a slice of it.
    fn view(
        &mut self,
        buf: Bits,
        off: usize,
        range: &std::ops::Range<usize>,
    ) -> Result<Value, Stop> {
        // A position past what a word counts in bits is past any binary.
        let bits = |n: Option<usize>| n.and_then(binary::bit_len).ok_or(Stop::HeapFull);
        let from = bits(off.checked_add(range.start))?;
        let len = bits(Some(range.len()))?;
        let cell = binary::slice(&mut self.heap, buf, from, len).map_err(fault)?;
        Ok(Value::cell(cell))
    }

    /// A header block's fields, as `Headers` of slices of `buf`.
    fn fields(&mut self, buf: Bits, off: usize, block: &http::Block) -> Result<Value, Stop> {
        let types = self.http_types()?;
        let mut items = Vec::with_capacity(block.fields.len());
        for f in &block.fields {
            let name = self.view(buf, off, &f.name)?;
            let value = self.view(buf, off, &f.value)?;
            items.push(Value::cell(
                self.heap
                    .ctor(types.header, &[name, value])
                    .map_err(fault)?,
            ));
        }
        Ok(Value::cell(
            array::from_values(&mut self.heap, &items).map_err(fault)?,
        ))
    }

    /// The `HeadFlags` a block's tokens make.
    fn head_flags(&mut self, flags: http::HeadFlags) -> Result<Value, Stop> {
        let types = self.http_types()?;
        let conn = match flags.conn {
            http::ConnTokens::Neither => types.conn_neither,
            http::ConnTokens::Close => types.conn_close,
            http::ConnTokens::KeepAlive => types.conn_keep_alive,
            http::ConnTokens::Both => types.conn_both,
        };
        let fields = [Value::nullary(conn), Value::bool(flags.expect_100_continue)];
        Ok(Value::cell(
            self.heap.ctor(types.head_flags, &fields).map_err(fault)?,
        ))
    }

    /// `variant(status)`, which is a refusal carrying the HTTP status it
    /// answers with.
    fn with_status(&mut self, variant: VariantRef, r: http::Reject) -> Result<Value, Stop> {
        let status = Value::int(r as i64).unwrap_or(Value::NIL);
        Ok(Value::cell(
            self.heap.ctor(variant, &[status]).map_err(fault)?,
        ))
    }

    /// The offset `off + n` into `buf`, as an Int.
    fn consumed(&mut self, off: usize, n: usize) -> Result<Value, Stop> {
        // Both are offsets into one buffer, so their sum is exact.
        bigint::int(&mut self.heap, off.saturating_add(n)).map_err(fault)
    }

    /// `h1.Parsed`, for the request head at byte `off` of `buf`, whose bytes
    /// from there are `window`.
    fn parsed_request(&mut self, buf: Bits, off: usize, window: &[u8]) -> Result<Value, Stop> {
        let types = self.http_types()?;
        match http::request(window) {
            http::Request::NeedMore => Ok(Value::nullary(types.parsed_need_more)),
            http::Request::Bad(r) => self.with_status(types.parsed_bad, r),
            http::Request::Done {
                method,
                target,
                http11,
                head,
            } => {
                let method = self.view(buf, off, &method)?;
                let target = self.view(buf, off, &target)?;
                let version = Value::nullary(if http11 { types.http11 } else { types.http10 });
                let headers = self.fields(buf, off, &head)?;
                let flags = self.head_flags(head.flags)?;
                let consumed = self.consumed(off, head.end)?;
                let fields = [method, target, version, headers, flags, consumed];
                Ok(Value::cell(
                    self.heap.ctor(types.parsed_done, &fields).map_err(fault)?,
                ))
            }
        }
    }

    /// `h1.ParsedResponse`, as [`Self::parsed_request`].
    fn parsed_response(&mut self, buf: Bits, off: usize, window: &[u8]) -> Result<Value, Stop> {
        let types = self.http_types()?;
        let bad = |why: http::BadResponse| match why {
            http::BadResponse::StatusLine => types.bad_status_line,
            http::BadResponse::Version => types.bad_version,
            http::BadResponse::Field => types.bad_field,
            http::BadResponse::TooLarge => types.head_too_large,
        };
        match http::response(window) {
            http::Response::NeedMore => Ok(Value::nullary(types.response_need_more)),
            http::Response::Bad(why) => {
                let why = Value::nullary(bad(why));
                Ok(Value::cell(
                    self.heap.ctor(types.response_bad, &[why]).map_err(fault)?,
                ))
            }
            http::Response::Done {
                http11,
                code,
                reason,
                head,
            } => {
                let version = Value::nullary(if http11 { types.http11 } else { types.http10 });
                let code = Value::int(i64::from(code)).unwrap_or(Value::NIL);
                let reason = self.view(buf, off, &reason)?;
                let headers = self.fields(buf, off, &head)?;
                let flags = self.head_flags(head.flags)?;
                let consumed = self.consumed(off, head.end)?;
                let fields = [version, code, reason, headers, flags, consumed];
                Ok(Value::cell(
                    self.heap
                        .ctor(types.response_done, &fields)
                        .map_err(fault)?,
                ))
            }
        }
    }

    /// `h1.ChunkBody`, for the chunked body at byte `off` of `buf`, whose
    /// bytes from there are `bytes`. The body is copied out once, when all of
    /// it has arrived.
    fn chunk_body(&mut self, buf: Bits, off: usize, bytes: &[u8], max: u64) -> Result<Value, Stop> {
        let types = self.http_types()?;
        match http::chunked(bytes, max) {
            http::Chunked::NeedMore => Ok(Value::nullary(types.chunked_need_more)),
            http::Chunked::Bad(r) => self.with_status(types.chunked_bad, r),
            http::Chunked::Done { pieces, trailers } => {
                let mut body = Vec::with_capacity(pieces.iter().map(|p| p.len()).sum());
                for p in &pieces {
                    body.extend_from_slice(bytes.get(p.clone()).unwrap_or(&[]));
                }
                let body = binary::from_bytes(&mut self.heap, &body);
                let body = Value::cell(body.map_err(fault)?);
                let trailer_fields = self.fields(buf, off, &trailers)?;
                let consumed = self.consumed(off, trailers.end)?;
                let fields = [body, trailer_fields, consumed];
                Ok(Value::cell(
                    self.heap.ctor(types.chunked_done, &fields).map_err(fault)?,
                ))
            }
        }
    }

    /// The bytes of the binary `v`, or `None` when it is not whole bytes.
    fn whole_bytes(&self, v: Value) -> Result<Option<Vec<u8>>, Stop> {
        let b = self.binary(v)?;
        Ok((b.len % 8 == 0).then(|| binary::bytes(&self.heap, b)))
    }

    /// `Ok` of a new binary holding `bytes`.
    fn ok_bytes(&mut self, bytes: &[u8]) -> Result<Value, Stop> {
        let cell = binary::from_bytes(&mut self.heap, bytes).map_err(fault)?;
        self.ok(Value::cell(cell))
    }

    /// The map `v` is.
    pub(super) fn map(&self, v: Value) -> Result<Cell, Stop> {
        v.as_cell()
            .filter(|&cell| self.heap.kind(cell) == Some(Kind::Map))
            .ok_or_else(|| Stop::broken(Broken::NotAMap(v.view())))
    }

    /// The array `v` holds, as a tree or a range.
    #[inline(never)]
    pub(super) fn seq(&self, v: Value) -> Result<Seq, Stop> {
        v.as_cell()
            .and_then(|cell| array::seq(&self.heap, cell))
            .ok_or_else(|| Stop::broken(Broken::NotAnArray(v.view())))
    }

    /// Element `i` of `s`, holding its own reference, or `None` past the end.
    /// A range's element is made from its start.
    #[inline(never)]
    pub(super) fn element(&mut self, s: Seq, i: usize) -> Result<Option<Value>, Stop> {
        match s {
            Seq::Tree(a) => match array::get(&self.heap, a, i) {
                Some(v) => Ok(Some(self.share(v)?)),
                None => Ok(None),
            },
            Seq::Range { start, end } if (i as u64) < array::range_len(start, end) => {
                // An i128 holds any i64 plus any index, so this is exact.
                let n = i128::from(start).saturating_add(i as i128);
                Ok(Some(bigint::int(&mut self.heap, n).map_err(fault)?))
            }
            Seq::Range { .. } => Ok(None),
        }
    }

    /// `s` as a tree, holding one reference. That is the tree itself, or a
    /// range's elements built into one.
    #[inline(never)]
    pub(super) fn tree(&mut self, s: Seq) -> Result<crate::heap::Cell, Stop> {
        match s {
            Seq::Tree(a) => {
                self.heap.share(a)?;
                Ok(a)
            }
            Seq::Range { start, end } => {
                let mut items = Vec::new();
                for n in start..end.max(start) {
                    items.push(bigint::int(&mut self.heap, n).map_err(fault)?);
                }
                array::from_values(&mut self.heap, &items).map_err(fault)
            }
        }
    }

    /// Element `index` of the array `a`, holding its own reference, or `None`
    /// when it has none.
    #[inline(never)]
    pub(super) fn index(&mut self, a: Value, index: Value) -> Result<Option<Value>, Stop> {
        let s = self.seq(a)?;
        match self.position(index)? {
            Some(i) => self.element(s, i),
            None => Ok(None),
        }
    }

    /// A range's end as an `i64`. A range past 64 bits could not be walked in
    /// any time a program has.
    #[inline(never)]
    fn range_end(&self, v: Value) -> Result<i64, Stop> {
        match v.view() {
            View::Int(n) => Ok(n),
            View::Cell(cell) if self.heap.kind(cell) == Some(Kind::BigInt) => {
                num_traits::ToPrimitive::to_i64(&self.heap.read_big_int(cell))
                    .ok_or(Stop::missing(Missing::BigRangeEnd))
            }
            v @ (View::Float(_)
            | View::Nil
            | View::Bool(_)
            | View::Func(_)
            | View::Cell(_)
            | View::Nullary(_)) => Err(Stop::broken(Broken::RangeEnd(v))),
        }
    }

    /// The Int `v` as a position in an array, or `None` when it names none.
    /// That is when it is negative, or a big int, which is past the end of
    /// any array.
    #[inline(never)]
    pub(super) fn position(&self, v: Value) -> Result<Option<usize>, Stop> {
        match v.view() {
            View::Int(i) => Ok(usize::try_from(i).ok()),
            View::Cell(cell) if self.heap.kind(cell) == Some(Kind::BigInt) => Ok(None),
            v @ (View::Float(_)
            | View::Nil
            | View::Bool(_)
            | View::Func(_)
            | View::Cell(_)
            | View::Nullary(_)) => Err(Stop::broken(Broken::Position(v))),
        }
    }

    /// `a` with `items`, in their order, at `end`. The references of `a` and
    /// of each item pass to the result, so an array nothing else holds can
    /// change in place.
    #[inline(never)]
    pub(super) fn push_all(
        &mut self,
        a: crate::heap::Cell,
        items: &[Value],
        end: End,
    ) -> Result<crate::heap::Cell, Stop> {
        let made = match items {
            [x] => array::push_owned(&mut self.heap, a, *x, end),
            _ => array::extend_owned(&mut self.heap, a, items, end),
        };
        made.map_err(fault)
    }

    #[inline]
    pub(super) fn is_string(&self, v: Value) -> bool {
        v.as_cell()
            .is_some_and(|c| self.heap.kind(c) == Some(Kind::String))
    }

    pub(super) fn is_variant(&self, v: Value, variant: VariantRef) -> bool {
        match v.view() {
            View::Nullary(n) => n == variant,
            View::Cell(cell) => {
                self.heap.kind(cell) == Some(Kind::Ctor) && self.heap.variant(cell) == variant
            }
            View::Float(_) | View::Int(_) | View::Nil | View::Bool(_) | View::Func(_) => false,
        }
    }

    pub(super) fn is_int(&self, v: Value, n: i64) -> bool {
        match v.small_int() {
            Some(m) => m == n,
            None => self.is_big_int(v, n),
        }
    }

    #[inline(never)]
    fn is_big_int(&self, v: Value, n: i64) -> bool {
        match v.as_cell() {
            Some(cell) if self.heap.kind(cell) == Some(Kind::BigInt) => {
                self.heap.read_big_int(cell) == n.into()
            }
            _ => false,
        }
    }

    /// `a op b` for an operator whose operands are both Ints, both Floats, or,
    /// for `+`, both strings.
    #[inline(never)]
    pub(super) fn num(&mut self, op: NumOp, a: Value, b: Value) -> Result<Value, Stop> {
        if let (View::Float(x), View::Float(y)) = (a.view(), b.view()) {
            return Ok(match op {
                NumOp::Arith(op) => float::op(op, x, y),
                NumOp::Order(op) => Value::bool(float::compare(op, x, y)),
            });
        }
        if op == NumOp::Arith(Arith::Add) && self.is_string(a) && self.is_string(b) {
            let mut text = Vec::new();
            for s in [a, b] {
                if let Some(cell) = s.as_cell() {
                    self.heap.read_string(cell, &mut text);
                }
            }
            return Ok(Value::cell(self.heap.string(&text).map_err(fault)?));
        }
        match op {
            NumOp::Arith(op) => int::op(&mut self.heap, op, a, b),
            NumOp::Order(op) => {
                let holds = int::compare(&self.heap, Compare::Order(op), a, b)?;
                Ok(Value::bool(holds))
            }
        }
    }

    /// `-a`, for an Int or a Float.
    #[inline(never)]
    pub(super) fn neg(&mut self, a: Value) -> Result<Value, Stop> {
        match a.view() {
            View::Float(f) => Ok(Value::float(-f)),
            View::Int(_)
            | View::Nil
            | View::Bool(_)
            | View::Func(_)
            | View::Cell(_)
            | View::Nullary(_) => int::neg(&mut self.heap, a),
        }
    }

    pub(super) fn int_of(&self, v: Value) -> Result<Int, Stop> {
        int::read(&self.heap, v)
    }

    /// How `println` and `${x}` show a value.
    #[inline(never)]
    pub(super) fn show(&self, v: Value, out: &mut Vec<u8>) -> Result<(), Stop> {
        show::show(&self.heap, self.code, v, out)
    }

    /// The big Int constant `id`, which is `n`.
    #[inline(never)]
    pub(super) fn big_const(&mut self, id: ConstId, n: i64) -> Result<Value, Stop> {
        if let Some(cell) = self.heap.constant(id) {
            return Ok(Value::cell(cell));
        }
        let v = bigint::int(&mut self.heap, n).map_err(fault)?;
        if let Some(cell) = v.as_cell() {
            self.heap.keep_constant(id, cell).map_err(fault)?;
        }
        Ok(v)
    }

    /// The string constant `text`, in a new cell.
    #[inline(always)]
    pub(super) fn text_const(&mut self, text: TextId) -> Result<Value, Stop> {
        let text = self.code.text(text)?;
        Ok(Value::cell(self.heap.string(text).map_err(fault)?))
    }

    /// The binary constant `bits`, in a new cell.
    #[inline(never)]
    pub(super) fn binary_const(&mut self, bits: BinaryId) -> Result<Value, Stop> {
        let b = self.code.binary(bits)?;
        let cell = binary::make(&mut self.heap, &b.bytes, b.len).map_err(fault)?;
        Ok(Value::cell(cell))
    }

    /// Whether `v` is the string constant `text`.
    #[inline(never)]
    pub(super) fn is_text(&self, v: Value, text: TextId) -> Result<bool, Stop> {
        let text = self.code.text(text)?;
        Ok(match v.as_cell() {
            Some(cell) if self.is_string(v) => self.heap.string_is(cell, text),
            _ => false,
        })
    }

    /// Whether `v` is the binary constant `bits`.
    #[inline(never)]
    pub(super) fn is_binary(&self, v: Value, bits: BinaryId) -> Result<bool, Stop> {
        let want = self.code.binary(bits)?;
        let is = v.as_cell().and_then(|c| binary::bits(&self.heap, c));
        Ok(is.is_some_and(|b| {
            b.len == want.len && binary::bytes(&self.heap, b) == want.bytes.as_ref()
        }))
    }

    /// Write what `v` shows to the output, and a newline after it if
    /// `newline`.
    #[inline(never)]
    pub(super) fn print(&mut self, v: Value, newline: bool) -> Result<(), Stop> {
        let mut text = Vec::new();
        self.show(v, &mut text)?;
        if newline {
            text.push(b'\n');
        }
        self.host.print(&text).map_err(|_| Stop::OutputClosed)
    }

    /// A new string of the text `${v}` shows.
    #[inline(always)]
    pub(super) fn text_shown(&mut self, v: Value) -> Result<Value, Stop> {
        let made = match v.view() {
            // As `show` writes one, without its list of what is left to show.
            View::Int(n) => self.heap.string(n.to_string().as_bytes()),
            View::Float(_)
            | View::Nil
            | View::Bool(_)
            | View::Func(_)
            | View::Cell(_)
            | View::Nullary(_) => {
                let mut text = Vec::new();
                self.show(v, &mut text)?;
                self.heap.string(&text)
            }
        };
        Ok(Value::cell(made.map_err(fault)?))
    }

    /// The strings in `parts` of `frame`, joined in order.
    #[inline(always)]
    pub(super) fn concat(&mut self, frame: &[Value; WINDOW], parts: &[Reg]) -> Result<Value, Stop> {
        let mut text = Vec::new();
        for r in parts {
            let v = frame.get(usize::from(r.0));
            let v = v.copied().unwrap_or(Value::NIL);
            match v.as_cell() {
                Some(cell) if self.heap.kind(cell) == Some(Kind::String) => {
                    self.heap.read_string(cell, &mut text);
                }
                _ => return Err(Stop::missing(Missing::Join(v.view()))),
            }
        }
        Ok(Value::cell(self.heap.string(&text).map_err(fault)?))
    }

    /// The function value `f` runs, and what a frame for it has in its closure
    /// register: `f`, with one more reference, when the function keeps the
    /// closure it runs as.
    #[inline(never)]
    pub(super) fn callee(&mut self, f: Value, argc: usize) -> Result<(&'c Entry, Value), Stop> {
        let func = match f.view() {
            View::Func(func) => func,
            View::Cell(cell) if self.heap.kind(cell) == Some(Kind::Closure) => {
                self.heap.closure_func(cell)
            }
            v @ (View::Float(_)
            | View::Int(_)
            | View::Nil
            | View::Bool(_)
            | View::Cell(_)
            | View::Nullary(_)) => {
                return Err(Stop::broken(Broken::NotAFunction(v)));
            }
        };
        let entry = self.code.funcs.get(func);
        let entry = entry.ok_or_else(|| no_func(func))?;
        if entry.params != argc {
            return Err(wrong_arity(entry, argc));
        }
        let closure = if entry.closure.keeps() {
            self.share(f)?
        } else {
            Value::NIL
        };
        Ok((entry, closure))
    }

    /// `start..end`.
    #[inline(never)]
    pub(super) fn range(&mut self, start: Value, end: Value) -> Result<Value, Stop> {
        let (start, end) = (self.range_end(start)?, self.range_end(end)?);
        let cell = array::range(&mut self.heap, start, end).map_err(fault)?;
        Ok(Value::cell(cell))
    }

    /// How many elements the array `v` has.
    #[inline(never)]
    pub(super) fn array_len(&mut self, v: Value) -> Result<Value, Stop> {
        let n = self.seq(v)?.len(&self.heap);
        bigint::int(&mut self.heap, n).map_err(fault)
    }

    /// `[..a, ..b]`.
    #[inline(never)]
    pub(super) fn array_concat(&mut self, a: Value, b: Value) -> Result<Value, Stop> {
        let (a, b) = (self.seq(a)?, self.seq(b)?);
        let (a, b) = (self.tree(a)?, self.tree(b)?);
        let cell = array::concat(&mut self.heap, a, b).map_err(fault);
        self.heap.release(a)?;
        self.heap.release(b)?;
        Ok(Value::cell(cell?))
    }

    /// `of[start..end]`, as `Ok` of the elements there or `Err(Nil)`.
    #[inline(never)]
    pub(super) fn array_slice(
        &mut self,
        of: Value,
        start: Value,
        end: Value,
    ) -> Result<Value, Stop> {
        let s = self.seq(of)?;
        let bounds = (self.position(start)?, self.position(end)?);
        let cut = match (s, bounds) {
            (Seq::Tree(a), (Some(from), Some(to))) => {
                array::slice(&mut self.heap, a, from, to).map_err(fault)?
            }
            (Seq::Range { start, end }, (Some(from), Some(to)))
                if from <= to && to as u64 <= array::range_len(start, end) =>
            {
                // Both are at most the range's length, so each sum lands
                // within `start..=end` and the add is exact.
                let (from, to) = (
                    start.wrapping_add_unsigned(from as u64),
                    start.wrapping_add_unsigned(to as u64),
                );
                Some(array::range(&mut self.heap, from, to).map_err(fault)?)
            }
            (Seq::Tree(_) | Seq::Range { .. }, _) => None,
        };
        match cut {
            Some(cut) => self.ok(Value::cell(cut)),
            None => self.err_nil(),
        }
    }

    /// `v` with one more reference, for when it goes somewhere new.
    #[inline]
    pub(super) fn share(&mut self, v: Value) -> Result<Value, DeadCell> {
        self.heap.own(v)
    }

    #[inline]
    pub(super) fn release(&mut self, v: Value) -> Result<(), DeadCell> {
        match v.as_cell() {
            Some(cell) => self.heap.release(cell),
            None => Ok(()),
        }
    }
}

/// The stop for a call that passes `entry` `argc` arguments, which is not how
/// many it takes.
#[cold]
#[inline(never)]
fn wrong_arity(entry: &Entry, argc: usize) -> Stop {
    Stop::broken(Broken::CallArity {
        func: entry.name.clone(),
        got: argc,
        want: entry.params,
    })
}

/// The stop for a function value that names no function of the program's.
#[cold]
#[inline(never)]
fn no_func(func: FuncIdx) -> Stop {
    Stop::missing(Missing::NoFunc(func))
}

/// `x` shifted `count` bits, left (times 2^count, exactly) or right (divided
/// by 2^count, rounded down). A left shift past what an Int can hold is a
/// full heap. A right shift past every bit leaves 0 or -1, by `x`'s sign.
#[expect(
    clippy::arithmetic_side_effects,
    reason = "an Int grows instead of overflowing, and its size is checked first"
)]
fn shift(
    x: num_bigint::BigInt,
    left: bool,
    count: &num_bigint::BigInt,
) -> Result<num_bigint::BigInt, Stop> {
    if x.sign() == Sign::NoSign {
        return Ok(x);
    }
    let Some(count) = count.to_u64() else {
        return if left {
            Err(Stop::HeapFull)
        } else {
            Ok(if x.sign() == Sign::Minus {
                (-1).into()
            } else {
                0.into()
            })
        };
    };
    if left {
        if x.bits().saturating_add(count) > bigint::MAX_BITS {
            return Err(Stop::HeapFull);
        }
        Ok(x << count)
    } else if count > x.bits() {
        Ok(if x.sign() == Sign::Minus {
            (-1).into()
        } else {
            0.into()
        })
    } else {
        Ok(x >> count)
    }
}

/// A `scarlet/json.Doc` taken apart into the binaries it holds, borrowed, and
/// the node it points at.
struct Doc {
    arena: Value,
    tape: Value,
    idx: usize,
}
