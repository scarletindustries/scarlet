use super::*;
use crate::OsHost;
use crate::bigint;
use crate::code::{BinaryId, EntryPc, Lists, Pc, Shares, TextId, Why, WhyId};
use crate::heap::{AuditFault, Cell, Fault};
use crate::value::{Float, SmallInt};
use num_bigint::BigInt;
use num_traits::ToPrimitive as _;
use scarlet_ir::core_ir::VariantRef;
use scarlet_ir::tivec::TiVec;

/// The fields the loop reads at every turn come first, where one load reaches
/// them, and the heap comes after everything else, so that its size cannot
/// move them.
#[test]
fn the_heap_is_behind_the_fields_the_loop_reads() {
    let stack = std::mem::offset_of!(Machine<'_, '_>, stack);
    let frames = std::mem::offset_of!(Machine<'_, '_>, frames);
    let passing = std::mem::offset_of!(Machine<'_, '_>, passing);
    let rest = std::mem::offset_of!(Machine<'_, '_>, rest);
    assert!(stack < frames && frames < passing && passing < rest);
    assert!(rest < 256, "{rest}");
    let heap = rest + std::mem::offset_of!(Rest<'_, '_>, heap);
    assert!(
        heap + size_of::<Heap>() == size_of::<Machine<'_, '_>>(),
        "{heap}"
    );
}

/// Run `src`, then release everything a finished run still holds, which
/// is the result and the globals. What is left is what reference counting
/// lost.
fn cells_left_after(src: &str) -> (String, usize) {
    cells_left_after_in(Vec::new(), Vec::new(), src)
}

fn cells_left_after_in(
    argv: Vec<String>,
    env: Vec<(String, String)>,
    src: &str,
) -> (String, usize) {
    let result = scarlet_core::compile::compile_text(src, None);
    assert!(result.success(), "{:?}", result.diagnostics());
    let program = result.into_runnable().expect("a clean compile is runnable");
    let code = crate::code::load(&program, Audit::Never).expect("the program loads");
    let mut host = OsHost::new(argv, env, Vec::new());
    let mut m = Machine::new(&code, &mut host);
    let ended = m.run().expect("the program runs");
    m.let_go(ended).expect("every count is right");
    assert!(
        m.frames.is_empty() && m.stack.iter().all(|v| v.bits() == Value::NIL.bits()),
        "a frame's registers outlived it"
    );
    let left = m.rest.heap.live();
    drop(m);
    let out = std::mem::take(host.out());
    (String::from_utf8(out).expect("UTF-8"), left)
}

#[test]
fn strings_made_and_joined_are_all_freed() {
    let (out, left) = cells_left_after(
        "fn greet(name String) String { 'hello, ${name}' }\n\
             pub fn main() {\n\
             \tprintln(greet('world'))\n\
             \tprintln('${1 + 1} and ${greet('you')}')\n\
             }\n",
    );
    assert_eq!(out, "hello, world\n2 and hello, you\n");
    assert_eq!(left, 0);
}

/// Each step makes a string and drops the last one. If a step leaked, the
/// heap would hold 10,000 cells at the end.
#[test]
fn a_loop_that_makes_strings_does_not_keep_them() {
    let (out, left) = cells_left_after(
        "fn spin(n Int, s String) String {\n\
             \tif n == 0 { s } else { spin(n - 1, '${n}') }\n\
             }\n\
             pub fn main() {\n\
             \tprintln(spin(10000, 'start'))\n\
             }\n",
    );
    assert_eq!(out, "1\n");
    assert_eq!(left, 0);
}

/// Each step makes a bigger big int and drops the last, and printing one
/// makes and frees its text.
#[test]
fn big_ints_made_in_a_loop_are_all_freed() {
    let (out, left) = cells_left_after(
        "fn grow(n Int, acc Int) Int {\n\
             \tif n == 0 { acc } else { grow(n - 1, acc * 3) }\n\
             }\n\
             pub fn main() {\n\
             \tbig = grow(200, 1)\n\
             \tprintln(big > 1)\n\
             \tprintln('${big / big}')\n\
             }\n",
    );
    assert_eq!(out, "True\n1\n");
    assert_eq!(left, 0);
}

/// Constructors hold references to what is in them. A list's cells, the
/// strings in them, and a record kept in a global are all freed.
#[test]
fn constructors_and_what_they_hold_are_all_freed() {
    let (out, left) = cells_left_after(
        "type L {\n\
             \tCons(h String, t L)\n\
             \tEnd\n\
             }\n\
             type Point {\n\
             \tPoint(x Int, y Int)\n\
             }\n\
             const origin = Point(x: 0, y: 0)\n\
             fn build(n Int, acc L) L {\n\
             \tif n == 0 { acc } else { build(n - 1, Cons('${n}', acc)) }\n\
             }\n\
             pub fn main() {\n\
             \tprintln(build(2, End))\n\
             \t_l = build(10000, End)\n\
             \tprintln(origin)\n\
             \tprintln(Some(origin))\n\
             }\n",
    );
    assert_eq!(
        out,
        "Cons(\n  1,\n  Cons(2, End)\n)\nPoint{ x: 0, y: 0 }\nSome(\n  Point{ x: 0, y: 0 }\n)\n"
    );
    assert_eq!(left, 0);
}

/// A match's arms read fields out of the value they match, and each field
/// read holds its own reference. Walking, rebuilding and dropping lists
/// of strings leaves nothing behind.
#[test]
fn what_a_match_reads_out_is_all_freed() {
    let (out, left) = cells_left_after(
        "type L {\n\
             \tCons(h String, t L)\n\
             \tEnd\n\
             }\n\
             fn build(n Int, acc L) L {\n\
             \tif n == 0 { acc } else { build(n - 1, Cons('${n}', acc)) }\n\
             }\n\
             fn rev(l L, acc L) L {\n\
             \tmatch l {\n\
             \t\tCons(h, t) -> rev(t, Cons(h, acc))\n\
             \t\tEnd -> acc\n\
             \t}\n\
             }\n\
             fn first(l L) String {\n\
             \tmatch l {\n\
             \t\tCons(h, _) -> h\n\
             \t\tEnd -> 'empty'\n\
             \t}\n\
             }\n\
             pub fn main() {\n\
             \tl = build(1000, End)\n\
             \tprintln(first(rev(l, End)))\n\
             \tprintln(first(l))\n\
             \tprintln(first(End))\n\
             }\n",
    );
    assert_eq!(out, "1000\n1\nempty\n");
    assert_eq!(left, 0);
}

/// A closure holds its captures, and each call to it holds the closure.
/// Closures made in a loop, returned, called and dropped leave nothing.
#[test]
fn closures_and_their_captures_are_all_freed() {
    let (out, left) = cells_left_after(
        "type Box {\n\
             \tBox(s String)\n\
             }\n\
             fn greeter(name String) fn(String) String {\n\
             \tb = Box(name)\n\
             \tfn(greeting) {\n\
             \t\tmatch b {\n\
             \t\t\tBox(n) -> '${greeting}, ${n}'\n\
             \t\t}\n\
             \t}\n\
             }\n\
             fn spin(n Int, f fn(String) String) String {\n\
             \tif n == 0 { f('bye') } else { spin(n - 1, greeter('${n}')) }\n\
             }\n\
             pub fn main() {\n\
             \tprintln(greeter('you')('hi'))\n\
             \tprintln(spin(1000, greeter('start')))\n\
             \tk = 'kept'\n\
             \tgo = fn(n) { if n == 0 { k } else { go(n - 1) } }\n\
             \tprintln(go(100))\n\
             }\n",
    );
    assert_eq!(out, "hi, you\nbye, 1\nkept\n");
    assert_eq!(left, 0);
}

/// A tuple holds references to its elements, and reading one out takes
/// its own.
#[test]
fn tuples_and_what_they_hold_are_all_freed() {
    let (out, left) = cells_left_after(
        "fn pairs(n Int, acc (String, Int)) (String, Int) {\n\
             \tif n == 0 { acc } else { pairs(n - 1, ('${n}', acc.1 + n)) }\n\
             }\n\
             pub fn main() {\n\
             \tp = pairs(1000, ('start', 0))\n\
             \tprintln(p.0)\n\
             \tprintln(p)\n\
             \tprintln(Some((p, 'x')))\n\
             }\n",
    );
    assert_eq!(
        out,
        "1\n(1, 500500)\nSome(\n  (\n    (1, 500500),\n    x\n  )\n)\n"
    );
    assert_eq!(left, 0);
}

/// An array shares its nodes with the arrays it was made from. Building
/// arrays of strings at both ends, walking them, joining them and keeping
/// old versions around leaves nothing once they are all dropped.
#[test]
fn arrays_and_what_they_share_are_all_freed() {
    let (out, left) = cells_left_after(
        "fn build(n Int, acc Array(String)) Array(String) {\n\
             \tif n == 0 { acc } else { build(n - 1, [..acc, '${n}']) }\n\
             }\n\
             fn front(n Int, acc Array(String)) Array(String) {\n\
             \tif n == 0 { acc } else { front(n - 1, ['${n}', ..acc]) }\n\
             }\n\
             fn count(xs Array(String), n Int) Int {\n\
             \tmatch xs {\n\
             \t\t[] -> n\n\
             \t\t[_, ..t] -> count(t, n + 1)\n\
             \t}\n\
             }\n\
             pub fn main() {\n\
             \ta = build(2000, [])\n\
             \tb = front(2000, a)\n\
             \tc = [..b, ..a]\n\
             \tprintln(count(c, 0))\n\
             \tprintln(c[0] or '?')\n\
             \tprintln(a[0] or '?')\n\
             \tprintln(count([..a, ..[]], 0))\n\
             }\n",
    );
    assert_eq!(out, "6000\n1\n2000\n2000\n");
    assert_eq!(left, 0);
}

/// A range holds no references, and one built into an array, big ints and
/// all, is freed like any array.
#[test]
fn ranges_and_the_arrays_made_from_them_are_all_freed() {
    let (out, left) = cells_left_after(
        "fn count(xs Array(Int), n Int) Int {\n\
             \tmatch xs {\n\
             \t\t[] -> n\n\
             \t\t[_, ..t] -> count(t, n + 1)\n\
             \t}\n\
             }\n\
             pub fn main() {\n\
             \tr = 0..1000\n\
             \tprintln(count([..r, 5], 0))\n\
             \tprintln(count([..{ 9223372036854775000..9223372036854775007 }, ..r], 0))\n\
             \tprintln(r[0..3])\n\
             \tprintln(count(r, 0))\n\
             }\n",
    );
    assert_eq!(out, "1001\n1007\nOk(\n  [0, 1, 2]\n)\n1000\n");
    assert_eq!(left, 0);
}

/// A slice of a binary holds its owner, so the owner lives as long as any
/// slice of it, and no longer. A binary walked a byte at a time by a
/// pattern, slices of slices and all, leaves nothing behind.
#[test]
fn binaries_and_their_slices_are_all_freed() {
    let (out, left) = cells_left_after(
        "import scarlet/binary\n\
             fn count(b Binary, n Int) Int {\n\
             \tmatch b {\n\
             \t\t<<_, rest:binary>> -> count(rest, n + 1)\n\
             \t\t_ -> n\n\
             \t}\n\
             }\n\
             fn grow(n Int, acc Binary) Binary {\n\
             \tif n == 0 { acc } else { grow(n - 1, <<acc:binary, n:size(8)>>) }\n\
             }\n\
             pub fn main() {\n\
             \tb = grow(1000, <<>>)\n\
             \tprintln(count(b, 0))\n\
             \tkept = binary::slice_bits(b, 8, 16)\n\
             \tprintln(kept)\n\
             }\n",
    );
    assert_eq!(out, "1000\nOk(<<231, 230>>)\n");
    assert_eq!(left, 0);
}

/// A string held in a global lives until the run ends, and no longer.
#[test]
fn a_string_in_a_global_is_freed_with_the_globals() {
    let (out, left) = cells_left_after(
        "const greeting = 'hi'\n\
             pub fn main() {\n\
             \tprintln(greeting)\n\
             \tprintln(greeting)\n\
             }\n",
    );
    assert_eq!(out, "hi\nhi\n");
    assert_eq!(left, 0);
}

/// `os.env`'s map is made once and shared, and what the host built-ins
/// hand out, errors included, holds its own references. All of it is
/// freed when the run ends.
#[test]
fn what_the_host_built_ins_make_is_all_freed() {
    let env = vec![("A".into(), "1".into()), ("B".into(), "2".into())];
    let argv = vec!["main.sca".into(), "x".into()];
    let (out, left) = cells_left_after_in(
        argv,
        env,
        "import scarlet/os\n\
             import scarlet/io\n\
             import scarlet/crypto\n\
             import scarlet/map\n\
             pub fn main() {\n\
             \tprintln(os::get_env('A'))\n\
             \tprintln(map::size(os::env()))\n\
             \tprintln(os::argv())\n\
             \tprintln(match io::read_file('/no/such/file') {\n\
             \t\tOk(_) -> 'read'\n\
             \t\tErr(_) -> 'failed'\n\
             \t})\n\
             \tprintln(match crypto::random_bytes(40) {\n\
             \t\tOk(_) -> 'random'\n\
             \t\tErr(Nil) -> 'none'\n\
             \t})\n\
             }\n",
    );
    assert_eq!(out, "Some(1)\n2\n[main.sca, x]\nfailed\nrandom\n");
    assert_eq!(left, 0);
}

/// Every `Doc` a read hands out shares its document's tape and strings,
/// and a parse error holds its own message. When the run ends, all of it
/// is freed.
#[test]
fn what_json_makes_is_all_freed() {
    let (out, left) = cells_left_after(
        "import scarlet/json\n\
             pub fn main() {\n\
             \tmatch json::parse('{\"a\": [1, {\"b\": \"deep\"}], \"c\": \"x\"}') {\n\
             \t\tOk(d) -> {\n\
             \t\t\tprintln(json::size(json::field(d, 'a') or d))\n\
             \t\t\t_entries = json::entries(d)\n\
             \t\t\t_elements = json::elements(json::field(d, 'a') or d)\n\
             \t\t\tprintln(json::reencode(d))\n\
             \t\t}\n\
             \t\tErr(_) -> println('no')\n\
             \t}\n\
             \tprintln(match json::parse('[') {\n\
             \t\tOk(_) -> 'parsed'\n\
             \t\tErr(_) -> 'refused'\n\
             \t})\n\
             \tprintln(json::encode(json::Object([('k', json::List([json::Integer(1), json::Str('v')]))])))\n\
             }\n",
    );
    assert_eq!(
        out,
        "2\n{\"a\":[1,{\"b\":\"deep\"}],\"c\":\"x\"}\nrefused\n{\"k\":[1,\"v\"]}\n"
    );
    assert_eq!(left, 0);
}

/// A parsed head's method, target and fields are slices of the buffer
/// read, sharing it, and a decoded chunked body is one copy. When the run
/// ends, all of it is freed.
#[test]
fn what_http_parsing_makes_is_all_freed() {
    let (out, left) = cells_left_after(
        "import scarlet/http/h1\n\
             import scarlet/http/headers\n\
             import scarlet/binary\n\
             pub fn main() {\n\
             \treq = <<'POST /x HTTP/1.1\\r\\nHost: a\\r\\nTransfer-Encoding: chunked\\r\\n\\r\\n3\\r\\nabc\\r\\n0\\r\\n\\r\\n'>>\n\
             \tmatch h1::parse_request(req, 0) {\n\
             \t\th1::Done(method, _, _, hs, _, consumed) -> {\n\
             \t\t\tprintln(binary::to_string(method))\n\
             \t\t\tprintln(headers::get(hs, <<'host'>>))\n\
             \t\t\tprintln(h1::framing(hs))\n\
             \t\t\tprintln(binary::byte_size(h1::serialize_head(200, hs)))\n\
             \t\t\tmatch h1::chunk_decode(req, consumed, 100) {\n\
             \t\t\t\th1::ChunkedDone(body, _, _) -> println(body)\n\
             \t\t\t\th1::ChunkedNeedMore | h1::ChunkedBad(_) -> println('no body')\n\
             \t\t\t}\n\
             \t\t}\n\
             \t\th1::NeedMore | h1::Bad(_) -> println('no head')\n\
             \t}\n\
             }\n",
    );
    assert_eq!(out, "Ok(POST)\nSome(<<97>>)\nChunked\n56\n<<97, 98, 99>>\n");
    assert_eq!(left, 0);
}

/// Every version of a map shares nodes with the one it came from, and
/// what a lookup or a listing hands out holds its own references. When
/// the run ends, all of it is freed.
#[test]
fn maps_and_what_they_hold_are_all_freed() {
    let (out, left) = cells_left_after(
        "import scarlet/map\n\
             fn fill(m map::Map(String, String), n Int) map::Map(String, String) {\n\
             \tif n == 0 { m } else { fill(map::set(m, 'k${n}', 'v${n}'), n - 1) }\n\
             }\n\
             fn drain(m map::Map(String, String), n Int) map::Map(String, String) {\n\
             \tif n == 0 { m } else { drain(map::delete(m, 'k${n}'), n - 2) }\n\
             }\n\
             pub fn main() {\n\
             \tfull = fill(map::new(), 400)\n\
             \thalf = drain(full, 400)\n\
             \tprintln('${map::size(full)} ${map::size(half)}')\n\
             \tprintln(map::get(half, 'k7'))\n\
             \tprintln(map::get(half, 'k8'))\n\
             \t_keys = map::keys(half)\n\
             \t_pairs = map::to_list(map::set(half, 'k7', 'again'))\n\
             \tprintln(full == half)\n\
             \tnested = map::set(map::new(), half, [full])\n\
             \tprintln(map::has(nested, drain(full, 400)))\n\
             }\n",
    );
    assert_eq!(out, "400 200\nSome(v7)\nNone\nFalse\nTrue\n");
    assert_eq!(left, 0);
}

/// A field held in different slots is read through a `match` on the
/// variant. The strings read, the values read from, and the ones a spread
/// builds are all freed.
#[test]
fn fields_read_by_name_are_all_freed() {
    let (out, left) = cells_left_after(
        "type S {\n\tA(name String, n Int, tag String)\n\tB(tag String, name String)\n}\n\
             fn both(s S) String { '${s.name}/${s.tag}' }\n\
             pub fn main() {\n\
             \tb = B(tag: 'x', name: 'bee')\n\
             \ta = A(..b, n: 1)\n\
             \tprintln(both(b))\n\
             \tprintln(both(a))\n\
             \tprintln(both(A(..a, name: 'aye', n: 2)))\n\
             }\n",
    );
    assert_eq!(out, "bee/x\nbee/x\naye/x\n");
    assert_eq!(left, 0);
}

/// Load `src`, run it, and hand the machine and the finished run to
/// `check`.
fn after_running(src: &str, check: impl FnOnce(&mut Machine<'_, '_>, Ended)) {
    let result = scarlet_core::compile::compile_text(src, None);
    assert!(result.success(), "{:?}", result.diagnostics());
    let program = result.into_runnable().expect("a clean compile is runnable");
    let code = crate::code::load(&program, Audit::Never).expect("the program loads");
    let mut host = OsHost::new(Vec::new(), Vec::new(), Vec::new());
    let mut m = Machine::new(&code, &mut host);
    let ended = m.run().expect("the program runs");
    check(&mut m, ended);
}

/// A program whose one global holds a cell when it ends.
const HOLDS_A_CELL: &str = "const kept = ['a', 'b']\npub fn main() {\n\tprintln(kept)\n}\n";

fn held_by_a_global(m: &Machine<'_, '_>) -> Cell {
    m.rest
        .globals
        .iter()
        .find_map(|v| v.as_cell())
        .expect("a global holding a cell")
}

#[test]
fn an_audit_of_a_run_with_every_count_right_finds_nothing() {
    after_running(HOLDS_A_CELL, |m, ended| {
        assert!(m.rest.heap.live() > 0, "the global still holds its array");
        assert_eq!(m.finish(ended, Audit::AtEnd), Ok(()));
        assert_eq!(m.rest.heap.live(), 0);
    });
}

/// A run that is not audited ends well whatever its heap holds, and
/// lets go of nothing.
#[test]
fn a_run_that_is_not_audited_leaves_its_heap_alone() {
    after_running(HOLDS_A_CELL, |m, ended| {
        let cell = held_by_a_global(m);
        m.rest.heap.share(cell).expect("a live cell");
        let live = m.rest.heap.live();
        assert_eq!(m.finish(ended, Audit::Never), Ok(()));
        assert_eq!(m.rest.heap.live(), live);
    });
}

/// A count one too high means a cell nothing will ever free. The audit says
/// which cell, what its count is and how many hold it.
#[test]
fn an_audit_names_the_cell_a_lost_release_left() {
    after_running(HOLDS_A_CELL, |m, ended| {
        let cell = held_by_a_global(m);
        m.rest.heap.share(cell).expect("a live cell");
        let fault = AuditFault::Miscounted {
            cell,
            count: 2,
            holders: 1,
        };
        assert_eq!(
            m.finish(ended, Audit::AtEnd),
            Err(Stop::broken(Broken::Audit(fault)))
        );
    });
}

/// A count one too low means a cell freed while something still holds it.
#[test]
fn an_audit_finds_a_reference_to_a_freed_cell() {
    after_running(HOLDS_A_CELL, |m, ended| {
        let cell = held_by_a_global(m);
        m.rest.heap.release(cell).expect("the global's reference");
        let fault = AuditFault::Dangling {
            holder: None,
            to: cell,
        };
        assert_eq!(
            m.finish(ended, Audit::AtEnd),
            Err(Stop::broken(Broken::Audit(fault)))
        );
    });
}

/// The case no release can find: the freed space is a new cell, so the old
/// holder and the new one hold the same cell and its count is 1.
#[test]
fn an_audit_finds_a_freed_cell_handed_out_again() {
    after_running(HOLDS_A_CELL, |m, ended| {
        let cell = held_by_a_global(m);
        m.rest.heap.release(cell).expect("the global's reference");
        let data = vec![Value::NIL.bits(); 6];
        let again = m.rest.heap.make(Kind::Binary, &data).expect("room");
        assert_eq!(again, cell);
        assert_eq!(m.rest.heap.audit(m.rest.globals.iter().copied()), Ok(()));
        m.rest.globals.push(Value::cell(again));
        let fault = AuditFault::Miscounted {
            cell,
            count: 1,
            holders: 2,
        };
        assert_eq!(
            m.finish(ended, Audit::AtEnd),
            Err(Stop::broken(Broken::Audit(fault)))
        );
    });
}

/// Cells that hold each other have the counts they should, so it takes the
/// end of the run, when nothing outside the heap is left, to see them. No
/// Scarlet program can make them.
#[test]
fn an_audit_finds_cells_that_hold_each_other() {
    after_running(HOLDS_A_CELL, |m, ended| {
        let a = m.rest.heap.tuple(&[Value::NIL]).expect("room");
        let b = m.rest.heap.tuple(&[Value::cell(a)]).expect("room");
        m.rest.heap.data_mut(a)[0] = Value::cell(b).bits();
        assert_eq!(
            m.finish(ended, Audit::AtEnd),
            Err(Stop::broken(Broken::CellsLeft { cells: 2 }))
        );
    });
}

/// A run's roots are its registers, its globals, `os.env`'s map and the
/// closure each frame runs as. With any one of them left out, the cell it
/// holds would look as if its count were too high.
#[test]
fn an_audit_is_told_of_every_reference_a_run_holds() {
    after_running("pub fn main() {\n\tNil\n}\n", |m, _| {
        let cells: Vec<Cell> = (0..6)
            .map(|_| m.rest.heap.tuple(&[Value::NIL]).expect("room"))
            .collect();
        let [reg, passed, global, env, running, waiting] = cells[..] else {
            panic!("six cells");
        };
        let miscounted = |cell| {
            Err(Stop::broken(Broken::Audit(AuditFault::Miscounted {
                cell,
                count: 1,
                holders: 0,
            })))
        };
        assert_eq!(m.audit(Value::NIL), miscounted(reg));
        m.open(1);
        m.stack[0] = Value::cell(reg);
        assert_eq!(m.audit(Value::NIL), miscounted(passed));
        m.passing.push(Value::cell(passed));
        assert_eq!(m.audit(Value::NIL), miscounted(global));
        m.rest.globals.push(Value::cell(global));
        assert_eq!(m.audit(Value::NIL), miscounted(env));
        m.rest.env = Some(env);
        for (cell, next) in [(running, miscounted(waiting)), (waiting, Ok(()))] {
            assert_eq!(m.audit(Value::NIL), miscounted(cell));
            assert_eq!(m.audit(Value::cell(cell)), next, "as the result");
            m.open_frame(Frame {
                pc: 0,
                below: Room(0),
                dst: Reg(0),
                env: Value::cell(cell),
            });
            assert_eq!(m.audit(Value::NIL), next, "as a frame's closure");
        }
    });
}

/// An `Audit` passes between any two instructions that leave every count
/// right, and stops the run at the first that finds one wrong.
#[test]
fn an_audit_instruction_stops_where_a_count_is_wrong() {
    let mut ins = an_array_in_r1().to_vec();
    ins.extend([
        Ins::Audit,
        mv(2, 1, false),
        Ins::Audit,
        drop_of(2),
        Ins::Audit,
        ret(1),
    ]);
    after(3, ins, the_only_cell_left);

    let shared_once_too_often = |heap: &mut Heap| {
        let cell = heap.tuple(&[Value::NIL]).expect("room");
        heap.share(cell).expect("a live cell");
        Some(Value::cell(cell))
    };
    after_given(
        1,
        vec![Ins::Audit, ret(0)],
        shared_once_too_often,
        |_, got| {
            let Err(Stop::BadProgram(why)) = got else {
                panic!("the audit finds the count");
            };
            assert!(
                why.to_string().ends_with("has a count of 2 and 1 holders"),
                "{why}"
            );
        },
    );
}

/// An instruction that takes another reference to a freed cell stops there,
/// before it writes the value anywhere.
#[test]
fn a_share_of_a_freed_cell_stops_the_instruction() {
    let freed = |heap: &mut Heap| {
        let cell = heap.tuple(&[Value::NIL]).expect("room");
        heap.release(cell).expect("a live cell");
        Some(Value::cell(cell))
    };
    let one = || Args::new(&[Reg(0)]).expect("room for one");
    let held = || Shares::new(&[Reg(0)]).expect("room for one");
    let shares = [
        mv(1, 0, false),
        Ins::Tuple {
            dst: Dst(Reg(1)),
            elements: held(),
        },
        array_of(1, &[0]),
        Ins::SetGlobal {
            slot: GlobalSlot(0),
            src: Shared(Reg(0)),
        },
        Ins::Ctor {
            dst: Dst(Reg(1)),
            variant: Variant::new(SOME_VARIANT),
            fields: Args::new(&[Reg(0)]).expect("room for one"),
        },
        Ins::Call {
            dst: Dst(Reg(1)),
            to: EntryPc(START),
            args: one(),
            needs: Room(2),
            own: Room(2),
        },
        Ins::TailCall {
            to: EntryPc(START),
            args: one(),
            needs: Room(2),
            own: Room(2),
        },
        Ins::TailCallSelf {
            to: EntryPc(START),
            args: one(),
            own: Room(2),
        },
    ];
    for share in shares {
        let what = format!("{share:?}");
        after_given(2, vec![share, ret(1)], freed, |m, got| {
            assert!(
                matches!(got, Err(ref stop) if *stop == Stop::broken(Broken::DeadCell)),
                "{what}"
            );
            assert_eq!(m.rest.heap.live(), 0, "{what}");
            let written = m.stack.iter().skip(1).chain(&m.rest.globals);
            let all_nil = written.copied().all(|v| v.bits() == Value::NIL.bits());
            assert!(all_nil, "{what} wrote the freed cell somewhere");
        });
    }
}

/// The second read of a big Int constant finds the cell the first one made,
/// and the end of the run frees it.
#[test]
fn a_big_int_constant_is_made_once() {
    let big = |dst| Ins::BigInt {
        dst: Dst(Reg(dst)),
        n: i64::MAX,
        id: scarlet_ir::core_ir::ConstId(3),
    };
    let body = vec![big(0), big(1), big(0), Ins::Audit, ret(1)];
    after(2, body, |m, got| {
        let v = got.expect("the function returns");
        assert_eq!(m.rest.heap.made(), 1);
        assert_eq!(m.rest.heap.live(), 1);
        assert_eq!(
            m.rest.int_of(v).map(crate::bigint::Int::big),
            Ok(i64::MAX.into())
        );
        assert_eq!(m.finish(Ended { last: v }, Audit::AtEnd), Ok(()));
        assert_eq!(m.rest.heap.live(), 0);
    });
}

/// A register that still names a cell after the cell was freed is found
/// when the register is written over, and when its frame ends.
#[test]
fn a_register_holding_a_freed_cell_stops_the_instruction_that_gives_it_up() {
    after_running(HOLDS_A_CELL, |m, _| {
        let cell = held_by_a_global(m);
        m.rest.heap.release(cell).expect("the global's reference");
        let stale = Value::cell(cell);
        m.open(1);
        m.stack[0] = stale;
        assert_eq!(m.set(0, Reg(0), Value::NIL), Err(DeadCell));
        m.stack[0] = stale;
        assert_eq!(m.leave(0, Room(1)), Err(DeadCell));
        assert_eq!(Stop::from(DeadCell), Stop::broken(Broken::DeadCell));
    });
}

/// Where the functions these tests run start: past the `Halt`.
const START: u32 = 1;

/// Instruction `i` of the function a test runs.
fn pc(i: u32) -> Pc {
    Pc(START + i)
}

/// A constructor of no type in particular.
const SOME_VARIANT: VariantRef = VariantRef {
    type_id: scarlet_ir::TypeId(263),
    variant_idx: 1,
};

/// A program of one function of `params` parameters and `needs` registers. It
/// lives as long as the tests do, so that any machine may run it.
fn function(params: usize, needs: u8, body: Vec<Ins<Reg>>) -> &'static Code {
    let result = scarlet_core::compile::compile_text("pub fn main() {\n\tNil\n}\n", None);
    assert!(result.success(), "{:?}", result.diagnostics());
    let program = result.into_runnable().expect("a clean compile is runnable");
    let halt = Ins::Halt { src: Taken(Reg(0)) };
    let ins: Vec<_> = std::iter::once(halt).chain(body).collect();
    let entry = || Entry {
        name: "test.function".to_string(),
        pc: EntryPc(START),
        end: Pc(u32::try_from(ins.len()).expect("a short function")),
        params,
        needs: Room(needs),
    };
    let funcs = std::iter::once(entry()).collect();
    let toplevels = vec![entry()];
    let wide = vec![Arg {
        reg: Reg(0),
        moved: true,
    }];
    Box::leak(Box::new(Code {
        ins,
        lists: Lists {
            regs: Vec::new(),
            args: vec![wide.into_boxed_slice()],
        },
        texts: vec![Box::from(*b"new")],
        binaries: Vec::new(),
        whys: vec![Why::Broken(Broken::NoArm)],
        funcs,
        toplevels,
        main: None,
        globals: 1,
        types: program.types.clone(),
        names: TiVec::new(),
        abi: program.abi,
    }))
}

/// Run `body` as a function of `needs` registers that takes nothing,
/// and hand the machine and what the call gave to `check`.
fn after(
    needs: u8,
    body: Vec<Ins<Reg>>,
    check: impl FnOnce(&mut Machine<'_, '_>, Result<Value, Stop>),
) {
    after_given(needs, body, |_| None, check);
}

/// [`after`], for a function that takes in `r0` what `given` makes in the
/// machine's heap, when it makes something. The machine has one global, which
/// holds `Nil`.
fn after_given(
    needs: u8,
    body: Vec<Ins<Reg>>,
    given: impl FnOnce(&mut Heap) -> Option<Value>,
    check: impl FnOnce(&mut Machine<'_, '_>, Result<Value, Stop>),
) {
    let mut host = OsHost::new(Vec::new(), Vec::new(), Vec::new());
    let code = function(1, needs, body);
    let mut m = Machine::new(code, &mut host);
    let args: Vec<Value> = given(&mut m.rest.heap).into_iter().collect();
    let got = m.call(&code.toplevels[0], &args);
    check(&mut m, got);
}

fn constant(dst: u8, value: Value) -> Ins<Reg> {
    Ins::Const {
        dst: Dst(Reg(dst)),
        value,
    }
}

fn int(dst: u8, n: i64) -> Ins<Reg> {
    constant(dst, Value::int(n).expect("small"))
}

fn ret(src: u8) -> Ins<Reg> {
    Ins::Ret {
        src: Taken(Reg(src)),
        own: Room(5),
    }
}

fn drop_of(r: u8) -> Ins<Reg> {
    Ins::Drop { reg: Dead(Reg(r)) }
}

fn mv(dst: u8, src: u8, moved: bool) -> Ins<Reg> {
    Ins::Move {
        dst: Dst(Reg(dst)),
        src: Arg {
            reg: Reg(src),
            moved,
        },
    }
}

fn array_of(dst: u8, elements: &[u8]) -> Ins<Reg> {
    let elements: Vec<Reg> = elements.iter().copied().map(Reg).collect();
    Ins::Array {
        dst: Dst(Reg(dst)),
        elements: Shares::new(&elements).expect("a few elements"),
    }
}

fn len_of(dst: u8, src: u8) -> Ins<Reg> {
    Ins::ArrayLen {
        dst: Dst(Reg(dst)),
        src: Src(Reg(src)),
    }
}

/// `r1` gets a new array of one element.
fn an_array_in_r1() -> [Ins<Reg>; 2] {
    [constant(0, Value::NIL), array_of(1, &[0])]
}

/// The cell a finished call returned, after checking that the frame left
/// nothing behind and that giving the result up empties the heap.
fn the_only_cell_left(m: &mut Machine<'_, '_>, got: Result<Value, Stop>) {
    let v = got.expect("the function returns");
    let cell = v.as_cell().expect("an array");
    assert_eq!(m.rest.heap.unique(cell), Ok(true));
    assert!(m.stack.iter().all(|v| v.bits() == Value::NIL.bits()));
    assert!(m.frames.is_empty());
    assert_eq!(m.rest.release(v), Ok(()));
    assert_eq!(m.rest.heap.live(), 0);
}

/// A `Move` that takes its source hands over the register's reference,
/// so a push after it finds one holder and writes in place. One that
/// shares leaves two holders, and the push copies.
#[test]
fn a_move_that_takes_leaves_one_holder_and_one_that_shares_leaves_two() {
    let made = |moved: bool| {
        let mut ins = an_array_in_r1().to_vec();
        ins.extend([
            mv(2, 1, moved),
            Ins::ArrayAppend {
                dst: Dst(Reg(3)),
                rest: Arg {
                    reg: Reg(2),
                    moved: true,
                },
                back: Shares::new(&[Reg(0)]).expect("room for one"),
            },
            ret(3),
        ]);
        let mut made = 0;
        after(5, ins, |m, got| {
            made = m.rest.heap.made();
            the_only_cell_left(m, got);
        });
        made
    };
    assert!(made(true) < made(false), "{} {}", made(true), made(false));
}

/// The register a `Move` took from is empty: a `Drop` of it gives up
/// nothing, and a read of it finds `Nil` and not the value.
#[test]
fn a_register_a_move_took_from_is_empty() {
    let mut dropped = an_array_in_r1().to_vec();
    dropped.extend([mv(2, 1, true), drop_of(1), ret(2)]);
    after(5, dropped, the_only_cell_left);

    let mut read = an_array_in_r1().to_vec();
    read.extend([mv(2, 1, true), len_of(0, 1), ret(0)]);
    after(5, read, |_, got| {
        let stopped = got.map(|v| v.bits());
        assert_eq!(stopped, Err(Stop::broken(Broken::NotAnArray(View::Nil))));
    });
}

/// The same value shared, then dropped under its old name, ends with the
/// same one holder, by a share and a release.
#[test]
fn a_move_that_shares_then_a_drop_leaves_one_holder() {
    let mut ins = an_array_in_r1().to_vec();
    ins.extend([mv(2, 1, false), drop_of(1), ret(2)]);
    after(5, ins, the_only_cell_left);
}

/// A value that is not a cell has no reference to hand over, so a `Move`
/// of one copies it whichever way it is marked.
#[test]
fn a_move_of_a_value_that_is_not_a_cell_copies_it() {
    for moved in [true, false] {
        let ins = vec![constant(0, Value::bool(true)), mv(1, 0, moved), ret(1)];
        after(5, ins, |m, got| {
            let bits = got.map(|v| v.bits());
            assert_eq!(bits, Ok(Value::bool(true).bits()));
            assert_eq!(m.rest.heap.made(), 0);
        });
    }
}

/// The loader leaves out a `Move` from a register to itself. One that is run
/// all the same leaves the value where it is, whichever way it is marked.
#[test]
fn a_move_onto_its_own_register_keeps_the_value() {
    for moved in [true, false] {
        let mut ins = an_array_in_r1().to_vec();
        ins.extend([mv(1, 1, moved), ret(1)]);
        after(5, ins, the_only_cell_left);
    }
}

/// An instruction that names a string or a binary the program does not have
/// stops the run. It does not take the one for empty, or a test of it for one
/// that does not match.
#[test]
fn a_constant_that_is_not_there_stops_the_run() {
    let (src, to) = (Src(Reg(0)), Pc(START));
    let (text, bits) = (TextId(1), BinaryId(0));
    let named = [
        Ins::Str {
            dst: Dst(Reg(0)),
            text,
        },
        Ins::BinaryConst {
            dst: Dst(Reg(0)),
            bits,
        },
        Ins::JumpUnlessStr { src, text, to },
        Ins::JumpUnlessBinary { src, bits, to },
    ];
    for ins in named {
        after(5, vec![int(0, 1), ins, ret(0)], |_, got| {
            assert_eq!(got.err(), Some(Stop::broken(Broken::NoPlace)), "{ins:?}");
        });
    }
}

/// A constructor over a cell kept for it takes that cell when it has the right
/// number of fields, whether it holds them itself or in a list. Over anything
/// else it makes a new one and gives up what was there.
#[test]
fn a_constructor_takes_a_kept_cell_that_fits() {
    let variant = Variant::new(SOME_VARIANT);
    let few = Ins::CtorOver {
        dst: Dst(Reg(1)),
        variant,
        fields: Args::new(&[Reg(2)]).expect("room for one"),
        reuse: Over(Reg(0)),
    };
    let wide = Ins::CtorOverWide {
        dst: Dst(Reg(1)),
        variant,
        fields: crate::code::ArgList(0),
        reuse: Over(Reg(3)),
    };
    let holding = |fields: usize| {
        move |heap: &mut Heap| {
            let fields = vec![Value::bool(true); fields];
            Some(Value::cell(heap.ctor(SOME_VARIANT, &fields).expect("room")))
        }
    };
    for (fields, reused) in [(1, 1), (2, 0)] {
        let keep = Ins::DropReuse { reg: Kept(Reg(0)) };
        let body = vec![int(2, 7), keep, few, Ins::Audit, ret(1)];
        after_given(5, body, holding(fields), |m, got| {
            assert_eq!(m.rest.heap.reused(), reused, "{fields} fields");
            the_only_cell_left(m, got);
        });
        // The list names `r0`, so the kept cell is moved out of the way.
        let keep = Ins::DropReuse { reg: Kept(Reg(3)) };
        let body = vec![mv(3, 0, true), int(0, 7), keep, wide, Ins::Audit, ret(1)];
        after_given(5, body, holding(fields), |m, got| {
            assert_eq!(m.rest.heap.reused(), reused, "{fields} fields");
            the_only_cell_left(m, got);
        });
    }
}

/// The wide forms of the calls do what the others do, with their arguments in
/// a list. Here a function calls itself, or goes round, once, passing the
/// array it was given, and returns what it was passed the second time.
#[test]
fn a_call_with_its_arguments_in_a_list_passes_them() {
    let list = crate::code::ArgList(0);
    let (to, own) = (EntryPc(START), Room(5));
    // A first run has `Nil` in `r1`, and a second has been told apart by the
    // global it finds set.
    let first = [
        Ins::GetGlobal {
            dst: Dst(Reg(1)),
            slot: GlobalSlot(0),
        },
        Ins::JumpUnlessInt {
            src: Src(Reg(1)),
            n: 1,
            to: pc(3),
        },
        ret(0),
        int(1, 1),
        Ins::SetGlobal {
            slot: GlobalSlot(0),
            src: Shared(Reg(1)),
        },
    ];
    let calls = [
        Ins::CallWide {
            dst: Dst(Reg(2)),
            to,
            args: list,
            needs: own,
            own,
        },
        Ins::CallSelfWide {
            dst: Dst(Reg(2)),
            to,
            args: list,
            own,
        },
        Ins::TailCallWide {
            to,
            args: list,
            needs: own,
            own,
        },
        Ins::TailCallSelfWide {
            to,
            args: list,
            own,
        },
    ];
    for call in calls {
        let body: Vec<_> = first
            .iter()
            .copied()
            .chain([call, Ins::Audit, ret(2)])
            .collect();
        let array = |heap: &mut Heap| {
            let array = array::from_values(heap, &[Value::NIL]).expect("room");
            Some(Value::cell(array))
        };
        after_given(5, body, array, the_only_cell_left);
    }
}

/// A call of a value that is not a function, is one the program does not have,
/// or takes another number of arguments stops and says so.
#[test]
fn a_call_of_a_value_that_cannot_be_called_so_stops() {
    let (dst, callee, own) = (Dst(Reg(2)), Shared(Reg(1)), Room(5));
    let args = |n: u8| Args::new(&vec![Reg(0); usize::from(n)]).expect("a few");
    let call = |f: Value, n: u8| {
        let calls = [
            Ins::CallValue {
                dst,
                callee,
                args: args(n),
                own,
            },
            Ins::TailCallValue {
                callee,
                args: args(n),
                own,
            },
        ];
        let stops = calls.map(|call| {
            let mut stopped = None;
            after(5, vec![constant(1, f), call, ret(2)], |_, got| {
                stopped = got.map(|v| v.bits()).err();
            });
            stopped.expect("the call stops")
        });
        let [by_call, by_tail_call] = stops;
        assert_eq!(by_call, by_tail_call);
        by_call
    };
    let arity = |got| {
        Stop::broken(Broken::CallArity {
            func: "test.function".to_string(),
            got,
            want: 1,
        })
    };
    let this = Value::func(FuncIdx(0));
    assert_eq!(call(this, 0), arity(0));
    assert_eq!(call(this, 3), arity(3));
    let missing = FuncIdx(9);
    let stop = Stop::missing(Missing::NoFunc(missing));
    assert_eq!(call(Value::func(missing), 1), stop);
    let stop = Stop::broken(Broken::NotAFunction(View::Bool(true)));
    assert_eq!(call(Value::bool(true), 1), stop);
}

/// A tail call to another function gives up the closure the frame ran as, and
/// one to the same function goes on as it.
#[test]
fn a_tail_call_by_name_gives_up_the_closure() {
    let (to, own) = (EntryPc(START), Room(5));
    let args = Args::new(&[]).expect("none");
    let list = crate::code::ArgList(1);
    let calls = [
        (
            Ins::TailCall {
                to,
                args,
                needs: own,
                own,
            },
            false,
        ),
        (
            Ins::TailCallWide {
                to,
                args: list,
                needs: own,
                own,
            },
            false,
        ),
        (Ins::TailCallSelf { to, args, own }, true),
        (
            Ins::TailCallSelfWide {
                to,
                args: list,
                own,
            },
            true,
        ),
    ];
    for (call, keeps) in calls {
        let body = vec![
            Ins::GetGlobal {
                dst: Dst(Reg(1)),
                slot: GlobalSlot(0),
            },
            Ins::JumpUnlessInt {
                src: Src(Reg(1)),
                n: 1,
                to: pc(4),
            },
            Ins::GetSelf {
                dst: Dst(Reg(2)),
                this: FuncIdx(0),
            },
            ret(2),
            int(1, 1),
            Ins::SetGlobal {
                slot: GlobalSlot(0),
                src: Shared(Reg(1)),
            },
            call,
        ];
        let code = function(0, 5, body);
        let mut host = OsHost::new(Vec::new(), Vec::new(), Vec::new());
        let mut m = Machine::new(code, &mut host);
        let captured = [Value::NIL];
        let closure = m.rest.heap.closure(FuncIdx(0), &captured).expect("room");
        m.open(5);
        m.open_frame(Frame {
            pc: 0,
            below: Room(0),
            dst: Reg(0),
            env: Value::cell(closure),
        });
        let v = m.steps(START as usize).expect("it returns");
        assert_eq!(v.as_cell(), keeps.then_some(closure), "{call:?}");
        assert_eq!(m.rest.heap.live(), usize::from(keeps), "{call:?}");
        assert_eq!(m.rest.release(v), Ok(()));
        assert_eq!(m.rest.heap.live(), 0);
    }
}

/// A tail call leaves nothing in the frame's registers but what it passes: a
/// register the function wrote before it reads as `Nil` after.
#[test]
fn a_tail_call_empties_the_registers_it_does_not_pass() {
    let (to, own) = (EntryPc(START), Room(5));
    let none = || Args::new(&[]).expect("none");
    let one = || Args::new(&[Reg(0)]).expect("room for one");
    let calls = [
        Ins::TailCallSelf {
            to,
            args: one(),
            own,
        },
        Ins::TailCallSelf {
            to,
            args: none(),
            own,
        },
        Ins::TailCall {
            to,
            args: one(),
            needs: own,
            own,
        },
        Ins::TailCallSelfWide {
            to,
            args: crate::code::ArgList(0),
            own,
        },
    ];
    for call in calls {
        let body = vec![
            Ins::GetGlobal {
                dst: Dst(Reg(1)),
                slot: GlobalSlot(0),
            },
            Ins::JumpUnlessInt {
                src: Src(Reg(1)),
                n: 1,
                to: pc(3),
            },
            ret(4),
            int(1, 1),
            Ins::SetGlobal {
                slot: GlobalSlot(0),
                src: Shared(Reg(1)),
            },
            int(4, 9),
            call,
        ];
        after(5, body, |m, got| {
            assert_eq!(got.map(|v| v.bits()), Ok(Value::NIL.bits()), "{call:?}");
            assert!(m.stack.iter().all(|v| v.bits() == Value::NIL.bits()));
        });
    }
}

/// A function that writes the string `new` into the one-element array
/// in `r1` at `index`. It returns the array the write made, from `r2`.
/// When the write misses it returns `after_miss` instead, and `r2` then
/// still holds the 7 it started with.
fn a_write(index: i64, moved: bool, value_moved: bool, after_miss: u8) -> Vec<Ins<Reg>> {
    let mut ins = an_array_in_r1().to_vec();
    ins.extend([
        int(2, 7),
        int(3, index),
        Ins::Str {
            dst: Dst(Reg(4)),
            text: TextId(0),
        },
        write(3, 4, moved, value_moved, 7),
        ret(2),
        ret(after_miss),
    ]);
    ins
}

/// `r2` gets the array in `r1` with the value in `value` at the index in
/// `index`, or the run continues at `miss`.
fn write(index: u8, value: u8, moved: bool, value_moved: bool, miss: u32) -> Ins<Reg> {
    Ins::ArraySet {
        dst: Hit(Reg(2)),
        src: Arg { reg: Reg(1), moved },
        index: Src(Reg(index)),
        value: Arg {
            reg: Reg(value),
            moved: value_moved,
        },
        miss: pc(miss),
    }
}

/// Element 0 of the array `v`, as text when it is a string.
fn first_text(m: &Machine<'_, '_>, v: Value) -> Option<String> {
    let first = array::get(&m.rest.heap, v.as_cell()?, 0)?.as_cell()?;
    let mut out = Vec::new();
    m.rest.heap.read_string(first, &mut out);
    String::from_utf8(out).ok()
}

/// A write that takes the register of an array nothing else holds
/// changes it in place: the only cells made are the array's two and the
/// string, whether the value is taken or shared.
#[test]
fn a_write_that_takes_an_array_with_one_holder_makes_no_cell() {
    for value_moved in [true, false] {
        after(5, a_write(0, true, value_moved, 0), |m, got| {
            assert_eq!(m.rest.heap.made(), 3);
            let v = *got.as_ref().expect("the function returns");
            assert_eq!(first_text(m, v).as_deref(), Some("new"));
            the_only_cell_left(m, got);
        });
    }
}

/// A write that shares its array's register finds two holders and
/// copies the root and the leaf. The register's array is as it was.
#[test]
fn a_write_that_shares_its_array_copies_and_leaves_the_old_one_alone() {
    after(5, a_write(0, false, true, 0), |m, got| {
        assert_eq!(m.rest.heap.made(), 5);
        let v = *got.as_ref().expect("the function returns");
        assert_eq!(first_text(m, v).as_deref(), Some("new"));
        the_only_cell_left(m, got);
    });
    // The same write, with the old array kept to look at afterwards.
    let mut kept = a_write(0, false, true, 0);
    kept[6] = ret(1);
    after(5, kept, |m, got| {
        let v = *got.as_ref().expect("the function returns");
        let first = v.as_cell().and_then(|a| array::get(&m.rest.heap, a, 0));
        assert_eq!(first.map(|x| x.bits()), Some(Value::NIL.bits()));
        the_only_cell_left(m, got);
    });
}

/// A write at an index the array does not have continues at its miss
/// target. It has written nothing and taken nothing, however its
/// operands are marked: the array and the value are still in their
/// registers with one holder each, and the destination holds what it
/// held.
#[test]
fn a_write_that_misses_takes_nothing_and_writes_nothing() {
    for index in [1, -1, 1 << 40] {
        for moved in [true, false] {
            after(5, a_write(index, moved, moved, 1), |m, got| {
                assert_eq!(m.rest.heap.made(), 3, "index {index}");
                let v = *got.as_ref().expect("the function returns");
                let first = v.as_cell().and_then(|a| array::get(&m.rest.heap, a, 0));
                assert_eq!(first.map(|x| x.bits()), Some(Value::NIL.bits()));
                the_only_cell_left(m, got);
            });
            after(5, a_write(index, moved, moved, 4), |m, got| {
                let v = *got.as_ref().expect("the function returns");
                let is_new = |c| m.rest.heap.string_is(c, b"new");
                assert!(v.as_cell().is_some_and(is_new));
                the_only_cell_left(m, got);
            });
            after(5, a_write(index, moved, moved, 2), |m, got| {
                let bits = got.map(|v| v.bits());
                assert_eq!(bits, Ok(Value::int(7).expect("small").bits()));
                assert_eq!(m.rest.heap.live(), 0);
            });
        }
    }
}

/// The value may sit in the array's own register. The array is read
/// before the value is taken, so the write shares the array, finds two
/// holders, and puts the old array in a copy.
#[test]
fn a_write_of_the_array_into_itself_holds_the_old_array() {
    let mut ins = an_array_in_r1().to_vec();
    ins.extend([
        int(3, 0),
        write(3, 1, false, true, 5),
        ret(2),
        Ins::Bad { why: WhyId(0) },
    ]);
    after(5, ins, |m, got| {
        let v = *got.as_ref().expect("the function returns");
        let outer = v.as_cell().expect("an array");
        let inner = array::get(&m.rest.heap, outer, 0).and_then(|x| x.as_cell());
        let inner = inner.expect("the old array, as the element");
        assert_ne!(inner, outer);
        assert_eq!(m.rest.heap.unique(inner), Ok(true));
        let first = array::get(&m.rest.heap, inner, 0).map(|x| x.bits());
        assert_eq!(first, Some(Value::NIL.bits()));
        the_only_cell_left(m, got);
    });
}

/// A function that writes -1 into the range `2..6` in `r1` at `index`,
/// and returns what is in `from`: `r2` for the array the write made, or
/// `r1` for the range. A miss returns the range.
fn a_write_to_a_range(index: i64, moved: bool, from: u8) -> Vec<Ins<Reg>> {
    vec![
        int(0, 2),
        int(4, 6),
        Ins::Range {
            dst: Dst(Reg(1)),
            start: Src(Reg(0)),
            end: Src(Reg(4)),
        },
        int(3, index),
        int(4, -1),
        write(3, 4, moved, false, 7),
        ret(from),
        ret(1),
    ]
}

/// The Ints in the array `v`, or none when it is not a tree.
fn tree_ints(m: &Machine<'_, '_>, v: Value) -> Option<Vec<i64>> {
    let Ok(Seq::Tree(a)) = m.rest.seq(v) else {
        return None;
    };
    let int = |x: Value| match x.view() {
        View::Int(n) => Some(n),
        _ => None,
    };
    array::elements(&m.rest.heap, a)
        .into_iter()
        .map(int)
        .collect()
}

/// The two ends of the range `v`, or none when it is not a range.
fn range_ends(m: &Machine<'_, '_>, v: Value) -> Option<(i64, i64)> {
    match m.rest.seq(v) {
        Ok(Seq::Range { start, end }) => Some((start, end)),
        Ok(Seq::Tree(_)) | Err(_) => None,
    }
}

/// A range stores no elements, so a write that does not miss builds its
/// elements as a tree and writes into that: a root and a leaf beside the
/// range's own cell. The range is given up when the write takes its
/// register, and is still the range it was when the write shares it. A
/// miss builds nothing.
#[test]
fn a_write_to_a_range_builds_a_tree_and_leaves_the_range_alone() {
    for moved in [true, false] {
        after(5, a_write_to_a_range(1, moved, 2), |m, got| {
            assert_eq!(m.rest.heap.made(), 3, "moved {moved}");
            let v = *got.as_ref().expect("the function returns");
            assert_eq!(tree_ints(m, v), Some(vec![2, -1, 4, 5]));
            the_only_cell_left(m, got);
        });
        for index in [4, -1] {
            after(5, a_write_to_a_range(index, moved, 2), |m, got| {
                assert_eq!(m.rest.heap.made(), 1, "index {index}");
                let v = *got.as_ref().expect("the function returns");
                assert_eq!(range_ends(m, v), Some((2, 6)));
                the_only_cell_left(m, got);
            });
        }
    }
    after(5, a_write_to_a_range(1, false, 1), |m, got| {
        let v = *got.as_ref().expect("the function returns");
        assert_eq!(range_ends(m, v), Some((2, 6)));
        the_only_cell_left(m, got);
    });
}

/// A write to something that is not an array, or at something that is
/// not an Int, stops the run and says what it found.
#[test]
fn a_write_to_what_is_not_an_array_or_at_what_is_not_an_int_stops() {
    let mut not_an_array = a_write(0, true, true, 0);
    not_an_array[1] = constant(1, Value::bool(true));
    after(5, not_an_array, |_, got| {
        let stopped = got.map(|v| v.bits());
        let want = Broken::NotAnArray(View::Bool(true));
        assert_eq!(stopped, Err(Stop::broken(want)));
    });
    let mut not_an_int = a_write(0, true, true, 0);
    not_an_int[3] = constant(3, Value::bool(false));
    after(5, not_an_int, |_, got| {
        let stopped = got.map(|v| v.bits());
        let want = Broken::Position(View::Bool(false));
        assert_eq!(stopped, Err(Stop::broken(want)));
    });
}

/// A write through a node that was freed while the array still pointed
/// at it stops the run as a wrong count, at the write.
#[test]
fn a_write_through_a_freed_node_stops_the_run_as_a_dead_cell() {
    let ins = vec![
        mv(1, 0, true),
        int(3, 0),
        write(3, 3, true, false, 4),
        ret(2),
        ret(1),
    ];
    let with_a_freed_leaf = |heap: &mut Heap| {
        let array = array::from_values(heap, &[Value::NIL, Value::NIL]).expect("room");
        // The tail leaf is the last of the root's words.
        let tail = heap.data(array).last().copied().map(Value::from_bits);
        let leaf = tail.and_then(Value::as_cell).expect("a tail leaf");
        heap.release(leaf).expect("a live cell");
        Some(Value::cell(array))
    };
    after_given(5, ins, with_a_freed_leaf, |_, got| {
        let stopped = got.map(|v| v.bits());
        assert_eq!(stopped, Err(Stop::broken(Broken::DeadCell)));
    });
}

/// The array and map code reports a freed cell as a heap fault, and
/// the run stops with it as a wrong count, not as a full heap.
#[test]
fn an_owned_push_onto_a_freed_array_stops_the_run_as_a_dead_cell() {
    after_running(HOLDS_A_CELL, |m, _| {
        let cell = held_by_a_global(m);
        m.rest.heap.release(cell).expect("the global's reference");
        let pushed = array::push_owned(&mut m.rest.heap, cell, Value::NIL, End::Back);
        assert_eq!(pushed.map_err(fault), Err(Stop::broken(Broken::DeadCell)));
        assert_eq!(fault(Fault::Full), Stop::HeapFull);
    });
}

/// A run that goes past the code's end, or whose last frame returns to
/// nothing, stops and says so.
#[test]
fn a_run_that_leaves_the_code_stops() {
    let body = vec![Ins::Jump { to: pc(1) }];
    after(5, body, |_, got| {
        let stopped = got.map(|v| v.bits());
        let func = "test.function".to_string();
        assert_eq!(stopped, Err(Stop::broken(Broken::RanPastEnd { func })));
    });
    on_a_machine(|m| {
        let got = m.steps(START as usize).map(|v| v.bits());
        assert_eq!(got, Err(Stop::broken(Broken::NoCaller)));
    });
}

/// Run `check` on a machine that has run nothing, to call small functions on
/// by hand.
fn on_a_machine(check: impl FnOnce(&mut Machine<'_, '_>)) {
    after(5, vec![ret(0)], |m, got| {
        assert_eq!(got.map(|v| v.bits()), Ok(Value::NIL.bits()));
        check(m);
    });
}

/// A function that does `ins` and returns what it left in `dst`.
fn stores(ins: Ins<Reg>, dst: u8) -> &'static Code {
    function(2, 5, vec![ins, ret(dst)])
}

/// A function that returns whether `ins` fell through, for an instruction
/// that jumps to 3 or does not.
fn falls_through(ins: Ins<Reg>) -> &'static Code {
    let answer = |b: bool| constant(2, Value::bool(b));
    function(2, 5, vec![ins, answer(true), ret(2), answer(false), ret(2)])
}

/// Call `code`'s function and empty the registers again. A run that stops
/// leaves its frames where they were.
fn call(m: &mut Machine<'_, '_>, code: &'static Code, args: &[Value]) -> Result<Value, Stop> {
    m.rest.code = code;
    let got = m.call(&code.toplevels[0], args);
    m.frames.clear();
    assert_eq!(m.leave(0, Room(5)), Ok(()));
    got
}

/// What an instruction's answer should be, when the operands are numbers.
#[derive(Debug, PartialEq)]
enum Answer {
    Int(BigInt),
    Bits(u64),
}

/// Check that `got` is `want`, in the one form it has, and give it up.
fn is(m: &mut Machine<'_, '_>, got: Result<Value, Stop>, want: &Answer, what: &dyn Fn() -> String) {
    let v = got.unwrap_or_else(|stop| panic!("{}: {stop:?}", what()));
    match want {
        Answer::Int(n) => {
            let small = n.to_i64().and_then(Value::int).is_some();
            assert_eq!(v.small_int().is_some(), small, "{}: its form", what());
            let number = int::read(&m.rest.heap, v).expect("an Int").big();
            assert_eq!(number, *n, "{}", what());
        }
        Answer::Bits(bits) => assert_eq!(v.bits(), *bits, "{}: {v:?}", what()),
    }
    assert_eq!(m.rest.release(v), Ok(()));
    assert_eq!(m.rest.heap.live(), 0, "{}: cells left", what());
}

/// Every instruction in the list of a family that writes the register `dst`,
/// with its operator. A comparison comes with the jump it becomes too.
macro_rules! of_pairs {
    ([] $($name:ident $op:path),*) => {
        |dst: u8| vec![$((
            Ins::$name { dst: Dst(Reg(dst)), a: Src(Reg(0)), b: Src(Reg(1)) },
            $op,
        )),*]
    };
}
macro_rules! with_constant {
    ([] $($name:ident $op:path),*) => {
        |k, dst: u8| vec![$((
            Ins::$name { dst: Dst(Reg(dst)), a: Src(Reg(0)), k },
            $op,
        )),*]
    };
}
macro_rules! compare_pairs {
    ([] $($name:ident $jump:ident $op:path),*) => {
        |dst: u8| vec![$((
            Ins::$name { dst: Dst(Reg(dst)), a: Src(Reg(0)), b: Src(Reg(1)) },
            Ins::$jump { a: Src(Reg(0)), b: Src(Reg(1)), to: pc(3) },
            $op,
        )),*]
    };
}
macro_rules! compare_with_constant {
    ([] $($name:ident $jump:ident $op:path),*) => {
        |k, dst: u8| vec![$((
            Ins::$name { dst: Dst(Reg(dst)), a: Src(Reg(0)), k },
            Ins::$jump { a: Src(Reg(0)), k, to: pc(3) },
            $op,
        )),*]
    };
}

/// The registers an instruction may write: both its operands' and another,
/// or its one operand's and another.
const OVER_TWO: [u8; 3] = [0, 1, 2];
const OVER_ONE: [u8; 2] = [0, 2];

/// Ints at every edge there is: both ends of the small range and just past
/// them, one word and past it, two words and past those.
fn edge_ints() -> Vec<BigInt> {
    let each = [
        BigInt::from(0),
        BigInt::from(1),
        BigInt::from(2),
        BigInt::from(7),
        BigInt::from(1 << 13),
        BigInt::from(69_069),
        BigInt::from(1 << 25),
        BigInt::from(1_i64 << 49),
        BigInt::from((1_i64 << 50) - 1),
        BigInt::from(1_i64 << 50),
        BigInt::from((1_i64 << 50) + 1),
        BigInt::from(i64::MAX),
        BigInt::from(u64::MAX),
        BigInt::from(u64::MAX) + 1,
        BigInt::from(i128::MAX) + 1,
        BigInt::from(3) << 200,
    ];
    each.into_iter().flat_map(|n| [n.clone(), -n]).collect()
}

/// Two hundred Ints of every size from one bit to 120, from a generator that
/// gives the same ones each time.
fn some_ints() -> Vec<BigInt> {
    let mut x: u64 = 0x9E37_79B9_7F4A_7C15;
    let mut next = || {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        x
    };
    (0..200u32)
        .map(|i| {
            let wide = BigInt::from(next()) << 64 | BigInt::from(next());
            let n: BigInt = wide >> (128 - (i % 120 + 1));
            if next() % 2 == 0 { n } else { -n }
        })
        .collect()
}

fn small_ints(of: &[BigInt]) -> Vec<(SmallInt, BigInt)> {
    let small = |n: &BigInt| n.to_i64().and_then(SmallInt::new);
    of.iter()
        .filter_map(|n| Some((small(n)?, n.clone())))
        .collect()
}

/// What `a op b` is, worked out on `num-bigint`'s numbers here.
fn exact(op: Arith, a: &BigInt, b: &BigInt) -> Answer {
    Answer::Int(match op {
        Arith::Add => a + b,
        Arith::Sub => a - b,
        Arith::Mul => a * b,
        Arith::Div => a.checked_div(b).unwrap_or_default(),
        Arith::Rem if b.sign() == num_bigint::Sign::NoSign => a.clone(),
        Arith::Rem => a % b,
    })
}

fn truth(b: bool) -> Answer {
    Answer::Bits(Value::bool(b).bits())
}

/// Each Int instruction gives what `num-bigint` gives, in the number's one
/// form, with a small or a big Int on either side, whichever register it
/// writes, and leaves no cell behind.
fn int_instructions_agree(xs: &[BigInt], pairs: &[(BigInt, BigInt)]) {
    on_a_machine(|m| {
        let value = |m: &mut Machine<'_, '_>, n: &BigInt| {
            bigint::value(&mut m.rest.heap, n.clone()).expect("room")
        };
        let mut two = Vec::new();
        for dst in OVER_TWO {
            for (ins, op) in int_arith!(of_pairs)(dst) {
                two.push((stores(ins, dst), format!("{op:?} into r{dst}"), Ok(op)));
            }
            for (stored, _, op) in int_compare!(compare_pairs)(dst) {
                two.push((stores(stored, dst), format!("{op:?} into r{dst}"), Err(op)));
            }
        }
        for (_, jump, op) in int_compare!(compare_pairs)(2) {
            two.push((falls_through(jump), format!("jump unless {op:?}"), Err(op)));
        }
        for (a, b) in pairs {
            for (body, name, op) in &two {
                let args = [value(m, a), value(m, b)];
                let want = match op {
                    Ok(op) => exact(*op, a, b),
                    Err(op) => truth(op.holds(a.cmp(b))),
                };
                let got = call(m, body, &args);
                is(m, got, &want, &|| format!("{a} {name} {b}"));
            }
        }
        for (k, n) in small_ints(xs) {
            let mut one = Vec::new();
            let mut compares = Vec::new();
            for dst in OVER_ONE {
                for (ins, op) in int_arith_k!(with_constant)(k, dst) {
                    one.push((stores(ins, dst), dst, op, false));
                }
                for (ins, op) in int_arith_k_first!(with_constant)(k, dst) {
                    one.push((stores(ins, dst), dst, op, true));
                }
                for (stored, jump, op) in int_compare_k!(compare_with_constant)(k, dst) {
                    compares.push((stores(stored, dst), falls_through(jump), dst, op));
                }
            }
            for x in xs {
                for (body, dst, op, k_first) in &one {
                    let (a, b) = if *k_first { (&n, x) } else { (x, &n) };
                    let args = [value(m, x), Value::NIL];
                    let got = call(m, body, &args);
                    is(m, got, &exact(*op, a, b), &|| {
                        format!("{a} {op:?} {b} into r{dst}")
                    });
                }
                for (stored, jump, dst, op) in &compares {
                    let want = truth(op.holds(x.cmp(&n)));
                    for body in [stored, jump] {
                        let args = [value(m, x), Value::NIL];
                        let got = call(m, body, &args);
                        is(m, got, &want, &|| format!("{x} {op:?} {n} into r{dst}"));
                    }
                }
            }
        }
        for dst in OVER_ONE {
            let neg = Ins::IntNeg {
                dst: Dst(Reg(dst)),
                a: Src(Reg(0)),
            };
            let body = stores(neg, dst);
            for x in xs {
                let args = [value(m, x), Value::NIL];
                let got = call(m, body, &args);
                is(m, got, &Answer::Int(-x), &|| format!("-{x} into r{dst}"));
            }
        }
    });
}

#[test]
fn int_instructions_are_right_at_every_edge() {
    let xs = edge_ints();
    let pairs: Vec<_> = xs
        .iter()
        .flat_map(|a| xs.iter().map(|b| (a.clone(), b.clone())))
        .collect();
    int_instructions_agree(&xs, &pairs);
}

#[test]
fn int_instructions_are_right_on_numbers_of_every_size() {
    let xs = some_ints();
    let pairs: Vec<_> = xs.iter().cloned().zip(xs.iter().rev().cloned()).collect();
    int_instructions_agree(&xs[..40], &pairs);
}

/// Floats at every edge, as [`edge_ints`] is for Ints.
fn edge_floats() -> Vec<f64> {
    let each = [
        0.0,
        f64::from_bits(1),
        f64::MIN_POSITIVE,
        1e-300,
        0.1,
        1.0,
        1.5,
        2.0,
        7.5,
        1e300,
        f64::MAX,
    ];
    each.into_iter().flat_map(|x| [x, -x]).collect()
}

/// What `a op b` is for two Floats, by the rules of `docs/semantics.md`.
fn ruled(op: Arith, a: f64, b: f64) -> Answer {
    let x = match op {
        Arith::Add => a + b,
        Arith::Sub => a - b,
        Arith::Mul => a * b,
        Arith::Div if b == 0.0 => 0.0,
        Arith::Div => a / b,
        Arith::Rem if b == 0.0 => a,
        Arith::Rem => a % b,
    };
    Answer::Bits(x.clamp(-f64::MAX, f64::MAX).to_bits())
}

/// Each Float instruction follows the rules, bit for bit, so a divide by zero
/// is 0.0, an answer too large is the largest Float, and a zero keeps its
/// sign.
#[test]
fn float_instructions_are_right_at_every_edge() {
    on_a_machine(|m| {
        let xs = edge_floats();
        let mut two = Vec::new();
        for dst in OVER_TWO {
            for (ins, op) in float_arith!(of_pairs)(dst) {
                two.push((stores(ins, dst), format!("{op:?} into r{dst}"), Ok(op)));
            }
            for (stored, _, op) in float_compare!(compare_pairs)(dst) {
                two.push((stores(stored, dst), format!("{op:?} into r{dst}"), Err(op)));
            }
        }
        for (_, jump, op) in float_compare!(compare_pairs)(2) {
            two.push((falls_through(jump), format!("jump unless {op:?}"), Err(op)));
        }
        let holds = |op: Order, a: f64, b: f64| {
            truth(match op {
                Order::Lt => a < b,
                Order::Le => a <= b,
                Order::Gt => a > b,
                Order::Ge => a >= b,
            })
        };
        for &a in &xs {
            for &b in &xs {
                let args = [Value::float(a), Value::float(b)];
                for (body, name, op) in &two {
                    let want = match op {
                        Ok(op) => ruled(*op, a, b),
                        Err(op) => holds(*op, a, b),
                    };
                    let got = call(m, body, &args);
                    is(m, got, &want, &|| format!("{a:e} {name} {b:e}"));
                }
            }
        }
        for &n in &xs {
            let k = Float::new(n);
            let mut one = Vec::new();
            let mut compares = Vec::new();
            for dst in OVER_ONE {
                for (ins, op) in float_arith_k!(with_constant)(k, dst) {
                    one.push((stores(ins, dst), dst, op, false));
                }
                for (ins, op) in float_arith_k_first!(with_constant)(k, dst) {
                    one.push((stores(ins, dst), dst, op, true));
                }
                for (stored, jump, op) in float_compare_k!(compare_with_constant)(k, dst) {
                    compares.push((stores(stored, dst), falls_through(jump), dst, op));
                }
            }
            for &x in &xs {
                let args = [Value::float(x), Value::NIL];
                for (body, dst, op, k_first) in &one {
                    let (a, b) = if *k_first { (n, x) } else { (x, n) };
                    let got = call(m, body, &args);
                    is(m, got, &ruled(*op, a, b), &|| {
                        format!("{a:e} {op:?} {b:e} into r{dst}")
                    });
                }
                for (stored, jump, dst, op) in &compares {
                    for body in [stored, jump] {
                        let got = call(m, body, &args);
                        is(m, got, &holds(*op, x, n), &|| {
                            format!("{x:e} {op:?} {n:e} into r{dst}")
                        });
                    }
                }
            }
        }
        for dst in OVER_ONE {
            let neg = Ins::FloatNeg {
                dst: Dst(Reg(dst)),
                a: Src(Reg(0)),
            };
            let body = stores(neg, dst);
            for &x in &xs {
                let got = call(m, body, &[Value::float(x), Value::NIL]);
                is(m, got, &Answer::Bits((-x).to_bits()), &|| {
                    format!("-{x:e} into r{dst}")
                });
            }
        }
    });
}

#[test]
fn an_int_of_any_size_becomes_a_float() {
    on_a_machine(|m| {
        let into = |dst: u8| {
            let from_int = Ins::FloatFromInt {
                dst: Dst(Reg(dst)),
                a: Src(Reg(0)),
            };
            stores(from_int, dst)
        };
        for body in OVER_ONE.map(into) {
            for n in edge_ints() {
                let v = bigint::value(&mut m.rest.heap, n.clone()).expect("room");
                let want = n.to_f64().expect("a Float");
                let got = call(m, body, &[v, Value::NIL]);
                is(m, got, &Answer::Bits(want.to_bits()), &|| format!("{n}"));
            }
        }
        let huge: BigInt = BigInt::from(1) << 2000;
        let v = bigint::value(&mut m.rest.heap, huge).expect("room");
        let got = call(m, into(0), &[v, Value::NIL]);
        is(m, got, &Answer::Bits(f64::MAX.to_bits()), &|| {
            "2^2000".into()
        });
    });
}

/// A value of every kind, as an operand of the wrong kind can be. The string,
/// the tuple and the big Int are cells with one reference.
fn one_of_each_kind(heap: &mut Heap) -> Vec<Value> {
    vec![
        Value::NIL,
        Value::bool(false),
        Value::bool(true),
        Value::func(FuncIdx(7)),
        Value::nullary(SOME_VARIANT),
        Value::float(1.5),
        Value::float(-0.0),
        Value::int(0).expect("small"),
        Value::int(-(1 << 50)).expect("small"),
        Value::cell(heap.string(b"text").expect("room")),
        Value::cell(heap.tuple(&[Value::NIL]).expect("room")),
        Value::cell(heap.big_int(&(BigInt::from(1) << 70)).expect("room")),
    ]
}

/// Call `body` with a value of each kind in each operand, and a number `with`
/// in the other, for an instruction that takes only what `takes` says. It
/// must stop with `stop`, which names the value, and leave every cell to its
/// one holder. `operands` is how many the instruction reads.
fn stops_on_the_wrong_kind(
    m: &mut Machine<'_, '_>,
    body: &'static Code,
    operands: usize,
    with: Value,
    takes: fn(&Heap, Value) -> bool,
    stop: fn(View) -> Stop,
) {
    let kinds = one_of_each_kind(&mut m.rest.heap).len();
    m.rest.heap = Heap::default();
    for kind in 0..kinds {
        for wrong in 0..operands {
            let all = one_of_each_kind(&mut m.rest.heap);
            let v = all[kind];
            for other in all.iter().filter(|o| o.bits() != v.bits()) {
                assert_eq!(m.rest.release(*other), Ok(()));
            }
            if takes(&m.rest.heap, v) {
                assert_eq!(m.rest.release(v), Ok(()));
                continue;
            }
            let mut args = [with, with];
            args[wrong] = v;
            let view = v.view();
            let got = call(m, body, &args).map(|v| v.bits());
            assert_eq!(got, Err(stop(view)), "{:?} on {view:?}", body.ins[1]);
            assert_eq!(m.rest.heap.live(), 0, "{:?} on {view:?}", body.ins[1]);
        }
    }
}

/// An Int instruction stops on any operand that is no Int, and a Float one on
/// any that is no Float, whichever side it is on. A Float instruction reads
/// its operands' words unchecked, so this is what shows that its test of the
/// answer catches every kind of word.
#[test]
fn every_operator_instruction_stops_on_an_operand_of_the_wrong_kind() {
    on_a_machine(|m| {
        let is_int = |heap: &Heap, v: Value| match v.view() {
            View::Int(_) => true,
            View::Cell(c) => heap.kind(c) == Some(Kind::BigInt),
            View::Float(_) | View::Nil | View::Bool(_) | View::Func(_) | View::Nullary(_) => false,
        };
        let is_float = |_: &Heap, v: Value| matches!(v.view(), View::Float(_));
        let no_int = |v| Stop::missing(Missing::IntOperation(v));
        let no_float = |v| Stop::broken(Broken::FloatOperation(v));
        let (seven, half) = (Value::int(7).expect("small"), Value::float(0.5));
        let (small, held) = (SmallInt::new(7).expect("small"), Float::new(0.5));

        let mut ints: Vec<(&'static Code, usize)> = Vec::new();
        ints.extend(
            int_arith!(of_pairs)(2)
                .into_iter()
                .map(|(i, _)| (stores(i, 2), 2)),
        );
        ints.extend(
            int_arith_k!(with_constant)(small, 2)
                .into_iter()
                .map(|(i, _)| (stores(i, 2), 1)),
        );
        ints.extend(
            int_arith_k_first!(with_constant)(small, 2)
                .into_iter()
                .map(|(i, _)| (stores(i, 2), 1)),
        );
        for (stored, jump, _) in int_compare!(compare_pairs)(2) {
            ints.extend([(stores(stored, 2), 2), (falls_through(jump), 2)]);
        }
        for (stored, jump, _) in int_compare_k!(compare_with_constant)(small, 2) {
            ints.extend([(stores(stored, 2), 1), (falls_through(jump), 1)]);
        }
        let (dst, a) = (Dst(Reg(2)), Src(Reg(0)));
        ints.push((stores(Ins::IntNeg { dst, a }, 2), 1));
        ints.push((stores(Ins::FloatFromInt { dst, a }, 2), 1));
        assert_eq!(ints.len(), 5 + 5 + 1 + 8 + 12 + 2);
        for (body, operands) in ints {
            stops_on_the_wrong_kind(m, body, operands, seven, is_int, no_int);
        }

        let mut floats: Vec<(&'static Code, usize)> = Vec::new();
        floats.extend(
            float_arith!(of_pairs)(2)
                .into_iter()
                .map(|(i, _)| (stores(i, 2), 2)),
        );
        floats.extend(
            float_arith_k!(with_constant)(held, 2)
                .into_iter()
                .map(|(i, _)| (stores(i, 2), 1)),
        );
        floats.extend(
            float_arith_k_first!(with_constant)(held, 2)
                .into_iter()
                .map(|(i, _)| (stores(i, 2), 1)),
        );
        for (stored, jump, _) in float_compare!(compare_pairs)(2) {
            floats.extend([(stores(stored, 2), 2), (falls_through(jump), 2)]);
        }
        for (stored, jump, _) in float_compare_k!(compare_with_constant)(held, 2) {
            floats.extend([(stores(stored, 2), 1), (falls_through(jump), 1)]);
        }
        floats.push((stores(Ins::FloatNeg { dst, a }, 2), 1));
        assert_eq!(floats.len(), 5 + 4 + 2 + 4 + 8 + 1);
        for (body, operands) in floats {
            stops_on_the_wrong_kind(m, body, operands, half, is_float, no_float);
        }
    });
}

/// Call `instr` with `args` on a heap that is full once it holds them, for an
/// answer that needs a cell. It must stop with `HeapFull` and leave each
/// operand with one holder.
fn finds_the_heap_full(m: &mut Machine<'_, '_>, ins: Ins<Reg>, dst: u8, args: [&BigInt; 2]) {
    let what = format!("{ins:?} on {args:?}");
    let mut room = Heap::default();
    let hold = |heap: &mut Heap| args.map(|n| bigint::value(heap, n.clone()).expect("room"));
    hold(&mut room);
    m.rest.heap = Heap::limited(room.words_used());
    let args = hold(&mut m.rest.heap);
    let held = m.rest.heap.live();
    let code = stores(ins, dst);
    m.rest.code = code;
    let got = m.call(&code.toplevels[0], &args).map(|v| v.bits());
    m.frames.clear();
    assert_eq!(got, Err(Stop::HeapFull), "{what}");
    assert_eq!(m.rest.heap.live(), held, "{what}");
    assert_eq!(m.leave(0, Room(5)), Ok(()), "{what}");
    assert_eq!(m.rest.heap.live(), 0, "{what}");
}

/// An Int instruction whose answer needs a cell, on a heap with no room for
/// one, stops and leaves its operands as they were, whichever register it was
/// to write.
#[test]
fn an_int_answer_with_no_room_stops_and_keeps_its_operands() {
    on_a_machine(|m| {
        let top = BigInt::from((1_i64 << 50) - 1);
        let bottom = BigInt::from(-(1_i64 << 50));
        let (one, big) = (BigInt::from(1), BigInt::from(3) << 200);
        let k = SmallInt::new(1).expect("small");
        for over in OVER_TWO {
            let (dst, a, b) = (Dst(Reg(over)), Src(Reg(0)), Src(Reg(1)));
            finds_the_heap_full(m, Ins::IntAdd { dst, a, b }, over, [&top, &one]);
            finds_the_heap_full(m, Ins::IntAdd { dst, a, b }, over, [&big, &big]);
            finds_the_heap_full(m, Ins::IntMul { dst, a, b }, over, [&big, &top]);
        }
        for over in OVER_ONE {
            let (dst, a) = (Dst(Reg(over)), Src(Reg(0)));
            for n in [&top, &big] {
                finds_the_heap_full(m, Ins::IntAddK { dst, a, k }, over, [n, &one]);
            }
            finds_the_heap_full(m, Ins::IntRSubK { dst, k, a }, over, [&bottom, &one]);
            for n in [&bottom, &big] {
                finds_the_heap_full(m, Ins::IntNeg { dst, a }, over, [n, &one]);
            }
        }
    });
}
