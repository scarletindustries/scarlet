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
    let src = "fn pair() (Int, Int) { (1, 2) }\n\
               pub fn main() {\n\
               \tprintln(1)\n\
               }\n";
    prints(src, "1\n");
    let calls = "fn pair() (Int, Int) { (1, 2) }\n\
                 pub fn main() {\n\
                 \t_ = pair()\n\
                 }\n";
    assert_eq!(
        run(calls),
        Err(Stop::NotBuiltYet("the operation MakeTuple".into()))
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
