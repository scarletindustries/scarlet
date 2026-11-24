use super::*;
use crate::OsHost;

/// `passing` sits where a 16-byte load reaches it in one instruction,
/// and the heap comes after it, so the heap's size cannot move it.
#[test]
fn the_heap_is_behind_the_fields_a_call_moves() {
    let passing = std::mem::offset_of!(Machine<'_, '_>, passing);
    assert!(passing <= 255 || passing.is_multiple_of(16), "{passing}");
    assert!(passing < std::mem::offset_of!(Machine<'_, '_>, heap));
    assert!(std::mem::offset_of!(Machine<'_, '_>, regs) < passing);
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
    let code = crate::code::load(&program).expect("the program loads");
    let mut host = OsHost::new(argv, env, Vec::new());
    let mut m = Machine::new(&code, &mut host);
    let ended = m.run().expect("the program runs");
    m.let_go(ended).expect("every count is right");
    assert!(
        m.top == 0 && m.regs.iter().all(|v| v.bits() == Value::NIL.bits()),
        "a frame's registers outlived it"
    );
    let left = m.heap.live();
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
    let code = crate::code::load(&program).expect("the program loads");
    let mut host = OsHost::new(Vec::new(), Vec::new(), Vec::new());
    let mut m = Machine::new(&code, &mut host);
    let ended = m.run().expect("the program runs");
    check(&mut m, ended);
}

/// A program whose one global holds a cell when it ends.
const HOLDS_A_CELL: &str = "const kept = ['a', 'b']\npub fn main() {\n\tprintln(kept)\n}\n";

fn held_by_a_global(m: &Machine<'_, '_>) -> Cell {
    m.globals
        .iter()
        .find_map(|v| v.as_cell())
        .expect("a global holding a cell")
}

#[test]
fn an_audit_of_a_run_with_every_count_right_finds_nothing() {
    after_running(HOLDS_A_CELL, |m, ended| {
        assert!(m.heap.live() > 0, "the global still holds its array");
        assert_eq!(m.finish(ended, AtExit::Audit), Ok(()));
        assert_eq!(m.heap.live(), 0);
    });
}

/// A run that is not audited ends well whatever its heap holds, and
/// lets go of nothing.
#[test]
fn a_run_that_is_not_audited_leaves_its_heap_alone() {
    after_running(HOLDS_A_CELL, |m, ended| {
        let cell = held_by_a_global(m);
        m.heap.share(cell);
        let live = m.heap.live();
        assert_eq!(m.finish(ended, AtExit::Leave), Ok(()));
        assert_eq!(m.heap.live(), live);
    });
}

/// A count one too high means a cell nothing will ever free. The audit
/// says how many cells that left behind: the array, its leaf and the two
/// strings in it.
#[test]
fn an_audit_counts_the_cells_a_lost_release_left() {
    after_running(HOLDS_A_CELL, |m, ended| {
        let cell = held_by_a_global(m);
        m.heap.share(cell);
        assert_eq!(
            m.finish(ended, AtExit::Audit),
            Err(Stop::broken(Broken::CellsLeft { cells: 4 }))
        );
    });
}

/// A count one too low means a cell freed while something still holds
/// it. The audit stops at the holder that gives it up next.
#[test]
fn an_audit_stops_at_a_cell_released_once_too_often() {
    after_running(HOLDS_A_CELL, |m, ended| {
        let cell = held_by_a_global(m);
        m.heap.release(cell).expect("the global's reference");
        assert_eq!(
            m.finish(ended, AtExit::Audit),
            Err(Stop::broken(Broken::DeadCell))
        );
    });
}

/// A register that still names a cell after the cell was freed is found
/// when the register is written over, and when its frame ends.
#[test]
fn a_register_holding_a_freed_cell_stops_the_instruction_that_gives_it_up() {
    after_running(HOLDS_A_CELL, |m, _| {
        let cell = held_by_a_global(m);
        m.heap.release(cell).expect("the global's reference");
        let stale = Value::cell(cell);
        m.open(1);
        m.regs[0] = stale;
        assert_eq!(m.set(0, Reg(0), Value::NIL), Err(DeadCell));
        m.regs[0] = stale;
        assert_eq!(m.leave(0), Err(DeadCell));
        assert_eq!(Stop::from(DeadCell), Stop::broken(Broken::DeadCell));
    });
}

/// Run `instrs` as a function of `regs` registers that takes nothing,
/// and hand the machine and what the call gave to `check`.
fn after_instrs(
    regs: u32,
    instrs: Vec<Instr>,
    check: impl FnOnce(&mut Machine<'_, '_>, Result<Value, Stop>),
) {
    after_instrs_given(regs, instrs, |_| None, check);
}

/// [`after_instrs`], for a function that takes in `r0` what `given`
/// makes in the machine's heap, when it makes something.
fn after_instrs_given(
    regs: u32,
    instrs: Vec<Instr>,
    given: impl FnOnce(&mut Heap) -> Option<Value>,
    check: impl FnOnce(&mut Machine<'_, '_>, Result<Value, Stop>),
) {
    let result = scarlet_core::compile::compile_text("pub fn main() {\n\tNil\n}\n", None);
    assert!(result.success(), "{:?}", result.diagnostics());
    let program = result.into_runnable().expect("a clean compile is runnable");
    let code = crate::code::load(&program).expect("the program loads");
    let mut host = OsHost::new(Vec::new(), Vec::new(), Vec::new());
    let mut m = Machine::new(&code, &mut host);
    let args: Vec<Value> = given(&mut m.heap).into_iter().collect();
    let body = Body {
        name: "test.body".to_string(),
        regs,
        params: args.iter().map(|_| Reg(0)).collect(),
        instrs: instrs.into_boxed_slice(),
        carried: Box::new([]),
    };
    let got = m.call(&body, &args);
    check(&mut m, got);
}

/// `r1` gets a new array of one element.
fn an_array_in_r1() -> [Instr; 2] {
    [
        Instr::Const {
            dst: Reg(0),
            value: Value::NIL,
        },
        Instr::Array {
            dst: Reg(1),
            elements: Box::new([Reg(0)]),
        },
    ]
}

fn mv(dst: u32, src: u32, moved: bool) -> Instr {
    Instr::Move {
        dst: Reg(dst),
        src: Reg(src),
        moved,
    }
}

/// The cell a finished call returned, after checking that the frame left
/// nothing behind and that giving the result up empties the heap.
fn the_only_cell_left(m: &mut Machine<'_, '_>, got: Result<Value, Stop>) {
    let v = got.expect("the function returns");
    let cell = v.as_cell().expect("an array");
    assert_eq!(m.heap.unique(cell), Ok(true));
    assert!(m.regs.iter().all(|v| v.bits() == Value::NIL.bits()));
    assert_eq!(m.release(v), Ok(()));
    assert_eq!(m.heap.live(), 0);
}

/// A `Move` that takes its source hands over the register's reference,
/// so a push after it finds one holder and writes in place. One that
/// shares leaves two holders, and the push copies.
#[test]
fn a_move_that_takes_leaves_one_holder_and_one_that_shares_leaves_two() {
    let made = |moved: bool| {
        let mut instrs = an_array_in_r1().to_vec();
        instrs.extend([
            mv(2, 1, moved),
            Instr::ArrayAppend {
                dst: Reg(3),
                rest: Reg(2),
                back: Box::new([Reg(0)]),
                moved: true,
            },
            Instr::Ret { src: Reg(3) },
        ]);
        let mut made = 0;
        after_instrs(4, instrs, |m, got| {
            made = m.heap.made();
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
    dropped.extend([
        mv(2, 1, true),
        Instr::Drop { reg: Reg(1) },
        Instr::Ret { src: Reg(2) },
    ]);
    after_instrs(3, dropped, the_only_cell_left);

    let mut read = an_array_in_r1().to_vec();
    read.extend([
        mv(2, 1, true),
        Instr::ArrayLen {
            dst: Reg(0),
            src: Reg(1),
        },
        Instr::Ret { src: Reg(0) },
    ]);
    after_instrs(3, read, |_, got| {
        let stopped = got.map(|v| v.bits());
        assert_eq!(stopped, Err(Stop::broken(Broken::NotAnArray(View::Nil))));
    });
}

/// The same value shared, then dropped under its old name, ends with the
/// same one holder, by a share and a release.
#[test]
fn a_move_that_shares_then_a_drop_leaves_one_holder() {
    let mut instrs = an_array_in_r1().to_vec();
    instrs.extend([
        mv(2, 1, false),
        Instr::Drop { reg: Reg(1) },
        Instr::Ret { src: Reg(2) },
    ]);
    after_instrs(3, instrs, the_only_cell_left);
}

/// A value that is not a cell has no reference to hand over, so a `Move`
/// of one copies it whichever way it is marked.
#[test]
fn a_move_of_a_value_that_is_not_a_cell_copies_it() {
    for moved in [true, false] {
        let instrs = vec![
            Instr::Const {
                dst: Reg(0),
                value: Value::bool(true),
            },
            mv(1, 0, moved),
            Instr::Ret { src: Reg(1) },
        ];
        after_instrs(2, instrs, |m, got| {
            let bits = got.map(|v| v.bits());
            assert_eq!(bits, Ok(Value::bool(true).bits()));
            assert_eq!(m.heap.made(), 0);
        });
    }
}

/// Two values that are never live at once may share a register, so a
/// `Move` can name one register twice. Either way the value stays.
#[test]
fn a_move_onto_its_own_register_keeps_the_value() {
    for moved in [true, false] {
        let mut instrs = an_array_in_r1().to_vec();
        instrs.extend([mv(1, 1, moved), Instr::Ret { src: Reg(1) }]);
        after_instrs(2, instrs, the_only_cell_left);
    }
}

/// A function that writes the string `new` into the one-element array
/// in `r1` at `index`. It returns the array the write made, from `r2`.
/// When the write misses it returns `after_miss` instead, and `r2` then
/// still holds the 7 it started with.
fn a_write(index: i64, moved: bool, value_moved: bool, after_miss: u32) -> Vec<Instr> {
    let int = |dst: u32, n: i64| Instr::Const {
        dst: Reg(dst),
        value: Value::int(n).expect("small"),
    };
    let mut instrs = an_array_in_r1().to_vec();
    instrs.extend([
        int(2, 7),
        int(3, index),
        Instr::Str {
            dst: Reg(4),
            text: Box::from(*b"new"),
        },
        Instr::ArraySet {
            dst: Reg(2),
            src: Reg(1),
            index: Reg(3),
            value: Arg {
                reg: Reg(4),
                moved: value_moved,
            },
            moved,
            miss: 7,
        },
        Instr::Ret { src: Reg(2) },
        Instr::Ret {
            src: Reg(after_miss),
        },
    ]);
    instrs
}

/// Element 0 of the array `v`, as text when it is a string.
fn first_text(m: &Machine<'_, '_>, v: Value) -> Option<String> {
    let first = array::get(&m.heap, v.as_cell()?, 0)?.as_cell()?;
    let mut out = Vec::new();
    m.heap.read_string(first, &mut out);
    String::from_utf8(out).ok()
}

/// A write that takes the register of an array nothing else holds
/// changes it in place: the only cells made are the array's two and the
/// string, whether the value is taken or shared.
#[test]
fn a_write_that_takes_an_array_with_one_holder_makes_no_cell() {
    for value_moved in [true, false] {
        after_instrs(5, a_write(0, true, value_moved, 0), |m, got| {
            assert_eq!(m.heap.made(), 3);
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
    after_instrs(5, a_write(0, false, true, 0), |m, got| {
        assert_eq!(m.heap.made(), 5);
        let v = *got.as_ref().expect("the function returns");
        assert_eq!(first_text(m, v).as_deref(), Some("new"));
        the_only_cell_left(m, got);
    });
    // The same write, with the old array kept to look at afterwards.
    let mut kept = a_write(0, false, true, 0);
    kept[6] = Instr::Ret { src: Reg(1) };
    after_instrs(5, kept, |m, got| {
        let v = *got.as_ref().expect("the function returns");
        let first = v.as_cell().and_then(|a| array::get(&m.heap, a, 0));
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
            after_instrs(5, a_write(index, moved, moved, 1), |m, got| {
                assert_eq!(m.heap.made(), 3, "index {index}");
                let v = *got.as_ref().expect("the function returns");
                let first = v.as_cell().and_then(|a| array::get(&m.heap, a, 0));
                assert_eq!(first.map(|x| x.bits()), Some(Value::NIL.bits()));
                the_only_cell_left(m, got);
            });
            after_instrs(5, a_write(index, moved, moved, 4), |m, got| {
                let v = *got.as_ref().expect("the function returns");
                assert!(v.as_cell().is_some_and(|c| m.heap.string_is(c, b"new")));
                the_only_cell_left(m, got);
            });
            after_instrs(5, a_write(index, moved, moved, 2), |m, got| {
                let bits = got.map(|v| v.bits());
                assert_eq!(bits, Ok(Value::int(7).expect("small").bits()));
                assert_eq!(m.heap.live(), 0);
            });
        }
    }
}

/// The value may sit in the array's own register. The array is read
/// before the value is taken, so the write shares the array, finds two
/// holders, and puts the old array in a copy.
#[test]
fn a_write_of_the_array_into_itself_holds_the_old_array() {
    let mut instrs = an_array_in_r1().to_vec();
    instrs.extend([
        Instr::Const {
            dst: Reg(3),
            value: Value::int(0).expect("small"),
        },
        Instr::ArraySet {
            dst: Reg(2),
            src: Reg(1),
            index: Reg(3),
            value: Arg {
                reg: Reg(1),
                moved: true,
            },
            moved: false,
            miss: 5,
        },
        Instr::Ret { src: Reg(2) },
        Instr::Bad { why: Broken::NoArm },
    ]);
    after_instrs(4, instrs, |m, got| {
        let v = *got.as_ref().expect("the function returns");
        let outer = v.as_cell().expect("an array");
        let inner = array::get(&m.heap, outer, 0).and_then(|x| x.as_cell());
        let inner = inner.expect("the old array, as the element");
        assert_ne!(inner, outer);
        assert_eq!(m.heap.unique(inner), Ok(true));
        let first = array::get(&m.heap, inner, 0).map(|x| x.bits());
        assert_eq!(first, Some(Value::NIL.bits()));
        the_only_cell_left(m, got);
    });
}

/// A function that writes -1 into the range `2..6` in `r1` at `index`,
/// and returns what is in `ret`: `r2` for the array the write made, or
/// `r1` for the range. A miss returns the range.
fn a_write_to_a_range(index: i64, moved: bool, ret: u32) -> Vec<Instr> {
    let int = |dst: u32, n: i64| Instr::Const {
        dst: Reg(dst),
        value: Value::int(n).expect("small"),
    };
    vec![
        int(0, 2),
        int(4, 6),
        Instr::Range {
            dst: Reg(1),
            start: Reg(0),
            end: Reg(4),
        },
        int(3, index),
        int(4, -1),
        Instr::ArraySet {
            dst: Reg(2),
            src: Reg(1),
            index: Reg(3),
            value: Arg {
                reg: Reg(4),
                moved: false,
            },
            moved,
            miss: 7,
        },
        Instr::Ret { src: Reg(ret) },
        Instr::Ret { src: Reg(1) },
    ]
}

/// The Ints in the array `v`, or none when it is not a tree.
fn tree_ints(m: &Machine<'_, '_>, v: Value) -> Option<Vec<i64>> {
    let Ok(Seq::Tree(a)) = m.seq(v) else {
        return None;
    };
    let int = |x: Value| match x.view() {
        View::Int(n) => Some(n),
        _ => None,
    };
    array::elements(&m.heap, a).into_iter().map(int).collect()
}

/// The two ends of the range `v`, or none when it is not a range.
fn range_ends(m: &Machine<'_, '_>, v: Value) -> Option<(i64, i64)> {
    match m.seq(v) {
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
        after_instrs(5, a_write_to_a_range(1, moved, 2), |m, got| {
            assert_eq!(m.heap.made(), 3, "moved {moved}");
            let v = *got.as_ref().expect("the function returns");
            assert_eq!(tree_ints(m, v), Some(vec![2, -1, 4, 5]));
            the_only_cell_left(m, got);
        });
        for index in [4, -1] {
            after_instrs(5, a_write_to_a_range(index, moved, 2), |m, got| {
                assert_eq!(m.heap.made(), 1, "index {index}");
                let v = *got.as_ref().expect("the function returns");
                assert_eq!(range_ends(m, v), Some((2, 6)));
                the_only_cell_left(m, got);
            });
        }
    }
    after_instrs(5, a_write_to_a_range(1, false, 1), |m, got| {
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
    not_an_array[1] = Instr::Const {
        dst: Reg(1),
        value: Value::bool(true),
    };
    after_instrs(5, not_an_array, |_, got| {
        let stopped = got.map(|v| v.bits());
        let want = Broken::NotAnArray(View::Bool(true));
        assert_eq!(stopped, Err(Stop::broken(want)));
    });
    let mut not_an_int = a_write(0, true, true, 0);
    not_an_int[3] = Instr::Const {
        dst: Reg(3),
        value: Value::bool(false),
    };
    after_instrs(5, not_an_int, |_, got| {
        let stopped = got.map(|v| v.bits());
        let want = Broken::Position(View::Bool(false));
        assert_eq!(stopped, Err(Stop::broken(want)));
    });
}

/// A write through a node that was freed while the array still pointed
/// at it stops the run as a wrong count, at the write.
#[test]
fn a_write_through_a_freed_node_stops_the_run_as_a_dead_cell() {
    let instrs = vec![
        Instr::Const {
            dst: Reg(1),
            value: Value::int(0).expect("small"),
        },
        Instr::ArraySet {
            dst: Reg(2),
            src: Reg(0),
            index: Reg(1),
            value: Arg {
                reg: Reg(1),
                moved: false,
            },
            moved: true,
            miss: 3,
        },
        Instr::Ret { src: Reg(2) },
        Instr::Ret { src: Reg(0) },
    ];
    let with_a_freed_leaf = |heap: &mut Heap| {
        let array = array::from_values(heap, &[Value::NIL, Value::NIL]).expect("room");
        // The tail leaf is the last of the root's words.
        let tail = heap.data(array).last().copied().map(Value::from_bits);
        let leaf = tail.and_then(Value::as_cell).expect("a tail leaf");
        heap.release(leaf).expect("a live cell");
        Some(Value::cell(array))
    };
    after_instrs_given(3, instrs, with_a_freed_leaf, |_, got| {
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
        m.heap.release(cell).expect("the global's reference");
        let pushed = array::push_owned(&mut m.heap, cell, Value::NIL, End::Back);
        assert_eq!(pushed.map_err(fault), Err(Stop::broken(Broken::DeadCell)));
        assert_eq!(fault(Fault::Full), Stop::HeapFull);
    });
}
