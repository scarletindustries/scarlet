use super::*;
use crate::OsHost;
use crate::bigint;
use crate::code::{BinaryId, EntryPc, Hit, Lists, Pc, ReadsClosure, Shares, TextId, Why, WhyId};
use crate::frame::FIRST;
use crate::heap::{AuditFault, Cell, Ctors, Fault, Kind, Numbering};
use crate::value::{Float, SmallInt, Tag};
use num_bigint::BigInt;
use num_traits::ToPrimitive as _;
use scarlet_ir::core_ir::{FuncIdx, VariantRef};
use scarlet_ir::intrinsic::Intrinsic;
use scarlet_ir::tivec::TiVec;

/// The fields the loop reads at every turn come first, where one load reaches
/// them, and the heap comes after everything else, so that its size cannot
/// move them.
#[test]
fn the_heap_is_behind_the_fields_the_loop_reads() {
    let heap = std::mem::offset_of!(Rest<'_, '_>, heap);
    assert!(heap < 256, "{heap}");
    assert_eq!(heap + size_of::<Heap>(), size_of::<Rest<'_, '_>>());
}

/// Whether none of `stack` is a cell, as when every frame on it has ended.
fn no_cell_on(stack: &[Value]) -> bool {
    stack.iter().all(|v| v.as_cell().is_none())
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
    assert!(no_cell_on(&m.stack), "a frame's registers outlived it");
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
            cell: cell.place(),
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
            to: cell.place(),
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
            cell: cell.place(),
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

/// A run's roots are its registers, what is on its way between two, its
/// globals and `os.env`'s map. With any one of them left out, the cell it
/// holds would look as if its count were too high.
#[test]
fn an_audit_is_told_of_every_reference_a_run_holds() {
    after_running("pub fn main() {\n\tNil\n}\n", |m, _| {
        let cells: Vec<Cell> = (0..6)
            .map(|_| m.rest.heap.tuple(&[Value::NIL]).expect("room"))
            .collect();
        let [closure, reg, above, passed, global, env] = cells[..] else {
            panic!("six cells");
        };
        let miscounted = |cell: Cell| {
            Err(Stop::broken(Broken::Audit(AuditFault::Miscounted {
                cell: cell.place(),
                count: 1,
                holders: 0,
            })))
        };
        let audit = |m: &Machine<'_, '_>, last| check(&m.stack, &m.rest, last);
        let last = m.stack.len() - 1;
        let held = [(1, closure, reg), (2, reg, above), (last, above, passed)];
        for (at, cell, next) in held {
            assert_eq!(audit(m, Value::NIL), miscounted(cell));
            assert_eq!(
                audit(m, Value::cell(cell)),
                miscounted(next),
                "as the result"
            );
            m.stack[at] = Value::cell(cell);
            assert_eq!(audit(m, Value::NIL), miscounted(next), "in register {at}");
        }
        m.rest.passing.push(Value::cell(passed));
        assert_eq!(audit(m, Value::NIL), miscounted(global));
        m.rest.globals.push(Value::cell(global));
        assert_eq!(audit(m, Value::NIL), miscounted(env));
        m.rest.env = Some(env);
        assert_eq!(audit(m, Value::NIL), Ok(()));
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
    let one = || Args::new(&[r(0)]).expect("room for one");
    let held = || Shares::new(&[r(0)]).expect("room for one");
    let shares = [
        mv(1, 0, false),
        Ins::Tuple {
            dst: Dst(r(1)),
            elements: held(),
        },
        Ins::Tuple2 {
            dst: Dst(r(1)),
            elements: [r(1), r(0)].map(Shared),
        },
        Ins::Tuple3 {
            dst: Dst(r(1)),
            elements: [r(1), r(1), r(0)].map(Shared),
        },
        Ins::Closure {
            dst: Dst(r(1)),
            func: FuncIdx(0),
            captures: held(),
        },
        Ins::CtorWide {
            dst: Dst(r(1)),
            ctor: ctors().1[0],
            fields: crate::code::ArgList(2),
        },
        array_of(1, &[0]),
        Ins::SetGlobal {
            slot: GlobalSlot(0),
            src: Shared(r(0)),
        },
        Ins::Ctor {
            dst: Dst(r(1)),
            ctor: ctors().1[0],
            fields: Args::new(&[r(0)]).expect("room for one"),
        },
        Ins::Call {
            dst: Dst(r(1)),
            own: Room(4),
            to: EntryPc(START),
            args: one(),
        },
        Ins::Call1Share {
            dst: Dst(r(1)),
            own: Room(4),
            to: EntryPc(START),
            a: Arg::shared(r(0)),
        },
        Ins::CallWide {
            dst: Dst(r(1)),
            own: Room(4),
            to: EntryPc(START),
            args: crate::code::ArgList(2),
        },
        Ins::TailCall {
            to: EntryPc(START),
            args: one(),
            own: Room(4),
        },
        Ins::TailCallSelf {
            to: EntryPc(START),
            args: one(),
            own: Room(4),
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
            let given = usize::from(r(0).0);
            let (below, above) = m.stack.split_at(given);
            let written = no_cell_on(below) && no_cell_on(&above[1..]);
            assert!(written, "{what} wrote the freed cell somewhere");
            assert!(no_cell_on(&m.rest.globals), "{what} wrote the freed cell");
        });
    }
}

/// The second read of a big Int constant finds the cell the first one made,
/// and the end of the run frees it.
#[test]
fn a_big_int_constant_is_made_once() {
    let big = |dst| Ins::BigInt {
        dst: Dst(r(dst)),
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
        let mut win = window(&mut m.stack, 0).expect("a window");
        let heap = &mut m.rest.heap;
        win.run_as(stale);
        assert_eq!(win.set(heap, CLOSURE, Value::NIL), Err(DeadCell));
        win.receive(&[stale]);
        assert_eq!(win.set(heap, r(0), Value::NIL), Err(DeadCell));
        win.receive(&[stale]);
        assert_eq!(win.leave(heap, Room(3).last()), Err(DeadCell));
        win.receive(&[stale]);
        assert_eq!(win.rewrite(heap, Room(3), &[]), Err(DeadCell));
        win.receive(&[stale]);
        assert_eq!(win.rewrite(heap, Room(3), &[Value::NIL]), Err(DeadCell));
        assert_eq!(Stop::from(DeadCell), Stop::broken(Broken::DeadCell));
    });
}

/// The register a test calls `r<n>`, which is the one for value `n` of a
/// function that takes its arguments in `r0` and up.
fn r(n: u8) -> Reg {
    Reg(FIRST.0 + n)
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

/// A program's constructors, and the two there are: [`SOME_VARIANT`], which
/// has one field, and the variant after it, which has two.
fn ctors() -> (Ctors, [Ctor; 2]) {
    let mut numbering = Numbering::default();
    let pair = VariantRef {
        variant_idx: 2,
        ..SOME_VARIANT
    };
    let one = numbering.add(SOME_VARIANT, 1).expect("the first");
    let two = numbering.add(pair, 2).expect("the second");
    (numbering.finish(), [one, two])
}

/// A program of one function of `params` parameters that holds `values` values
/// and keeps the closure it runs as. It lives as long as the tests do, so that
/// any machine may run it.
fn function(params: usize, values: u16, body: Vec<Ins<Reg>>) -> &'static Code {
    function_that(ReadsClosure::Captures, params, values, body)
}

/// [`function`], of a function that reads `reads` of its closure.
fn function_that(
    reads: ReadsClosure,
    params: usize,
    values: u16,
    body: Vec<Ins<Reg>>,
) -> &'static Code {
    let wide = vec![Arg {
        reg: r(0),
        moved: true,
    }];
    let lists = Lists {
        regs: Vec::new(),
        args: vec![
            wide.into_boxed_slice(),
            Box::new([]),
            Box::new([Arg::shared(r(0))]),
        ],
        dsts: Vec::new(),
    };
    function_of(reads, params, values, body, lists, ctors().0)
}

/// [`function_that`], whose instructions name these lists, in a program with
/// these constructors.
fn function_of(
    reads: ReadsClosure,
    params: usize,
    values: u16,
    body: Vec<Ins<Reg>>,
    lists: Lists<Reg>,
    ctors: Ctors,
) -> &'static Code {
    // The same for every function, and slow to make.
    static EMPTY: std::sync::OnceLock<&'static Code> = std::sync::OnceLock::new();
    let empty = EMPTY.get_or_init(|| loaded("pub fn main() {\n\tNil\n}\n", Audit::Never));
    let halt = Ins::Halt { src: Taken(RESULT) };
    let ins: Vec<_> = std::iter::once(halt).chain(body).collect();
    let entry = || Entry {
        name: "test.function".to_string(),
        pc: EntryPc(START),
        end: Pc(u32::try_from(ins.len()).expect("a short function")),
        params,
        own: Room(values + 2),
        closure: reads,
    };
    let funcs = std::iter::once(entry()).collect();
    let toplevels = vec![entry()];
    Box::leak(Box::new(Code {
        ins,
        lists,
        texts: vec![Box::from(*b"new")],
        binaries: Vec::new(),
        whys: vec![Why::Broken(Broken::NoArm)],
        funcs,
        toplevels,
        main: None,
        globals: 1,
        types: empty.types.clone(),
        ctors,
        names: TiVec::new(),
        abi: empty.abi,
    }))
}

/// Run `body` as a function that holds `values` values and takes nothing,
/// and hand the machine and what the call gave to `check`.
fn after(
    values: u16,
    body: Vec<Ins<Reg>>,
    check: impl FnOnce(&mut Machine<'_, '_>, Result<Value, Stop>),
) {
    after_given(values, body, |_| None, check);
}

/// [`after`], for a function that takes in `r0` what `given` makes in the
/// machine's heap, when it makes something. The machine has one global, which
/// holds `Nil`.
fn after_given(
    values: u16,
    body: Vec<Ins<Reg>>,
    given: impl FnOnce(&mut Heap) -> Option<Value>,
    check: impl FnOnce(&mut Machine<'_, '_>, Result<Value, Stop>),
) {
    let mut host = OsHost::new(Vec::new(), Vec::new(), Vec::new());
    let code = function(1, values, body);
    let mut m = Machine::new(code, &mut host);
    let args: Vec<Value> = given(&mut m.rest.heap).into_iter().collect();
    let got = m.call(&code.toplevels[0], &args);
    check(&mut m, got);
}

fn constant(dst: u8, value: Value) -> Ins<Reg> {
    Ins::Const {
        dst: Dst(r(dst)),
        value,
    }
}

fn int(dst: u8, n: i64) -> Ins<Reg> {
    constant(dst, Value::int(n).expect("small"))
}

fn ret(src: u8) -> Ins<Reg> {
    Ins::Ret {
        src: Taken(r(src)),
        last: Room(7).last(),
    }
}

fn drop_of(n: u8) -> Ins<Reg> {
    Ins::Drop { reg: Dead(r(n)) }
}

fn mv(dst: u8, src: u8, moved: bool) -> Ins<Reg> {
    Ins::Move {
        dst: Dst(r(dst)),
        src: Arg { reg: r(src), moved },
    }
}

fn array_of(dst: u8, elements: &[u8]) -> Ins<Reg> {
    let elements: Vec<Reg> = elements.iter().copied().map(r).collect();
    Ins::Array {
        dst: Dst(r(dst)),
        elements: Shares::new(&elements).expect("a few elements"),
    }
}

fn len_of(dst: u8, src: u8) -> Ins<Reg> {
    Ins::ArrayLen {
        dst: Dst(r(dst)),
        src: Src(r(src)),
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
    assert!(no_cell_on(&m.stack));
    assert_eq!(m.rest.release(v), Ok(()));
    assert_eq!(m.rest.heap.live(), 0);
}

/// A cell of every kind there is, some long, each with three words or more but
/// the pair, and how many values it starts with.
fn a_cell_of_each_kind(heap: &mut Heap) -> Vec<(Cell, usize)> {
    let nil = Value::NIL;
    let kinds = [
        (Kind::String, 5, 0),
        (Kind::String, 300, 0),
        (Kind::BigInt, 5, 0),
        (Kind::Range, 5, 0),
        (Kind::Binary, 5, 0),
        (Kind::Binary, 300, 0),
        (Kind::Handle, 5, 0),
        (Kind::BinarySlice, 3, 1),
        (Kind::Tuple, 5, 5),
        (Kind::Tuple, 253, 253),
        (Kind::Tuple, 300, 300),
        (Kind::ArrayLeaf, 5, 5),
        (Kind::ArrayRoot, 5, 0),
        (Kind::ArrayBranch, 5, 0),
        (Kind::Map, 5, 0),
        (Kind::MapNode, 5, 0),
        (Kind::MapCollision, 5, 0),
        (Kind::MapCollision, 300, 0),
    ];
    let mut all: Vec<(Cell, usize)> = kinds
        .into_iter()
        .map(|(kind, words, values)| {
            let words = vec![nil.bits(); words];
            (heap.make(kind, &words).expect("room"), values)
        })
        .collect();
    let pair = ctors().1[1];
    all.extend([
        (heap.ctor(pair, &[nil; 2]).expect("room"), 2),
        (heap.closure(FuncIdx(0), &[nil; 4]).expect("room"), 5),
        (heap.closure(FuncIdx(0), &[nil; 299]).expect("room"), 300),
    ]);
    all
}

/// A field is read from any cell that starts with that many values. The
/// instruction does not ask which kind it is, because the type checker has.
/// Any other cell, whatever words it has there, and any other value stops the
/// run.
#[test]
fn a_field_that_is_not_there_stops_the_run() {
    let read = |index| {
        let field = Ins::Field {
            dst: Dst(r(1)),
            src: Src(r(0)),
            index,
        };
        stores(field, 1)
    };
    on_a_machine(|m| {
        for index in [0, 1, 2, 4, 5, 252, 253, 254, 299, 300, u16::MAX] {
            let code = read(index);
            let stop = || Err(Stop::broken(Broken::NoField { index }));
            for v in one_of_each_kind(&mut m.rest.heap) {
                if v.as_cell().is_none() {
                    assert_eq!(call(m, code, &[v]).map(Value::bits), stop());
                } else {
                    assert_eq!(m.rest.release(v), Ok(()));
                }
            }
            for (cell, values) in a_cell_of_each_kind(&mut m.rest.heap) {
                let kind = m.rest.heap.kind(cell);
                let has = usize::from(index) < values;
                let got = call(m, code, &[Value::cell(cell)]).map(Value::bits);
                if has {
                    assert!(got.is_ok(), "{kind:?} {index}");
                } else {
                    assert_eq!(got, stop(), "{kind:?} {index}");
                }
            }
            assert_eq!(m.rest.heap.live(), 0, "{index}");
        }
    });
}

/// A capture is read from whatever cell is in the closure's register, at its
/// index plus one. The instruction does not ask whether that is a closure,
/// because a call puts nothing else there (`Rest::callee`). A cell that does
/// not start with that many values has no such capture.
#[test]
fn a_capture_that_is_not_there_stops_the_run() {
    for index in [0, 3, 4, 5, 251, 252, 253, 298, 299, 300, 70_000, u32::MAX] {
        let read = Ins::GetCapture {
            dst: Dst(r(0)),
            index,
        };
        let code = stores(read, 0);
        on_a_machine(|m| {
            m.rest.code = code;
            for (cell, values) in a_cell_of_each_kind(&mut m.rest.heap) {
                let kind = m.rest.heap.kind(cell);
                let has = (index as usize).saturating_add(1) < values;
                m.stack[usize::from(CLOSURE.0)] = Value::cell(cell);
                let got = m.call(&code.toplevels[0], &[]).map(Value::bits);
                if has {
                    assert!(got.is_ok(), "{kind:?} {index}");
                } else {
                    let func = code.toplevels[0].name.clone();
                    let stop = Err(Stop::broken(Broken::NoCapture { func, index }));
                    assert_eq!(got, stop, "{kind:?} {index}");
                }
                m.stack[usize::from(CLOSURE.0)] = Value::NIL;
                assert_eq!(emptied(m), Ok(()));
                assert_eq!(m.rest.heap.release(cell), Ok(()));
            }
            assert_eq!(m.rest.heap.live(), 0, "{index}");
        });
    }
}

/// A constructor the program does not list has no name to show, and a cell with
/// no constructor of the heap's or a closure with no function is one nothing
/// can show. Each stops the run and says which.
#[test]
fn a_value_with_no_name_stops_the_run() {
    on_a_machine(|m| {
        let nil = Value::NIL;
        let heap = &mut m.rest.heap;
        let one = heap.ctor(ctors().1[0], &[nil]).expect("room");
        let closure = heap.closure(FuncIdx(0), &[nil]).expect("room");
        heap.data_mut(closure)[0] = nil.bits();
        let shown = |v| m.rest.show(v, &mut Vec::new());
        let unlisted = Err(Stop::broken(Broken::NoCtor(SOME_VARIANT)));
        assert_eq!(shown(Value::nullary(SOME_VARIANT)), unlisted);
        assert_eq!(shown(Value::cell(one)), unlisted);
        let no_function = Err(Stop::broken(Broken::ClosureCell));
        assert_eq!(shown(Value::cell(closure)), no_function);
        m.rest.heap = Heap::default();
        let unknown = m.rest.heap.ctor(ctors().1[0], &[nil]).expect("room");
        let got = m.rest.show(Value::cell(unknown), &mut Vec::new());
        assert_eq!(got, Err(Stop::broken(Broken::CtorCell)));
        assert_eq!(m.rest.heap.release(unknown), Ok(()));
    });
}

/// The JSON built-ins know a document and a tree by which constructor each is,
/// and not by its variant: a constructor of another type stops the run, with
/// fields or without, whatever its number.
#[test]
fn a_json_built_in_on_another_constructor_stops_the_run() {
    let src = "import scarlet/json\npub fn main() {\n\tprintln(json::parse('1'))\n}\n";
    let result = scarlet_core::compile::compile_text(src, None);
    let program = result.into_runnable().expect("a clean compile is runnable");
    let code = crate::code::load(&program, Audit::Never).expect("it loads");
    let json = code.abi.json.expect("the json types");
    let mut host = OsHost::new(Vec::new(), Vec::new(), Vec::new());
    let mut m = Machine::new(&code, &mut host);
    let mut call = |i: Intrinsic, ctor: Option<Ctor>, plain: Value| {
        let fields = [Value::NIL; 3];
        let v = match ctor {
            Some(ctor) => Value::cell(m.rest.heap.ctor(ctor, &fields).expect("room")),
            None => plain,
        };
        let mut frame = [Value::NIL; WINDOW];
        frame[2] = v;
        let got = m.rest.builtin(i, &frame, &[r(0)]).map(Value::bits);
        (v.view(), got)
    };
    let (doc, error) = (json.doc.ctor(), json.parse_error.ctor());
    let (some, err) = (code.abi.some.ctor(), code.abi.err.ctor());
    let others = [error, json.boolean.ctor(), json.number.ctor(), some];
    for ctor in others.map(Some).into_iter().chain([None]) {
        let (view, got) = call(Intrinsic::JsonKind, ctor, json.null);
        assert_eq!(got, Err(Stop::broken(Broken::NotADoc(view))), "{ctor:?}");
    }
    let others = [doc, error, some, err];
    for ctor in others.map(Some).into_iter().chain([None]) {
        let plain = json.reasons.unexpected_end;
        assert_eq!(plain.tag(), json.null.tag(), "of the same number");
        let (view, got) = call(Intrinsic::JsonEncode, ctor, plain);
        assert_eq!(got, Err(Stop::broken(Broken::NotJson(view))), "{ctor:?}");
    }
    let (_, got) = call(Intrinsic::JsonEncode, None, json.null);
    assert!(got.is_ok());
}

/// A jump on a tag goes on at a value of that variant, and jumps at one of any
/// other, at one with fields where the variant has none, and at any other kind
/// of value but a cell with no variant of its own, which counts as variant 0.
#[test]
fn a_jump_on_a_tag_goes_on_at_one_variant() {
    let nullary = |variant_idx| {
        let name = VariantRef {
            variant_idx,
            ..SOME_VARIANT
        };
        Value::nullary(name).tag()
    };
    let tags = [0, 1, 2, u16::MAX].map(|i| [Tag::of_cell(i), nullary(i)]);
    for tag in tags.into_iter().flatten() {
        let code = falls_through(Ins::JumpUnlessTag {
            src: Src(r(0)),
            tag,
            to: pc(3),
        });
        on_a_machine(|m| {
            let heap = &mut m.rest.heap;
            let [one, pair] = ctors().1;
            let cells = [
                (heap.ctor(one, &[Value::NIL]), Tag::of_cell(1)),
                (heap.ctor(pair, &[Value::NIL; 2]), Tag::of_cell(2)),
                (heap.tuple(&[Value::NIL; 2]), Tag::of_cell(0)),
                (heap.string(b"text"), Tag::of_cell(0)),
            ];
            let cells = cells.map(|(cell, tag)| (Value::cell(cell.expect("room")), tag));
            let plain = [
                (Value::nullary(SOME_VARIANT), nullary(1)),
                (Value::NIL, Value::NIL.tag()),
                (Value::int(-1).expect("small"), Value::float(0.0).tag()),
                (Value::float(1.5), Value::float(1.5).tag()),
                (Value::func(FuncIdx(1)), Value::func(FuncIdx(1)).tag()),
            ];
            for (v, its) in cells.into_iter().chain(plain) {
                let got = call(m, code, &[v]).map(Value::bits);
                let want = Value::bool(its == tag).bits();
                assert_eq!(got, Ok(want), "{v:?} {tag:?}");
            }
            assert_eq!(m.rest.heap.live(), 0);
        });
    }
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
                dst: Dst(r(3)),
                rest: Arg {
                    reg: r(2),
                    moved: true,
                },
                back: Shares::new(&[r(0)]).expect("room for one"),
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
    let (src, to) = (Src(r(0)), Pc(START));
    let (text, bits) = (TextId(1), BinaryId(0));
    let named = [
        Ins::Str {
            dst: Dst(r(0)),
            text,
        },
        Ins::BinaryConst {
            dst: Dst(r(0)),
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

/// The wide forms of the calls do what the others do, with their arguments in
/// a list. Here a function calls itself, or goes round, once, passing the
/// array it was given, and returns what it was passed the second time.
#[test]
fn a_call_with_its_arguments_in_a_list_passes_them() {
    let list = crate::code::ArgList(0);
    let (to, own) = (EntryPc(START), Room(7));
    // A first run has `Nil` in `r1`, and a second has been told apart by the
    // global it finds set.
    let first = [
        Ins::GetGlobal {
            dst: Dst(r(1)),
            slot: GlobalSlot(0),
        },
        Ins::JumpUnlessInt {
            src: Src(r(1)),
            n: 1,
            to: pc(3),
        },
        ret(0),
        int(1, 1),
        Ins::SetGlobal {
            slot: GlobalSlot(0),
            src: Shared(r(1)),
        },
    ];
    let calls = [
        Ins::CallWide {
            dst: Dst(r(2)),
            to,
            args: list,
            own,
        },
        Ins::CallSelfWide {
            dst: Dst(r(2)),
            to,
            args: list,
            own,
        },
        Ins::TailCallWide {
            to,
            args: list,
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
    let (dst, callee, own) = (Dst(r(2)), Shared(r(1)), Room(7));
    let args = |n: u8| Args::new(&vec![r(0); usize::from(n)]).expect("a few");
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
    let (to, own) = (EntryPc(START), Room(7));
    let args = Args::new(&[]).expect("none");
    let list = crate::code::ArgList(1);
    let calls = [
        (Ins::TailCall { to, args, own }, false),
        (
            Ins::TailCallWide {
                to,
                args: list,
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
                dst: Dst(r(1)),
                slot: GlobalSlot(0),
            },
            Ins::JumpUnlessInt {
                src: Src(r(1)),
                n: 1,
                to: pc(4),
            },
            Ins::GetSelf {
                dst: Dst(r(2)),
                this: FuncIdx(0),
            },
            Ins::RetClosure {
                src: Taken(r(2)),
                last: Room(7).last(),
            },
            int(1, 1),
            Ins::SetGlobal {
                slot: GlobalSlot(0),
                src: Shared(r(1)),
            },
            call,
        ];
        let code = function(0, 5, body);
        let mut host = OsHost::new(Vec::new(), Vec::new(), Vec::new());
        let mut m = Machine::new(code, &mut host);
        let captured = [Value::NIL];
        let closure = m.rest.heap.closure(FuncIdx(0), &captured).expect("room");
        let at = started(&mut m, Link::new(0, Room(0), Dst(RESULT)));
        let mut win = window(&mut m.stack, 0).expect("a window");
        win.run_as(Value::cell(closure));
        let v = m.go_on(at).expect("it returns");
        assert_eq!(v.as_cell(), keeps.then_some(closure), "{call:?}");
        assert_eq!(m.rest.heap.live(), usize::from(keeps), "{call:?}");
        assert_eq!(m.rest.release(v), Ok(()));
        assert_eq!(m.rest.heap.live(), 0);
    }
}

/// A tail call leaves no cell in the frame's registers but those it passes: a
/// register the function put one in before it reads as `Nil` after.
#[test]
fn a_tail_call_empties_the_registers_it_does_not_pass() {
    let (to, own) = (EntryPc(START), Room(7));
    let none = || Args::new(&[]).expect("none");
    let one = || Args::new(&[r(0)]).expect("room for one");
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
                dst: Dst(r(1)),
                slot: GlobalSlot(0),
            },
            Ins::JumpUnlessInt {
                src: Src(r(1)),
                n: 1,
                to: pc(3),
            },
            ret(4),
            int(1, 1),
            Ins::SetGlobal {
                slot: GlobalSlot(0),
                src: Shared(r(1)),
            },
            Ins::Str {
                dst: Dst(r(4)),
                text: TextId(0),
            },
            call,
        ];
        after(5, body, |m, got| {
            assert_eq!(got.map(|v| v.bits()), Ok(Value::NIL.bits()), "{call:?}");
            assert!(no_cell_on(&m.stack));
            assert_eq!(m.rest.heap.live(), 0);
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
            dst: Dst(r(4)),
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
        dst: Hit(r(2)),
        src: Arg { reg: r(1), moved },
        index: Src(r(index)),
        value: Arg {
            reg: r(value),
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
            dst: Dst(r(1)),
            start: Src(r(0)),
            end: Src(r(4)),
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
/// nothing, stops and says so. So does one that counts its frames past a link
/// that leads to none.
#[test]
fn a_run_that_leaves_the_code_stops() {
    let body = vec![Ins::Jump { to: pc(1) }];
    after(5, body, |_, got| {
        let stopped = got.map(|v| v.bits());
        let func = "test.function".to_string();
        assert_eq!(stopped, Err(Stop::broken(Broken::RanPastEnd { func })));
    });
    on_a_machine(|m| {
        let at = started(m, Link::new(0, Room(9), Dst(RESULT)));
        let got = m.go_on(at).map(|v| v.bits());
        assert_eq!(got, Err(Stop::broken(Broken::NoWindow)));
    });
    let depth = Ins::StackDepth { dst: Dst(r(1)) };
    let halt = Ins::Halt { src: Taken(r(1)) };
    let code = function(0, 5, vec![depth, halt]);
    for (below, want) in [
        (0, Ok(Value::int(1).expect("small").bits())),
        (9, Err(Stop::broken(Broken::NoWindow))),
    ] {
        let mut host = OsHost::new(Vec::new(), Vec::new(), Vec::new());
        let mut m = Machine::new(code, &mut host);
        let at = started(&mut m, Link::new(0, Room(below), Dst(RESULT)));
        assert_eq!(m.go_on(at).map(|v| v.bits()), want, "{below}");
    }
}

/// Start a frame at the bottom of `m`'s stack, which returns where `link`
/// says, and give where to run it from.
fn started(m: &mut Machine<'_, '_>, link: Link) -> At {
    m.grow(0).expect("room");
    window(&mut m.stack, 0).expect("a window").start(link);
    At {
        pc: START as usize,
        base: 0,
    }
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
/// leaves what they held.
fn call(m: &mut Machine<'_, '_>, code: &'static Code, args: &[Value]) -> Result<Value, Stop> {
    m.rest.code = code;
    let got = m.call(&code.toplevels[0], args);
    assert_eq!(emptied(m), Ok(()));
    got
}

/// Give up what the frame at the bottom of the stack holds.
fn emptied(m: &mut Machine<'_, '_>) -> Result<(), DeadCell> {
    let mut win = window(&mut m.stack, 0).expect("a window");
    win.leave(&mut m.rest.heap, Room(7).last())
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
            Ins::$name { dst: Dst(r(dst)), a: Src(r(0)), b: Src(r(1)) },
            $op,
        )),*]
    };
}
macro_rules! with_constant {
    ([] $($name:ident $op:path),*) => {
        |k, dst: u8| vec![$((
            Ins::$name { dst: Dst(r(dst)), a: Src(r(0)), k },
            $op,
        )),*]
    };
}
macro_rules! compare_pairs {
    ([] $($name:ident $jump:ident $op:path),*) => {
        |dst: u8| vec![$((
            Ins::$name { dst: Dst(r(dst)), a: Src(r(0)), b: Src(r(1)) },
            Ins::$jump { a: Src(r(0)), b: Src(r(1)), to: pc(3) },
            $op,
        )),*]
    };
}
macro_rules! compare_with_constant {
    ([] $($name:ident $jump:ident $op:path),*) => {
        |k, dst: u8| vec![$((
            Ins::$name { dst: Dst(r(dst)), a: Src(r(0)), k },
            Ins::$jump { a: Src(r(0)), k, to: pc(3) },
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
                dst: Dst(r(dst)),
                a: Src(r(0)),
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
                dst: Dst(r(dst)),
                a: Src(r(0)),
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
                dst: Dst(r(dst)),
                a: Src(r(0)),
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
        let (dst, a) = (Dst(r(2)), Src(r(0)));
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
    assert_eq!(got, Err(Stop::HeapFull), "{what}");
    assert_eq!(m.rest.heap.live(), held, "{what}");
    assert_eq!(emptied(m), Ok(()), "{what}");
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
            let (dst, a, b) = (Dst(r(over)), Src(r(0)), Src(r(1)));
            finds_the_heap_full(m, Ins::IntAdd { dst, a, b }, over, [&top, &one]);
            finds_the_heap_full(m, Ins::IntAdd { dst, a, b }, over, [&big, &big]);
            finds_the_heap_full(m, Ins::IntMul { dst, a, b }, over, [&big, &top]);
        }
        for over in OVER_ONE {
            let (dst, a) = (Dst(r(over)), Src(r(0)));
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

/// A recursion through each kind of call there is, with the kinds it is to be
/// loaded as and what it prints for `N` of 30. Each holds a string or an array
/// through it, so that a call that counted twice, or not at all, shows.
const DEEP: [(&[&str], &str, &str); 12] = [
    (
        &["Call1", "Call0"],
        "fn leaf() String { '${N}' }\n\
         fn down(n Int) String { if n == 0 { '${leaf()}' } else { '${down(n - 1)}.' } }\n\
         pub fn main() { println(string::length(down(N))) }\n",
        "32\n",
    ),
    (
        &["Call1Share"],
        "fn down(xs Array(Int)) Int {\n\
         \tmatch xs {\n\t\t[] -> 0\n\t\t[_, ..t] -> part(t) + array::length(xs)\n\t}\n}\n\
         fn part(xs Array(Int)) Int { down(xs) + array::length(xs) }\n\
         pub fn main() { println(down([..{ 0..N }, 1])) }\n",
        "961\n",
    ),
    (
        &["Call2"],
        "fn down(s String, n Int) Int {\n\
         \tif n == 0 { string::length(s) } else { down('${s}x', n - 1) + 1 }\n}\n\
         pub fn main() { println(down('', N)) }\n",
        "60\n",
    ),
    (
        &["Call2Share"],
        "fn down(s String, n Int) Int {\n\
         \tif n == 0 { 0 } else { down(s, n - 1) + string::length(s) }\n}\n\
         pub fn main() { println(down('ab', N)) }\n",
        "60\n",
    ),
    (
        &["Call3", "Call3Share"],
        "fn down(s String, t String, n Int) Int {\n\
         \tif n == 0 { 0 } else { down(t, s, n - 1) + last(s, n, n) }\n}\n\
         fn last(s String, a Int, b Int) Int {\n\
         \tif a == 0 { string::length(s) + b } else { last('${s}y', 0, 0) + 1 }\n}\n\
         pub fn main() { println(down('ab', 'c', N)) }\n",
        "105\n",
    ),
    (
        &["Call"],
        "fn down(s String, n Int, a Int, b Int, t String) Int {\n\
         \tif n == 0 { a + b + string::length(t) } else { down(s, n - 1, b, a, '${t}z') + string::length(s) }\n}\n\
         pub fn main() { println(down('ab', N, 1, 2, '')) }\n",
        "93\n",
    ),
    (
        &["CallWide"],
        "fn down(s String, n Int, a Int, b Int, t String, c Int, d Int) Int {\n\
         \tif n == 0 { a + b + c + d + string::length(t) } else { down(s, n - 1, b, a, '${t}z', d, c) + string::length(s) }\n}\n\
         pub fn main() { println(down('ab', N, 1, 2, '', 3, 4)) }\n",
        "100\n",
    ),
    (
        &["CallSelf", "RetClosure", "GetCapture"],
        "pub fn main() {\n\
         \tkept = '${N}'\n\
         \tdown = fn(s, n) { if n == 0 { '${s}${kept}' } else { '${down(s, n - 1)}${s}' } }\n\
         \tprintln(string::length(down('ab', N)))\n}\n",
        "64\n",
    ),
    (
        &["CallSelfWide"],
        "pub fn main() {\n\
         \tkept = '${N}'\n\
         \tdown = fn(s, n, a, b, c, d, e) {\n\
         \t\tif n == 0 { '${s}${kept}' } else { '${down(s, n - 1, b, c, d, e, a)}${s}' }\n\t}\n\
         \tprintln(string::length(down('ab', N, 1, 2, 3, 4, 5)))\n}\n",
        "64\n",
    ),
    (
        &["CallValue"],
        "fn down(f fn(String) String, n Int) Int {\n\
         \tif n == 0 { 0 } else { down(fn(s) f('${s}.'), n - 1) + string::length(f('')) }\n}\n\
         pub fn main() {\n\tkept = 'ab'\n\tprintln(down(fn(s) '${s}${kept}', N))\n}\n",
        "495\n",
    ),
    (
        &["CallValueWide"],
        "fn down(f fn(Int, Int, Int, Int, Int, Int, String) String, n Int) String {\n\
         \tif n == 0 { '${f(1, 2, 3, 4, 5, 6, 'x')}' } else {\n\t\tdown(fn(a, b, c, d, e, g, s) '${f(b, c, d, e, g, a, '${s}.')}', n - 1)\n\t}\n}\n\
         pub fn main() {\n\tkept = 'ab'\n\
         \tprintln(string::length(down(fn(a, b, c, d, e, g, s) '${s}${kept}${a + b + c + d + e + g}', N)))\n}\n",
        "35\n",
    ),
    (
        &["CallValue"],
        "fn down(n Int) Array(String) {\n\
         \tif n == 0 { ['a'] } else {\n\
         \t\tarray::map(['${n}'], fn(s) '${array::length(down(n - 1))}${folded(n)}${s}')\n\t}\n}\n\
         fn folded(n Int) Int {\n\
         \tmap::fold(map::set(map::new(), 'k${n}', n), 0, fn(acc, _k, v) acc + v)\n}\n\
         pub fn main() { println(down(N)) }\n",
        "[13030]\n",
    ),
];

fn deep_source(body: &str, n: usize) -> String {
    let imports = "import scarlet/array\nimport scarlet/map\nimport scarlet/string\n";
    format!("{imports}{}", body.replace('N', &n.to_string()))
}

/// What a run of `src` on a stack of no more than `limit` registers came to,
/// with how many it had at its end and what it printed. Whether it ends or
/// stops, every reference count is right then.
fn on_a_stack_of(code: &'static Code, limit: usize) -> (Result<(), Stop>, usize, String) {
    let mut host = OsHost::new(Vec::new(), Vec::new(), Vec::new());
    let mut m = Machine::new(code, &mut host);
    m.stack_limit = limit;
    let ended = m.run();
    let len = m.stack.len();
    let ended = match ended {
        Ok(ended) => {
            assert!(no_cell_on(&m.stack), "a frame's registers outlived it");
            m.finish(ended, Audit::AtEnd)
        }
        Err(stop) => {
            assert_eq!(check(&m.stack, &m.rest, Value::NIL), Ok(()), "{stop:?}");
            assert!(m.rest.passing.is_empty());
            Err(stop)
        }
    };
    drop(m);
    let out = String::from_utf8(std::mem::take(host.out())).expect("UTF-8");
    (ended, len, out)
}

fn loaded(src: &str, audit: Audit) -> &'static Code {
    let result = scarlet_core::compile::compile_text(src, None);
    assert!(result.success(), "{src}: {:?}", result.diagnostics());
    let program = result.into_runnable().expect("a clean compile is runnable");
    Box::leak(Box::new(
        crate::code::load(&program, audit).expect("the program loads"),
    ))
}

/// A recursion that goes deeper than the stack it starts with has room made
/// for it, in front of every kind of call, and nothing about it changes: it
/// prints what it would, and no cell is left. The stack is made longer a
/// little at a time, so any of its calls may be the one that asks.
#[test]
fn a_recursion_through_any_kind_of_call_makes_the_stack_longer() {
    for (kinds, body, out) in DEEP {
        let code = loaded(&deep_source(body, 30), Audit::Never);
        for kind in kinds {
            let loaded = code.ins.iter().any(|ins| ins.op().name() == *kind);
            assert!(loaded, "{kind}: {body}");
        }
        let (ended, _, printed) = on_a_stack_of(code, MAX_STACK);
        assert_eq!((ended, printed.as_str()), (Ok(()), out), "{body}");

        let code = loaded(&deep_source(body, 1000), Audit::Never);
        let (ended, len, _) = on_a_stack_of(code, MAX_STACK);
        assert_eq!(ended, Ok(()), "{body}");
        assert!(len > 3000, "{len}: {body}");
    }
}

/// A call that finds no room stops the run as a full stack, having shared and
/// written nothing: every count is right, whichever call it was and whatever it
/// was to pass. With one register more it goes one call further, until the run
/// has the room it needs.
#[test]
fn a_call_with_no_room_stops_with_every_count_right() {
    for (_, body, out) in DEEP {
        let code = loaded(&deep_source(body, 30), Audit::Never);
        let (mut stopped, mut limit) = (0, WINDOW);
        loop {
            let (ended, len, printed) = on_a_stack_of(code, limit);
            assert!(len <= limit, "{len} of {limit}");
            match ended {
                Ok(()) => {
                    assert_eq!(printed, out, "{limit}: {body}");
                    break;
                }
                Err(stop) => assert_eq!(stop, Stop::StackFull, "{limit}: {body}"),
            }
            stopped += 1;
            limit += 1;
        }
        // Each of 30 frames has a link, a closure and a value at least.
        assert!(stopped > 90, "{stopped}: {body}");
    }
    // Below a window not even the first frame is opened.
    let code = loaded("pub fn main() { println(1) }\n", Audit::Never);
    let (ended, len, _) = on_a_stack_of(code, WINDOW - 1);
    assert_eq!((ended, len), (Err(Stop::StackFull), 0));
}

/// The same recursions, with the heap checked in front of every instruction.
#[test]
fn a_recursion_is_right_at_every_instruction() {
    for (_, body, _) in DEEP {
        let src = deep_source(body, 12);
        let plain = on_a_stack_of(loaded(&src, Audit::Never), MAX_STACK);
        let checked = on_a_stack_of(loaded(&src, Audit::EachInstruction), MAX_STACK);
        assert_eq!(plain.0, Ok(()), "{body}");
        assert_eq!(plain, checked, "{body}");
    }
}

/// A stack longer than its limit is a full stack, and so is one past what a
/// frame's place can be numbered in. What was there is as it was. What the stack
/// does when the system has no memory to give is not tested here.
#[test]
fn a_stack_past_its_limit_is_full() {
    on_a_machine(|m| {
        let len = m.stack.len();
        for base in [u32::MAX, u32::MAX - 254] {
            assert_eq!(m.grow(base), Err(Stop::StackFull), "{base}");
        }
        assert_eq!(m.stack.len(), len);
        // A stack is longer by a sixteenth than was asked for, but never past
        // its limit, and a window that fits is not asked for again.
        m.stack_limit = 1000;
        assert_eq!(m.grow(100), Ok(Grown::Longer));
        assert_eq!(m.stack.len(), 356 + 356 / 16);
        assert_eq!(m.grow(744), Ok(Grown::Longer));
        assert_eq!(m.stack.len(), 1000);
        assert_eq!(m.grow(745), Err(Stop::StackFull));
        for base in [0, 100, 744] {
            assert_eq!(m.grow(base), Ok(Grown::HadRoom), "{base}");
        }
        assert_eq!(m.stack.len(), 1000);
        assert!(no_cell_on(&m.stack));
    });
}

/// A function that does `first` in its first run and `second` in its second,
/// which is told apart by the global it finds set.
fn runs_twice(reads: ReadsClosure, first: &[Ins<Reg>], second: &[Ins<Reg>]) -> &'static Code {
    let until = u32::try_from(second.len()).expect("a short function");
    let told_apart = [
        Ins::GetGlobal {
            dst: Dst(r(1)),
            slot: GlobalSlot(0),
        },
        Ins::JumpUnlessInt {
            src: Src(r(1)),
            n: 1,
            to: pc(2 + until),
        },
    ];
    let set = [
        int(1, 1),
        Ins::SetGlobal {
            slot: GlobalSlot(0),
            src: Shared(r(1)),
        },
    ];
    let body = [&told_apart[..], second, &set, first].concat();
    function_that(reads, 1, 5, body)
}

/// The calls of the value in `r0`, each of which passes it to itself and leaves
/// its result in `r2`, when it is one that comes back.
fn calls_of_a_value() -> [Ins<Reg>; 4] {
    let (dst, callee, own) = (Dst(r(2)), Shared(r(0)), Room(7));
    let (args, list) = (Args::new(&[r(0)]).expect("one"), crate::code::ArgList(2));
    [
        Ins::CallValue {
            dst,
            callee,
            own,
            args,
        },
        Ins::CallValueWide {
            dst,
            callee,
            own,
            args: list,
        },
        Ins::TailCallValue { callee, args, own },
        Ins::TailCallValueWide {
            callee,
            args: list,
            own,
        },
    ]
}

/// The return of a function that reads `reads` of its closure.
fn back(reads: ReadsClosure, src: u8) -> Ins<Reg> {
    let (src, last) = (Taken(r(src)), Room(7).last());
    if reads.keeps() {
        Ins::RetClosure { src, last }
    } else {
        Ins::Ret { src, last }
    }
}

/// Run `code`'s function on a new machine, with what `given` makes in its heap.
fn on_a_new_machine(
    code: &'static Code,
    given: impl FnOnce(&mut Heap) -> Value,
    check: impl FnOnce(&mut Machine<'_, '_>, Value, Result<Value, Stop>),
) {
    let mut host = OsHost::new(Vec::new(), Vec::new(), Vec::new());
    let mut m = Machine::new(code, &mut host);
    let given = given(&mut m.rest.heap);
    let got = m.call(&code.toplevels[0], &[given]);
    check(&mut m, given, got);
}

/// A closure of the function these tests run, over `captures`.
fn closure_over(heap: &mut Heap, captures: &[Value]) -> Value {
    Value::cell(heap.closure(FuncIdx(0), captures).expect("room"))
}

/// A call of a closure whose function reads none of it passes no closure on, so
/// the frame has none to give up, and is the plain function if it asks. One
/// whose function keeps it holds a reference to it for as long as the frame
/// runs, and is that closure.
#[test]
fn a_frame_holds_its_closure_when_its_function_keeps_it() {
    let all = [
        ReadsClosure::Nothing,
        ReadsClosure::Itself,
        ReadsClosure::Captures,
    ];
    for reads in all {
        for call in calls_of_a_value() {
            let itself = Ins::GetSelf {
                dst: Dst(r(3)),
                this: FuncIdx(0),
            };
            let second = [Ins::Audit, itself, Ins::Audit, back(reads, 3)];
            let first = [call, Ins::Audit, back(reads, 2)];
            let code = runs_twice(reads, &first, &second);
            let closure = |heap: &mut Heap| closure_over(heap, &[Value::NIL]);
            on_a_new_machine(code, closure, |m, closure, got| {
                let v = got.expect("it returns");
                let want = if reads.keeps() {
                    closure
                } else {
                    Value::func(FuncIdx(0))
                };
                assert_eq!(v.bits(), want.bits(), "{call:?} {reads:?}");
                assert!(no_cell_on(&m.stack));
                assert_eq!(m.rest.release(v), Ok(()));
                assert_eq!(m.rest.heap.live(), 0, "{call:?} {reads:?}");
            });
        }
    }
}

/// A function called by name runs as no closure, whatever the frame that was
/// there before left in the closure's register.
#[test]
fn a_function_called_by_name_is_no_closure() {
    let reads = ReadsClosure::Itself;
    let call = Ins::Call0 {
        dst: Dst(r(2)),
        own: Room(7),
        to: EntryPc(START),
    };
    let itself = Ins::GetSelf {
        dst: Dst(r(3)),
        this: FuncIdx(0),
    };
    for left in [Value::func(FuncIdx(5)), Value::float(1.5), Value::NIL] {
        let code = runs_twice(reads, &[call, back(reads, 2)], &[itself, back(reads, 3)]);
        let mut host = OsHost::new(Vec::new(), Vec::new(), Vec::new());
        let mut m = Machine::new(code, &mut host);
        m.grow(0).expect("room");
        window(&mut m.stack, 7).expect("a window").run_as(left);
        let got = m.call(&code.toplevels[0], &[Value::NIL]).map(|v| v.bits());
        assert_eq!(got, Ok(Value::func(FuncIdx(0)).bits()), "{left:?}");
    }
}

/// A read of a capture that the closure does not have stops the run and names
/// the function, and so does one in a function that runs as no closure. Every
/// count is right then.
#[test]
fn a_read_of_a_capture_that_is_not_there_stops() {
    let reads = ReadsClosure::Captures;
    for (closure, index, there) in [
        (true, 0, true),
        (true, 1, false),
        (true, u32::MAX, false),
        (false, 0, false),
        (false, 1, false),
    ] {
        let make = |heap: &mut Heap| match closure {
            true => closure_over(heap, &[Value::bool(true)]),
            false => Value::func(FuncIdx(0)),
        };
        for call in calls_of_a_value() {
            let read = Ins::GetCapture {
                dst: Dst(r(3)),
                index,
            };
            let code = runs_twice(reads, &[call, back(reads, 2)], &[read, back(reads, 3)]);
            on_a_new_machine(code, make, |m, _, got| {
                let want = if there {
                    Ok(Value::bool(true).bits())
                } else {
                    let func = "test.function".to_string();
                    Err(Stop::broken(Broken::NoCapture { func, index }))
                };
                assert_eq!(got.map(|v| v.bits()), want, "{call:?} {index}");
                assert_eq!(check(&m.stack, &m.rest, Value::NIL), Ok(()));
            });
        }
    }
}

/// An instruction that makes a cell, and what it is.
struct Maker {
    ins: Ins<Reg>,
    /// The list it names, if it names one.
    lists: Lists<Reg>,
    kind: Kind,
    /// What it hands to the cell, in order.
    given: Vec<Arg<Reg>>,
}

impl Maker {
    /// A function that does the instruction and then stops, so that what it
    /// left in its registers can be looked at.
    fn code(&self) -> &'static Code {
        let stop = Ins::Bad { why: WhyId(0) };
        let (reads, values) = (ReadsClosure::Nothing, 254);
        let body = vec![self.ins, stop];
        let lists = Lists {
            regs: self.lists.regs.clone(),
            args: self.lists.args.clone(),
            dsts: Vec::new(),
        };
        function_of(reads, 0, values, body, lists, as_wide_as_you_like().0)
    }

    /// How many values its cell holds.
    fn holds(&self) -> usize {
        self.given.len() + usize::from(self.kind == Kind::Closure)
    }

    /// The registers of its operands.
    fn regs(&self) -> Vec<Reg> {
        self.given.iter().map(|a| a.reg).collect()
    }
}

/// A program's constructors, where the one of each number of fields is at that
/// place, and its variant is its place.
fn as_wide_as_you_like() -> (Ctors, Vec<Ctor>) {
    let mut numbering = Numbering::default();
    let ctors = (0..=400).map(|n| {
        let variant = VariantRef {
            variant_idx: u16::try_from(n).expect("a few"),
            ..SOME_VARIANT
        };
        numbering.add(variant, n).expect("few constructors")
    });
    let ctors = ctors.collect();
    (numbering.finish(), ctors)
}

/// Every instruction that makes a cell out of `regs`, writing `dst`. Of the
/// operands that name one register only the last may take it, as the loader has
/// it.
fn makers(dst: Reg, regs: &[Reg]) -> Vec<Maker> {
    let n = regs.len();
    let dst = Dst(dst);
    let func = FuncIdx(0);
    let mut made = Vec::new();
    let mut add = |ins: Ins<Reg>, kind, given: &[Arg<Reg>]| {
        let args = vec![given.into()];
        let regs = vec![given.iter().map(|a| a.reg).collect()];
        made.push(Maker {
            ins,
            lists: Lists {
                regs,
                args,
                dsts: Vec::new(),
            },
            kind,
            given: given.to_vec(),
        });
    };
    let shared: Vec<_> = regs.iter().copied().map(Arg::shared).collect();
    if let Some(elements) = Shares::new(regs) {
        add(Ins::Tuple { dst, elements }, Kind::Tuple, &shared);
    }
    if let Ok(two) = <[Reg; 2]>::try_from(regs) {
        let elements = two.map(Shared);
        add(Ins::Tuple2 { dst, elements }, Kind::Tuple, &shared);
    }
    if let Ok(three) = <[Reg; 3]>::try_from(regs) {
        let elements = three.map(Shared);
        add(Ins::Tuple3 { dst, elements }, Kind::Tuple, &shared);
    }
    let elements = crate::code::ShareList(0);
    add(Ins::TupleWide { dst, elements }, Kind::Tuple, &shared);
    if n == 0 {
        return made;
    }
    if let Some(captures) = Shares::new(regs) {
        let closure = Ins::Closure {
            dst,
            func,
            captures,
        };
        add(closure, Kind::Closure, &shared);
    }
    let closure = Ins::ClosureWide {
        dst,
        func,
        captures: crate::code::ShareList(0),
    };
    add(closure, Kind::Closure, &shared);
    // The constructors, with every field taken that may be, none, and each in
    // turn with the ones around it the other way.
    let ctor = as_wide_as_you_like().1[n];
    let last = |i: usize| regs.iter().rposition(|r| *r == regs[i]) == Some(i);
    let taking = [
        |_: usize| true,
        |_: usize| false,
        |i: usize| i.is_multiple_of(2),
        |i: usize| !i.is_multiple_of(2),
    ];
    for takes in taking {
        let given: Vec<_> = (0..n)
            .map(|i| Arg {
                reg: regs[i],
                moved: takes(i) && last(i),
            })
            .collect();
        let fields = crate::code::ArgList(0);
        add(Ins::CtorWide { dst, ctor, fields }, Kind::Ctor, &given);
        let few = Args::new(regs).map(|fields| Ins::Ctor { dst, ctor, fields });
        if let Some(mut ins) = few {
            let mut moved = given.iter().map(|a| a.moved);
            ins.decide(&mut Lists::default(), &mut |_| {
                moved.next().expect("as many as fields")
            });
            add(ins, Kind::Ctor, &given);
        }
        // The form that takes each, where each may be taken.
        macro_rules! form {
            ($($name:ident)*) => {
                $(if let Ok(fields) = regs.try_into() && given.iter().all(|a| a.moved) {
                    let fields = <[Reg; _]>::map(fields, Taken);
                    add(Ins::$name { dst, ctor, fields }, Kind::Ctor, &given);
                })*
            };
        }
        form!(Ctor1 Ctor2 Ctor3 Ctor4 Ctor5 Ctor6 Ctor7 Ctor8);
    }
    made
}

/// The operands a test of `n` of them uses: each in a register of its own, as
/// far as there are registers, one register for all of them, and two that come
/// by turns.
fn operand_registers(n: usize) -> Vec<Vec<Reg>> {
    let mut all = vec![(0..n).map(|i| r((i % 240) as u8)).collect::<Vec<_>>()];
    if n > 1 {
        all.push(vec![r(0); n]);
        all.push((0..n).map(|i| r(u8::from(i % 2 == 1))).collect());
    }
    all
}

/// Give registers `regs` a value each, every other one a new cell and the rest
/// numbers, and `dst` a cell of its own when it is none of them. Gives what
/// each register of the frame then holds.
fn hold(m: &mut Machine<'_, '_>, regs: &[Reg], dst: Reg, cells: bool) -> Vec<Value> {
    let heap = &mut m.rest.heap;
    let mut registers = vec![Value::NIL; WINDOW];
    for (i, reg) in (0..).zip(regs) {
        let held = &mut registers[usize::from(reg.0)];
        if held.bits() == Value::NIL.bits() {
            *held = if cells && i % 2 == 0 {
                Value::cell(heap.make(Kind::Binary, &[i; 12]).expect("room"))
            } else {
                Value::int(1000 + i64::from(reg.0)).expect("small")
            };
        }
    }
    if cells && !regs.contains(&dst) {
        let old = heap.make(Kind::Binary, &[77; 12]).expect("room");
        registers[usize::from(dst.0)] = Value::cell(old);
    }
    registers
}

/// Run `code` from its start with `registers`, until it stops, and give what
/// it stopped with.
fn run_with(m: &mut Machine<'_, '_>, code: &'static Code, registers: &[Value]) -> Stop {
    m.rest.code = code;
    let at = started(m, Link::new(0, Room(0), Dst(RESULT)));
    let held = usize::from(FIRST.0)..WINDOW;
    m.stack[held.clone()].copy_from_slice(&registers[held]);
    match m.go_on(at) {
        Ok(v) => panic!("it returned {v:?}"),
        Err(stop) => stop,
    }
}

/// What a run of `maker` is to leave in the registers but `dst`, which held
/// `registers`: those that were handed on with their reference hold nothing.
fn left_in(maker: &Maker, registers: &[Value]) -> Vec<u64> {
    let mut left: Vec<u64> = registers.iter().map(|v| v.bits()).collect();
    let taken = maker.given.iter().filter(|a| a.moved).map(|a| a.reg);
    for reg in taken {
        left[usize::from(reg.0)] = Value::NIL.bits();
    }
    left
}

/// Run `maker` with its operands, some of them cells if `cells`, on a heap that
/// already holds `beside` more and has a freed cell of the size it needs if
/// `freed`, and check everything it did.
fn makes(m: &mut Machine<'_, '_>, code: &'static Code, maker: &Maker, dst: Reg, at: (bool, usize)) {
    let (cells, beside) = at;
    for freed in [false, true] {
        let what = format!("{:?} {cells} {freed} {beside}", maker.ins);
        m.rest.heap = Heap::new(code.ctors.clone());
        m.rest.globals = (0..beside)
            .map(|_| Value::cell(m.rest.heap.tuple(&[]).expect("room")))
            .collect();
        let registers = hold(m, &maker.regs(), dst, cells);
        let space = freed.then(|| a_freed_cell(&mut m.rest.heap, maker.holds()));
        let made = m.rest.heap.made();
        let stop = run_with(m, code, &registers);
        assert_eq!(stop, Stop::broken(Broken::NoArm), "{what}");
        assert_eq!(m.rest.heap.made(), made + 1, "{what}");
        let cell = is_made(m, maker, dst, &registers, &what);
        if let Some(space) = space {
            assert_eq!(cell.place(), space.place(), "{what}");
        }
        all_given_up(m);
    }
}

/// A cell of `size` values that has been freed.
fn a_freed_cell(heap: &mut Heap, size: usize) -> Cell {
    let cell = heap.tuple(&vec![Value::NIL; size]).expect("room");
    heap.release(cell).expect("a live cell");
    cell
}

/// Check that `maker`, run with `registers`, left in `dst` the cell it says,
/// and what it should in every other register, with every count right. Gives
/// the cell.
fn is_made(m: &Machine<'_, '_>, maker: &Maker, dst: Reg, registers: &[Value], what: &str) -> Cell {
    let heap = &m.rest.heap;
    let regs = maker.regs();
    let cell = m.stack[usize::from(dst.0)].as_cell().expect("a cell");
    assert!(heap.is(cell, maker.kind), "{what}");
    assert_eq!(heap.unique(cell), Ok(true), "{what}");
    let want: Vec<u64> = regs
        .iter()
        .map(|r| registers[usize::from(r.0)].bits())
        .collect();
    let held: Vec<u64> = if maker.kind == Kind::Closure {
        assert_eq!(heap.closure_func(cell), Some(FuncIdx(0)), "{what}");
        heap.captures(cell).map(Value::bits).collect()
    } else {
        heap.values(cell).map(Value::bits).collect()
    };
    assert_eq!(held, want, "{what}");
    let is_ctor = (maker.kind == Kind::Ctor).then(|| as_wide_as_you_like().1[regs.len()]);
    assert_eq!(heap.ctor_of(cell), is_ctor, "{what}");
    let left = left_in(maker, registers);
    for (i, (got, want)) in m.stack.iter().zip(&left).enumerate().skip(2) {
        if i != usize::from(dst.0) {
            assert_eq!(got.bits(), *want, "{what}: register {i}");
        }
    }
    assert_eq!(check(&m.stack, &m.rest, Value::NIL), Ok(()), "{what}");
    cell
}

/// Give up everything the registers and the globals hold, which is everything
/// there is.
fn all_given_up(m: &mut Machine<'_, '_>) {
    let mut win = window(&mut m.stack, 0).expect("a window");
    win.leave(&mut m.rest.heap, Room(256).last())
        .expect("every count is right");
    for v in std::mem::take(&mut m.rest.globals) {
        m.rest.release(v).expect("every count is right");
    }
    assert_eq!(m.rest.heap.live(), 0);
}

/// Every instruction that makes a cell makes the one it says, holding its
/// operands in order, whatever it shares and takes and whichever register it
/// writes: one of its own, or an operand. What it takes leaves its register,
/// what it shares stays, the value its register held is given up, and every
/// count is right. It is the same in the space of a freed cell, at the end of
/// the arena, where the arena has to be made longer, and in a box.
#[test]
fn every_instruction_that_makes_a_cell_makes_it_of_its_operands() {
    let forms = [
        "Ctor1",
        "Ctor2",
        "Ctor3",
        "Ctor4",
        "Ctor5",
        "Ctor6",
        "Ctor7",
        "Ctor8",
        "Ctor",
        "CtorWide",
        "Tuple2",
        "Tuple3",
        "Tuple",
        "TupleWide",
        "Closure",
        "ClosureWide",
    ];
    let mut ran: Vec<&str> = Vec::new();
    on_a_machine(|m| {
        for n in (0..=9).chain([20, 252, 253, 254, 300]) {
            for regs in operand_registers(n) {
                let ends = [regs.first(), regs.last()];
                let dsts = ends.into_iter().flatten().copied().chain([r(250)]);
                for dst in dsts {
                    for maker in makers(dst, &regs) {
                        ran.push(maker.ins.op().name());
                        let code = maker.code();
                        for cells in [false, true] {
                            makes(m, code, &maker, dst, (cells, 0));
                        }
                        // With more and more beside it, so that the arena is
                        // full somewhere on the way.
                        let more = if n < 4 { 1..9 } else { 0..0 };
                        for beside in more {
                            makes(m, code, &maker, dst, (beside % 2 == 0, beside));
                        }
                    }
                }
            }
        }
    });
    for form in forms {
        assert!(ran.contains(&form), "{form}");
    }
}

/// An instruction that makes a cell on a heap with no room for it stops with a
/// full heap and has done nothing: each register holds what it held, and every
/// count is right. It is the same where the arena is full, where a box is too
/// long, and where a box fits and the stub beside it does not.
#[test]
fn a_cell_with_no_room_leaves_every_operand_as_it_was() {
    on_a_machine(|m| {
        for n in (0..=9).chain([20, 252, 253, 254, 300]) {
            for regs in operand_registers(n) {
                let ends = [regs.first(), regs.last()];
                let dsts = ends.into_iter().flatten().copied().chain([r(250)]);
                for dst in dsts {
                    for maker in makers(dst, &regs) {
                        refuses(m, &maker, dst);
                    }
                }
            }
        }
    });
}

/// Run `maker` on a heap with less room than its cell needs, every amount of it
/// for a short cell, and for a long one the amounts at each end, and check that
/// it changed nothing.
fn refuses(m: &mut Machine<'_, '_>, maker: &Maker, dst: Reg) {
    let regs = maker.regs();
    let code = maker.code();
    let needs = words_of(maker.holds());
    m.rest.heap = Heap::default();
    hold(m, &regs, dst, true);
    let held = m.rest.heap.words_used().max(1);
    let ends = |room: &usize| needs < 12 || *room < 4 || *room > needs - 5;
    for room in (0..needs).filter(ends) {
        let what = format!("{:?} in {room}", maker.ins);
        m.rest.heap = Heap::limited(held + room);
        let registers = hold(m, &regs, dst, true);
        let before = (
            m.rest.heap.made(),
            m.rest.heap.live(),
            m.rest.heap.words_used(),
        );
        assert_eq!(run_with(m, code, &registers), Stop::HeapFull, "{what}");
        let heap = &m.rest.heap;
        let after = (heap.made(), heap.live(), heap.words_used());
        assert_eq!(after, before, "{what}");
        is_untouched(m, &registers, &what);
        assert_eq!(check(&m.stack, &m.rest, Value::NIL), Ok(()), "{what}");
        all_given_up(m);
    }
}

/// How many words a cell of `holds` values needs: a long one's, with its stub,
/// are one more.
fn words_of(holds: usize) -> usize {
    let size = holds + 1;
    if size > 254 { size + 1 } else { size.max(2) }
}

/// Check that each register holds what it did.
fn is_untouched(m: &Machine<'_, '_>, registers: &[Value], what: &str) {
    for (i, (got, want)) in m.stack.iter().zip(registers).enumerate().skip(2) {
        assert_eq!(got.bits(), want.bits(), "{what}: register {i}");
    }
}

/// A value whose count is at its end, or past it, is handed to a new cell like
/// any other, wherever it is among the operands. A share of one at its end
/// leaves it never counted, and a take leaves its count alone. With no room for
/// the cell, what was added is given back and nothing else.
#[test]
fn a_cell_is_given_a_value_whose_count_is_at_its_end() {
    on_a_machine(|m| {
        for n in (1..=9).chain([20, 253, 254, 300]) {
            let regs = &operand_registers(n)[0];
            for maker in makers(r(250), regs) {
                let code = maker.code();
                let places = regs
                    .iter()
                    .enumerate()
                    .filter(|(i, _)| n < 10 || *i == 0 || *i == n - 1 || *i == 239);
                for (_, at) in places {
                    let shares = maker.given.iter().filter(|a| a.reg == *at && !a.moved);
                    let shares = i32::try_from(shares.count()).expect("a few");
                    for (held, full) in [
                        (Some(i32::MAX), false),
                        (None, false),
                        (Some(i32::MAX - shares), true),
                        (None, true),
                    ] {
                        let what = format!("{:?} {at:?} {held:?} {full}", maker.ins);
                        let mut room = Heap::default();
                        room.tuple(&[Value::NIL]).expect("room");
                        m.rest.heap = if full {
                            Heap::limited(room.words_used())
                        } else {
                            Heap::new(code.ctors.clone())
                        };
                        let cell = m.rest.heap.tuple(&[Value::NIL]).expect("room");
                        let mut registers = vec![Value::NIL; WINDOW];
                        registers[usize::from(at.0)] = Value::cell(cell);
                        m.rest.heap.say_held(cell, held.unwrap_or(i32::MIN));
                        let stop = run_with(m, code, &registers);
                        let heap = &mut m.rest.heap;
                        let holders = if full {
                            assert_eq!(stop, Stop::HeapFull, "{what}");
                            assert_eq!(heap.held_by(cell), held, "{what}");
                            1
                        } else {
                            assert_eq!(stop, Stop::broken(Broken::NoArm), "{what}");
                            let after = held.filter(|_| shares == 0);
                            assert_eq!(heap.held_by(cell), after, "{what}");
                            // The register's own, which is the cell's if it was
                            // taken, and one for each share.
                            1 + shares
                        };
                        heap.say_held(cell, holders);
                        assert_eq!(check(&m.stack, &m.rest, Value::NIL), Ok(()), "{what}");
                        all_given_up(m);
                    }
                }
            }
        }
    });
}

/// A freed cell among the values that are shared stops the instruction that
/// was to make a cell of them, wherever it is among them. Nothing is made, and
/// nothing is written or taken.
#[test]
fn a_freed_value_among_the_operands_stops_the_instruction() {
    on_a_machine(|m| {
        for n in (1..=9).chain([20, 253, 300]) {
            let regs = &operand_registers(n)[0];
            for maker in makers(r(250), regs) {
                let code = maker.code();
                let shared = maker.given.iter().filter(|a| !a.moved).map(|a| a.reg);
                let mut dead: Vec<Reg> = shared.collect();
                if n > 9 {
                    let ends = [dead.first(), dead.last()];
                    dead = ends.into_iter().flatten().copied().collect();
                }
                for dead in dead {
                    let what = format!("{:?} of {dead:?}", maker.ins);
                    m.rest.heap = Heap::new(code.ctors.clone());
                    let mut registers = vec![Value::NIL; WINDOW];
                    for reg in regs {
                        registers[usize::from(reg.0)] = Value::int(5).expect("small");
                    }
                    let cell = a_freed_cell(&mut m.rest.heap, 1);
                    let before = (m.rest.heap.made(), m.rest.heap.live());
                    registers[usize::from(dead.0)] = Value::cell(cell);
                    let stop = run_with(m, code, &registers);
                    assert_eq!(stop, Stop::broken(Broken::DeadCell), "{what}");
                    let after = (m.rest.heap.made(), m.rest.heap.live());
                    assert_eq!(after, before, "{what}");
                    is_untouched(m, &registers, &what);
                }
            }
        }
    });
}

/// An instruction that takes a cell apart.
struct Opener {
    ins: Ins<Reg>,
    /// Where each field goes, when it goes anywhere.
    dsts: Vec<Option<Reg>>,
    code: &'static Code,
}

/// Where the tests of these instructions put the cell.
const OPENED: Reg = Reg(FIRST.0 + 250);

/// Every instruction that takes a cell in [`OPENED`] apart, each field to its
/// place in `dsts`. Each is in a function that then stops.
fn openers(dsts: &[Option<Reg>]) -> Vec<Opener> {
    let ctor = as_wide_as_you_like().1[dsts.len()];
    let src = Taken(OPENED);
    let fields: Vec<MaybeDst<Reg>> = dsts.iter().copied().map(MaybeDst::new).collect();
    let mut made = Vec::new();
    let mut add = |ins: Ins<Reg>| {
        let lists = Lists {
            regs: Vec::new(),
            args: Vec::new(),
            dsts: vec![fields.clone().into()],
        };
        let body = vec![ins, Ins::Bad { why: WhyId(0) }];
        let ctors = as_wide_as_you_like().0;
        made.push(Opener {
            ins,
            dsts: dsts.to_vec(),
            code: function_of(ReadsClosure::Nothing, 0, 254, body, lists, ctors),
        });
    };
    let dsts = crate::code::DstList(0);
    add(Ins::OpenWide { dsts, src, ctor });
    macro_rules! form {
        ($($name:ident)*) => {
            $(if let Ok(dsts) = fields.as_slice().try_into() {
                add(Ins::$name { dsts, src, ctor });
            })*
        };
    }
    form!(Open1 Open2 Open3 Open4 Open5 Open6 Open7 Open8);
    made
}

/// Where the `n` fields of a cell go, for a test: each to a register of its own,
/// as far as there are registers, with the cell's own register first, last or
/// not among them, all to one register, to two by turns, and with some of them
/// going nowhere.
fn field_registers(n: usize) -> Vec<Vec<Option<Reg>>> {
    let apart: Vec<_> = (0..n).map(|i| Some(r((i % 240) as u8))).collect();
    let with = |i: usize, dst: Option<Reg>| {
        let mut dsts = apart.clone();
        dsts[i] = dst;
        dsts
    };
    let only = |keep: &dyn Fn(usize) -> bool| -> Vec<_> {
        let each = apart.iter().enumerate();
        each.map(|(i, dst)| dst.filter(|_| keep(i))).collect()
    };
    let mut all = vec![
        with(0, Some(OPENED)),
        with(n - 1, Some(OPENED)),
        vec![Some(r(0)); n],
        vec![Some(OPENED); n],
        (0..n).map(|i| Some(r(u8::from(i % 2 == 1)))).collect(),
        with(0, None),
        with(n - 1, None),
        only(&|i| i % 2 == 0),
        only(&|i| i % 2 == 1),
        only(&|i| i == n / 2),
        only(&|_| false),
    ];
    let mut both = with(0, None);
    both[n - 1] = Some(OPENED);
    all.push(both);
    all.push(apart);
    all
}

/// Who else holds a cell that is taken apart.
#[derive(Debug, Clone, Copy, PartialEq)]
enum Else {
    Nothing,
    /// A global.
    Global,
    /// The register that the first field with one goes to, and the last.
    FirstDst,
    LastDst,
    /// So many that it is never counted again.
    Everyone,
    /// One fewer than that.
    Almost,
}

/// A frame with a constructor in [`OPENED`] whose fields are cells and numbers by
/// turns, every other cell held by a global too. Every register a field goes to
/// holds a cell or a number. `beside` is who else holds the constructor. Gives
/// the cell, its fields and what each register of the frame holds.
fn to_open(
    m: &mut Machine<'_, '_>,
    opener: &Opener,
    beside: Else,
) -> (Cell, Vec<Value>, Vec<Value>) {
    let n = opener.dsts.len();
    m.rest.heap = Heap::new(opener.code.ctors.clone());
    m.rest.globals = Vec::new();
    let heap = &mut m.rest.heap;
    let mut fields = Vec::new();
    for i in 0..n {
        fields.push(if i % 3 == 2 {
            Value::int(5000 + i as i64).expect("small")
        } else {
            let field = Value::cell(heap.make(Kind::Binary, &[i as u64; 12]).expect("room"));
            if i % 3 == 1 {
                m.rest.globals.push(heap.own(field).expect("a live cell"));
            }
            field
        });
    }
    let ctor = as_wide_as_you_like().1[n];
    let cell = heap.ctor(ctor, &fields).expect("room");
    let mut registers = vec![Value::NIL; WINDOW];
    for (i, dst) in opener.dsts.iter().enumerate() {
        if let Some(dst) = dst {
            let held = &mut registers[usize::from(dst.0)];
            if held.bits() == Value::NIL.bits() && *dst != OPENED {
                *held = if i % 2 == 0 {
                    Value::cell(heap.make(Kind::Binary, &[77; 12]).expect("room"))
                } else {
                    Value::int(9).expect("small")
                };
            }
        }
    }
    let held = Value::cell(cell);
    match beside {
        Else::Nothing => {}
        Else::Global => m.rest.globals.push(heap.own(held).expect("a live cell")),
        Else::FirstDst | Else::LastDst => {
            let mut dsts = opener.dsts.iter().flatten().filter(|dst| **dst != OPENED);
            let dst = match beside {
                Else::FirstDst => dsts.next(),
                _ => dsts.next_back(),
            };
            if let Some(dst) = dst {
                heap.share(cell).expect("a live cell");
                let old = std::mem::replace(&mut registers[usize::from(dst.0)], held);
                m.rest.release(old).expect("a live cell");
            }
        }
        Else::Everyone => heap.say_held(cell, i32::MIN),
        Else::Almost => heap.say_held(cell, i32::MAX),
    }
    registers[usize::from(OPENED.0)] = held;
    (cell, fields, registers)
}

/// What taking the cell apart is to leave in the registers: each field in its
/// own, where the last that goes to one stays, and nothing in the cell's.
fn opened(opener: &Opener, fields: &[Value], registers: &[Value]) -> Vec<u64> {
    let mut left: Vec<u64> = registers.iter().map(|v| v.bits()).collect();
    left[usize::from(OPENED.0)] = Value::NIL.bits();
    for (dst, v) in opener.dsts.iter().zip(fields) {
        if let Some(dst) = dst {
            left[usize::from(dst.0)] = v.bits();
        }
    }
    left
}

fn holds(m: &Machine<'_, '_>, want: &[u64], what: &str) {
    for (i, (got, want)) in m.stack.iter().zip(want).enumerate().skip(2) {
        assert_eq!(got.bits(), *want, "{what}: register {i}");
    }
}

/// Run `check` with every instruction that takes a cell apart, over fields that
/// go to every mix of registers.
fn each_opener(widths: impl Iterator<Item = usize>, mut check: impl FnMut(&Opener)) {
    for n in widths {
        for dsts in field_registers(n) {
            openers(&dsts).iter().for_each(&mut check);
        }
    }
}

/// Every instruction that takes a cell apart puts each field in its register and
/// empties the cell's, whichever registers those are. What a register held is
/// given up, and so is the cell's one reference. A cell that had no other is
/// freed, and a field that goes nowhere goes with it. Any other keeps its fields
/// and each register has a reference of its own. The space of one that was freed
/// is the next cell's of its size.
#[test]
fn every_instruction_that_takes_a_cell_apart_gives_each_field_its_register() {
    let mut ran: Vec<&str> = Vec::new();
    let alone = [Else::Nothing];
    let held = [
        Else::Global,
        Else::FirstDst,
        Else::LastDst,
        Else::Everyone,
        Else::Almost,
    ];
    on_a_machine(|m| {
        each_opener((1..=9).chain([20, 252, 253, 254, 300]), |opener| {
            ran.push(opener.ins.op().name());
            for (beside, freed) in alone
                .iter()
                .flat_map(|b| [(*b, false), (*b, true)])
                .chain(held.iter().map(|b| (*b, false)))
            {
                let what = format!("{:?} {:?} {beside:?} {freed}", opener.ins, opener.dsts);
                let (cell, fields, registers) = to_open(m, opener, beside);
                if freed {
                    a_freed_cell(&mut m.rest.heap, fields.len());
                }
                let before = m.rest.heap.held_by(cell);
                let made = m.rest.heap.made();
                let stop = run_with(m, opener.code, &registers);
                assert_eq!(stop, Stop::broken(Broken::NoArm), "{what}");
                assert_eq!(m.rest.heap.made(), made, "{what}");
                holds(m, &opened(opener, &fields, &registers), &what);
                // What a register held besides is given up as it is written.
                let in_dst = matches!(beside, Else::FirstDst | Else::LastDst);
                let gone = beside == Else::Nothing || in_dst;
                if gone {
                    assert_eq!(m.rest.heap.held_by(cell), Some(0), "{what}");
                } else {
                    let now = m.rest.heap.held_by(cell);
                    assert_eq!(now, before.map(|n| n - 1), "{what}");
                    let still: Vec<_> = m.rest.heap.values(cell).map(Value::bits).collect();
                    let want: Vec<_> = fields.iter().map(|v| v.bits()).collect();
                    assert_eq!(still, want, "{what}");
                    // It has the holders a test gives it, for the rest of it.
                    if beside != Else::Global {
                        m.rest.heap.say_held(cell, 1);
                        m.rest.globals.push(Value::cell(cell));
                    }
                }
                assert_eq!(check(&m.stack, &m.rest, Value::NIL), Ok(()), "{what}");
                if beside == Else::Nothing {
                    let next = m.rest.heap.tuple(&vec![Value::NIL; fields.len()]);
                    let next = next.expect("room");
                    assert_eq!(next.place(), cell.place(), "{what}");
                    m.rest.release(Value::cell(next)).expect("a live cell");
                }
                all_given_up(m);
            }
        });
    });
    for form in [
        "Open1", "Open2", "Open3", "Open4", "Open5", "Open6", "Open7", "Open8", "OpenWide",
    ] {
        assert!(ran.contains(&form), "{form}");
    }
}

/// Only the constructor that the instruction names is taken apart. Any other
/// value, and any other cell, whether something else holds it or not, stops it
/// where it stands, with every register and every count as they were. So does a
/// freed cell.
#[test]
fn only_the_constructor_named_is_taken_apart() {
    on_a_machine(|m| {
        for n in (1..=9).chain([20, 253, 254, 300]) {
            for opener in openers(&field_registers(n)[7]) {
                let (_, _, registers) = to_open(m, &opener, Else::Nothing);
                let at = usize::from(OPENED.0);
                m.rest.release(registers[at]).expect("a live cell");
                let heap = &mut m.rest.heap;
                let ctors = as_wide_as_you_like().1;
                let same = heap.tuple(&vec![Value::NIL; n]).expect("room");
                let other = heap.ctor(ctors[n + 1], &vec![Value::NIL; n]).expect("room");
                let fewer = heap.ctor(ctors[n], &vec![Value::NIL; n - 1]).expect("room");
                let more = heap.ctor(ctors[n], &vec![Value::NIL; n + 1]).expect("room");
                let long = heap.ctor(ctors[n], &[Value::NIL; 350]).expect("room");
                let mut cells = vec![same, other, fewer, more, long];
                cells.extend(a_cell_of_each_kind(heap).into_iter().map(|(cell, _)| cell));
                let plain = [
                    Value::int(3).expect("small"),
                    Value::float(0.5),
                    Value::NIL,
                    Value::bool(true),
                    Value::func(FuncIdx(0)),
                    Value::nullary(SOME_VARIANT),
                ];
                let wrong: Vec<Value> = cells.into_iter().map(Value::cell).collect();
                let wrong = wrong.iter().flat_map(|v| [(*v, 1), (*v, 2)]);
                for (v, holders) in wrong.chain(plain.map(|v| (v, 0))) {
                    let what = format!("{:?} of {v:?} held {holders}", opener.ins);
                    if let Some(cell) = v.as_cell() {
                        m.rest.heap.say_held(cell, holders);
                    }
                    let mut registers = registers.clone();
                    registers[at] = v;
                    let live = m.rest.heap.live();
                    let stop = run_with(m, opener.code, &registers);
                    let want = Stop::broken(Broken::OpenedOther(v.view()));
                    assert_eq!(stop, want, "{what}");
                    is_untouched(m, &registers, &what);
                    assert_eq!(m.rest.heap.live(), live, "{what}");
                    if let Some(cell) = v.as_cell() {
                        assert_eq!(m.rest.heap.held_by(cell), Some(holders), "{what}");
                    }
                }
                let ctor = as_wide_as_you_like().1[n];
                let dying = m.rest.heap.ctor(ctor, &vec![Value::NIL; n]).expect("room");
                let freed = m.rest.heap.ctor(ctor, &vec![Value::NIL; n]).expect("room");
                m.rest.release(Value::cell(freed)).expect("a live cell");
                m.rest.heap.say_held(dying, -7);
                for cell in [dying, freed] {
                    let what = format!("{:?} of {cell:?}", opener.ins);
                    let mut registers = registers.clone();
                    registers[at] = Value::cell(cell);
                    let live = m.rest.heap.live();
                    let stop = run_with(m, opener.code, &registers);
                    assert_eq!(stop, Stop::broken(Broken::DeadCell), "{what}");
                    is_untouched(m, &registers, &what);
                    assert_eq!(m.rest.heap.live(), live, "{what}");
                }
            }
        }
    });
}

/// A freed cell among the fields of a cell that something else holds stops the
/// instruction, when the field has a register to go to. The cell is in its
/// register still, with the reference it had. An instruction with its registers
/// inside it has changed nothing else either. One with them in a list has written
/// those before, and so has any instruction when the cell is never counted. A
/// field that goes nowhere is not looked at.
#[test]
fn a_freed_field_of_a_held_cell_stops_the_instruction() {
    on_a_machine(|m| {
        each_opener((1..=9).chain([20, 254]), |opener| {
            let n = opener.dsts.len();
            let places = (0..n).filter(|i| n < 10 || *i < 2 || *i > n - 3);
            for (at, beside) in places.flat_map(|at| [(at, Else::Global), (at, Else::Everyone)]) {
                let what = format!("{:?} {:?} {at} {beside:?}", opener.ins, opener.dsts);
                let (cell, mut fields, registers) = to_open(m, opener, beside);
                let holders = m.rest.heap.held_by(cell);
                let dead = a_freed_cell(&mut m.rest.heap, 1);
                let slot = &mut m.rest.heap.data_mut(cell)[at];
                *slot = Value::cell(dead).bits();
                let old = std::mem::replace(&mut fields[at], Value::cell(dead));
                m.rest.release(old).expect("a live cell");
                let stop = run_with(m, opener.code, &registers);
                let left = if opener.dsts[at].is_some() {
                    assert_eq!(stop, Stop::broken(Broken::DeadCell), "{what}");
                    assert_eq!(m.rest.heap.held_by(cell), holders, "{what}");
                    let in_a_list = matches!(opener.ins, Ins::OpenWide { .. });
                    let written = if in_a_list || holders.is_none() {
                        at
                    } else {
                        0
                    };
                    let before = Opener {
                        dsts: opener.dsts[..written].to_vec(),
                        ..*opener
                    };
                    let mut left = opened(&before, &fields, &registers);
                    left[usize::from(OPENED.0)] = Value::cell(cell).bits();
                    left
                } else {
                    assert_eq!(stop, Stop::broken(Broken::NoArm), "{what}");
                    let now = m.rest.heap.held_by(cell);
                    assert_eq!(now, holders.map(|n| n - 1), "{what}");
                    opened(opener, &fields, &registers)
                };
                holds(m, &left, &what);
                m.rest.heap.data_mut(cell)[at] = Value::NIL.bits();
                if holders.is_none() {
                    counted_again(m, cell);
                }
                assert_eq!(check(&m.stack, &m.rest, Value::NIL), Ok(()), "{what}");
                all_given_up(m);
            }
        });
    });
}

/// Give `cell`, which was never counted, as many holders as hold it: a global,
/// and [`OPENED`] when it is still there.
fn counted_again(m: &mut Machine<'_, '_>, cell: Cell) {
    m.rest.globals.push(Value::cell(cell));
    let still = m.stack[usize::from(OPENED.0)].as_cell() == Some(cell);
    m.rest.heap.say_held(cell, 1 + i32::from(still));
}

/// A freed cell that the instruction only moves, as a field of a cell that nothing
/// else holds, is not looked at. It is in its register, and the next that takes a
/// reference to it stops.
#[test]
fn a_freed_field_that_only_moves_is_not_looked_at() {
    on_a_machine(|m| {
        each_opener((1..=9).chain([20, 254, 300]), |opener| {
            let n = opener.dsts.len();
            let Some(at) = opener.dsts.iter().rposition(Option::is_some) else {
                return;
            };
            let what = format!("{:?} {:?} {at}", opener.ins, opener.dsts);
            let (cell, mut fields, registers) = to_open(m, opener, Else::Nothing);
            let dead = a_freed_cell(&mut m.rest.heap, 1);
            m.rest.heap.data_mut(cell)[at] = Value::cell(dead).bits();
            let old = std::mem::replace(&mut fields[at], Value::cell(dead));
            m.rest.release(old).expect("a live cell");
            let stop = run_with(m, opener.code, &registers);
            assert_eq!(stop, Stop::broken(Broken::NoArm), "{what}");
            assert_eq!(m.rest.heap.held_by(cell), Some(0), "{what}");
            holds(m, &opened(opener, &fields, &registers), &what);
            let dst = opener.dsts[at].expect("it has a register");
            assert_eq!(m.rest.heap.share(dead), Err(DeadCell), "{what} {n}");
            m.stack[usize::from(dst.0)] = Value::NIL;
            all_given_up(m);
        });
    });
}

/// A freed cell that the instruction has only to give up, as what a register held
/// or as a field that goes nowhere, stops it once everything is in its place.
#[test]
fn a_freed_cell_that_is_given_up_stops_the_instruction_at_its_end() {
    let stop_at = Stop::broken(Broken::DeadCell);
    on_a_machine(|m| {
        each_opener((1..=9).chain([20, 254]), |opener| {
            let n = opener.dsts.len();
            for at in (0..n).filter(|i| n < 10 || *i < 2 || *i > n - 3) {
                for beside in [Else::Nothing, Else::Global] {
                    let what = format!("{:?} {:?} {at} {beside:?}", opener.ins, opener.dsts);
                    let (cell, mut fields, mut registers) = to_open(m, opener, beside);
                    let dead = Value::cell(a_freed_cell(&mut m.rest.heap, 1));
                    match opener.dsts[at] {
                        // The first field to go to it finds it there.
                        Some(dst) if dst != OPENED => {
                            let held = &mut registers[usize::from(dst.0)];
                            let old = std::mem::replace(held, dead);
                            m.rest.release(old).expect("a live cell");
                        }
                        None if beside == Else::Nothing => {
                            m.rest.heap.data_mut(cell)[at] = dead.bits();
                            let old = std::mem::replace(&mut fields[at], dead);
                            m.rest.release(old).expect("a live cell");
                        }
                        Some(_) | None => {
                            all_given_up_of(m, &registers);
                            continue;
                        }
                    }
                    let stop = run_with(m, opener.code, &registers);
                    assert_eq!(stop, stop_at, "{what}");
                    holds(m, &opened(opener, &fields, &registers), &what);
                    assert_eq!(check(&m.stack, &m.rest, Value::NIL), Ok(()), "{what}");
                    all_given_up(m);
                }
            }
        });
    });
}

/// [`all_given_up`], of what `registers` hold, when nothing ran.
fn all_given_up_of(m: &mut Machine<'_, '_>, registers: &[Value]) {
    started(m, Link::new(0, Room(0), Dst(RESULT)));
    let held = usize::from(FIRST.0)..WINDOW;
    m.stack[held.clone()].copy_from_slice(&registers[held]);
    all_given_up(m);
}

/// Something the host holds is let go of when the cell it is in is taken apart,
/// when its field goes nowhere and nothing else holds the cell. Otherwise it is
/// not.
#[test]
fn a_handle_in_a_field_that_goes_nowhere_is_let_go_of() {
    on_a_machine(|m| {
        for (n, at) in [
            (1, 0),
            (2, 0),
            (2, 1),
            (5, 3),
            (9, 8),
            (254, 100),
            (300, 299),
        ] {
            for (beside, wanted, want) in [
                (Else::Nothing, false, vec![41]),
                (Else::Nothing, true, vec![]),
                (Else::Global, false, vec![]),
            ] {
                let mut dsts: Vec<_> = (0..n).map(|i| Some(r((i % 240) as u8))).collect();
                if !wanted {
                    dsts[at] = None;
                }
                for opener in openers(&dsts) {
                    let what = format!("{:?} {at} {beside:?} {wanted}", opener.ins);
                    let (cell, _, registers) = to_open(m, &opener, beside);
                    let handle = m.rest.heap.make(Kind::Handle, &[41]).expect("room");
                    let slot = &mut m.rest.heap.data_mut(cell)[at];
                    let old = std::mem::replace(slot, Value::cell(handle).bits());
                    let old = Value::from_bits(old);
                    m.rest.release(old).expect("a live cell");
                    let stop = run_with(m, opener.code, &registers);
                    assert_eq!(stop, Stop::broken(Broken::NoArm), "{what}");
                    assert_eq!(m.rest.heap.take_released(), want, "{what}");
                    all_given_up(m);
                }
            }
        }
    });
}
