//! Closures across the wire, end to end through `al run`. A function value
//! encodes as the run that made it, its function index and its captures —
//! each capture self-described — and decodes, in that run, to a closure that
//! runs the same body over the same captures.
//!
//! Every test here CALLS the decoded closure and checks what it computes, so
//! a decoder that rebuilt a plausible-looking different closure would pass
//! `==` against nothing and these against less. The capture forms are the
//! runtime's, not the type system's: `Bool` and `Nil` are immediates, a big
//! integer is boxed, a range stores no elements, a record carries its names —
//! and each has a row of its own in `wire.scrl`'s capture table.
//!
//! The tampering tests cut the bytes by position. A closure row is the
//! eleven-byte header, the sixteen-byte run, then the function index as a
//! varint, then the capture count; with the stdlib ahead of it a user
//! function's index is two bytes, so the tests slice from the end rather
//! than assume a width.

mod common;

use common::run_outputs;

/// The ticket's first case: an `Int` and a `String` captured, the decoded
/// copy called with the right result. `apply` pins the type `decode` is used
/// at, since `println` alone would leave the return polymorphic.
#[test]
#[ignore = "needs the VM"]
fn a_closure_capturing_an_int_and_a_string_round_trips_and_is_called() {
    run_outputs(
        "import scarlet/wire\n\
         fn apply(f fn(Int) String, x Int) String {\n\
         \tf(x)\n\
         }\n\
         pub fn main() {\n\
         \tn = 5\n\
         \ts = 'x'\n\
         \tf = fn(a) { '${s}${a + n}' }\n\
         \tmatch wire.decode(wire.encode(f)) {\n\
         \t\tOk(g) -> {\n\
         \t\t\tprintln(apply(g, 1))\n\
         \t\t\tprintln(apply(g, 1) == apply(f, 1))\n\
         \t\t\tprintln(g == f)\n\
         \t\t}\n\
         \t\tErr(_) -> println('refused')\n\
         \t}\n\
         }\n",
        "x6\nTrue\nTrue\n",
    );
}

/// Every other capture form the ticket names, in one closure: a record, a
/// list, a map, a `Bool`, a `Nil`, a `Float`, an integer outside the
/// immediate range, and a range. The decoded copy hands them all back, and
/// each is checked against what went in rather than only `==` to itself.
#[test]
#[ignore = "needs the VM"]
fn a_closure_capturing_every_value_form_round_trips_and_is_called() {
    run_outputs(
        "import scarlet/map\n\
         import scarlet/wire\n\
         type P {\n\
         \tP(x Int, name String)\n\
         }\n\
         fn call(f fn() (P, Array(Int), Map(String, Int), Bool, Nil, Float, Int, Array(Int))) (P, Array(Int), Map(String, Int), Bool, Nil, Float, Int, Array(Int)) {\n\
         \tf()\n\
         }\n\
         pub fn main() {\n\
         \tp = P(1, 'p')\n\
         \txs = [1, 2, 3]\n\
         \tm = map.set(map.new(), 'k', 7)\n\
         \tb = True\n\
         \tz = Nil\n\
         \tfl = 1.5\n\
         \tbig = 9223372036854775807\n\
         \tr = 0..3\n\
         \tf = fn() { (p, xs, m, b, z, fl, big, r) }\n\
         \tmatch wire.decode(wire.encode(f)) {\n\
         \t\tOk(g) -> match call(g) {\n\
         \t\t\t(P(x, name), ys, m2, b2, Nil, fl2, big2, r2) -> {\n\
         \t\t\t\tprintln(x + 1)\n\
         \t\t\t\tprintln(name)\n\
         \t\t\t\tprintln(ys == [1, 2, 3])\n\
         \t\t\t\tprintln(map.get(m2, 'k') == Some(7))\n\
         \t\t\t\tprintln(b2)\n\
         \t\t\t\tprintln(fl2)\n\
         \t\t\t\tprintln(big2 == 9223372036854775807)\n\
         \t\t\t\tprintln(r2 == [0, 1, 2])\n\
         \t\t\t\tprintln(call(g) == call(f))\n\
         \t\t\t}\n\
         \t\t}\n\
         \t\tErr(_) -> println('refused')\n\
         \t}\n\
         }\n",
        "2\np\nTrue\nTrue\nTrue\n1.5\nTrue\nTrue\nTrue\n",
    );
}

/// A closure capturing a closure: the decoded outer calls the decoded inner,
/// so both bodies run over their own captures.
#[test]
#[ignore = "needs the VM"]
fn a_closure_capturing_a_closure_round_trips_and_is_called_through_both() {
    run_outputs(
        "import scarlet/wire\n\
         fn apply(f fn(Int) Int, x Int) Int {\n\
         \tf(x)\n\
         }\n\
         pub fn main() {\n\
         \tn = 3\n\
         \tadd = fn(x) { x + n }\n\
         \ttwice = fn(x) { add(add(x)) }\n\
         \tmatch wire.decode(wire.encode(twice)) {\n\
         \t\tOk(g) -> {\n\
         \t\t\tprintln(apply(g, 1))\n\
         \t\t\tprintln(apply(g, 10) == apply(twice, 10))\n\
         \t\t}\n\
         \t\tErr(_) -> println('refused')\n\
         \t}\n\
         }\n",
        "7\nTrue\n",
    );
}

/// Handles inside captures are the identity row. A subject captured by a
/// closure is the mailbox it was: a message sent through the decoded copy
/// arrives at the original's owner. A pid captured by a closure is the
/// process it was.
#[test]
#[ignore = "needs the VM"]
fn a_closure_capturing_a_subject_and_a_pid_round_trips_and_the_copies_are_used() {
    run_outputs(
        "import scarlet/process\n\
         import scarlet/wire\n\
         fn run(f fn() Nil) Nil {\n\
         \tf()\n\
         }\n\
         fn check(f fn(process.Pid) Bool, p process.Pid) Bool {\n\
         \tf(p)\n\
         }\n\
         pub fn main() {\n\
         \tinbox = process.subject()\n\
         \tsend_hi = fn() { process.send(inbox, 'through the copy') }\n\
         \tmatch wire.decode(wire.encode(send_hi)) {\n\
         \t\tOk(g) -> {\n\
         \t\t\trun(g)\n\
         \t\t\tprintln(process.receive(inbox))\n\
         \t\t}\n\
         \t\tErr(_) -> println('refused')\n\
         \t}\n\
         \tme = process.self()\n\
         \tis_me = fn(p) { p == me }\n\
         \tmatch wire.decode(wire.encode(is_me)) {\n\
         \t\tOk(g) -> println(check(g, me))\n\
         \t\tErr(_) -> println('refused')\n\
         \t}\n\
         }\n",
        "through the copy\nTrue\n",
    );
}

/// A named function as a value, and a lambda closing over nothing: the row
/// is the run, the index and a count of zero.
#[test]
#[ignore = "needs the VM"]
fn a_zero_capture_closure_round_trips_and_is_called() {
    run_outputs(
        "import scarlet/wire\n\
         fn apply(f fn(Int) Int, x Int) Int {\n\
         \tf(x)\n\
         }\n\
         fn helper(a Int) Int {\n\
         \ta * 2\n\
         }\n\
         pub fn main() {\n\
         \tmatch wire.decode(wire.encode(helper)) {\n\
         \t\tOk(g) -> println(apply(g, 3))\n\
         \t\tErr(_) -> println('refused')\n\
         \t}\n\
         \tmatch wire.decode(wire.encode(fn(x) { x + 1 })) {\n\
         \t\tOk(g) -> println(apply(g, 3))\n\
         \t\tErr(_) -> println('refused')\n\
         \t}\n\
         }\n",
        "6\n4\n",
    );
}

/// A record with a `fn` field crosses with its closure, and the closure
/// still runs.
#[test]
#[ignore = "needs the VM"]
fn a_record_with_a_fn_field_round_trips_and_the_field_is_called() {
    run_outputs(
        "import scarlet/wire\n\
         pub type Handler {\n\
         \tHandler(name String, run fn(Int) Int)\n\
         }\n\
         fn fire(h Handler, x Int) Int {\n\
         \th.run(x)\n\
         }\n\
         fn name_of(h Handler) String {\n\
         \th.name\n\
         }\n\
         pub fn main() {\n\
         \tk = 40\n\
         \th = Handler('h', fn(x) { x + k })\n\
         \tmatch wire.decode(wire.encode(h)) {\n\
         \t\tOk(back) -> {\n\
         \t\t\tprintln(name_of(back))\n\
         \t\t\tprintln(fire(back, 2))\n\
         \t\t}\n\
         \t\tErr(_) -> println('refused')\n\
         \t}\n\
         }\n",
        "h\n42\n",
    );
}

/// The bytes of a closure with its sixteen run-identity bytes zeroed are
/// another run's, and the refusal is `OtherRun`, this run's identity first.
/// Bytes 11..27 are the run, as for a handle.
#[test]
#[ignore = "needs the VM"]
fn a_closure_from_another_run_is_refused_with_other_run() {
    run_outputs(
        "import scarlet/binary\n\
         import scarlet/wire\n\
         import scarlet/wire.{OtherRun}\n\
         fn apply(f fn(Int) Int, x Int) Int {\n\
         \tf(x)\n\
         }\n\
         pub fn main() {\n\
         \tbytes = wire.encode(fn(x) { x + 1 })\n\
         \tsize = binary.byte_size(bytes)\n\
         \thead = binary.slice_bytes(bytes, 0, 11) or <<>>\n\
         \ttail = binary.slice_bytes(bytes, 27, size - 27) or <<>>\n\
         \tzeros = <<0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0>>\n\
         \tmatch wire.decode(binary.concat([head, zeros, tail])) {\n\
         \t\tOk(g) -> println(apply(g, 1))\n\
         \t\tErr(OtherRun(mine, theirs)) -> {\n\
         \t\t\tprintln(binary.byte_size(mine) == 16)\n\
         \t\t\tprintln(theirs == zeros)\n\
         \t\t\tprintln(mine == theirs)\n\
         \t\t}\n\
         \t\tErr(_) -> println('some other refusal')\n\
         \t}\n\
         }\n",
        "True\nTrue\nFalse\n",
    );
}

/// A function index past the program's table is `Malformed` at the index's
/// offset, never a panic at `program.functions[..]`. The index is replaced
/// by the three-byte varint for 2^20, which no program has that many
/// functions for; the zero-capture closure's row ends with its count, so the
/// index is everything between the run and the last byte.
#[test]
#[ignore = "needs the VM"]
fn a_tampered_function_index_is_malformed_not_a_panic() {
    run_outputs(
        "import scarlet/binary\n\
         import scarlet/wire\n\
         import scarlet/wire.{Malformed}\n\
         fn apply(f fn(Int) Int, x Int) Int {\n\
         \tf(x)\n\
         }\n\
         pub fn main() {\n\
         \tbytes = wire.encode(fn(x) { x + 1 })\n\
         \tsize = binary.byte_size(bytes)\n\
         \thead = binary.slice_bytes(bytes, 0, 27) or <<>>\n\
         \tcount = binary.slice_bytes(bytes, size - 1, 1) or <<>>\n\
         \tprintln(count == <<0>>)\n\
         \tmatch wire.decode(binary.concat([head, <<128, 128, 64>>, count])) {\n\
         \t\tOk(g) -> println(apply(g, 1))\n\
         \t\tErr(Malformed(offset, what)) -> {\n\
         \t\t\tprintln(offset)\n\
         \t\t\tprintln(what)\n\
         \t\t}\n\
         \t\tErr(_) -> println('some other refusal')\n\
         \t}\n\
         }\n",
        "True\n27\nclosure function index out of range\n",
    );
}

/// A capture count that is not the function's is `Malformed` at the count's
/// offset — here a zero-capture lambda's row claiming one `Nil` capture —
/// rather than a capture-index error when the body runs.
#[test]
#[ignore = "needs the VM"]
fn a_tampered_capture_count_is_malformed() {
    run_outputs(
        "import scarlet/binary\n\
         import scarlet/wire\n\
         import scarlet/wire.{Malformed}\n\
         fn apply(f fn(Int) Int, x Int) Int {\n\
         \tf(x)\n\
         }\n\
         pub fn main() {\n\
         \tbytes = wire.encode(fn(x) { x + 1 })\n\
         \tsize = binary.byte_size(bytes)\n\
         \tup_to_count = binary.slice_bytes(bytes, 0, size - 1) or <<>>\n\
         \tmatch wire.decode(binary.concat([up_to_count, <<1, 11>>])) {\n\
         \t\tOk(g) -> println(apply(g, 1))\n\
         \t\tErr(Malformed(offset, what)) -> {\n\
         \t\t\tprintln(offset == size - 1)\n\
         \t\t\tprintln(what)\n\
         \t\t}\n\
         \t\tErr(_) -> println('some other refusal')\n\
         \t}\n\
         }\n",
        "True\nclosure capture count does not match its function\n",
    );
}

/// The other direction: a one-capture closure's row cut short and its count
/// rewritten to zero. Without the check the closure would be built over no
/// captures and the body's `PushCapture` would fail at the call; with it the
/// bytes are refused at the count's offset. The row ends `count, tag,
/// zigzag(5)`, so the count is the third-to-last byte.
#[test]
#[ignore = "needs the VM"]
fn a_capture_count_short_of_the_functions_is_malformed() {
    run_outputs(
        "import scarlet/binary\n\
         import scarlet/wire\n\
         import scarlet/wire.{Malformed}\n\
         fn apply(f fn(Int) Int, x Int) Int {\n\
         \tf(x)\n\
         }\n\
         pub fn main() {\n\
         \tn = 5\n\
         \tbytes = wire.encode(fn(x) { x + n })\n\
         \tsize = binary.byte_size(bytes)\n\
         \tcount = binary.slice_bytes(bytes, size - 3, 1) or <<>>\n\
         \tprintln(count == <<1>>)\n\
         \tup_to_count = binary.slice_bytes(bytes, 0, size - 3) or <<>>\n\
         \tmatch wire.decode(binary.concat([up_to_count, <<0>>])) {\n\
         \t\tOk(g) -> println(apply(g, 1))\n\
         \t\tErr(Malformed(offset, what)) -> {\n\
         \t\t\tprintln(offset == size - 3)\n\
         \t\t\tprintln(what)\n\
         \t\t}\n\
         \t\tErr(_) -> println('some other refusal')\n\
         \t}\n\
         }\n",
        "True\nTrue\nclosure capture count does not match its function\n",
    );
}

/// A tag byte not in the capture table is `Malformed` at the tag's offset.
/// The closure captures one `Int`, so its row ends `tag, zigzag(5)`; the
/// tag is the second-to-last byte.
#[test]
#[ignore = "needs the VM"]
fn a_tampered_capture_tag_is_malformed() {
    run_outputs(
        "import scarlet/binary\n\
         import scarlet/wire\n\
         import scarlet/wire.{Malformed}\n\
         fn apply(f fn(Int) Int, x Int) Int {\n\
         \tf(x)\n\
         }\n\
         pub fn main() {\n\
         \tn = 5\n\
         \tbytes = wire.encode(fn(x) { x + n })\n\
         \tsize = binary.byte_size(bytes)\n\
         \tup_to_tag = binary.slice_bytes(bytes, 0, size - 2) or <<>>\n\
         \tvalue = binary.slice_bytes(bytes, size - 1, 1) or <<>>\n\
         \tprintln(value == <<10>>)\n\
         \tmatch wire.decode(binary.concat([up_to_tag, <<255>>, value])) {\n\
         \t\tOk(g) -> println(apply(g, 1))\n\
         \t\tErr(Malformed(offset, what)) -> {\n\
         \t\t\tprintln(offset == size - 2)\n\
         \t\t\tprintln(what)\n\
         \t\t}\n\
         \t\tErr(_) -> println('some other refusal')\n\
         \t}\n\
         }\n",
        "True\nTrue\ncapture tag is not in the table\n",
    );
}
