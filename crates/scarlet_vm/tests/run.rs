//! Programs run end to end: compiled from Scarlet source, then run by the VM,
//! checking exactly what they print.

use scarlet_vm::Stop;

fn compile(src: &str) -> scarlet_core::core_ir::Program {
    let mut scanner = scarlet_core::scanner::new_scanner(src.to_string());
    let parsed = scarlet_core::parser::new_parser(&mut scanner).parse_program();
    let expr = scarlet_core::ast::Expression::BlockExpression(parsed.ast);
    let result = scarlet_core::bytecode::compile(&expr, None);
    assert!(result.success(), "{:?}", result.diagnostics);
    result.into_runnable().expect("a clean compile is runnable")
}

fn run(src: &str) -> Result<String, Stop> {
    let mut out = Vec::new();
    scarlet_vm::run(&compile(src), &mut out)?;
    Ok(String::from_utf8(out).expect("println writes UTF-8"))
}

fn prints(src: &str, want: &str) {
    assert_eq!(run(src).as_deref(), Ok(want), "{src}");
}

#[test]
fn one_plus_two() {
    prints("pub fn main() {\n\tprintln(1 + 2)\n}\n", "3\n");
}

#[test]
fn a_call_passes_its_arguments_and_returns_its_result() {
    prints(
        "fn add(a Int, b Int) Int { a + b }\n\
         fn twice(x Int) Int { add(x, x) }\n\
         pub fn main() {\n\
         \tprintln(twice(21))\n\
         \tprintln(add(1, add(2, 3)))\n\
         }\n",
        "42\n6\n",
    );
}

/// `docs/semantics.md`'s rules for `/` and `%`.
#[test]
fn division_follows_the_rules() {
    prints(
        "pub fn main() {\n\
         \tprintln(-7 / 2)\n\
         \tprintln(7 / 0)\n\
         \tprintln(-7 % 2)\n\
         \tprintln(7 % 0)\n\
         }\n",
        "-3\n0\n-1\n7\n",
    );
}

#[test]
fn comparisons_give_bools() {
    prints(
        "pub fn main() {\n\
         \tprintln(3 < 4)\n\
         \tprintln(3 == 4)\n\
         \tprintln(4 >= 4)\n\
         }\n",
        "True\nFalse\nTrue\n",
    );
}

#[test]
fn nil_prints_as_nil() {
    prints("pub fn main() {\n\tprintln(println(1))\n}\n", "1\nNil\n");
}

#[test]
fn if_picks_a_branch_in_both_positions() {
    prints(
        "fn max(a Int, b Int) Int {\n\
         \tif a > b { a } else { b }\n\
         }\n\
         pub fn main() {\n\
         \tx = if 1 < 2 { 10 } else { 20 }\n\
         \tprintln(x + max(3, 4))\n\
         \tprintln(if max(1, 2) == 2 { 7 } else { 8 })\n\
         }\n",
        "14\n7\n",
    );
}

/// A loop written as tail recursion: each step's call is a tail call, which
/// reuses the frame, so a million steps finish in the room of one.
#[test]
fn a_tail_recursive_loop_of_a_million_steps_finishes() {
    prints(
        "fn count(n Int, acc Int) Int {\n\
         \tif n == 0 { acc } else { count(n - 1, acc + 1) }\n\
         }\n\
         pub fn main() {\n\
         \tprintln(count(1000000, 0))\n\
         }\n",
        "1000000\n",
    );
}

/// Frames live on the VM's own stack, not Rust's, so a deep recursion that is
/// not a tail call just uses memory: nothing overflows.
#[test]
fn a_deep_recursion_uses_memory_not_the_rust_stack() {
    prints(
        "fn depth(n Int) Int {\n\
         \tif n == 0 { 0 } else { 1 + depth(n - 1) }\n\
         }\n\
         pub fn main() {\n\
         \tprintln(depth(200000))\n\
         }\n",
        "200000\n",
    );
}

/// A function that uses something not built yet still loads; only calling
/// it stops the run, and the stop says what it needs.
#[test]
fn only_calling_an_unbuilt_function_stops_the_run() {
    let src = "fn bytes() Binary { <<1>> }\n\
               pub fn main() {\n\
               \tprintln(1)\n\
               }\n";
    prints(src, "1\n");
    let calls = "fn bytes() Binary { <<1>> }\n\
                 pub fn main() {\n\
                 \t_ = bytes()\n\
                 }\n";
    assert_eq!(
        run(calls),
        Err(Stop::NotBuiltYet("the operation BinaryFromInt".into()))
    );
}

#[test]
fn strings_print_join_and_interpolate() {
    prints(
        "fn greet(name String) String { 'hello, ' + name }\n\
         pub fn main() {\n\
         \tprintln('hello, world')\n\
         \tprintln(greet('Scarlet'))\n\
         \tprintln('2 + 2 = ${2 + 2}, and 3 < 4 is ${3 < 4}')\n\
         }\n",
        "hello, world\nhello, Scarlet\n2 + 2 = 4, and 3 < 4 is True\n",
    );
}

/// An Int is exact however big it gets: past 48 bits it moves to the heap,
/// and past 64 it keeps going.
#[test]
fn ints_are_exact_past_48_and_64_bits() {
    prints(
        "fn fact(n Int) Int { if n == 0 { 1 } else { n * fact(n - 1) } }\n\
         pub fn main() {\n\
         \tprintln(140737488355327 + 1)\n\
         \tprintln(9223372036854775807 + 1)\n\
         \tprintln(fact(25))\n\
         \tprintln(0 - 9223372036854775807 - 1)\n\
         }\n",
        "140737488355328\n9223372036854775808\n15511210043330985984000000\n-9223372036854775808\n",
    );
}

/// A result that fits comes back small, so a number has one form and `==`
/// cannot see two.
#[test]
fn a_big_result_that_fits_is_small_again() {
    prints(
        "pub fn main() {\n\
         \tbig = 9223372036854775807 * 4\n\
         \tprintln(big / 4 == 9223372036854775807)\n\
         \tprintln(big - big == 0)\n\
         \tprintln({big - big} + 1)\n\
         \tprintln(big > 1)\n\
         \tprintln(0 - big < 0)\n\
         }\n",
        "True\nTrue\n1\nTrue\nTrue\n",
    );
}

/// `docs/semantics.md`'s rules hold for big ints too, including the zero
/// cases the library would otherwise panic on. The number is built by
/// multiplying: a literal past 64 bits is still a compile error, because the
/// compiler keeps Int constants as `i64`.
#[test]
fn big_division_follows_the_rules() {
    prints(
        "pub fn main() {\n\
         \tbig = 1000000000000000 * 1000000000000000\n\
         \tprintln({0 - big} / 7)\n\
         \tprintln({0 - big} % 7)\n\
         \tprintln(big / 0)\n\
         \tprintln(big % 0 == big)\n\
         }\n",
        "-142857142857142857142857142857\n-1\n0\nTrue\n",
    );
}

/// Output that goes nowhere, like a pipe into `head` that has closed.
struct Closed;

impl std::io::Write for Closed {
    fn write(&mut self, _: &[u8]) -> std::io::Result<usize> {
        Err(std::io::ErrorKind::BrokenPipe.into())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

#[test]
fn a_closed_output_stops_the_run_quietly() {
    let program = compile("pub fn main() {\n\tprintln(1)\n}\n");
    assert_eq!(
        scarlet_vm::run(&program, &mut Closed),
        Err(Stop::OutputClosed)
    );
}

#[test]
fn a_constructor_prints_as_it_is_written() {
    prints(
        "type Shape {\n\
         \tCircle(radius Int)\n\
         \tRect(w Int, h Int)\n\
         \tDot\n\
         }\n\
         pub fn main() {\n\
         \tprintln(Circle(radius: 2))\n\
         \tprintln(Rect(w: 3, h: 4))\n\
         \tprintln(Dot)\n\
         \tprintln(Some(1))\n\
         \tprintln(Some(None))\n\
         \tprintln(Err(Nil))\n\
         \tprintln(Ok('fine'))\n\
         \tprintln('${Some(True)} and ${None}')\n\
         }\n",
        "Circle(2)\nRect(3, 4)\nDot\nSome(1)\nSome(None)\nErr(Nil)\nOk(fine)\nSome(True) and None\n",
    );
}

/// A constructor named after its own type is a record, and shows its labels.
#[test]
fn a_record_shows_its_labels() {
    prints(
        "type Point {\n\
         \tPoint(x Int, y Int)\n\
         }\n\
         pub fn main() {\n\
         \tprintln(Point(x: 1, y: -2))\n\
         }\n",
        "Point{ x: 1, y: -2 }\n",
    );
}

/// A constructor holding anything but small values takes a line per field,
/// and what it holds is laid out the same way, one level in.
#[test]
fn a_nested_constructor_takes_a_line_per_field() {
    prints(
        "type Point {\n\
         \tPoint(x Int, y Int)\n\
         }\n\
         type Seg {\n\
         \tSeg(a Point, b Point)\n\
         }\n\
         pub fn main() {\n\
         \tprintln(Seg(a: Point(x: 1, y: 2), b: Point(x: 3, y: 4)))\n\
         \tprintln(Some(Some(1)))\n\
         \tprintln(Ok('a string of twenty or more'))\n\
         }\n",
        "Seg {\n  a: Point{ x: 1, y: 2 },\n  b: Point{ x: 3, y: 4 }\n}\n\
         Some(\n  Some(1)\n)\n\
         Ok(\n  a string of twenty or more\n)\n",
    );
}

/// A list 100,000 long is built by a loop and freed when `main` ends, and
/// neither takes a Rust call per link.
#[test]
fn a_long_list_is_built_and_freed() {
    prints(
        "type L {\n\
         \tCons(h Int, t L)\n\
         \tEnd\n\
         }\n\
         fn build(n Int, acc L) L {\n\
         \tif n == 0 { acc } else { build(n - 1, Cons(n, acc)) }\n\
         }\n\
         pub fn main() {\n\
         \t_l = build(100000, End)\n\
         \tprintln(Cons(0, End))\n\
         }\n",
        "Cons(0, End)\n",
    );
}

#[test]
fn a_match_picks_the_arm_that_fits() {
    prints(
        "type Shape {\n\
         \tCircle(radius Int)\n\
         \tRect(w Int, h Int)\n\
         \tDot\n\
         }\n\
         fn area(s Shape) Int {\n\
         \tmatch s {\n\
         \t\tCircle(r) -> 3 * r * r\n\
         \t\tRect(w, h) -> w * h\n\
         \t\tDot -> 0\n\
         \t}\n\
         }\n\
         pub fn main() {\n\
         \tprintln(area(Circle(radius: 2)))\n\
         \tprintln(area(Rect(w: 3, h: 4)))\n\
         \tprintln(area(Dot))\n\
         }\n",
        "12\n12\n0\n",
    );
}

/// A nested pattern becomes a match inside a match, and a case that fails
/// part-way jumps on to the next one.
#[test]
fn a_nested_pattern_falls_through_to_the_next_case() {
    prints(
        "fn describe(o Option(Option(Int))) String {\n\
         \tmatch o {\n\
         \t\tSome(Some(0)) -> 'zero'\n\
         \t\tSome(Some(n)) -> 'some ${n}'\n\
         \t\tSome(None) -> 'some none'\n\
         \t\tNone -> 'none'\n\
         \t}\n\
         }\n\
         pub fn main() {\n\
         \tprintln(describe(Some(Some(0))))\n\
         \tprintln(describe(Some(Some(5))))\n\
         \tprintln(describe(Some(None)))\n\
         \tprintln(describe(None))\n\
         }\n",
        "zero\nsome 5\nsome none\nnone\n",
    );
}

#[test]
fn a_match_on_ints_and_strings() {
    prints(
        "fn count(n Int) String {\n\
         \tmatch n {\n\
         \t\t0 -> 'none'\n\
         \t\t1 | 2 -> 'a few'\n\
         \t\t_ -> 'many'\n\
         \t}\n\
         }\n\
         fn greet(s String) String {\n\
         \tmatch s {\n\
         \t\t'hi' -> 'hello'\n\
         \t\t'' -> 'nothing'\n\
         \t\tother -> 'what is ${other}?'\n\
         \t}\n\
         }\n\
         pub fn main() {\n\
         \tprintln(count(0))\n\
         \tprintln(count(2))\n\
         \tprintln(count(9))\n\
         \tprintln(count(9223372036854775807))\n\
         \tprintln(greet('hi'))\n\
         \tprintln(greet(''))\n\
         \tprintln(greet('hiya'))\n\
         }\n",
        "none\na few\nmany\nmany\nhello\nnothing\nwhat is hiya?\n",
    );
}

#[test]
fn a_record_field_reads_by_name() {
    prints(
        "type Point {\n\
         \tPoint(x Int, y Int)\n\
         }\n\
         pub fn main() {\n\
         \tp = Point(x: 3, y: 4)\n\
         \tprintln(p.x * p.x + p.y * p.y)\n\
         }\n",
        "25\n",
    );
}

/// A list walked by recursion, and one walked by a loop, 100,000 long.
#[test]
fn a_list_is_walked() {
    prints(
        "type L {\n\
         \tCons(h Int, t L)\n\
         \tEnd\n\
         }\n\
         fn build(n Int, acc L) L {\n\
         \tif n == 0 { acc } else { build(n - 1, Cons(n, acc)) }\n\
         }\n\
         fn sum(l L) Int {\n\
         \tmatch l {\n\
         \t\tCons(h, t) -> h + sum(t)\n\
         \t\tEnd -> 0\n\
         \t}\n\
         }\n\
         fn length(l L, n Int) Int {\n\
         \tmatch l {\n\
         \t\tCons(_, t) -> length(t, n + 1)\n\
         \t\tEnd -> n\n\
         \t}\n\
         }\n\
         pub fn main() {\n\
         \tprintln(sum(build(10, End)))\n\
         \tprintln(length(build(100000, End), 0))\n\
         }\n",
        "55\n100000\n",
    );
}

#[test]
fn a_closure_sees_what_it_captured() {
    prints(
        "fn apply(f fn(Int) String, x Int) String { f(x) }\n\
         pub fn main() {\n\
         \tn = 5\n\
         \ts = 'x'\n\
         \tprintln(apply(fn(a) { '${s}${a + n}' }, 1))\n\
         }\n",
        "x6\n",
    );
}

/// A closure made in one call and called after that call has returned.
#[test]
fn a_returned_closure_keeps_its_captures() {
    prints(
        "fn adder(n Int) fn(Int) Int { fn(x) { x + n } }\n\
         fn twice(f fn(Int) Int, x Int) Int { f(f(x)) }\n\
         pub fn main() {\n\
         \tadd3 = adder(3)\n\
         \tprintln(add3(1))\n\
         \tprintln(twice(add3, 1))\n\
         \tprintln(adder(2)(4))\n\
         \tprintln(twice(adder(10), 0))\n\
         }\n",
        "4\n7\n6\n20\n",
    );
}

/// A lambda that calls itself keeps its captures on every call, and one
/// that loops by calling itself in tail position runs in constant space.
#[test]
fn a_lambda_calling_itself_keeps_its_captures() {
    prints(
        "pub fn main() {\n\
         \tk = 7\n\
         \tgo = fn(n) { if n == 0 { k } else { go(n - 1) } }\n\
         \tprintln(go(3))\n\
         \tprintln(go(100000))\n\
         \tdepth = fn(n) { if n == 0 { k } else { 1 + depth(n - 1) } }\n\
         \tprintln(depth(10))\n\
         \th = fn(n) { if n == 0 { go } else { h(n - 1) } }\n\
         \tprintln(h(2)(0))\n\
         }\n",
        "7\n7\n17\n7\n",
    );
}

/// A named function works as a value as well as a lambda does.
#[test]
fn a_named_function_is_a_value() {
    prints(
        "fn double(x Int) Int { x * 2 }\n\
         fn apply(f fn(Int) Int, x Int) Int { f(x) }\n\
         pub fn main() {\n\
         \tprintln(apply(double, 21))\n\
         \tf = double\n\
         \tprintln(f(4))\n\
         \tprintln(double)\n\
         \tprintln(Some(double))\n\
         }\n",
        "42\n8\n<fn#double>\nSome(<fn#double>)\n",
    );
}

#[test]
fn a_tuple_is_built_read_and_shown() {
    prints(
        "fn swap(t (Int, String)) (String, Int) { (t.1, t.0) }\n\
         fn sum(t (Int, (Int, Int))) Int {\n\
         \tmatch t {\n\
         \t\t(a, (b, c)) -> a + b + c\n\
         \t}\n\
         }\n\
         pub fn main() {\n\
         \tprintln(swap((1, 'one')))\n\
         \tprintln(sum((1, (2, 3))))\n\
         \tprintln((1, (2, 3)))\n\
         \tprintln(Some((True, Nil)))\n\
         \tprintln('${(1, 2)}')\n\
         }\n",
        "(one, 1)\n6\n(\n  1,\n  (2, 3)\n)\nSome(\n  (True, Nil)\n)\n(1, 2)\n",
    );
}

/// A tuple of small values stays on one line up to 80 columns, and takes a
/// line per element past that.
#[test]
fn a_wide_tuple_takes_a_line_per_element() {
    let exactly_80 =
        "('aaaaaaaaaaaaaaaaaa', 'bbbbbbbbbbbbbbbbbb', 'cccccccccccccccccc', 'dddddddddddddddddd')";
    prints(
        &format!("pub fn main() {{\n\tprintln({exactly_80})\n}}\n"),
        "(aaaaaaaaaaaaaaaaaa, bbbbbbbbbbbbbbbbbb, cccccccccccccccccc, dddddddddddddddddd)\n",
    );
    let past_80 =
        "('aaaaaaaaaaaaaaaaaa', 'bbbbbbbbbbbbbbbbbb', 'cccccccccccccccccc', 'ddddddddddddddddddd')";
    prints(
        &format!("pub fn main() {{\n\tprintln({past_80})\n}}\n"),
        "(\n  aaaaaaaaaaaaaaaaaa,\n  bbbbbbbbbbbbbbbbbb,\n  cccccccccccccccccc,\n  ddddddddddddddddddd\n)\n",
    );
}

#[test]
fn an_array_is_built_and_shown() {
    prints(
        "pub fn main() {\n\
         \tprintln([1, 2, 3])\n\
         \tprintln([])\n\
         \tprintln(['a', 'b'])\n\
         \tprintln([[1, 2], [3, 4]])\n\
         \tprintln(Some([1]))\n\
         \tprintln('${[True, False]}')\n\
         }\n",
        "[1, 2, 3]\n[]\n[a, b]\n[\n  [1, 2],\n  [3, 4]\n]\nSome(\n  [1]\n)\n[True, False]\n",
    );
}

/// A long array of small values goes six to a line.
#[test]
fn a_long_array_goes_six_to_a_line() {
    prints(
        "fn upto(n Int, acc Array(Int)) Array(Int) {\n\
         \tif n == 0 { acc } else { upto(n - 1, [n, ..acc]) }\n\
         }\n\
         pub fn main() {\n\
         \tprintln(upto(40, []))\n\
         }\n",
        "[\n  1, 2, 3, 4, 5, 6, \n  7, 8, 9, 10, 11, 12, \n  13, 14, 15, 16, 17, 18, \n  \
         19, 20, 21, 22, 23, 24, \n  25, 26, 27, 28, 29, 30, \n  31, 32, 33, 34, 35, 36, \n  \
         37, 38, 39, 40\n]\n",
    );
}

/// Arrays walked the way Scarlet code walks them: head and rest, built by
/// appending in a loop and by prepending in one, joined and indexed.
#[test]
fn an_array_is_walked_built_and_joined() {
    prints(
        "fn sum(xs Array(Int)) Int {\n\
         \tmatch xs {\n\
         \t\t[] -> 0\n\
         \t\t[h, ..t] -> h + sum(t)\n\
         \t}\n\
         }\n\
         fn total(xs Array(Int), acc Int) Int {\n\
         \tmatch xs {\n\
         \t\t[] -> acc\n\
         \t\t[h, ..t] -> total(t, acc + h)\n\
         \t}\n\
         }\n\
         fn build(n Int, acc Array(Int)) Array(Int) {\n\
         \tif n == 0 { acc } else { build(n - 1, [..acc, n]) }\n\
         }\n\
         fn describe(xs Array(Int)) String {\n\
         \tmatch xs {\n\
         \t\t[] -> 'empty'\n\
         \t\t[a] -> 'one ${a}'\n\
         \t\t[a, b] -> 'two ${a} ${b}'\n\
         \t\t[a, _, ..rest] -> 'from ${a}, then ${rest}'\n\
         \t}\n\
         }\n\
         pub fn main() {\n\
         \tprintln(sum([1, 2, 3, 4]))\n\
         \tbig = build(10000, [])\n\
         \tprintln(total(big, 0))\n\
         \tprintln(describe([]))\n\
         \tprintln(describe([7]))\n\
         \tprintln(describe([7, 8]))\n\
         \tprintln(describe([7, 8, 9, 10]))\n\
         \tprintln([0, ..[1, 2], 3])\n\
         \tprintln([..[1, 2], ..[3, 4]])\n\
         \tprintln([10, 20, 30][1] or 99)\n\
         \tprintln([10, 20, 30][3] or 99)\n\
         \tprintln([10, 20, 30][-1] or 99)\n\
         \tprintln(big[9999] or 99)\n\
         \tprintln(big[9223372036854775807] or 99)\n\
         }\n",
        "10\n50005000\nempty\none 7\ntwo 7 8\nfrom 7, then [9, 10]\n[0, 1, 2, 3]\n[1, 2, 3, 4]\n\
         20\n99\n99\n1\n99\n",
    );
}

/// `xs[i]` is an `Option`: `Some` of the element, or `None` past either end.
#[test]
fn indexing_an_array_gives_an_option() {
    prints(
        "pub fn main() {\n\
         \txs = ['a', 'b', 'c']\n\
         \tprintln(xs[0])\n\
         \tprintln(xs[2])\n\
         \tprintln(xs[3])\n\
         \tprintln(xs[-1])\n\
         \tprintln(xs[9223372036854775807])\n\
         \tmatch xs[1] {\n\
         \t\tSome(s) -> println('found ${s}')\n\
         \t\tNone -> println('none')\n\
         \t}\n\
         \tprintln(xs[5] or 'default')\n\
         \tprintln(xs[1 - 2] or 'computed')\n\
         }\n",
        "Some(a)\nSome(c)\nNone\nNone\nNone\nfound b\ndefault\ncomputed\n",
    );
}

/// `xs[a..b]` is a `Result`: `Ok` of the elements when the range is inside
/// the array, and `Err(Nil)` when it is not, never a crash.
#[test]
fn a_slice_is_a_result() {
    prints(
        "fn upto(n Int, acc Array(Int)) Array(Int) {\n\
         \tif n == 0 { acc } else { upto(n - 1, [n, ..acc]) }\n\
         }\n\
         pub fn main() {\n\
         \txs = [1, 2, 3, 4, 5]\n\
         \tprintln(xs[1..3])\n\
         \tprintln(xs[0..5])\n\
         \tprintln(xs[2..2])\n\
         \tprintln(xs[5..5])\n\
         \tprintln(xs[3..9])\n\
         \tprintln(xs[3..1])\n\
         \tprintln(xs[-1..2])\n\
         \tprintln(xs[0..9223372036854775807])\n\
         \tbig = upto(5000, [])\n\
         \tmatch big[4990..4995] {\n\
         \t\tOk(part) -> println(part)\n\
         \t\tErr(Nil) -> println('missed')\n\
         \t}\n\
         \tprintln(xs[9..10] or [0])\n\
         }\n",
        "Ok(\n  [2, 3]\n)\nOk(\n  [1, 2, 3, 4, 5]\n)\nOk(\n  []\n)\nOk(\n  []\n)\n\
         Err(Nil)\nErr(Nil)\nErr(Nil)\nErr(Nil)\n[4991, 4992, 4993, 4994, 4995]\n[0]\n",
    );
}

/// A range stores only its two ends, and is an array of Ints in every way a
/// program can see: shown, indexed, walked, sliced, spread and joined.
#[test]
fn a_range_is_an_array_of_its_ints() {
    prints(
        "fn total(xs Array(Int), acc Int) Int {\n\
         \tmatch xs {\n\
         \t\t[] -> acc\n\
         \t\t[h, ..t] -> total(t, acc + h)\n\
         \t}\n\
         }\n\
         pub fn main() {\n\
         \tprintln(1..4)\n\
         \tprintln(3..3)\n\
         \tprintln(5..2)\n\
         \tprintln({ 10..20 }[2])\n\
         \tprintln({ 10..20 }[10])\n\
         \tprintln(total(0..100001, 0))\n\
         \tprintln({ 0..10 }[2..5])\n\
         \tprintln({ 0..10 }[8..11])\n\
         \tprintln([1, ..{ 5..8 }, 9])\n\
         \tprintln([..{ 0..2 }, ..{ 7..9 }])\n\
         \tprintln('${0..3}')\n\
         \tprintln(0..40)\n\
         \tprintln({ 9223372036854775800..9223372036854775807 }[6] or 0)\n\
         \tbig = 0..1000000000000\n\
         \tprintln(big[999999999999] or 0)\n\
         }\n",
        "[1, 2, 3]\n[]\n[]\nSome(12)\nNone\n5000050000\nOk(\n  [2, 3, 4]\n)\nErr(Nil)\n\
         [1, 5, 6, 7, 9]\n[0, 1, 7, 8]\n[0, 1, 2]\n\
         [\n  0, 1, 2, 3, 4, 5, \n  6, 7, 8, 9, 10, 11, \n  12, 13, 14, 15, 16, 17, \n  \
         18, 19, 20, 21, 22, 23, \n  24, 25, 26, 27, 28, 29, \n  30, 31, 32, 33, 34, 35, \n  \
         36, 37, 38, 39\n]\n9223372036854775806\n999999999999\n",
    );
}

/// `==` compares what values hold, of every kind the VM builds.
#[test]
fn equality_is_structural() {
    prints(
        "type Shape {\n\
         \tCircle(r Int)\n\
         \tDot\n\
         }\n\
         fn add(n Int) fn(Int) Int { fn(x) { x + n } }\n\
         pub fn main() {\n\
         \tprintln(1 == 1)\n\
         \tprintln(1 != 2)\n\
         \tprintln(9223372036854775807 + 1 == 9223372036854775807 + 1)\n\
         \tprintln('ab' == 'a${'b'}')\n\
         \tprintln('ab' == 'abc')\n\
         \tprintln(Circle(r: 2) == Circle(r: 2))\n\
         \tprintln(Circle(r: 2) == Circle(r: 3))\n\
         \tprintln(Circle(r: 2) == Dot)\n\
         \tprintln(Dot == Dot)\n\
         \tprintln(Some([1, 2]) == Some([1, 2]))\n\
         \tprintln((1, 'a') == (1, 'a'))\n\
         \tprintln((1, 'a') == (1, 'b'))\n\
         \tprintln([1, 2, 3] == [1, 2])\n\
         \tprintln(0..3 == [0, 1, 2])\n\
         \tprintln([0, 1, 2] == 0..3)\n\
         \tprintln(5..2 == 7..7)\n\
         \tprintln(0..3 == 1..4)\n\
         \tprintln(add(1) == add(1))\n\
         \tprintln(add(1) == add(2))\n\
         \tprintln(True == !False)\n\
         \tprintln(Nil == Nil)\n\
         }\n",
        "True\nTrue\nTrue\nTrue\nFalse\nTrue\nFalse\nFalse\nTrue\nTrue\nTrue\nFalse\nFalse\n\
         True\nTrue\nTrue\nFalse\nTrue\nFalse\nTrue\nTrue\n",
    );
}

/// Two lists 200,000 long compare without overflowing the stack, and
/// the first difference, however deep, decides.
#[test]
fn equality_walks_deep_values_in_constant_stack() {
    prints(
        "type L {\n\
         \tCons(h Int, t L)\n\
         \tEnd\n\
         }\n\
         fn build(n Int, last Int, acc L) L {\n\
         \tif n == 0 { Cons(last, acc) } else { build(n - 1, last, Cons(n, acc)) }\n\
         }\n\
         pub fn main() {\n\
         \tprintln(build(200000, 0, End) == build(200000, 0, End))\n\
         \tprintln(build(200000, 0, End) == build(200000, 1, End))\n\
         }\n",
        "True\nFalse\n",
    );
}

#[test]
fn the_first_built_ins_run() {
    prints(
        "import scarlet/array\n\
         import scarlet/int\n\
         import scarlet/string\n\
         pub fn main() {\n\
         \tprintln(string.inspect(Some([1, 2])))\n\
         \tprintln(string.inspect('as is'))\n\
         \tprintln(string.length('héllo'))\n\
         \tprintln(string.length(''))\n\
         \tprintln(array.length([1, 2, 3]))\n\
         \tprintln(array.length(0..1000000000000))\n\
         \tprintln(int.to_string(-42))\n\
         \tprintln(int.to_string(9223372036854775807 + 1))\n\
         \tprintln(array.map([1, 2, 3], fn(x) { x * 10 }))\n\
         \tprintln(array.reverse([1, 2, 3]))\n\
         }\n",
        "Some(\n  [1, 2]\n)\nas is\n5\n0\n3\n1000000000000\n-42\n9223372036854775808\n[10, 20, 30]\n[3, 2, 1]\n",
    );
}

/// Floats follow `docs/semantics.md`: no NaN and no infinity, ever.
#[test]
fn floats_are_never_nan_or_infinite() {
    prints(
        "import scarlet/float\n\
         fn double(x) { x + x }\n\
         fn big(x Float, n Int) Float { if n == 0 { x } else { big(x * 10.0, n - 1) } }\n\
         pub fn main() {\n\
         \tprintln(1.5 + 2.25)\n\
         \tprintln(1.0)\n\
         \tprintln(0.1 + 0.2)\n\
         \tprintln(-2.5 * 4.0)\n\
         \tprintln(1.0 / 0.0)\n\
         \tprintln(0.0 / 0.0)\n\
         \tprintln(7.5 % 0.0)\n\
         \tprintln(-7.5 % 2.0)\n\
         \tprintln(big(1.0, 400))\n\
         \tprintln(big(-1.0, 400))\n\
         \tprintln(big(1.0, 400) > 1.0)\n\
         \tprintln(0.0 == -0.0)\n\
         \tprintln(2.5 < 3.0)\n\
         \tprintln(double(1.25))\n\
         \tprintln(double(3))\n\
         \tn = 2.5\n\
         \tprintln(-n)\n\
         \tprintln('${Some(1.5)}')\n\
         \tprintln(float.floor(-2.5))\n\
         \tprintln(float.round(2.5))\n\
         \tprintln(float.truncate(-2.9))\n\
         \tprintln(float.floor(big(1.0, 20)))\n\
         \tprintln(float.from_int(3))\n\
         \tprintln(float.to_string(2.0))\n\
         }\n",
        &format!(
            "3.75\n1.0\n0.30000000000000004\n-10.0\n0.0\n0.0\n7.5\n-1.5\n\
             {max}.0\n-{max}.0\nTrue\nTrue\nTrue\n2.5\n6\n-2.5\nSome(1.5)\n-3\n3\n-2\n\
             100000000000000000000\n3.0\n2.0\n",
            max = f64::MAX
        ),
    );
}
