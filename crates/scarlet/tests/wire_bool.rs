//! These test `Bool` across the typed wire, end to end through `al run`. To the
//! descriptor, `True` and `False` are a two-constructor `Data` type, but to the
//! runtime they're an immediate. So the typed encoder's `Data` arm has to handle
//! an unboxed constructor and find its tag through the pre-built word. An arm
//! that only handles cells panics in debug and writes no body in release, and
//! `decode` then reports `Truncated`. Inside a closure's captures `Bool` has a
//! tag of its own (`wire_closures.rs`), so this file covers only the typed path.
//!
//! `Nil` is the control. The prelude's `Nil` is a frozen cell at runtime
//! (`Op::PushNil` pushes the `Unit` ABI slot's pre-built constructor), so it
//! takes the ordinary `Data` path with an empty body.

mod common;

use common::run_outputs;

/// Round-trips both values and `Nil`, and pins the tag as a byte too. `True` is
/// variant 0 because `scarlet.sca` declares it first, so the body of
/// `wire.encode(True)` is the single byte 0 after the eleven-byte header. `Nil`
/// has only one constructor, so it has no body at all.
#[test]
#[ignore = "needs the VM"]
fn true_false_and_nil_round_trip_and_the_tag_is_the_declared_index() {
    run_outputs(
        "import scarlet/binary\n\
         import scarlet/wire\n\
         pub fn main() {\n\
         \tprintln(wire::decode(wire::encode(True)) == Ok(True))\n\
         \tprintln(wire::decode(wire::encode(False)) == Ok(False))\n\
         \tprintln(wire::decode(wire::encode(Nil)) == Ok(Nil))\n\
         \tprintln(binary::slice_bytes(wire::encode(True), 11, 1) == Ok(<<0>>))\n\
         \tprintln(binary::slice_bytes(wire::encode(False), 11, 1) == Ok(<<1>>))\n\
         \tprintln(binary::byte_size(wire::encode(Nil)))\n\
         }\n",
        "True\nTrue\nTrue\nTrue\nTrue\n11\n",
    );
}

/// Round-trips a record holding a `Bool` beside an `Int`, and an `Array(Bool)`.
/// This puts the immediate under a `Data` node as a field and as an element
/// rather than at the root. The test branches on the decoded `Bool` as well as
/// comparing it, which a cell standing in for it couldn't get through.
#[test]
#[ignore = "needs the VM"]
fn a_record_with_a_bool_field_and_an_array_of_bools_round_trip() {
    run_outputs(
        "import scarlet/wire\n\
         type Flag {\n\
         \tFlag(on Bool, n Int)\n\
         }\n\
         pub fn main() {\n\
         \tf = Flag(True, 3)\n\
         \tmatch wire::decode(wire::encode(f)) {\n\
         \t\tOk(back) -> {\n\
         \t\t\tprintln(back == f)\n\
         \t\t\tprintln(if back.on { back.n } else { 0 })\n\
         \t\t}\n\
         \t\tErr(_) -> println('refused')\n\
         \t}\n\
         \txs = [True, False, True]\n\
         \tmatch wire::decode(wire::encode(xs)) {\n\
         \t\tOk(ys) -> {\n\
         \t\t\tprintln(ys == xs)\n\
         \t\t\tprintln(ys == [True, True, True])\n\
         \t\t}\n\
         \t\tErr(_) -> println('refused')\n\
         \t}\n\
         }\n",
        "True\n3\nTrue\nFalse\n",
    );
}
