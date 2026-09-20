use crate::parser::new_parser;
use crate::scanner::new_scanner;

/// Parse `src`, asserting it parses cleanly.
fn parse_ok(src: &str) -> crate::ast::BlockExpression {
    let mut s = new_scanner(src.to_string());
    let pr = new_parser(&mut s).parse_program();
    assert!(
        !crate::diagnostic::has_errors(&pr.diagnostics),
        "snippet failed to parse: {:?}",
        pr.diagnostics,
    );
    pr.ast
}

/// Compile a snippet with codegen on, as a script: these tests are about
/// what the compiler does with the code, and a script is the shortest way
/// to hand it some (it is also exactly what the REPL submits).
fn compile_script(src: &str) -> super::CompileResult {
    super::compile_with(
        &crate::ast::Expression::BlockExpression(parse_ok(src)),
        super::CompileOptions {
            module_scope: super::ModuleScope::Script,
            ..super::CompileOptions::default()
        },
    )
}

/// The lowered body of the function `name` in `p`.
fn body_named<'p>(p: &'p crate::core_ir::Program, name: &str) -> &'p crate::core_ir::CoreFn {
    for f in &p.fns {
        if f.name == name {
            return &f.core;
        }
    }
    panic!("no function named `{name}` in the program")
}

/// Every atom in `e`, in no particular order.
fn atoms(e: &crate::core_ir::CoreExpr) -> Vec<&crate::core_ir::Atom> {
    use crate::core_ir::CoreExpr as E;
    fn go<'e>(e: &'e E, out: &mut Vec<&'e crate::core_ir::Atom>) {
        match e {
            E::Let { rhs, body, .. } => {
                out.push(rhs);
                go(body, out);
            }
            E::LetJoin { join, body, .. } => {
                go(join, out);
                go(body, out);
            }
            E::LetCont { cont, body, .. } => {
                go(cont, out);
                go(body, out);
            }
            E::Drop { body, .. } => go(body, out),
            E::If { then, els, .. } => {
                go(then, out);
                go(els, out);
            }
            E::Match { arms, .. } => {
                for (_, b) in arms {
                    go(b, out);
                }
            }
            E::Tail(a) => out.push(a),
            E::Goto(_) => {}
        }
    }
    let mut out = Vec::new();
    go(e, &mut out);
    out
}

/// The locals `e` drops, in spine order.
fn drops(e: &crate::core_ir::CoreExpr) -> Vec<crate::core_ir::LocalId> {
    use crate::core_ir::CoreExpr as E;
    fn go(e: &E, out: &mut Vec<crate::core_ir::LocalId>) {
        match e {
            E::Drop { local, body, .. } => {
                out.push(*local);
                go(body, out);
            }
            E::Let { body, .. } => go(body, out),
            E::LetJoin { join, body, .. }
            | E::LetCont {
                cont: join, body, ..
            } => {
                go(join, out);
                go(body, out);
            }
            E::If { then, els, .. } => {
                go(then, out);
                go(els, out);
            }
            E::Match { arms, .. } => {
                for (_, b) in arms {
                    go(b, out);
                }
            }
            E::Tail(_) | E::Goto(_) => {}
        }
    }
    let mut out = Vec::new();
    go(e, &mut out);
    out
}

/// Compile `src` as the entry module on the LSP path, with
/// `collect_hover_facts` on so occurrence collection fires. A script, for
/// the reason `compile_script` is.
fn collect(src: &str) -> super::Compiler {
    let block = parse_ok(src);
    let mut c = super::new_compiler(None, true);
    c.collect_hover_facts = true;
    c.module_scope = super::ModuleScope::Script;
    c.register_prelude();
    assert!(
        !crate::diagnostic::has_errors(&c.engine.diagnostics),
        "prelude failed to load: {:?}",
        c.engine.diagnostics,
    );
    c.process_imports(&block);
    c.env.push_scope();
    c.analyse_module(&block, None);
    c.env.pop_scope();
    assert!(
        !crate::diagnostic::has_errors(&c.engine.diagnostics),
        "snippet failed to compile: {:?}",
        c.engine.diagnostics,
    );
    c
}

mod local_binders_as_definitions {
    //! Local binders (bindings, parameters, pattern binders, `or`-receivers)
    //! must be registered as graph `Definition`s, not merely typed in the env.
    //! Without the record `ReferenceGraph::definition(target)` returns `None`
    //! and goto-def / find-refs / hover are dead on every local. Gated on
    //! `collect_hover_facts`, so `al run` / `al check` are untouched.

    use super::super::*;
    use super::collect;

    /// The single `DefId` declared under `name` in the entry collector.
    fn sole_def(c: &Compiler, name: &str) -> DefId {
        let defs = c.module_refs.defs_named(name);
        assert_eq!(
            defs.len(),
            1,
            "expected exactly one `{name}` def, got {defs:?}"
        );
        defs[0]
    }

    /// Whether any recorded occurrence is an unqualified use of `target`.
    fn has_use(c: &Compiler, target: DefId) -> bool {
        c.module_refs
            .occurrences()
            .iter()
            .map(|o| o.reference)
            .any(|r| r.target == target && r.kind == ReferenceKind::Unqualified)
    }

    #[test]
    fn every_local_binder_kind_is_a_graph_definition() {
        let c = collect(
            "fn ident(p Int) Int {\n\
            \x20 v = p\n\
            \x20 v\n\
            }\n\
            \n\
            fn matcher(o Option(Int)) Int {\n\
            \x20 match o {\n\
            \x20   Some(inner) -> inner\n\
            \x20   None -> 0\n\
            \x20 }\n\
            }\n\
            \n\
            fn recover(r Result(Int, Int)) Int {\n\
            \x20 r or e -> e\n\
            }\n",
        );

        // p: bind_param, v: var binding, inner: pattern binder, e: or-receiver.
        for name in ["p", "v", "inner", "e"] {
            let d = sole_def(&c, name);
            assert_eq!(d.entity, EntityKind::Value, "`{name}` is not a Value def");
            assert!(
                c.module_refs.definition(d).is_some(),
                "`{name}` binder was not registered as a graph Definition",
            );
            // Closes the goto-def chain: occurrence -> target -> definition().
            assert!(has_use(&c, d), "no recorded use targets `{name}`");
        }
    }

    #[test]
    fn goto_def_on_a_local_use_resolves_to_a_real_definition() {
        // Mirrors the handler path: resolve_position(use) -> definition().
        let c = collect("fn f(p Int) Int {\n  v = p\n  v\n}\n");
        let v = sole_def(&c, "v");

        let target = c
            .module_refs
            .resolve_position(2, 2)
            .expect("cursor on the `v` use resolves to a target");
        assert_eq!(target, v);
        assert!(
            c.module_refs.definition(target).is_some(),
            "use resolved to a target with no Definition record",
        );
    }

    #[test]
    fn shadowing_keeps_inner_and_outer_as_distinct_definitions() {
        // `define_at` overwrites the env, so the RHS of `x = x` (compiled
        // before the second binder lands) sees the outer binder and the
        // trailing `x` sees the inner.
        let c = collect("fn f(s Int) Int {\n  x = s\n  x = x\n  x\n}\n");
        let mut defs = c.module_refs.defs_named("x").to_vec();
        assert_eq!(defs.len(), 2, "expected outer + inner `x`, got {defs:?}");
        defs.sort_by_key(|d| d.span.start_line);
        let (outer, inner) = (defs[0], defs[1]);
        assert_ne!(outer, inner);
        assert!(c.module_refs.definition(outer).is_some());
        assert!(c.module_refs.definition(inner).is_some());

        assert_eq!(c.module_refs.resolve_position(2, 6), Some(outer));
        assert_eq!(c.module_refs.resolve_position(3, 2), Some(inner));
    }
}

/// Perceus drop/reuse assertions on the lowered program.
mod perceus_drop {
    use super::{atoms, body_named, drops};
    use crate::core_ir::{Atom, CoreExpr, Program};

    /// Compile `src` and return the lowered program.
    fn lowered(src: &str) -> Program {
        let r = super::compile_script(src);
        assert!(
            !crate::diagnostic::has_errors(&r.diagnostics),
            "snippet failed to compile: {:?}",
            r.diagnostics,
        );
        r.into_runnable().expect("a clean compile is runnable")
    }

    /// The constructors in `e` that reuse a dropped cell.
    fn reusing_ctors(e: &CoreExpr) -> Vec<&Atom> {
        atoms(e)
            .into_iter()
            .filter(|a| matches!(a, Atom::Ctor { reuse: Some(_), .. }))
            .collect()
    }

    #[test]
    fn a_heap_local_is_dropped_once_and_an_int_never() {
        // `p` is heap-shaped and read twice, so it is dropped once, after the
        // second read. `n` is an Int and gets no drop at all.
        let p = lowered(
            "fn f(p (Int, Int), n Int) Int {\n\
            \x20 a = p.0\n\
            \x20 b = p.1\n\
            \x20 a + b + n\n\
            }\n\
            f((1, 2), 3)\n",
        );
        let f = body_named(&p, "f");
        assert_eq!(
            drops(&f.body),
            vec![f.params[0].id()],
            "exactly `p` is dropped:\n{f}"
        );
    }

    #[test]
    fn an_unboxed_prim_is_never_dropped() {
        let p = lowered("fn g(x Int) Int { x + x }\ng(1)\n");
        let g = body_named(&p, "g");
        assert!(
            drops(&g.body).is_empty(),
            "an Int local is never dropped:\n{g}"
        );
    }

    #[test]
    fn a_destructured_cell_is_reused_by_a_same_shaped_constructor() {
        // Canonical Perceus shape: destructure a Cons, construct a same-arity
        // Cons in the arm body, pairing the dropped cell with the constructor.
        let p = lowered(
            "type List {\n\tLNil\n\tLCons(head Int, tail List)\n}\n\
             fn lmap(xs List, f fn(Int) Int) List {\n\
             \x20 match xs {\n\
             \x20   LNil -> LNil\n\
             \x20   LCons(h, t) -> LCons(f(h), lmap(t, f))\n\
             \x20 }\n\
             }\n\
             lmap(LNil, fn(x) { x })\n",
        );
        let lmap = body_named(&p, "lmap");
        let reusing = reusing_ctors(&lmap.body);
        assert_eq!(reusing.len(), 1, "one constructor reuses a cell:\n{lmap}");
        assert!(
            matches!(reusing[0], Atom::Ctor { fields, .. } if fields.len() == 2),
            "the reuse pairs with the two-field LCons, not LNil:\n{lmap}"
        );
    }

    #[test]
    fn a_reuse_candidate_stays_in_its_own_arm() {
        // Reuse pairing is arm-scoped: arm 1's dropped two-field cell must not
        // be consumed by arm 2's constructor, which runs when the cell is a B.
        let p = lowered(
            "type T {\n\tA(x Int, y Int)\n\tB\n}\n\
             fn f(v T) T {\n\
             \x20 match v {\n\
             \x20   A(_x, _y) -> B\n\
             \x20   B -> A(1, 2)\n\
             \x20 }\n\
             }\n\
             f(B)\n",
        );
        let f = body_named(&p, "f");
        assert!(
            reusing_ctors(&f.body).is_empty(),
            "arm 1's candidate must not leak to arm 2's constructor:\n{f}"
        );
    }
}

/// A module the typechecker rejected must never reach the elaborator.
///
/// `CleanModule` is why `lower` and `perceus` need no poison arm: they
/// take a `TypedProgram`, which only `elaborate_body`/`elaborate_toplevel` can
/// build. `Elab` aborts when `resolve_name` returns `None`, so without the
/// gate an ordinary type error would abort the compiler.
mod clean_module_gate {
    use super::super::*;
    use super::parse_ok;
    use crate::parser::new_parser;
    use crate::scanner::new_scanner;

    fn diagnose(src: &str) -> Vec<Diagnostic> {
        super::compile_script(src).diagnostics
    }

    fn codes(ds: &[Diagnostic]) -> Vec<DiagnosticCode> {
        ds.iter()
            .filter(|d| d.severity == crate::diagnostic::Severity::Error)
            .map(|d| d.code)
            .collect()
    }

    /// One ill-typed fn beside a well-typed sibling: the type error is the
    /// only diagnostic, and neither body is elaborated. Reaching the
    /// elaborator would abort the process, so this also pins that a type error
    /// never aborts the compiler.
    #[test]
    fn a_type_error_is_the_only_diagnostic() {
        let ds = diagnose(
            "fn bad(x Int) Int {\n\
             \x20 x + \"not an int\"\n\
             }\n\
             fn good(y Int) Int {\n\
             \x20 y + 1\n\
             }\n\
             good(1)\n",
        );
        let codes = codes(&ds);
        assert_eq!(
            codes,
            vec![DiagnosticCode::TypeError],
            "exactly the one type error, no cascade: {ds:#?}"
        );
    }

    /// The gate is the diagnostics list, not the shape of the offending node:
    /// an unbound name poisons the module just as a mismatch does, and must be
    /// reported once.
    #[test]
    fn an_unbound_name_is_reported_once() {
        let ds = diagnose("fn f() Int { nope() }\nf()\n");
        assert_eq!(
            codes(&ds).len(),
            1,
            "an unbound identifier is one diagnostic, not two: {ds:#?}"
        );
    }

    /// `Expression::ErrorNode` is the only form with nothing to elaborate. The
    /// check walk denies it the `CleanModule` proof, so a plain syntax error
    /// cannot surface as a compiler bug from the elaborator.
    #[test]
    fn an_error_node_denies_the_proof() {
        let mut s = new_scanner("pub fn main() {\n  x = 1 +\n}\n".to_string());
        let pr = new_parser(&mut s).parse_program();
        assert!(
            crate::diagnostic::has_errors(&pr.diagnostics),
            "snippet must fail to parse"
        );
        let r = compile(&ast::Expression::BlockExpression(pr.ast), None);
        assert!(!r.success(), "an unparseable program must not compile");
        assert!(
            codes(&r.diagnostics).contains(&DiagnosticCode::ParseError),
            "the check walk restates the parse error: {:#?}",
            r.diagnostics
        );
    }

    /// And the clean module still elaborates: the gate is not a mute button.
    #[test]
    fn a_clean_module_reaches_the_core_pipeline() {
        let block = parse_ok("fn f(x Int) Int { x + 1 }\npub fn main() { f(1) }\n");
        let r = compile(&ast::Expression::BlockExpression(block), None);
        assert!(r.success(), "{:#?}", r.diagnostics);
        let program = r.into_runnable().expect("a clean compile is runnable");
        let _ = super::body_named(&program, "f");
    }
}

mod toplevel_slot_queue {
    use super::super::*;
    use super::parse_ok;

    /// `toplevel_binds` is positional, so only a module's own statement list
    /// may fill it. Depth alone does not identify that walk: a bare-expression
    /// program runs with `scope_marks` empty, so an arm's pattern binding
    /// would sit at "module depth" and steal the next `let`'s slot.
    #[test]
    fn only_the_module_statement_walk_queues_a_slot() {
        let mut c = new_compiler(None, true);
        c.push_block_scope();
        let a = c.engine.intern("a");
        c.bind_local(a, 7);
        assert!(
            c.toplevel_binds.is_empty(),
            "a binding made outside the module statement walk was queued"
        );

        c.walking_module_statements = true;
        let b = c.engine.intern("b");
        c.bind_local(b, 8);
        assert_eq!(c.toplevel_binds.pop_front(), Some(GlobalSlot(8)));
    }

    /// The bare-expression entry point. Its outermost block is an arm body,
    /// which the elaborator treats as a module toplevel and lets drain the
    /// queue, so the arm's pattern bindings must never have reached it.
    #[test]
    fn a_bare_match_expression_compiles_without_stealing_a_pattern_slot() {
        let src = "match Some(1) { Some(v) -> { w = v + 1\n v + w }\n None -> 0 }";
        let block = parse_ok(src);
        let [ast::Node::Expression(expr)] = &block.body[..] else {
            panic!("expected a single bare expression");
        };
        let r = compile(expr, None);
        assert!(
            !crate::diagnostic::has_errors(&r.diagnostics),
            "{:?}",
            r.diagnostics
        );
    }
}

mod qualified_ctor_pattern_occurrences {
    //! A qualified constructor pattern (`io.NotFound(p)`) must record the same
    //! occurrence pair as the expression path: a `Qualified` use of the ctor
    //! plus a `Qualifier` occurrence on the module alias. Without the pair,
    //! unused-import liveness and rename are blind to modules referenced only
    //! from patterns.

    use super::super::*;
    use super::collect;

    #[test]
    fn qualified_ctor_pattern_records_qualified_plus_qualifier() {
        let src = "import scarlet/io\n\
            x = match io.read_text(\"nope\") {\n\
            \x20 Ok(s) -> s\n\
            \x20 Err(io.NotFound(p)) -> p\n\
            \x20 Err(_) -> \"other\"\n\
            }\n\
            println(x)\n";
        let c = collect(src);

        // The qualified pattern head sits on 0-based line 3. `Err` and the
        // body's `p` on that line are ordinary Unqualified occurrences; the
        // call's own pair is on line 1, outside the filter.
        let on_pattern: Vec<Reference> = c
            .module_refs
            .occurrences()
            .iter()
            .map(|o| o.reference)
            .filter(|r| r.span.start_line == 3)
            .collect();

        let qualified: Vec<_> = on_pattern
            .iter()
            .filter(|r| r.kind == ReferenceKind::Qualified)
            .collect();
        assert_eq!(
            qualified.len(),
            1,
            "expected exactly one Qualified occurrence (the ctor), got {on_pattern:?}"
        );
        assert_eq!(
            qualified[0].target.entity,
            EntityKind::Constructor,
            "the Qualified occurrence targets the constructor"
        );

        let alias = c
            .module_refs
            .defs_named("io")
            .iter()
            .find(|d| d.entity == EntityKind::ModuleAlias)
            .copied()
            .expect("`import scarlet/io` registers a ModuleAlias def");
        let qualifiers: Vec<_> = on_pattern
            .iter()
            .filter(|r| r.kind == ReferenceKind::Qualifier)
            .collect();
        assert_eq!(
            qualifiers.len(),
            1,
            "expected exactly one Qualifier occurrence (the alias), got {on_pattern:?}"
        );
        assert_eq!(
            qualifiers[0].target, alias,
            "the Qualifier occurrence targets the `io` module alias"
        );

        // The correct pair must replace the Unqualified record, not merely
        // join it. `Err` on the same line legitimately records Unqualified, so
        // only the qualified ctor's def is checked.
        assert!(
            !on_pattern
                .iter()
                .any(|r| r.kind == ReferenceKind::Unqualified && r.target == qualified[0].target),
            "no Unqualified occurrence may target the qualified ctor: {on_pattern:?}"
        );
    }
}

mod runnable_programs {
    //! Two facts a `CompileResult` must never confuse: what analysis saw, and
    //! whether there is a program worth running. A rejected module records no
    //! toplevel, so running what it built would run the stdlib init and stop
    //! — and the pre-filled globals would make that look like a computed `0`
    //! rather than a failure.
    //!
    //! Also: a REPL entry is a fragment of a session, not a whole program, so
    //! the line that uses a binding is typed next. That is why the prompt
    //! turns the unused-binding check off — and why turning it off must mean
    //! "do not report", never "report and skip the lowering".

    use super::parse_ok;
    use crate::ast;
    use crate::bytecode::{
        CompileOptions, ModuleScope, UnusedBindings, check, compile, compile_with,
    };
    use crate::core_ir::{Atom, Const, CoreExpr};

    fn entry(src: &str) -> ast::Expression {
        ast::Expression::BlockExpression(parse_ok(src))
    }

    #[test]
    fn an_unused_binding_is_reported_in_a_file() {
        let result = compile(&entry("pub fn main() {\n  x = 5\n  42\n}\n"), None);
        assert!(
            !result.success(),
            "a file's unused binding must be an error"
        );
    }

    /// The shape of a real bug: the REPL filtered a diagnostic it did not want
    /// to show, `success()` then said yes, and the `Program` it ran had no
    /// toplevel — so the entry "evaluated" to one of the entry frame's
    /// pre-filled globals. Whether a program is runnable is now the recorded
    /// toplevel's answer, not the diagnostics'.
    #[test]
    fn a_rejected_compile_stays_unrunnable_even_with_its_diagnostics_removed() {
        let mut result = compile(&entry("pub fn main() {\n  x = 5\n  42\n}\n"), None);
        assert!(!result.success(), "an unused binding rejects a file");
        result.diagnostics.clear();
        assert!(result.success(), "the filtered result looks clean");
        assert!(
            result.into_runnable().is_none(),
            "a program whose toplevel was never recorded must not be runnable"
        );
    }

    /// A check records no toplevel: analysis, never a run. It also does not
    /// need a `main`: a library file checks.
    #[test]
    fn a_check_has_nothing_to_run() {
        let src = entry("pub const x = 1\n");
        let checked = check(&src, None);
        assert!(checked.success(), "{:?}", checked.diagnostics);
        assert!(check(&src, None).into_runnable().is_none());
    }

    /// The atom a toplevel ends in, past its `Let`/`Drop` spine.
    fn tail(mut e: &CoreExpr) -> &Atom {
        loop {
            match e {
                CoreExpr::Let { body, .. }
                | CoreExpr::LetJoin { body, .. }
                | CoreExpr::LetCont { body, .. }
                | CoreExpr::Drop { body, .. } => e = body,
                CoreExpr::Tail(a) => return a,
                CoreExpr::If { .. } | CoreExpr::Match { .. } | CoreExpr::Goto(_) => {
                    panic!("toplevel ends in a branch")
                }
            }
        }
    }

    /// A program is entered at `main`, which the program names by index.
    #[test]
    fn a_program_starts_at_main() {
        let result = compile(&entry("pub fn main() {\n  42\n}\n"), None);
        assert!(result.success(), "{:?}", result.diagnostics);
        let program = result.into_runnable().expect("a clean compile is runnable");
        let main = program
            .main
            .expect("a program with `pub fn main` starts there");
        assert_eq!(program.fns[main].name, "main");
    }

    fn messages(src: &str) -> Vec<String> {
        compile(&entry(src), None)
            .diagnostics
            .into_iter()
            .map(|d| d.message)
            .collect()
    }

    #[test]
    fn a_program_without_main_does_not_run_but_checks() {
        let src = "fn helper() Int {\n  1\n}\n\npub fn exported() Int {\n  helper()\n}\n";
        assert!(
            messages(src)
                .iter()
                .any(|m| m.starts_with("No `main` function")),
            "{:?}",
            messages(src)
        );
        assert!(check(&entry(src), None).success());
    }

    #[test]
    fn main_must_be_public_and_take_nothing() {
        let private = messages("fn main() {\n  1\n}\n");
        assert!(
            private.iter().any(|m| m.contains("`main` must be public")),
            "{private:?}"
        );
        let with_args = messages("pub fn main(x Int) Int {\n  x\n}\n");
        assert!(
            with_args.iter().any(|m| m.contains("takes no parameters")),
            "{with_args:?}"
        );
        // Both are reported by `check` too: a malformed entry point is a
        // fact about the file, not about running it.
        let checked = check(&entry("fn main() {\n  1\n}\n"), None);
        assert!(!checked.success());
    }

    #[test]
    fn statements_at_module_scope_are_rejected_in_a_program() {
        let ms = messages("println(1)\n\npub fn main() {\n  2\n}\n");
        assert!(
            ms.iter()
                .any(|m| m.starts_with("Statements are not allowed at module scope")),
            "{ms:?}"
        );
    }

    /// The REPL's mode: statements are the input, bindings persist as
    /// module-scope binds, and the entry's value is its tail expression — no
    /// `main` involved.
    #[test]
    fn a_script_entry_runs_its_statements_and_leaves_the_tail() {
        let result = compile_with(
            &entry("x = 5\n42\n"),
            CompileOptions {
                unused_bindings: UnusedBindings::Ignore,
                module_scope: ModuleScope::Script,
                ..CompileOptions::default()
            },
        );
        assert!(result.success(), "{:?}", result.diagnostics);
        let program = result.into_runnable().expect("a clean compile is runnable");
        assert!(program.main.is_none(), "a script has no `main`");
        let Atom::Const(c) = tail(&program.toplevel.core.body) else {
            panic!(
                "the script's tail is not a constant:\n{}",
                program.toplevel.core
            );
        };
        assert_eq!(program.consts[c.0 as usize], Const::Int(42));
    }
}

mod ctor_visibility_survives_on_the_type {
    //! `analyse_type_decl` computes `is_public && !opaque` to decide what goes
    //! in the module interface, then dropped it. `wire`'s descriptor builder
    //! asks the same question of a type it did not declare — `decode` builds
    //! values by constructor without running any of the declaring module's
    //! code — so the bit has to survive on the body.
    //!
    //! Each case is asserted on its own. The four answers come from three
    //! independent inputs (`pub`, `opaque`, having a body at all), and one
    //! aggregate over them would be carried by whichever happened to be
    //! wrong last.

    use super::collect;

    fn ctors_public(src: &str, ty: &str) -> Option<bool> {
        collect(src).env.lookup_type_info(ty)?.ctors_public()
    }

    #[test]
    fn a_pub_type_exposes_its_constructors() {
        assert_eq!(
            ctors_public("pub type Colour {\n\tRed\n\tBlue\n}\n", "Colour"),
            Some(true)
        );
    }

    #[test]
    fn an_opaque_type_hides_them() {
        assert_eq!(
            ctors_public("pub opaque type Id {\n\tId(n Int)\n}\n", "Id"),
            Some(false)
        );
    }

    #[test]
    fn a_private_type_hides_them() {
        assert_eq!(
            ctors_public("type Hidden {\n\tHidden(n Int)\n}\n", "Hidden"),
            Some(false)
        );
    }

    #[test]
    fn an_alias_has_no_constructors_to_report() {
        // `None`, never `Some(false)`: a caller building values by
        // constructor must look through an alias, not refuse it. Collapsing
        // this into `false` is how an alias to an encodable type would come
        // back as "opaque".
        assert_eq!(ctors_public("pub type Name = String\n", "Name"), None);
    }
}

/// `scarlet/wire`'s declaration surface: the two `@vm` keys reach the core IR
/// as their intrinsics.
mod wire_surface {
    use crate::core_ir::Atom;
    use scarlet_types::intrinsic::Intrinsic;

    #[test]
    fn a_wire_call_reaches_the_core_ir_as_its_intrinsic() {
        let r = super::compile_script(
            "import scarlet/wire\n\
             b = wire.encode(1)\n\
             match wire.decode(b) {\n\
             \x20 Ok(n) -> n\n\
             \x20 Err(_) -> 0\n\
             }\n",
        );
        assert!(
            !crate::diagnostic::has_errors(&r.diagnostics),
            "snippet failed to compile: {:?}",
            r.diagnostics,
        );
        let program = r.into_runnable().expect("a clean compile is runnable");
        let calls: Vec<Intrinsic> = super::atoms(&program.toplevel.core.body)
            .into_iter()
            .filter_map(|a| {
                if let Atom::Intrinsic { intrinsic, .. } = a {
                    Some(*intrinsic)
                } else {
                    None
                }
            })
            .collect();
        assert!(calls.contains(&Intrinsic::WireEncode), "{calls:?}");
        assert!(calls.contains(&Intrinsic::WireDecode), "{calls:?}");
    }
}

mod typed_prim_ops {
    //! An operator on a type inference has fixed lowers to its typed
    //! [`PrimOp`] (`IntAdd`), never the unresolved one (`Add`) a backend
    //! would have to dispatch on at run time.

    use super::{atoms, body_named, parse_ok};
    use crate::ast;
    use crate::bytecode::compile;
    use crate::core_ir::{Atom, PrimOp};

    /// The prim ops in the body of `name`.
    fn prim_ops(src: &str, name: &str) -> Vec<PrimOp> {
        let result = compile(&ast::Expression::BlockExpression(parse_ok(src)), None);
        assert!(result.success(), "compile failed: {:?}", result.diagnostics);
        let program = result.into_runnable().expect("a clean compile is runnable");
        atoms(&body_named(&program, name).body)
            .into_iter()
            .filter_map(|a| match a {
                Atom::PrimOp { op, .. } => Some(*op),
                _ => None,
            })
            .collect()
    }

    /// `sq(n)` adds a known call's return type to the operands' sources.
    #[test]
    fn int_operators_select_the_int_ops() {
        let ops = prim_ops(
            "fn sq(x Int) Int { x * x }\n\
             fn f(n Int) Int {\n\
             \tif n == 0 { 0 } else { sq(n) + n - 1 }\n\
             }\n\
             pub fn main() {\n\
             \tprintln(f(3))\n\
             }\n",
            "f",
        );
        for typed in [PrimOp::IntEq, PrimOp::IntAdd, PrimOp::IntSub] {
            assert!(ops.contains(&typed), "{typed:?} not selected: {ops:?}");
        }
        for unresolved in [PrimOp::Eq, PrimOp::Add, PrimOp::Sub] {
            assert!(!ops.contains(&unresolved), "{unresolved:?} leaked: {ops:?}");
        }
    }

    /// `v` is `Int` only because inference unified `Some`'s payload with the
    /// literal `3`, so lowering must read the solved type back rather than
    /// re-instantiate `Some`'s scheme.
    ///
    /// `fn g(a, b) { a + b }` would not test this: it really is
    /// `Addable a => (a, a) -> a`, one body for every instantiation, and the
    /// unresolved `Add` is correct there.
    #[test]
    fn an_operand_typed_only_by_inference_selects_the_int_op() {
        let ops = prim_ops(
            "fn f() Int {\n\
             \tmatch Some(3) {\n\
             \t\tNone -> 0\n\
             \t\tSome(v) -> v + 1\n\
             \t}\n\
             }\n\
             pub fn main() {\n\
             \tprintln(f())\n\
             }\n",
            "f",
        );
        assert!(
            ops.contains(&PrimOp::IntAdd),
            "IntAdd not selected: {ops:?}"
        );
        assert!(!ops.contains(&PrimOp::Add), "Add leaked: {ops:?}");
    }
}
