//! JSON parsing and encoding.
//!
//! The parse is SIMD (`simd-json`, the Rust port of simdjson, with runtime
//! CPU-feature detection so one binary serves SSE4.2/AVX2 and NEON). What it
//! produces is not a Scarlet value tree: it is a **compact tape** — a flat
//! array of fixed-width nodes in document order — plus a string arena, both
//! handed to Scarlet as `Binary`s inside an opaque `Doc`.
//!
//! That indirection is the whole point. Materialising a Scarlet value per JSON
//! node would put an allocation and a string copy on every node of the
//! document, whether or not the caller ever looks at it, and the parser's
//! speed would disappear into the boundary. With a tape, `json.field` is a
//! bounded walk over 16-byte records and a caller that reads 6 fields out of a
//! 400-field payload pays for 6 strings.
//!
//! Nothing here recurses. `simd-json` accepts nesting thousands deep (measured:
//! 10,000 nested arrays parse fine), so a recursive tape walk or encoder would
//! be a stack overflow reachable from any caller that parses untrusted input.
//! The tape is built in one linear pass and the encoder carries its own work
//! stack.
//!
//! ## Tape format
//!
//! One node is two little-endian `u64` words, 16 bytes:
//!
//! ```text
//! word0: bits 0..8   kind
//!        bits 32..64 skip — nodes in this subtree, including this one
//! word1: payload (see `K_*`)
//! ```
//!
//! Children of a container follow it immediately, so element `i` is found by
//! adding `skip` `i` times. An object's members are laid out key, value, key,
//! value…; keys are always `K_STR`, whose `skip` is 1.
//!
//! Every read is bounds-checked against the tape and arena lengths. A `Doc`
//! can only be built by `json_parse`, but the checks are cheap and mean a
//! malformed tape is `None` rather than an out-of-bounds read.

use simd_json::StaticNode;
use simd_json::value::tape::Node;

use crate::abi::AbiSlot;
use crate::bytecode::{Value, seq};

use super::{VM, VmResult, bin_ref, str_ref};

// Tape node kinds. `K_UINT_BIG` is a JSON integer that is a valid `u64` but
// larger than `i64::MAX`: Scarlet's `Int` is exactly `i64` (the `BigInt` box
// holds one `i64`, it is not arbitrary precision), so the value cannot be
// represented. It is kept distinct rather than truncated or silently widened
// to a float — `json.int` reports it as out of range and the caller finds out.
const K_NULL: u8 = 0;
const K_BOOL: u8 = 1;
const K_INT: u8 = 2;
const K_FLOAT: u8 = 3;
const K_STR: u8 = 4;
const K_ARRAY: u8 = 5;
const K_OBJECT: u8 = 6;
const K_UINT_BIG: u8 = 7;

/// The `Kind` ordinals `scarlet/json` maps to its `Kind` variants. `K_UINT_BIG`
/// reports as `Int`, because that is what it is; only its value is out of range.
const KIND_NULL: i64 = 0;
const KIND_BOOL: i64 = 1;
const KIND_INT: i64 = 2;
const KIND_FLOAT: i64 = 3;
const KIND_STRING: i64 = 4;
const KIND_ARRAY: i64 = 5;
const KIND_OBJECT: i64 = 6;

const NODE_BYTES: usize = 16;

/// A decoded tape node.
#[derive(Clone, Copy)]
struct TapeNode {
    kind: u8,
    /// Nodes in this subtree including this one; always >= 1.
    skip: usize,
    payload: u64,
}

/// Where a `K_STR` node's bytes are in the arena.
struct ArenaSpan {
    off: usize,
    len: usize,
}

impl TapeNode {
    fn str_span(self) -> ArenaSpan {
        ArenaSpan {
            off: (self.payload >> 32) as usize,
            len: (self.payload & 0xffff_ffff) as usize,
        }
    }
}

/// Read node `i`. `None` if `i` is past the end or the record is truncated.
fn node_at(tape: &[u8], i: usize) -> Option<TapeNode> {
    let off = i.checked_mul(NODE_BYTES)?;
    let rec = tape.get(off..off.checked_add(NODE_BYTES)?)?;
    let w0 = u64::from_le_bytes(rec[0..8].try_into().ok()?);
    let payload = u64::from_le_bytes(rec[8..16].try_into().ok()?);
    let skip = (w0 >> 32) as usize;
    // A zero skip would let a container walk stand still and loop forever.
    if skip == 0 {
        return None;
    }
    Some(TapeNode {
        kind: w0 as u8,
        skip,
        payload,
    })
}

/// The `Kind` ordinal of the node at `idx`, or `None` for a tape that does not
/// decode. The `json.kind` builtin and the `inspect` image both read it, so
/// the two can never disagree about what a node is.
fn kind_ordinal(tape: &[u8], idx: usize) -> Option<i64> {
    Some(match node_at(tape, idx)?.kind {
        K_NULL => KIND_NULL,
        K_BOOL => KIND_BOOL,
        K_INT | K_UINT_BIG => KIND_INT,
        K_FLOAT => KIND_FLOAT,
        K_STR => KIND_STRING,
        K_ARRAY => KIND_ARRAY,
        K_OBJECT => KIND_OBJECT,
        _ => return None,
    })
}

fn kind_name(ordinal: i64) -> &'static str {
    match ordinal {
        KIND_NULL => "null",
        KIND_BOOL => "bool",
        KIND_INT => "int",
        KIND_FLOAT => "float",
        KIND_STRING => "string",
        KIND_ARRAY => "array",
        KIND_OBJECT => "object",
        // `kind_ordinal` yields only the seven above. The remaining case is a
        // tape that does not decode, which `json.kind` reports as -1.
        _ => "invalid",
    }
}

/// What a `Doc` cursor points at and where, for `inspect`.
///
/// Never the document's contents. A `Doc` carries the whole string arena —
/// every key and every string value, for a body that may be megabytes — and
/// printing one is the first thing a caller reaches for while debugging, so
/// this image stays bounded however large the document is.
fn doc_image(tape: &[u8], idx: usize) -> String {
    format!(
        "<json {}#{}>",
        kind_name(kind_ordinal(tape, idx).unwrap_or(-1)),
        idx
    )
}

/// The `inspect` image of a `Doc` value. Total: a printer must not panic, so a
/// payload that is not the shape the parser builds still renders bounded.
pub(super) fn inspect_doc(doc: &Value) -> String {
    (|| {
        let (_, tape_v, idx) = VM::doc_parts(doc)?;
        let bin = tape_v.as_binary()?;
        Some(doc_image(&bin.full_bytes(), idx))
    })()
    .unwrap_or_else(|| "<json invalid>".to_string())
}

/// The bytes of a `K_STR` node. `None` if the span escapes the arena.
fn str_bytes(arena: &[u8], n: TapeNode) -> Option<&[u8]> {
    let ArenaSpan { off, len } = n.str_span();
    arena.get(off..off.checked_add(len)?)
}

/// Tape index of the value of the first member of the object at `idx` whose
/// key is `needle`. `None` if `idx` is not an object, if the object has no
/// such member, or if any node the walk touches is out of range.
fn field_index(tape: &[u8], arena: &[u8], idx: usize, needle: &[u8]) -> Option<usize> {
    let obj = node_at(tape, idx)?;
    if obj.kind != K_OBJECT {
        return None;
    }
    let members = usize::try_from(obj.payload).ok()?;
    let mut cursor = idx.checked_add(1)?;
    for _ in 0..members {
        let key = node_at(tape, cursor)?;
        if key.kind != K_STR {
            return None;
        }
        let value_at = cursor.checked_add(key.skip)?;
        // Read the value before the key is compared, so a matching key cannot
        // hand back an index the advancing arm would have refused.
        let value = node_at(tape, value_at)?;
        if str_bytes(arena, key)? == needle {
            return Some(value_at);
        }
        cursor = value_at.checked_add(value.skip)?;
    }
    None
}

/// Tape index of element `want` of the array at `idx`. `None` if `idx` is not
/// an array, if it has no such element, or if any node the walk touches —
/// including the element itself — is out of range.
fn element_index(tape: &[u8], idx: usize, want: usize) -> Option<usize> {
    let arr = node_at(tape, idx)?;
    if arr.kind != K_ARRAY || want as u64 >= arr.payload {
        return None;
    }
    let mut cursor = idx.checked_add(1)?;
    for _ in 0..want {
        cursor = cursor.checked_add(node_at(tape, cursor)?.skip)?;
    }
    // The walk reads every node it steps over but not the one it stops on, and
    // for `want == 0` it steps nowhere. `payload` is read off the tape, so on a
    // forged tape it is the only remaining guard and it bounds nothing.
    node_at(tape, cursor)?;
    Some(cursor)
}

/// Tape index of every element of the array at `idx`, in document order —
/// `json_elements`' walk, and the number of `node_at` reads it took.
///
/// `None` if `idx` is not an array or the walk runs off the tape.
///
/// The read count exists for the same reason `scan_for_lone_surrogate`'s
/// does (T-173): no assertion about the *positions* can tell this walk,
/// which advances `cursor` by `skip` and never revisits a node, from one
/// that calls [`element_index`] once per element and restarts at `idx`
/// every time — both produce the identical `Vec<usize>` for every input.
/// The second shape is `element_index`'s own cost, `want`, summed over
/// `want in 0..count`: `O(n^2)`. The read count is the smallest thing that
/// tells them apart.
fn collect_element_positions(tape: &[u8], idx: usize) -> Option<(Vec<usize>, usize)> {
    let arr = node_at(tape, idx)?;
    if arr.kind != K_ARRAY {
        return None;
    }
    let count = usize::try_from(arr.payload).ok()?;
    let mut out = Vec::with_capacity(count);
    let mut cursor = idx.checked_add(1)?;
    let mut reads = 0usize;
    for _ in 0..count {
        // Every element is bounds-checked as it is reached, so a walk that
        // runs off the tape yields the empty array rather than a truncated
        // one.
        reads += 1;
        node_at(tape, cursor)?;
        out.push(cursor);
        reads += 1;
        cursor = cursor.checked_add(node_at(tape, cursor)?.skip)?;
    }
    Some((out, reads))
}

/// Build the compact tape and the string arena from a parsed `simd-json` tape.
///
/// One linear pass: `simd-json` already emits nodes in document order with a
/// subtree node count, so the skip field is a field copy rather than a walk.
/// Strings are copied into an arena instead of being pointed at inside the
/// mutable input buffer — the input buffer is dropped when this returns, and
/// resolving `&str`s back to offsets in it would depend on `simd-json`
/// unescaping in place, which is an implementation detail rather than a
/// contract.
///
/// `Err` carries a message for a document too large for the 32-bit offset
/// fields. Both bounds are ~4 billion; a document that reaches one has already
/// exhausted memory, but they must not wrap silently.
fn build_tape(nodes: &[Node<'_>]) -> Result<(Vec<u8>, Vec<u8>), &'static str> {
    if nodes.len() > u32::MAX as usize {
        return Err("document has too many JSON nodes to index");
    }
    let mut tape = Vec::with_capacity(nodes.len() * NODE_BYTES);
    let mut arena: Vec<u8> = Vec::new();
    for n in nodes {
        let (kind, skip, payload): (u8, u64, u64) = match n {
            Node::String(s) => {
                let off = arena.len();
                if off > u32::MAX as usize || s.len() > u32::MAX as usize {
                    return Err("document has too much string data to index");
                }
                arena.extend_from_slice(s.as_bytes());
                (K_STR, 1, ((off as u64) << 32) | s.len() as u64)
            }
            // `count` is the number of nodes after this one that belong to it,
            // so the subtree size is one more. An empty container has count 0.
            Node::Object { len, count } => (K_OBJECT, *count as u64 + 1, *len as u64),
            Node::Array { len, count } => (K_ARRAY, *count as u64 + 1, *len as u64),
            Node::Static(StaticNode::Null) => (K_NULL, 1, 0),
            Node::Static(StaticNode::Bool(b)) => (K_BOOL, 1, u64::from(*b)),
            Node::Static(StaticNode::I64(i)) => (K_INT, 1, i.cast_unsigned()),
            Node::Static(StaticNode::U64(u)) => {
                if *u <= i64::MAX as u64 {
                    (K_INT, 1, *u)
                } else {
                    (K_UINT_BIG, 1, *u)
                }
            }
            Node::Static(StaticNode::F64(f)) => (K_FLOAT, 1, f.to_bits()),
            // No catch-all on purpose. `StaticNode` grows `I128`/`U128`
            // variants when `simd-json`'s `128bit` feature is on, and feature
            // unification can turn that on from another crate in the
            // workspace. A missing arm is then a compile error here, which is
            // what should happen: the alternative is a 128-bit number silently
            // taking a fallback path and arriving wrong.
        };
        tape.extend_from_slice(&(u64::from(kind) | (skip << 32)).to_le_bytes());
        tape.extend_from_slice(&payload.to_le_bytes());
    }
    Ok((tape, arena))
}

/// The byte offset of the first unpaired UTF-16 surrogate escape, if any.
///
/// `simd-json` accepts `"\ud800"` and decodes it to U+0000 rather than
/// rejecting it. That is silent corruption: a lone surrogate becomes a NUL
/// indistinguishable from a legitimate `\x00`, so a caller comparing the
/// decoded string against a token can be fooled and has no way to tell.
/// RFC 8259 §7 requires the pair, and `serde_json` rejects it, so this scan
/// restores the standard answer.
///
/// Runs only after a successful parse, which is what makes the backslash rule
/// sound: every backslash left in the document is inside a string literal and
/// starts a valid escape, so a `u` preceded by an odd number of backslashes is
/// a `\u` escape and nothing else. Documents with no backslash at all cost one
/// `memchr` pass and stop.
fn first_lone_surrogate(bytes: &[u8]) -> Option<usize> {
    scan_for_lone_surrogate(bytes).0
}

/// The scan proper: the answer, and the number of times the outer loop ran.
///
/// The step count exists so the `memchr` skip is observable. Nothing outside
/// the tests reads it, and `first_lone_surrogate` drops it — but no assertion
/// about the *answer* can tell the skip from a byte-at-a-time walk, since both
/// return the same thing for every input. Replacing the skip with `i += 1`
/// left the whole suite green, which meant the guard on the one part of this
/// function that touches every byte of a document was decorative. A step count
/// is the smallest thing that distinguishes them.
fn scan_for_lone_surrogate(bytes: &[u8]) -> (Option<usize>, usize) {
    fn hex4(b: &[u8]) -> Option<u32> {
        let s = std::str::from_utf8(b.get(..4)?).ok()?;
        u32::from_str_radix(s, 16).ok()
    }

    let mut steps = 0usize;
    let mut i = 0usize;
    while i < bytes.len() {
        steps += 1;
        if bytes[i] != b'\\' {
            match memchr::memchr(b'\\', &bytes[i..]) {
                Some(d) => i += d,
                None => return (None, steps),
            }
            continue;
        }
        let esc = i;
        if bytes.get(i + 1) == Some(&b'u') {
            if let Some(hi) = hex4(&bytes[i + 2..]) {
                if (0xd800..=0xdbff).contains(&hi) {
                    // A high surrogate is only legal immediately followed by a
                    // low one. Consume both, so the low half is never later
                    // mistaken for a lone surrogate of its own.
                    let tail = &bytes[i + 6..];
                    let paired = tail.starts_with(b"\\u")
                        && hex4(&tail[2..]).is_some_and(|lo| (0xdc00..=0xdfff).contains(&lo));
                    if paired {
                        i += 12;
                        continue;
                    }
                    return (Some(esc), steps);
                }
                if (0xdc00..=0xdfff).contains(&hi) {
                    return (Some(esc), steps);
                }
            }
            i += 6;
            continue;
        }
        // Any other escape — `\\`, `\"`, `\n`. Consuming both bytes is what
        // stops an escaped backslash from being read as starting an escape.
        i += 2;
    }
    (None, steps)
}

/// Emit `s` as a JSON string literal, escapes and all.
fn write_json_string(out: &mut String, s: &str) {
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{8}' => out.push_str("\\b"),
            '\u{c}' => out.push_str("\\f"),
            c if (c as u32) < 0x20 => {
                out.push_str("\\u");
                for shift in [12, 8, 4, 0] {
                    let nibble = (c as u32 >> shift) & 0xf;
                    out.push(char::from_digit(nibble, 16).unwrap_or('0'));
                }
            }
            c => out.push(c),
        }
    }
    out.push('"');
}

/// Render `f` so it reads back as a JSON number *and* as a float.
///
/// `Display` for `f64` prints `1` for `1.0`, which round-trips through JSON as
/// an integer and changes the value's kind. `Debug` is the same
/// shortest-round-trip algorithm but always writes the point.
///
/// JSON has no NaN or infinity. They are written as `null`, matching
/// `JSON.stringify`; the alternative is an error channel on every encode for a
/// case no correct payload contains.
fn write_json_float(out: &mut String, f: f64) {
    if f.is_finite() {
        out.push_str(&format!("{f:?}"));
    } else {
        out.push_str("null");
    }
}

/// Whether `s` is a JSON number, by RFC 8259's grammar.
///
/// `Json.Number` carries its text straight into the output, because that is
/// how a JSON integer wider than an `Int` survives a re-encode. The text can
/// come from a caller as well as from `to_json`, and nothing in the type says
/// it is a number — Scarlet has no refinement to say it with. So the check is
/// here, at the one place the text becomes output. Without it a single
/// hand-built value would make `encode` emit bytes that are not JSON, which is
/// worse than dropping the value: the whole document stops parsing, at the
/// receiver, for a reason the sender cannot see.
fn is_json_number(s: &str) -> bool {
    let b = s.as_bytes();
    let mut i = 0usize;
    let digits = |b: &[u8], i: &mut usize| {
        let start = *i;
        while matches!(b.get(*i), Some(b'0'..=b'9')) {
            *i += 1;
        }
        *i > start
    };

    if b.get(i) == Some(&b'-') {
        i += 1;
    }
    // `int`: a lone `0`, or a nonzero digit and the rest. A leading zero is
    // not JSON, so `01` fails on the trailing-bytes check below.
    if b.get(i) == Some(&b'0') {
        i += 1;
    } else if !digits(b, &mut i) {
        return false;
    }
    // `frac`: the point must be followed by at least one digit.
    if b.get(i) == Some(&b'.') {
        i += 1;
        if !digits(b, &mut i) {
            return false;
        }
    }
    // `exp`: likewise, and the sign is optional.
    if matches!(b.get(i), Some(b'e' | b'E')) {
        i += 1;
        if matches!(b.get(i), Some(b'+' | b'-')) {
            i += 1;
        }
        if !digits(b, &mut i) {
            return false;
        }
    }
    i == b.len()
}

/// One pending piece of encoder output.
enum Step {
    /// Encode this `scarlet/json.Json` value.
    Val(Value),
    /// A literal byte of punctuation.
    Punct(&'static str),
    /// A Scarlet `String` to emit as a JSON string literal.
    Key(Value),
}

/// Encode the constructible `scarlet/json.Json` tree onto `out`. Iterative:
/// the work stack is heap, so nesting depth costs memory rather than call
/// frames.
///
/// Variants are matched by name, not by declaration index. An index would be
/// an identity the source does not promise — reordering the `Json`
/// constructors would silently re-tag every value.
///
/// Every path emits a value, so a container's separators are never left
/// stranded: a `Json` this encoder does not recognise is written as `null`,
/// the way an unrepresentable `Number` is.
fn write_json_value(out: &mut String, root: Value) {
    let mut stack = vec![Step::Val(root)];

    while let Some(step) = stack.pop() {
        match step {
            Step::Punct(p) => out.push_str(p),
            Step::Key(v) => write_json_string(out, v.as_str().unwrap_or_default()),
            Step::Val(v) => {
                let Some(e) = v.as_enum() else {
                    out.push_str("null");
                    continue;
                };
                let payload = e.payload();
                match e.variant_name() {
                    "Boolean" => out.push_str(match payload.first().and_then(Value::as_bool) {
                        Some(true) => "true",
                        _ => "false",
                    }),
                    "Integer" => {
                        let i = payload.first().and_then(Value::as_int).unwrap_or(0);
                        out.push_str(&i.to_string());
                    }
                    "Real" => {
                        let f = payload.first().and_then(Value::as_float).unwrap_or(0.0);
                        write_json_float(out, f);
                    }
                    // A number with no Scarlet representation, verbatim.
                    // Text that is not a JSON number is written as `null`
                    // rather than corrupting the document around it.
                    "Number" => {
                        let text = payload.first().and_then(Value::as_str).unwrap_or_default();
                        if is_json_number(text) {
                            out.push_str(text);
                        } else {
                            out.push_str("null");
                        }
                    }
                    "Str" => {
                        write_json_string(
                            out,
                            payload.first().and_then(Value::as_str).unwrap_or_default(),
                        );
                    }
                    "List" => {
                        let Some(items) = payload.first() else {
                            out.push_str("[]");
                            continue;
                        };
                        let n = seq::len(items);
                        stack.push(Step::Punct("]"));
                        // Pushed back-to-front: the stack pops in emission
                        // order. A separator is pushed with the element it
                        // precedes, never on its own. `seq::get` is total
                        // below `len`, so `null` here is unreachable today —
                        // it keeps the element count honest if that changes.
                        for i in (0..n).rev() {
                            match seq::get(items, i) {
                                Some(item) => stack.push(Step::Val(item)),
                                None => stack.push(Step::Punct("null")),
                            }
                            if i > 0 {
                                stack.push(Step::Punct(","));
                            }
                        }
                        out.push('[');
                    }
                    "Object" => {
                        let Some(entries) = payload.first() else {
                            out.push_str("{}");
                            continue;
                        };
                        let n = seq::len(entries);
                        // A member that is not a `(String, Json)` pair has no
                        // key to hang a `null` on, so it is dropped whole —
                        // its separator with it. `first` is the surviving
                        // member that takes no separator.
                        let member = |i: usize| {
                            let entry = seq::get(entries, i)?;
                            let Some([k, val]) = entry.as_tuple() else {
                                return None;
                            };
                            Some((k.clone(), val.clone()))
                        };
                        let first = (0..n).find(|&i| member(i).is_some());
                        stack.push(Step::Punct("}"));
                        for i in (0..n).rev() {
                            let Some((k, val)) = member(i) else {
                                continue;
                            };
                            stack.push(Step::Val(val));
                            stack.push(Step::Punct(":"));
                            stack.push(Step::Key(k));
                            if Some(i) != first {
                                stack.push(Step::Punct(","));
                            }
                        }
                        out.push('{');
                    }
                    // `Null`, and any variant a future `Json` gains that
                    // this encoder has not been taught.
                    _ => out.push_str("null"),
                }
            }
        }
    }
}

impl VM {
    /// `[src Binary] -> Result(Doc, ParseError)`
    pub(super) fn json_parse(&mut self) -> VmResult<()> {
        let src_v = self.pop_binary("json.parse")?;
        // `simd-json` parses destructively, so it needs its own mutable copy.
        // This copy is inherent to the algorithm, not to the binding.
        let mut buf = bin_ref(&src_v).full_bytes().into_owned();

        // The surrogate scan reads the input, so it has to happen before
        // `to_tape`, which rewrites the buffer in place. Running it only on a
        // document that parsed would mean keeping a second copy.
        let lone_surrogate = first_lone_surrogate(&buf);

        let built = match simd_json::to_tape(&mut buf) {
            Ok(_) if lone_surrogate.is_some() => Err((
                lone_surrogate.unwrap_or(0),
                "unpaired UTF-16 surrogate escape".to_string(),
            )),
            Ok(tape) => build_tape(&tape.0).map_err(|m| (0usize, m.to_string())),
            Err(e) => Err((e.index(), e.to_string())),
        };

        let v = match built {
            Ok((tape, arena)) => {
                let arena_v = Value::binary_in(&mut self.heap, arena);
                let tape_v = Value::binary_in(&mut self.heap, tape);
                let doc =
                    self.abi_make(AbiSlot::JsonDoc, &[arena_v, tape_v, Value::small_int(0)])?;
                self.make_ok(doc)?
            }
            Err((offset, message)) => {
                let msg = Value::str_in(&mut self.heap, &message);
                let off = Value::int_in(&mut self.heap, i64::try_from(offset).unwrap_or(-1));
                let err = self.abi_make(AbiSlot::JsonParseError, &[off, msg])?;
                self.make_err(err)?
            }
        };
        self.stack.push(v);
        Ok(())
    }

    /// Pull `(arena, tape, idx)` out of a `Doc`, then read the node it points
    /// at. `None` for a tape that does not decode — see the module note on why
    /// every read is checked.
    fn doc_parts(doc: &Value) -> Option<(Value, Value, usize)> {
        let e = doc.as_enum()?;
        let p = e.payload();
        let [arena, tape, idx] = p else { return None };
        let i = idx.as_int()?;
        if i < 0 {
            return None;
        }
        Some((arena.clone(), tape.clone(), i as usize))
    }

    /// Build a `Doc` over the same arena and tape, pointing at node `idx`.
    fn make_doc(&mut self, arena: &Value, tape: &Value, idx: usize) -> VmResult<Value> {
        let idx = Value::int_in(&mut self.heap, idx as i64);
        self.abi_make(AbiSlot::JsonDoc, &[arena.clone(), tape.clone(), idx])
    }

    /// `[d Doc] -> Int` — the `Kind` ordinal, or -1 for an undecodable tape.
    pub(super) fn json_kind(&mut self) -> VmResult<()> {
        let doc = self.pop()?;
        let kind = (|| {
            let (_, tape_v, idx) = Self::doc_parts(&doc)?;
            kind_ordinal(&bin_ref(&tape_v).full_bytes(), idx)
        })()
        .unwrap_or(-1);
        self.stack.push(Value::small_int(kind));
        Ok(())
    }

    /// `[d Doc] -> Int` — element count for an array or object, else -1.
    pub(super) fn json_len(&mut self) -> VmResult<()> {
        let doc = self.pop()?;
        let len = (|| {
            let (_, tape_v, idx) = Self::doc_parts(&doc)?;
            let n = node_at(&bin_ref(&tape_v).full_bytes(), idx)?;
            match n.kind {
                K_ARRAY | K_OBJECT => i64::try_from(n.payload).ok(),
                _ => None,
            }
        })()
        .unwrap_or(-1);
        self.stack.push(Value::small_int(len));
        Ok(())
    }

    /// `[d Doc, name String] -> Option(Doc)`
    ///
    /// The **first** member whose key matches, which is what every JSON
    /// implementation that keeps one value per key does. Duplicate keys are
    /// preserved on the tape and visible through `json.entries`.
    pub(super) fn json_field(&mut self) -> VmResult<()> {
        let name_v = self.pop_str("json.field")?;
        let doc = self.pop()?;

        let found = (|| {
            let (arena_v, tape_v, idx) = Self::doc_parts(&doc)?;
            let tape = bin_ref(&tape_v).full_bytes();
            let arena = bin_ref(&arena_v).full_bytes();
            let needle = str_ref(&name_v).as_bytes();
            let at = field_index(&tape, &arena, idx, needle)?;
            Some((arena_v.clone(), tape_v.clone(), at))
        })();

        let v = match found {
            Some((arena, tape, at)) => {
                let d = self.make_doc(&arena, &tape, at)?;
                self.make_some(d)?
            }
            None => self.make_none()?,
        };
        self.stack.push(v);
        Ok(())
    }

    /// `[d Doc, i Int] -> Option(Doc)`
    pub(super) fn json_index(&mut self) -> VmResult<()> {
        let want = self.pop_int("json.index")?;
        let doc = self.pop()?;

        let found = (|| {
            let (arena_v, tape_v, idx) = Self::doc_parts(&doc)?;
            let tape = bin_ref(&tape_v).full_bytes();
            let want = usize::try_from(want).ok()?;
            let at = element_index(&tape, idx, want)?;
            Some((arena_v.clone(), tape_v.clone(), at))
        })();

        let v = match found {
            Some((arena, tape, at)) => {
                let d = self.make_doc(&arena, &tape, at)?;
                self.make_some(d)?
            }
            None => self.make_none()?,
        };
        self.stack.push(v);
        Ok(())
    }

    /// `[d Doc] -> Array((String, Doc))` — an object's members in document
    /// order, duplicates included. Empty for anything that is not an object.
    pub(super) fn json_entries(&mut self) -> VmResult<()> {
        let doc = self.pop()?;

        // Collect positions first: building the Scarlet values needs `&mut
        // self.heap`, which cannot be held across the borrow of the tape.
        let spans: Vec<(usize, usize, usize)> = (|| {
            let (arena_v, tape_v, idx) = Self::doc_parts(&doc)?;
            let tape = bin_ref(&tape_v).full_bytes();
            let obj = node_at(&tape, idx)?;
            if obj.kind != K_OBJECT {
                return None;
            }
            let members = usize::try_from(obj.payload).ok()?;
            let mut out = Vec::with_capacity(members);
            let mut cursor = idx.checked_add(1)?;
            let arena = bin_ref(&arena_v).full_bytes();
            for _ in 0..members {
                let key = node_at(&tape, cursor)?;
                if key.kind != K_STR {
                    return None;
                }
                let ArenaSpan { off, len } = key.str_span();
                // Reject a span that would panic later, while the arena is
                // still in hand.
                str_bytes(&arena, key)?;
                let value_at = cursor.checked_add(key.skip)?;
                out.push((off, len, value_at));
                cursor = value_at.checked_add(node_at(&tape, value_at)?.skip)?;
            }
            Some(out)
        })()
        .unwrap_or_default();

        let parts = Self::doc_parts(&doc);
        let mut items = Vec::with_capacity(spans.len());
        if let Some((arena_v, tape_v, _)) = parts {
            for (off, len, at) in spans {
                let key = {
                    let arena = bin_ref(&arena_v).full_bytes();
                    let bytes = arena.get(off..off + len).unwrap_or_default();
                    let s = std::str::from_utf8(bytes).unwrap_or_default().to_owned();
                    Value::str_in(&mut self.heap, &s)
                };
                let val = self.make_doc(&arena_v, &tape_v, at)?;
                items.push(Value::tuple_in(&mut self.heap, &[key, val]));
            }
        }
        let v = Value::array_in(&mut self.heap, &items);
        self.stack.push(v);
        Ok(())
    }

    /// `[d Doc] -> Array(Doc)` — an array's elements in document order. Empty
    /// for anything that is not an array.
    ///
    /// One linear walk, the same shape as `json_entries` — [`collect_element_positions`]
    /// does the walk and carries the read count that gates it staying linear
    /// (T-173). The obvious spelling — `json.index` once per element, from
    /// Scarlet — is O(n²): every `index` restarts the cursor at the
    /// container and adds `skip` `i` times. The element count is whatever
    /// the sender wrote, so that is an algorithmic denial of service on any
    /// caller that decodes an array it did not author, which is every
    /// caller that reads a payload off a socket.
    pub(super) fn json_elements(&mut self) -> VmResult<()> {
        let doc = self.pop()?;

        // Positions first: building the Scarlet values needs `&mut self.heap`,
        // which cannot be held across the borrow of the tape.
        let ats: Vec<usize> = (|| {
            let (_, tape_v, idx) = Self::doc_parts(&doc)?;
            let tape = bin_ref(&tape_v).full_bytes();
            Some(collect_element_positions(&tape, idx)?.0)
        })()
        .unwrap_or_default();

        let parts = Self::doc_parts(&doc);
        let mut items = Vec::with_capacity(ats.len());
        if let Some((arena_v, tape_v, _)) = parts {
            for at in ats {
                let d = self.make_doc(&arena_v, &tape_v, at)?;
                items.push(d);
            }
        }
        let v = Value::array_in(&mut self.heap, &items);
        self.stack.push(v);
        Ok(())
    }

    /// `[d Doc] -> Option(String)` — the string, already unescaped by the
    /// parser and already validated UTF-8.
    pub(super) fn json_string(&mut self) -> VmResult<()> {
        let doc = self.pop()?;
        let text: Option<String> = (|| {
            let (arena_v, tape_v, idx) = Self::doc_parts(&doc)?;
            let n = node_at(&bin_ref(&tape_v).full_bytes(), idx)?;
            if n.kind != K_STR {
                return None;
            }
            let arena = bin_ref(&arena_v).full_bytes();
            Some(std::str::from_utf8(str_bytes(&arena, n)?).ok()?.to_owned())
        })();
        let v = match text {
            Some(s) => {
                let s = Value::str_in(&mut self.heap, &s);
                self.make_some(s)?
            }
            None => self.make_none()?,
        };
        self.stack.push(v);
        Ok(())
    }

    /// `[d Doc] -> Option(Int)`
    ///
    /// `None` for a float, and `None` for an integer above `i64::MAX` — a
    /// JSON number that is a valid `u64` but not a Scarlet `Int`. Reporting it
    /// as absent is what stops a silent truncation.
    pub(super) fn json_int(&mut self) -> VmResult<()> {
        let doc = self.pop()?;
        let n = (|| {
            let (_, tape_v, idx) = Self::doc_parts(&doc)?;
            let n = node_at(&bin_ref(&tape_v).full_bytes(), idx)?;
            (n.kind == K_INT).then_some(n.payload.cast_signed())
        })();
        let v = match n {
            Some(i) => {
                let i = Value::int_in(&mut self.heap, i);
                self.make_some(i)?
            }
            None => self.make_none()?,
        };
        self.stack.push(v);
        Ok(())
    }

    /// `[d Doc] -> Option(String)` — the integer at `d` written in decimal.
    ///
    /// `json.int` is `None` for a `K_UINT_BIG` and that is right: there is no
    /// `Int` to put it in. But it left `to_json` with nothing to build from,
    /// and the default it reached for turned a 64-bit identifier into `0` —
    /// the exact truncation `K_UINT_BIG` exists to prevent, arrived at from
    /// the other side. The digits are what a document being passed through
    /// has to keep, so they are available here.
    ///
    /// `None` for anything that is not an integer, including a float.
    pub(super) fn json_int_text(&mut self) -> VmResult<()> {
        let doc = self.pop()?;
        let text: Option<String> = (|| {
            let (_, tape_v, idx) = Self::doc_parts(&doc)?;
            let n = node_at(&bin_ref(&tape_v).full_bytes(), idx)?;
            match n.kind {
                K_INT => Some(n.payload.cast_signed().to_string()),
                K_UINT_BIG => Some(n.payload.to_string()),
                _ => None,
            }
        })();
        let v = match text {
            Some(s) => {
                let s = Value::str_in(&mut self.heap, &s);
                self.make_some(s)?
            }
            None => self.make_none()?,
        };
        self.stack.push(v);
        Ok(())
    }

    /// `[d Doc] -> Option(Float)` — any JSON number, integers included.
    pub(super) fn json_float(&mut self) -> VmResult<()> {
        let doc = self.pop()?;
        let f = (|| {
            let (_, tape_v, idx) = Self::doc_parts(&doc)?;
            let n = node_at(&bin_ref(&tape_v).full_bytes(), idx)?;
            match n.kind {
                K_FLOAT => Some(f64::from_bits(n.payload)),
                K_INT => Some(n.payload.cast_signed() as f64),
                K_UINT_BIG => Some(n.payload as f64),
                _ => None,
            }
        })();
        let v = match f {
            Some(f) => {
                let f = Value::float(f);
                self.make_some(f)?
            }
            None => self.make_none()?,
        };
        self.stack.push(v);
        Ok(())
    }

    /// `[d Doc] -> Option(Bool)`
    pub(super) fn json_bool(&mut self) -> VmResult<()> {
        let doc = self.pop()?;
        let b = (|| {
            let (_, tape_v, idx) = Self::doc_parts(&doc)?;
            let n = node_at(&bin_ref(&tape_v).full_bytes(), idx)?;
            (n.kind == K_BOOL).then_some(n.payload != 0)
        })();
        let v = match b {
            Some(b) => self.make_some(Value::bool(b))?,
            None => self.make_none()?,
        };
        self.stack.push(v);
        Ok(())
    }

    /// `[j Json] -> String`
    pub(super) fn json_encode(&mut self) -> VmResult<()> {
        let root = self.pop()?;
        let mut out = String::new();
        write_json_value(&mut out, root);
        let v = Value::str_in(&mut self.heap, &out);
        self.stack.push(v);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::heap::ProcHeap;

    /// Parse with `simd-json` and build the compact tape, without a VM.
    fn tape_of(src: &str) -> (Vec<u8>, Vec<u8>) {
        let mut buf = src.as_bytes().to_vec();
        let t = simd_json::to_tape(&mut buf).expect("valid JSON");
        build_tape(&t.0).expect("tape fits")
    }

    /// Walk to the value of `name` in the object at node 0. This is
    /// `json_field`'s own walk, not a copy of it — a copy would have gone on
    /// passing while the real one returned an unchecked index.
    fn field_at(tape: &[u8], arena: &[u8], name: &str) -> Option<usize> {
        field_index(tape, arena, 0, name.as_bytes())
    }

    #[test]
    fn skip_walks_past_a_nested_object() {
        let (tape, arena) = tape_of(r#"{"a":{"x":[1,2,{"y":3}]},"b":7}"#);
        let b = field_at(&tape, &arena, "b").expect("b is found past the nest");
        assert_eq!(node_at(&tape, b).unwrap().kind, K_INT);
        assert_eq!(node_at(&tape, b).unwrap().payload.cast_signed(), 7);
    }

    #[test]
    fn empty_containers_have_skip_one() {
        let (tape, _) = tape_of(r#"[{},[],1]"#);
        assert_eq!(node_at(&tape, 0).unwrap().kind, K_ARRAY);
        assert_eq!(node_at(&tape, 1).unwrap().skip, 1);
        assert_eq!(node_at(&tape, 2).unwrap().skip, 1);
        // The `1` is reachable only if both empty containers advanced by one.
        assert_eq!(node_at(&tape, 3).unwrap().payload.cast_signed(), 1);
    }

    #[test]
    fn duplicate_keys_both_survive_and_first_wins() {
        let (tape, arena) = tape_of(r#"{"a":1,"a":2}"#);
        let at = field_at(&tape, &arena, "a").unwrap();
        assert_eq!(node_at(&tape, at).unwrap().payload.cast_signed(), 1);
        assert_eq!(
            node_at(&tape, 0).unwrap().payload,
            2,
            "both members on the tape"
        );
    }

    #[test]
    fn integer_above_i64_max_is_kept_distinct_not_truncated() {
        let (tape, _) = tape_of("[18446744073709551615]");
        let n = node_at(&tape, 1).unwrap();
        assert_eq!(n.kind, K_UINT_BIG);
        assert_eq!(n.payload, u64::MAX);
        let (tape, _) = tape_of("[9223372036854775807]");
        assert_eq!(node_at(&tape, 1).unwrap().kind, K_INT);
    }

    /// `simd-json` accepts nesting this deep, so the tape build must not be
    /// recursive. This is the parse half of the stack-overflow guard; the
    /// encoder half is `deep_nesting_encodes_without_recursion`.
    #[test]
    fn ten_thousand_deep_nesting_builds_a_tape() {
        let src = format!("{}{}", "[".repeat(10_000), "]".repeat(10_000));
        let (tape, _) = tape_of(&src);
        assert_eq!(tape.len(), 10_000 * NODE_BYTES);
        assert_eq!(node_at(&tape, 0).unwrap().skip, 10_000);
    }

    /// The image names the node's kind and its index. The assertion that
    /// matters is the last one: no node's image quotes the arena, whose whole
    /// point is that it holds the caller's payload.
    #[test]
    fn doc_image_names_kind_and_position_never_contents() {
        let (tape, arena) = tape_of(r#"{"secret":"hunter2","n":2,"xs":[1],"f":1.5}"#);
        assert_eq!(doc_image(&tape, 0), "<json object#0>");

        let at = |name| field_at(&tape, &arena, name).expect("member is on the tape");
        assert_eq!(
            doc_image(&tape, at("secret")),
            format!("<json string#{}>", at("secret"))
        );
        assert_eq!(doc_image(&tape, at("n")), format!("<json int#{}>", at("n")));
        assert_eq!(
            doc_image(&tape, at("xs")),
            format!("<json array#{}>", at("xs"))
        );
        assert_eq!(
            doc_image(&tape, at("f")),
            format!("<json float#{}>", at("f"))
        );
    }

    /// The property the ticket is about: the image is bounded by nothing the
    /// document controls. Grepping the image for the payload would not witness
    /// this — the record rendering prints a `Binary` as decimal bytes, so a
    /// leaked arena carries no readable text to find. Its length is the tell.
    #[test]
    fn doc_image_stays_bounded_however_large_the_document() {
        let (tape, _) = tape_of(&format!(r#"{{"k":"{}"}}"#, "x".repeat(100_000)));
        for i in 0..tape.len() / NODE_BYTES {
            let image = doc_image(&tape, i);
            assert!(
                image.len() < 32,
                "image scales with the document: {} bytes",
                image.len()
            );
        }
    }

    /// An index off the end of the tape is what a `Doc` over a mismatched tape
    /// would carry. The image stays bounded rather than panicking the printer.
    #[test]
    fn doc_image_of_an_undecodable_node_is_still_bounded() {
        let (tape, _) = tape_of("{}");
        assert_eq!(doc_image(&tape, 99), "<json invalid#99>");
    }

    #[test]
    fn strings_are_unescaped_into_the_arena() {
        let (tape, arena) = tape_of(r#"{"k":"a\nbA"}"#);
        let at = field_at(&tape, &arena, "k").unwrap();
        let n = node_at(&tape, at).unwrap();
        assert_eq!(str_bytes(&arena, n).unwrap(), b"a\nbA");
    }

    #[test]
    fn float_encoding_round_trips_and_keeps_its_kind() {
        let mut out = String::new();
        write_json_float(&mut out, 1.0);
        assert_eq!(out, "1.0", "an integral float must not read back as an Int");
        let mut out = String::new();
        write_json_float(&mut out, 0.1 + 0.2);
        assert_eq!(out.parse::<f64>().unwrap(), 0.1 + 0.2);
        let mut out = String::new();
        write_json_float(&mut out, f64::NAN);
        assert_eq!(out, "null");
    }

    #[test]
    fn string_escaping_covers_quotes_backslashes_and_controls() {
        let mut out = String::new();
        write_json_string(&mut out, "a\"b\\c\nd\u{1}e");
        assert_eq!(out, "\"a\\\"b\\\\c\\nd\\u0001e\"");
    }

    /// The parser underneath decodes `\ud800` to U+0000 instead of rejecting
    /// it, so without this scan a lone surrogate becomes a NUL that no caller
    /// can tell from a real one.
    #[test]
    fn lone_surrogates_are_found_and_valid_pairs_are_not() {
        assert_eq!(first_lone_surrogate(br#"["\ud800"]"#), Some(2));
        assert_eq!(first_lone_surrogate(br#"["\udc00"]"#), Some(2));
        assert_eq!(first_lone_surrogate(br#"["\ud800\ud800"]"#), Some(2));
        // A high surrogate followed by something that is not a low one.
        assert_eq!(first_lone_surrogate(br#"["\ud800A"]"#), Some(2));
        assert_eq!(first_lone_surrogate(br#"["\ud800x"]"#), Some(2));

        // Valid pairs and ordinary escapes stay accepted.
        assert_eq!(first_lone_surrogate(br#"["\ud83d\ude00"]"#), None);
        assert_eq!(first_lone_surrogate(br#"["a\nb\tc\\d\"e"]"#), None);
        assert_eq!(first_lone_surrogate(br#"[1,2,3]"#), None);
        // Two pairs back to back: consuming the first must land exactly on the
        // second's high half, or the second's low half reads as lone.
        assert_eq!(
            first_lone_surrogate(br#"["\ud83d\ude00\ud83d\ude01"]"#),
            None
        );
        // An ESCAPED backslash followed by a literal "u D800" is not an escape.
        assert_eq!(first_lone_surrogate(br#"["\\ud800"]"#), None);
    }

    /// The `memchr` skip is what keeps the scan off every byte of a document
    /// that has no escapes, which is nearly every document.
    ///
    /// This asserts the step count, not the answer. `assert_eq!(…, None)` on
    /// 4096 `x` bytes — which is what stood here — holds whether the skip
    /// exists or not: replacing it with `i += 1` left all twelve tests in this
    /// module green. The answer cannot tell the two implementations apart,
    /// because they agree on it for every input.
    #[test]
    fn the_surrogate_scan_skips_to_the_next_backslash_rather_than_walking() {
        // No escape anywhere: one `memchr`, which finds nothing and stops.
        let big = vec![b'x'; 4096];
        assert_eq!(
            scan_for_lone_surrogate(&big),
            (None, 1),
            "one memchr over the whole input, not one step per byte"
        );

        // One escape at the very end: the skip lands on it directly, so it is
        // one step to reach it and one to consume it.
        let mut trailing = vec![b'x'; 4096];
        trailing.extend_from_slice(br"\n");
        assert_eq!(
            scan_for_lone_surrogate(&trailing),
            (None, 2),
            "skip to the escape, consume it, done"
        );

        // And it still finds a lone surrogate behind 4096 bytes of filler:
        // one step to skip to the escape, one to read it. A byte-at-a-time
        // walk would take 4097.
        let mut buried = vec![b'x'; 4096];
        buried.extend_from_slice(br"\ud800");
        assert_eq!(scan_for_lone_surrogate(&buried), (Some(4096), 2));
    }

    #[test]
    fn elements_walk_reaches_every_element_of_a_nested_array() {
        // The positions `json_elements` collects, by the same arithmetic: one
        // cursor advanced by `skip`, never restarted at the container.
        let (tape, _) = tape_of(r#"[1,{"a":[2,3]},[4],5]"#);
        let arr = node_at(&tape, 0).unwrap();
        assert_eq!(arr.kind, K_ARRAY);
        let mut ats = Vec::new();
        let mut cursor = 1;
        for _ in 0..arr.payload {
            ats.push(cursor);
            cursor += node_at(&tape, cursor).unwrap().skip;
        }
        assert_eq!(ats.len(), 4);
        assert_eq!(node_at(&tape, ats[0]).unwrap().payload.cast_signed(), 1);
        assert_eq!(node_at(&tape, ats[1]).unwrap().kind, K_OBJECT);
        assert_eq!(node_at(&tape, ats[2]).unwrap().kind, K_ARRAY);
        assert_eq!(node_at(&tape, ats[3]).unwrap().payload.cast_signed(), 5);
        // One walk covers the whole subtree: the cursor ends past the last node.
        assert_eq!(cursor, arr.skip);
    }

    /// T-173: the test above proves the *positions* are right; it does not
    /// prove the walk stayed linear, because a walk that restarts at the
    /// container for every element finds the identical four positions. This
    /// asserts the read count instead, on the same fixture: two `node_at`
    /// calls per top-level element (the bounds check, then the one that
    /// reads `skip`) is 8, not `element_index`'s cost summed over
    /// `0..4` — `(0+1)+(1+1)+(2+1)+(3+1) = 10` — which is what the deleted
    /// `collect_elements` shape (`json.index` once per element) paid, and
    /// which the gap widens against as the array grows.
    #[test]
    fn elements_walk_reads_the_tape_a_fixed_number_of_times_per_element() {
        let (tape, _) = tape_of(r#"[1,{"a":[2,3]},[4],5]"#);
        let (ats, reads) = collect_element_positions(&tape, 0).unwrap();
        assert_eq!(ats.len(), 4);
        assert_eq!(reads, 8, "2 reads x 4 elements, not the O(n^2) shape");
    }

    /// The read count must grow linearly with the element count, not
    /// quadratically — the property T-173 asks to be gated, demonstrated
    /// rather than pinned to one array's magic number. Flat arrays of
    /// integers only, so every element costs exactly the same 2 reads and
    /// the count is exact rather than approximate: `reads(n) == 2n`.
    /// Quadratic (`element_index` once per element, restarting at the
    /// container) would instead cost `n(n+1)`, i.e. roughly `n^2/2` — for
    /// `n = 512` that is 131,584 against this walk's 1,024.
    #[test]
    fn elements_walk_reads_scale_linearly_with_element_count() {
        fn reads_for(n: usize) -> usize {
            let src = format!(
                "[{}]",
                (0..n).map(|i| i.to_string()).collect::<Vec<_>>().join(",")
            );
            let (tape, _) = tape_of(&src);
            collect_element_positions(&tape, 0).unwrap().1
        }

        assert_eq!(reads_for(8), 16);
        assert_eq!(reads_for(64), 128);
        assert_eq!(
            reads_for(512),
            1024,
            "reads(n) == 2n, or the walk is no longer O(n)"
        );
    }

    /// `json.int` is `None` for a `u64` above `i64::MAX`, so `to_json` has to
    /// get the digits from somewhere or it invents a value. It invented `0`.
    #[test]
    fn the_digits_of_an_integer_survive_whether_or_not_it_fits_an_int() {
        let (tape, _) = tape_of("[18446744073709551615,9223372036854775807,-1,0]");
        let text = |i: usize| {
            let n = node_at(&tape, i).unwrap();
            match n.kind {
                K_INT => Some(n.payload.cast_signed().to_string()),
                K_UINT_BIG => Some(n.payload.to_string()),
                _ => None,
            }
        };
        assert_eq!(text(1).as_deref(), Some("18446744073709551615"));
        assert_eq!(text(2).as_deref(), Some("9223372036854775807"));
        assert_eq!(text(3).as_deref(), Some("-1"));
        assert_eq!(text(4).as_deref(), Some("0"));
        // Not an integer: no digits to hand back.
        let (tape, _) = tape_of("[1.5]");
        assert_eq!(node_at(&tape, 1).unwrap().kind, K_FLOAT);
    }

    /// `Json.Number` puts its text into the output unchanged, and the text can
    /// be hand-built. Anything that is not a number would break the document
    /// around it, so it is written as `null` instead.
    #[test]
    fn only_a_json_number_is_written_verbatim() {
        for ok in [
            "0",
            "-0",
            "18446744073709551615",
            "-9223372036854775808",
            "1.5",
            "-1.5e-10",
            "1E+3",
            "1e3",
        ] {
            assert!(is_json_number(ok), "{ok} is a JSON number");
        }
        for bad in [
            "", "-", "+1", "01", "1.", ".5", "1e", "1e+", "0x10", "NaN", "1 ", " 1", "1,2", "--5",
            "1.2.3", "Infinity",
        ] {
            assert!(!is_json_number(bad), "{bad:?} is not a JSON number");
        }
    }

    #[test]
    fn a_zero_skip_node_is_refused_rather_than_looping() {
        // A hand-forged record with skip 0: a container walk over it would
        // never advance.
        let mut tape = vec![0u8; NODE_BYTES];
        tape[0] = K_INT;
        assert!(node_at(&tape, 0).is_none());
    }

    #[test]
    fn out_of_range_reads_are_none_not_panics() {
        let (tape, arena) = tape_of("[1]");
        assert!(node_at(&tape, 99).is_none());
        let forged = TapeNode {
            kind: K_STR,
            skip: 1,
            payload: (900u64 << 32) | 4,
        };
        assert!(str_bytes(&arena, forged).is_none());
    }

    /// Append one hand-forged node. `skip` and `payload` are not checked
    /// against each other, which is the point: these are the tapes
    /// `build_tape` would never emit.
    fn push_node(tape: &mut Vec<u8>, kind: u8, skip: u64, payload: u64) {
        tape.extend_from_slice(&((skip << 32) | kind as u64).to_le_bytes());
        tape.extend_from_slice(&payload.to_le_bytes());
    }

    /// A truncated object: it claims one member and carries the key, but the
    /// key's value node is off the end of the tape. The matching arm used to
    /// return that index without reading it, so `json.field` handed back a
    /// `Doc` pointing past the tape while the non-matching arm next to it
    /// refused the very same index.
    #[test]
    fn a_matching_key_whose_value_is_off_the_tape_is_none() {
        let arena = b"a".to_vec();
        let mut tape = Vec::new();
        push_node(&mut tape, K_OBJECT, 3, 1);
        push_node(&mut tape, K_STR, 1, 1);
        assert_eq!(field_index(&tape, &arena, 0, b"a"), None);

        // Control: the same forged object with its value node present is
        // found, so the `None` above is the absent node and not a tape shape
        // the walk rejects for some other reason.
        push_node(&mut tape, K_INT, 1, 7);
        assert_eq!(field_index(&tape, &arena, 0, b"a"), Some(2));
    }

    /// An array claiming an element the tape does not carry. Element 0 is the
    /// walk's worst case: the loop body never runs, so the index it returns is
    /// `idx + 1` unread, and `payload` — the only other guard — is read off the
    /// same forged tape.
    #[test]
    fn element_zero_off_the_end_of_the_tape_is_none() {
        let mut tape = Vec::new();
        push_node(&mut tape, K_ARRAY, 2, 1);
        assert_eq!(element_index(&tape, 0, 0), None);

        // Control: the same forged array with its element present is found, so
        // the `None` above is the absent node and not a tape shape the walk
        // rejects for some other reason.
        push_node(&mut tape, K_INT, 1, 7);
        assert_eq!(element_index(&tape, 0, 0), Some(1));
    }

    /// The same defect one step along: the walk reads the node it steps over
    /// and still not the one it lands on.
    #[test]
    fn a_later_element_off_the_end_of_the_tape_is_none() {
        let mut tape = Vec::new();
        push_node(&mut tape, K_ARRAY, 3, 2);
        push_node(&mut tape, K_INT, 1, 7);
        assert_eq!(element_index(&tape, 0, 1), None);

        push_node(&mut tape, K_INT, 1, 9);
        assert_eq!(element_index(&tape, 0, 1), Some(2));
    }

    /// Build a `scarlet/json.Json` variant. The `TypeId` and the variant
    /// index are arbitrary: the encoder dispatches on the variant name.
    fn json(h: &mut ProcHeap, variant: &str, labels: &[&str], payload: &[Value]) -> Value {
        Value::enum_with_names_in(h, crate::TypeId(0), 0, "Json", variant, labels, payload)
    }

    fn json_int(h: &mut ProcHeap, i: i64) -> Value {
        json(h, "Integer", &["value"], &[Value::small_int(i)])
    }

    /// A well-formed `(String, Json)` member.
    fn member(h: &mut ProcHeap, key: &str, value: Value) -> Value {
        let k = Value::str_in(h, key);
        Value::tuple_in(h, &[k, value])
    }

    fn encoded(root: Value) -> String {
        let mut out = String::new();
        write_json_value(&mut out, root);
        out
    }

    #[test]
    fn a_well_formed_document_encodes() {
        let mut h = ProcHeap::new();
        let one = json_int(&mut h, 1);
        let two = json_int(&mut h, 2);
        let items = seq::from_slice(&mut h, &[one, two]);
        let list = json(&mut h, "List", &["items"], &[items]);
        let a = member(&mut h, "a", list);
        let members = seq::from_slice(&mut h, &[a]);
        let obj = json(&mut h, "Object", &["members"], &[members]);
        assert_eq!(encoded(obj), r#"{"a":[1,2]}"#);
    }

    /// Every list element emits something, so a variant the encoder does not
    /// know is a `null` element rather than a gap beside a separator.
    #[test]
    fn an_unrecognised_variant_is_a_null_element_not_a_gap() {
        let mut h = ProcHeap::new();
        let null = json(&mut h, "Null", &[], &[]);
        let one = json_int(&mut h, 1);
        let items = seq::from_slice(&mut h, &[null, one]);
        let list = json(&mut h, "List", &["items"], &[items]);
        assert_eq!(encoded(list), "[null,1]");
    }

    /// Regression: the separator used to be pushed independently of the
    /// member, so a member the `(String, Json)` pattern refused left its
    /// comma behind and the encoder emitted `{"a":1,,"c":3}` — not JSON.
    #[test]
    fn a_member_that_is_not_a_pair_takes_its_separator_with_it() {
        let mut h = ProcHeap::new();
        let one = json_int(&mut h, 1);
        let a = member(&mut h, "a", one);
        let three = json_int(&mut h, 3);
        let c = member(&mut h, "c", three);
        let members = seq::from_slice(&mut h, &[a, Value::small_int(7), c]);
        let obj = json(&mut h, "Object", &["members"], &[members]);
        assert_eq!(encoded(obj), r#"{"a":1,"c":3}"#);
    }

    /// The dropped member is the first one, so the stranded separator would
    /// lead — `{,"a":1}`. This is the case an `i > 0` guard cannot see.
    #[test]
    fn a_dropped_first_member_leaves_no_leading_separator() {
        let mut h = ProcHeap::new();
        let one = json_int(&mut h, 1);
        let a = member(&mut h, "a", one);
        let members = seq::from_slice(&mut h, &[Value::small_int(7), a]);
        let obj = json(&mut h, "Object", &["members"], &[members]);
        assert_eq!(encoded(obj), r#"{"a":1}"#);
    }

    /// `as_tuple` succeeds at any arity, so it is the `[k, val]` pattern that
    /// refuses a 3-tuple — a second way to reach the drop that has nothing to
    /// do with `seq::get`. An object whose every member is refused is `{}`.
    #[test]
    fn a_member_tuple_of_the_wrong_arity_is_dropped_whole() {
        let mut h = ProcHeap::new();
        let one = json_int(&mut h, 1);
        let k = Value::str_in(&mut h, "a");
        let extra = Value::small_int(0);
        let three = Value::tuple_in(&mut h, &[k, one, extra]);
        let members = seq::from_slice(&mut h, &[three]);
        let obj = json(&mut h, "Object", &["members"], &[members]);
        assert_eq!(encoded(obj), "{}");
    }
}
