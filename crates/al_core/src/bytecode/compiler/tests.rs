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

/// Compile `src` as the entry module on the LSP path, with
/// `collect_hover_facts` on so occurrence collection fires.
fn collect(src: &str) -> super::Compiler {
    let block = parse_ok(src);
    let mut c = super::new_compiler(None, true);
    c.collect_hover_facts = true;
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
    use super::parse_ok;

    /// Compile `src` with codegen on and return the emitted instructions.
    fn emitted(src: &str) -> Vec<crate::bytecode::Instruction> {
        let block = parse_ok(src);
        let r = compile(&ast::Expression::BlockExpression(block), None, None);
        assert!(
            !crate::diagnostic::has_errors(&r.diagnostics),
            "snippet failed to compile: {:?}",
            r.diagnostics,
        );
        r.emitted.expect("a non-check compile emits").program.code
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
        let block = parse_ok(src);
        compile(&ast::Expression::BlockExpression(block), None, None).diagnostics
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
        let mut s = new_scanner("x = 1 +\n".to_string());
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
        let block = parse_ok("fn f(x Int) Int { x + 1 }\nf(1)\n");
        let r = compile(&ast::Expression::BlockExpression(block), None, None);
        assert!(r.success(), "{:#?}", r.diagnostics);
        let emitted = r.emitted.expect("a non-check compile emits");
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
        let src = "import al/io\n\
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
            .expect("`import al/io` registers a ModuleAlias def");
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
    use std::collections::{BTreeMap, BTreeSet};
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
        plan: Option<clif::NativePlan>,
        reasons: Vec<String>,
    }

    #[test]
    fn stdlib_gate_coverage_report() {
        let prelude = prelude_bindings();
        let probes: Rc<RefCell<Vec<Probe>>> = Rc::default();
        let sink = Rc::clone(&probes);
        let mut c = new_compiler(None, false);
        c.native_hook = Some(Box::new(move |idx, f, pool| {
            let plan = clif::plan(idx, f, pool, &prelude);
            let reasons = clif::gate_diag::reasons(f, pool, &prelude);
            sink.borrow_mut().push(Probe { idx, plan, reasons });
        }));

        let at = crate::span::Span::DUMMY;
        c.register_prelude();
        assert!(!crate::diagnostic::has_errors(c.diagnostics()));
        // Hook-fire counts per module, in load order.
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

        for p in &probes {
            assert_eq!(
                p.plan.is_some(),
                p.reasons.is_empty(),
                "gate_diag disagrees with plan on fn#{} {}: {:?}",
                p.idx.index(),
                fn_name(p.idx),
                p.reasons
            );
        }

        let fire_order: Vec<bool> = probes.iter().map(|p| p.plan.is_some()).collect();
        let failed: Vec<(FuncIdx, Vec<String>)> = probes
            .iter()
            .filter(|p| p.plan.is_none())
            .map(|p| (p.idx, p.reasons.clone()))
            .collect();
        let plans: Vec<clif::NativePlan> = probes.into_iter().filter_map(|p| p.plan).collect();
        let compilable = clif::native_set(&plans, &c.program);

        println!("== stdlib native-gate probe ==");
        println!("function table size:     {}", c.program.functions.len());
        println!("bodies seen by hook:     {}", fire_order.len());
        println!("pass A0 coverage gate:   {}", plans.len());
        println!(
            "also pass compile gates: {}  (consts / ctor-sites / use-scan)",
            compilable.len()
        );

        println!("\n-- per module (gate-pass / hooked) --");
        let mut prev = 0usize;
        for (name, upto) in &bounds {
            if *upto > prev {
                let pass = fire_order[prev..*upto].iter().filter(|ok| **ok).count();
                println!("{:24} {:3} / {:3}", name, pass, *upto - prev);
            }
            prev = *upto;
        }

        println!("\n-- blocking reasons (count = rejected fns containing it) --");
        let mut blocking: BTreeMap<&str, usize> = BTreeMap::new();
        for (_, reasons) in &failed {
            for r in reasons.iter().map(String::as_str).collect::<BTreeSet<_>>() {
                *blocking.entry(r).or_default() += 1;
            }
        }
        let mut blocking: Vec<_> = blocking.into_iter().collect();
        blocking.sort_by_key(|(_, n)| std::cmp::Reverse(*n));
        for (r, n) in &blocking {
            println!("{n:4}  {r}");
        }

        println!("\n-- rejected fns and their distinct blockers --");
        for (idx, reasons) in &failed {
            let distinct: BTreeSet<&str> = reasons.iter().map(String::as_str).collect();
            println!(
                "fn#{:<4} {:32} {}",
                idx.index(),
                fn_name(*idx),
                distinct.into_iter().collect::<Vec<_>>().join(" ")
            );
        }

        println!("\n-- gate-passing fns --");
        for p in &plans {
            let jit = if compilable.contains(&p.func_idx) {
                "jit"
            } else {
                "PLAN-ONLY"
            };
            println!(
                "fn#{:<4} {:32} {}",
                p.func_idx.index(),
                fn_name(p.func_idx),
                jit
            );
        }
    }
}
