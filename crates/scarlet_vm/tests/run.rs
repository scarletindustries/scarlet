//! These tests run programs end to end. Each is compiled from Scarlet source
//! and run by the VM, and the test checks exactly what it prints.

use scarlet_core::core_ir::{Atom, CoreExpr};
use scarlet_ir::rty::RTy;
use scarlet_vm::{Audit, OsHost, Stop};

fn compile(src: &str) -> scarlet_core::core_ir::Program {
    let result = scarlet_core::compile::compile_text(src, None);
    assert!(result.success(), "{:?}", result.diagnostics());
    result.into_runnable().expect("a clean compile is runnable")
}

fn run(src: &str) -> Result<String, Stop> {
    let mut host = OsHost::new(Vec::new(), Vec::new(), Vec::new());
    scarlet_vm::run(&compile(src), &mut host, Audit::AtEnd)?;
    Ok(String::from_utf8(std::mem::take(host.out())).expect("println writes UTF-8"))
}

fn prints(src: &str, want: &str) {
    assert_eq!(run(src).as_deref(), Ok(want), "{src}");
}

/// Run `src` in a world with `argv` and `env`.
fn prints_in(argv: Vec<String>, env: Vec<(String, String)>, src: &str, want: &str) {
    let mut host = OsHost::new(argv, env, Vec::new());
    let ran = scarlet_vm::run(&compile(src), &mut host, Audit::AtEnd);
    assert_eq!(ran, Ok(()), "{src}");
    let out = std::mem::take(host.out());
    assert_eq!(String::from_utf8(out).expect("UTF-8"), want, "{src}");
}

/// A fresh directory for one test's files.
fn scratch(name: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("scarlet-vm-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("a scratch directory");
    dir
}

#[test]
fn one_plus_two() {
    prints("pub fn main() {\n\tprintln(1 + 2)\n}\n", "3\n");
}

/// `print` writes what `println` would, without the newline.
#[test]
fn print_leaves_the_line_open() {
    prints(
        "pub fn main() {\n\
         \tprint('a')\n\
         \tprint(1 + 2)\n\
         \tprintln(' b')\n\
         \tprint([1, 2])\n\
         }\n",
        "a3 b\n[1, 2]",
    );
}

/// `process.sleep` waits at least as long as it is asked, and a wait of zero
/// or less returns at once.
#[test]
fn sleep_waits() {
    prints(
        "import scarlet/process\n\
         import scarlet/time\n\
         pub fn main() {\n\
         \tstart = time::monotonic()\n\
         \tprocess::sleep(30)\n\
         \tprintln(time::since_ms(time::monotonic(), start) >= 30)\n\
         \tprocess::sleep(0)\n\
         \tprocess::sleep(-5)\n\
         \tprocess::sleep(-9223372036854775807 * 1000)\n\
         \tprintln('done')\n\
         }\n",
        "True\ndone\n",
    );
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

/// A call takes over an argument only when nothing reads it later. Here
/// `xs` and `m` go to calls and pushes and are read again afterwards, on
/// both arms of an `if`, so each must still hold what it held, unchanged by
/// what the calls built from it.
#[test]
fn a_value_passed_on_is_still_there_for_what_reads_it_later() {
    prints(
        "import scarlet/array\n\
         import scarlet/map\n\
         fn keep(xs Array(Int)) Array(Int) { xs }\n\
         fn grow(xs Array(Int)) Array(Int) { [..xs, 9] }\n\
         fn both(xs Array(Int), flag Bool) Int {\n\
         \tys = grow(xs)\n\
         \tif flag { array::length(xs) + array::length(ys) } else { array::length(keep(xs)) }\n\
         }\n\
         pub fn main() {\n\
         \txs = [1, 2, 3]\n\
         \tys = [0, ..xs]\n\
         \tzs = [..xs, 4]\n\
         \tm = map::set(map::new(), 1, 10)\n\
         \tn = map::set(m, 2, 20)\n\
         \tprintln(xs)\n\
         \tprintln(ys)\n\
         \tprintln(zs)\n\
         \tprintln(map::size(m))\n\
         \tprintln(map::size(n))\n\
         \tprintln(both(xs, True))\n\
         \tprintln(both(xs, False))\n\
         \tprintln(xs)\n\
         }\n",
        "[1, 2, 3]\n[0, 1, 2, 3]\n[1, 2, 3, 4]\n1\n2\n7\n3\n[1, 2, 3]\n",
    );
}

/// A branch that ends in a name and one that ends in the call itself are the
/// same function, and they make the same number of cells. The name's
/// register hands its reference to the branch's result, so the push after
/// it finds one holder and writes in place both times. Before a copy could
/// take its source, the first spelling left the array held twice and the
/// push copied it: 4 cells against 3.
#[test]
fn two_spellings_of_a_branch_make_the_same_number_of_cells() {
    prints(
        "import scarlet/internal\n\
         fn fresh(n Int) Array(Int) { [n, n + 1, n + 2] }\n\
         fn named(c Bool, n Int) Array(Int) {\n\
         \tys = if c {\n\
         \t\tt = fresh(n)\n\
         \t\tprintln(n)\n\
         \t\tt\n\
         \t} else {\n\
         \t\tfresh(0)\n\
         \t}\n\
         \t[..ys, 9]\n\
         }\n\
         fn direct(c Bool, n Int) Array(Int) {\n\
         \tys = if c {\n\
         \t\tprintln(n)\n\
         \t\tfresh(n)\n\
         \t} else {\n\
         \t\tfresh(0)\n\
         \t}\n\
         \t[..ys, 9]\n\
         }\n\
         pub fn main() {\n\
         \tstart = internal::cells_made()\n\
         \ta = named(True, 1)\n\
         \tmiddle = internal::cells_made()\n\
         \tb = direct(True, 1)\n\
         \tend = internal::cells_made()\n\
         \tprintln(middle - start)\n\
         \tprintln(end - middle)\n\
         \tprintln(a == b)\n\
         \tprintln(a)\n\
         }\n",
        "1\n1\n3\n3\nTrue\n[1, 2, 3, 9]\n",
    );
}

/// A copy takes its source only when nothing reads the source later. Here
/// `t` is read again after `ys` is made from it, so both names hold the
/// array, the push onto `ys` copies, and `t` still reads as it was built.
#[test]
fn a_value_copied_to_a_new_name_is_still_there_under_the_old_one() {
    prints(
        "fn fresh(n Int) Array(Int) { [n, n + 1, n + 2] }\n\
         fn joined(c Bool, n Int) {\n\
         \tt = fresh(n)\n\
         \tys = if c { t } else { fresh(0) }\n\
         \tprintln([..ys, 9])\n\
         \tprintln(t)\n\
         }\n\
         fn bound(n Int) {\n\
         \tt = fresh(n)\n\
         \tmatch t {\n\
         \t\tys -> println([..ys, 9])\n\
         \t}\n\
         \tprintln(t)\n\
         }\n\
         pub fn main() {\n\
         \tjoined(True, 1)\n\
         \tjoined(False, 1)\n\
         \tbound(1)\n\
         }\n",
        "[1, 2, 3, 9]\n[1, 2, 3]\n[0, 1, 2, 9]\n[1, 2, 3]\n[1, 2, 3, 9]\n[1, 2, 3]\n",
    );
}

/// One path copies `t` to the branch's result and takes it, and the other
/// drops it. Each path gives the array up exactly once, which the audit at
/// the end of the run checks, and the path that keeps it pushes in place.
#[test]
fn a_value_taken_on_one_path_and_dropped_on_the_other_is_freed_once() {
    prints(
        "import scarlet/internal\n\
         fn fresh(n Int) Array(Int) { [n, n + 1, n + 2] }\n\
         fn pick(c Bool, n Int) Array(Int) {\n\
         \tt = fresh(n)\n\
         \tys = if c { t } else { fresh(0) }\n\
         \t[..ys, 9]\n\
         }\n\
         pub fn main() {\n\
         \tstart = internal::cells_made()\n\
         \ta = pick(True, 1)\n\
         \tmiddle = internal::cells_made()\n\
         \tb = pick(False, 1)\n\
         \tend = internal::cells_made()\n\
         \tprintln(a)\n\
         \tprintln(b)\n\
         \tprintln(middle - start)\n\
         \tprintln(end - middle)\n\
         }\n",
        "[1, 2, 3, 9]\n[0, 1, 2, 9]\n3\n5\n",
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

/// A loop written as tail recursion. Each step's call is a tail call, which
/// reuses the frame, so a million steps fit in the room of one.
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

/// How many cells `src` makes and how many words its heap grows to, when its
/// `N` is `n`. It prints `want`.
fn cells_and_words(src: &str, n: usize, want: &str) -> (usize, usize) {
    let (out, counts) = counted(&src.replace('N', &n.to_string()));
    assert_eq!(out, want, "{n}");
    (counts.cells_made, counts.heap_words)
}

/// A match that has no more use for a cell nothing else holds frees it, and the
/// next constructor of its size takes its space. Here each turn makes a pair and
/// takes the last one apart, so a thousand turns make a thousand cells and the
/// heap is no larger than for one.
#[test]
fn a_loop_takes_the_space_of_the_cell_it_just_gave_up() {
    let src = "type Pair {\n\
         \tPair(a Int, b Int)\n\
         }\n\
         fn spin(n Int, p Pair) Int {\n\
         \tmatch p {\n\
         \t\tPair(a, b) -> if n <= 0 then a + b else spin(n - 1, Pair(b, a + 1))\n\
         \t}\n\
         }\n\
         pub fn main() {\n\
         \tprintln(spin(N, Pair(0, 0)))\n\
         }\n";
    let (made, words) = cells_and_words(src, 1, "1\n");
    assert_eq!(cells_and_words(src, 1000, "1000\n"), (made + 999, words));
}

/// The same across a call, which needs the call to be a move. The caller hands
/// over its own reference, so the callee is the only holder and may take the
/// cell apart. Every `Cons` of the new list goes where one of the old was, so a
/// walk that builds a list needs no more room than one that only reads it.
#[test]
fn a_walk_over_a_list_takes_the_space_of_the_list_it_reads() {
    let src = |main: &str| {
        format!(
            "type List {{\n\
             \tCons(head Int, tail List)\n\
             \tEmpty\n\
             }}\n\
             fn build(n Int, acc List) List {{\n\
             \tif n <= 0 then acc else build(n - 1, Cons(n, acc))\n\
             }}\n\
             fn add_one(xs List) List {{\n\
             \tmatch xs {{\n\
             \t\tCons(h, t) -> Cons(h + 1, add_one(t))\n\
             \t\tEmpty -> Empty\n\
             \t}}\n\
             }}\n\
             fn sum(xs List, acc Int) Int {{\n\
             \tmatch xs {{\n\
             \t\tCons(h, t) -> sum(t, acc + h)\n\
             \t\tEmpty -> acc\n\
             \t}}\n\
             }}\n\
             pub fn main() {{\n\
             \tprintln(sum({main}, 0))\n\
             }}\n"
        )
    };
    let read = cells_and_words(&src("build(N, Empty)"), 1000, "500500\n");
    let rebuilt = cells_and_words(&src("add_one(build(N, Empty))"), 1000, "501500\n");
    assert_eq!(rebuilt, (read.0 + 1000, read.1));
}

/// The cell here is given up once the match that reads it has given its value, so
/// the turn after has its space.
#[test]
fn a_loop_has_the_space_of_the_cell_its_last_turn_made() {
    let src = "type Pair {\n\
         \tPair(a Int, b Int)\n\
         }\n\
         fn go(n Int, acc Int) Int {\n\
         \tp = Pair(n, n + 1)\n\
         \ts = match p {\n\
         \t\tPair(a, b) -> a + b\n\
         \t}\n\
         \tif n <= 0 then acc + s else go(n - 1, acc + s)\n\
         }\n\
         pub fn main() {\n\
         \tprintln(go(N, 0))\n\
         }\n";
    let (made, words) = cells_and_words(src, 1, "4\n");
    assert_eq!(cells_and_words(src, 1000, "1002001\n"), (made + 999, words));
}

/// The same loop with the pair kept in a list makes one every turn, and every
/// pair still reads back as it was written. Taking apart a cell something else
/// holds would show up here.
#[test]
fn a_cell_something_else_holds_is_left_as_it_was() {
    prints(
        "import scarlet/array\n\
         import scarlet/internal\n\
         type Pair {\n\
         \tPair(a Int, b Int)\n\
         }\n\
         fn keep(n Int, ps Array(Pair)) Array(Pair) {\n\
         \tif n <= 0 then ps else keep(n - 1, array::concat(ps, [Pair(n, n)]))\n\
         }\n\
         pub fn main() {\n\
         \tmade = internal::cells_made()\n\
         \tps = keep(100, [])\n\
         \tprintln(array::fold(ps, 0, fn(t, p) {\n\
         \t\tmatch p {\n\
         \t\t\tPair(a, b) -> t + a + b\n\
         \t\t}\n\
         \t}))\n\
         \tprintln(internal::cells_made() - made > 100)\n\
         }\n",
        "10100\nTrue\n",
    );
}

/// Frames live on the VM's own stack, not Rust's, so a deep recursion that is
/// not a tail call just uses memory, and nothing overflows.
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

/// A function that uses something not built yet still loads. Only calling
/// it stops the run, and the stop says what it needs.
#[test]
fn only_calling_an_unbuilt_function_stops_the_run() {
    let src = "import scarlet/process\n\
               fn me() process::Pid { process::self() }\n\
               pub fn main() {\n\
               \tprintln(1)\n\
               }\n";
    prints(src, "1\n");
    let calls = "import scarlet/process\n\
                 fn me() process::Pid { process::self() }\n\
                 pub fn main() {\n\
                 \t_ = me()\n\
                 }\n";
    match run(calls) {
        Err(Stop::NotBuiltYet(what)) => assert_eq!(what.to_string(), "the built-in ProcessSelf"),
        other => panic!("expected NotBuiltYet, got {other:?}"),
    }
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

/// An Int is exact however big it gets. Past 51 bits it moves to the heap,
/// and past 64 it keeps going. The first line is a number well inside the
/// small range.
#[test]
fn ints_are_exact_past_51_and_64_bits() {
    prints(
        "fn fact(n Int) Int { if n == 0 { 1 } else { n * fact(n - 1) } }\n\
         pub fn main() {\n\
         \tprintln(140737488355327 + 1)\n\
         \tprintln(1125899906842623 + 1)\n\
         \tprintln(0 - 1125899906842623 - 2)\n\
         \tprintln(9223372036854775807 + 1)\n\
         \tprintln(fact(25))\n\
         \tprintln(0 - 9223372036854775807 - 1)\n\
         }\n",
        "140737488355328\n1125899906842624\n-1125899906842625\n9223372036854775808\n\
         15511210043330985984000000\n-9223372036854775808\n",
    );
}

/// A small Int holds -2^50 to 2^50 - 1 and costs no cell. One step past
/// either end is a heap cell, and one step back is no cell again. A program
/// cannot see the difference, so this counts cells to find it.
#[test]
fn a_small_int_holds_51_bits_and_the_next_one_is_a_cell() {
    prints(
        "import scarlet/internal\n\
         fn step(x Int, by Int, n Int) Int { if n == 0 { x } else { step(x + by, by, n - 1) } }\n\
         fn cells(from Int, by Int, n Int) Int {\n\
         \tmade = internal::cells_made()\n\
         \t_ = step(from, by, n)\n\
         \tinternal::cells_made() - made\n\
         }\n\
         pub fn main() {\n\
         \ttop = 1125899906842623\n\
         \tbottom = 0 - top - 1\n\
         \tprintln(cells(top - 10, 1, 10))\n\
         \tprintln(cells(top - 10, 1, 11))\n\
         \tprintln(cells(top - 10, 1, 13))\n\
         \tprintln(cells(bottom + 10, -1, 10))\n\
         \tprintln(cells(bottom + 10, -1, 11))\n\
         \tprintln(cells(bottom + 10, -1, 13))\n\
         \tprintln(cells(step(top, 1, 3), -1, 3))\n\
         \tprintln(cells(step(bottom, -1, 3), 1, 3))\n\
         \tprintln(step(top - 10, 1, 13))\n\
         \tprintln(step(bottom + 10, -1, 13))\n\
         \tprintln(step(step(top, 1, 3), -1, 3) == top)\n\
         \tprintln(step(step(bottom, -1, 3), 1, 3) == bottom)\n\
         }\n",
        "0\n1\n3\n0\n1\n3\n2\n2\n1125899906842626\n-1125899906842627\nTrue\nTrue\n",
    );
}

/// The small Int range holds its lowest number and not that number negated,
/// so `min / -1` and `-min` leave it though both fit 64 bits, and come back
/// into it the other way. Nothing at the top end leaves.
#[test]
fn divide_and_negate_are_right_at_both_ends_of_the_small_range() {
    prints(
        "import scarlet/internal\n\
         fn div(a Int, b Int) Int { a / b }\n\
         fn rem(a Int, b Int) Int { a % b }\n\
         fn neg(a Int) Int { -a }\n\
         fn cells(f fn(Int, Int) Int, a Int, b Int) Int {\n\
         \tmade = internal::cells_made()\n\
         \t_ = f(a, b)\n\
         \tinternal::cells_made() - made\n\
         }\n\
         pub fn main() {\n\
         \tmax = 1125899906842623\n\
         \tmin = 0 - max - 1\n\
         \tprintln(div(min, -1))\n\
         \tprintln(neg(min))\n\
         \tprintln(neg(neg(min)) == min)\n\
         \tprintln(div(div(min, -1), -1) == min)\n\
         \tprintln(div(max, -1))\n\
         \tprintln(neg(max))\n\
         \tprintln(neg(neg(max)) == max)\n\
         \tprintln(div(min, 1) == min)\n\
         \tprintln(div(min, 0))\n\
         \tprintln(rem(min, 0) == min)\n\
         \tprintln(rem(min, -1))\n\
         \tprintln(rem(max, 0) == max)\n\
         \tprintln(div(min, 2))\n\
         \tprintln(div(max, 2))\n\
         \tprintln(cells(div, min, -1))\n\
         \tprintln(cells(div, max, -1))\n\
         \tprintln(cells(div, min, 1))\n\
         \tprintln(cells(fn(a, _b) neg(a), min, 0))\n\
         \tprintln(cells(fn(a, _b) neg(a), max, 0))\n\
         \tprintln(cells(fn(a, _b) neg(neg(a)), min, 0))\n\
         \tprintln(cells(fn(a, b) a * b, 33554432, 33554432))\n\
         \tprintln(cells(fn(a, b) a * b, 33554432, -33554432))\n\
         \tprintln(33554432 * -33554432 == min)\n\
         }\n",
        "1125899906842624\n1125899906842624\nTrue\nTrue\n-1125899906842623\n-1125899906842623\n\
         True\nTrue\n0\nTrue\n0\nTrue\n-562949953421312\n562949953421311\n\
         1\n0\n0\n1\n0\n1\n1\n0\nTrue\n",
    );
}

/// The join of an empty array and a range is a tree, so a push on it, or a write to it,
/// while something else holds it costs the same however long it is. A push on a range
/// that is held builds the range as a tree each time.
#[test]
fn a_range_joined_to_the_empty_array_is_built_once() {
    prints(
        "import scarlet/array\n\
         import scarlet/internal\n\
         fn none(n Int) Array(Int) { if n < 0 { [n] } else { [] } }\n\
         fn pushes(xs Array(Int), i Int, n Int, acc Int) Int {\n\
         \tif i == n {\n\
         \t\tacc\n\
         \t} else {\n\
         \t\tpushes(xs, i + 1, n, acc + array::length([..xs, i]) + array::length([i, ..xs]))\n\
         \t}\n\
         }\n\
         fn puts(xs Array(Int), i Int, n Int, acc Int) Int {\n\
         \tif i == n { acc } else { puts(xs, i + 1, n, acc + { array::put(xs, 3, 7)[3] or 0 }) }\n\
         }\n\
         fn cost(xs Array(Int)) {\n\
         \tmade = internal::cells_made()\n\
         \ta = pushes(xs, 0, 100, 0)\n\
         \tpushed = internal::cells_made()\n\
         \tb = puts(xs, 0, 100, 0)\n\
         \tput = internal::cells_made()\n\
         \tprintln(\"${pushed - made} ${put - pushed} ${a} ${b}\")\n\
         }\n\
         pub fn main() {\n\
         \tcost(array::concat(none(0), 0..100))\n\
         \tcost(array::concat(none(0), 0..1000))\n\
         \tcost(array::concat(0..1000, none(1)))\n\
         \tcost([..none(0), ..0..1000])\n\
         \tcost(array::concat(0..10000, none(1)))\n\
         \tcost(0..1000)\n\
         }\n",
        // A write copies the root and a node a level, and 10,000 has one level more.
        "400 300 20200 700\n\
         400 300 200200 700\n\
         400 300 200200 700\n\
         400 300 200200 700\n\
         400 400 2000200 700\n\
         7100 3400 200200 700\n",
    );
}

/// A 32-bit number times a small constant stays a small Int. This is
/// `map_churn`'s key generator, whose products reach about 2^48.1, so it
/// makes no heap cell.
#[test]
fn a_32_bit_generator_makes_no_cell() {
    prints(
        "import scarlet/internal\n\
         fn next(x Int) Int { { x * 69069 + 1 } % 4294967296 }\n\
         fn run(x Int, n Int) Int { if n == 0 { x } else { run(next(x), n - 1) } }\n\
         pub fn main() {\n\
         \tmade = internal::cells_made()\n\
         \tx = run(42, 10000)\n\
         \tprintln(internal::cells_made() - made)\n\
         \tprintln(x)\n\
         \tprintln(4294967295 * 69069 + 1)\n\
         }\n",
        "0\n1722027866\n296649096098356\n",
    );
}

/// A map's order comes from its keys' hashes, and a program can see it. The
/// hash reads a key by what it is and not by the word it is stored as, so
/// these orders are pinned and no change to the value word may move one.
/// The Int keys sit inside the small range, at its edge and past it.
#[test]
fn a_maps_order_does_not_depend_on_how_its_keys_are_stored() {
    prints(
        "import scarlet/map\n\
         type Light {\n\
         \tRed\n\
         \tAmber\n\
         \tGreen\n\
         \tBlue\n\
         \tWhite\n\
         }\n\
         pub fn main() {\n\
         \tlights = map::new() |> map::set(Red, 1) |> map::set(Amber, 2) |> map::set(Green, 3) |> map::set(Blue, 4) |> map::set(White, 5)\n\
         \tprintln(map::keys(lights) == [Red, Green, White, Blue, Amber])\n\
         \tbools = map::new() |> map::set(True, 1) |> map::set(False, 2)\n\
         \tprintln(map::keys(bools) == [False, True])\n\
         \tpairs = map::new() |> map::set((True, Red), 1) |> map::set((False, Green), 2) |> map::set((True, Blue), 3) |> map::set((False, White), 4)\n\
         \tprintln(map::keys(pairs) == [(False, Green), (True, Blue), (True, Red), (False, White)])\n\
         \topts = map::new() |> map::set(Some(Red), 1) |> map::set(None, 2) |> map::set(Some(White), 3) |> map::set(Some(Amber), 4)\n\
         \tprintln(map::keys(opts) == [Some(Amber), None, Some(Red), Some(White)])\n\
         \tints = map::new() |> map::set(140737488355328, 1) |> map::set(1125899906842623, 2) |> map::set(1125899906842624, 3) |> map::set(7, 4) |> map::set(281474976710656, 5) |> map::set(-140737488355329, 6)\n\
         \tprintln(map::keys(ints) == [1125899906842623, -140737488355329, 281474976710656, 7, 1125899906842624, 140737488355328])\n\
         \ttuples = map::new() |> map::set((140737488355328, 1), 1) |> map::set((1125899906842624, 3), 3) |> map::set((281474976710656, 5), 5) |> map::set((7, 4), 4) |> map::set((1125899906842623, 2), 2)\n\
         \tprintln(map::keys(tuples) == [(140737488355328, 1), (1125899906842624, 3), (281474976710656, 5), (7, 4), (1125899906842623, 2)])\n\
         \tprintln(lights)\n\
         }\n",
        "True\nTrue\nTrue\nTrue\nTrue\nTrue\n{Red: 1, Green: 3, White: 5, Blue: 4, Amber: 2}\n",
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
/// multiplying. A literal past 64 bits is still a compile error, because the
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
        scarlet_vm::run(
            &program,
            &mut OsHost::new(Vec::new(), Vec::new(), Closed),
            Audit::AtEnd
        ),
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

/// `xs[i]` is an `Option`. It is `Some` of the element, or `None` past either
/// end.
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

/// `xs[a..b]` is a `Result`. It is `Ok` of the elements when the range is
/// inside the array, and `Err(Nil)` when it is not, never a crash.
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

/// `array::put` gives the array with one element changed, and the array it
/// was given is as it was for anything that still holds it. An index the
/// array does not have changes nothing. `array::set` is the same write, and
/// says whether the index was there.
#[test]
fn a_write_at_an_index_gives_a_new_array_and_leaves_the_old_one() {
    prints(
        "import scarlet/array\n\
         const kept = ['a', 'b', 'c']\n\
         pub fn main() {\n\
         \txs = [1, 2, 3]\n\
         \tprintln(array::put(xs, 1, 9))\n\
         \tprintln(array::put(xs, 3, 9))\n\
         \tprintln(array::put(xs, -1, 9))\n\
         \tprintln(array::put(xs, 9223372036854775807 * 1000, 9))\n\
         \tprintln(array::put([], 0, 9))\n\
         \tprintln(array::set(xs, 0, 7))\n\
         \tprintln(array::set(xs, 3, 7))\n\
         \tprintln(array::set(xs, -1, 7))\n\
         \tprintln(xs)\n\
         \tprintln(array::put(kept, 2, 'z'))\n\
         \tprintln(kept)\n\
         \tprintln(array::put(0..5, 2, 40))\n\
         \tprintln(array::put(0..5, 5, 40))\n\
         \tprintln(array::put([[1], [2]], 0, [3, 4]))\n\
         \tput = array::put\n\
         \tset = array::set\n\
         \tprintln(put(xs, 2, 0))\n\
         \tprintln(set(xs, 9, 0))\n\
         \tmatch array::set(xs, 1, 5) {\n\
         \t\tOk(ys) -> println('wrote ${ys}, kept ${xs}')\n\
         \t\tErr(Nil) -> println('missed')\n\
         \t}\n\
         \tmatch array::set(xs, 5, 5) {\n\
         \t\tOk(ys) -> println(ys)\n\
         \t\tErr(Nil) -> println('missed, kept ${xs}')\n\
         \t}\n\
         \tr = 0..6\n\
         \tw = array::put(r, 2, 100)\n\
         \tprintln(w)\n\
         \tprintln(r)\n\
         \tprintln(array::put(w, 3, 200))\n\
         \tprintln(array::set(r, 6, 0))\n\
         \tprintln(array::length(r))\n\
         }\n",
        "[1, 9, 3]\n[1, 2, 3]\n[1, 2, 3]\n[1, 2, 3]\n[]\n\
         Ok(\n  [7, 2, 3]\n)\nErr(Nil)\nErr(Nil)\n[1, 2, 3]\n\
         [a, b, z]\n[a, b, c]\n[0, 1, 40, 3, 4]\n[0, 1, 2, 3, 4]\n\
         [\n  [3, 4],\n  [2]\n]\n[1, 2, 0]\nErr(Nil)\n\
         wrote [1, 5, 3], kept [1, 2, 3]\nmissed, kept [1, 2, 3]\n\
         [0, 1, 100, 3, 4, 5]\n[0, 1, 2, 3, 4, 5]\n[0, 1, 100, 200, 4, 5]\nErr(Nil)\n6\n",
    );
}

/// A write to an array nothing else holds changes it in place. Filling a
/// 5,000-element array one slot at a time makes no cell at all, and the
/// same loop over an array something else holds copies a path each time.
#[test]
fn a_write_to_an_array_with_one_holder_changes_it_in_place() {
    prints(
        "import scarlet/array\n\
         import scarlet/internal\n\
         fn build(n Int, acc Array(Int)) Array(Int) {\n\
         \tif n == 0 { acc } else { build(n - 1, [..acc, 0]) }\n\
         }\n\
         fn fill(xs Array(Int), i Int, n Int) Array(Int) {\n\
         \tif i == n { xs } else { fill(array::put(xs, i, i * 2), i + 1, n) }\n\
         }\n\
         fn checked(xs Array(Int), i Int, n Int) Array(Int) {\n\
         \tif i == n {\n\
         \t\txs\n\
         \t} else {\n\
         \t\tmatch array::set(xs, i, i * 3) {\n\
         \t\t\tOk(ys) -> checked(ys, i + 1, n)\n\
         \t\t\tErr(Nil) -> []\n\
         \t\t}\n\
         \t}\n\
         }\n\
         fn sum(xs Array(Int), acc Int) Int {\n\
         \tmatch xs {\n\
         \t\t[] -> acc\n\
         \t\t[h, ..t] -> sum(t, acc + h)\n\
         \t}\n\
         }\n\
         fn held(xs Array(Int), i Int, n Int, acc Int) Int {\n\
         \tif i == n {\n\
         \t\tacc\n\
         \t} else {\n\
         \t\tys = array::put(xs, i, 1)\n\
         \t\theld(xs, i + 1, n, acc + { ys[i] or 0 } + { xs[i] or 0 })\n\
         \t}\n\
         }\n\
         pub fn main() {\n\
         \txs = build(5000, [])\n\
         \tmade = internal::cells_made()\n\
         \tys = fill(xs, 0, 5000)\n\
         \tprintln(internal::cells_made() - made)\n\
         \tprintln(sum(ys, 0))\n\
         \tzs = build(5000, [])\n\
         \tmade = internal::cells_made()\n\
         \tzs = checked(zs, 0, 5000)\n\
         \tprintln(internal::cells_made() - made)\n\
         \tprintln(sum(zs, 0))\n\
         \tws = build(100, [])\n\
         \tmade = internal::cells_made()\n\
         \tprintln(held(ws, 0, 100, 0))\n\
         \tprintln(internal::cells_made() - made)\n\
         }\n",
        // `checked` makes no `Ok`, because its `match` is on the write. `held`
        // copies the root and the nodes down to the slot: a branch and a leaf
        // for the 96 elements in the tree, and the tail leaf for the last 4.
        "0\n24995000\n0\n37492500\n100\n296\n",
    );
}

/// A match on a checked write is the write's two edges, so an arm for a miss that
/// gives the array back is no read of it after a hit, and the write is in place. So it
/// is with `or`, and where the `Result` has a name that only the match reads. No
/// `Result` is made. One that is kept is made, and nothing else is.
#[test]
fn a_checked_write_is_in_place_when_its_err_arm_gives_the_array_back() {
    prints(
        "import scarlet/array\n\
         import scarlet/internal\n\
         fn build(n Int, acc Array(Int)) Array(Int) {\n\
         \tif n == 0 { acc } else { build(n - 1, [..acc, 0]) }\n\
         }\n\
         fn arm(xs Array(Int), i Int, n Int) Array(Int) {\n\
         \tif i == n {\n\
         \t\txs\n\
         \t} else {\n\
         \t\tmatch array::set(xs, i, i * 3) {\n\
         \t\t\tOk(ys) -> arm(ys, i + 1, n)\n\
         \t\t\tErr(Nil) -> xs\n\
         \t\t}\n\
         \t}\n\
         }\n\
         fn or_else(xs Array(Int), i Int, n Int) Array(Int) {\n\
         \tif i == n { xs } else { or_else(array::set(xs, i, i * 3) or xs, i + 1, n) }\n\
         }\n\
         fn named(xs Array(Int), i Int, n Int) Array(Int) {\n\
         \tif i == n {\n\
         \t\txs\n\
         \t} else {\n\
         \t\twrote = array::set(xs, i, i * 3)\n\
         \t\tmatch wrote {\n\
         \t\t\tErr(_) -> xs\n\
         \t\t\tOk(ys) -> named(ys, i + 1, n)\n\
         \t\t}\n\
         \t}\n\
         }\n\
         fn one(xs Array(Int), i Int) Result(Array(Int), Nil) { array::set(xs, i, i * 3) }\n\
         fn kept(xs Array(Int), i Int, n Int) Array(Int) {\n\
         \tif i == n { xs } else { kept(one(xs, i) or [], i + 1, n) }\n\
         }\n\
         fn total(xs Array(Int), i Int, n Int) Array(Int) {\n\
         \tif i == n { xs } else { total(array::put(xs, i, i * 3), i + 1, n) }\n\
         }\n\
         fn sum(xs Array(Int), acc Int) Int {\n\
         \tmatch xs {\n\
         \t\t[] -> acc\n\
         \t\t[h, ..t] -> sum(t, acc + h)\n\
         \t}\n\
         }\n\
         pub fn main() {\n\
         \ta = build(5000, [])\n\
         \tmade = internal::cells_made()\n\
         \ta = arm(a, 0, 5000)\n\
         \tprintln(internal::cells_made() - made)\n\
         \tprintln(sum(a, 0))\n\
         \tb = build(5000, [])\n\
         \tmade = internal::cells_made()\n\
         \tb = or_else(b, 0, 5000)\n\
         \tprintln(internal::cells_made() - made)\n\
         \tprintln(sum(b, 0))\n\
         \tc = build(5000, [])\n\
         \tmade = internal::cells_made()\n\
         \tc = total(c, 0, 5000)\n\
         \tprintln(internal::cells_made() - made)\n\
         \tprintln(sum(c, 0))\n\
         \td = build(5000, [])\n\
         \tmade = internal::cells_made()\n\
         \td = named(d, 0, 5000)\n\
         \tprintln(internal::cells_made() - made)\n\
         \tprintln(sum(d, 0))\n\
         \te = build(5000, [])\n\
         \tmade = internal::cells_made()\n\
         \te = kept(e, 0, 5000)\n\
         \tprintln(internal::cells_made() - made)\n\
         \tprintln(sum(e, 0))\n\
         }\n",
        "0\n37492500\n0\n37492500\n0\n37492500\n0\n37492500\n5000\n37492500\n",
    );
}

/// `array::swap` on an array nothing else holds changes it in place, and makes no
/// cell.
#[test]
fn a_swap_of_an_array_with_one_holder_changes_it_in_place() {
    prints(
        "import scarlet/array\n\
         import scarlet/internal\n\
         fn build(n Int, acc Array(Int)) Array(Int) {\n\
         \tif n == 0 { acc } else { build(n - 1, [..acc, n]) }\n\
         }\n\
         fn turn(xs Array(Int), i Int, j Int) Array(Int) {\n\
         \tif i >= j { xs } else { turn(array::swap(xs, i, j), i + 1, j - 1) }\n\
         }\n\
         pub fn main() {\n\
         \txs = build(1000, [])\n\
         \tmade = internal::cells_made()\n\
         \tys = turn(xs, 0, 999)\n\
         \tprintln(internal::cells_made() - made)\n\
         \tprintln(ys[0..3])\n\
         \tprintln(ys[997..1000])\n\
         }\n",
        "0\nOk(\n  [1, 2, 3]\n)\nOk(\n  [998, 999, 1000]\n)\n",
    );
}

/// Elements that are cells: the one a write replaces is given up, the one it
/// puts in is held, and an array kept beside its changed copy shares the
/// rest. The audit at the end of the run finds every count right.
#[test]
fn a_write_keeps_every_element_counted() {
    prints(
        "import scarlet/array\n\
         type Board {\n\
         \trows Array(String)\n\
         \tturn Int\n\
         }\n\
         fn names(n Int, acc Array(String)) Array(String) {\n\
         \tif n == 0 { acc } else { names(n - 1, ['n${n}', ..acc]) }\n\
         }\n\
         fn mark(b Board, i Int) Board {\n\
         \tBoard(..b, rows: array::put(b.rows, i, 'x${i}'), turn: b.turn + 1)\n\
         }\n\
         fn play(b Board, i Int) Board {\n\
         \tif i >= 40 { b } else { play(mark(b, i), i + 3) }\n\
         }\n\
         pub fn main() {\n\
         \tstart = Board(names(40, []), 0)\n\
         \tdone = play(start, 0)\n\
         \tprintln(done.turn)\n\
         \tprintln(done.rows[0..7])\n\
         \tprintln(start.rows[0..7])\n\
         \tswapped = array::swap(done.rows, 0, 39)\n\
         \tprintln(swapped[0])\n\
         \tprintln(swapped[39])\n\
         \tprintln(done.rows[39])\n\
         \tprintln(array::swap(['a', 'b', 'c'], 0, 2))\n\
         \tprintln(array::swap(['a', 'b', 'c'], 1, 1))\n\
         \tprintln(array::swap(['a', 'b', 'c'], 0, 3))\n\
         \tprintln(array::swap(['a', 'b', 'c'], -1, 0))\n\
         \tkeep = fn(i) array::put(start.rows, i, 'kept')\n\
         \tprintln(keep(1)[0..3])\n\
         \tprintln(start.rows[0..3])\n\
         \tjoined = if done.turn > 3 { array::put(done.rows, 0, 'first') } else { done.rows }\n\
         \tprintln(joined[0])\n\
         }\n",
        "14\nOk(\n  [x0, n2, n3, x3, n5, n6, x6]\n)\nOk(\n  [n1, n2, n3, n4, n5, n6, n7]\n)\n\
         Some(x39)\nSome(x0)\nSome(x39)\n[c, b, a]\n[a, b, c]\n[a, b, c]\n[a, b, c]\n\
         Ok(\n  [n1, kept, n3]\n)\nOk(\n  [n1, n2, n3]\n)\nSome(first)\n",
    );
}

/// A record update reads the fields it keeps as soon as its base has run,
/// before any field written after it. So in
/// `State(..s, v: array::put(s.v, i, x))` the read of `s.v` is the last use
/// of `s`, the record is given up before the write, and the write finds the
/// array with one holder. Each turn then makes the new `State` and no array
/// cell. Where the `..s` is written makes no difference, and neither does an
/// argument before the write that reads `s`, or reading the array into a
/// name of its own before the update, or taking the record apart with a
/// `match`. Neither does an update inside an `if` whose value is used.
///
/// One spelling copies the root and the nodes down to the slot at every write,
/// 24,984 cells with the 5,000 records. That is a rule: an argument written after
/// the write reads `s`, so `s` has to live past the write.
///
/// A record `main` built itself and has no more use for is the callee's once it
/// is passed, as any other is, and its array is written in place.
#[test]
fn a_record_update_gives_up_the_record_before_it_runs_the_new_fields() {
    prints(
        "import scarlet/array\n\
         import scarlet/internal\n\
         type State {\n\
         \tv Array(Int)\n\
         \tcount Int\n\
         \tcheck Int\n\
         }\n\
         fn build(n Int, acc Array(Int)) Array(Int) {\n\
         \tif n == 0 { acc } else { build(n - 1, [..acc, 0]) }\n\
         }\n\
         fn fresh(n Int) State { State(build(n, []), 0, 0) }\n\
         fn first(s State, i Int, n Int) State {\n\
         \tif i == n { s } else { first(State(..s, v: array::put(s.v, i, i * 3)), i + 1, n) }\n\
         }\n\
         fn last(s State, i Int, n Int) State {\n\
         \tif i == n { s } else { last(State(v: array::put(s.v, i, i * 3), ..s), i + 1, n) }\n\
         }\n\
         fn matched(s State, i Int, n Int) State {\n\
         \tif i == n {\n\
         \t\ts\n\
         \t} else {\n\
         \t\tmatch s {\n\
         \t\t\tState(v, count, check) -> matched(State(array::put(v, i, i * 3), count, check), i + 1, n)\n\
         \t\t}\n\
         \t}\n\
         }\n\
         fn earlier(s State, i Int, n Int) State {\n\
         \tif i == n { s } else { earlier(State(..s, count: s.count + 1, v: array::put(s.v, i, i * 3)), i + 1, n) }\n\
         }\n\
         fn named(s State, i Int, n Int) State {\n\
         \tif i == n {\n\
         \t\ts\n\
         \t} else {\n\
         \t\tv = s.v\n\
         \t\tnamed(State(..s, v: array::put(v, i, i * 3)), i + 1, n)\n\
         \t}\n\
         }\n\
         fn later(s State, i Int, n Int) State {\n\
         \tif i == n { s } else { later(State(..s, v: array::put(s.v, i, i * 3), count: s.count + 1), i + 1, n) }\n\
         }\n\
         fn joined(s State, i Int, n Int) State {\n\
         \tif i == n {\n\
         \t\ts\n\
         \t} else {\n\
         \t\tt = if i >= 0 { State(..s, v: array::put(s.v, i, i * 3)) } else { s }\n\
         \t\tjoined(t, i + 1, n)\n\
         \t}\n\
         }\n\
         fn sum(xs Array(Int), acc Int) Int {\n\
         \tmatch xs {\n\
         \t\t[] -> acc\n\
         \t\t[h, ..t] -> sum(t, acc + h)\n\
         \t}\n\
         }\n\
         fn cells(run fn(State, Int, Int) State) Int {\n\
         \ts = fresh(5000)\n\
         \tmade = internal::cells_made()\n\
         \ts = run(s, 0, 5000)\n\
         \tmade = internal::cells_made() - made\n\
         \tprintln(sum(s.v, 0) + s.count)\n\
         \tmade\n\
         }\n\
         pub fn main() {\n\
         \tprintln(cells(first))\n\
         \tprintln(cells(last))\n\
         \tprintln(cells(matched))\n\
         \tprintln(cells(earlier))\n\
         \tprintln(cells(named))\n\
         \tprintln(cells(later))\n\
         \tprintln(cells(joined))\n\
         \tkept = State(build(5000, []), 0, 0)\n\
         \tmade = internal::cells_made()\n\
         \tdone = first(kept, 0, 5000)\n\
         \tprintln(internal::cells_made() - made)\n\
         \tprintln(sum(done.v, 0))\n\
         \tprintln(State(..done, check: 1).check)\n\
         }\n",
        "37492500\n5000\n37492500\n5000\n37492500\n5000\n37497500\n5000\n37492500\n5000\n\
         37497500\n24984\n37492500\n5000\n5000\n37492500\n1\n",
    );
}

/// A record update never changes a record something else still holds. Here
/// `p`, `q` and `w` are each read after an update of them, so the write
/// under the update copies, and each keeps what it held. The kept fields
/// are cells too, and the audit at the end of the run finds every count
/// right. The last type holds `xs` in a different slot in each variant, so
/// the update reads it through a `match` on the variant.
#[test]
fn a_record_update_leaves_a_record_that_is_still_held_alone() {
    prints(
        "import scarlet/array\n\
         type Page {\n\
         \ttitle String\n\
         \tlines Array(String)\n\
         \ttags Array(String)\n\
         }\n\
         type Shape {\n\
         \tWide(pad Int, xs Array(Int), n Int)\n\
         \tThin(xs Array(Int), n Int)\n\
         }\n\
         fn retitle(p Page, i Int) Page {\n\
         \tPage(..p, title: 'page ${i}', lines: array::put(p.lines, 0, 'line ${i}'))\n\
         }\n\
         fn thin(s Shape) Shape { Thin(..s, n: array::length(s.xs)) }\n\
         pub fn main() {\n\
         \tp = Page('first', ['a', 'b'], ['x', 'y'])\n\
         \tq = retitle(p, 2)\n\
         \tr = Page(lines: [..q.lines, 'c'], ..q)\n\
         \tprintln(p)\n\
         \tprintln(q)\n\
         \tprintln(r)\n\
         \tprintln(retitle(retitle(Page('t', ['l'], []), 3), 4))\n\
         \tw = Wide(0, [1, 2, 3], 0)\n\
         \tprintln(thin(w))\n\
         \tprintln(thin(Thin([4, 5], 0)))\n\
         \tprintln(w)\n\
         }\n",
        "Page {\n  title: first,\n  lines: [a, b],\n  tags: [x, y]\n}\n\
         Page {\n  title: page 2,\n  lines: [line 2, b],\n  tags: [x, y]\n}\n\
         Page {\n  title: page 2,\n  lines: [line 2, b, c],\n  tags: [x, y]\n}\n\
         Page {\n  title: page 4,\n  lines: [line 4],\n  tags: []\n}\n\
         Thin(\n  [1, 2, 3],\n  3\n)\nThin(\n  [4, 5],\n  2\n)\nWide(\n  0,\n  [1, 2, 3],\n  0\n)\n",
    );
}

/// The base of a record update and the fields it is given run in the order
/// they are written, whichever slot each one fills, so what they print comes
/// out in that order. A base that is only a name has nothing to run.
#[test]
fn a_record_update_runs_its_parts_in_the_order_written() {
    prints(
        "type P {\n\
         \ta Int\n\
         \tb Int\n\
         \tc Int\n\
         }\n\
         fn say(name String, x Int) Int {\n\
         \tprintln(name)\n\
         \tx\n\
         }\n\
         fn base(name String) P {\n\
         \tprintln(name)\n\
         \tP(10, 20, 30)\n\
         }\n\
         pub fn main() {\n\
         \tprintln(P(a: say('a', 1), ..base('base'), c: say('c', 3)))\n\
         \tprintln(P(c: say('c', 3), a: say('a', 1), ..base('base')))\n\
         \tprintln(P(..base('base'), c: say('c', 3), a: say('a', 1)))\n\
         \tp = base('p')\n\
         \tprintln(P(c: say('c', 3), ..p, a: say('a', p.b)))\n\
         }\n",
        "a\nbase\nc\nP{ a: 1, b: 20, c: 3 }\n\
         c\na\nbase\nP{ a: 1, b: 20, c: 3 }\n\
         base\nc\na\nP{ a: 1, b: 20, c: 3 }\n\
         p\nc\na\nP{ a: 20, b: 20, c: 3 }\n",
    );
}

/// A field or a tuple element read off a name has nothing to run, as a name
/// has not, so a base such as `..o.inner` may be written anywhere. Each turn
/// makes the two records, or the tuple and the record, and no array cell,
/// where it made 29,984 cells when the kept fields were read last.
#[test]
fn a_nested_record_update_writes_in_place_wherever_its_spread_is() {
    prints(
        "import scarlet/array\n\
         import scarlet/internal\n\
         type Inner {\n\
         \tv Array(Int)\n\
         \ttag Int\n\
         }\n\
         type Outer {\n\
         \tinner Inner\n\
         \tcount Int\n\
         }\n\
         fn build(n Int, acc Array(Int)) Array(Int) {\n\
         \tif n == 0 { acc } else { build(n - 1, [..acc, 0]) }\n\
         }\n\
         fn fresh(n Int) Outer { Outer(Inner(build(n, []), 1), 0) }\n\
         fn start(n Int) (Int, Inner) { (0, Inner(build(n, []), 1)) }\n\
         fn first(o Outer, i Int, n Int) Outer {\n\
         \tif i == n {\n\
         \t\to\n\
         \t} else {\n\
         \t\tfirst(Outer(..o, inner: Inner(..o.inner, v: array::put(o.inner.v, i, i * 3))), i + 1, n)\n\
         \t}\n\
         }\n\
         fn last(o Outer, i Int, n Int) Outer {\n\
         \tif i == n {\n\
         \t\to\n\
         \t} else {\n\
         \t\tlast(Outer(inner: Inner(v: array::put(o.inner.v, i, i * 3), ..o.inner), ..o), i + 1, n)\n\
         \t}\n\
         }\n\
         fn pair(t (Int, Inner), i Int, n Int) (Int, Inner) {\n\
         \tif i == n { t } else { pair((t.0 + 1, Inner(v: array::put(t.1.v, i, i * 3), ..t.1)), i + 1, n) }\n\
         }\n\
         fn sum(xs Array(Int), acc Int) Int {\n\
         \tmatch xs {\n\
         \t\t[] -> acc\n\
         \t\t[h, ..t] -> sum(t, acc + h)\n\
         \t}\n\
         }\n\
         fn cells(run fn(Outer, Int, Int) Outer) Int {\n\
         \to = fresh(5000)\n\
         \tmade = internal::cells_made()\n\
         \to = run(o, 0, 5000)\n\
         \tmade = internal::cells_made() - made\n\
         \tprintln(sum(o.inner.v, 0) + o.inner.tag + o.count)\n\
         \tmade\n\
         }\n\
         pub fn main() {\n\
         \tprintln(cells(first))\n\
         \tprintln(cells(last))\n\
         \tt = start(5000)\n\
         \tmade = internal::cells_made()\n\
         \tt = pair(t, 0, 5000)\n\
         \tprintln(internal::cells_made() - made)\n\
         \tprintln(sum(t.1.v, 0) + t.0)\n\
         }\n",
        "37492501\n10000\n37492501\n10000\n10000\n37497500\n",
    );
}

/// A map held in a record is set in place the same way. In `by_key` the
/// record is given up before the set, and the set finds the map with one
/// holder: 1,000 sets of keys the map already has make the 1,000 records and
/// the 1,000 tasks, and no map cell.
///
/// `by_task` is the same, with a value that is a cell bound before the update.
/// Its drop comes after the set and before the map's.
#[test]
fn a_record_update_sets_a_map_it_holds_in_place() {
    prints(
        "import scarlet/internal\n\
         import scarlet/map\n\
         type Task {\n\
         \tidentity Int\n\
         \tnames Array(String)\n\
         }\n\
         type Sched {\n\
         \ttasks map::Map(Int, Task)\n\
         \theld Int\n\
         }\n\
         fn by_key(s Sched, k Int) Sched {\n\
         \tSched(..s, tasks: map::set(s.tasks, k, Task(k, [])))\n\
         }\n\
         fn by_task(s Sched, t Task) Sched {\n\
         \tSched(..s, tasks: map::set(s.tasks, t.identity, t))\n\
         }\n\
         fn keyed(s Sched, i Int, n Int) Sched {\n\
         \tif i == n { s } else { keyed(by_key(s, i % 16), i + 1, n) }\n\
         }\n\
         fn tasked(s Sched, i Int, n Int) Sched {\n\
         \tif i == n { s } else { tasked(by_task(s, Task(i % 16, [])), i + 1, n) }\n\
         }\n\
         fn cells(run fn(Sched, Int, Int) Sched) Int {\n\
         \ts = run(Sched(map::new(), 0), 0, 16)\n\
         \tmade = internal::cells_made()\n\
         \ts = run(s, 16, 1016)\n\
         \tmade = internal::cells_made() - made\n\
         \tprintln(s.held + map::size(s.tasks))\n\
         \tmade\n\
         }\n\
         pub fn main() {\n\
         \tprintln(cells(keyed))\n\
         \tprintln(cells(tasked))\n\
         }\n",
        "16\n2000\n16\n2000\n",
    );
}

/// A record that nothing else holds is updated where it is, whatever the new
/// field is. So each turn here makes one cell, and the heap grows by none.
#[test]
fn a_record_that_nothing_else_holds_is_updated_where_it_is() {
    let src = |field: &str| {
        format!(
            "type P {{\n\
             \ta Int\n\
             \tb Int\n\
             \tc Int\n\
             }}\n\
             fn pick(i Int) Int {{\n\
             \tif i > 5 {{ 1 }} else {{ 2 }}\n\
             }}\n\
             fn spin(i Int, acc Int) Int {{\n\
             \tif i == 0 {{\n\
             \t\tacc\n\
             \t}} else {{\n\
             \t\tt = P(i, i, i)\n\
             \t\tu = P(..t, c: {field})\n\
             \t\tspin(i - 1, acc + u.c + u.a)\n\
             \t}}\n\
             }}\n\
             pub fn main() {{\n\
             \tprintln(spin(N, 0))\n\
             }}\n"
        )
    };
    for field in ["pick(i)", "if i > 5 { 1 } else { 2 }"] {
        let src = src(field);
        let (made, words) = cells_and_words(&src, 1, "3\n");
        assert_eq!(
            cells_and_words(&src, 10000, "50015005\n"),
            (made + 9999, words),
            "{field}"
        );
    }
}

/// What an update makes and writes, for each way that a record is held: cells
/// for a thousand updates. One that nothing else holds is written where it is,
/// and so is one out of a call, out of a match that took apart what held it, and
/// one that an arm has shown to be its variant. Any other is copied, and what was
/// copied is as it was. Of two records, one in the other, the outer is made again
/// in the space of the old one and the inner is written where it is.
#[test]
fn an_update_writes_a_record_in_place_when_nothing_else_holds_it() {
    let src = |step: &str| {
        format!(
            "import scarlet/internal\n\
             type P {{\n\ta Int\n\tb String\n\tc Array(Int)\n\td Float\n}}\n\
             type O {{\n\tp P\n\tn Int\n}}\n\
             fn made(n Int) P {{ P(n, 'b', [n], 0.5) }}\n\
             fn step(n Int, o O) O {{\n\t{step}\n}}\n\
             fn out(n Int, o O) O {{ if n == 0 {{ o }} else {{ out(n - 1, step(n, o)) }} }}\n\
             pub fn main() {{\n\
             \to = out(1, O(made(0), 0))\n\
             \tcells = internal::cells_made()\n\
             \tq = out(1000, o)\n\
             \tprintln(internal::cells_made() - cells)\n\
             \tprintln('${{q.p.a}} ${{q.p.b}} ${{q.p.c}} ${{q.p.d}} ${{q.n}}')\n\
             }}\n"
        )
    };
    for (step, cells, shown) in [
        ("O(..o, n: n)", 0, "0 b [0] 0.5 1"),
        ("O(..o, n: o.n + o.p.a + n)", 0, "0 b [0] 0.5 500501"),
        ("O(..o, p: P(..o.p, a: n))", 1000, "1 b [0] 0.5 0"),
        (
            "match o {\n O(p, k) -> O(P(..p, a: n, d: 1.5), k + 1)\n }",
            1000,
            "1 b [0] 1.5 1001",
        ),
        ("O(P(..made(n), a: 7), o.n + 1)", 5000, "7 b [1] 0.5 1001"),
        ("O(P(..o.p, a: n), o.n)", 2000, "1 b [0] 0.5 0"),
    ] {
        prints(&src(step), &format!("{cells}\n{shown}\n"));
    }
    // A record that something else holds is copied and left alone.
    prints(
        "import scarlet/internal\n\
         type P {\n\ta Int\n\tb String\n\tc Array(Int)\n}\n\
         fn go(n Int, p P, acc Int) Int {\n\
         \tif n == 0 {\n\t\tacc\n\t} else {\n\
         \t\tq = P(..p, a: n)\n\
         \t\tgo(n - 1, p, acc + q.a + p.a)\n\t}\n\
         }\n\
         pub fn main() {\n\
         \tp = P(1, 'b', [2])\n\
         \tcells = internal::cells_made()\n\
         \tprintln(go(1000, p, 0))\n\
         \tprintln(internal::cells_made() - cells)\n\
         \tprintln(p.a)\n\
         }\n",
        "501500\n1000\n1\n",
    );
    // A variant that an arm has shown.
    prints(
        "import scarlet/internal\n\
         type V {\n\tA(x Int, xs Array(Int), k Int)\n\tB(xs Array(Int), x Int, k Int)\n}\n\
         fn go(n Int, v V) V {\n\
         \tif n == 0 {\n\t\tv\n\t} else {\n\
         \t\tmatch v {\n\
         \t\t\tA(x, ..) -> go(n - 1, A(..v, x: x + 1))\n\
         \t\t\tB(_, x, _) -> go(n - 1, B(..v, x: x + 2))\n\t\t}\n\t}\n\
         }\n\
         pub fn main() {\n\
         \tv = go(1, A(0, [5], 3))\n\
         \tw = go(1, B([6], 0, 3))\n\
         \tcells = internal::cells_made()\n\
         \tx = go(1000, v)\n\
         \ty = go(1000, w)\n\
         \tprintln(internal::cells_made() - cells)\n\
         \tprintln(x)\n\
         \tprintln(y)\n\
         }\n",
        "0\nA(\n  1001,\n  [5],\n  3\n)\nB(\n  [6],\n  2002,\n  3\n)\n",
    );
}

/// A record may be set in one of its own fields. It is copied, so nothing holds
/// itself, and the audit at the end of the run finds every cell freed.
#[test]
fn a_record_set_in_itself_holds_the_old_one() {
    prints(
        "type W {\n\tinner Option(W)\n\tn Int\n\tm Int\n}\n\
         fn wrap(w W) W { W(..w, inner: Some(w)) }\n\
         fn over(w W, n Int) W { if n == 0 { w } else { over(W(..w, inner: Some(w), n: n), n - 1) } }\n\
         fn depth(w W, acc Int) Int {\n\
         \tmatch w.inner {\n\
         \t\tSome(i) -> depth(i, acc + 1)\n\
         \t\tNone -> acc\n\t}\n\
         }\n\
         pub fn main() {\n\
         \tw = W(None, 1, 2)\n\
         \tprintln(depth(wrap(wrap(w)), 0))\n\
         \tprintln(depth(over(w, 100), 0))\n\
         \tprintln(depth(w, 0))\n\
         }\n",
        "2\n100\n0\n",
    );
}

/// A record too wide for a function to read whole is updated all the same, in the
/// arena, at its end and in a box, where nothing else holds it and where something
/// does. The audit before each instruction finds every count right.
#[test]
fn a_record_of_any_width_is_updated() {
    for n in [200, 253, 254, 300, 1000] {
        let last = n - 1;
        let fields: String = (0..n).map(|i| format!("\tf{i} Int\n")).collect();
        let args = format!("a, {}b", "m, ".repeat(n - 2));
        let many: String = (1..60).map(|i| format!(", f{i}: x")).collect();
        let src = format!(
            "import scarlet/internal\n\
             type Wide {{\n{fields}}}\n\
             fn wide(a Int, m Int, b Int) Wide {{ Wide({args}) }}\n\
             fn ends(w Wide) Int {{ w.f0 * 1000 + w.f1 * 100 + w.f{last} }}\n\
             fn one(w Wide, x Int) Wide {{ Wide(..w, f0: x) }}\n\
             fn two(w Wide, x Int) Wide {{ Wide(..w, f{last}: x, f0: x + 1) }}\n\
             fn many(w Wide, x Int) Wide {{ Wide(..w{many}) }}\n\
             fn spin(w Wide, n Int) Wide {{\n\
             \tif n == 0 {{ w }} else {{ spin(many(two(one(w, 3), 5), 8), n - 1) }}\n\
             }}\n\
             pub fn main() {{\n\
             \tw = wide(1, 0, 2)\n\
             \tmade = internal::cells_made()\n\
             \tv = spin(w, 50)\n\
             \tprintln(internal::cells_made() - made)\n\
             \tprintln(ends(w))\n\
             \tprintln(ends(v))\n\
             \tmade = internal::cells_made()\n\
             \tprintln(ends(many(w, 4)) + ends(two(w, 4)) + ends(w))\n\
             \tprintln(internal::cells_made() - made)\n\
             }}\n"
        );
        let mut host = OsHost::new(Vec::new(), Vec::new(), Vec::new());
        let ran = scarlet_vm::run(&compile(&src), &mut host, Audit::EachInstruction);
        assert_eq!(ran, Ok(()), "{n}");
        let out = String::from_utf8(std::mem::take(host.out())).expect("UTF-8");
        assert_eq!(out, "1\n1002\n6805\n7408\n2\n", "{n}");
    }
}

/// Runs a program that connects to a listener here, holds the connection in
/// a record, and updates that record with `update`. The new field connects
/// again and waits for a byte before it returns. Says whether the first
/// connection had ended when the second arrived, waiting up to `wait` for
/// its end.
fn first_connection_ends_before_the_new_field_runs(
    update: &str,
    wait: std::time::Duration,
) -> bool {
    use std::io::{Read, Write};
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
    let port = listener.local_addr().expect("addr").port();
    let peer = std::thread::spawn(move || {
        let (mut first, _) = listener.accept().expect("accept");
        let (mut second, _) = listener.accept().expect("accept");
        first.set_read_timeout(Some(wait)).expect("timeout");
        let ended = matches!(first.read(&mut [0u8; 1]), Ok(0));
        second.write_all(b"x").expect("write");
        ended
    });
    let src = format!(
        "import scarlet/net\n\
         import scarlet/net/socket\n\
         type Held {{\n\
         \tconn Option(socket::Socket)\n\
         \tport Int\n\
         }}\n\
         type Both(a, b) {{\n\
         \tBoth(x a, y b)\n\
         }}\n\
         fn dial(port Int) Option(socket::Socket) {{\n\
         \tmatch net::connect('127.0.0.1', port) {{\n\
         \t\tOk(sock) -> Some(sock)\n\
         \t\tErr(_) -> None\n\
         \t}}\n\
         }}\n\
         fn redial(port Int) Option(socket::Socket) {{\n\
         \tmatch dial(port) {{\n\
         \t\tSome(sock) -> match socket::read(sock, 1) {{\n\
         \t\t\tOk(_) -> Some(sock)\n\
         \t\t\tErr(_) -> None\n\
         \t\t}}\n\
         \t\tNone -> None\n\
         \t}}\n\
         }}\n\
         fn start(port Int) Held {{ Held(dial(port), port) }}\n\
         fn second(both Both(a, Int)) Held {{\n\
         \tmatch both {{\n\
         \t\tBoth(_, port) -> Held(redial(port), port)\n\
         \t}}\n\
         }}\n\
         fn within(h Held) Option(socket::Socket) {{ redial(h.port) }}\n\
         fn kept(v a) a {{ v }}\n\
         fn turn(x Option(a), port Int) Option(socket::Socket) {{\n\
         \tmatch x {{\n\
         \t\tNone -> redial(port)\n\
         \t\tSome(y) -> {{\n\
         \t\t\tz = kept(y)\n\
         \t\t\tif z == z {{ turn(None, port) }} else {{ None }}\n\
         \t\t}}\n\
         \t}}\n\
         }}\n\
         fn leave(x a, port Int) Option(socket::Socket) {{\n\
         \tz = kept(x)\n\
         \tif z == z {{ redial(port) }} else {{ None }}\n\
         }}\n\
         fn chosen(b Bool, _x a, port Int) Option(socket::Socket) {{\n\
         \tc = kept(b)\n\
         \tnew = redial(port)\n\
         \tif c {{ new }} else {{ None }}\n\
         }}\n\
         fn loose(x a, port Int) Option(socket::Socket) {{\n\
         \ty = kept(x)\n\
         \tc = if y == y {{ 1 }} else {{ 2 }}\n\
         \tnew = redial(port)\n\
         \tif c == 1 {{ new }} else {{ None }}\n\
         }}\n\
         fn replace(_x a, y a, n Int, port Int) Option(socket::Socket) {{\n\
         \tif n == 0 {{ redial(port) }} else {{ replace(y, y, n - 1, port) }}\n\
         }}\n\
         fn trade(x a, y a, n Int, port Int) Option(socket::Socket) {{\n\
         \tif n == 0 {{\n\
         \t\tnew = redial(port)\n\
         \t\tif x == y {{ None }} else {{ new }}\n\
         \t}} else {{ trade(y, x, n - 1, port) }}\n\
         }}\n\
         fn swap(h Held) Held {{\n\
         \t{update}\n\
         }}\n\
         pub fn main() {{\n\
         \tmatch swap(start({port})).conn {{\n\
         \t\tSome(_) -> println('open')\n\
         \t\tNone -> println('failed')\n\
         \t}}\n\
         }}\n"
    );
    prints(&src, "open\n");
    peer.join().expect("peer")
}

/// A record update gives the old record up at its last read, and a program
/// can see that when the record held the last reference to a handle. In
/// `Held(..h, conn: redial(h.port))` the read of `h.port` is the last use
/// of `h`, so the connection `h` held is closed before `redial` runs, and
/// the peer reads its end before the second connection sends anything. A
/// base that is only a name may be written last and the same happens.
#[test]
fn a_record_update_closes_a_handle_only_the_old_record_held_before_the_new_field_runs() {
    let wait = std::time::Duration::from_secs(10);
    assert!(first_connection_ends_before_the_new_field_runs(
        "Held(..h, conn: redial(h.port))",
        wait
    ));
    assert!(first_connection_ends_before_the_new_field_runs(
        "Held(conn: redial(h.port), ..h)",
        wait
    ));
}

/// An update that holds its base closes a connection in a field it sets as it
/// sets it, and leaves open one in a field it keeps, whatever the new fields are.
/// A connection that something else holds stays open.
#[test]
fn an_update_closes_a_handle_in_a_field_it_sets() {
    let ends = |update: &str, wait: u64| {
        first_connection_ends_before_the_new_field_runs(
            update,
            std::time::Duration::from_millis(wait),
        )
    };
    assert!(ends(
        "gone = Held(..h, conn: None)\n\tHeld(redial(gone.port), 1)",
        10_000
    ));
    assert!(ends(
        "gone = Held(..Held(..h, conn: None), port: h.port)\n\tHeld(redial(gone.port), 1)",
        10_000
    ));
    assert!(!ends(
        "kept = Held(..h, port: h.port + 0)\n\tnew = redial(kept.port)\n\
         \tmatch kept.conn {\n\t\tSome(_) -> Held(new, 1)\n\t\tNone -> kept\n\t}",
        200
    ));
    assert!(!ends(
        "gone = Held(..h, conn: None)\n\tnew = redial(gone.port)\n\
         \tmatch h.conn {\n\t\tSome(_) -> Held(new, 1)\n\t\tNone -> gone\n\t}",
        200
    ));
}

/// A field written after the new one that reads the old record keeps it,
/// and what it holds, until then. So the first connection is still open
/// while `redial` runs, and the peer's read of it runs out of time.
#[test]
fn a_record_update_keeps_a_handle_a_later_field_still_reads() {
    assert!(!first_connection_ends_before_the_new_field_runs(
        "Held(..h, conn: redial(h.port), port: h.port + 1)",
        std::time::Duration::from_millis(200),
    ));
}

/// A match that has no more use for what it matched gives up the fields it
/// leaves out there and then, so a connection in one is closed before the arm
/// runs. It is the same where the function does not know what type the field
/// has.
#[test]
fn a_match_closes_a_handle_in_a_field_it_leaves_out_before_the_arm_runs() {
    let wait = std::time::Duration::from_secs(10);
    assert!(first_connection_ends_before_the_new_field_runs(
        "match h {\n\t\tHeld(_, port) -> Held(redial(port), port)\n\t}",
        wait
    ));
    assert!(first_connection_ends_before_the_new_field_runs(
        "second(Both(h.conn, h.port))",
        wait
    ));
}

/// A field a match binds and reads stays open until that read, and so does one
/// that something else still holds.
#[test]
fn a_match_keeps_a_handle_that_is_read_or_held_after() {
    let wait = std::time::Duration::from_millis(200);
    assert!(!first_connection_ends_before_the_new_field_runs(
        "match h {\n\t\tHeld(conn, port) -> {\n\
         \t\t\tnew = redial(port)\n\
         \t\t\tmatch conn {\n\
         \t\t\t\tSome(_) -> Held(new, port)\n\
         \t\t\t\tNone -> Held(None, 0)\n\
         \t\t\t}\n\t\t}\n\t}",
        wait
    ));
    assert!(!first_connection_ends_before_the_new_field_runs(
        "new = match h {\n\t\tHeld(_, port) -> redial(port)\n\t}\n\
         \tHeld(..h, conn: new)",
        wait
    ));
}

/// A value that a function made and has no more use for is the callee's once it
/// is passed, so a connection in it is closed when the callee has no more use
/// for it.
#[test]
fn a_call_closes_a_handle_in_a_value_its_caller_is_done_with() {
    assert!(first_connection_ends_before_the_new_field_runs(
        "held = Held(h.conn, h.port)\n\tnew = within(held)\n\tHeld(new, 1)",
        std::time::Duration::from_secs(10),
    ));
}

/// A tail call gives up what its turn is done with: a value that a name of its
/// own held, of a type the function does not know, a parameter that it writes
/// over, and what it does not pass to another function. A connection in one is
/// closed before the next turn, or the callee, runs. What is passed on is not.
#[test]
fn a_tail_call_closes_a_handle_the_turn_is_done_with() {
    let ends = |call: &str, wait: u64| {
        first_connection_ends_before_the_new_field_runs(
            &format!("held = h.conn\n\tport = h.port\n\tHeld({call}, 1)"),
            std::time::Duration::from_millis(wait),
        )
    };
    assert!(ends("turn(held, port)", 10_000));
    assert!(ends("leave(held, port)", 10_000));
    assert!(ends("replace(held, None, 1, port)", 10_000));
    assert!(ends("replace(held, None, 2, port)", 10_000));
    assert!(!ends("trade(held, None, 3, port)", 200));
}

/// A handle is closed at its last use where that is inside a branch whose value is
/// used, and where what holds it has a type the function does not know: in a name
/// that the branch makes, in one from outside that only the branch reads, in one
/// that only the other branch reads, in what a match in a branch takes apart and in
/// the parameter of a join. What is read after the branch stays open.
#[test]
fn a_handle_is_closed_at_its_last_use_inside_a_branch() {
    let ends = |new: &str, wait: u64| {
        first_connection_ends_before_the_new_field_runs(
            &format!("held = h.conn\n\tport = h.port\n\tnew = {new}\n\tHeld(new, 1)"),
            std::time::Duration::from_millis(wait),
        )
    };
    let closed = [
        "chosen(True, held, port)",
        "loose(held, port)",
        "if port > 0 {\n\t\tx = kept(held)\n\t\tif x == x { redial(port) } else { None }\n\t\
         } else { None }",
        "if port > 0 && held != None { redial(port) } else { None }",
        "if port < 0 { held } else { redial(port) }",
        "match held {\n\t\tSome(_) -> redial(port)\n\t\tNone -> None\n\t}",
        "match Both(held, port) {\n\t\tBoth(Some(_), p) if p > 0 -> redial(p)\n\t\t\
         Both(Some(_), _) | Both(None, _) -> None\n\t}",
        "if held != None { redial(port) } else { held }",
        "match { if port > 0 { held } else { None } } {\n\t\tSome(_) -> redial(port)\n\t\t\
         None -> None\n\t}",
        "{\n\t\ty = if port > 0 { kept(held) } else { None }\n\t\t\
         if y == y { redial(port) } else { None }\n\t}",
    ];
    for new in closed {
        assert!(ends(new, 10_000), "{new}");
    }
    let open = "{\n\t\tn = if port > 0 { redial(port) } else { held }\n\t\t\
                if held == None { held } else { n }\n\t}";
    assert!(!ends(open, 200));
}

/// The listing shows a write with the array it takes marked, as it marks an
/// argument a call takes, and where the write continues when it misses.
#[test]
fn the_listing_shows_a_write_and_where_a_miss_goes() {
    let src = "import scarlet/array\n\
               fn strike(flags Array(Bool), k Int) Array(Bool) {\n\
               \tarray::put(flags, k - 1, False)\n\
               }\n\
               pub fn main() {\n\
               \tprintln(strike([True, True], 2))\n\
               }\n";
    let listing = scarlet_vm::listing(&compile(src)).expect("the program loads");
    // `array::swap` has two writes of its own, and comes first.
    let strike = (0..)
        .map_while(|i| listing.function(i))
        .filter(|f| f.contains("ArraySet"))
        .last()
        .expect("strike is listed");
    let write = "    2  r2 = ArraySet(move r2, r3, move r4) -> 4\n";
    let ends = "    3  Ret(move r2)\n    4  Ret(move r2)\n";
    assert!(strike.ends_with(&format!("{write}{ends}")), "{strike}");
}

/// A range may have more elements than a 64-bit number with a sign can count.
#[test]
fn the_longest_range_has_its_length() {
    prints(
        "import scarlet/array\n\
         fn kind(xs Array(Int)) String {\n\
         \tmatch xs {\n\
         \t\t[] -> 'none'\n\
         \t\t[_] -> 'one'\n\
         \t\t_ -> 'more'\n\
         \t}\n\
         }\n\
         pub fn main() {\n\
         \tlow = -9223372036854775807 - 1\n\
         \thigh = 9223372036854775807\n\
         \tprintln(array::length(low..high))\n\
         \tprintln(array::length(low..0))\n\
         \tprintln(array::length(0..high))\n\
         \tprintln(array::length(high..low))\n\
         \tprintln(kind(low..high))\n\
         \tprintln(kind(high..low))\n\
         \tprintln(kind({ high - 1 }..high))\n\
         }\n",
        "18446744073709551615\n9223372036854775808\n9223372036854775807\n0\nmore\nnone\none\n",
    );
}

/// The empty array is a constant in an instruction, and it is listed as it is written.
#[test]
fn the_listing_shows_the_empty_array() {
    let src = "fn none(n Int) Array(Int) { if n == 0 { [] } else { [n] } }\n\
               pub fn main() {\n\
               \tprintln(none(0))\n\
               }\n";
    let listing = scarlet_vm::listing(&compile(src)).expect("the program loads");
    let none = listing.function(0).expect("none is listed");
    assert!(none.contains("r2 = Const([])\n"), "{none}");
    assert_eq!(none.matches("Array").count(), 1, "{none}");
}

/// Nothing that gives an empty array makes a cell for it, and what is grown from one
/// makes its own two cells and no more.
#[test]
fn the_empty_array_makes_no_cell() {
    let made = |what: &str| {
        let src = format!(
            "import scarlet/array\n\
             import scarlet/internal\n\
             import scarlet/map\n\
             import scarlet/string\n\
             fn rest(xs Array(Int)) Array(Int) {{\n\
             \tmatch xs {{\n\
             \t\t[_, ..t] -> t\n\
             \t\t[] -> xs\n\
             \t}}\n\
             }}\n\
             fn drop(xs Array(Int), n Int) Array(Int) {{\n\
             \tif n == 0 {{ xs }} else {{ drop(rest(xs), n - 1) }}\n\
             }}\n\
             fn build(n Int, acc Array(Int)) Array(Int) {{\n\
             \tif n == 0 {{ acc }} else {{ build(n - 1, [..acc, n]) }}\n\
             }}\n\
             fn made(xs Array(Int), five Array(Int), m map::Map(Int, Int), n Int) Int {{\n\
             \tstart = internal::cells_made()\n\
             \tgot = {what}\n\
             \tend = internal::cells_made()\n\
             \tif array::length(got) == 0 && xs == [] && array::length(five) == 5 && map::size(m) == 0 && n == 3 {{\n\
             \t\tend - start\n\
             \t}} else {{\n\
             \t\t-1\n\
             \t}}\n\
             }}\n\
             pub fn main() {{\n\
             \tprintln(made([], build(5, []), map::new(), 3))\n\
             }}\n"
        );
        run(&src).expect("the program runs")
    };
    let nothing = [
        "[]",
        "xs",
        "n..n",
        "n..2",
        "n..n - 5",
        "{ five[2..2] or five }",
        "{ five[5..5] or five }",
        "{ xs[0..0] or five }",
        "[..xs, ..xs]",
        "[..xs, ..n..n]",
        "rest(xs)",
        "array::put(xs, 0, 1)",
        "array::swap(xs, 0, 0)",
        "array::reverse(xs)",
        "map::keys(m)",
        "map::values(m)",
    ];
    for what in nothing {
        assert_eq!(made(what), "0\n", "{what}");
    }
    // The array that is taken apart has two cells, one of two has two more for what is
    // left of it, and a range has one. A `filter` and a `map` make a closure for their
    // loop.
    for (what, cells) in [
        ("rest([n])", 2),
        ("rest(n..n + 1)", 1),
        ("drop([n, n], 2)", 4),
        ("drop(build(1, []), 1)", 2),
        ("array::filter(xs, fn(x) x > 1)", 1),
        ("array::map(xs, fn(x) x + 1)", 1),
    ] {
        assert_eq!(made(what), format!("{cells}\n"), "{what}");
    }
}

/// An array that is built from the empty one, at either end, and walked back to it makes
/// cells in proportion to its length, whether or not something else holds it meanwhile.
#[test]
fn a_build_from_the_empty_array_and_a_walk_back_are_linear() {
    let made = |n: usize, push: &str, held: bool| {
        let src = format!(
            "import scarlet/internal\n\
             fn build(n Int, acc Array(Int), before Array(Int)) Array(Int) {{\n\
             \tif n == 0 {{ if before == [0] {{ [] }} else {{ acc }} }} else {{\n\
             \t\tbuild(n - 1, {push}, {})\n\
             \t}}\n\
             }}\n\
             fn walk(xs Array(Int), whole Array(Int), n Int) Int {{\n\
             \tmatch xs {{\n\
             \t\t[] -> if whole == [0] {{ 0 - n }} else {{ n }}\n\
             \t\t[_, ..t] -> walk(t, whole, n + 1)\n\
             \t}}\n\
             }}\n\
             pub fn main() {{\n\
             \tstart = internal::cells_made()\n\
             \txs = build({n}, [], [])\n\
             \tprintln(walk(xs, {}, 0) == {n})\n\
             \tprintln(internal::cells_made() - start)\n\
             }}\n",
            if held { "acc" } else { "[]" },
            if held { "xs" } else { "[]" },
        );
        let out = run(&src).expect("the program runs");
        let cells = out.strip_prefix("True\n").expect("every element");
        cells.trim().parse::<usize>().expect("a number")
    };
    for push in ["[..acc, n]", "[n, ..acc]"] {
        for held in [false, true] {
            let (few, many) = (made(1000, push, held), made(4000, push, held));
            assert!(many <= few * 4 + few / 2, "{push} {held}: {few} {many}");
        }
    }
}

/// A range stores only its two ends, and is an array of Ints in every way a
/// program can see. It can be shown, indexed, walked, sliced, spread and
/// joined.
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
         \tprintln(string::inspect(Some([1, 2])))\n\
         \tprintln(string::inspect('as is'))\n\
         \tprintln(string::length('héllo'))\n\
         \tprintln(string::length(''))\n\
         \tprintln(array::length([1, 2, 3]))\n\
         \tprintln(array::length(0..1000000000000))\n\
         \tprintln(int::to_string(-42))\n\
         \tprintln(int::to_string(9223372036854775807 + 1))\n\
         \tprintln(array::map([1, 2, 3], fn(x) { x * 10 }))\n\
         \tprintln(array::reverse([1, 2, 3]))\n\
         }\n",
        "Some(\n  [1, 2]\n)\nas is\n5\n0\n3\n1000000000000\n-42\n9223372036854775808\n[10, 20, 30]\n[3, 2, 1]\n",
    );
}

/// Floats follow `docs/semantics.md`, so there is never a NaN or an infinity.
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
         \tprintln(float::floor(-2.5))\n\
         \tprintln(float::round(2.5))\n\
         \tprintln(float::truncate(-2.9))\n\
         \tprintln(float::floor(big(1.0, 20)))\n\
         \tprintln(float::from_int(3))\n\
         \tprintln(float::to_string(2.0))\n\
         }\n",
        &format!(
            "3.75\n1.0\n0.30000000000000004\n-10.0\n0.0\n0.0\n7.5\n-1.5\n\
             {max}.0\n-{max}.0\nTrue\nTrue\nTrue\n2.5\n6\n-2.5\nSome(1.5)\n-3\n3\n-2\n\
             100000000000000000000\n3.0\n2.0\n",
            max = f64::MAX
        ),
    );
}

/// Binary literals of every segment kind, shown as the old VM showed them
/// and taken apart by patterns, and the core of `scarlet/binary`.
#[test]
fn binaries_are_built_shown_and_matched() {
    prints(
        "import scarlet/binary\n\
         fn describe(b Binary) String {\n\
         \tmatch b {\n\
         \t\t<<1, rest:binary>> -> 'one then ${rest}'\n\
         \t\t<<n:size(16), _:binary>> -> 'a 16-bit ${n}'\n\
         \t\t_ -> 'something else'\n\
         \t}\n\
         }\n\
         fn first_char(b Binary) Int {\n\
         \tmatch b {\n\
         \t\t<<c:utf8, _:binary>> -> c\n\
         \t\t_ -> -1\n\
         \t}\n\
         }\n\
         pub fn main() {\n\
         \tprintln(<<1, 2, 3>>)\n\
         \tprintln(<<>>)\n\
         \tprintln(<<5:size(3)>>)\n\
         \tprintln(<<255, 5:size(3)>>)\n\
         \tprintln(<<-1:size(12)>>)\n\
         \tprintln(<<'hi':utf8>>)\n\
         \tprintln(<<1:size(4), 2:size(4)>>)\n\
         \tprintln(describe(<<1, 7, 9>>))\n\
         \tprintln(describe(<<2, 1>>))\n\
         \tprintln(describe(<<2>>))\n\
         \tprintln(first_char(<<'é':utf8>>))\n\
         \tprintln(first_char(<<255>>))\n\
         \tprintln(<<1, 2>> == <<1, 2>>)\n\
         \tprintln(<<1, 2>> == <<1, 2, 0>>)\n\
         \tprintln(binary::bit_size(<<1, 5:size(3)>>))\n\
         \tprintln(binary::byte_size(<<1, 5:size(3)>>))\n\
         \tprintln(binary::to_string(binary::from_string('héllo')))\n\
         \tprintln(binary::to_string(<<255>>))\n\
         \tprintln(binary::append(<<1>>, <<2, 3>>))\n\
         \tprintln(binary::slice_bits(<<1, 2, 3>>, 8, 16))\n\
         \tprintln(binary::slice_bits(<<1, 2, 3>>, 8, 17))\n\
         \tprintln(binary::slice_bits(<<1, 2, 3>>, -1, 8))\n\
         \tprintln(binary::slice_bytes(<<1, 2, 3>>, 1, 1) == Ok(<<2>>))\n\
         \tprintln(binary::drop_bytes(<<1, 2, 3>>, 2))\n\
         }\n",
        "<<1, 2, 3>>\n<<>>\n<<5:size(3)>>\n<<255, 5:size(3)>>\n<<255, 15:size(4)>>\n<<104, 105>>\n<<18>>\n\
         one then <<7, 9>>\na 16-bit 513\nsomething else\n233\n-1\nTrue\nFalse\n11\n2\n\
         Ok(héllo)\nErr(Nil)\n<<1, 2, 3>>\nOk(<<2, 3>>)\nErr(Nil)\nErr(Nil)\nTrue\n<<3>>\n",
    );
}

/// The rest of `scarlet/binary`, with its edge cases.
#[test]
fn the_rest_of_the_binary_module_runs() {
    prints(
        "import scarlet/binary\n\
         pub fn main() {\n\
         \tb = <<'hello':utf8>>\n\
         \tprintln(binary::byte_at(b, 0))\n\
         \tprintln(binary::byte_at(b, 4))\n\
         \tprintln(binary::byte_at(b, 5))\n\
         \tprintln(binary::byte_at(b, -1))\n\
         \tprintln(binary::byte_at(<<1, 5:size(3)>>, 1))\n\
         \tprintln(binary::index_of(b, <<'l':utf8>>, 0))\n\
         \tprintln(binary::index_of(b, <<'l':utf8>>, 3))\n\
         \tprintln(binary::index_of(b, <<'z':utf8>>, 0))\n\
         \tprintln(binary::index_of(b, <<>>, 2))\n\
         \tprintln(binary::index_of(b, <<'h':utf8>>, 99))\n\
         \tprintln(binary::parse_int(<<'1234':utf8>>, binary::Dec))\n\
         \tprintln(binary::parse_int(<<'ff':utf8>>, binary::Hex))\n\
         \tprintln(binary::parse_int(<<'FF':utf8>>, binary::Hex))\n\
         \tprintln(binary::parse_int(<<'12a':utf8>>, binary::Dec))\n\
         \tprintln(binary::parse_int(<<>>, binary::Dec))\n\
         \tprintln(binary::parse_int(<<'-1':utf8>>, binary::Dec))\n\
         \tprintln(binary::parse_int(<<'99999999999999999999':utf8>>, binary::Dec))\n\
         \tprintln(binary::eq_ignore_ascii_case(<<'Content-Length':utf8>>, <<'content-length':utf8>>))\n\
         \tprintln(binary::eq_ignore_ascii_case(<<'ab':utf8>>, <<'abc':utf8>>))\n\
         \tprintln(binary::to_ascii_lower(<<'HeLLo':utf8>>) == <<'hello':utf8>>)\n\
         \tprintln(binary::from_int_ascii(255, binary::Hex) == <<'ff':utf8>>)\n\
         \tprintln(binary::from_int_ascii(-42, binary::Dec) == <<'-42':utf8>>)\n\
         }\n",
        "104\n111\n-1\n-1\n-1\nSome(2)\nSome(3)\nNone\nSome(2)\nNone\n\
         Ok(1234)\nOk(255)\nOk(255)\nErr(Nil)\nErr(Nil)\nErr(Nil)\nOk(99999999999999999999)\n\
         True\nFalse\nTrue\nTrue\nTrue\n",
    );
}

#[test]
fn the_string_built_ins_run() {
    prints(
        "import scarlet/int\n\
         import scarlet/string\n\
         pub fn main() {\n\
         \tprintln(string::split('a,b,,c', ','))\n\
         \tprintln(string::split('abc', ''))\n\
         \tprintln(string::split('abc', 'x'))\n\
         \tprintln(string::split('', ','))\n\
         \tprintln(string::contains('hello', 'ell'))\n\
         \tprintln(string::contains('hello', ''))\n\
         \tprintln(string::contains('hello', 'z'))\n\
         \tprintln('[${string::trim('  hi \\t\\n')}]')\n\
         \tprintln(string::to_graphemes('e\\u{0301}a'))\n\
         \tprintln(int::from_string('-42'))\n\
         \tprintln(int::from_string('+007'))\n\
         \tprintln(int::from_string('20O'))\n\
         \tprintln(int::from_string(' 1'))\n\
         \tprintln(int::from_string('-'))\n\
         \tprintln(int::from_string('123456789012345678901234567890'))\n\
         }\n",
        "[a, b, , c]\n[a, b, c]\n[abc]\n[]\nTrue\nTrue\nFalse\n[hi]\n[e\u{0301}, a]\n\
         Ok(-42)\nOk(7)\nErr(Nil)\nErr(Nil)\nErr(Nil)\nOk(123456789012345678901234567890)\n",
    );
}

/// Bitwise operations treat an Int as an endless row of two's-complement
/// bits, as `int.sca` says. No bit ever falls off an end.
#[test]
fn bitwise_operations_are_any_size() {
    prints(
        "import scarlet/binary\n\
         import scarlet/int\n\
         pub fn main() {\n\
         \tprintln(int::bitwise_and(12, 10))\n\
         \tprintln(int::bitwise_or(12, 10))\n\
         \tprintln(int::bitwise_xor(12, 10))\n\
         \tprintln(int::bitwise_not(0))\n\
         \tprintln(int::bitwise_not(5))\n\
         \tprintln(int::bitwise_and(-1, 255))\n\
         \tprintln(int::bitwise_shift_left(1, 64))\n\
         \tprintln(int::bitwise_shift_left(3, -1))\n\
         \tprintln(int::bitwise_shift_right(-8, 1))\n\
         \tprintln(int::bitwise_shift_right(-7, 1))\n\
         \tprintln(int::bitwise_shift_right(5, 100))\n\
         \tprintln(int::bitwise_shift_right(-5, 100))\n\
         \tprintln(int::bitwise_shift_right(1, -3))\n\
         \tbig = int::bitwise_shift_left(1, 100)\n\
         \tprintln(int::bitwise_and(big + 5, 7))\n\
         \tprintln(int::bitwise_shift_right(big, 99))\n\
         \tprintln(int::bitwise_and(int::bitwise_shift_right(-1, 4), int::bitwise_shift_left(1, 28) - 1))\n\
         \tprintln(binary::hex_byte(13) == <<'0D':utf8>>)\n\
         \tprintln(binary::hex_byte(255) == <<'FF':utf8>>)\n\
         }\n",
        "8\n14\n6\n-1\n-6\n255\n18446744073709551616\n1\n-4\n-4\n0\n-1\n8\n5\n2\n268435455\nTrue\nTrue\n",
    );
}

#[test]
fn the_map_built_ins_run() {
    prints(
        "import scarlet/map\n\
         pub fn main() {\n\
         \tm = map::set(map::set(map::new(), 'a', 1), 'b', 2)\n\
         \tprintln(map::get(m, 'a'))\n\
         \tprintln(map::get(m, 'z'))\n\
         \tprintln(map::has(m, 'b'))\n\
         \tprintln(map::size(m))\n\
         \tn = map::set(m, 'a', 10)\n\
         \tprintln('${map::get(n, 'a')} ${map::get(m, 'a')} ${map::size(n)}')\n\
         \tgone = map::delete(m, 'a')\n\
         \tprintln('${map::has(gone, 'a')} ${map::has(m, 'a')} ${map::size(gone)}')\n\
         \tprintln(map::size(map::delete(m, 'nope')))\n\
         \tprintln(gone)\n\
         \tprintln(map::keys(gone))\n\
         \tprintln(map::values(gone))\n\
         \tprintln(map::to_list(gone))\n\
         \tprintln(map::new())\n\
         \tprintln(map::delete(gone, 'b'))\n\
         }\n",
        "Some(1)\nNone\nTrue\n2\nSome(10) Some(1) 2\nFalse True 1\n2\n{b: 2}\n[b]\n[2]\n[\n  (b, 2)\n]\n{}\n{}\n",
    );
}

/// A map's order depends on its entries alone, so two maps that are `==`
/// list them the same, however they were built.
#[test]
fn equal_maps_are_in_the_same_order() {
    prints(
        "import scarlet/map\n\
         import scarlet/array\n\
         fn fill(m map::Map(Int, Int), from Int, to Int, step Int) map::Map(Int, Int) {\n\
         \tif from == to { m } else { fill(map::set(m, from, from * 2), from + step, to, step) }\n\
         }\n\
         pub fn main() {\n\
         \tup = fill(map::new(), 0, 500, 1)\n\
         \tdown = fill(map::new(), 499, -1, -1)\n\
         \tprintln(up == down)\n\
         \tprintln(map::keys(up) == map::keys(down))\n\
         \tprintln('${up}' == '${down}')\n\
         \tfewer = map::delete(map::set(down, 1000, 0), 1000)\n\
         \tprintln(fewer == up)\n\
         \tprintln(map::to_list(fewer) == map::to_list(up))\n\
         \tprintln(up == map::set(up, 7, 0))\n\
         \tprintln(up == map::delete(up, 7))\n\
         \tprintln(array::length(map::keys(up)))\n\
         }\n",
        "True\nTrue\nTrue\nTrue\nTrue\nFalse\nFalse\n500\n",
    );
}

/// A key is found by any value `==` to it, however that value was made.
#[test]
fn a_key_is_found_by_any_equal_value() {
    prints(
        "import scarlet/map\n\
         import scarlet/binary\n\
         import scarlet/int\n\
         type P { P(x Int, y String) }\n\
         fn one(key k, look k) Option(String) { map::get(map::set(map::new(), key, 'found'), look) }\n\
         pub fn main() {\n\
         \tprintln(one(0..3, [0, 1, 2]))\n\
         \tprintln(one(0.0, -0.0))\n\
         \tprintln(match binary::slice_bits(<<1, 2, 3>>, 8, 16) {\n\
         \t\tOk(b) -> one(<<2, 3>>, b)\n\
         \t\tErr(Nil) -> None\n\
         \t})\n\
         \tprintln(one(int::bitwise_shift_left(1, 100), int::bitwise_shift_left(2, 99)))\n\
         \tprintln(one((1, 'a'), (1, 'a')))\n\
         \tprintln(one(P(1, 'p'), P(1, 'p')))\n\
         \tprintln(one('abc', 'ab${'c'}'))\n\
         \tinner = map::set(map::set(map::new(), 1, 'one'), 2, 'two')\n\
         \tprintln(one(inner, map::set(map::set(map::new(), 2, 'two'), 1, 'one')))\n\
         \tprintln(one(0..3, [0, 1]))\n\
         \tprintln(one(P(1, 'p'), P(1, 'q')))\n\
         \tprintln(one(inner, map::delete(inner, 1)))\n\
         }\n",
        "Some(found)\nSome(found)\nSome(found)\nSome(found)\nSome(found)\nSome(found)\nSome(found)\nSome(found)\nNone\nNone\nNone\n",
    );
}

/// `os.argv` and `os.env` are what the host says. When the OS lists a name
/// twice, the first value wins, as it does for `getenv`.
#[test]
fn argv_and_env_come_from_the_host() {
    let env = [("HOME", "/home/al"), ("LANG", "C"), ("HOME", "/elsewhere")];
    prints_in(
        vec!["main.sca".into(), "--fast".into(), "two words".into()],
        env.iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect(),
        "import scarlet/os\n\
         import scarlet/map\n\
         pub fn main() {\n\
         \tprintln(os::argv())\n\
         \tprintln(os::get_env('HOME'))\n\
         \tprintln(os::get_env('LANG'))\n\
         \tprintln(os::get_env('NOT_SET'))\n\
         \tprintln(map::size(os::env()))\n\
         \tprintln(os::env() == os::env())\n\
         }\n",
        "[main.sca, --fast, two words]\nSome(/home/al)\nSome(C)\nNone\n2\nTrue\n",
    );
}

/// A file written is the file read back, and each way a path can fail is the
/// `IoError` that names it, holding the path.
#[test]
fn files_read_and_write_and_fail_with_their_error() {
    let dir = scratch("files");
    let file = dir.join("a.txt");
    std::fs::write(dir.join("plain"), "x").expect("a plain file");
    let src = "import scarlet/io\n\
         fn say(r Result(a, io::IoError)) String {\n\
         \tmatch r {\n\
         \t\tOk(v) -> 'ok ${v}'\n\
         \t\tErr(io::NotFound(p)) -> 'not found ${p}'\n\
         \t\tErr(io::IsADirectory(p)) -> 'a directory ${p}'\n\
         \t\tErr(io::NotADirectory(p)) -> 'not a directory ${p}'\n\
         \t\tErr(io::InvalidData(p)) -> 'not text ${p}'\n\
         \t\tErr(io::UnalignedBinary) -> 'not whole bytes'\n\
         \t\tErr(io::PermissionDenied(_) | io::AlreadyExists(_) | io::ReadOnlyFilesystem(_) | io::FilesystemLoop(_) | io::StorageFull | io::QuotaExceeded | io::FileTooLarge(_) | io::Errno(_)) -> 'another error'\n\
         \t}\n\
         }\n\
         pub fn main() {\n\
         \tprintln(say(io::write_text('DIR/a.txt', 'hello')))\n\
         \tprintln(say(io::read_text('DIR/a.txt')))\n\
         \tprintln(say(io::write_file('DIR/a.txt', <<104, 105>>)))\n\
         \tprintln(say(io::read_file('DIR/a.txt')))\n\
         \tprintln(say(io::write_file('DIR/b.txt', <<1:4>>)))\n\
         \tprintln(say(io::read_file('DIR/missing')))\n\
         \tprintln(say(io::read_file('DIR')))\n\
         \tprintln(say(io::write_text('DIR/no/such/dir', 'x')))\n\
         \tprintln(say(io::read_file('DIR/plain/under')))\n\
         \tprintln(say(io::write_file('DIR/x.bin', <<255>>)))\n\
         \tprintln(say(io::read_text('DIR/x.bin')))\n\
         }\n"
    .replace("DIR", &dir.display().to_string());
    let d = dir.display();
    let want = format!(
        "ok Nil\nok hello\nok Nil\nok <<104, 105>>\nnot whole bytes\n\
         not found {d}/missing\na directory {d}\nnot found {d}/no/such/dir\n\
         not a directory {d}/plain/under\nok Nil\nnot text {d}/x.bin\n"
    );
    prints_in(Vec::new(), Vec::new(), &src, &want);
    assert_eq!(std::fs::read(&file).expect("written"), b"hi");
    assert!(
        !dir.join("b.txt").exists(),
        "an unaligned write made a file"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// The monotonic clock never goes back, and the wall clock says it is after
/// 2023. Random bytes come in the number asked for, and a negative number
/// is an error.
#[test]
fn the_clocks_and_random_bytes_run() {
    prints(
        "import scarlet/time\n\
         import scarlet/crypto\n\
         import scarlet/binary\n\
         pub fn main() {\n\
         \tstart = time::monotonic()\n\
         \tprintln(time::since_ms(time::monotonic(), start) >= 0)\n\
         \tprintln(time::epoch_ms() > 1700000000000)\n\
         \tprintln(match crypto::random_bytes(33) {\n\
         \t\tOk(b) -> binary::byte_size(b)\n\
         \t\tErr(Nil) -> -1\n\
         \t})\n\
         \tprintln(crypto::random_bytes(0))\n\
         \tprintln(crypto::random_bytes(-1))\n\
         \tprintln(crypto::random_bytes(16) == crypto::random_bytes(16))\n\
         }\n",
        "True\nTrue\n33\nOk(<<>>)\nErr(Nil)\nFalse\n",
    );
}

/// `bytes` written as a literal like `<<1, 2, 3>>`, to put them into a
/// program's source.
fn bytes_literal(bytes: &[u8]) -> String {
    let parts: Vec<String> = bytes.iter().map(u8::to_string).collect();
    format!("<<{}>>", parts.join(", "))
}

fn unhex(s: &str) -> Vec<u8> {
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).expect("hex"))
        .collect()
}

/// The digests and the HMAC tag give the published answers (FIPS 180-2's
/// `abc`, RFC 4231's second case), and a binary that is not whole bytes has
/// none. `const_eq` compares bits. The old VM dropped a last partial byte, so
/// `<<1, 2:4>>` and `<<1, 3:4>>` were equal. Signatures made here by aws-lc
/// check out, and anything of the wrong shape does not.
#[test]
fn the_crypto_built_ins_run() {
    use aws_lc_rs::rand::SystemRandom;
    use aws_lc_rs::signature::{
        ECDSA_P256_SHA256_ASN1_SIGNING, EcdsaKeyPair, Ed25519KeyPair, KeyPair,
    };
    let rng = SystemRandom::new();
    let p256 = EcdsaKeyPair::generate(&ECDSA_P256_SHA256_ASN1_SIGNING).expect("a P-256 key");
    let p256_sig = p256.sign(&rng, b"signed").expect("a P-256 signature");
    let ed_doc = Ed25519KeyPair::generate_pkcs8(&rng).expect("an Ed25519 key");
    let ed = Ed25519KeyPair::from_pkcs8(ed_doc.as_ref()).expect("an Ed25519 key");
    let ed_sig = ed.sign(b"signed");
    let src = "import scarlet/crypto\n\
         import scarlet/binary\n\
         pub fn main() {\n\
         \tprintln(crypto::sha256(<<'abc'>>) == Ok(SHA256_ABC))\n\
         \tprintln(crypto::sha512(<<'abc'>>) == Ok(SHA512_ABC))\n\
         \tprintln(crypto::sha1(<<'abc'>>) == Ok(SHA1_ABC))\n\
         \tprintln(crypto::hmac_sha256(<<'Jefe'>>, <<'what do ya want for nothing?'>>) == Ok(HMAC_JEFE))\n\
         \tprintln(crypto::sha256(<<1:4>>))\n\
         \tprintln(crypto::sha512(<<97, 1:4>>))\n\
         \tprintln(crypto::hmac_sha256(<<1:4>>, <<>>))\n\
         \tprintln(crypto::hmac_sha256(<<>>, <<1:4>>))\n\
         \tprintln(crypto::const_eq(<<1, 2, 3>>, <<1, 2, 3>>))\n\
         \tprintln(crypto::const_eq(<<1, 2, 3>>, <<1, 2, 4>>))\n\
         \tprintln(crypto::const_eq(<<1, 2>>, <<1, 2, 3>>))\n\
         \tprintln(crypto::const_eq(<<1, 2:4>>, <<1, 3:4>>))\n\
         \tprintln(crypto::const_eq(<<1, 2:4>>, <<1, 2:4>>))\n\
         \tprintln(crypto::const_eq(<<1:4>>, <<16>>))\n\
         \tprintln(crypto::p256_verify(P256_KEY, <<'signed'>>, P256_SIG))\n\
         \tprintln(crypto::p256_verify(P256_KEY, <<'signeD'>>, P256_SIG))\n\
         \tprintln(crypto::p256_verify(<<4, 1, 2>>, <<'signed'>>, P256_SIG))\n\
         \tprintln(crypto::p256_verify(P256_KEY, <<'signed', 1:4>>, P256_SIG))\n\
         \tprintln(crypto::ed25519_verify(ED_KEY, <<'signed'>>, ED_SIG))\n\
         \tprintln(crypto::ed25519_verify(ED_KEY, <<'signeD'>>, ED_SIG))\n\
         \tprintln(crypto::ed25519_verify(ED_KEY, <<'signed'>>, <<1, 2, 3>>))\n\
         \tprintln(crypto::ed25519_verify(ED_KEY, <<'signed'>>, P256_SIG))\n\
         }\n"
        .replace(
            "SHA256_ABC",
            &bytes_literal(&unhex(
                "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad",
            )),
        )
        .replace(
            "SHA512_ABC",
            &bytes_literal(&unhex(
                "ddaf35a193617abacc417349ae20413112e6fa4e89a97ea20a9eeee64b55d39a\
                 2192992a274fc1a836ba3c23a3feebbd454d4423643ce80e2a9ac94fa54ca49f",
            )),
        )
        .replace(
            "SHA1_ABC",
            &bytes_literal(&unhex("a9993e364706816aba3e25717850c26c9cd0d89d")),
        )
        .replace(
            "HMAC_JEFE",
            &bytes_literal(&unhex(
                "5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843",
            )),
        )
        .replace("P256_KEY", &bytes_literal(p256.public_key().as_ref()))
        .replace("P256_SIG", &bytes_literal(p256_sig.as_ref()))
        .replace("ED_KEY", &bytes_literal(ed.public_key().as_ref()))
        .replace("ED_SIG", &bytes_literal(ed_sig.as_ref()));
    prints(
        &src,
        "True\nTrue\nTrue\nTrue\nErr(Nil)\nErr(Nil)\nErr(Nil)\nErr(Nil)\n\
         True\nFalse\nFalse\nFalse\nTrue\nFalse\n\
         True\nFalse\nFalse\nFalse\n\
         True\nFalse\nFalse\nFalse\n",
    );
}

/// A parsed document is read in place, including fields, elements, each kind
/// of scalar, and an integer past `i64::MAX`, which an `Int` holds exactly. A
/// `Doc` shows as what it points at, never the document. A `Json` tree
/// encodes, and a `Number` that is not a number becomes `null`.
#[test]
fn the_json_built_ins_run() {
    prints(
        "import scarlet/json\n\
         pub fn main() {\n\
         \tsrc = '{\"id\": 18446744073709551615, \"n\": -3, \"x\": 0.5, \"s\": \"a\\\\u00e9\", \"t\": [true, null], \"o\": {}}'\n\
         \tmatch json::parse(src) {\n\
         \t\tOk(d) -> {\n\
         \t\t\tprintln('${d} ${json::size(d)}')\n\
         \t\t\tprintln(json::int(json::field(d, 'id') or d))\n\
         \t\t\tprintln(json::int_text(json::field(d, 'n') or d))\n\
         \t\t\tprintln(json::float(json::field(d, 'x') or d))\n\
         \t\t\tprintln(json::string(json::field(d, 's') or d))\n\
         \t\t\tt = json::field(d, 't') or d\n\
         \t\t\tprintln(json::bool(json::index(t, 0) or d))\n\
         \t\t\tprintln(json::is_null(json::index(t, 1) or d))\n\
         \t\t\tprintln(json::index(t, 2))\n\
         \t\t\tprintln(json::field(d, 'missing'))\n\
         \t\t\tprintln(json::size(json::field(d, 'o') or d))\n\
         \t\t\tprintln(json::reencode(d))\n\
         \t\t}\n\
         \t\tErr(_) -> println('did not parse')\n\
         \t}\n\
         \tprintln(json::encode(json::List([json::Null, json::Boolean(True), json::Integer(-7), json::Real(1.0), json::Str('a\"b'), json::Object([('k', json::Number('1e9'))]), json::Number('x')])))\n\
         \tprintln(match json::parse('[1,]') {\n\
         \t\tOk(_) -> 'parsed'\n\
         \t\tErr(_) -> 'refused'\n\
         \t})\n\
         }\n",
        "<json object#0> 6\nSome(18446744073709551615)\nSome(-3)\nSome(0.5)\nSome(aé)\n\
         Some(True)\nTrue\nNone\nNone\n0\n\
         {\"id\":18446744073709551615,\"n\":-3,\"x\":0.5,\"s\":\"aé\",\"t\":[true,null],\"o\":{}}\n\
         [null,true,-7,1.0,\"a\\\"b\",{\"k\":1e9},null]\nrefused\n",
    );
}

/// A Float literal pattern matches as `==` does, so `-0.0` is `0.0`.
#[test]
fn a_float_pattern_matches_as_equality_does() {
    prints(
        "fn name(x Float) String {\n\
         \tmatch x {\n\
         \t\t0.0 -> 'zero'\n\
         \t\t1.5 -> 'one and a half'\n\
         \t\t_ -> 'other'\n\
         \t}\n\
         }\n\
         pub fn main() {\n\
         \tprintln(name(0.0))\n\
         \tprintln(name(-0.0))\n\
         \tprintln(name(1.5))\n\
         \tprintln(name(1.25 + 0.25))\n\
         \tprintln(name(2.0))\n\
         }\n",
        "zero\nzero\none and a half\none and a half\nother\n",
    );
}

/// A name's init reads the binding the name had before it, whether that
/// binding is in an outer block of the same body or in an enclosing function.
#[test]
fn a_rebinding_init_reads_the_binding_it_shadows() {
    prints(
        "pub fn main() {\n\
         \tx = 5\n\
         \tg = fn() {\n\
         \t\tx = x + 1\n\
         \t\tx\n\
         \t}\n\
         \ty = if x > 1 {\n\
         \t\tx = x + 10\n\
         \t\tx\n\
         \t} else { 0 }\n\
         \tprintln(g())\n\
         \tprintln(y)\n\
         \tprintln(x)\n\
         }\n",
        "6\n15\n5\n",
    );
}

/// A segment's size reads the pattern's names bound left of it, and an outer
/// binding for a name the pattern only binds later.
#[test]
fn a_segment_size_sees_only_the_names_bound_before_it() {
    prints(
        "pub fn main() {\n\
         \tn = 1\n\
         \tr = match <<2, 2, 1, 9>> {\n\
         \t\t<<a:bytes(n), n:8, b:bytes(n)>> -> {\n\
         \t\t\tprintln(a)\n\
         \t\t\tprintln(b)\n\
         \t\t\tn\n\
         \t\t}\n\
         \t\t_ -> 0\n\
         \t}\n\
         \tprintln(r)\n\
         }\n",
        "<<2>>\n<<1, 9>>\n2\n",
    );
}

/// A segment's size is read before the segment's own value, so a size naming
/// the segment's own name reads the binding that name had outside.
#[test]
fn a_segment_size_naming_its_own_value_reads_the_outer_binding() {
    prints(
        "pub fn main() {\n\
         \tx = 8\n\
         \tr = match <<1, 2>> {\n\
         \t\t<<x:size(x), ..>> -> x\n\
         \t\t_ -> 0\n\
         \t}\n\
         \tprintln(r)\n\
         }\n",
        "1\n",
    );
}

/// A backpass binds a pattern: `Nil` for a step that returns nothing, a tuple
/// taken apart in place.
#[test]
fn a_backpass_binds_a_pattern() {
    prints(
        "import scarlet/result\n\
         fn check(n Int) Result(Nil, String) {\n\
         \tif n > 0 { Ok(Nil) } else { Err('bad') }\n\
         }\n\
         fn pair(n Int) Result((Int, Int), String) {\n\
         \tOk((n, n + 1))\n\
         }\n\
         fn run(n Int) Result(Int, String) {\n\
         \tNil <- result::then(check(n))\n\
         \t(a, b) <- result::then(pair(n))\n\
         \tOk(a + b)\n\
         }\n\
         pub fn main() {\n\
         \tprintln(run(3))\n\
         \tprintln(run(0))\n\
         }\n",
        "Ok(7)\nErr(bad)\n",
    );
}

/// A connection nothing holds any more is closed, even though the program
/// never called `socket.close`: the peer reads the end of the stream.
#[test]
fn a_dropped_connection_is_closed() {
    use std::io::Read;
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
    let port = listener.local_addr().expect("addr").port();
    let peer = std::thread::spawn(move || {
        let (mut conn, _) = listener.accept().expect("accept");
        conn.set_read_timeout(Some(std::time::Duration::from_secs(10)))
            .expect("timeout");
        let mut buf = [0u8; 8];
        conn.read(&mut buf).expect("the stream ends, not times out")
    });
    let src = format!(
        "import scarlet/net\n\
         import scarlet/net/address\n\
         import scarlet/result\n\
         fn open() Nil {{\n\
         \tmatch net::connect('127.0.0.1', {port}) {{\n\
         \t\tOk(sock) -> println('connected to ${{address::to_string(sock.peer)}}')\n\
         \t\tErr(e) -> println('failed: ${{e}}')\n\
         \t}}\n\
         }}\n\
         pub fn main() {{\n\
         \topen()\n\
         \tprintln(result::map(net::resolve('::1'), address::ip_to_string))\n\
         \tprintln(address::parse('nope'))\n\
         }}\n"
    );
    prints(
        &src,
        &format!("connected to 127.0.0.1:{port}\nOk(::1)\nErr(Nil)\n"),
    );
    assert_eq!(peer.join().expect("peer"), 0);
}

const FIB: &str = "fn fib(n Int) Int {\n\
                   \tif n < 2 { n } else { fib(n - 1) + fib(n - 2) }\n\
                   }\n\
                   pub fn main() {\n\
                   \tprintln(fib(10))\n\
                   }\n";

fn counted(src: &str) -> (String, scarlet_vm::Counts) {
    let mut host = OsHost::new(Vec::new(), Vec::new(), Vec::new());
    let counts = scarlet_vm::run_counted(&compile(src), &mut host, Audit::AtEnd);
    let out = String::from_utf8(std::mem::take(host.out())).expect("UTF-8");
    (out, counts.expect("the program runs to its end"))
}

fn ran(counts: &scarlet_vm::Counts, op: &str) -> u64 {
    let found = counts.ops.iter().find(|(name, _)| *name == op);
    found
        .unwrap_or_else(|| panic!("no instruction called {op}"))
        .1
}

/// A counted run prints what an ordinary one does, and counts each
/// instruction under its kind. `fib(10)` is entered 177 times, once from
/// `main` and 176 times from itself, and 89 of those return `n` as it is.
#[test]
fn a_counted_run_counts_each_instruction_by_its_kind() {
    let (out, counts) = counted(FIB);
    assert_eq!(out, "55\n");
    assert_eq!(ran(&counts, "Call1"), 177);
    assert_eq!(ran(&counts, "CallSelf"), 0);
    assert_eq!(ran(&counts, "JumpUnlessIntLtK"), 177);
    assert_eq!(ran(&counts, "IntAdd"), 88);
    assert_eq!(ran(&counts, "Print"), 1);
    assert_eq!(ran(&counts, "Bad"), 0);
    assert_eq!(counts.cells_live, 0);
}

/// A lookup that hits and whose value nothing reads puts it nowhere, whatever its type,
/// so nothing is dropped.
#[test]
fn a_found_value_that_nothing_reads_is_put_nowhere() {
    for (values, ty) in [
        ("n", "Int"),
        ("n * 1000000 * 1000000 * 1000000", "Int"),
        ("[n]", "Array(Int)"),
    ] {
        let src = format!(
            "import scarlet/map\n\
             fn fill(m map::Map(Int, v), n Int, of fn(Int) v) map::Map(Int, v) {{\n\
             \tif n == 0 {{ m }} else {{ fill(map::set(m, n, of(n)), n - 1, of) }}\n\
             }}\n\
             fn go(n Int, m map::Map(Int, {ty}), x Int) Int {{\n\
             \tif n == 0 {{ x }} else {{\n\
             \t\tgo(n - 1, m, x + match map::get(m, n) {{\n\
             \t\t\tSome(_) -> 1\n\
             \t\t\tNone -> 0\n\
             \t\t}})\n\
             \t}}\n\
             }}\n\
             pub fn main() {{ println(go(2000, fill(map::new(), 1000, fn(n Int) {{ {values} }}), 0)) }}\n",
        );
        let (out, counts) = counted(&src);
        assert_eq!(out, "1000\n", "{values}");
        assert_eq!(counts.cells_live, 0, "{values}");
        assert_eq!(ran(&counts, "MapGet"), 2000, "{values}");
        // `fill` gives up what `of` gave it.
        assert_eq!(ran(&counts, "Drop"), 1000, "{values}");
    }
}

/// Where every case of a switch goes to one place, what is there gives up what the
/// value holds, though one of the cases is for a constructor that holds nothing.
#[test]
fn cases_that_come_to_one_place_give_up_the_value() {
    let src = "type A {\n\tA0(f Option(Int))\n\tA1\n}\n\
        type Rec {\n\tid Int\n\tname String\n}\n\
        fn say(k Int, s String) String {\n\t'${k}${s}'\n}\n\
        fn f(v (A, String, Int, Rec)) String {\n\
        \tmatch v {\n\
        \t\t(A0(_) | A1, 'a' | 'b' | 'c' | 'd', n | n, Rec(m | m | m, _)) if n > 4 -> say(m, 'big')\n\
        \t\t(_, 'a', _, Rec(k, 'z')) -> say(k, 'one')\n\
        \t\t(A0(_) | A1, _, _, Rec(_, _)) -> say(0, 'other')\n\
        \t}\n}\n\
        pub fn main() {\n\
        \tprintln(f((A0(Some(1)), 'a', 9, Rec(7, 'y'))))\n\
        \tprintln(f((A0(Some(1)), 'a', 3, Rec(1, 'z'))))\n\
        \tprintln(f((A0(Some(1)), 'e', 3, Rec(7, 'y'))))\n\
        \tprintln(f((A1, 'e', 3, Rec(7, 'y'))))\n}\n";
    let (out, counts) = counted(src);
    assert_eq!(out, "7big\n1one\n0other\n0other\n");
    assert_eq!(counts.cells_live, 0);
}

/// The counts are a property of the program, so a second run gives the same
/// ones.
#[test]
fn a_counted_run_counts_the_same_every_time() {
    let src = "type L {\n\tCons(h Int, t L)\n\tEnd\n}\n\
               fn build(n Int, acc L) L {\n\
               \tif n == 0 { acc } else { build(n - 1, Cons(n, acc)) }\n\
               }\n\
               pub fn main() {\n\
               \tprintln(build(3, End))\n\
               }\n";
    let (out, first) = counted(src);
    assert_eq!(
        out,
        "Cons(\n  1,\n  Cons(\n    2,\n    Cons(3, End)\n  )\n)\n"
    );
    assert_eq!(counted(src).1, first);
    assert_eq!(ran(&first, "Ctor2"), 3);
    // Each turn but the last ends in a jump back to the first.
    assert_eq!(ran(&first, "Jump"), 3);
    assert_eq!(ran(&first, "JumpUnlessIntEqK"), 4);
    assert!(first.cells_made >= 3, "{}", first.cells_made);
    // Word 0 and the three cells.
    assert!(first.heap_words > 3 * 3, "{}", first.heap_words);
}

/// A program that makes, shares, changes and frees a cell of every kind it
/// can without the network or two keys with one hash, in loops, calls,
/// closures and matches.
const EVERY_KIND: &str = "import scarlet/array\n\
    import scarlet/map\n\
    type L {\n\tCons(h String, t L)\n\tEnd\n}\n\
    type P {\n\tP(name String, xs Array(Int), n Int)\n}\n\
    const big = 9223372036854775807\n\
    const kept = ['a', 'b']\n\
    fn build(n Int, acc L) L {\n\
    \tif n == 0 { acc } else { build(n - 1, Cons('${n}', acc)) }\n\
    }\n\
    fn flip(l L, acc L) L {\n\
    \tmatch l {\n\t\tCons(h, t) -> flip(t, Cons(h, acc))\n\t\tEnd -> acc\n\t}\n\
    }\n\
    fn push(n Int, xs Array(Int)) Array(Int) {\n\
    \tif n == 0 { xs } else { push(n - 1, [..xs, n]) }\n\
    }\n\
    fn walk(xs Array(Int), acc Int) Int {\n\
    \tmatch xs {\n\t\t[h, ..t] -> walk(t, acc + h)\n\t\t[] -> acc\n\t}\n\
    }\n\
    fn fill(n Int, m map::Map(String, Int)) map::Map(String, Int) {\n\
    \tif n == 0 { m } else { fill(n - 1, map::set(m, 'k${n}', n)) }\n\
    }\n\
    fn drain(n Int, m map::Map(String, Int)) map::Map(String, Int) {\n\
    \tif n == 0 { m } else { drain(n - 1, map::delete(m, 'k${n}')) }\n\
    }\n\
    fn bump(p P, n Int) P {\n\
    \tif n == 0 { p } else { bump(P(..p, xs: array::put(p.xs, n, n), n: p.n + n), n - 1) }\n\
    }\n\
    fn adder(s String) fn(Int) String {\n\tfn(n) { '${s}${n}' }\n}\n\
    fn over(n Int, acc Int) Int {\n\
    \tif n == 0 { acc } else { over(n - 1, { acc + big } - big) }\n\
    }\n\
    fn tail(b Binary) Binary {\n\
    \tmatch b {\n\t\t<<1, rest:binary>> -> rest\n\t\t_ -> b\n\t}\n\
    }\n\
    pub fn main() {\n\
    \tl = build(5, End)\n\
    \tprintln(flip(l, End))\n\
    \tprintln(l == build(5, End))\n\
    \txs = push(40, [])\n\
    \tprintln(walk(xs, 0))\n\
    \tprintln(walk([..xs, ..xs], 0))\n\
    \tprintln(xs[2..4])\n\
    \tm = fill(20, map::new())\n\
    \tprintln(map::size(drain(15, m)))\n\
    \tprintln(map::get(m, 'k3'))\n\
    \tprintln(bump(P('p', [0, 0, 0, 0], 0), 3))\n\
    \tf = adder('n')\n\
    \tprintln((f(1), f(2)))\n\
    \tprintln(over(3, 7))\n\
    \tb = tail(<<1, 2, 'x'>>)\n\
    \tprintln((b, b == <<2, 'x'>>))\n\
    \tprintln(kept)\n\
    }\n";

/// A run audited before every instruction prints what an ordinary run
/// prints, and counts what a counted run counts: the audits are not part of
/// the program.
#[test]
fn a_run_audited_before_every_instruction_is_the_same_run() {
    let mut host = OsHost::new(Vec::new(), Vec::new(), Vec::new());
    let program = compile(EVERY_KIND);
    assert_eq!(
        scarlet_vm::run(&program, &mut host, Audit::EachInstruction),
        Ok(())
    );
    let out = String::from_utf8(std::mem::take(host.out())).expect("UTF-8");
    let (plain, counts) = counted(EVERY_KIND);
    assert_eq!(out, plain);
    assert!(out.starts_with("Cons(\n"), "{out}");
    assert!(out.ends_with("(<<2, 120>>, True)\n[a, b]\n"), "{out}");
    let each = scarlet_vm::run_counted(&program, &mut host, Audit::EachInstruction);
    assert_eq!(each, Ok(counts.clone()));
    assert!(!counts.ops.iter().any(|(name, _)| name.starts_with("Audit")));
}

/// Values of every kind of type, each read out of a cell, an array, a map and a
/// tuple, given by a call, passed through a function value, and made in a join:
/// enums with no fields and with some, Floats, Bools, `Nil`, Ints small and big,
/// recursive and generic types and one that holds itself at another type,
/// functions, and generic code given each.
const EVERY_TYPE: &str = "import scarlet/array\n\
    import scarlet/map\n\
    type Shade {\n\tDark\n\tLight\n}\n\
    pub opaque type One {\n\tOne\n}\n\
    type Mixed {\n\tPlain\n\tHeld(n Int)\n\tOther\n}\n\
    type Tree {\n\tLeaf\n\tNode(l Tree, shade Shade, n Int, r Tree)\n}\n\
    type Proxy(a) {\n\tProxy\n\tLost\n}\n\
    type Box(a) {\n\tBox(value a)\n}\n\
    type Nest(a) {\n\tFlat(x a)\n\tDeep(inner Nest((a, a)))\n}\n\
    type All {\n\tAll(a Shade, b One, c Mixed, d Float, e Bool, f Nil, g Int, h Option(Shade), i Proxy(Int), j fn(Shade) Shade)\n}\n\
    fn same(x a) a { x }\n\
    fn twice(f fn(a) a, x a) a { f(f(x)) }\n\
    fn pick(c Bool, x a, y a) a {\n\
    \tz = if c { x } else { same(y) }\n\
    \tw = if c && twice(fn(b) { !b }, c) { z } else { if c { y } else { z } }\n\
    \tsame(w)\n}\n\
    fn out(b Box(a)) a { b.value }\n\
    fn first(xs Array(a), or_else a) a { xs[0] or or_else }\n\
    fn depth(n Nest(a)) Int {\n\
    \tmatch n {\n\t\tFlat(_) -> 0\n\t\tDeep(Flat(_)) -> 1\n\t\tDeep(Deep(_)) -> 2\n\t}\n}\n\
    fn deepen(x a) Nest(a) { Deep(Deep(Flat(((x, x), (x, x))))) }\n\
    fn through(x a, n Int) a {\n\
    \tif n == 0 { x } else { out(Box(pick(n % 2 == 0, through(x, n - 1), first([x, x], x)))) }\n}\n\
    fn flip(s Shade) Shade {\n\tmatch s {\n\t\tDark -> Light\n\t\tLight -> Dark\n\t}\n}\n\
    fn add(t Tree, n Int) Tree {\n\
    \tmatch t {\n\
    \t\tLeaf -> Node(Leaf, if n % 2 == 0 { Dark } else { Light }, n, Leaf)\n\
    \t\tNode(l, s, m, r) -> if n < m { Node(add(l, n), flip(s), m, r) } else { Node(l, s, m, add(r, n)) }\n\
    \t}\n}\n\
    fn shades(t Tree, acc Array(Shade)) Array(Shade) {\n\
    \tmatch t {\n\t\tLeaf -> acc\n\t\tNode(l, s, _, r) -> shades(r, [..shades(l, acc), s])\n\t}\n}\n\
    fn mixed(m Mixed, s Shade) Int {\n\
    \tmatch m {\n\
    \t\tPlain -> leave(s, 1, 'x')\n\
    \t\tHeld(n) -> leave(flip(s), n, 'y')\n\
    \t\tOther -> {\n\t\t\tagain = same(m)\n\t\t\tif again == m { 3 } else { 4 }\n\t\t}\n\
    \t}\n}\n\
    fn leave(s Shade, n Int, name String) Int { if s == Dark { n } else { n + 10 * array::length([name]) } }\n\
    fn turns(n Int, s Shade, x Float, ok Bool, big Int, kept Array(Float)) (Shade, Float, Bool, Int) {\n\
    \tif n == 0 { (s, x, ok, big) } else {\n\
    \t\ty = kept[n % 2] or 0.5\n\
    \t\tf = if ok { flip } else { same }\n\
    \t\tturns(n - 1, f(s), x + y, !ok, big * 3, kept)\n\t}\n}\n\
    fn away(n Int, s Shade, x Float) Int { mixed(if x > 2.0 { Held(n) } else { Plain }, s) }\n\
    pub fn main() {\n\
    \ta = All(Dark, One, Other, 1.5, True, Nil, 7, Some(Light), Lost, flip)\n\
    \tprintln((a.a, a.b, a.c, a.d, a.e, a.f, a.g, a.h, a.i, a.j(a.a)))\n\
    \tbig = 1125899906842624 * 1125899906842624\n\
    \tprintln((through(Dark, 5), through(One, 4), through(2.5, 3), through(True, 5), through(Nil, 4)))\n\
    \tprintln((through(big, 5), through('s', 4), through([1, 2], 3), through(Held(3), 5), through(Plain, 4)))\n\
    \tprintln((twice(flip, Dark), twice(fn(n) { n * big }, 1), twice(fn(x) { x * 2.0 }, 1.5)))\n\
    \tprintln((depth(deepen(Dark)), depth(deepen([1])), depth(Deep(Flat((1.5, 2.5))))))\n\
    \tt = add(add(add(add(Leaf, 5), 2), 9), 4)\n\
    \tprintln(shades(t, []))\n\
    \tprintln([mixed(Plain, Dark), mixed(Plain, Light), mixed(Held(5), Dark), mixed(Other, Dark)])\n\
    \tprintln(turns(9, Dark, 0.0, True, big, [1.25, 2.5]))\n\
    \tprintln((away(1, Light, 1.0), away(2, Dark, 3.0)))\n\
    \tm = map::set(map::set(map::new(), Dark, 1.5), Light, 2.5)\n\
    \tprintln((map::get(m, Dark) or 0.0, map::get(m, flip(Dark)), map::set(m, Dark, 0.0)))\n\
    \tprintln(array::map([Dark, Light], flip))\n\
    \tprintln(array::fold([1.5, 2.5], 0.0, fn(x, y) { x + y }))\n\
    \tprintln(array::any([True, False], same))\n\
    \tprintln([array::length([Proxy, Lost]), if same(Proxy) == Lost { 1 } else { 2 }])\n\
    }\n";

/// What a value's type says it may be is what it is, in a run that checks each
/// value after the instruction that gave it.
#[test]
fn a_value_is_what_its_type_says_at_run_time() {
    let mut host = OsHost::new(Vec::new(), Vec::new(), Vec::new());
    let program = compile(EVERY_TYPE);
    let ran = scarlet_vm::run(&program, &mut host, Audit::EachInstruction);
    assert_eq!(ran, Ok(()));
    let out = String::from_utf8(std::mem::take(host.out())).expect("UTF-8");
    let want = "(\n\
        \x20\x20Dark,\n\
        \x20\x20One,\n\
        \x20\x20Other,\n\
        \x20\x201.5,\n\
        \x20\x20True,\n\
        \x20\x20Nil,\n\
        \x20\x207,\n\
        \x20\x20Some(Light),\n\
        \x20\x20Lost,\n\
        \x20\x20Light\n\
        )\n\
        (Dark, One, 2.5, True, Nil)\n\
        (\n\
        \x20\x201267650600228229401496703205376,\n\
        \x20\x20s,\n\
        \x20\x20[1, 2],\n\
        \x20\x20Held(3),\n\
        \x20\x20Plain\n\
        )\n\
        (Dark, 1606938044258990275541962092341162602522202993782792835301376, 6.0)\n\
        (2, 2, 1)\n\
        [Dark, Dark, Light, Light]\n\
        [1, 11, 15, 3]\n\
        (Light, 17.5, False, 24951166764292239309659609191415808)\n\
        (11, 12)\n\
        (\n\
        \x20\x201.5,\n\
        \x20\x20Some(2.5),\n\
        \x20\x20{Light: 2.5, Dark: 0.0}\n\
        )\n\
        [Light, Dark]\n\
        4.0\n\
        True\n\
        [2, 2]\n";
    assert_eq!(out, want);
}

/// A type that says a value is no cell when it is one is found where the value
/// is written, and where a function starts with it.
#[test]
fn a_type_that_says_what_is_not_so_is_found() {
    let src = "type Shade {\n\tDark\n\tLight\n}\n\
        fn make(n Int) Option(Int) { Some(n) }\n\
        fn take(o Option(Int), s Shade, n Int) Int {\n\
        \tmatch o {\n\t\tSome(m) -> if s == Dark { m } else { n }\n\t\tNone -> n\n\t}\n}\n\
        pub fn main() {\n\
        \tmade = make(1)\n\
        \tprintln(take(made, Light, 2))\n\
        }\n";
    let ran = |program: &scarlet_core::core_ir::Program| {
        let mut host = OsHost::new(Vec::new(), Vec::new(), Vec::new());
        scarlet_vm::run(program, &mut host, Audit::EachInstruction)
    };
    let program = compile(src);
    assert_eq!(ran(&program), Ok(()));
    let params = |name: &str| {
        let f = (&program.fns).into_iter().find(|f| f.name == name);
        let params = f.expect("a function").core.params.iter();
        params.map(|p| p.ty).collect::<Vec<_>>()
    };
    let [_, shade, int] = params("take")[..] else {
        panic!("three parameters");
    };
    for lie in [shade, int] {
        // What `make` gives, where `main` binds it.
        let mut bound = program.clone();
        // What `take` is given.
        let mut given = program.clone();
        for (program, name) in [(&mut bound, "main"), (&mut given, "take")] {
            let fns = std::mem::take(&mut program.fns);
            let changed = fns.into_iter().map(|mut f| {
                if f.name == name && name == "take" {
                    f.core.params[0].ty = lie;
                } else if f.name == name {
                    retype_calls(&mut f.core.body, lie);
                }
                f
            });
            program.fns = changed.collect();
        }
        for program in [bound, given] {
            let Err(Stop::BadProgram(why)) = ran(&program) else {
                panic!("the check finds it");
            };
            let why = why.to_string();
            assert!(why.contains("says there is no such cell"), "{why}");
        }
    }
}

/// Say that the first call in `e` gives a `ty`.
fn retype_calls(mut e: &mut CoreExpr, ty: RTy) {
    while let CoreExpr::Let { bind, rhs, body } = e {
        if matches!(rhs, Atom::Call { .. }) {
            bind.ty = ty;
            return;
        }
        e = body;
    }
    panic!("no call");
}

/// A program that does every operator on two values it is given, and on one
/// of them with a constant on each side, stored and branched on. It does each
/// a second time through a function that works on any kind of number, whose
/// instruction is another one, and prints where the two are not alike. Each of
/// `values` is paired with each.
fn every_operator(kind: &str, constants: &[String], values: &[String]) -> String {
    let arith = [
        ("+", "add"),
        ("-", "sub"),
        ("*", "mul"),
        ("/", "div"),
        ("%", "rem"),
    ];
    let compare = [
        ("==", "eq"),
        ("!=", "ne"),
        ("<", "lt"),
        ("<=", "le"),
        (">", "gt"),
        (">=", "ge"),
    ];
    // A number is compared by how it shows, which tells the two zeros apart.
    let alike = |a: &str, b: &str| -> Vec<String> {
        let mut out = Vec::new();
        for (op, name) in arith {
            out.push(format!("'${{{a} {op} {b}}}' == '${{{name}({a}, {b})}}'"));
        }
        for (op, name) in compare {
            out.push(format!("{{ {a} {op} {b} }} == {name}({a}, {b})"));
            out.push(format!(
                "{{ if {a} {op} {b} {{ 'y' }} else {{ 'n' }} }} == '${{yes({name}({a}, {b}))}}'"
            ));
        }
        out
    };
    let mut src = String::from(
        "import scarlet/float\n\
         fn yes(b Bool) String { if b { 'y' } else { 'n' } }\n",
    );
    for (op, name) in arith.into_iter().chain(compare) {
        src += &format!("fn {name}(a, b) {{ a {op} b }}\n");
    }
    let mut groups = vec![alike("x", "y")];
    for k in constants {
        groups.push(alike("x", k));
        groups.push(alike(k, "x"));
    }
    let zero = if kind == "Int" { "0" } else { "-0.0" };
    groups.push(vec![
        format!("'${{-x}}' == '${{sub({zero}, x)}}'"),
        String::from("float::from_int(-3) == -3.0"),
    ]);
    let calls: String = (0..groups.len())
        .map(|i| format!("\tcheck('${{x}} ${{y}} {i}', f{i}(x, y), 0)\n"))
        .collect();
    for (i, lines) in groups.iter().enumerate() {
        let lines = lines.join(",\n\t\t");
        src += &format!(
            "fn f{i}(x {kind}, y {kind}) Array(Bool) {{\n\
             \t_y = y\n\
             \t[\n\t\t{lines},\n\t]\n\
             }}\n"
        );
    }
    let pairs: String = values
        .iter()
        .flat_map(|x| values.iter().map(move |y| format!("\tall({x}, {y})\n")))
        .collect();
    src + &format!(
        "fn check(at String, same Array(Bool), line Int) Nil {{\n\
         \tmatch same {{\n\
         \t\t[] -> Nil\n\
         \t\t[ok, ..rest] -> {{\n\
         \t\t\tif ok {{ Nil }} else {{ println('${{at}} ${{line}}') }}\n\
         \t\t\tcheck(at, rest, line + 1)\n\
         \t\t}}\n\
         \t}}\n\
         }}\n\
         fn all(x {kind}, y {kind}) Nil {{\n{calls}\tNil\n}}\n\
         pub fn main() {{\n{pairs}\tprintln('done')\n}}\n"
    )
}

fn each(of: &[&str]) -> Vec<String> {
    of.iter().map(|s| (*s).to_string()).collect()
}

/// `src` runs to its end and finds no operator that is not what the
/// instruction for any number gives. It runs every operator instruction there
/// is whose name starts as `kinds` says.
fn every_instruction_runs(src: &str, kinds: [&str; 2]) {
    let (out, counts) = counted(src);
    assert_eq!(out, "done\n");
    let ours: Vec<_> = counts
        .ops
        .iter()
        .filter(|(name, _)| kinds.iter().any(|k| name.starts_with(k)))
        .collect();
    assert!(ours.len() > 20, "{ours:?}");
    for (name, n) in ours {
        // A pattern on a number is not an operator.
        assert!(*n > 0 || name.ends_with(kinds[1]), "{name} never ran");
    }
}

/// Every Int instruction a program can name gives what the one instruction
/// for any number gives, with the constants and the values at both ends of
/// the small Ints and past them.
#[test]
fn int_operators_and_constants_agree_with_the_operator_on_any_number() {
    let constants = each(&["0", "1", "7", "1125899906842623", "1125899906842624"]);
    let values = each(&[
        "0",
        "-1",
        "7",
        "1125899906842623",
        "-1125899906842624",
        "1125899906842624",
        "-1125899906842625",
        "{ 9223372036854775807 * 4 }",
        "{ -9223372036854775807 - 1 }",
    ]);
    let src = every_operator("Int", &constants, &values);
    every_instruction_runs(&src, ["Int", "JumpUnlessInt"]);
}

#[test]
fn float_operators_and_constants_agree_with_the_operator_on_any_number() {
    let huge = format!("{:.1}", f64::MAX);
    let tiny = format!("{:.1074}", f64::from_bits(1));
    let constants = [each(&["0.0", "0.1", "2.0"]), vec![huge.clone()]].concat();
    let values = [
        each(&["0.0", "-0.0", "0.1", "-7.5", "2.0"]),
        vec![huge.clone(), format!("-{huge}"), tiny],
    ]
    .concat();
    let src = every_operator("Float", &constants, &values);
    every_instruction_runs(&src, ["Float", "JumpUnlessFloat"]);
}

/// The same with the heap checked before every instruction, on fewer values.
#[test]
fn operators_at_the_edges_keep_every_count_right() {
    let ints = every_operator(
        "Int",
        &each(&["0", "1125899906842623"]),
        &each(&["7", "-1125899906842624", "{ 9223372036854775807 * 4 }"]),
    );
    let floats = every_operator("Float", &each(&["0.0"]), &each(&["-0.0", "7.5"]));
    for src in [ints, floats] {
        let mut host = OsHost::new(Vec::new(), Vec::new(), Vec::new());
        let ran = scarlet_vm::run(&compile(&src), &mut host, Audit::EachInstruction);
        assert_eq!(ran, Ok(()));
        assert_eq!(std::mem::take(host.out()), b"done\n");
    }
}

/// A Float answer with no value, one too large, and the remainder of a Float
/// by zero, through each instruction that can meet them.
#[test]
fn float_rules_hold_in_every_instruction() {
    let max = format!("{:.1}", f64::MAX);
    let src = format!(
        "fn over(x Float, y Float) Array(Float) {{ [x / y, x % y, y / x, 1.0 / y, y / 2.0] }}\n\
         fn by_zero(x Float) Array(Float) {{ [x / 0.0, 0.0 / x, x % 0.0, x - x] }}\n\
         fn large(x Float) Array(Bool) {{\n\
         \t[x * 2.0 == x, x + x == x, 2.0 * x == x, -x - x == -x, 0.0 - x == -x, x * -2.0 == -x]\n\
         }}\n\
         pub fn main() {{\n\
         \tprintln(over(7.5, 0.0))\n\
         \tprintln(over(-7.5, -0.0))\n\
         \tprintln(by_zero(3.5))\n\
         \tprintln(by_zero(0.0))\n\
         \tprintln(large({max}))\n\
         }}\n"
    );
    prints(
        &src,
        "[0.0, 7.5, 0.0, 0.0, 0.0]\n[0.0, -7.5, 0.0, 0.0, -0.0]\n[0.0, 0.0, 3.5, 0.0]\n\
         [0.0, 0.0, 0.0, 0.0]\n[True, True, True, True, True, True]\n",
    );
}

/// A big Int constant's cell is made the first time the program reads it and
/// is the same cell every time after, so a loop that reads one makes it once.
#[test]
fn a_big_int_constant_is_made_once_however_often_it_is_read() {
    let src = |n: u32| {
        format!(
            "fn go(n Int, hits Int) Int {{\n\
             \tif n == 0 {{ hits }} else {{\n\
             \t\tgo(n - 1, if n < 9007199254740993 {{ hits + 1 }} else {{ hits }})\n\
             \t}}\n\
             }}\n\
             pub fn main() {{\n\tprintln(go({n}, 0))\n}}\n"
        )
    };
    let (out, few) = counted(&src(10));
    assert_eq!(out, "10\n");
    let (out, many) = counted(&src(1000));
    assert_eq!(out, "1000\n");
    assert_eq!(ran(&few, "BigInt"), 10);
    assert_eq!(ran(&many, "BigInt"), 1000);
    assert_eq!((few.cells_made, many.cells_made), (1, 1));
    // The constant is kept, and the audit at the end gives it up.
    assert_eq!(many.cells_live, 1);
}

/// Counting is by whole blocks of instructions, and a stop leaves some of
/// them half run, so a run that stops has its stop and no counts.
#[test]
fn a_counted_run_that_stops_has_no_counts() {
    let src = "import scarlet/internal\n\
               fn later() Int { internal::live_subjects() }\n\
               pub fn main() {\n\
               \tprintln('before')\n\
               \tprintln(later())\n\
               }\n";
    let mut host = OsHost::new(Vec::new(), Vec::new(), Vec::new());
    let stopped = scarlet_vm::run_counted(&compile(src), &mut host, Audit::AtEnd);
    assert!(matches!(stopped, Err(Stop::NotBuiltYet(_))), "{stopped:?}");
    assert_eq!(std::mem::take(host.out()), b"before\n");
}

/// The listing shows each function as the VM loaded it, under the names a
/// counted run counts by.
#[test]
fn the_listing_shows_the_loaded_instructions() {
    let listing = scarlet_vm::listing(&compile(FIB)).expect("the program loads");
    let fib = listing.function(0).expect("fib is the first function");
    assert!(
        fib.starts_with("; loaded: 4 registers, parameter in r2\n"),
        "{fib}"
    );
    assert!(
        fib.contains("    0  JumpUnlessIntLtK(r2, 2) -> 2\n"),
        "{fib}"
    );
    assert!(fib.contains("= Call1(fn#0, move r"), "{fib}");
    assert!(fib.contains("= IntAdd(r"), "{fib}");
    let main = listing.function(1).expect("main is the second");
    assert!(main.contains("r2 = Call1(fn#0, move r2)\n"), "{main}");
    assert!(main.contains("r2 = Print(r2, newline)\n"), "{main}");
    assert!(listing.function(2).is_none());
    assert!(listing.toplevel().starts_with("; loaded: 3 registers\n"));
}

/// The listing marks the source a `Move` takes as it marks an argument a
/// call takes, and leaves a source that is read again unmarked.
#[test]
fn the_listing_marks_a_move_that_takes_its_source() {
    let src = "fn fresh(n Int) Array(Int) { [n, n + 1] }\n\
               fn twice(n Int) Array(Int) {\n\
               \tt = fresh(n)\n\
               \tys = t\n\
               \t[..ys, ..t]\n\
               }\n\
               fn once(n Int, last Array(Int), acc Array(Int)) Array(Int) {\n\
               \tif n == 0 {\n\
               \t\tacc\n\
               \t} else {\n\
               \t\tnext = fresh(n)\n\
               \t\tonce(n - 1, next, [..acc, ..last])\n\
               \t}\n\
               }\n\
               pub fn main() {\n\
               \tprintln(twice(1))\n\
               \tprintln(once(2, [], []))\n\
               }\n";
    let listing = scarlet_vm::listing(&compile(src)).expect("the program loads");
    let mut all = (0..).map_while(|i| listing.function(i));
    let twice = all.find(|f| f.contains("r3 = Move(r2)"));
    let twice = twice.expect("twice");
    assert!(twice.contains("    1  r3 = Move(r2)\n"), "{twice}");
    let once = all.next().expect("once");
    assert!(once.contains("r3 = Move(move r5)\n"), "{once}");
}

/// The listing shows a loop as a jump to its first instruction, after the moves
/// that put the arguments in place, and a tail call to another function as the
/// function it enters.
#[test]
fn the_listing_shows_a_tail_call_as_moves_and_a_jump() {
    let src = "fn down(n Int, a Int, b Int, c Int) Int {\n\
               \tif n == 0 { up(b, a) } else { down(n - 1, b, c, a) }\n\
               }\n\
               fn up(a Int, b Int) Int { if a > 9 { b } else { up(a + b, b + 1) } }\n\
               fn turn(a Int, b Int) Int { if a == b { turn(b, a) } else { a } }\n\
               pub fn main() {\n\
               \tprintln(down(7, 1, 2, 3))\n\
               \tprintln(turn(1, 2))\n\
               }\n";
    let listing = scarlet_vm::listing(&compile(src)).expect("the program loads");
    let down = listing.function(0).expect("down is the first function");
    assert_eq!(
        down,
        "; loaded: 6 registers, parameters in r2 to r5\n    \
         0  JumpUnlessIntEqK(r2, 0) -> 4\n    \
         1  Drop(r5)\n    \
         2  r2 = Move(move r4)\n    \
         3  Enter(fn#1)\n    \
         4  r2 = IntSubK(r2, 1)\n    \
         5  r3, r4, r5 = Move3(move r4, move r5, move r3)\n    \
         6  Jump -> 0\n"
    );
    let up = listing.function(1).expect("up is the second");
    assert!(
        up.ends_with("2  r2 = IntAdd(r2, r3)\n    3  r3 = IntAddK(r3, 1)\n    4  Jump -> 0\n"),
        "{up}"
    );
    let turn = listing.function(2).expect("turn is the third");
    assert!(
        turn.contains("1  r2, r3 = Move2(move r3, move r2)\n    2  Jump -> 0\n"),
        "{turn}"
    );
}

/// A loop's arguments are computed in the order that leaves nothing to move,
/// whatever else it passes.
#[test]
fn the_listing_shows_the_order_of_a_loops_arguments() {
    let listed = |ty: &str| {
        let src = format!(
            "fn same(x {ty}) {ty} {{ x }}\n\
             fn go(x {ty}, n Int, acc Int) Int {{\n\
             \tif n == 0 {{ acc }} else {{\n\
             \t\ty = same(x)\n\
             \t\tgo(y, n - 1, acc + n)\n\
             \t}}\n\
             }}\n\
             pub fn main() {{ println(go([1], 3, 0)) }}\n"
        );
        let listing = scarlet_vm::listing(&compile(&src)).expect("the program loads");
        let all = (0..).map_while(|i| listing.function(i));
        let go = all.filter(|f| f.contains("IntSubK(")).collect::<Vec<_>>();
        let [go] = &go[..] else {
            panic!("one function counts down: {go:?}")
        };
        (go.find("IntSubK("), go.find("IntAdd("), go.contains("Move"))
    };
    for ty in ["Array(Int)", "a"] {
        let (sub, add, moves) = listed(ty);
        assert!(add < sub && !moves, "{ty}: {add:?} {sub:?} {moves}");
    }
}

/// A function that needs something the VM does not run yet has no
/// instructions, and its listing says what it is waiting for.
#[test]
fn the_listing_says_why_a_function_did_not_load() {
    let src = "import scarlet/internal\n\
               pub fn main() {\n\
               \tprintln(internal::live_subjects())\n\
               }\n";
    let listing = scarlet_vm::listing(&compile(src)).expect("the program loads");
    let main = listing.function(0).expect("main");
    assert_eq!(
        main,
        "; not loaded: the VM does not run the built-in InternalLiveSubjects yet\n"
    );
}

/// Long literals do not need a register for each element: a table of numbers
/// in a constant and in a function, tuples that hold a string and an array, an
/// interpolation, a binary, and arrays inside one another. Each has more
/// elements than a frame has registers, and is what it would be if it were
/// made at once.
#[test]
fn a_literal_may_be_longer_than_a_frame() {
    let ints = |n: usize| each_of(n, |i| (i * 7919 % 1000).to_string()).join(", ");
    let pairs = each_of(400, |i| format!("({i}, 'n{i}', [{i}, k])")).join(", ");
    let parts = each_of(600, |i| format!("${{k + {i}}}")).join(" ");
    let bytes = each_of(300, |i| format!("k:size(8), {}:size(8)", i % 256)).join(", ");
    let rows = each_of(290, |i| format!("[..ys, '{i}']")).join(", ");
    let texts = each_of(300, |i| format!("'${{k}}{i}'")).join(", ");
    let src = format!(
        "import scarlet/array\n\
         import scarlet/binary\n\
         import scarlet/string\n\
         const table = [{table}]\n\
         fn ints(k Int) Array(Int) {{ [k, {table}, k] }}\n\
         fn pairs(k Int) Array((Int, String, Array(Int))) {{\n\
         \txs = [{pairs}]\n\
         \tprintln(k)\n\
         \txs\n\
         }}\n\
         fn text(k Int) String {{ '{parts}' }}\n\
         fn bytes(k Int) Binary {{ <<{bytes}>> }}\n\
         fn rows(c Bool, k Int) Array(Array(String)) {{\n\
         \tys = if c {{ [{texts}] }} else {{ [] }}\n\
         \t[{rows}]\n\
         }}\n\
         fn same(x String) Array(String) {{ [{same}] }}\n\
         fn count(xs Array(Array(String)), acc Int) Int {{\n\
         \tmatch xs {{\n\
         \t\t[] -> acc\n\
         \t\t[y, ..ys] -> count(ys, acc + array::length(y))\n\
         \t}}\n\
         }}\n\
         pub fn main() {{\n\
         \tprintln(array::length(table))\n\
         \tprintln(table[999])\n\
         \tprintln([..table, 5] == array::reverse(array::reverse([..table, 5])))\n\
         \tprintln(array::length(ints(3)))\n\
         \tprintln(ints(4) == [4, ..[..table, 4]])\n\
         \tprintln(pairs(7)[399])\n\
         \tprintln(string::length(text(1)))\n\
         \tprintln(string::trim(text(1)) == text(1))\n\
         \tprintln(binary::byte_size(bytes(9)))\n\
         \tprintln(binary::byte_at(bytes(9), 599))\n\
         \tprintln(count(rows(True, 2), 0))\n\
         \tprintln(count(rows(False, 2), 0))\n\
         \tprintln(array::length(same('x')))\n\
         }}\n",
        table = ints(1000),
        same = "x, ".repeat(2999) + "x",
    );
    let want = "1000\nSome(81)\nTrue\n1002\nTrue\n7\nSome(\n  (\n    399,\n    n399,\n    [399, 7]\n  )\n)\n\
                2291\nTrue\n600\n43\n87290\n290\n3000\n";
    prints(&src, want);
    // A short run of it, with the heap checked before every instruction.
    let src = format!(
        "fn ints(k Int) Array(Int) {{ [k, {}] }}\n\
         fn text(k Int) String {{ '{}' }}\n\
         pub fn main() {{\n\
         \tprintln(ints(3)[260])\n\
         \tprintln(text(0) == text(1))\n\
         }}\n",
        ints(300),
        each_of(260, |i| format!("${{k + {i}}}")).join(" "),
    );
    let mut host = OsHost::new(Vec::new(), Vec::new(), Vec::new());
    let ran = scarlet_vm::run(&compile(&src), &mut host, Audit::EachInstruction);
    assert_eq!(ran, Ok(()));
    assert_eq!(std::mem::take(host.out()), b"Some(21)\nFalse\n");
}

/// So may one with a spread in it, at either end, in the middle, or in both
/// places. The array that is spread stays as it was.
#[test]
fn a_literal_with_a_spread_may_be_longer_than_a_frame() {
    let texts = |from: usize, n: usize| each_of(n, |i| format!("'${{k + {}}}'", from + i));
    let src = |n: usize| {
        let (a, b) = (texts(0, n).join(", "), texts(1000, n).join(", "));
        format!(
            "import scarlet/array\n\
             fn back(xs Array(String), k Int) Array(String) {{ [..xs, {a}] }}\n\
             fn front(xs Array(String), k Int) Array(String) {{ [{a}, ..xs] }}\n\
             fn middle(xs Array(String), k Int) Array(String) {{ [{a}, ..xs, {b}] }}\n\
             fn both(xs Array(String), k Int) Array(String) {{ [..xs, {a}, ..xs] }}\n\
             fn lengths(xs Array(Int), k Int) Array(Int) {{\n\
             \t[..xs, {}, array::length(xs) * k, {}]\n\
             }}\n\
             fn row(k Int, xs Array(String)) Array(String) {{\n\
             \t[..array::map(xs, fn(x) {{ '${{x}}${{k}}' }}), {a}]\n\
             }}\n\
             fn shown(xs Array(String)) {{\n\
             \tprintln('${{array::length(xs)}} ${{array::reverse(xs)[0]}} ${{xs[0]}} ${{xs[1]}} ${{xs[2]}} ${{xs[250]}} ${{xs[258]}}')\n\
             }}\n\
             pub fn main() {{\n\
             \tt = 't'\n\
             \txs = ['s${{t}}', t]\n\
             \tshown(back(xs, 1))\n\
             \tshown(front(xs, 1))\n\
             \tshown(middle(xs, 1))\n\
             \tshown(both(xs, 1))\n\
             \tshown(row(5, xs))\n\
             \tshown(back([], 1))\n\
             \tshown(front([], 1))\n\
             \tprintln(lengths(0..5, 1)[255])\n\
             \tprintln(xs)\n\
             }}\n",
            each_of(250, |i| i.to_string()).join(", "),
            each_of(250, |i| i.to_string()).join(", "),
        )
    };
    let want = "302 Some(300) Some(st) Some(t) Some(1) Some(249) Some(257)\n\
                302 Some(t) Some(1) Some(2) Some(3) Some(251) Some(259)\n\
                602 Some(1300) Some(1) Some(2) Some(3) Some(251) Some(259)\n\
                304 Some(t) Some(st) Some(t) Some(1) Some(249) Some(257)\n\
                302 Some(304) Some(st5) Some(t5) Some(5) Some(253) Some(261)\n\
                300 Some(300) Some(1) Some(2) Some(3) Some(251) Some(259)\n\
                300 Some(300) Some(1) Some(2) Some(3) Some(251) Some(259)\n\
                Some(5)\n[st, t]\n";
    prints(&src(300), want);
    // A short run of it, with the heap checked before every instruction.
    let elements = texts(0, 256).join(", ");
    let src = format!(
        "fn back(xs Array(String), k Int) Array(String) {{ [..xs, {elements}] }}\n\
         fn front(xs Array(String), k Int) Array(String) {{ [{elements}, ..xs] }}\n\
         pub fn main() {{\n\
         \txs = ['${{1}}']\n\
         \tprintln(back(xs, 1)[256])\n\
         \tprintln(front(xs, 1)[256])\n\
         }}\n"
    );
    let mut host = OsHost::new(Vec::new(), Vec::new(), Vec::new());
    let ran = scarlet_vm::run(&compile(&src), &mut host, Audit::EachInstruction);
    assert_eq!(ran, Ok(()));
    assert_eq!(std::mem::take(host.out()), b"Some(256)\nSome(1)\n");
}

/// A long literal is made in pieces of one leaf each, so it costs a cell for
/// each leaf and at most one more for where it hangs, and not one for each
/// element.
#[test]
fn a_long_literal_makes_a_few_cells_for_each_leaf() {
    let src = format!(
        "import scarlet/array\n\
         import scarlet/internal\n\
         fn table(k Int) Array(Int) {{ [k, {}] }}\n\
         pub fn main() {{\n\
         \tbefore = internal::cells_made()\n\
         \txs = table(1)\n\
         \tmade = internal::cells_made() - before\n\
         \tprintln(array::length(xs))\n\
         \tprintln(made)\n\
         }}\n",
        each_of(999, |i| i.to_string()).join(", ")
    );
    // 32 leaves, the root, and the branch made anew for each leaf hung but the
    // first.
    prints(&src, "1000\n63\n");
}

/// The forms that take their operands from a list do what those that hold them
/// do: a tuple and a prepend and an append of 8 strings, and of 9 and 10.
#[test]
fn a_wide_tuple_and_a_wide_push_hold_every_operand_in_order() {
    let program = |n: usize| {
        let parts = each_of(n, |i| format!("s{i}")).join(", ");
        let lets = each_of(n, |i| format!("\ts{i} = '${{k + {i}}}'\n")).concat();
        format!(
            "fn tuple(k Int) ({types}) {{\n{lets}\t({parts})\n}}\n\
             fn front(k Int, xs Array(String)) Array(String) {{\n{lets}\t[{parts}, ..xs]\n}}\n\
             fn back(k Int, xs Array(String)) Array(String) {{\n{lets}\t[..xs, {parts}]\n}}\n\
             pub fn main() {{\n\
             \txs = ['${{0}}']\n\
             \tprintln('${{tuple(10)}}')\n\
             \tprintln('${{front(10, xs)}}')\n\
             \tprintln('${{back(10, xs)}}')\n\
             \tprintln('${{front(10, [])}} ${{xs}}')\n\
             }}\n",
            types = each_of(n, |_| "String".to_string()).join(", "),
        )
    };
    let words = |out: &[u8]| -> String {
        let text = String::from_utf8(out.to_vec()).expect("UTF-8");
        text.split(['\n', ' ', ',', '(', ')', '[', ']'])
            .filter(|w| !w.is_empty())
            .collect::<Vec<_>>()
            .join(" ")
    };
    for n in [8, 9, 10] {
        let mut host = OsHost::new(Vec::new(), Vec::new(), Vec::new());
        let ran = scarlet_vm::run(&compile(&program(n)), &mut host, Audit::EachInstruction);
        assert_eq!(ran, Ok(()));
        let all = each_of(n, |i| (10 + i).to_string()).join(" ");
        let want = format!("{all} {all} 0 0 {all} {all} 0");
        assert_eq!(words(&std::mem::take(host.out())), want, "{n}");
    }
}

fn each_of(n: usize, f: impl Fn(usize) -> String) -> Vec<String> {
    (0..n).map(f).collect()
}

/// A function of thousands of locals runs when few of them are held at once. One
/// that holds more than a frame has room for loads, and stops the run when it is
/// called and not before, naming itself.
#[test]
fn a_function_may_hold_254_values_at_once() {
    let long = each_of(4000, |i| match i {
        0 => "\tx0 = a\n\tt0 = '${x0}'\n".to_string(),
        i => format!(
            "\tx{i} = x{} + string::length(t{})\n\tt{i} = '${{x{i}}}'\n",
            i - 1,
            i - 1
        ),
    });
    let src = format!(
        "import scarlet/string\n\
         fn long(a Int) Int {{\n{}\tx3999 + string::length(t3999)\n}}\n\
         pub fn main() {{ println(long(1)) }}\n",
        long.concat()
    );
    prints(&src, "16980\n");

    let holds = |n: usize| {
        let lets = each_of(n, |i| format!("\tx{i} = a * {i}\n")).concat();
        let sums = each_of(n, |i| format!("\ts{} = s{i} + x{i}\n", i + 1)).concat();
        format!(
            "fn wide(a Int) Int {{\n{lets}\ts0 = 0\n{sums}\ts{n}\n}}\n\
             pub fn main() {{\n\
             \tprintln('before')\n\
             \tprintln(wide(2))\n\
             }}\n"
        )
    };
    prints(&holds(254), "before\n64262\n");
    let mut host = OsHost::new(Vec::new(), Vec::new(), Vec::new());
    let stopped = scarlet_vm::run(&compile(&holds(255)), &mut host, Audit::AtEnd);
    let Err(Stop::NotBuiltYet(what)) = stopped else {
        panic!("{stopped:?}");
    };
    let says = "main.wide, which holds more than 254 values at once,";
    assert_eq!(what.to_string(), says);
    assert_eq!(std::mem::take(host.out()), b"before\n");
    // Uncalled, it stops nothing.
    let uncalled = holds(255).replace("println(wide(2))", "println(wide)");
    let mut host = OsHost::new(Vec::new(), Vec::new(), Vec::new());
    let ran = scarlet_vm::run(&compile(&uncalled), &mut host, Audit::AtEnd);
    assert_eq!(ran, Ok(()));
}

/// A closure called in tail position reads its captures, however many arguments
/// it takes and whichever kind of function the caller is.
#[test]
fn a_closure_called_in_tail_position_reads_its_captures() {
    for n in [1, 6, 7, 20] {
        let ints = vec!["Int"; n].join(", ");
        let names = each_of(n, |i| format!("a{i}")).join(", ");
        let sum = each_of(n, |i| format!("a{i}")).join(" + ");
        let args = each_of(n, |i| format!("{}", i + 1)).join(", ");
        prints(
            &format!(
                "fn through(f fn({ints}) String) String {{ f({args}) }}\n\
                 pub fn main() {{\n\
                 \tkept = '${{40 + 2}}'\n\
                 \tinner = fn({names}) '${{kept}}:${{{sum}}}'\n\
                 \tprintln(through(inner))\n\
                 \tafter = '${{kept}}!'\n\
                 \tfrom = fn(g) if kept == '42' {{ g({args}) }} else {{ after }}\n\
                 \tprintln(from(inner))\n\
                 \tprintln('${{inner({args})}} ${{after}}')\n\
                 }}\n"
            ),
            &{
                let sum = n * (n + 1) / 2;
                format!("42:{sum}\n42:{sum}\n42:{sum} 42!\n")
            },
        );
    }
}

/// `internal::stack_depth` counts the frames a call is in, whatever kind of call
/// each came by. A tail call takes over its caller's frame, so a loop adds none.
#[test]
fn the_stack_depth_counts_frames_and_not_turns() {
    prints(
        "import scarlet/internal\n\
         fn down(n Int) Int {\n\
         \tif n == 0 { internal::stack_depth() } else { down(n - 1) + 0 }\n\
         }\n\
         fn round(n Int) Int {\n\
         \tif n == 0 { internal::stack_depth() } else { round(n - 1) }\n\
         }\n\
         fn through(f fn(Int) Int, n Int) Int { f(n) + 0 }\n\
         pub fn main() {\n\
         \tprintln(internal::stack_depth())\n\
         \tprintln(down(0))\n\
         \tprintln(down(9))\n\
         \tprintln(down(5000))\n\
         \tprintln(round(5000))\n\
         \tk = 2\n\
         \tgo = fn(n) { if n == 0 { internal::stack_depth() } else { go(n - 1) + k - 2 } }\n\
         \tprintln(go(7))\n\
         \tprintln(through(go, 7))\n\
         \tprintln(through(round, 7))\n\
         \tprintln(through(fn(n) { internal::stack_depth() + n }, 0))\n\
         }\n",
        "1\n2\n11\n5002\n2\n9\n10\n3\n3\n",
    );
}

/// A frame as wide as there are registers is called from one as wide, hundreds
/// deep, by name, and through a closure that reads as many captures.
#[test]
fn frames_of_every_register_call_each_other() {
    // One value added at a time, so that the others are all held.
    let sum = |n: usize, from: &str| {
        let adds = each_of(n, |i| {
            format!("\t\ts{} = s{i} + string::length(x{i})\n", i + 1)
        });
        format!("\t\ts0 = {from}\n{}\t\ts{n}\n", adds.concat())
    };
    // The result of a call is a value beside those held across it, so the most
    // that the caller in the middle of one has is one fewer.
    for (n, m, wide) in [(200, 200, 0), (253, 254, 2)] {
        let lets = each_of(n, |i| format!("\t\tx{i} = '${{n + {i}}}'\n")).concat();
        let captured = each_of(m, |i| format!("\tx{i} = '${{{i}}}'\n")).concat();
        let src = format!(
            "import scarlet/string\n\
             fn down(n Int) Int {{\n\
             \tif n == 0 {{ 0 }} else {{\n{lets}\t\tr = down(n - 1)\n{}\t}}\n\
             }}\n\
             fn held(f fn(Int) Int, n Int) Int {{\n\
             \tif n == 0 {{ f(0) }} else {{ held(f, n - 1) + f(n) }}\n\
             }}\n\
             fn holder() fn(Int) Int {{\n{captured}\tfn(k) {{\n{}\t}}\n}}\n\
             pub fn main() {{\n\
             \tprintln(down(300))\n\
             \tprintln(held(holder(), 100))\n\
             }}\n",
            sum(n, "r"),
            sum(m, "k"),
        );
        let listing = scarlet_vm::listing(&compile(&src)).expect("the program loads");
        let all = (0..).map_while(|i| listing.function(i));
        let whole = all.filter(|f| f.starts_with("; loaded: 256 registers"));
        assert_eq!(whole.count(), wide, "{n}");
        let digits = |from: usize, n: usize| -> usize {
            (from..from + n).map(|i| i.to_string().len()).sum()
        };
        let down: usize = (1..=300).map(|from| digits(from, n)).sum();
        let want = format!("{down}\n{}\n", 5050 + 101 * digits(0, m));
        prints(&src, &want);
    }
}

/// A tail call to a function of fewer registers gives up what the others hold,
/// and one to a function of more finds the rest of them empty.
#[test]
fn a_tail_call_fits_the_frame_to_its_callee() {
    prints(
        "import scarlet/string\n\
         fn narrow(k Int) Int { k }\n\
         fn wide(n Int) Int {\n\
         \ta = '${n}a'\n\
         \tb = '${n}b'\n\
         \tc = '${n}c'\n\
         \tk = string::length('${a}${b}${c}${a}')\n\
         \tnarrow(k)\n\
         }\n\
         fn up(n Int) Int { if n == 0 { narrow(1) } else { wide(n) } }\n\
         fn down(n Int, acc Int) Int {\n\
         \tif n == 0 { acc } else { down(n - 1, acc + up(n)) }\n\
         }\n\
         pub fn main() {\n\
         \tprintln(wide(5))\n\
         \tprintln(down(100, 0))\n\
         }\n",
        "8\n1168\n",
    );
}

/// Calls and loops of up to 250 arguments, by name, to self and through a
/// value, pass every one to its parameter.
#[test]
fn a_call_may_pass_250_arguments() {
    for n in [6, 7, 100, 250] {
        let params = each_of(n, |i| format!("p{i} Int")).join(", ");
        let all = each_of(n, |i| format!("p{i}")).join(", ");
        let weighted = format!("weigh([{all}], 1, 0)");
        let turned = each_of(n, |i| format!("p{}", (i + 1) % n)).join(", ");
        let args = each_of(n, |i| format!("{}", i * i % 11)).join(", ");
        let same = vec!["a"; n].join(", ");
        let rest: usize = n - 1;
        let src = format!(
            "fn weigh(xs Array(Int), i Int, acc Int) Int {{\n\
             \tmatch xs {{\n\
             \t\t[] -> acc\n\
             \t\t[x, ..rest] -> weigh(rest, i + 1, acc + x * i)\n\
             \t}}\n\
             }}\n\
             fn sum({params}) Int {{ {weighted} }}\n\
             fn turn({params}) Int {{\n\
             \tif p{rest} > 100 {{ {weighted} }} else {{ turn({turned_up}) }}\n\
             }}\n\
             fn twice({params}) Int {{\n\
             \tif p0 == 99 {{ {weighted} }} else {{ twice(99, {tail}) + p0 }}\n\
             }}\n\
             fn all_same(a Int) Int {{ sum({same}) }}\n\
             fn through(f fn({types}) Int, {params}) Int {{ f({turned}) }}\n\
             pub fn main() {{\n\
             \tprintln(sum({args}))\n\
             \tprintln(turn({args}))\n\
             \tprintln(twice({args}))\n\
             \tprintln(all_same(3))\n\
             {through}\
             }}\n",
            turned_up = each_of(n, |i| match i {
                0 => format!("p{rest} + 7"),
                i => format!("p{}", i - 1),
            })
            .join(", "),
            tail = each_of(n - 1, |i| format!("p{}", i + 1)).join(", "),
            types = vec!["Int"; n].join(", "),
            // A function of one more parameter is one too many at 250.
            through = if n < 250 {
                format!("\tprintln(through(sum, {args}))\n")
            } else {
                "\tf = sum\n\tprintln(f(".to_string() + &args + "))\n"
            },
        );
        let src = if n < 250 { src } else { without_through(&src) };
        let v: Vec<i64> = (0..n).map(|i| (i * i % 11) as i64).collect();
        let weigh = |v: &[i64]| -> i64 { v.iter().zip(1..).map(|(p, i)| p * i).sum() };
        let mut turned = v.clone();
        while turned[rest] <= 100 {
            let last = turned.pop().expect("a parameter") + 7;
            turned.insert(0, last);
        }
        let mut second = v.clone();
        second[0] = 99;
        let mut rotated = v.clone();
        rotated.rotate_left(1);
        let through = if n < 250 { &rotated } else { &v };
        let want = format!(
            "{}\n{}\n{}\n{}\n{}\n",
            weigh(&v),
            weigh(&turned),
            weigh(&second) + v[0],
            weigh(&vec![3; n]),
            weigh(through),
        );
        prints(&src, &want);
    }
}

/// A function with a join that holds nearly as many values as there are
/// registers still loads, and its arguments stay in the order they are written
/// in when another would hold one more past the join.
#[test]
fn a_loop_past_a_full_join_loads() {
    for args in 1..=3 {
        let all = each_of(args, |i| format!("x{i} Int, y{i} Int, w{i} Int")).join(", ");
        let sums = each_of(args, |i| format!("\t\ts{i} = x{i} + y{i}\n")).concat();
        let next = each_of(args, |i| format!("s{i}, n, w{i}")).join(", ");
        // The most the join may hold besides: each sum, the parameters that the
        // call passes on, `p`, and the value being added up are held too.
        let many = 253 - 2 * args;
        let made = each_of(many, |i| format!("\t\t\tv{i} = p * {}\n", i + 2)).concat();
        let added = each_of(many - 1, |i| match i {
            0 => "\t\t\tt0 = v0 + v1\n".to_string(),
            i => format!("\t\t\tt{i} = t{} + v{}\n", i - 1, i + 1),
        });
        let (added, last) = (added.concat(), many - 2);
        let of = each_of(args, |i| format!("x{i} * y{i}")).join(" + ");
        let src = format!(
            "fn f(n Int, {all}) Int {{\n\
             \tif n == 0 {{ x0 + w0 }} else {{\n\
             {sums}\
             \t\tj = if n > 3 {{\n\t\t\tp = {of}\n{made}{added}\t\t\tt{last}\n\t\t}} else {{ 0 }}\n\
             \t\tf(n - 1 + j * 0, {next})\n\t}}\n}}\n\
             pub fn main() {{ println(f(5, {})) }}\n",
            vec!["1, 2, 3"; args].join(", "),
        );
        // `x0` goes 1, 3, 8, 12, 15, 17, with `n` in `y0`.
        prints(&src, "20\n");
    }
}

/// `src` without its function `through`.
fn without_through(src: &str) -> String {
    let kept = src.lines().filter(|l| !l.starts_with("fn through("));
    kept.map(|l| format!("{l}\n")).collect()
}

/// A record of 253 fields is the longest cell the heap's arena holds. It is
/// read, updated, compared, hashed and freed like any other record. The first
/// update in the loop makes a cell, since something else holds the record, and
/// the rest write to that one. The audit finds none left at the end.
#[test]
fn the_widest_record_in_the_arena_is_a_record() {
    let fields: String = (0..253).map(|i| format!("\tf{i} Int\n")).collect();
    let args: Vec<String> = (0..253).map(|i| i.to_string()).collect();
    let src = format!(
        "import scarlet/internal\n\
         import scarlet/map\n\
         type Wide {{\n{fields}}}\n\
         fn with(w Wide, n Int) Wide {{ Wide(..w, f0: n) }}\n\
         fn bump(w Wide, n Int) Wide {{\n\
         \tif n == 0 {{ w }} else {{ bump(with(w, n), n - 1) }}\n\
         }}\n\
         fn total(w Wide) Int {{ w.f0 + w.f1 + w.f149 + w.f150 + w.f251 + w.f252 }}\n\
         pub fn main() {{\n\
         \tw = Wide({args})\n\
         \tprintln(total(w))\n\
         \tmade = internal::cells_made()\n\
         \tv = bump(w, 100)\n\
         \tprintln(internal::cells_made() - made)\n\
         \tprintln(total(v))\n\
         \tprintln(w == v)\n\
         \tprintln(w == with(v, 0))\n\
         \tm = map::set(map::set(map::new(), w, 'first'), v, 'second')\n\
         \tprintln(map::get(m, with(v, 0)))\n\
         \tprintln(map::get(m, with(w, 1)))\n\
         \tprintln(map::get(m, with(w, 8)))\n\
         \tprintln(map::size(m))\n\
         }}\n",
        args = args.join(", "),
    );
    let want = "803\n1\n804\nFalse\nTrue\nSome(first)\nSome(second)\nNone\n2\n";
    prints(&src, want);
}

/// A record wider than the widest cell in the arena has its fields in a box of
/// their own, and is a record like any other, as one at the line is: read at
/// both ends, shared, compared, hashed, shown, matched and freed.
#[test]
fn a_record_too_wide_for_the_arena_is_a_record() {
    for n in [252, 253, 254, 255, 300, 1000] {
        let last = n - 1;
        let fields: String = (0..n).map(|i| format!("\tf{i} Int\n")).collect();
        let args = format!("a, {}b", "m, ".repeat(n - 2));
        let shown: String = (1..last).map(|i| format!("f{i}: 0, ")).collect();
        let src = format!(
            "import scarlet/internal\n\
             import scarlet/map\n\
             type Wide {{\n{fields}}}\n\
             type Any {{\n\tPlain\n\tW(w Wide)\n}}\n\
             fn wide(a Int, m Int, b Int) Wide {{ Wide({args}) }}\n\
             fn ends(w Wide) Int {{ w.f0 * 1000 + w.f1 * 100 + w.f{last} }}\n\
             fn both(v Wide, w Wide) Int {{ ends(v) - ends(w) }}\n\
             fn first(x Any) Int {{\n\
             \tmatch x {{\n\
             \t\tPlain -> 0\n\
             \t\tW(Wide(a, ..)) -> a\n\
             \t}}\n\
             }}\n\
             fn spin(w Wide, n Int) Wide {{\n\
             \tif n == 0 {{ w }} else {{ spin(wide(w.f{last}, w.f1, w.f0), n - 1) }}\n\
             }}\n\
             pub fn main() {{\n\
             \tw = wide(1, 0, 2)\n\
             \tprintln(ends(w))\n\
             \tprintln(both(w, w))\n\
             \tprintln(w == wide(1, 0, 2))\n\
             \tprintln(w == wide(1, 0, 3))\n\
             \tprintln(w == wide(1, 1, 2))\n\
             \tmade = internal::cells_made()\n\
             \tv = spin(w, 101)\n\
             \tprintln(internal::cells_made() - made)\n\
             \tprintln(ends(v))\n\
             \tm = map::set(map::set(map::new(), w, 'w'), v, 'v')\n\
             \tprintln(map::get(m, wide(2, 0, 1)))\n\
             \tprintln(map::get(m, wide(2, 0, 2)))\n\
             \tprintln([first(W(v)), first(Plain)])\n\
             \tprintln(w)\n\
             }}\n",
        );
        let want = format!(
            "1002\n0\nTrue\nFalse\nFalse\n101\n2001\nSome(v)\nNone\n[2, 0]\n\
             Wide{{ f0: 1, {shown}f{last}: 2 }}\n"
        );
        prints(&src, &want);
    }
}

/// A constructor as wide as the arena takes, or wider, that is matched and
/// built again takes the words that the one it was had in the arena, as a narrow
/// one does, and as the same variant or another. So a thousand turns grow the
/// arena no more than one. While something else still holds it, the same turn
/// leaves it as it was. What a wider one has in a box is not among those words:
/// `a_long_constructor_takes_the_space_of_the_last` in the heap's tests counts it.
#[test]
fn a_constructor_too_wide_for_the_arena_is_rebuilt_in_the_same_words() {
    for n in [252, 253, 254, 300] {
        let fields: String = (0..n - 1).map(|i| format!("\t\ta{i} Int,\n")).collect();
        let pattern = format!("a0, {}s", "_, ".repeat(n - 2));
        let zeros = "z, ".repeat(n - 2);
        let src = format!(
            "import scarlet/internal\n\
             import scarlet/string\n\
             type T {{\n\
             \tB(x Int)\n\
             \tA(\n{fields}\t\ts String,\n\t)\n\
             \tC(\n{fields}\t\ts String,\n\t)\n\
             }}\n\
             fn step(v T, z Int) T {{\n\
             \tmatch v {{\n\
             \t\tA({pattern}) -> C(a0 + 1, {zeros}s)\n\
             \t\tC({pattern}) -> A(a0 + 1, {zeros}s)\n\
             \t\tB(x) -> B(x)\n\
             \t}}\n\
             }}\n\
             fn spin(v T, n Int) T {{\n\
             \tif n == 0 {{ v }} else {{ spin(step(v, 0), n - 1) }}\n\
             }}\n\
             fn first(v T) Int {{\n\
             \tmatch v {{\n\
             \t\tA({pattern}) -> a0 + string::length(s)\n\
             \t\tC({pattern}) -> -a0 - string::length(s)\n\
             \t\tB(x) -> x\n\
             \t}}\n\
             }}\n\
             fn new(z Int, s String) T {{ A(0, {zeros}s) }}\n\
             pub fn main() {{\n\
             \tv = new(0, 'tail' + 'end')\n\
             \tmade = internal::cells_made()\n\
             \tw = spin(v, N - 1)\n\
             \tx = spin(w, 1)\n\
             \tprintln(internal::cells_made() - made)\n\
             \tprintln(first(x))\n\
             \tkeep = spin(x, 1)\n\
             \tprintln(first(x) + first(keep))\n\
             \tprintln(internal::cells_made() - made)\n\
             }}\n",
        );
        let (made, words) = cells_and_words(&src, 2, "2\n9\n-1\n3\n");
        assert_eq!(
            cells_and_words(&src, 1000, "1000\n1007\n-1\n1001\n"),
            (made + 998, words),
            "{n} fields"
        );
    }
}

/// A match tells every variant of a type from the rest, with fields or without
/// and however many there are, and rebuilding a value as another variant gives
/// a value that is that one. `turn` takes each of them round to the next, in
/// place where the two have as many fields.
#[test]
fn a_match_tells_every_variant_of_a_type() {
    for n in [2usize, 5, 33, 300] {
        // Variant `i` has `i % 3` fields.
        let args = |i: usize, a: &str, b: &str| match i % 3 {
            0 => String::new(),
            1 => format!("({a})"),
            _ => format!("({a}, {b})"),
        };
        let mut src = String::from("import scarlet/array\ntype E {\n");
        for i in 0..n {
            src += &format!("\tV{i}{}\n", args(i, "x Int", "y Array(Int)"));
        }
        src += "}\nfn number(e E) Int {\n\tmatch e {\n";
        for i in 0..n {
            src += &format!("\t\tV{i}{} -> {i}\n", args(i, "_", "_"));
        }
        src += "\t}\n}\nfn held(e E) Int {\n\tmatch e {\n";
        for i in 0..n {
            let sum = ["0", "x", "x + array::length(y)"][i % 3];
            src += &format!("\t\tV{i}{} -> {sum}\n", args(i, "x", "y"));
        }
        src += "\t}\n}\nfn turn(e E) E {\n\tmatch e {\n";
        for i in 0..n {
            let to = (i + 1) % n;
            // Each passes on the fields the next has too, and makes the rest.
            let (x, y) = match i % 3 {
                0 => ("1", "[1, 2]"),
                1 => ("x + 1", "[x]"),
                _ => ("x + 1", "[..y, 0]"),
            };
            let reads = match (i % 3, to % 3) {
                (_, 0) => ("_", "_"),
                (2, 1) => ("x", "_"),
                _ => ("x", "y"),
            };
            let from = args(i, reads.0, reads.1);
            src += &format!("\t\tV{i}{from} -> V{to}{}\n", args(to, x, y));
        }
        src += "\t}\n}\n\
                fn go(e E, n Int, sum Int) Int {\n\
                \tif n == 0 { sum } else { go(turn(e), n - 1, sum * 31 % 1000003 + number(e) * 7 + held(e)) }\n\
                }\n\
                fn at(e E, n Int) E {\n\
                \tif n == 0 { e } else { at(turn(e), n - 1) }\n\
                }\n\
                fn numbers(e E, n Int) Bool {\n";
        src += &format!(
            "\tif n == {n} {{ True }} else {{ number(at(e, n)) == n && numbers(e, n + 1) }}\n}}\n"
        );
        src += &format!(
            "pub fn main() {{\n\
             \tprintln(numbers(V0, 0))\n\
             \tprintln(go(V0, {}, 0))\n\
             \tprintln(at(V0, {}) == at(at(V0, 1), {}))\n\
             \tprintln(at(V0, 1) == V0)\n\
             }}\n",
            n * 3,
            n - 1,
            n - 2,
        );
        // The same sum, worked out here.
        let (mut sum, mut x, mut y) = (0i64, 0i64, 0i64);
        for k in 0..n * 3 {
            let i = k % n;
            let held = [0, x, x + y][i % 3];
            sum = sum * 31 % 1_000_003 + i as i64 * 7 + held;
            (x, y) = match i % 3 {
                0 => (1, 2),
                1 => (x + 1, 1),
                _ => (x + 1, y + 1),
            };
        }
        prints(&src, &format!("True\n{sum}\nTrue\nFalse\n"));
    }
}

/// A closure with more captures than the arena takes a cell for reads each of
/// them, and so does one at the line.
#[test]
fn a_closure_too_wide_for_the_arena_is_a_closure() {
    for extra in [0, 1, 2, 3] {
        let params: Vec<String> = (0..250).map(|i| format!("a{i} Int")).collect();
        let more: String = (0..extra)
            .map(|i| format!("\tb{i} = a0 + {}\n", i + 1000))
            .collect();
        let names = (0..250)
            .map(|i| format!("a{i}"))
            .chain((0..extra).map(|i| format!("b{i}")));
        // A few to a line, because one sum of them all nests too deep to parse.
        let parts: Vec<String> = names.zip(1..).map(|(a, k)| format!("{a} * {k}")).collect();
        let mut sum = String::from("\t\ts0 = x\n");
        for (i, few) in parts.chunks(10).enumerate() {
            sum += &format!("\t\ts{} = s{i} + {}\n", i + 1, few.join(" + "));
        }
        let last = parts.chunks(10).count();
        let args: Vec<String> = (0..250).map(|i| i.to_string()).collect();
        let src = format!(
            "fn made({}) fn(Int) Int {{\n{more}\tfn(x) {{\n{sum}\t\ts{last}\n\t}}\n}}\n\
             fn twice(f fn(Int) Int) Int {{ f(f(1)) }}\n\
             pub fn main() {{\n\
             \tf = made({})\n\
             \tprintln(f(1))\n\
             \tprintln(twice(f) - f(1))\n\
             \tprintln(f == f)\n\
             }}\n",
            params.join(", "),
            args.join(", "),
        );
        let held: i64 = (0..250)
            .chain(1000..1000 + extra)
            .zip(1..)
            .map(|(v, k): (i64, i64)| v * k)
            .sum();
        prints(&src, &format!("{}\n{held}\nTrue\n", held + 1));
    }
}

/// A string of 2,016 bytes is the longest whose cell the arena holds, and a
/// longer one keeps its text in a box. `a` is 2,010 bytes and `b` is 2,020,
/// and `lengths` grows a string a byte at a time from one side of the line
/// to the other. Strings and binaries on both sides are compared, joined,
/// searched, split, sliced and used as map keys, and print what they printed
/// before the heap had an arena.
#[test]
fn a_string_or_binary_too_long_for_the_arena_reads_the_same() {
    prints(
        "import scarlet/array\n\
         import scarlet/binary\n\
         import scarlet/internal\n\
         import scarlet/map\n\
         import scarlet/string\n\
         fn grow(s String, n Int) String { if n == 0 { s } else { grow(s + 'abcdefghij', n - 1) } }\n\
         fn lengths(s String, n Int, acc Int) Int {\n\
         \tif n == 0 { acc } else {\n\
         \t\tt = s + 'x'\n\
         \t\tlengths(t, n - 1, acc + string::length(t))\n\
         \t}\n\
         }\n\
         pub fn main() {\n\
         \ta = grow('', 201)\n\
         \tb = grow('', 202)\n\
         \tc = grow('', 1000)\n\
         \tprintln(string::length(a))\n\
         \tprintln(string::length(b))\n\
         \tprintln(a == b)\n\
         \tprintln(b == grow('', 202))\n\
         \tprintln(string::length(a + b + c))\n\
         \tprintln(string::contains(c, 'jabc'))\n\
         \tprintln(string::contains(c, 'jj'))\n\
         \tprintln(array::length(string::split(c, 'j')))\n\
         \tmade = internal::cells_made()\n\
         \tprintln(lengths(grow('', 195), 100, 0))\n\
         \tprintln(internal::cells_made() - made)\n\
         \tm = map::set(map::set(map::new(), b, 1), c, 2)\n\
         \tprintln(map::get(m, grow('', 202)))\n\
         \tprintln(map::get(m, grow('', 1000)))\n\
         \tprintln(map::get(m, a))\n\
         \tbits = binary::from_string(c)\n\
         \tprintln(binary::byte_size(bits))\n\
         \tprintln(binary::byte_at(bits, 9999))\n\
         \tprintln(binary::byte_size(binary::append(bits, bits)))\n\
         \tmatch binary::slice_bytes(bits, 5000, 5000) {\n\
         \t\tOk(half) -> {\n\
         \t\t\tprintln(binary::byte_size(half))\n\
         \t\t\tprintln(binary::byte_at(half, 4999))\n\
         \t\t\tprintln(binary::to_string(binary::drop_bytes(half, 4990)))\n\
         \t\t}\n\
         \t\tErr(Nil) -> println('no slice')\n\
         \t}\n\
         }\n",
        "2010\n2020\nFalse\nTrue\n14030\nTrue\nFalse\n1001\n200050\n591\nSome(1)\nSome(2)\nNone\n\
         10000\n106\n20000\n5000\n106\nOk(abcdefghij)\n",
    );
}
