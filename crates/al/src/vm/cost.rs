//! Arena word costs of every heap object the VM allocates, mirroring the
//! object layouts: `[header][payload words…]`.
//!
//! This is the budget side of the rooting rule (idea 5 of `al_core::heap`'s
//! front door): every allocating opcode computes its worst-case need from
//! these BEFORE popping operands (while they are still rooted on the VM
//! stack) and passes it to `VM::ensure`; allocation after `ensure` is a pure
//! bump that never collects, and a debug build proves opcode-by-opcode that
//! each budget covers everything the opcode allocates (the watermark assert
//! in `ProcHeap::alloc_young`). Costs are worst-case upper bounds:
//! over-budgeting wastes a little headroom, under-budgeting is exactly the
//! dangling-pointer bug the watermark exists to catch.

/// Every arena object carries one header word (type tag, payload length
/// in words, forwarding bit, off-heap-list bit).
pub const HEADER: usize = 1;

/// Words covering `n` bytes of inline data.
pub const fn bytes(n: usize) -> usize {
    n.div_ceil(8)
}

/// Str: header + byte length + inline UTF-8.
pub const fn str(len: usize) -> usize {
    HEADER + 1 + bytes(len)
}

/// Tuple: header + element count + element values.
pub const fn tuple(n: usize) -> usize {
    HEADER + 1 + n
}

/// Closure: header + func_idx + capture count + capture values.
pub const fn closure(captures: usize) -> usize {
    HEADER + 2 + captures
}

/// Enum: header + type_id + hash + enum-name ref + variant-name ref +
/// field-labels ref + payload count + payload values. The name and label
/// words are single references — their text lives in the frozen area /
/// constant pool, never in the process arena.
pub const fn enum_(payload: usize) -> usize {
    HEADER + 6 + payload
}

/// Binary box: header + refcounted backing handle (two words) + bit
/// offset + bit length + off-heap list link. The bytes themselves stay
/// off-heap behind the shared `Arc` and cost no arena words.
pub const BINARY: usize = HEADER + 5;

/// Boxed integer (the spill box for ints outside the NaN-box's 48-bit
/// inline payload range): header + the i64. The box lives in the process
/// arena (`Value::int_in`) and is charged like any other allocation, in
/// one of two ways: opcodes whose arm budget already covers a possible
/// spill include this constant and build the result with `int_value`
/// (never collects); standalone integer results go through `boxed_int`,
/// whose cold `spill_int` half runs its own `ensure(BIG_INT)`. Range
/// materialization budgets one box per element when either bound is
/// out of inline range (`peek_seq` in `gc.rs`).
pub const BIG_INT: usize = HEADER + 1;

/// Range: header + start + end.
pub const RANGE: usize = HEADER + 2;

/// Map handle: header + backing discriminant. The `Env` backing stores no
/// entries in the arena (it reads through to `std::env`), so this is the whole
/// cost of `process.env`.
pub const MAP: usize = HEADER + 1;

/// Socket handles pack into the NaN box — no arena words.
pub const SOCKET: usize = 0;

/// An Option/Result wrapper (`Some`/`Ok`/`Err`) around one value.
pub const WRAP: usize = enum_(1);

/// `NetError` built by `net_error_value`: one variant carrying at most
/// an errno payload.
pub const NET_ERR: usize = enum_(1);

/// `IoError` built by `io_error_value`: one variant carrying the path.
pub const fn io_err(path_len: usize) -> usize {
    enum_(1) + str(path_len)
}

/// `IpAddress` built by `ip_address`: variant + textual address (45
/// bytes is the longest IPv6 form; 64 leaves slack for scope ids).
pub const IP_ADDR: usize = enum_(1) + str(64);

/// `SocketAddress` built by `socket_address`: variant + IpAddress + port.
pub const SOCK_ADDR: usize = enum_(2) + IP_ADDR;

/// `Socket{conn, peer}` record built by `make_socket` (excluding the
/// conn handle and the Ok wrapper, costed separately).
pub const SOCKET_RECORD: usize = enum_(2) + SOCK_ADDR;

/// Everything `adopt_connection` builds: the socket handle, the Socket
/// record (with its peer address), and the Ok wrapper.
pub const ADOPT: usize = SOCKET + SOCKET_RECORD + WRAP;

// ---- persistent vector ---------------
//
// Budget mirrors of the `al_core::bytecode::seq` cost model (its
// LEAF_WORDS / BRANCH_WORDS / ROOT_WORDS). `seq::cost_*` reads the
// live depth off a rooted root; these budgets are stated in terms of
// element COUNTS instead, because the VM sizes an opcode before
// popping its operands, and an operand may still be a lazy Range with
// no tree to read a depth from. Each formula repeats its `seq::cost_*`
// lemma word for word, substituting `seq_levels(len) + 2` — the packed
// depth plus two headroom levels — for the live depth.
//
// The substitution bounds WORDS, not depth. Slicing shrinks `len`
// while leaving depth untouched, so a heavily sliced tree can be
// deeper than `seq_levels(len) + 2`; what it cannot be is deep AND
// wide along a copied path. A copied branch costs `3 + 2*width` words
// (`seq::branch_from`), reaching the budgeted SEQ_NODE = 67 only at
// full width, and path width is paid for by elements: a path node's
// off-path children are disjoint nonempty subtrees, so the widths
// along any root-to-leaf path sum to under `len + depth`. Wide levels
// are the ones `seq_levels(len)` accounts for; the surplus levels
// slicing leaves are single-child chains — `seq::tree_take` and
// `seq::tree_drop` thin only the two cut paths and keep every off-cut
// sibling intact, and `seq::collapse` strips single-child tops — each
// costing 5 of the budgeted 67 words, so the two headroom levels alone
// absorb 26 of them. Sparse shapes cannot compound past that: depth
// grows only over width at the moment of growth (a push deepens only
// over a full 32-wide edge spine — `seq::try_push` returns None only
// past 32-child nodes — and a concat only when its rebalanced top run
// still exceeds 32 nodes, which the E_MAX invariant permits only over
// (32-2)*32 occupied child slots), a push widens a node only by
// hanging a fresh element-bearing leaf under it, and concat's
// rebalance repacks any seam where two slice-thinned edges meet.
//
// Under-coverage here would be a collection mid-opcode with operands
// already popped — the dangling-pointer bug. The watermark assert in
// `ProcHeap::alloc_young` is the backstop: a debug build checks every
// allocation against the words the opcode's `ensure` secured, so a
// shape this analysis missed trips the assert at the faulting
// allocation instead of dangling.

/// Fan-out of the persistent vector's tree nodes.
pub const SEQ_BRANCH: usize = 32;

/// Leaf node: header + count + up to 32 element slots.
pub const SEQ_LEAF: usize = 2 + SEQ_BRANCH;

/// Branch node: header + meta + up to 32 children plus their size table
/// (relaxed-node form — the worst case).
pub const SEQ_NODE: usize = 3 + 2 * SEQ_BRANCH;

/// Root object words.
pub const SEQ_ROOT: usize = 6;

/// Tree levels of a perfectly packed vector of `len` elements.
pub fn seq_levels(len: usize) -> usize {
    let mut levels = 1;
    let mut cap = SEQ_BRANCH;
    while cap < len {
        cap = cap.saturating_mul(SEQ_BRANCH);
        levels += 1;
    }
    levels
}

/// Building a fresh vector of `n` elements (`seq::from_slice`): every
/// leaf (plus a tail) and the interior spine above them.
pub fn seq_build(n: usize) -> usize {
    let leaves = n.div_ceil(SEQ_BRANCH).max(1);
    let mut words = SEQ_ROOT + (leaves + 1) * SEQ_LEAF;
    let mut nodes = leaves;
    while nodes > 1 {
        nodes = nodes.div_ceil(SEQ_BRANCH);
        words += nodes * SEQ_NODE;
    }
    words
}

/// One push/update: `seq::cost_push` charges `depth + 2` branches (path
/// copy + one new edge spine + one root growth); `+ 4` is that `+ 2`
/// plus the two headroom levels above. `seq::cost_update` charges
/// strictly less, so this budget covers both.
pub fn seq_push(len: usize) -> usize {
    SEQ_ROOT + 2 * SEQ_LEAF + (seq_levels(len) + 4) * SEQ_NODE
}

/// One split into a prefix and a suffix (take + skip): two path copies,
/// one along each cut edge — `seq::cost_slice` twice. The lemma charges
/// `depth + 1` branches per cut; `+ 3` is that `+ 1` plus the two
/// headroom levels above.
pub fn seq_split(len: usize) -> usize {
    2 * (SEQ_ROOT + 2 * SEQ_LEAF + (seq_levels(len) + 3) * SEQ_NODE)
}

/// Concatenation: `seq::cost_concat` term by term — two boundary-buffer
/// pushes, one rebalance per level rebuilding up to `2 * SEQ_BRANCH + 2`
/// nodes, top grouping, final root. The lemma scales each term by
/// `max(depth(l), depth(r)) + 2`; `levels` is that term under the same
/// substitution — `seq_levels` is monotone and both operand lengths are
/// below `total`, so the words argument above covers whichever operand
/// is deeper.
pub fn seq_concat(total: usize) -> usize {
    let levels = seq_levels(total) + 4;
    2 * (SEQ_ROOT + 2 * SEQ_LEAF + levels * SEQ_NODE)
        + levels * (2 * SEQ_BRANCH + 2) * SEQ_NODE
        + 2 * SEQ_NODE
        + SEQ_ROOT
}

// ---- HTTP protocol ops (`vm::http`) ------------------------------------

/// Budget pre-scan ceiling for the HTTP ops: header/trailer blocks are
/// capped by `http`'s `MAX_HEAD` (64 KiB) well below this, so counting
/// line breaks in at most this many bytes always over-counts the fields
/// the parser can produce, never under-counts.
pub const HTTP_SCAN_CAP: usize = 128 * 1024;

/// `h1.parse_request` worst case over a head of `lines` CRLF-terminated
/// lines: the Done variant + method/path views + the header array, with
/// one Header enum + two views per line.
pub fn http_head(lines: usize) -> usize {
    enum_(5) + 2 * BINARY + seq_build(lines) + lines * (enum_(2) + 2 * BINARY)
}

/// `h1.chunk_decode` worst case: the Done variant + the decoded-body box
/// + the trailer array (same per-line shape as the head).
pub fn http_chunks(lines: usize) -> usize {
    enum_(3) + BINARY + seq_build(lines) + lines * (enum_(2) + 2 * BINARY)
}
