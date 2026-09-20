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

/// Perceus drop/reuse assertions on the `lower → perceus → emit` output.
mod perceus_drop {
    use super::super::*;

    /// Compile `src` with codegen on and return the emitted instructions.
    fn emitted(src: &str) -> Vec<crate::bytecode::Instruction> {
        let r = super::compile_script(src);
        assert!(
            !crate::diagnostic::has_errors(&r.diagnostics),
            "snippet failed to compile: {:?}",
            r.diagnostics,
        );
        r.into_runnable()
            .expect("a non-check compile emits")
            .program
            .code
    }

    #[test]
    fn drop_slot_emitted_at_heap_local_last_use() {
        // `p` is heap-shaped and read twice, so `Drop 0` belongs right after
        // the second read's `Let`. Int local `n` gets no Drop.
        let code = emitted(
            "fn f(p (Int, Int), n Int) Int {\n\
            \x20 a = p.0\n\
            \x20 b = p.1\n\
            \x20 a + b + n\n\
            }\n\
            f((1, 2), 3)\n",
        );
        let drops: Vec<_> = code.iter().filter(|i| i.op == Op::Drop).collect();
        assert_eq!(drops.len(), 1, "expected one DropSlot, got {drops:?}");
        assert_eq!(drops[0].operand, 0, "DropSlot targets param slot 0 (`p`)");
        let pos = code.iter().position(|i| i.op == Op::Drop).unwrap();
        let last_read = code[..pos]
            .iter()
            .rposition(|i| i.op == Op::PushLocal && i.operand == 0)
            .unwrap();
        assert!(
            code[last_read + 1..pos]
                .iter()
                .all(|i| matches!(i.op, Op::TupleIndex | Op::StoreLocal)),
            "Drop is placed on the spine right after `p.1`'s let",
        );
        assert!(
            !code[pos..]
                .iter()
                .take_while(|i| i.op != Op::Ret)
                .any(|i| i.op == Op::PushLocal && i.operand == 0),
            "no read of `p` after its Drop",
        );
        assert!(!code.iter().any(|i| i.op == Op::Drop && i.operand == 1));
    }

    #[test]
    fn drop_slot_not_emitted_for_unboxed_prim() {
        let code = emitted("fn g(x Int) Int { x + x }\ng(1)\n");
        assert!(
            !code.iter().any(|i| i.op == Op::Drop),
            "Int local must not receive a DropSlot",
        );
    }

    #[test]
    fn reuse_paired_for_match_destructure_then_construct() {
        // Canonical Perceus shape: destructure a Cons, construct a same-arity
        // Cons in the arm body, pairing the dropped cell with the constructor.
        let code = emitted(
            "type List {\n\tLNil\n\tLCons(head Int, tail List)\n}\n\
             fn lmap(xs List, f fn(Int) Int) List {\n\
             \x20 match xs {\n\
             \x20   LNil -> LNil\n\
             \x20   LCons(h, t) -> LCons(f(h), lmap(t, f))\n\
             \x20 }\n\
             }\n\
             lmap(LNil, fn(x) { x })\n",
        );
        let reuse_at = code
            .iter()
            .position(|i| i.op == Op::Reuse)
            .expect("Op::Reuse emitted for LCons arm");
        let ctor = code[reuse_at + 1];
        assert_eq!(ctor.op, Op::MakeEnumPayload);
        assert_eq!(ctor.a, 1, "constructor's a-byte set for in-place reuse");
        assert_eq!(ctor.b, 2, "reuse paired with the 2-arity Cons, not LNil");
        let nil_ctors: Vec<_> = code
            .iter()
            .filter(|i| i.op == Op::MakeEnumPayload && i.b == 0)
            .collect();
        assert!(
            nil_ctors.iter().all(|i| i.a == 0),
            "0-arity constructor must allocate fresh (a=0)",
        );
    }

    #[test]
    fn reuse_candidate_scoped_per_match_arm() {
        // Reuse pairing is arm-scoped: arm 1's dropped 2-field cell must not
        // be consumed by arm 2's constructor. At runtime the slot holds a
        // 0-field cell in arm 2 and `reuse_or_alloc`'s shape assert fires.
        let code = emitted(
            "type T {\n\tA(x Int, y Int)\n\tB\n}\n\
             fn f(v T) T {\n\
             \x20 match v {\n\
             \x20   A(_x, _y) -> B\n\
             \x20   B -> A(1, 2)\n\
             \x20 }\n\
             }\n\
             f(B)\n",
        );
        assert!(
            !code.iter().any(|i| i.op == Op::Reuse),
            "arm 1's Enum/2 candidate must not leak to arm 2's Enum/2 constructor",
        );
    }
}

/// A module the typechecker rejected must never reach the elaborator.
///
/// `CleanModule` is why `lower`, `perceus` and `emit` need no poison arm: they
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
        let r = compile(&ast::Expression::BlockExpression(pr.ast), None, None);
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
        let r = compile(&ast::Expression::BlockExpression(block), None, None);
        assert!(r.success(), "{:#?}", r.diagnostics);
        let emitted = r.into_runnable().expect("a non-check compile emits");
        assert!(
            !emitted.core.fns.is_empty(),
            "a diagnostics-clean module must produce Core"
        );
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
        let r = compile(expr, None, None);
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

mod stdlib_native_gate_probe {
    //! Diagnostic: if the native hook could see stdlib bodies, how many would
    //! the A0 coverage gate accept? Replicates `precompile_stdlib`'s pipeline
    //! with a gate-probing hook installed from the start, so prelude bodies
    //! are probed too. Prints a report under `--nocapture`; the only assertion
    //! is that the diag walker mirrors `plan` exactly.

    use std::cell::RefCell;
    use std::rc::Rc;

    use super::super::*;
    use crate::core_ir::FuncIdx;
    use crate::core_ir::clif;
    use crate::module::{ModuleKey, stdlib};

    /// Prelude bindings from a first, hook-free run. Stdlib compilation is
    /// deterministic, so these identities hold for the probed second run.
    fn prelude_bindings() -> PreludeBindings {
        let mut c = new_compiler(None, false);
        c.register_prelude();
        assert!(!crate::diagnostic::has_errors(c.diagnostics()));
        c.prelude.clone()
    }

    struct Probe {
        idx: FuncIdx,
        plan: clif::NativePlan,
    }

    /// Every stdlib body must survive the Core IR byte codec exactly — the
    /// static blob will ship these bytes, so a lossy encode here is a
    /// miscompiled stdlib at every startup. Equality is the golden renderer's,
    /// the same notion the `.core` snapshots pin.
    #[test]
    fn every_stdlib_body_round_trips_through_the_codec() {
        type Captured = Vec<(FuncIdx, String, Vec<u8>)>;
        let bodies: Rc<RefCell<Captured>> = Rc::default();
        let sink = Rc::clone(&bodies);
        let mut c = new_compiler(None, false);
        c.native_hook = Some(Box::new(move |idx, f, _pool, _counts| {
            sink.borrow_mut()
                .push((idx, format!("{f}"), crate::core_ir::codec::encode_fn(f)));
        }));
        let at = crate::span::Span::DUMMY;
        c.register_prelude();
        for path in stdlib::all_modules() {
            c.load_module(&crate::ast::ImportPath::canonical(path.clone()), at);
        }
        assert!(!crate::diagnostic::has_errors(c.diagnostics()));
        let bodies = bodies.take();
        assert!(bodies.len() > 200, "hook saw {} bodies", bodies.len());
        let mut total = 0usize;
        for (idx, rendered, bytes) in &bodies {
            total += bytes.len();
            let back = crate::core_ir::codec::decode_fn(bytes)
                .unwrap_or_else(|e| panic!("fn#{} failed to decode: {e}", idx.index()));
            assert_eq!(
                *rendered,
                format!("{back}"),
                "fn#{} changed across the codec",
                idx.index()
            );
        }
        println!(
            "codec round-tripped {} bodies, {} KiB total",
            bodies.len(),
            total / 1024
        );
    }

    /// Every stdlib body must reach native code. There is no admission gate
    /// any more — `plan` is infallible — so this asserts the *compile* step
    /// covers all of them, and prints the per-module breakdown.
    #[test]
    fn every_stdlib_body_compiles_to_native() {
        let prelude = prelude_bindings();
        let probes: Rc<RefCell<Vec<Probe>>> = Rc::default();
        let sink = Rc::clone(&probes);
        let mut c = new_compiler(None, false);
        c.native_hook = Some(Box::new(move |idx, f, pool, counts| {
            let plan = clif::plan(idx, f, pool, &prelude, counts);
            sink.borrow_mut().push(Probe { idx, plan });
        }));

        let at = crate::span::Span::DUMMY;
        c.register_prelude();
        assert!(!crate::diagnostic::has_errors(c.diagnostics()));
        let mut bounds = vec![("al (prelude)".to_string(), probes.borrow().len())];
        for path in stdlib::all_modules() {
            c.load_module(&crate::ast::ImportPath::canonical(path.clone()), at);
            assert!(
                !crate::diagnostic::has_errors(c.diagnostics()),
                "errors compiling {}",
                ModuleKey::for_stdlib(&path).as_str()
            );
            bounds.push((
                ModuleKey::for_stdlib(&path).as_str().to_string(),
                probes.borrow().len(),
            ));
        }

        let probes = probes.take();
        let fn_name = |idx: FuncIdx| c.program.functions[idx.index()].name.to_string();
        let idxs: Vec<FuncIdx> = probes.iter().map(|p| p.idx).collect();
        let plans: Vec<clif::NativePlan> = probes.into_iter().map(|p| p.plan).collect();
        let compilable = clif::native_set(&plans, &c.program, &c.frame_layouts);

        println!("== stdlib native coverage ==");
        println!("function table size: {}", c.program.functions.len());
        println!("bodies seen by hook: {}", idxs.len());
        println!("compile to native:   {}", compilable.len());

        println!("\n-- per module --");
        let mut prev = 0usize;
        for (name, upto) in &bounds {
            if *upto > prev {
                let pass = idxs[prev..*upto]
                    .iter()
                    .filter(|i| compilable.contains(i))
                    .count();
                println!("{:24} {:3} / {:3}", name, pass, *upto - prev);
            }
            prev = *upto;
        }

        let missing: Vec<FuncIdx> = idxs
            .iter()
            .copied()
            .filter(|i| !compilable.contains(i))
            .collect();
        assert!(
            missing.is_empty(),
            "these stdlib bodies did not compile to native: {:?}",
            missing
                .iter()
                .map(|i| format!("fn#{} {}", i.index(), fn_name(*i)))
                .collect::<Vec<_>>()
        );
    }
}

mod runnable_programs {
    //! Two facts a `CompileResult` must never confuse: what analysis built,
    //! and whether there is a program worth running. A rejected module emits
    //! no toplevel, so its `Program` runs the stdlib init and halts — and the
    //! entry frame's pre-filled locals make that look like a computed `0`
    //! rather than a failure.
    //!
    //! Also: a REPL entry is a fragment of a session, not a whole program, so
    //! the line that uses a binding is typed next. That is why the prompt
    //! turns the unused-binding check off — and why turning it off must mean
    //! "do not report", never "report and skip the emit".

    use super::parse_ok;
    use crate::ast;
    use crate::bytecode::{
        CompileOptions, ModuleScope, Op, UnusedBindings, check, compile, compile_with,
    };

    fn entry(src: &str) -> ast::Expression {
        ast::Expression::BlockExpression(parse_ok(src))
    }

    #[test]
    fn an_unused_binding_is_reported_in_a_file() {
        let result = compile(&entry("pub fn main() {\n  x = 5\n  42\n}\n"), None, None);
        assert!(
            !result.success(),
            "a file's unused binding must be an error"
        );
    }

    /// The shape of a real bug: the REPL filtered a diagnostic it did not want
    /// to show, `success()` then said yes, and the `Program` it ran had no
    /// toplevel — so the entry "evaluated" to one of the entry frame's
    /// pre-filled locals. Whether a program is runnable is now the splice's
    /// answer, not the diagnostics'.
    #[test]
    fn a_rejected_compile_stays_unrunnable_even_with_its_diagnostics_removed() {
        let mut result = compile(&entry("pub fn main() {\n  x = 5\n  42\n}\n"), None, None);
        assert!(!result.success(), "an unused binding rejects a file");
        result.diagnostics.clear();
        assert!(result.success(), "the filtered result looks clean");
        assert!(
            result.into_runnable().is_none(),
            "a program whose toplevel was never emitted must not be runnable"
        );
    }

    /// A check builds the function table but splices no toplevel: analysis,
    /// never a run. It also does not need a `main`: a library file checks.
    #[test]
    fn a_check_has_artifacts_but_nothing_to_run() {
        let src = entry("pub const x = 1\n");
        let checked = check(&src, None, None);
        assert!(checked.success(), "{:?}", checked.diagnostics);
        assert!(check(&src, None, None).into_runnable().is_none());
        assert!(check(&src, None, None).into_artifacts().is_some());
    }

    /// The last three instructions of a program's entry frame after the
    /// declarations have been initialised.
    fn entry_tail(src: &str) -> Vec<Op> {
        let result = compile(&entry(src), None, None);
        assert!(result.success(), "{:?}", result.diagnostics);
        let emitted = result.into_runnable().expect("a successful compile emits");
        let mut ops: Vec<Op> = emitted
            .program
            .code
            .iter()
            .rev()
            .take(3)
            .map(|i| i.op)
            .collect();
        ops.reverse();
        ops
    }

    /// A program is entered at `main`: the toplevel's own Nil is popped and
    /// `main` is called by index, so its result is what `Halt` sees.
    #[test]
    fn a_program_starts_at_main() {
        assert_eq!(
            entry_tail("pub fn main() {\n  42\n}\n"),
            vec![Op::Pop, Op::CallKnown, Op::Halt]
        );
    }

    fn messages(src: &str) -> Vec<String> {
        compile(&entry(src), None, None)
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
        assert!(check(&entry(src), None, None).success());
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
        let checked = check(&entry("fn main() {\n  1\n}\n"), None, None);
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
    /// module-scope binds, and the entry's value is its tail expression,
    /// left on the stack for the `Halt` the REPL reads — no `main` involved.
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
        let emitted = result.into_runnable().expect("a successful compile emits");
        let ops: Vec<Op> = emitted
            .program
            .code
            .iter()
            .rev()
            .take(3)
            .map(|i| i.op)
            .collect();
        assert_eq!(
            ops,
            vec![Op::Halt, Op::PushConst, Op::StoreLocal],
            "the toplevel was not emitted: {ops:?}"
        );
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

/// `scarlet/wire`'s declaration surface: the two `@vm` keys reach the two new
/// opcodes, and a program that calls them binds `DecodeError`'s ABI slots.
mod wire_surface {
    use super::super::*;

    /// A clean compile is half the assertion here. `bind_abi` refuses a
    /// program whose emitted ops construct unbound slots, and `slots_for`
    /// declares all five `DecodeError` slots against `WireDecode` — so a
    /// constructor renamed out from under `BINDINGS`, or one whose arity no
    /// longer matches its slot, is an error in `r.diagnostics`, not a
    /// mis-built value at runtime.
    ///
    /// The ops themselves have no bodies yet; nothing here runs them.
    #[test]
    fn a_wire_call_emits_its_op_and_binds_the_decode_error_slots() {
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
        let code = r
            .into_runnable()
            .expect("a non-check compile emits")
            .program
            .code;
        assert!(
            code.iter().any(|i| i.op == Op::WireEncode),
            "wire.encode must reach Op::WireEncode",
        );
        assert!(
            code.iter().any(|i| i.op == Op::WireDecode),
            "wire.decode must reach Op::WireDecode",
        );
    }
}

/// `bind_abi` rebuilds the ABI prefix and records its length so a later
/// emit can append descriptor templates past it.
mod abi_prefix {
    use super::super::*;
    use scarlet_vm::template::EnumTemplate;

    #[test]
    fn bind_abi_records_the_prefix_and_drops_a_suffix_on_rebuild() {
        let mut c = new_compiler(None, false);
        c.register_prelude();
        assert!(
            !crate::diagnostic::has_errors(c.diagnostics()),
            "prelude failed to load: {:?}",
            c.diagnostics(),
        );
        c.bind_abi();
        let n = c.abi_template_count;
        assert_eq!(n, c.program.templates.len());
        assert!(
            n > 0,
            "prelude must bind at least Nil/Option/Result, got an empty prefix"
        );

        let extra = EnumTemplate::build(
            &mut c.program.frozen.builder(),
            crate::type_def::TypeId(0),
            0,
            "Desc",
            "V",
            &[],
        );
        let suffix = c.program.templates.push(extra);
        assert_eq!(suffix.index(), n);
        assert_eq!(c.program.templates.len(), n + 1);
        assert_eq!(c.abi_template_count, n);

        // The reset stays: a later emit rebuilds the prefix and the suffix
        // is gone, so a TemplateIdx minted against the previous table cannot
        // silently name a different constructor.
        c.bind_abi();
        assert_eq!(c.program.templates.len(), c.abi_template_count);
        assert!(
            c.program.templates.get(suffix).is_none(),
            "bind_abi must drop a template appended past the previous prefix"
        );
    }
}
