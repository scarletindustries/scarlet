//! A type no value has, across the wire, end to end through `al run`. A
//! `pub type Native` with no body that is none of the five handles is an
//! uninhabited node in a descriptor: never written, never read, and met only
//! where the walk never goes — the phantom argument of `Tagged(Native)`, the
//! element of an empty `Array(Native)`, the payload of a `None`.
//!
//! The decoder's half is that bytes steering it into the node are refused as
//! `Malformed` and never a panic. The only way to reach one is to forge a
//! count or a tag the encoder never writes, so each test here forges one
//! from the header of a value the program could encode.

mod common;

use common::run_outputs;

/// `Tagged(Native)` over an `Int`. The body is the `Int` alone — one byte,
/// zigzag 7 — and the value round trips to `==` with its field intact.
#[test]
#[ignore = "needs the VM"]
fn a_record_with_a_phantom_bodiless_argument_round_trips() {
    run_outputs(
        "import scarlet/binary\n\
         import scarlet/wire\n\
         pub type Native\n\
         type Tagged(t) {\n\
         \tTagged(value Int)\n\
         }\n\
         fn tag(n Int) Tagged(Native) {\n\
         \tTagged(n)\n\
         }\n\
         pub fn main() {\n\
         \tt = tag(7)\n\
         \tbytes = wire.encode(t)\n\
         \tprintln(binary.byte_size(bytes))\n\
         \tprintln(binary.slice_bytes(bytes, 11, 1) == Ok(<<14>>))\n\
         \tmatch wire.decode(bytes) {\n\
         \t\tOk(back) -> {\n\
         \t\t\tprintln(back == t)\n\
         \t\t\tprintln(back.value)\n\
         \t\t}\n\
         \t\tErr(_) -> println('refused')\n\
         \t}\n\
         }\n",
        "12\nTrue\nTrue\n7\n",
    );
}

/// The empty array is the one inhabitant of `Array(Native)` and round trips
/// as a count of zero. A forged count the input cannot cover is refused by
/// the count guard at the count's own offset, before anything is queued;
/// one the input does cover is refused at the element, where the walk meets
/// the node. Both are `Malformed`, and the program goes on running.
#[test]
#[ignore = "needs the VM"]
fn an_empty_array_of_a_bodiless_type_round_trips_and_forged_counts_are_malformed() {
    run_outputs(
        "import scarlet/binary\n\
         import scarlet/wire\n\
         import scarlet/wire.{Malformed}\n\
         pub type Native\n\
         fn empty() Array(Native) {\n\
         \t[]\n\
         }\n\
         fn report(r Result(Array(Native), wire.DecodeError), xs Array(Native)) {\n\
         \tmatch r {\n\
         \t\tOk(back) -> println(back == xs)\n\
         \t\tErr(Malformed(at, what)) -> println('${at} ${what}')\n\
         \t\tErr(_) -> println('some other refusal')\n\
         \t}\n\
         }\n\
         pub fn main() {\n\
         \txs = empty()\n\
         \tbytes = wire.encode(xs)\n\
         \tprintln(binary.byte_size(bytes))\n\
         \treport(wire.decode(bytes), xs)\n\
         \thead = binary.slice_bytes(bytes, 0, 11) or <<>>\n\
         \treport(wire.decode(binary.concat([head, <<1>>])), xs)\n\
         \treport(wire.decode(binary.concat([head, <<1, 0>>])), xs)\n\
         }\n",
        "12\nTrue\n11 count is larger than the remaining input can hold\n\
         12 no value of this type can exist\n",
    );
}

/// `Option(Native)`: `None` is a value and round trips as its tag; a forged
/// `Some` tag steers the decoder into the payload's node and is refused
/// there.
#[test]
#[ignore = "needs the VM"]
fn none_of_a_bodiless_type_round_trips_and_a_forged_some_is_malformed() {
    run_outputs(
        "import scarlet/binary\n\
         import scarlet/wire\n\
         import scarlet/wire.{Malformed}\n\
         pub type Native\n\
         fn absent() Option(Native) {\n\
         \tNone\n\
         }\n\
         fn report(r Result(Option(Native), wire.DecodeError), n Option(Native)) {\n\
         \tmatch r {\n\
         \t\tOk(back) -> println(back == n)\n\
         \t\tErr(Malformed(at, what)) -> println('${at} ${what}')\n\
         \t\tErr(_) -> println('some other refusal')\n\
         \t}\n\
         }\n\
         pub fn main() {\n\
         \tn = absent()\n\
         \tbytes = wire.encode(n)\n\
         \tprintln(binary.slice_bytes(bytes, 11, 1) == Ok(<<1>>))\n\
         \treport(wire.decode(bytes), n)\n\
         \thead = binary.slice_bytes(bytes, 0, 11) or <<>>\n\
         \treport(wire.decode(binary.concat([head, <<0>>])), n)\n\
         }\n",
        "True\nTrue\n12 no value of this type can exist\n",
    );
}

/// `decode` typed at the bodiless type itself. No program can produce its
/// header, so the test does, from `wire.scrl`'s fingerprint specification:
/// version 3, one node, root 0, tag 14 alone is `0xf5c99787f8ebc4ff`, written
/// little-endian behind `SW` 3. The same number is pinned in the descriptor
/// builder's own tests, so a disagreement between the two instruments fails
/// one of them. The body is empty, and the decoder is refused at byte 11 —
/// `Malformed`, never a panic, never a value.
#[test]
#[ignore = "needs the VM"]
fn decode_at_a_bodiless_type_is_malformed_rather_than_a_value_or_a_panic() {
    run_outputs(
        "import scarlet/wire\n\
         import scarlet/wire.{Malformed}\n\
         pub type Native\n\
         fn read(b Binary) Result(Native, wire.DecodeError) {\n\
         \twire.decode(b)\n\
         }\n\
         pub fn main() {\n\
         \tmatch read(<<'SW', 3, 255, 196, 235, 248, 135, 151, 201, 245>>) {\n\
         \t\tOk(_) -> println('a value of a type that has none')\n\
         \t\tErr(Malformed(at, what)) -> println('${at} ${what}')\n\
         \t\tErr(_) -> println('some other refusal')\n\
         \t}\n\
         }\n",
        "11 no value of this type can exist\n",
    );
}
