//! Golden snapshots of the printed Core IR (typed ANF) for a handful of small
//! programs, so refactors of `lower` / `perceus` / the printer are diffable
//! rather than silently behaviour-changing. Each test parses + typechecks a
//! self-contained source string, lowers it to [`al::core_ir::CoreProgram`],
//! and compares its `Display` output against `tests/golden/core_ir/<name>.core`.
//!
//! Regenerate after an intentional IR change with
//! `UPDATE_CORE_GOLDEN=1 cargo test -p al --test core_ir`.

use std::path::PathBuf;

mod common;
use common::{diff_lines, parse};

fn golden_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/golden/core_ir")
}

/// Parse + typecheck + lower `source` and return the pretty-printed Core.
///
/// Every id the printer emits — `Ty` (`:N`), `StrId` (`sN`, a function name),
/// `ConstId` (`cN`) — is an index into a pool shared with the prelude, so its
/// absolute value moves whenever the stdlib mints a different number of them.
/// A snapshot that recorded those raw indices would go red on any stdlib edit
/// (renaming a module is enough), and the only way through would be to
/// regenerate — which is exactly how a real IR regression slips past. So all
/// three are renumbered by order of first appearance before comparison, and
/// each referenced constant's `Value` is printed in a `where` block so the
/// snapshot still shows what the program computes.
fn lower(source: &str) -> String {
    let ast = parse(source);
    let r = al::bytecode::compile(&ast, None, Some(&al::STDLIB));
    assert!(
        r.success(),
        "compile failed:\n{source}\n--- diagnostics ---\n{:#?}",
        r.diagnostics
    );
    let emitted = r.emitted.as_ref().expect("a successful compile emits");
    let core: &al::core_ir::CoreProgram = &emitted.core;
    let raw = format!("{core}");
    // Renumber consts first: the `where` block needs each original index to
    // look up its `Value`, and its new name to print under.
    let const_map = renumber(&raw, b'c');
    // A printed `Ty` is a raw index into the inference engine's node arena,
    // shared with the prelude, so it shifts whenever the stdlib mints a
    // different number of fresh vars; `:tK` records only what the snapshot
    // means to assert (which binders share a type, where a distinct one
    // appears) rather than an interner offset.
    let mut out = apply_renumber(&raw, b':', ":t", &renumber(&raw, b':'));
    out = apply_renumber(&out, b'c', "c", &const_map);
    out = apply_renumber(&out, b's', "s", &renumber(&raw, b's'));
    // Function indices and global slots are absolute program offsets shared
    // with the whole stdlib, so adding one stdlib function used to shift
    // every `fn#N`/`@gN` in every golden. Dense-renumber them like the rest.
    out = renumber_prefixed(&out, "fn#");
    out = renumber_prefixed(&out, "@g");
    if !const_map.is_empty() {
        let mut rows: Vec<(usize, usize)> = const_map.iter().map(|(&o, &n)| (n, o)).collect();
        rows.sort_unstable();
        out.push_str("where\n");
        for (new, orig) in rows {
            let v = core
                .consts
                .get(orig)
                .map(|c| format!("{c:?}"))
                .unwrap_or_else(|| "<oob>".into());
            out.push_str(&format!("  c{new} = {v}\n"));
        }
    }
    out
}

/// Renumber every `<prefix><digits>` token in `s` densely by first
/// appearance, keeping the prefix. For multi-byte sigils (`fn#`, `@g`) the
/// single-byte `renumber`/`apply_renumber` pair cannot be used.
fn renumber_prefixed(s: &str, prefix: &str) -> String {
    let mut map: std::collections::HashMap<usize, usize> = std::collections::HashMap::new();
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(pos) = rest.find(prefix) {
        let after = &rest[pos + prefix.len()..];
        let digits: String = after.chars().take_while(char::is_ascii_digit).collect();
        if digits.is_empty() {
            out.push_str(&rest[..pos + prefix.len()]);
            rest = after;
            continue;
        }
        let orig: usize = digits.parse().expect("digits");
        let next = map.len();
        let new = *map.entry(orig).or_insert(next);
        out.push_str(&rest[..pos]);
        out.push_str(prefix);
        out.push_str(&new.to_string());
        rest = &after[digits.len()..];
    }
    out.push_str(rest);
    out
}

/// Map each id printed with sigil `sig` in `s` to a dense index, in order of
/// first appearance. `sig` is the byte the printer emits before the digits:
/// `c` for `ConstId`, `s` for `StrId`, `:` for a `Ty`.
fn renumber(s: &str, sig: u8) -> std::collections::HashMap<usize, usize> {
    let mut map = std::collections::HashMap::new();
    for (_, orig) in id_spans(s, sig) {
        let next = map.len();
        map.entry(orig).or_insert(next);
    }
    map
}

/// Rewrite every id printed with sigil `sig` through `map`, re-emitting it as
/// `<out_prefix><new>`. Ids missing from `map` are left verbatim.
fn apply_renumber(
    s: &str,
    sig: u8,
    out_prefix: &str,
    map: &std::collections::HashMap<usize, usize>,
) -> String {
    let mut out = String::with_capacity(s.len());
    let mut last = 0usize;
    for ((lo, hi), orig) in id_spans(s, sig) {
        out.push_str(&s[last..lo]);
        match map.get(&orig) {
            Some(n) => out.push_str(&format!("{out_prefix}{n}")),
            None => out.push_str(&s[lo..hi]),
        }
        last = hi;
    }
    out.push_str(&s[last..]);
    out
}

/// Byte ranges of the `<sig><digits>` tokens in `s`, paired with the parsed
/// number. The sigil must begin a fresh token: the `c` of `proc` and the `s` of
/// `self` are identifier characters, not ids, and a `:` after a letter is a
/// `ReuseShape`'s `[Enum:2]` rather than a `Ty`. Only an alphanumeric sigil can
/// be continued by a preceding digit, so `%0:3`'s `:3` is still a `Ty`.
fn id_spans(s: &str, sig: u8) -> Vec<((usize, usize), usize)> {
    let b = s.as_bytes();
    let ident_sigil = sig.is_ascii_alphanumeric();
    let mut out = Vec::new();
    let mut i = 0;
    while i < b.len() {
        let fresh = i == 0 || {
            let p = b[i - 1];
            !p.is_ascii_alphabetic() && p != b'_' && !(ident_sigil && p.is_ascii_digit())
        };
        if fresh && b[i] == sig {
            let mut j = i + 1;
            while j < b.len() && b[j].is_ascii_digit() {
                j += 1;
            }
            let trailing_alpha = j < b.len() && (b[j].is_ascii_alphabetic() || b[j] == b'_');
            if j > i + 1
                && !trailing_alpha
                && let Ok(n) = s[i + 1..j].parse()
            {
                out.push(((i, j), n));
                i = j;
                continue;
            }
        }
        i += 1;
    }
    out
}

/// Compare `got` against the on-disk golden for `name`, or overwrite it when
/// `UPDATE_CORE_GOLDEN` is set. On mismatch, print a line-by-line diff so the
/// intentional-vs-regression call is easy to make from the test log alone.
fn assert_core_golden(name: &str, source: &str) {
    let got = lower(source);
    let path = golden_dir().join(format!("{name}.core"));
    if std::env::var_os("UPDATE_CORE_GOLDEN").is_some() {
        std::fs::create_dir_all(golden_dir()).expect("create golden dir");
        std::fs::write(&path, &got).expect("write golden");
        return;
    }
    let want = std::fs::read_to_string(&path).unwrap_or_else(|e| {
        panic!(
            "missing golden for {name}: {e}\n\
             --- got ---\n{got}\n\
             (run with UPDATE_CORE_GOLDEN=1 to create it)"
        )
    });
    if got != want {
        panic!(
            "Core IR mismatch for {name}:\n{}\n\
             (run with UPDATE_CORE_GOLDEN=1 if this change is intentional)",
            diff_lines(&want, &got)
        );
    }
}

macro_rules! core_golden {
    ($name:ident, $src:expr) => {
        #[test]
        fn $name() {
            assert_core_golden(stringify!($name), $src);
        }
    };
}

// ── Programs ───────────────────────────────────────────────────────────────
// One per CoreExpr / Atom shape. Kept import-free so the toplevel snapshot
// stays small and independent of stdlib churn.

// Let-spine of PrimOps: nested arithmetic linearised into `%n = op(...)`.
core_golden!(
    let_primop,
    "x = 1 + 2\n\
     y = x * 3\n\
     y - x\n"
);

// If: both arms Tail a local.
core_golden!(
    if_branch,
    "fn abs(n Int) Int {\n\
     \tif n < 0 { 0 - n } else { n }\n\
     }\n\
     abs(0 - 5)\n"
);

// Match on Int literals with a wildcard fall-through.
core_golden!(
    match_lit,
    "fn step(n Int) Int {\n\
     \tmatch n % 2 {\n\
     \t\t0 -> n / 2\n\
     \t\telse -> 3 * n + 1\n\
     \t}\n\
     }\n\
     step(7)\n"
);

// Sum type: Ctor construction + Match with Ctor patterns binding fields.
core_golden!(
    match_ctor,
    "type Shape {\n\
     \tCircle(r Int)\n\
     \tRect(w Int, h Int)\n\
     }\n\
     fn area(s Shape) Int {\n\
     \tmatch s {\n\
     \t\tCircle(r) -> r * r * 3\n\
     \t\tRect(w, h) -> w * h\n\
     \t}\n\
     }\n\
     area(Rect(4, 5))\n"
);

// Callee::Self_ in tail position — the loop-carried-reuse target shape.
core_golden!(
    self_tail,
    "fn count(n Int, acc Int) Int {\n\
     \tmatch n {\n\
     \t\t0 -> acc\n\
     \t\telse -> count(n - 1, acc + 1)\n\
     \t}\n\
     }\n\
     count(3, 0)\n"
);

// Callee::Known — one top-level fn calling another.
core_golden!(
    known_call,
    "fn sq(x Int) Int { x * x }\n\
     fn hyp2(a Int, b Int) Int { sq(a) + sq(b) }\n\
     hyp2(3, 4)\n"
);

// Product type: Ctor with multiple fields, then field projection.
core_golden!(
    ctor_fields,
    "type Point {\n\tx Int\n\ty Int\n}\n\
     p = Point(x: 1, y: 2)\n\
     p.x + p.y\n"
);

// Irrefutable ctor destructuring must project by DECLARED field order (labels
// and `..` resolved), and each heap-typed projection must get its own `drop` —
// its bind type comes from the ctor's fn-type, not a `fresh_var()` (a type var
// is never heap-shaped, so Perceus would silently emit no `Drop` and reuse
// could not propagate down a recursion).
core_golden!(
    ctor_destructure_binding,
    "type L {\n\tCons(h Int, t L)\n\tLNil\n}\n\
     type P { P(a L, b L) }\n\
     fn g(l L) Int {\n\
     \tmatch l {\n\
     \t\tCons(h, _) -> h\n\
     \t\tLNil -> 0\n\
     \t}\n\
     }\n\
     fn f(p P) Int {\n\
     \tP(b: y, a: x) = p\n\
     \tg(x) + g(y)\n\
     }\n\
     f(P(LNil, LNil))\n"
);

// Same for tuple destructuring: element types come from the scrutinee's Tuple
// node, so a heap element is `Drop`ped rather than held to frame end.
core_golden!(
    tuple_destructure_binding,
    "type L {\n\tCons(h Int, t L)\n\tLNil\n}\n\
     fn g(l L) Int {\n\
     \tmatch l {\n\
     \t\tCons(h, _) -> h\n\
     \t\tLNil -> 0\n\
     \t}\n\
     }\n\
     fn f(p (L, L)) Int {\n\
     \t(x, y) = p\n\
     \tg(x) + g(y)\n\
     }\n\
     f((LNil, LNil))\n"
);

// An `If` in operand position becomes a `LetJoin` whose value is heap-typed,
// not the Int/Bool the join machinery once assumed.
core_golden!(
    heap_join_operand,
    "type L {\n\tCons(h Int, t L)\n\tLNil\n}\n\
     fn g(l L) Int {\n\
     \tmatch l {\n\
     \t\tCons(h, _) -> h\n\
     \t\tLNil -> 0\n\
     \t}\n\
     }\n\
     fn f(c Bool) Int {\n\
     \tg(if c { Cons(1, LNil) } else { LNil })\n\
     }\n\
     f(True)\n"
);

// Perceus inserts no drops INSIDE a join body, so `t` below is held to the end
// of `f`'s frame rather than released after its last use. Sound (the VM clones
// on push and releases at frame teardown) but it forfeits a `Reuse` token.
// This snapshot pins the gap: sinking drops into joins should change it.
core_golden!(
    join_body_defers_drop,
    "type L {\n\tCons(h Int, t L)\n\tLNil\n}\n\
     fn g(l L) Int {\n\
     \tmatch l {\n\
     \t\tCons(h, _) -> h\n\
     \t\tLNil -> 0\n\
     \t}\n\
     }\n\
     fn f(c Bool) Int {\n\
     \tn = if c {\n\
     \t\tt = Cons(1, LNil)\n\
     \t\tg(t) + g(t)\n\
     \t} else {\n\
     \t\t0\n\
     \t}\n\
     \t1 + n\n\
     }\n\
     f(True)\n"
);

// A match whose scrutinee's type nothing in the source states: it comes from
// `Some(Boxed(3))` alone. `lower` used to re-instantiate `Some`'s scheme here,
// so the payload bind `%3` carried an unbound type-var — not heap-shaped, so
// Perceus emitted no `drop %3` and the `Boxed` was held to frame end, and
// `b.n` could not even find the field (it aborted lowering outright). The
// snapshot pins the `drop` and the `AddInt`: both come from `b`'s real type.
core_golden!(
    inferred_scrutinee_drops_heap_payload,
    "type Boxed { Boxed(n Int) }\n\
     fn f() Int {\n\
     \tmatch Some(Boxed(3)) {\n\
     \t\tNone -> 0\n\
     \t\tSome(b) -> {\n\
     \t\t\tx = b.n\n\
     \t\t\tx + 1\n\
     \t\t}\n\
     \t}\n\
     }\n\
     f()\n"
);

// A fallible multi-arm binary-literal match — the `http.to_method` shape.
// Every failure edge (each segment compare, each arm's exhausted pattern)
// jumps to a shared per-suffix continuation, so each arm body and the final
// no-match trap are lowered exactly once. Before join points this shape
// re-lowered every remaining arm at every failure edge, multiplying the IR.
core_golden!(
    binary_match_method,
    "type Method {\n\
     \tGet\n\
     \tPost\n\
     \tPut\n\
     \tDelete\n\
     \tOther(m Binary)\n\
     }\n\
     fn to_method(m Binary) Method {\n\
     \tmatch m {\n\
     \t\t<<'GET'>> -> Get\n\
     \t\t<<'POST'>> -> Post\n\
     \t\t<<'PUT'>> -> Put\n\
     \t\t<<'DELETE'>> -> Delete\n\
     \t\telse -> Other(m)\n\
     \t}\n\
     }\n\
     to_method(<<'PUT'>>)\n"
);

// A guarded match: a guard's false edge is a failure edge like any pattern
// mismatch, so it routes to the next arm's shared continuation instead of
// re-lowering the trailing arms under the guard's `if`.
core_golden!(
    guarded_match,
    "fn clamp(n Int, lo Int, hi Int) Int {\n\
     \tmatch n {\n\
     \t\tx if x < lo -> lo\n\
     \t\tx if x > hi -> hi\n\
     \t\tx -> x\n\
     \t}\n\
     }\n\
     clamp(5, 0, 10)\n"
);

// An or-pattern match: every alternative of `a | b | c` shares the one arm
// body, and each alternative's failure edge shares the next-arm continuation.
core_golden!(
    or_pattern_match,
    "fn small(n Int) Int {\n\
     \tmatch n {\n\
     \t\t0 | 1 -> 1\n\
     \t\t2 | 3 | 4 -> 2\n\
     \t\telse -> 0\n\
     \t}\n\
     }\n\
     small(3)\n"
);

// ── Structural assertions ──────────────────────────────────────────────────
// The goldens above pin the whole IR, so they also move when the type arena
// hands out a different number of vars. These assert only the property that
// actually matters — that a heap-typed projection gets released — so they stay
// green across unrelated lowering churn and red on the real regression.

/// The `idx`-th printed `fn` block (0-based, in emission order). Addressed by
/// position, not by the `fn s<StrId>` in its signature: interner ids shift
/// whenever the stdlib interns a different number of strings.
fn fn_body(core: &str, idx: usize) -> String {
    let fns: Vec<&str> = core
        .split("\n\n")
        .filter(|b| b.starts_with("fn "))
        .collect();
    fns.get(idx)
        .unwrap_or_else(|| panic!("no fn #{idx} in:\n{core}"))
        .to_string()
}

const HEAP_PAIR_SRC: &str = "\
type L {
	Cons(h Int, t L)
	LNil
}\n\
type P { P(a L, b L) }\n\
fn g(l L) Int {\n\
\tmatch l {\n\
\t\tCons(h, _) -> h\n\
\t\tLNil -> 0\n\
\t}\n\
}\n";

/// A destructured heap field must get its own `drop`. The bind's type comes
/// from the constructor's fn-type; a `fresh_var()` is never heap-shaped, so
/// Perceus would silently emit none and reuse could not propagate.
#[test]
fn ctor_destructure_drops_each_heap_field() {
    let core = lower(&format!(
        "{HEAP_PAIR_SRC}\
         fn f(p P) Int {{\n\
         \tP(b: y, a: x) = p\n\
         \tg(x) + g(y)\n\
         }}\n\
         f(P(LNil, LNil))\n"
    ));
    let body = fn_body(&core, 1);
    let drops = body
        .lines()
        .filter(|l| l.trim_start().starts_with("drop "))
        .count();
    assert!(
        drops >= 3,
        "want a drop for the scrutinee and each of the 2 heap fields, got {drops}:\n{body}"
    );
}

/// Same guarantee for tuple destructuring: element types come from the
/// scrutinee's `Tuple` node, not a fresh var.
#[test]
fn tuple_destructure_drops_each_heap_element() {
    let core = lower(&format!(
        "{HEAP_PAIR_SRC}\
         fn f(p (L, L)) Int {{\n\
         \t(x, y) = p\n\
         \tg(x) + g(y)\n\
         }}\n\
         f((LNil, LNil))\n"
    ));
    let body = fn_body(&core, 1);
    let drops = body
        .lines()
        .filter(|l| l.trim_start().starts_with("drop "))
        .count();
    assert!(
        drops >= 3,
        "want a drop for the scrutinee and each of the 2 heap elements, got {drops}:\n{body}"
    );
}

/// The `match` spelling of the same destructure has always emitted the drops;
/// pin the two spellings together so they cannot drift apart again.
#[test]
fn destructure_binding_matches_match_spelling() {
    let mk = |body: &str| {
        lower(&format!(
            "{HEAP_PAIR_SRC}fn f(p P) Int {{\n{body}}}\nf(P(LNil, LNil))\n"
        ))
    };
    let binding = mk("\tP(x, y) = p\n\tg(x) + g(y)\n");
    let matched = mk("\tmatch p {\n\t\tP(x, y) -> g(x) + g(y)\n\t}\n");
    let count = |c: &str| {
        fn_body(c, 1)
            .lines()
            .filter(|l| l.trim_start().starts_with("drop "))
            .count()
    };
    assert_eq!(
        count(&binding),
        count(&matched),
        "destructuring binding and match must release the same set:\n--- binding ---\n{}\n--- match ---\n{}",
        fn_body(&binding, 1),
        fn_body(&matched, 1)
    );
}

// ── Fallible matches lower each arm body exactly once ──────────────────────
// A failure edge (a binary segment mismatch, a false guard, an exhausted
// or-pattern alternative) jumps to a shared continuation for the remaining
// arms instead of re-lowering them in place. Each program below marks every
// arm body with a distinct integer literal no other part of the program (and
// nothing the renumbering rewrites) can produce, so counting the marker in
// the printed fn body counts how many times that arm body was lowered.

/// The renumbered `ConstId` whose `where`-block value contains `marker` as a
/// standalone digit run. An integer literal always lowers to `Atom::Const`,
/// so the marker never appears in the fn body as digits — only as this `cN`.
fn marker_const(core: &str, marker: usize) -> usize {
    let needle = marker.to_string();
    let standalone = |line: &str| {
        let b = line.as_bytes();
        let mut i = 0;
        while let Some(pos) = line[i..].find(&needle) {
            let lo = i + pos;
            let hi = lo + needle.len();
            let fresh = lo == 0 || !b[lo - 1].is_ascii_digit();
            if fresh && (hi == b.len() || !b[hi].is_ascii_digit()) {
                return true;
            }
            i = lo + 1;
        }
        false
    };
    let where_block = core.split("where\n").nth(1).expect("a where block");
    let mut found = Vec::new();
    for line in where_block.lines() {
        if let Some((name, value)) = line.trim().split_once(" = ")
            && standalone(value)
        {
            found.push(
                name.strip_prefix('c')
                    .expect("const name")
                    .parse()
                    .expect("const index"),
            );
        }
    }
    // Exactly one pool entry: were a re-lowered arm body to mint a fresh
    // (undeduplicated) constant, counting only the first id would miss it.
    match found[..] {
        [id] => id,
        [] => panic!("marker {marker} not in the const pool:\n{core}"),
        _ => panic!("marker {marker} pooled more than once ({found:?}):\n{core}"),
    }
}

fn assert_arm_bodies_once(source: &str, markers: &[usize]) {
    let core = lower(source);
    let body = fn_body(&core, 0);
    for &m in markers {
        let id = marker_const(&core, m);
        let n = id_spans(&body, b'c')
            .iter()
            .filter(|(_, k)| *k == id)
            .count();
        assert_eq!(
            n, 1,
            "arm body marker {m} (c{id}) must be lowered exactly once, found {n} times in:\n{body}"
        );
    }
}

/// The `to_method` shape: multi-arm binary-literal match with an `else`. Every
/// segment-compare failure edge must reach the remaining arms through a shared
/// continuation, not a fresh copy of them.
#[test]
fn binary_match_arm_bodies_lowered_once() {
    assert_arm_bodies_once(
        "fn code(m Binary) Int {\n\
         \tmatch m {\n\
         \t\t<<'GET'>> -> 101\n\
         \t\t<<'POST'>> -> 202\n\
         \t\t<<'PUT'>> -> 303\n\
         \t\t<<'DELETE'>> -> 404\n\
         \t\t<<'PATCH'>> -> 505\n\
         \t\telse -> 606\n\
         \t}\n\
         }\n\
         code(<<'HEAD'>>)\n",
        &[101, 202, 303, 404, 505, 606],
    );
}

/// A false guard falls through to the next arm via the same shared
/// continuation as a pattern mismatch — the trailing arms are not duplicated
/// under the guard's `if`.
#[test]
fn guarded_match_arm_bodies_lowered_once() {
    assert_arm_bodies_once(
        "fn band(x Int, y Int) Int {\n\
         \tmatch x {\n\
         \t\tn if n < y -> 101\n\
         \t\tn if n > y -> 202\n\
         \t\telse -> 303\n\
         \t}\n\
         }\n\
         band(1, 2)\n",
        &[101, 202, 303],
    );
}

/// Every alternative of an or-pattern shares one lowered arm body, and an
/// exhausted alternative list shares the next arm's continuation.
#[test]
fn or_pattern_arm_bodies_lowered_once() {
    assert_arm_bodies_once(
        "fn cls(n Int) Int {\n\
         \tmatch n {\n\
         \t\t0 | 1 -> 101\n\
         \t\t2 | 3 | 4 -> 202\n\
         \t\telse -> 303\n\
         \t}\n\
         }\n\
         cls(3)\n",
        &[101, 202, 303],
    );
}

/// An always-matching mid-chain alternative (`<<..>>`) supersedes the
/// alternatives written after it: they are lowered then discarded, and the
/// discard must retract their `goto` counts. Otherwise a join whose every
/// edge lived in the discarded subtree — here the arm's own fallthrough,
/// targeted only by the unreachable `<<2>>` — is still materialized as a
/// `letc` block nothing jumps to.
#[test]
fn discarded_alternative_leaves_no_unreachable_continuation() {
    let core = lower(
        "fn g(b Binary) Int {\n\
         \tmatch b {\n\
         \t\t<<1>> | <<..>> | <<2>> -> 1\n\
         \t}\n\
         }\n\
         g(<<1>>)\n",
    );
    let body = fn_body(&core, 0);
    let mut declared = Vec::new();
    let mut targeted = Vec::new();
    for line in body.lines() {
        let t = line.trim();
        if let Some(label) = t.strip_prefix("letc ").and_then(|r| r.strip_suffix(" =")) {
            declared.push(label);
        } else if let Some(label) = t.strip_prefix("goto ") {
            targeted.push(label);
        }
    }
    assert!(!declared.is_empty(), "expected join points in:\n{body}");
    for label in declared {
        assert!(
            targeted.contains(&label),
            "letc {label} is declared but no goto targets it:\n{body}"
        );
    }
}

// ── Infallible matches stay on the flat no-continuation path ────────────────
// The join-point machinery exists to deduplicate *failure* continuations; a
// match with no failure edge must not pay for it. These shapes lowered to a
// single flat `Match` before join points existed and must keep doing so —
// `emit`'s `SwitchTag` fast path pattern-matches that flat shape, so a
// gratuitous `LetCont` here would also demote an exhaustive enum match to the
// `MatchEnum` ladder. The bytecode-level twin of this test
// (`dis::an_infallible_match_keeps_the_flat_lowering`) pins the exact
// instruction sequences.

/// No `letc`/`goto` in the printed IR of a fn whose matches cannot fail.
fn assert_no_continuations(source: &str, fn_idx: usize) {
    let core = lower(source);
    let body = fn_body(&core, fn_idx);
    for needle in ["letc ", "goto "] {
        assert!(
            !body.contains(needle),
            "infallible match lowered a `{}` continuation:\n{body}",
            needle.trim_end()
        );
    }
}

/// Exhaustive one-arm-per-variant enum match: every head is tested by the
/// `Match` itself and every payload bind is irrefutable, so no failure edge
/// exists and no continuation is minted.
#[test]
fn exhaustive_enum_match_stays_flat() {
    assert_no_continuations(
        "type Shape {\n\
         \tCircle(r Int)\n\
         \tRect(w Int, h Int)\n\
         }\n\
         fn area(s Shape) Int {\n\
         \tmatch s {\n\
         \t\tCircle(r) -> r * r * 3\n\
         \t\tRect(w, h) -> w * h\n\
         \t}\n\
         }\n\
         area(Circle(2))\n",
        0,
    );
}

/// A single irrefutable arm (tuple destructure) is pure projection.
#[test]
fn single_irrefutable_arm_stays_flat() {
    assert_no_continuations(
        "fn single(p (Int, Int)) Int {\n\
         \tmatch p {\n\
         \t\t(a, b) -> a + b\n\
         \t}\n\
         }\n\
         single((1, 2))\n",
        0,
    );
}

/// A literal ladder with a wildcard: a head miss falls to the next arm of the
/// same flat `Match` (the ladder's own `JumpIfFalse`), which is not a failure
/// edge — nothing resumes matching mid-arm — so no continuation is minted.
#[test]
fn literal_ladder_stays_flat() {
    assert_no_continuations(
        "fn lits(n Int) String {\n\
         \tmatch n {\n\
         \t\t1 -> 'one'\n\
         \t\t2 -> 'two'\n\
         \t\telse -> 'many'\n\
         \t}\n\
         }\n\
         lits(1)\n",
        0,
    );
}

// ── Unlowerable programs are diagnostics, not panics ───────────────────────

/// `lower` used to `panic!` on any form it could not handle, and `al check`
/// returned before lowering ran — so a program the typechecker accepted but
/// `lower` rejected passed `al check` and aborted the process at `al run`.
///
/// Threading the typechecker's types through the pipeline closed that hole
/// completely: `lower`, `perceus` and `emit` consume a `TypedProgram` and are
/// total, and the elaborator that builds one runs only on a module the
/// typechecker left clean. `Expression::ErrorNode` — the one AST node with no
/// meaning to elaborate, and the one the check walk used to type as a fresh var
/// while saying nothing — is now a *checked* error, so no diagnostic telling the
/// user to report a compiler bug survives anywhere in the compiler.
///
/// The parser never produces an `ErrorNode` without also producing a parse
/// error, so the node has to be spliced in by hand to drive the check walk's
/// arm directly. Both entry points must reject it, with the diagnostic pointing
/// at the bad form, and neither may panic.
mod unlowerable {
    use al::diagnostic::{DiagnosticCode, Severity};
    use al::{ast, span::Span};

    fn program_with_an_error_node() -> (ast::Expression, Span) {
        let at = Span::single_line(2, 5, 9);
        let node = ast::Node::Expression(ast::Expression::ErrorNode(ast::ErrorNode {
            message: "spliced".into(),
            span: at,
        }));
        let block = ast::BlockExpression {
            body: vec![node],
            span: Span::single_line(1, 1, 1),
        };
        (ast::Expression::BlockExpression(block), at)
    }

    fn assert_rejected(r: &al::bytecode::CompileResult, at: Span, what: &str) {
        assert!(!r.success(), "{what} accepted an unlowerable program");
        let d = r
            .diagnostics
            .iter()
            .find(|d| d.severity == Severity::Error)
            .unwrap_or_else(|| panic!("{what}: no error diagnostic: {:?}", r.diagnostics));
        assert_eq!(
            d.code,
            DiagnosticCode::ParseError,
            "{what}: an unreadable region is a parse error, not a compiler bug"
        );
        assert_eq!(d.span, at, "{what}: diagnostic must point at the bad form");
    }

    #[test]
    fn al_check_reports_it_rather_than_passing() {
        let (expr, at) = program_with_an_error_node();
        let r = al::bytecode::check(&expr, None, Some(&al::STDLIB));
        assert_rejected(&r, at, "check");
    }

    #[test]
    fn al_run_reports_it_rather_than_panicking() {
        let (expr, at) = program_with_an_error_node();
        let r = al::bytecode::compile(&expr, None, Some(&al::STDLIB));
        assert_rejected(&r, at, "compile");
    }

    /// And the neighbouring statements of a program the checker accepts still
    /// reach the VM: rejecting the `ErrorNode` is a gate on the module, not a
    /// mute button on the pipeline. The same block, with the spliced node
    /// replaced by the expression the parser would have built, compiles and
    /// runs.
    #[test]
    fn the_same_block_without_the_error_node_compiles_and_runs() {
        let expr = crate::common::parse("1 + 1\n");
        let r = al::bytecode::compile(&expr, None, Some(&al::STDLIB));
        assert!(r.success(), "{:?}", r.diagnostics);
        let program = r.emitted.expect("a successful compile emits").program;
        let mut vm = al::vm::new_vm(program).expect("vm init");
        let val = vm.run().expect("vm run");
        assert_eq!(al::vm::inspect(&val, vm.program()), "2");
    }
}
