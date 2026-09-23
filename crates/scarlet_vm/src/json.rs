//! JSON: parsing a document to a tape, reading the tape, and encoding a
//! `Json` tree. The old VM's design (`vm/json.rs` on master before the
//! rewrite), on the new heap.
//!
//! The parse is `simd-json`, the Rust port of simdjson, with runtime
//! CPU-feature detection so one binary serves SSE4.2, AVX2 and NEON. What it
//! produces is not a tree of Scarlet values: it is a **compact tape**, a flat
//! run of fixed-width nodes in document order, plus a string arena. Both are
//! binaries inside an opaque `scarlet/json.Doc`, with the index of the node
//! the `Doc` points at.
//!
//! That indirection is the point. A Scarlet value per JSON node would put an
//! allocation and a string copy on every node of the document, whether or not
//! the program reads it. With a tape, `json.field` is a bounded walk over
//! 16-byte nodes, read in place (`binary::word`), and a program that reads 6
//! fields out of a 400-field payload pays for 6 strings.
//!
//! Nothing here recurses. `simd-json` accepts nesting thousands deep, so a
//! recursive walk or encoder would be a stack overflow reachable from any
//! program that parses what it is sent. The tape is built in one pass, and
//! the encoder keeps its own list of work.
//!
//! ## Tape format
//!
//! A node is two little-endian words, 16 bytes:
//!
//! ```text
//! word 0: bits 0..8   kind
//!         bits 32..64 skip: nodes in this subtree, this one included
//! word 1: payload (see `K_*`)
//! ```
//!
//! A container's children follow it, so element `i` is found by adding
//! `skip` `i` times. An object's members are laid out key, value, key,
//! value; a key is always a `K_STR`, whose skip is 1.
//!
//! Every read is checked against the tape's and the arena's lengths. Only
//! `parse` makes a tape, but the checks are cheap, and they make a node that
//! is not there `None` rather than a wrong read.

use num_bigint::BigInt;
use simd_json::StaticNode;
use simd_json::value::tape::Node;

use crate::binary::{self, Bits};
use crate::heap::{Cell, Heap};
use crate::value::{Value, View};

// Node kinds. `K_UINT_BIG` is an integer above `i64::MAX` that fits a `u64`:
// its payload is the `u64`, where a `K_INT`'s is an `i64`'s bits.
const K_NULL: u8 = 0;
const K_BOOL: u8 = 1;
const K_INT: u8 = 2;
const K_FLOAT: u8 = 3;
const K_STR: u8 = 4;
const K_ARRAY: u8 = 5;
const K_OBJECT: u8 = 6;
const K_UINT_BIG: u8 = 7;

/// The ordinals `scarlet/json.kind_raw` turns into `Kind`'s variants. A
/// `K_UINT_BIG` is an `Int`.
const KIND_NULL: i64 = 0;
const KIND_BOOL: i64 = 1;
const KIND_INT: i64 = 2;
const KIND_FLOAT: i64 = 3;
const KIND_STRING: i64 = 4;
const KIND_ARRAY: i64 = 5;
const KIND_OBJECT: i64 = 6;

/// A node, read.
#[derive(Clone, Copy)]
struct TapeNode {
    kind: u8,
    /// Nodes in this subtree, this one included: always at least 1.
    skip: usize,
    payload: u64,
}

impl TapeNode {
    /// Where a `K_STR`'s bytes are in the arena, and how many.
    fn span(self) -> (u64, u64) {
        (self.payload >> 32, self.payload & 0xFFFF_FFFF)
    }
}

/// A parsed document, as the two binaries a `Doc` holds.
#[derive(Clone, Copy)]
pub(crate) struct Tape<'h> {
    pub(crate) heap: &'h Heap,
    pub(crate) tape: Bits,
    pub(crate) arena: Bits,
}

impl Tape<'_> {
    /// Node `i`, or `None` past the tape's end.
    fn node(&self, i: usize) -> Option<TapeNode> {
        let k = u64::try_from(i).ok()?.checked_mul(2)?;
        let w0 = binary::word(self.heap, self.tape, k)?;
        let payload = binary::word(self.heap, self.tape, k + 1)?;
        let skip = usize::try_from(w0 >> 32).ok()?;
        // A zero skip would let a walk stand still, forever.
        if skip == 0 {
            return None;
        }
        Some(TapeNode {
            kind: w0 as u8,
            skip,
            payload,
        })
    }

    /// A `K_STR`'s bytes, or `None` when they run past the arena.
    fn text(&self, n: TapeNode) -> Option<Vec<u8>> {
        let (at, len) = n.span();
        binary::byte_range(self.heap, self.arena, at, len)
    }

    /// The `Kind` ordinal of node `idx`.
    pub(crate) fn kind(&self, idx: usize) -> Option<i64> {
        Some(match self.node(idx)?.kind {
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

    /// How many elements or members the array or object at `idx` has.
    pub(crate) fn len(&self, idx: usize) -> Option<u64> {
        let n = self.node(idx)?;
        matches!(n.kind, K_ARRAY | K_OBJECT).then_some(n.payload)
    }

    /// Where the value of the first member named `name` of the object at
    /// `idx` is. `None` when `idx` is not an object, when it has no such
    /// member, or when a node the walk reaches is not there.
    pub(crate) fn field(&self, idx: usize, name: &[u8]) -> Option<usize> {
        let obj = self.node(idx)?;
        if obj.kind != K_OBJECT {
            return None;
        }
        let mut at = idx.checked_add(1)?;
        for _ in 0..obj.payload {
            let key = self.node(at)?;
            if key.kind != K_STR {
                return None;
            }
            let value_at = at.checked_add(key.skip)?;
            // Read before the key is compared, so a matching key cannot hand
            // back a value the walk past it would have refused.
            let value = self.node(value_at)?;
            let (from, len) = key.span();
            if len == name.len() as u64 && binary::bytes_are(self.heap, self.arena, from, name) {
                return Some(value_at);
            }
            at = value_at.checked_add(value.skip)?;
        }
        None
    }

    /// Where element `want` of the array at `idx` is.
    pub(crate) fn element(&self, idx: usize, want: usize) -> Option<usize> {
        let arr = self.node(idx)?;
        if arr.kind != K_ARRAY || u64::try_from(want).ok()? >= arr.payload {
            return None;
        }
        let mut at = idx.checked_add(1)?;
        for _ in 0..want {
            at = at.checked_add(self.node(at)?.skip)?;
        }
        // The walk reads each node it steps over but not the one it stops
        // on, and for `want == 0` it steps over none.
        self.node(at)?;
        Some(at)
    }

    /// Where each element of the array at `idx` is, in order: one walk, so
    /// a sender's element count costs its length and not its square, as
    /// calling [`Self::element`] once for each would.
    pub(crate) fn elements(&self, idx: usize) -> Option<Vec<usize>> {
        let arr = self.node(idx)?;
        if arr.kind != K_ARRAY {
            return None;
        }
        let count = usize::try_from(arr.payload).ok()?;
        let mut out = Vec::with_capacity(count.min(1 << 16));
        let mut at = idx.checked_add(1)?;
        for _ in 0..count {
            let n = self.node(at)?;
            out.push(at);
            at = at.checked_add(n.skip)?;
        }
        Some(out)
    }

    /// Each member of the object at `idx`, its key's bytes and where its
    /// value is, in document order and duplicates included.
    pub(crate) fn members(&self, idx: usize) -> Option<Vec<(Vec<u8>, usize)>> {
        let obj = self.node(idx)?;
        if obj.kind != K_OBJECT {
            return None;
        }
        let count = usize::try_from(obj.payload).ok()?;
        let mut out = Vec::with_capacity(count.min(1 << 16));
        let mut at = idx.checked_add(1)?;
        for _ in 0..count {
            let key = self.node(at)?;
            if key.kind != K_STR {
                return None;
            }
            let value_at = at.checked_add(key.skip)?;
            let value = self.node(value_at)?;
            out.push((self.text(key)?, value_at));
            at = value_at.checked_add(value.skip)?;
        }
        Some(out)
    }

    /// The string at `idx`, unescaped.
    pub(crate) fn string(&self, idx: usize) -> Option<Vec<u8>> {
        let n = self.node(idx)?;
        if n.kind != K_STR {
            return None;
        }
        self.text(n)
    }

    /// The integer at `idx`. An `Int` has no bounds, so every integer the
    /// parser takes is one.
    pub(crate) fn int(&self, idx: usize) -> Option<BigInt> {
        let n = self.node(idx)?;
        match n.kind {
            K_INT => Some(BigInt::from(n.payload as i64)),
            K_UINT_BIG => Some(BigInt::from(n.payload)),
            _ => None,
        }
    }

    /// The number at `idx` as a float, integers included.
    pub(crate) fn float(&self, idx: usize) -> Option<f64> {
        let n = self.node(idx)?;
        match n.kind {
            K_FLOAT => Some(f64::from_bits(n.payload)),
            K_INT => Some(n.payload as i64 as f64),
            K_UINT_BIG => Some(n.payload as f64),
            _ => None,
        }
    }

    pub(crate) fn bool(&self, idx: usize) -> Option<bool> {
        let n = self.node(idx)?;
        (n.kind == K_BOOL).then_some(n.payload != 0)
    }

    /// How a `Doc` at `idx` shows: what it points at and where, never the
    /// document. A `Doc` holds every string of a body that may be
    /// megabytes, and printing one is the first thing reached for while
    /// debugging, so this stays short however large the document is.
    fn image(&self, idx: usize) -> String {
        let kind = match self.kind(idx) {
            Some(KIND_NULL) => "null",
            Some(KIND_BOOL) => "bool",
            Some(KIND_INT) => "int",
            Some(KIND_FLOAT) => "float",
            Some(KIND_STRING) => "string",
            Some(KIND_ARRAY) => "array",
            Some(KIND_OBJECT) => "object",
            _ => "invalid",
        };
        format!("<json {kind}#{idx}>")
    }
}

/// How the `Doc` in `cell` shows: [`Tape::image`], or `<json invalid>` for
/// one that is not the shape `parse` makes.
pub(crate) fn doc_image(heap: &Heap, cell: Cell) -> String {
    let bits = |i| {
        let v: Value = heap.field(cell, i)?;
        binary::bits(heap, v.as_cell()?)
    };
    let idx = heap.field(cell, 2).and_then(|v| match v.view() {
        View::Int(n) => usize::try_from(n).ok(),
        View::Float(_)
        | View::Nil
        | View::Bool(_)
        | View::Func(_)
        | View::Cell(_)
        | View::Nullary(_) => None,
    });
    match (bits(0), bits(1), idx) {
        (Some(arena), Some(tape), Some(idx)) => Tape { heap, tape, arena }.image(idx),
        _ => "<json invalid>".into(),
    }
}

/// Why a document did not parse: the byte offset the parser stopped at, and
/// what it found.
pub(crate) struct ParseError {
    pub(crate) offset: usize,
    pub(crate) message: String,
}

/// Parse `src` into a tape and a string arena, as bytes.
pub(crate) fn parse(src: &[u8]) -> Result<(Vec<u8>, Vec<u8>), ParseError> {
    // `simd-json` parses in place, so it needs a copy of its own. The
    // surrogate scan reads the input, so it runs before that copy is
    // rewritten.
    let mut buf = src.to_vec();
    let lone = first_lone_surrogate(&buf);
    let tape = simd_json::to_tape(&mut buf).map_err(|e| ParseError {
        offset: e.index(),
        message: e.to_string(),
    })?;
    if let Some(offset) = lone {
        return Err(ParseError {
            offset,
            message: "unpaired UTF-16 surrogate escape".into(),
        });
    }
    build_tape(&tape.0).map_err(|message| ParseError {
        offset: 0,
        message: message.into(),
    })
}

/// The compact tape and the string arena, from `simd-json`'s tape.
///
/// One pass: `simd-json` already gives nodes in document order with each
/// container's count, so a skip is a copy rather than a walk. Strings are
/// copied into the arena rather than pointed at in the input, which is gone
/// when this returns.
///
/// An `Err` for a document too large for the 32-bit offsets: both bounds are
/// about 4 billion, and a document that reaches one has used up memory
/// already, but they must not wrap.
fn build_tape(nodes: &[Node<'_>]) -> Result<(Vec<u8>, Vec<u8>), &'static str> {
    if nodes.len() > u32::MAX as usize {
        return Err("document has too many JSON nodes to index");
    }
    let mut tape = Vec::with_capacity(nodes.len() * 16);
    let mut arena: Vec<u8> = Vec::new();
    for n in nodes {
        let (kind, skip, payload): (u8, u64, u64) = match n {
            Node::String(s) => {
                let at = arena.len();
                if at > u32::MAX as usize || s.len() > u32::MAX as usize {
                    return Err("document has too much string data to index");
                }
                arena.extend_from_slice(s.as_bytes());
                (K_STR, 1, (at as u64) << 32 | s.len() as u64)
            }
            // `count` is the nodes after this one that belong to it, so the
            // subtree is one more. An empty container's count is 0.
            Node::Object { len, count } => (K_OBJECT, *count as u64 + 1, *len as u64),
            Node::Array { len, count } => (K_ARRAY, *count as u64 + 1, *len as u64),
            Node::Static(StaticNode::Null) => (K_NULL, 1, 0),
            Node::Static(StaticNode::Bool(b)) => (K_BOOL, 1, u64::from(*b)),
            Node::Static(StaticNode::I64(i)) => (K_INT, 1, *i as u64),
            Node::Static(StaticNode::U64(u)) => match i64::try_from(*u) {
                Ok(i) => (K_INT, 1, i as u64),
                Err(_) => (K_UINT_BIG, 1, *u),
            },
            Node::Static(StaticNode::F64(f)) => (K_FLOAT, 1, f.to_bits()),
            // No catch-all, on purpose: `simd-json`'s `128bit` feature adds
            // `I128` and `U128`, and another crate can turn it on. A missing
            // arm is then a compile error here, rather than a 128-bit number
            // quietly taking some other path.
        };
        tape.extend_from_slice(&(u64::from(kind) | skip << 32).to_le_bytes());
        tape.extend_from_slice(&payload.to_le_bytes());
    }
    Ok((tape, arena))
}

/// The byte offset of the first unpaired UTF-16 surrogate escape, if any.
///
/// `simd-json` takes `"\ud800"` and decodes it to U+0000 rather than refusing
/// it. That is silent corruption: a lone surrogate becomes a NUL no one can
/// tell from a real `\u0000`. RFC 8259 section 7 requires the pair, and
/// `serde_json` refuses it, so this scan restores the standard answer.
///
/// It matters only for a document that parses, where every backslash is in a
/// string and starts a valid escape, so a `u` after an escape's backslash is a
/// `\u` escape and nothing else. A document with no backslash costs one
/// `memchr` pass.
fn first_lone_surrogate(bytes: &[u8]) -> Option<usize> {
    fn hex4(b: &[u8]) -> Option<u32> {
        let s = std::str::from_utf8(b.get(..4)?).ok()?;
        u32::from_str_radix(s, 16).ok()
    }
    let mut i = 0usize;
    while i < bytes.len() {
        if bytes.get(i) != Some(&b'\\') {
            i += memchr::memchr(b'\\', bytes.get(i..)?)?;
            continue;
        }
        if bytes.get(i + 1) == Some(&b'u') {
            match bytes.get(i + 2..).and_then(hex4) {
                // A high surrogate is legal only right before a low one. Both
                // are consumed, so the low half is never taken for a lone one.
                Some(hi @ 0xD800..=0xDBFF) => {
                    let _ = hi;
                    let tail = bytes.get(i + 6..).unwrap_or(&[]);
                    let paired = tail.starts_with(b"\\u")
                        && tail
                            .get(2..)
                            .and_then(hex4)
                            .is_some_and(|lo| (0xDC00..=0xDFFF).contains(&lo));
                    if !paired {
                        return Some(i);
                    }
                    i += 12;
                }
                Some(0xDC00..=0xDFFF) => return Some(i),
                _ => i += 6,
            }
            continue;
        }
        // Any other escape: consuming both bytes is what stops an escaped
        // backslash from being read as the start of one.
        i += 2;
    }
    None
}

/// `s` as a JSON string literal, escapes and all.
pub(crate) fn write_string(out: &mut Vec<u8>, s: &str) {
    out.push(b'"');
    for c in s.chars() {
        match c {
            '"' => out.extend_from_slice(b"\\\""),
            '\\' => out.extend_from_slice(b"\\\\"),
            '\n' => out.extend_from_slice(b"\\n"),
            '\r' => out.extend_from_slice(b"\\r"),
            '\t' => out.extend_from_slice(b"\\t"),
            '\u{8}' => out.extend_from_slice(b"\\b"),
            '\u{c}' => out.extend_from_slice(b"\\f"),
            c if (c as u32) < 0x20 => {
                out.extend_from_slice(format!("\\u{:04x}", c as u32).as_bytes());
            }
            c => {
                let mut buf = [0u8; 4];
                out.extend_from_slice(c.encode_utf8(&mut buf).as_bytes());
            }
        }
    }
    out.push(b'"');
}

/// `f` so it reads back as a JSON number and as a float.
///
/// `Display` writes `1` for `1.0`, which reads back as an integer, a
/// different kind. `Debug` is the same shortest round-trip form but always
/// writes the point. A Float is never NaN or infinite (`docs/semantics.md`),
/// but JSON has neither, so one would be `null`, as `JSON.stringify` has it.
pub(crate) fn write_float(out: &mut Vec<u8>, f: f64) {
    if f.is_finite() {
        out.extend_from_slice(format!("{f:?}").as_bytes());
    } else {
        out.extend_from_slice(b"null");
    }
}

/// Whether `s` is a JSON number, by RFC 8259's grammar.
///
/// `Json.Number` carries its text straight into the output, and the text can
/// come from a caller, with nothing in its type to say it is a number. So it
/// is checked here, where it becomes output: one hand-built value that is not
/// a number would otherwise make the whole document fail to parse at the far
/// end, for a reason the sender cannot see.
pub(crate) fn is_number(s: &[u8]) -> bool {
    let mut i = 0usize;
    let digits = |i: &mut usize| {
        let start = *i;
        while matches!(s.get(*i), Some(b'0'..=b'9')) {
            *i += 1;
        }
        *i > start
    };
    if s.get(i) == Some(&b'-') {
        i += 1;
    }
    // A lone `0`, or a digit from 1 and the rest: a leading zero is not
    // JSON, so `01` fails on the end check.
    if s.get(i) == Some(&b'0') {
        i += 1;
    } else if !digits(&mut i) {
        return false;
    }
    if s.get(i) == Some(&b'.') {
        i += 1;
        if !digits(&mut i) {
            return false;
        }
    }
    if matches!(s.get(i), Some(b'e' | b'E')) {
        i += 1;
        if matches!(s.get(i), Some(b'+' | b'-')) {
            i += 1;
        }
        if !digits(&mut i) {
            return false;
        }
    }
    i == s.len()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A parsed document on a heap of its own.
    fn doc(src: &str) -> (Heap, Bits, Bits) {
        let (tape, arena) = parse(src.as_bytes()).unwrap_or_else(|e| panic!("{}", e.message));
        let mut heap = Heap::default();
        let t = binary::make(&mut heap, &tape, tape.len() as u64 * 8).expect("room");
        let a = binary::make(&mut heap, &arena, arena.len() as u64 * 8).expect("room");
        let (t, a) = (
            binary::bits(&heap, t).expect("a binary"),
            binary::bits(&heap, a).expect("a binary"),
        );
        (heap, t, a)
    }

    #[test]
    fn a_field_is_found_past_a_nested_object() {
        let (heap, tape, arena) = doc(r#"{"a": {"x": [1, 2, {"y": 3}]}, "b": "found"}"#);
        let t = Tape {
            heap: &heap,
            tape,
            arena,
        };
        let b = t.field(0, b"b").expect("a b");
        assert_eq!(t.string(b), Some(b"found".to_vec()));
        assert_eq!(t.field(0, b"c"), None);
        assert_eq!(t.len(0), Some(2));
    }

    #[test]
    fn duplicate_keys_both_survive_and_the_first_wins() {
        let (heap, tape, arena) = doc(r#"{"k": 1, "k": 2}"#);
        let t = Tape {
            heap: &heap,
            tape,
            arena,
        };
        let first = t.field(0, b"k").expect("a k");
        assert_eq!(t.int(first), Some(1.into()));
        let members = t.members(0).expect("an object");
        assert_eq!(members.len(), 2);
        assert_eq!(t.int(members[1].1), Some(2.into()));
    }

    /// An `Int` has no bounds, so an integer past `i64::MAX` is one too.
    #[test]
    fn an_integer_past_i64_max_is_an_int() {
        let (heap, tape, arena) =
            doc("[9223372036854775807, 18446744073709551615, -9223372036854775808]");
        let t = Tape {
            heap: &heap,
            tape,
            arena,
        };
        let at = t.elements(0).expect("an array");
        assert_eq!(t.int(at[0]), Some(i64::MAX.into()));
        assert_eq!(t.int(at[1]), Some(u64::MAX.into()));
        assert_eq!(t.int(at[2]), Some(i64::MIN.into()));
        assert_eq!(t.kind(at[1]), Some(KIND_INT));
    }

    /// Past 64 bits the parser underneath has no integer to put it in.
    #[test]
    fn an_integer_past_64_bits_does_not_parse() {
        assert!(parse(b"[18446744073709551616]").is_err());
    }

    #[test]
    fn ten_thousand_deep_nesting_parses_and_walks() {
        let src = format!("{}{}", "[".repeat(10_000), "]".repeat(10_000));
        let (heap, tape, arena) = doc(&src);
        let t = Tape {
            heap: &heap,
            tape,
            arena,
        };
        let mut at = 0;
        for _ in 0..9_999 {
            at = t.element(at, 0).expect("one deeper");
        }
        assert_eq!(t.len(at), Some(0));
    }

    #[test]
    fn strings_are_unescaped() {
        let (heap, tape, arena) = doc(r#"["a\"b\\c\n\u00e9\ud83d\ude00"]"#);
        let t = Tape {
            heap: &heap,
            tape,
            arena,
        };
        let s = t.string(t.element(0, 0).expect("one")).expect("a string");
        assert_eq!(String::from_utf8(s).expect("UTF-8"), "a\"b\\c\né😀");
    }

    #[test]
    fn a_lone_surrogate_is_refused_and_a_pair_is_not() {
        assert!(parse(br#"["\ud800"]"#).is_err());
        assert!(parse(br#"["\udc00"]"#).is_err());
        assert!(parse(br#"["\ud800x"]"#).is_err());
        assert!(parse(br#"["\ud83d\ude00"]"#).is_ok());
        assert!(parse(br#"["\\ud800"]"#).is_ok());
        assert_eq!(
            first_lone_surrogate(br#"{"k": "ok", "v": "\udfff"}"#),
            Some(18)
        );
    }

    #[test]
    fn floats_keep_their_point_and_strings_their_escapes() {
        let mut out = Vec::new();
        for f in [1.0, -0.5, 1e300, 0.1] {
            write_float(&mut out, f);
            out.push(b' ');
        }
        write_string(&mut out, "q\"b\\n\n\u{1}é");
        assert_eq!(
            String::from_utf8(out).expect("UTF-8"),
            "1.0 -0.5 1e300 0.1 \"q\\\"b\\\\n\\n\\u0001é\""
        );
    }

    #[test]
    fn only_a_json_number_is_a_number() {
        for s in ["0", "-0", "12", "1.5", "1e10", "1E+2", "-3.25e-7"] {
            assert!(is_number(s.as_bytes()), "{s}");
        }
        for s in ["", "-", "01", "1.", ".5", "1e", "+1", "NaN", "1 ", "0x1"] {
            assert!(!is_number(s.as_bytes()), "{s}");
        }
    }

    /// A node the walk reaches that is not on the tape is `None`, and so is
    /// a node whose skip is 0, which would stop a walk moving.
    #[test]
    fn a_node_off_the_tape_is_none() {
        let mut heap = Heap::default();
        let mut bytes = Vec::new();
        let mut node = |kind: u8, skip: u64, payload: u64| {
            bytes.extend_from_slice(&(u64::from(kind) | skip << 32).to_le_bytes());
            bytes.extend_from_slice(&payload.to_le_bytes());
        };
        // An array that says it has 3 elements, holding one, then a zero skip.
        node(K_ARRAY, 2, 3);
        node(K_NULL, 1, 0);
        node(K_NULL, 0, 0);
        let t = binary::make(&mut heap, &bytes, bytes.len() as u64 * 8).expect("room");
        let a = binary::make(&mut heap, &[], 0).expect("room");
        let tape = Tape {
            heap: &heap,
            tape: binary::bits(&heap, t).expect("a binary"),
            arena: binary::bits(&heap, a).expect("a binary"),
        };
        assert!(tape.element(0, 0).is_some());
        assert_eq!(tape.element(0, 1), None);
        assert_eq!(tape.element(0, 2), None);
        assert_eq!(tape.elements(0), None);
        assert_eq!(tape.kind(99), None);
    }
}
