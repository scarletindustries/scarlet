#[cfg(test)]
mod bug2_local_binders_as_definitions {
    //! Local binders — `name = ..` bindings, fn/lambda parameters, pattern
    //! binders (match arms, tuple/array/ctor destructure), and `or`-receivers —
    //! must be registered as graph `Definition`s, not merely typed in the env.
    //! Without the `Definition` record `ReferenceGraph::definition(target)`
    //! returns `None`, so goto-def / find-refs / hover (the handler path, which
    //! goes through `definition_at` / `references_to`) are dead on every local.
    //! Emission is gated on `collect_hover_facts`, so this is LSP-only and the
    //! `al run` / `al check` graph stays untouched.

    use super::super::*;
    use crate::parser::new_parser;
    use crate::scanner::new_scanner;

    /// Compile `src` as the entry module on the LSP path (`collect_hover_facts`
    /// on, so local-def emission fires) and hand back the populated collector.
    fn collect(src: &str) -> Compiler {
        let mut s = new_scanner(src.to_string());
        let pr = new_parser(&mut s).parse_program();
        assert!(
            !crate::diagnostic::has_errors(&pr.diagnostics),
            "snippet failed to parse: {:?}",
            pr.diagnostics,
        );
        let block = pr.ast;
        let mut c = new_compiler(None, true);
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

    /// Whether any recorded occurrence is an unqualified *use* of `target`.
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
            // The use site already recorded an Unqualified occurrence targeting
            // this binder; with the Definition present the goto-def / find-refs
            // chain (occurrence -> target -> definition()) now closes.
            assert!(has_use(&c, d), "no recorded use targets `{name}`");
        }
    }

    #[test]
    fn goto_def_on_a_local_use_resolves_to_a_real_definition() {
        // Mirrors the handler path: resolve_position(use) -> target, then
        // definition(target) — the latter returned `None` before the fix.
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
        // Two sequential `x` binders -> two distinct DefIds (distinct identifier
        // spans). `define_at` overwrites the env, so the RHS of `x = x` (compiled
        // before the second binder lands) sees the outer and the trailing `x`
        // the inner.
        let c = collect("fn f(s Int) Int {\n  x = s\n  x = x\n  x\n}\n");
        let mut defs = c.module_refs.defs_named("x").to_vec();
        assert_eq!(defs.len(), 2, "expected outer + inner `x`, got {defs:?}");
        defs.sort_by_key(|d| d.span.start_line);
        let (outer, inner) = (defs[0], defs[1]);
        assert_ne!(outer, inner);
        assert!(c.module_refs.definition(outer).is_some());
        assert!(c.module_refs.definition(inner).is_some());

        // RHS use on line 2 targets the OUTER binder; the trailing `x` on line 3
        // targets the INNER one.
        assert_eq!(c.module_refs.resolve_position(2, 6), Some(outer));
        assert_eq!(c.module_refs.resolve_position(3, 2), Some(inner));
    }
}

/// Perceus drop/reuse assertions on emitted bytecode: validate the
/// `lower → perceus → emit` pipeline output.
#[cfg(test)]
mod perceus_drop {
    use super::super::*;
    use crate::parser::new_parser;
    use crate::scanner::new_scanner;

    /// Compile `src` for real (codegen on) and return the emitted instruction
    /// stream, so tests can assert on generated ops.
    fn emitted(src: &str) -> Vec<crate::bytecode::Instruction> {
        let mut s = new_scanner(src.to_string());
        let pr = new_parser(&mut s).parse_program();
        assert!(
            !crate::diagnostic::has_errors(&pr.diagnostics),
            "snippet failed to parse: {:?}",
            pr.diagnostics,
        );
        let r = compile(&ast::Expression::BlockExpression(pr.ast), None, None);
        assert!(
            !crate::diagnostic::has_errors(&r.diagnostics),
            "snippet failed to compile: {:?}",
            r.diagnostics,
        );
        r.program.code
    }

    #[test]
    fn drop_slot_emitted_at_heap_local_last_use() {
        // `p` is a `(Int, Int)` tuple → heap-shaped. It is read twice; the
        // second read is its last use, so the Core perceus pass must insert
        // `Drop 0` immediately after that read's `Let` (`PushLocal 0;
        // TupleIndex; StoreLocal`). Int local `n` gets no Drop.
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
        // Drop sits right after the last read of `p` (the second `TupleIndex`
        // let), before any use of `a`/`b`/`n`.
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
        // Cons in the arm body. Core perceus pairs the scrutinee's dropped
        // cell with the arm's constructor — `Reuse slot; MakeEnumPayload a=1`.
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
        // Arm 1 destructures a 2-field variant but constructs nothing. Arm 2
        // (0-field variant) constructs a 2-field value. Reuse pairing is
        // arm-scoped: arm 1's dropped cell must not be consumed by arm 2's
        // constructor — at runtime the slot holds a 0-field cell in arm 2 and
        // the debug shape assert in `reuse_or_alloc` would fire.
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
/// `CleanModule` is what makes that true, and it is the whole reason `lower`,
/// `perceus` and `emit` need no poison arm: their input is a `TypedProgram`,
/// and only `typed_ir::elaborate_body`/`elaborate_toplevel` can build one.
/// `Elab` aborts whenever `resolve_name` returns `None`, or the check walk's
/// recorded types run out under it — exactly what a subtree inference never
/// resolved looks like — so without the gate an ordinary type error would reach
/// `typed_ir::elaborator_bug` and abort the compiler.
#[cfg(test)]
mod clean_module_gate {
    use super::super::*;
    use crate::parser::new_parser;
    use crate::scanner::new_scanner;

    fn diagnose(src: &str) -> Vec<Diagnostic> {
        let mut s = new_scanner(src.to_string());
        let pr = new_parser(&mut s).parse_program();
        assert!(
            !crate::diagnostic::has_errors(&pr.diagnostics),
            "snippet failed to parse: {:?}",
            pr.diagnostics,
        );
        compile(&ast::Expression::BlockExpression(pr.ast), None, None).diagnostics
    }

    fn codes(ds: &[Diagnostic]) -> Vec<DiagnosticCode> {
        ds.iter()
            .filter(|d| d.severity == crate::diagnostic::Severity::Error)
            .map(|d| d.code)
            .collect()
    }

    /// One ill-typed fn beside a well-typed sibling: the type error is the only
    /// error. The sibling is parked and closed out empty; neither body is
    /// elaborated, because the proof cannot be minted for the module. Reaching
    /// the elaborator here would abort the process in `typed_ir::elaborator_bug`, so this
    /// test also pins that a type error never aborts the compiler.
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
    /// an unbound name poisons the module for the elaborator just as a mismatch
    /// does, and `resolve_name` returning `None` must not be reported twice —
    /// nor reach `typed_ir::elaborator_bug`.
    #[test]
    fn an_unbound_name_is_reported_once() {
        let ds = diagnose("fn f() Int { nope() }\nf()\n");
        assert_eq!(
            codes(&ds).len(),
            1,
            "an unbound identifier is one diagnostic, not two: {ds:#?}"
        );
    }

    /// `Expression::ErrorNode` is the only form in the language with nothing to
    /// elaborate, and the typechecker used to type it as a fresh var and say
    /// nothing — leaving the elaborator to report a compiler bug for a plain
    /// syntax error. The check walk denies it the `CleanModule` proof instead,
    /// so the elaborator never sees one.
    #[test]
    fn an_error_node_denies_the_proof() {
        let mut s = new_scanner("x = 1 +\n".to_string());
        let pr = new_parser(&mut s).parse_program();
        assert!(
            crate::diagnostic::has_errors(&pr.diagnostics),
            "snippet must fail to parse"
        );
        let r = compile(&ast::Expression::BlockExpression(pr.ast), None, None);
        assert!(!r.success, "an unparseable program must not compile");
        assert!(
            codes(&r.diagnostics).contains(&DiagnosticCode::ParseError),
            "the check walk restates the parse error: {:#?}",
            r.diagnostics
        );
    }

    /// And the clean module still elaborates: the gate is not a mute button.
    #[test]
    fn a_clean_module_reaches_the_core_pipeline() {
        let mut s = new_scanner("fn f(x Int) Int { x + 1 }\nf(1)\n".to_string());
        let pr = new_parser(&mut s).parse_program();
        let r = compile(&ast::Expression::BlockExpression(pr.ast), None, None);
        assert!(r.success, "{:#?}", r.diagnostics);
        assert!(
            !r.core.fns.is_empty(),
            "a diagnostics-clean module must produce Core"
        );
    }
}

#[cfg(test)]
mod toplevel_slot_queue {
    use super::super::*;
    use crate::parser::new_parser;
    use crate::scanner::new_scanner;

    /// `toplevel_binds` is positional, so only the walk the toplevel
    /// elaboration mirrors — a module's own statement list — may fill it.
    /// Depth alone does not identify that walk: a bare-expression program runs
    /// with `scope_marks` empty, so an arm's pattern binding would sit at
    /// "module depth" with no module statement behind it, and the next `let`
    /// the elaborator saw would be pinned to the pattern's slot.
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

    /// The bare-expression entry point (`bytecode::compile` on a non-block).
    /// Its outermost block is an arm body, which the elaborator treats as a
    /// module toplevel and lets drain the queue; the arm's own pattern bindings
    /// must never have reached it.
    #[test]
    fn a_bare_match_expression_compiles_without_stealing_a_pattern_slot() {
        let src = "match Some(1) { Some(v) -> { w = v + 1\n v + w }\n None -> 0 }";
        let mut s = new_scanner(src.to_string());
        let pr = new_parser(&mut s).parse_program();
        assert!(
            !crate::diagnostic::has_errors(&pr.diagnostics),
            "{:?}",
            pr.diagnostics
        );
        let [ast::Node::Expression(expr)] = &pr.ast.body[..] else {
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
