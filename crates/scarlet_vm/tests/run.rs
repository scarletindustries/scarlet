//! These tests run programs end to end. Each is compiled from Scarlet source
//! and run by the VM, and the test checks exactly what it prints.

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

/// Perceus reuse. A constructor whose cell nothing else holds any more is
/// overwritten in place instead of allocated. Here the loop gives its pair up
/// at the `match` and builds another on the way round, so one cell serves
/// every turn. `scarlet/internal` counts both, so the test pins the saving
/// and not just the answer.
#[test]
fn a_loop_overwrites_the_cell_it_just_gave_up() {
    prints(
        "import scarlet/internal\n\
         type Pair {\n\
         \tPair(a Int, b Int)\n\
         }\n\
         fn spin(n Int, p Pair) Int {\n\
         \tmatch p {\n\
         \t\tPair(a, b) -> if n <= 0 then a + b else spin(n - 1, Pair(b, a + 1))\n\
         \t}\n\
         }\n\
         pub fn main() {\n\
         \tmade = internal::cells_made()\n\
         \treused = internal::cells_reused()\n\
         \tprintln(spin(1000, Pair(0, 0)))\n\
         \tprintln(internal::cells_made() - made)\n\
         \tprintln(internal::cells_reused() - reused)\n\
         }\n",
        "1000\n1\n1000\n",
    );
}

/// Reuse across a call, which needs the call to be a move. The caller hands
/// over its own reference, so the callee is the only holder and may overwrite
/// the cell. Every `Cons` of the new list is the matching one of the old,
/// written over, so the walk allocates nothing at all.
#[test]
fn a_walk_over_a_list_overwrites_the_list_it_reads() {
    prints(
        "import scarlet/internal\n\
         type List {\n\
         \tCons(head Int, tail List)\n\
         \tEmpty\n\
         }\n\
         fn build(n Int, acc List) List {\n\
         \tif n <= 0 then acc else build(n - 1, Cons(n, acc))\n\
         }\n\
         fn add_one(xs List) List {\n\
         \tmatch xs {\n\
         \t\tCons(h, t) -> Cons(h + 1, add_one(t))\n\
         \t\tEmpty -> Empty\n\
         \t}\n\
         }\n\
         fn sum(xs List, acc Int) Int {\n\
         \tmatch xs {\n\
         \t\tCons(h, t) -> sum(t, acc + h)\n\
         \t\tEmpty -> acc\n\
         \t}\n\
         }\n\
         pub fn main() {\n\
         \txs = build(1000, Empty)\n\
         \tmade = internal::cells_made()\n\
         \treused = internal::cells_reused()\n\
         \tprintln(sum(add_one(xs), 0))\n\
         \tprintln(internal::cells_made() - made)\n\
         \tprintln(internal::cells_reused() - reused)\n\
         }\n",
        "501500\n0\n1000\n",
    );
}

/// The cell is given up *after* the constructor here, so the only turn that
/// can take it is the next one. That needs the cell to survive the call to
/// self in tail position, which reuses the frame.
#[test]
fn a_loop_carries_its_cell_across_the_call_that_ends_it() {
    prints(
        "import scarlet/internal\n\
         type Pair {\n\
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
         \tmade = internal::cells_made()\n\
         \treused = internal::cells_reused()\n\
         \tprintln(go(1000, 0))\n\
         \tprintln(internal::cells_made() - made)\n\
         \tprintln(internal::cells_reused() - reused)\n\
         }\n",
        "1002001\n1\n1000\n",
    );
}

/// Reuse waits on the last reference. The same loop with the pair kept in a
/// list allocates every turn, and every pair still reads back as it was
/// written. Overwriting a cell something else holds would show up here.
#[test]
fn a_cell_something_else_holds_is_not_overwritten() {
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
        // `checked` makes one `Ok` a turn, and no array cell, because its
        // `Err` arm does not read `xs`. `held` copies the root and the
        // nodes down to the slot: a branch and a leaf for the 96 elements in
        // the tree, and the tail leaf for the last 4.
        "0\n24995000\n5000\n37492500\n100\n296\n",
    );
}

/// A checked write is in place only when nothing reads the array after the
/// call, and an `Err` arm that gives the array back is such a read: the
/// `Result` is built before it is matched, so the loader cannot tell that
/// the arm runs only after a miss. Each write then copies the root and the
/// nodes down to the slot, whether the fallback is spelled as an arm or
/// with `or`. `array::put` is the same fallback inside the write, and
/// copies nothing.
///
/// The 24,984 is a limit to remove, not a rule to keep. Once a match on
/// the call loads as the write's two edges, it is the 5,000 `Ok` cells or
/// fewer.
#[test]
fn a_checked_write_copies_when_its_err_arm_reads_the_array() {
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
         }\n",
        // An `Ok` a turn, and beside it the root, two branches and a leaf
        // for the 4,992 elements in the tree, and the root and the tail
        // leaf for the last 8.
        "24984\n37492500\n24984\n37492500\n0\n37492500\n",
    );
}

/// `array::swap` on an array nothing else holds changes it in place. The
/// only cells 500 swaps make are the two `Some` each one reads its
/// elements through, and the 1,000-element array is never copied.
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
        "1000\nOk(\n  [1, 2, 3]\n)\nOk(\n  [998, 999, 1000]\n)\n",
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
/// name of its own before the update. Taking the record apart with a `match`
/// makes no cell at all, because the pattern proves the shape and the new
/// `State` takes the old one's cell.
///
/// Two spellings still copy the root and the nodes down to the slot at
/// every write, 24,984 cells with the 5,000 records. The first is a rule:
/// an argument written after the write reads `s`, so `s` has to live past
/// the write. The second is a limit to remove, not a rule to keep: an
/// update inside an `if` used as a value copies because no drop is placed
/// inside a branch that gives a value. Step 17 of docs/perf-plan.md should
/// bring it to 5,000.
///
/// A third spelling copies each node of the tree once, 164 cells on top of
/// the 5,000 records, and is a limit too. A record `main` built itself is
/// kept across the call, so that a later `State` can take its cell, and it
/// holds the array until the call returns. Step 13 should bring it to
/// 5,000.
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
        "37492500\n5000\n37492500\n5000\n37492500\n0\n37497500\n5000\n37492500\n5000\n\
         37497500\n24984\n37492500\n24984\n5164\n37492500\n1\n",
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
/// holder: 1,000 sets of keys the map already has make the 1,000 records,
/// the 1,000 tasks and their 1,000 empty arrays, and no map cell.
///
/// `by_task` is a limit to remove, not a rule to keep. It is the same
/// update, and the record is given up before the set there too, but the
/// set still copies, 5,436 cells. The value is a cell bound before the
/// update, so its drop comes after the set and before the map's, and the
/// loader only lets a set take its map when the map's drop is the first
/// thing after it. Step 33 of docs/perf-plan.md sets that from liveness and
/// should bring the second number to 3,000.
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
        "16\n3000\n16\n5436\n",
    );
}

/// Giving the record up early has a cost in one shape, pinned here as a
/// limit to remove. `t` is built in the same function, so its drop leaves
/// its cell for the next `P` to take. That drop used to sit straight before
/// the new `P`. It now comes before the new field, and when that field is
/// an `if` used as a value the cell is not carried past it: the old cell is
/// freed and the new record is made fresh, 20,001 cells and none reused
/// where it was 10,001 and 10,000. A field that is a call keeps the cell.
/// Step 13 of docs/perf-plan.md takes reuse away and step 16 updates such a
/// record where it is, so the second pair of numbers changes at both.
#[test]
fn a_record_update_loses_its_cell_to_a_branch_in_a_later_field() {
    prints(
        "import scarlet/internal\n\
         type P {\n\
         \ta Int\n\
         \tb Int\n\
         \tc Int\n\
         }\n\
         fn pick(i Int) Int {\n\
         \tif i > 5 { 1 } else { 2 }\n\
         }\n\
         fn called(i Int, acc Int) Int {\n\
         \tif i == 0 {\n\
         \t\tacc\n\
         \t} else {\n\
         \t\tt = P(i, i, i)\n\
         \t\tu = P(..t, c: pick(i))\n\
         \t\tcalled(i - 1, acc + u.c + u.a)\n\
         \t}\n\
         }\n\
         fn joined(i Int, acc Int) Int {\n\
         \tif i == 0 {\n\
         \t\tacc\n\
         \t} else {\n\
         \t\tt = P(i, i, i)\n\
         \t\tu = P(..t, c: if i > 5 { 1 } else { 2 })\n\
         \t\tjoined(i - 1, acc + u.c + u.a)\n\
         \t}\n\
         }\n\
         fn report(made Int, reused Int) {\n\
         \tprintln('made ${internal::cells_made() - made}, reused ${internal::cells_reused() - reused}')\n\
         }\n\
         pub fn main() {\n\
         \tmade = internal::cells_made()\n\
         \treused = internal::cells_reused()\n\
         \tprintln(called(10000, 0))\n\
         \treport(made, reused)\n\
         \tmade = internal::cells_made()\n\
         \treused = internal::cells_reused()\n\
         \tprintln(joined(10000, 0))\n\
         \treport(made, reused)\n\
         }\n",
        "50015005\nmade 10002, reused 9999\n50015005\nmade 20001, reused 0\n",
    );
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
    assert!(
        strike.contains("= ArraySet(move r0, r1, move r2) -> 4\n"),
        "{strike}"
    );
    assert!(strike.contains("    4  r"), "{strike}");
    assert!(strike.contains(" = Move move r0\n"), "{strike}");
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
    assert_eq!(ran(&counts, "CallSelf"), 176);
    assert_eq!(ran(&counts, "Call"), 1);
    assert_eq!(ran(&counts, "JumpUnlessIntLtK"), 177);
    assert_eq!(ran(&counts, "IntAdd"), 88);
    assert_eq!(ran(&counts, "Print"), 1);
    assert_eq!(ran(&counts, "Bad"), 0);
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
    assert_eq!(ran(&first, "Ctor"), 3);
    assert!(first.cells_made >= 3, "{}", first.cells_made);
    assert_eq!(first.cells_reused, 0);
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
    assert!(!counts.ops.iter().any(|(name, _)| *name == "Audit"));
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
        fib.starts_with("; loaded: 2 registers, parameters in r0\n"),
        "{fib}"
    );
    assert!(
        fib.contains("    0  JumpUnlessIntLtK(r0, 2) -> 2\n"),
        "{fib}"
    );
    assert!(fib.contains("= CallSelf(move r"), "{fib}");
    assert!(fib.contains("= IntAdd(r"), "{fib}");
    let main = listing.function(1).expect("main is the second");
    assert!(main.contains("r0 = Call fn#0(move r0)\n"), "{main}");
    assert!(main.contains("r0 = Print r0 and a newline\n"), "{main}");
    assert!(listing.function(2).is_none());
    assert!(listing.toplevel().starts_with("; loaded: 1 register\n"));
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
               fn once(n Int) Array(Int) {\n\
               \tmatch fresh(n) {\n\
               \t\tys -> [..ys, 9]\n\
               \t}\n\
               }\n\
               pub fn main() {\n\
               \tprintln(twice(1))\n\
               \tprintln(once(1))\n\
               }\n";
    let listing = scarlet_vm::listing(&compile(src)).expect("the program loads");
    let twice = listing.function(1).expect("twice is the second function");
    assert!(twice.contains("    1  r1 = Move r0\n"), "{twice}");
    let once = listing.function(2).expect("once is the third");
    assert!(once.contains("    1  r1 = Move move r0\n"), "{once}");
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

/// A record of 300 fields is longer than a cell the heap's arena holds, so
/// its fields live in a box of their own. It is read, updated, compared,
/// hashed and freed like any other record. Each update in the loop makes one
/// cell and frees the one before, and the audit finds none left at the end.
#[test]
fn a_record_too_wide_for_the_arena_is_a_record() {
    let fields: String = (0..300).map(|i| format!("\tf{i} Int\n")).collect();
    let args: Vec<String> = (0..300).map(|i| i.to_string()).collect();
    let src = format!(
        "import scarlet/internal\n\
         import scarlet/map\n\
         type Wide {{\n{fields}}}\n\
         fn bump(w Wide, n Int) Wide {{\n\
         \tif n == 0 {{ w }} else {{ bump(Wide(..w, f150: w.f150 + n, f299: w.f299 - 1), n - 1) }}\n\
         }}\n\
         fn total(w Wide) Int {{ w.f0 + w.f1 + w.f149 + w.f150 + w.f298 + w.f299 }}\n\
         pub fn main() {{\n\
         \tw = Wide({args})\n\
         \tprintln(total(w))\n\
         \tmade = internal::cells_made()\n\
         \tv = bump(w, 100)\n\
         \tprintln(internal::cells_made() - made)\n\
         \tprintln(total(v))\n\
         \tprintln(w == v)\n\
         \tprintln(w == Wide(..v, f150: 150, f299: 299))\n\
         \tm = map::set(map::set(map::new(), w, 'first'), v, 'second')\n\
         \tprintln(map::get(m, Wide(..w, f7: 7)))\n\
         \tprintln(map::get(m, Wide(..w, f7: 8)))\n\
         \tprintln(map::size(m))\n\
         }}\n",
        args = args.join(", "),
    );
    prints(&src, "897\n100\n5847\nFalse\nTrue\nSome(first)\nNone\n2\n");
}

/// A constructor of 300 fields that is matched and built again is written
/// over where it is, in its box, as a narrow one is. A thousand turns make
/// no cell and reuse one a thousand times. While something else still holds
/// it, the same turn makes a new cell and leaves the old one as it was.
#[test]
fn a_constructor_too_wide_for_the_arena_is_rebuilt_in_place() {
    let fields: String = (0..299).map(|i| format!("\t\ta{i} Int,\n")).collect();
    let pattern = format!("a0, {}s", "_, ".repeat(298));
    let zeros = "0, ".repeat(298);
    let src = format!(
        "import scarlet/internal\n\
         import scarlet/string\n\
         type T {{\n\tA(\n{fields}\t\ts String,\n\t)\n\tB(x Int)\n}}\n\
         fn step(v T) T {{\n\
         \tmatch v {{\n\
         \t\tA({pattern}) -> A(a0 + 1, {zeros}s)\n\
         \t\tB(x) -> B(x)\n\
         \t}}\n\
         }}\n\
         fn spin(v T, n Int) T {{ if n == 0 {{ v }} else {{ spin(step(v), n - 1) }} }}\n\
         fn first(v T) Int {{\n\
         \tmatch v {{\n\
         \t\tA({pattern}) -> a0 + string::length(s)\n\
         \t\tB(x) -> x\n\
         \t}}\n\
         }}\n\
         pub fn main() {{\n\
         \tv = A(0, {zeros}'tail' + 'end')\n\
         \tmade = internal::cells_made()\n\
         \treused = internal::cells_reused()\n\
         \tw = spin(v, 1000)\n\
         \tprintln(internal::cells_made() - made)\n\
         \tprintln(internal::cells_reused() - reused)\n\
         \tprintln(first(w))\n\
         \tkeep = spin(w, 1)\n\
         \tprintln(first(w) + first(keep))\n\
         \tprintln(internal::cells_made() - made)\n\
         \tprintln(internal::cells_reused() - reused)\n\
         }}\n",
    );
    prints(&src, "0\n1000\n1007\n2015\n1\n1000\n");
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
