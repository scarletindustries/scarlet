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
    let src = "fn greet() { println('hi') }\n\
               pub fn main() {\n\
               \tprintln(1)\n\
               }\n";
    prints(src, "1\n");
    let calls = "fn greet() { println('hi') }\n\
                 pub fn main() {\n\
                 \tgreet()\n\
                 }\n";
    assert_eq!(run(calls), Err(Stop::NotBuiltYet("String".into())));
}

/// Past 48 bits the exact answer needs a big int. Until those exist the run
/// stops rather than print a wrapped, wrong number.
#[test]
fn an_int_past_48_bits_stops_rather_than_wrapping() {
    let src = "pub fn main() {\n\tprintln(140737488355327 + 1)\n}\n";
    assert_eq!(
        run(src),
        Err(Stop::NotBuiltYet("Int beyond 48 bits (big ints)".into()))
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
