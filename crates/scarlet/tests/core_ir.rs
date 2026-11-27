//! Golden snapshots of the printed Core IR (typed ANF), compared against
//! `tests/golden/core_ir/<name>.core`.
//!
//! Regenerate after an intentional IR change with
//! `UPDATE_CORE_GOLDEN=1 cargo test -p scarlet --test core_ir`.

use std::path::PathBuf;

mod common;
use common::diff_lines;

fn golden_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/golden/core_ir")
}

/// Parses, type-checks and lowers `source`, and returns the printed Core of
/// the entry file's functions with its toplevel last. The stdlib compiles into
/// the same program, but we leave it out.
///
/// Every printed id is an index into a pool shared with the prelude, so its
/// value moves on any stdlib edit. We renumber them all by order of first
/// appearance, and print each referenced constant's value in a `where` block
/// so the snapshot still shows what the program computes.
fn lower(source: &str) -> String {
    let r = scarlet::compile::compile_text(source, None);
    assert!(
        r.success(),
        "compile failed:\n{source}\n--- diagnostics ---\n{:#?}",
        r.diagnostics()
    );
    let program = r.into_runnable().expect("a successful compile is runnable");
    let entry = scarlet::module::ModuleKey::main();
    let mut raw = String::new();
    for f in &program.fns {
        if f.module == entry.as_str() {
            raw.push_str(&format!("{f}\n"));
        }
    }
    raw.push_str(&format!("toplevel:\n{}", program.toplevel));
    // Renumber consts first, because the `where` block needs both the original
    // index and the new name.
    let const_map = renumber(&raw, b'c');
    // `:tK` records only what the snapshot asserts, which is which binders
    // share a type, rather than an interner offset.
    let mut out = apply_renumber(&raw, b':', ":t", &renumber(&raw, b':'));
    out = apply_renumber(&out, b'c', "c", &const_map);
    // `fn#N` and `globalN` are absolute program offsets shared with the stdlib,
    // so we renumber them densely like the rest.
    out = renumber_prefixed(&out, "fn#");
    out = renumber_prefixed(&out, "global");
    if !const_map.is_empty() {
        let mut rows: Vec<(usize, usize)> = const_map.iter().map(|(&o, &n)| (n, o)).collect();
        rows.sort_unstable();
        out.push_str("where\n");
        for (new, orig) in rows {
            let v = program
                .consts
                .get(orig)
                .map(|c| format!("{c:?}"))
                .unwrap_or_else(|| "<oob>".into());
            out.push_str(&format!("  c{new} = {v}\n"));
        }
    }
    out
}

/// Renumbers every `<prefix><digits>` token in `s` densely by first appearance.
/// The single-byte `renumber` and `apply_renumber` can't handle multi-byte
/// sigils like `fn#` and `@g`.
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

/// Maps each id printed with sigil `sig` to a dense index, in order of first
/// appearance. `sig` is `c` for `ConstId`, `s` for `StrId`, `:` for a `Ty`.
fn renumber(s: &str, sig: u8) -> std::collections::HashMap<usize, usize> {
    let mut map = std::collections::HashMap::new();
    for (_, orig) in id_spans(s, sig) {
        let next = map.len();
        map.entry(orig).or_insert(next);
    }
    map
}

/// Rewrites every id printed with sigil `sig` through `map`, writing it back as
/// `<out_prefix><new>`. Ids missing from `map` stay as they are.
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

/// The byte ranges of the `<sig><digits>` tokens in `s`, paired with the parsed
/// number. The sigil must start a fresh token: the `c` of `proc` isn't an id.
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

/// Compares the lowered `source` against the golden for `name` on disk, or
/// overwrites the golden when `UPDATE_CORE_GOLDEN` is set.
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

// There's one golden per CoreExpr or Atom shape. They import nothing, so the
// toplevel snapshot stays small and doesn't move when the stdlib changes. Each
// program's code sits in `pub fn main()`, declared last, so the fn under test
// is the first printed `fn` block and `main` is the last.

// A chain of lets binding PrimOps.
core_golden!(
    let_primop,
    "pub fn main() {\n\
     \tx = 1 + 2\n\
     \ty = x * 3\n\
     \ty - x\n\
     }\n"
);

// An if whose arms both return a local.
core_golden!(
    if_branch,
    "fn abs(n Int) Int {\n\
     \tif n < 0 { 0 - n } else { n }\n\
     }\n\
     pub fn main() {\n\
     \tabs(0 - 5)\n\
     }\n"
);

// Match on Int literals with a wildcard fall-through.
core_golden!(
    match_lit,
    "fn step(n Int) Int {\n\
     \tmatch n % 2 {\n\
     \t\t0 -> n / 2\n\
     \t\t_ -> 3 * n + 1\n\
     \t}\n\
     }\n\
     pub fn main() {\n\
     \tstep(7)\n\
     }\n"
);

// Building a Ctor, then a Match with Ctor patterns that bind fields.
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
     pub fn main() {\n\
     \tarea(Rect(4, 5))\n\
     }\n"
);

// Callee::Self_ in tail position.
core_golden!(
    self_tail,
    "fn count(n Int, acc Int) Int {\n\
     \tmatch n {\n\
     \t\t0 -> acc\n\
     \t\t_ -> count(n - 1, acc + 1)\n\
     \t}\n\
     }\n\
     pub fn main() {\n\
     \tcount(3, 0)\n\
     }\n"
);

// Callee::Known, where one top-level fn calls another.
core_golden!(
    known_call,
    "fn sq(x Int) Int { x * x }\n\
     fn hyp2(a Int, b Int) Int { sq(a) + sq(b) }\n\
     pub fn main() {\n\
     \thyp2(3, 4)\n\
     }\n"
);

// A product type: a Ctor with several fields, then field projection.
core_golden!(
    ctor_fields,
    "type Point {\n\tx Int\n\ty Int\n}\n\
     pub fn main() {\n\
     \tp = Point(x: 1, y: 2)\n\
     \tp.x + p.y\n\
     }\n"
);

// A record update reads the fields it keeps as soon as its base has run. The
// read of `s.v` is then the last use of `s`, so `s` is dropped before the
// append, which finds the array with one holder.
core_golden!(
    record_update_reads_kept_fields_first,
    "type State {\n\tv Array(Int)\n\tcount Int\n\tcheck Int\n}\n\
     fn push(s State, n Int) State {\n\
     \tState(..s, v: [..s.v, n])\n\
     }\n\
     pub fn main() {\n\
     \tpush(State([], 0, 0), 1).count\n\
     }\n"
);

// A base that has to run keeps its place among the arguments: `n + 1`, then
// `make(n)`, then `n + 2`. The fields that are set hold numbers, so it is one
// update.
core_golden!(
    record_update_base_runs_in_its_place,
    "type P {\n\ta Int\n\tb Int\n\tc Int\n\td Int\n}\n\
     fn make(n Int) P { P(n, n, n, n) }\n\
     fn f(n Int) P {\n\
     \tP(a: n + 1, ..make(n), c: n + 2)\n\
     }\n\
     pub fn main() {\n\
     \tf(1).b\n\
     }\n"
);

// The same where a field that is set holds an array: `[n]`, then `make(n)` and
// the reads of the fields it keeps, then `[..xs, n]`.
core_golden!(
    record_update_reads_where_its_base_runs,
    "type P {\n\ta Array(Int)\n\tb Int\n\tc Array(Int)\n\td Int\n}\n\
     fn make(n Int) P { P([], n, [], n) }\n\
     fn f(n Int, xs Array(Int)) P {\n\
     \tP(a: [n], ..make(n), c: [..xs, n])\n\
     }\n\
     pub fn main() {\n\
     \tf(1, []).b\n\
     }\n"
);

/// What the function `f` of `src` is in Core IR.
fn f_of(src: &str) -> String {
    let core = lower(src);
    let functions = core.split("\n\n");
    let of_f = |f: &&str| {
        f.lines()
            .next()
            .is_some_and(|head| head.contains("main.f("))
    };
    let f = functions.into_iter().find(of_f);
    f.unwrap_or_else(|| panic!("no `f` in:\n{core}"))
        .to_string()
}

/// The words of `text` that are names.
fn names(text: &str) -> Vec<&str> {
    let part = |c: char| !c.is_ascii_alphanumeric() && c != '_';
    text.split(part).collect()
}

/// `f` in Core IR, with those of these parameters that `body` names.
fn f_with(params: &[(&str, &str)], ty: &str, body: &str) -> String {
    f_of(&program_with(params, ty, body))
}

/// A program whose `f` is `body`, with those of these parameters that it names.
fn program_with(params: &[(&str, &str)], ty: &str, body: &str) -> String {
    let named = names(body);
    let params = params.iter().filter(|(name, _)| named.contains(name));
    let params: Vec<String> = params.map(|(name, ty)| format!("{name} {ty}")).collect();
    let import = |m: &str| named.contains(&m).then(|| format!("import scarlet/{m}\n"));
    let imports: String = ["array", "string"].into_iter().filter_map(import).collect();
    format!(
        "import scarlet/net/socket\n\
         import scarlet/map\n\
         {imports}\
         pub type Color {{\n\tRed\n\tBlue\n}}\n\
         pub type Q {{\n\tQ(n Int, s String, t String)\n}}\n\
         pub type Tree {{\n\tLeaf\n\tNode(l Tree, r Tree)\n}}\n\
         pub type V {{\n\tA(n Int, xs Array(Int), k Int)\n\tB(xs Array(Int), n Int, k Int)\n}}\n\
         pub type U {{\n\tC(n Int, x Int, k Int)\n\tD(x Int, n Int, k Int)\n}}\n\
         pub type Two {{\n\tTwo(n Int, m Int)\n}}\n\
         pub type G(a) {{\n\tG(v a, n Int, k Int)\n}}\n\
         pub type P {{\n\
         \tn Int\n\tx Float\n\tb Bool\n\tc Color\n\ts String\n\txs Array(Int)\n\
         \tm map::Map(Int, Int)\n\tq Q\n\tt Tree\n\tconn Option(socket::Socket)\n\
         \tg fn(Int) Int\n\to Option(Int)\n}}\n\
         pub fn work(n Int) Int {{ n * n }}\n\
         pub fn list(n Int) Array(Int) {{ [n] }}\n\
         pub fn make(n Int) Two {{ Two(n, n) }}\n\
         pub fn copy(p P) P {{ p }}\n\
         pub fn f({}) {ty} {{\n\t{body}\n}}\n\
         pub fn main() {{ println(\"\") }}\n",
        params.join(", ")
    )
}

/// How many updates there are in `f`.
fn updates(f: &str) -> usize {
    f.matches(" update ").count()
}

/// A record update is one update when nothing can tell that it holds its base
/// until the new fields are computed. Otherwise it reads the fields it keeps and
/// makes a new constructor.
#[test]
fn a_record_update_is_one_where_holding_its_base_cannot_be_seen() {
    let of_p = |body: &str| {
        let params = [
            ("p", "P"),
            ("r", "P"),
            ("i", "Int"),
            ("j", "Int"),
            ("y", "Float"),
            ("ys", "Array(Int)"),
            ("f2", "fn(Int) Int"),
            ("z", "Option(Int)"),
        ];
        f_with(&params, "P", body)
    };
    for body in [
        // Fields that hold numbers are set to anything.
        "P(..p, n: 1)",
        "P(..p, n: p.n + 1)",
        "P(..p, n: work(p.n))",
        "P(..p, n: f2(i))",
        "P(..p, n: work(p.n), x: p.x * 2.0, b: !p.b)",
        "P(..p, c: if i > j { Red } else { p.c })",
        "P(..p, b: p.s == r.s)",
        "P(..p, n: array::length(array::put(p.xs, 0, 1)))",
        r#"P(..p, n: string::length("${i}"))"#,
        "P(..p, n: match ys { [] -> 0\n [h, ..] -> h + 1\n })",
        "P(n: work(1), ..p)",
        r#"P(..p, n: Q(..p.q, s: "x").n)"#,
        // Fields that hold anything are set to what takes a few steps.
        r#"P(..p, s: "x")"#,
        "P(..p, s: r.s)",
        "P(..p, xs: [])",
        "P(..p, xs: [i, j])",
        "P(..p, xs: p.xs)",
        "P(..p, xs: ys)",
        "P(..p, conn: None)",
        "P(..p, conn: r.conn)",
        r#"P(..p, conn: r.conn, xs: ys, s: "x")"#,
        "P(..p, g: f2)",
        "P(..p, g: fn(k) k + i)",
        "P(..p, t: Node(p.t, Leaf))",
        r#"P(..p, q: Q(i, "s", p.s))"#,
        "P(..p, q: r.q)",
        "P(..p, m: r.m)",
        "P(..p, o: Some(i))",
        "P(..p, o: p.xs[0])",
        "P(..p, xs: match z { Some(_) -> ys\n None -> p.xs\n })",
        "P(..p, xs: match ys { [] -> p.xs\n [_, _, ..] -> ys\n [_] -> [1]\n })",
        "P(..p, xs: match z { Some(1..4 | 7) -> ys\n Some(_) | None -> p.xs\n })",
        "P(..p, xs: if p.b && r.b || !r.b { ys } else { p.xs })",
        "P(..p, x: y * 2.0, xs: { k = ys\n k })",
        "P(..p, xs: 0..5)",
    ] {
        let f = of_p(body);
        assert!(updates(&f) > 0, "{body}:\n{f}");
        // It reads no field that it only keeps.
        assert!(!f.contains("= Field(6)(%0)"), "{body}:\n{f}");
    }
    for body in [
        // Computing what is set calls, writes, or takes as long as it is big.
        "P(..p, xs: list(i))",
        "P(..p, xs: array::put(p.xs, 0, 1))",
        "P(..p, xs: [..p.xs, 1])",
        "P(..p, xs: [1, ..ys])",
        "P(..p, xs: match p.xs { [] -> ys\n [_, ..t] -> t\n })",
        "P(..p, xs: match work(i) { 0 -> ys\n _ -> p.xs\n })",
        "P(..p, xs: match z { Some(_) if i > 2 -> ys\n Some(_) | None -> p.xs\n })",
        "P(..p, xs: match z { Some(_) -> list(1)\n None -> p.xs\n })",
        "P(..p, xs: if i > j { ys } else { p.xs })",
        "P(..p, xs: ys[0..1] or [])",
        "P(..p, n: 1, xs: list(i))",
        "P(..p, xs: ys, n: work(i))",
        r#"P(..p, s: p.s + "x")"#,
        r#"P(..p, s: "${i}")"#,
        "P(..p, m: map::set(p.m, 1, 2))",
        "P(..p, conn: match f2(i) { 0 -> None\n _ -> p.conn\n })",
        "P(..p, conn: None, n: i * j)",
        "P(..p, conn: None, n: -i)",
        "P(..p, conn: None, b: i == j)",
        "P(..p, conn: None, b: p.s == r.s)",
        "P(..p, t: Node(p.t, Leaf), n: i + 1)",
        "P(..p, o: Some(i + 1))",
        "P(..p, o: Some(f2(i)))",
        "P(..p, g: fn(k) k + i, n: work(1))",
        "P(..p, t: { println(\"\")\n Leaf })",
        "P(..p, o: p.xs[i + 1])",
        // What is set before the base runs is of a field of it too.
        "P(conn: r.conn, ..copy(p), n: work(i))",
        // Nothing is set.
        "P(..p)",
    ] {
        let f = of_p(body);
        assert_eq!(updates(&f), 0, "{body}:\n{f}");
        assert!(f.contains("ctor"), "{body}:\n{f}");
    }
    // An update that a field is set to may write in place, so it is not brief.
    let nested = of_p(r#"P(..p, q: Q(..p.q, s: "x"))"#);
    assert_eq!(updates(&nested), 1, "{nested}");
    let of_g = |ty: &str, body: &str| {
        let params = [("g", ty), ("val", "a"), ("i", "Int")];
        updates(&f_with(&params, ty, body))
    };
    // A value of a type variable may be anything.
    assert_eq!(of_g("G(a)", "G(..g, v: val)"), 1);
    assert_eq!(of_g("G(a)", "G(..g, n: work(i))"), 1);
    assert_eq!(of_g("G(a)", "G(..g, v: val, n: i + 1)"), 0);
    assert_eq!(of_g("G(Int)", "G(..g, v: work(i))"), 1);
    assert_eq!(of_g("G(Array(Int))", "G(..g, v: list(i))"), 0);
    // Every field is set.
    let all = f_with(&[("i", "Int")], "Two", "Two(..make(i), n: 1, m: 2)");
    assert_eq!(updates(&all), 0, "{all}");
    // The base has to be the variant that is made.
    let of_v = |body: &str| {
        let params = [("v", "V"), ("w", "V"), ("i", "Int")];
        updates(&f_with(&params, "V", body))
    };
    for (body, want) in [
        ("A(..v, n: i)", 0),
        (
            "match v { A(..) -> A(..v, n: i)\n B(..) -> B(..v, n: i) }",
            2,
        ),
        (
            "match v { A(..) -> B(..v, n: i)\n B(..) -> A(..v, n: i) }",
            0,
        ),
        ("match v { A(..) -> A(..w, n: i)\n B(..) -> w }", 0),
        (
            "match v { A(..) -> match w { A(..) -> A(..v, n: 1)\n B(..) -> B(..w, n: 2) }\n B(..) -> w }",
            2,
        ),
        (
            "match v { A(..) -> match v { A(..) -> A(..v, n: 1)\n B(..) -> B(..v, n: 2) }\n B(..) -> w }",
            2,
        ),
        (
            "match v { A(n, ..) if n > 0 -> A(..v, n: i)\n A(..) | B(..) -> v }",
            1,
        ),
        ("match v { A(..) | B(..) -> A(..v, n: i) }", 0),
        ("match v { u -> A(..u, n: i) }", 0),
        (
            "match v { A(..) -> { v = w\n A(..v, n: i) }\n B(..) -> v }",
            0,
        ),
    ] {
        assert_eq!(of_v(body), want, "{body}");
    }
    // An arm says nothing past its end.
    let of_u = |body: &str| updates(&f_with(&[("u", "U"), ("i", "Int")], "U", body));
    for (body, want) in [
        ("C(..u, n: i)", 0),
        (
            "j = match u { C(..) -> 1\n D(..) -> 2 }\n C(..u, n: i + j)",
            0,
        ),
        (
            "j = match u { C(..) -> 1\n D(..) -> 2 }\n D(..u, n: i + j)",
            0,
        ),
        (
            "match u { C(..) -> { j = match u { C(..) -> 1\n D(..) -> 2 }\n C(..u, n: i + j) }\n D(..) -> u }",
            1,
        ),
        (
            "match u { C(..) -> { j = match u { C(..) -> 1\n D(..) -> 2 }\n D(..u, n: i + j) }\n D(..) -> u }",
            0,
        ),
    ] {
        assert_eq!(of_u(body), want, "{body}");
    }
    // It is known in a function made in the arm too.
    let params = [("v", "V")];
    let body = "match v {\n A(..) -> fn(i) A(..v, n: i)\n B(..) -> fn(_) v\n}";
    let source = program_with(&params, "fn(Int) V", body);
    let core = lower(&source);
    let made: Vec<&str> = core
        .split("\n\n")
        .filter(|f| f.contains(" update "))
        .collect();
    assert_eq!(made.len(), 1, "{core}");
    assert!(
        !made[0].contains("main.f(") && !made[0].contains(" ctor "),
        "{core}"
    );
}

/// An update of few fields is one whatever it keeps, and one of more when it keeps
/// twice as many.
#[test]
fn a_record_update_of_many_fields_is_one_when_it_keeps_more() {
    for n in 1..=14usize {
        for sets in 1..=n {
            let fields: String = (0..n).map(|i| format!("\tf{i} Int\n")).collect();
            let set: String = (0..sets).map(|i| format!(", f{i}: i")).collect();
            let core = f_of(&format!(
                "type P {{\n{fields}}}\nfn f(p P, i Int) P {{ P(..p{set}) }}\npub fn main() {{ println(\"\") }}\n"
            ));
            let kept = n - sets;
            let want = kept > 0 && (sets <= 3 || kept >= 2 * sets);
            assert_eq!(core.contains("update "), want, "{sets} of {n}:\n{core}");
        }
    }
}

/// An update is one wherever it is: in a branch whose value is used, and after one
/// that holds a handle.
#[test]
fn a_record_update_is_one_beside_a_branch_and_in_one() {
    let f = |inside: &str| {
        f_of(&format!(
            "import scarlet/net/socket\n\
             type P {{\n\tn Int\n\tm Int\n\tconn Option(socket::Socket)\n}}\n\
             fn open(i Int) Option(socket::Socket) {{ if i > 0 {{ None }} else {{ open(i + 1) }} }}\n\
             fn f(p P, i Int) P {{\n\
             \ta = if i > 0 {{\n\t\t{inside}\n\t}} else {{ 3 }}\n\
             \tP(..p, n: a)\n\
             }}\n\
             pub fn main() {{ println(open(0)) }}\n"
        ))
    };
    let held = f("o = open(i)\n\t\tmatch o {\n\t\t\tSome(_) -> 1\n\t\t\tNone -> 2\n\t\t}");
    assert_eq!(updates(&held), 1, "{held}");
    let made = f("q = P(..p, n: i)\n\t\tq.n + q.m");
    assert_eq!(updates(&made), 2, "{made}");
    assert_eq!(updates(&f("p.n + p.m")), 1);
}

/// A base that is only a name has nothing to run, so where its `..s` is
/// written changes nothing: all three spellings lower to the same code.
#[test]
fn record_update_of_a_name_lowers_the_same_wherever_the_spread_is() {
    let mk = |update: &str| {
        let core = lower(&format!(
            "type State {{\n\tv Array(Int)\n\tcount Int\n\tcheck Int\n}}\n\
             fn push(s State, n Int) State {{\n\
             \t{update}\n\
             }}\n\
             pub fn main() {{\n\
             \tpush(State([], 0, 0), 1).count\n\
             }}\n"
        ));
        fn_body(&core, 0)
    };
    let first = mk("State(..s, v: [..s.v, n], count: n)");
    assert_eq!(first, mk("State(v: [..s.v, n], ..s, count: n)"));
    assert_eq!(first, mk("State(v: [..s.v, n], count: n, ..s)"));
    let lines: Vec<&str> = first.lines().map(str::trim_start).collect();
    let at = |what: &str| {
        lines
            .iter()
            .position(|l| l.contains(what))
            .unwrap_or_else(|| panic!("no `{what}` in:\n{first}"))
    };
    assert!(
        at("Field(2)") < at("Field(0)")
            && at("Field(0)") < at("drop %0")
            && at("drop %0") < at("ArrayAppend"),
        "the kept field is read first, and the record is dropped before the append:\n{first}"
    );
}

/// A field or a tuple element read off a name has nothing to run either, so
/// a base such as `..o.inner` or `..t.0` may sit anywhere too. A base that
/// calls something may not: moving its spread moves the call.
#[test]
fn record_update_of_a_path_lowers_the_same_wherever_the_spread_is() {
    let mk = |param: &str, arg: &str, update: &str| {
        let core = lower(&format!(
            "type Inner {{\n\tv Array(Int)\n\ttag Int\n\tcheck Int\n}}\n\
             type Outer {{\n\tinner Inner\n\tcount Int\n}}\n\
             fn id(i Inner) Inner {{ i }}\n\
             fn push({param}, n Int) Inner {{\n\
             \t{update}\n\
             }}\n\
             pub fn main() {{\n\
             \tpush({arg}, 1).tag\n\
             }}\n"
        ));
        fn_body(&core, 1)
    };
    let field = |update: &str| mk("o Outer", "Outer(Inner([], 0, 0), 0)", update);
    let first = field("Inner(..o.inner, v: [..o.inner.v, n], tag: n)");
    assert_eq!(
        first,
        field("Inner(v: [..o.inner.v, n], ..o.inner, tag: n)")
    );
    assert_eq!(
        first,
        field("Inner(v: [..o.inner.v, n], tag: n, ..o.inner)")
    );
    assert_ne!(
        field("Inner(..id(o.inner), v: [..o.inner.v, n])"),
        field("Inner(v: [..o.inner.v, n], ..id(o.inner))")
    );
    let element = |update: &str| mk("t (Int, Inner)", "(0, Inner([], 0, 0))", update);
    assert_eq!(
        element("Inner(..t.1, v: [..t.1.v, n], tag: n)"),
        element("Inner(v: [..t.1.v, n], tag: n, ..t.1)")
    );
}

// Irrefutable ctor destructuring projects in *declared* field order, and each
// heap-typed projection gets its own `drop`. A type var is never heap-shaped,
// so a bind typed from a `fresh_var()` would silently get no `Drop`.
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
     pub fn main() {\n\
     \tf(P(LNil, LNil))\n\
     }\n"
);

// The same for tuple destructuring. Each element gets the type the checker
// inferred for it.
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
     pub fn main() {\n\
     \tf((LNil, LNil))\n\
     }\n"
);

// An `If` in operand position is the body of a join, whose parameter is of a
// type that gets a drop.
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
     pub fn main() {\n\
     \tf(True)\n\
     }\n"
);

// The arm of an or-pattern that binds names is a join that takes them. The guard
// is at each alternative, because a miss there tries the next.
core_golden!(
    or_pattern_binds,
    "type L {\n\tCons(h Int, t L)\n\tLNil\n}\n\
     fn f(a L, b L, k Int) L {\n\
     \tmatch (a, b) {\n\
     \t\t(Cons(n, t), _) | (_, Cons(n, t)) if n > k -> Cons(n + k, t)\n\
     \t\t(Cons(_, _) | LNil, Cons(_, t)) -> t\n\
     \t\t(Cons(_, _) | LNil, LNil) -> LNil\n\
     \t}\n\
     }\n\
     pub fn main() {\n\
     \tf(LNil, Cons(1, LNil), 0)\n\
     }\n"
);

// A module's constants are each set as soon as they are worked out, so one may
// read another, and the toplevel gives up its own reference after.
core_golden!(
    module_constants,
    "const base = 4\n\
     const pair = (base, [base, base + 1])\n\
     const picked = if base > 3 { pair.1 } else { [] }\n\
     fn f(i Int) Int { picked[i] or base }\n\
     pub fn main() {\n\
     \tf(1)\n\
     }\n"
);

// `True`, `False` and `Nil` are immediates, never the prelude's
// constructors, and a match on a Bool is an `if`. A match on Nil is its arm,
// and a lone arm that binds the value binds it.
core_golden!(
    bool_match_is_if,
    "fn pick(b Bool) Int {\n\
     \tmatch b {\n\
     \t\tFalse -> 0\n\
     \t\tTrue -> 1\n\
     \t}\n\
     }\n\
     fn yes(b Bool) Bool {\n\
     \tmatch b {\n\
     \t\tTrue -> False\n\
     \t\tFalse -> True\n\
     \t}\n\
     }\n\
     fn unit(n Nil) Int {\n\
     \tmatch n {\n\
     \t\tNil -> 2\n\
     \t}\n\
     }\n\
     fn same(b Bool) Bool {\n\
     \tmatch b {\n\
     \t\tx -> x\n\
     \t}\n\
     }\n\
     pub fn main() {\n\
     \tpick(yes(same(True))) + unit(Nil)\n\
     }\n"
);

// What a branch made and reads last is dropped in the branch.
core_golden!(
    join_body_drops_its_own,
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
     pub fn main() {\n\
     \tf(True)\n\
     }\n"
);

// Nothing in the source states this match's scrutinee type, which comes from
// `Some(Boxed(3))` alone. The snapshot pins the `drop` and the `AddInt`, which
// both need `b`'s real type rather than an unbound var.
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
     pub fn main() {\n\
     \tf()\n\
     }\n"
);

// A fallible match over several binary-literal arms. Every failure edge jumps
// to a continuation shared per suffix, so each arm body and the no-match trap
// are lowered exactly once.
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
     \t\t<<\"GET\">> -> Get\n\
     \t\t<<\"POST\">> -> Post\n\
     \t\t<<\"PUT\">> -> Put\n\
     \t\t<<\"DELETE\">> -> Delete\n\
     \t\t_ -> Other(m)\n\
     \t}\n\
     }\n\
     pub fn main() {\n\
     \tto_method(<<\"PUT\">>)\n\
     }\n"
);

// A guard's false edge is a failure edge like any pattern mismatch, so it
// routes to the next arm's shared continuation.
core_golden!(
    guarded_match,
    "fn clamp(n Int, lo Int, hi Int) Int {\n\
     \tmatch n {\n\
     \t\tx if x < lo -> lo\n\
     \t\tx if x > hi -> hi\n\
     \t\tx -> x\n\
     \t}\n\
     }\n\
     pub fn main() {\n\
     \tclamp(5, 0, 10)\n\
     }\n"
);

// Every alternative of `a | b | c` shares one arm body.
core_golden!(
    or_pattern_match,
    "fn small(n Int) Int {\n\
     \tmatch n {\n\
     \t\t0 | 1 -> 1\n\
     \t\t2 | 3 | 4 -> 2\n\
     \t\t_ -> 0\n\
     \t}\n\
     }\n\
     pub fn main() {\n\
     \tsmall(3)\n\
     }\n"
);

// The goldens above pin the whole IR, so they move when the type arena hands
// out a different number of vars. These only assert that a heap-typed
// projection gets released, so unrelated changes leave them green.

/// The `idx`-th printed `fn` block, counting from 0 in emission order. We pick
/// it by position because interner ids shift with the stdlib.
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

/// A destructured heap field must get its own `drop`. The bind's type has to
/// come from the constructor's fn type, because a `fresh_var()` is never
/// heap-shaped.
#[test]
fn ctor_destructure_drops_each_heap_field() {
    let core = lower(&format!(
        "{HEAP_PAIR_SRC}\
         fn f(p P) Int {{\n\
         \tP(b: y, a: x) = p\n\
         \tg(x) + g(y)\n\
         }}\n\
         pub fn main() {{\n\
         \tf(P(LNil, LNil))\n\
         }}\n"
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

/// The same for tuple destructuring. Element types come from the scrutinee's
/// `Tuple` node, not a fresh var.
#[test]
fn tuple_destructure_drops_each_heap_element() {
    let core = lower(&format!(
        "{HEAP_PAIR_SRC}\
         fn f(p (L, L)) Int {{\n\
         \t(x, y) = p\n\
         \tg(x) + g(y)\n\
         }}\n\
         pub fn main() {{\n\
         \tf((LNil, LNil))\n\
         }}\n"
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

/// The `match` spelling of the same destructure has always given up the value, with
/// a drop or by taking it apart. We pin the two spellings together so they can't
/// drift apart again.
#[test]
fn destructure_binding_matches_match_spelling() {
    let mk = |body: &str| {
        lower(&format!(
            "{HEAP_PAIR_SRC}fn f(p P) Int {{\n{body}}}\npub fn main() {{\n\tf(P(LNil, LNil))\n}}\n"
        ))
    };
    let binding = mk("\tP(x, y) = p\n\tg(x) + g(y)\n");
    let matched = mk("\tmatch p {\n\t\tP(x, y) -> g(x) + g(y)\n\t}\n");
    let count = |c: &str| {
        fn_body(c, 1)
            .lines()
            .map(str::trim_start)
            .filter(|l| l.starts_with("drop ") || l.starts_with("open "))
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

// Each program below marks every arm body with a distinct integer literal
// nothing else in the program can produce, so counting the marker in the
// printed fn body counts how many times that arm body was lowered.

/// The renumbered `ConstId` whose `where`-block value contains `marker` as a
/// standalone digit run. An integer literal always lowers to `Atom::Const`, so
/// the marker never appears in the fn body as digits.
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
    // Demand exactly one pool entry. Taking only the first id would miss a
    // re-lowered arm body that minted a fresh, undeduplicated constant.
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

/// Every segment-compare failure edge must reach the remaining arms through a
/// shared continuation, not a fresh copy of them.
#[test]
fn binary_match_arm_bodies_lowered_once() {
    assert_arm_bodies_once(
        "fn code(m Binary) Int {\n\
         \tmatch m {\n\
         \t\t<<\"GET\">> -> 101\n\
         \t\t<<\"POST\">> -> 202\n\
         \t\t<<\"PUT\">> -> 303\n\
         \t\t<<\"DELETE\">> -> 404\n\
         \t\t<<\"PATCH\">> -> 505\n\
         \t\t_ -> 606\n\
         \t}\n\
         }\n\
         pub fn main() {\n\
         \tcode(<<\"HEAD\">>)\n\
         }\n",
        &[101, 202, 303, 404, 505, 606],
    );
}

/// A false guard falls through to the next arm via the same shared
/// continuation as a pattern mismatch.
#[test]
fn guarded_match_arm_bodies_lowered_once() {
    assert_arm_bodies_once(
        "fn band(x Int, y Int) Int {\n\
         \tmatch x {\n\
         \t\tn if n < y -> 101\n\
         \t\tn if n > y -> 202\n\
         \t\t_ -> 303\n\
         \t}\n\
         }\n\
         pub fn main() {\n\
         \tband(1, 2)\n\
         }\n",
        &[101, 202, 303],
    );
}

/// The body of an arm is lowered once when an or-pattern in it binds names, and
/// so the arm may be matched in several places: with one or-pattern, with two, one
/// inside the other, one inside a tuple, a guard, and a `<<>>` pattern before it.
/// The rest of the pattern and the guard are lowered at each. The body takes the
/// names that it reads.
#[test]
fn an_or_pattern_that_binds_shares_its_arm_body() {
    let body = |arms: &str| {
        let source = format!(
            "type T {{\n\tA(x Int)\n\tB(x Int)\n\tC(t T, x Int)\n}}\n\
             fn f(t T, u T, b Binary) Int {{\n\tmatch (t, u, b) {{\n{arms}\t}}\n}}\n\
             pub fn main() {{ println(f(A(1), B(2), <<>>)) }}\n"
        );
        assert_arm_bodies_once(&source, &[101, 909]);
        fn_body(&lower(&source), 0)
    };
    let any = "A(_) | B(_) | C(_, _)";
    for (pattern, rest, adds, takes, guards) in [
        ("(A(n) | B(n), _, _)", "(C(_, _), _, _)", "n", 1, 0),
        (
            "(A(n) | B(n), A(m) | B(m), _)",
            "(A(_) | B(_) | C(_, _), A(_) | B(_) | C(_, _), _)",
            "n + m",
            2,
            0,
        ),
        (
            "(C(A(n) | B(n), m) | C(C(_, n), m), _, _)",
            "(A(_) | B(_), _, _)",
            "n + m",
            2,
            0,
        ),
        ("(A(n) | B(n), _, _) if n > 7", any, "n", 1, 2),
        (
            "(A(n) | B(n), A(m) | B(m), _) if n > m",
            "(A(_) | B(_) | C(_, _), A(_) | B(_) | C(_, _), _)",
            "n",
            1,
            4,
        ),
        ("(A(n) | B(n), _, <<k>>)", any, "n + k", 2, 0),
        (
            "(_, A(n) | B(n), <<k, _:binary>>) if k > n",
            "(_, A(_) | B(_) | C(_, _), _)",
            "n + k",
            2,
            2,
        ),
    ] {
        let rest = match rest == any {
            true => format!("({any}, _, _)"),
            false => rest.to_string(),
        };
        let arms = format!("\t\t{pattern} -> {adds} + 101\n\t\t{rest} -> 909\n");
        // The body is the continuation of the one join that takes anything.
        let body = body(&arms);
        let joins = body.lines().filter(|l| l.contains("join j"));
        let taking: Vec<&str> = joins.filter(|l| !l.contains("()")).collect();
        let [join] = taking[..] else {
            panic!("one join takes names:\n{body}");
        };
        assert_eq!(join.matches('%').count(), takes, "{join}\n{body}");
        let tests = body.matches("IntGt(").count();
        assert_eq!(tests, guards, "{body}");
    }
}

/// An arm whose pattern binds names and matches in one place takes them as they are,
/// whatever kinds of pattern are in it. So does an or-pattern that binds none.
#[test]
fn an_arm_that_matches_in_one_place_takes_no_names() {
    let core = lower(
        "type T {\n\tA(x Int)\n\tB(x Int)\n}\n\
         fn f(t T, xs Array(T), b Binary) Int {\n\
         \tmatch (t, xs, b) {\n\
         \t\t(A(_) | B(_), [A(n), ..rest], <<k, m:utf8, v:binary>>) -> {\n\
         \t\t\tif rest == [] && v == b { n + k + m } else { k }\n\
         \t\t}\n\
         \t\t(A(_) | B(_), _, <<_:8, _:binary>>) -> 1\n\
         \t\t(A(_) | B(_), _, _) -> 0\n\
         \t}\n\
         }\n\
         pub fn main() {\n\
         \tf(A(1), [], <<>>)\n\
         }\n",
    );
    let body = fn_body(&core, 0);
    let joins = body.lines().filter(|l| l.contains("join j"));
    // The one is the value of the `&&`.
    let taking = joins.filter(|l| !l.contains("()")).count();
    assert_eq!(taking, 1, "{body}");
}

/// A chain of `&&` or `||` is joins one after another and not one in the next, so
/// its length is no depth. What a branch is on is worked out in the body of its
/// join, beside it.
#[test]
fn a_chain_of_short_circuits_is_no_nest() {
    for op in ["&&", "||"] {
        let f = f_of(&format!(
            "fn f(a Int, b Int, c Int, d Int) Int {{\n\
             \tx = a < 1 {op} b < 2 {op} c < 3 {op} d < 4\n\
             \tif x {{ 1 }} else {{ 2 }}\n\
             }}\n\
             pub fn main() {{ println(f(1, 2, 3, 4)) }}\n"
        ));
        let joins = f.lines().filter(|l| l.contains("join j"));
        let depths: Vec<usize> = joins.map(|l| l.len() - l.trim_start().len()).collect();
        assert_eq!(depths, [2, 2, 2], "{f}");
        let body = f.lines().skip_while(|l| !l.contains("join j")).nth(2);
        assert!(body.is_some_and(|l| l.contains("IntLt(")), "{f}");
    }
    // Where it is returned there is a join for each but the last.
    let f = f_of(
        "fn f(a Int, b Int, c Int) Bool { a < 1 && b < 2 && c < 3 }\n\
         pub fn main() { println(f(1, 2, 3)) }\n",
    );
    assert_eq!(f.matches("join j").count(), 1, "{f}");
}

/// What may not be there is a probe, and a match on it, spelt as one or with `or`, is the
/// probe's two arms, where its value is used as where it is returned. The default runs on
/// a miss alone.
#[test]
fn a_match_on_what_may_not_be_there_is_a_probe() {
    let f = f_of(
        "fn f(xs Array(Int), i Int) Int {\n\
         \ta = xs[i] or { i * 2 }\n\
         \ta + match xs[a..i] {\n\
         \t\tOk(ys) -> ys[0] or a\n\
         \t\tErr(Nil) -> i\n\
         \t}\n\
         }\n\
         pub fn main() { println(f([1], 0)) }\n",
    );
    assert_eq!(f.matches("probe ArrayGet(").count(), 2, "{f}");
    assert_eq!(f.matches("probe ArraySlice(").count(), 1, "{f}");
    assert!(!f.contains("switch") && !f.contains("ctor"), "{f}");
    let miss = f.split("| miss ->").nth(1).expect("the first miss");
    assert!(
        miss.lines().nth(2).is_some_and(|l| l.contains("IntMul")),
        "{f}"
    );
    // Nothing is made where the value has a name that only the match reads, which comes
    // straight after it, or where the arms have guards and patterns in the value.
    let f = f_of(
        "import scarlet/map\n\
         import scarlet/array\n\
         fn f(m map::Map(Int, Array(Int)), k Int) Int {\n\
         \tfound = map::get(m, k)\n\
         \tmatch found {\n\
         \t\tSome([x, ..]) if x > k -> x\n\
         \t\tSome([]) | None -> match array::set([k], k, 1) {\n\
         \t\t\tErr(_) -> 0\n\
         \t\t\tOk([y]) -> y\n\
         \t\t\tOk(_) -> 2\n\
         \t\t}\n\
         \t\tSome(_) -> 3\n\
         \t}\n\
         }\n\
         pub fn main() { println(f(map::new(), 0)) }\n",
    );
    assert_eq!(f.matches("probe MapGet(").count(), 1, "{f}");
    assert_eq!(f.matches("probe ArraySet(").count(), 1, "{f}");
    assert!(!f.contains("switch") && !f.contains("ctor"), "{f}");
}

/// The match a probe's value is for may be on a block that ends in the probe, or on a
/// name for it, in a block that gives a value or not. The arms are lowered once, and the
/// one value has one local. Where the name is another's, or is read again, the value is
/// made.
#[test]
fn a_probe_goes_into_the_match_on_it_however_it_is_spelt() {
    let f = f_of(
        "fn f(xs Array(Int), i Int) Int {\n\
         \ta = match {\n\t\tj = i * 2\n\t\txs[j]\n\t} {\n\
         \t\tSome(x) -> x\n\t\tNone -> 777\n\t}\n\
         \tb = {\n\t\to = {\n\t\t\tk = a * 3\n\t\t\txs[k]\n\t\t}\n\
         \t\tmatch o {\n\t\t\tSome(x) -> x\n\t\t\tNone -> 888\n\t\t}\n\t}\n\
         \to = xs[b]\n\
         \tmatch o {\n\t\tSome(x) -> x\n\t\tNone -> 999\n\t}\n\
         }\n\
         pub fn main() { println(f([1], 0)) }\n",
    );
    assert_eq!(f.matches("probe ArrayGet(").count(), 3, "{f}");
    assert!(!f.contains("switch") && !f.contains("ctor"), "{f}");
    let made = |body: &str| {
        let f = f_of(&format!(
            "fn g(o Option(Int)) Int {{ o or 5 }}\n\
             fn f(xs Array(Int), i Int) Int {{\n{body}\n}}\n\
             pub fn main() {{ println(f([1], 0)) }}\n"
        ));
        assert_eq!(
            f.matches("ctor").count(),
            2 * f.matches("probe").count(),
            "{f}"
        );
        assert_eq!(f.matches("switch").count(), 1, "{f}");
        f
    };
    made("\to = xs[i]\n\tmatch o {\n\t\tSome(v) -> v + g(o)\n\t\tNone -> g(o)\n\t}");
    made("\to = xs[i]\n\tp = xs[0]\n\tmatch o {\n\t\tSome(v) -> v + g(p)\n\t\tNone -> g(p)\n\t}");
    // A name on the outermost spine is the join's own, with no copy of it.
    let f = f_of(
        "fn g(o Option(Int)) Int { o or 5 }\n\
         fn f(xs Array(Int), i Int) Int {\n\
         \to = xs[i]\n\tg(o) + g(o)\n\
         }\n\
         pub fn main() { println(f([1], 0)) }\n",
    );
    assert!(!f.contains(" = %"), "{f}");
    // What is in an operand is in front of the join and not in its body.
    let f = f_of(
        "fn f(xs Array(Int), i Int) Int { xs[xs[xs[i] or 1] or 2] or 3 }\n\
         pub fn main() { println(f([1], 0)) }\n",
    );
    let joins = f.lines().filter(|l| l.contains("join j"));
    let depths: Vec<usize> = joins.map(|l| l.len() - l.trim_start().len()).collect();
    assert_eq!(depths, [2, 2], "{f}");
}

/// Where nothing reads the value there is the probe, which a write or a slice may stop
/// at, and what comes after it.
#[test]
fn the_value_of_a_probe_that_nothing_reads_is_not_made() {
    for body in [
        "\tmatch xs[i] {\n\t\t_ -> 777\n\t}",
        "\tmatch xs[i] {\n\t\tSome(_) | None -> 777\n\t}",
        "\tmatch xs[0..i] {\n\t\tOk(_) | Err(_) -> 777\n\t}",
        "\t_ = xs[i]\n\t777",
        "\t_ = {\n\t\tj = i + 1\n\t\txs[j]\n\t}\n\t777",
        "\t_ = array::set(xs, i, 5)\n\t777",
        "\t_ = array::put(xs, i, 5)\n\t777",
        "\t_ = xs[0..i]\n\t777",
    ] {
        let f = f_of(&format!(
            "import scarlet/array\n\
             fn f(xs Array(Int), i Int) Int {{\n{body}\n}}\n\
             pub fn main() {{ println(f([1], 0)) }}\n"
        ));
        assert_eq!(f.matches("probe ").count(), 1, "{body}: {f}");
        assert_eq!(f.matches("hit(_)").count(), 1, "{body}: {f}");
        assert!(!f.contains("ctor") && !f.contains("switch"), "{body}: {f}");
    }
}

/// Any other use but a match makes the value in both arms.
#[test]
fn the_value_of_a_probe_is_made_in_its_arms() {
    let f = f_of(
        "fn g(o Option(Int), p Option(Int)) Bool { o == p }\n\
         fn f(xs Array(Int), i Int) Bool { g(xs[i], xs[i + 1]) }\n\
         pub fn main() { println(f([1], 0)) }\n",
    );
    assert_eq!(f.matches("probe ArrayGet(").count(), 2, "{f}");
    assert_eq!(f.matches("ctor").count(), 4, "{f}");
    // The joins are beside each other, with the operands in front.
    let joins = f.lines().filter(|l| l.contains("join j"));
    let depths: Vec<usize> = joins.map(|l| l.len() - l.trim_start().len()).collect();
    assert_eq!(depths, [2, 2], "{f}");
}

/// The `Nil` in an `Err` has its own type, is made once on the way that two arms read it
/// on, and is not made where nothing reads it.
#[test]
fn the_nil_of_a_miss_is_made_where_it_is_read() {
    let f = |body: &str| {
        f_of(&format!(
            "import scarlet/array\n\
             fn g(n Nil, i Int) Int {{ if n == Nil {{ i }} else {{ 0 }} }}\n\
             fn f(xs Array(Int), i Int) Int {{\n\
             \tmatch array::set(xs, i, 1) {{\n{body}\t}}\n}}\n\
             pub fn main() {{ println(f([1], 0)) }}\n"
        ))
    };
    let read = f("\t\tErr(n) if g(n, i) > 3 -> 4\n\t\tErr(n) -> g(n, 5)\n\t\tOk(_) -> 777\n");
    assert_eq!(read.matches("nil").count(), 1, "{read}");
    let not = f("\t\tErr(_) -> 4\n\t\tOk(_) -> 777\n");
    assert_eq!(not.matches("nil").count(), 0, "{not}");
    let kept = f_of(
        "import scarlet/array\n\
         fn f(xs Array(Int), i Int) Result(Array(Int), Nil) { array::set(xs, i, 1) }\n\
         pub fn main() { println(f([1], 0)) }\n",
    );
    let typed = |s: &str| {
        s.split(':')
            .nth(1)
            .map(|t| t.trim_end_matches(')').to_string())
    };
    let nil = kept
        .lines()
        .find_map(|l| typed(l.trim().strip_suffix(" = nil")?));
    let head = kept.lines().next().unwrap_or_default();
    let params: Vec<_> = head
        .split(&['(', ','][..])
        .skip(1)
        .take(2)
        .map(typed)
        .collect();
    assert!(nil.is_some() && !params.contains(&nil), "{kept}");
}

/// `array::put` gives the array back where it has no such index, and writes made of one
/// another are beside each other.
#[test]
fn a_put_is_a_probe_whose_miss_is_the_array() {
    let f = f_of(
        "import scarlet/array\n\
         fn f(xs Array(Int), i Int) Array(Int) {\n\
         \txs |> array::put(i, 1) |> array::put(i + 1, 2) |> array::put(i + 2, 3)\n\
         }\n\
         pub fn main() { println(f([1], 0)) }\n",
    );
    assert_eq!(f.matches("probe ArraySet(").count(), 3, "{f}");
    assert!(f.contains("| miss ->\n        jump j0(%0)"), "{f}");
    let joins = f.lines().filter(|l| l.contains("join j"));
    let depths: Vec<usize> = joins.map(|l| l.len() - l.trim_start().len()).collect();
    assert_eq!(depths, [2, 2], "{f}");
}

/// A built-in that asks a probe is one as a function value too, and a function of the
/// program's own with such a name is a call.
#[test]
fn a_built_in_is_a_probe_by_what_it_is_and_not_by_its_name() {
    let core = lower(
        "import scarlet/map\n\
         import scarlet/array\n\
         fn get(xs Array(Int), i Int) Option(Int) { if i > 0 { xs[i] } else { None } }\n\
         fn put(xs Array(Int), i Int, x Int) Array(Int) { [i, x, ..xs] }\n\
         fn by(f fn(Array(Int), Int, Int) Array(Int)) Array(Int) { f([1], 0, 2) }\n\
         fn f(xs Array(Int), m map::Map(Int, Int)) Int {\n\
         \ta = get(xs, 1) or 0\n\
         \tb = by(put)\n\
         \tc = by(array::put)\n\
         \tlook = map::get\n\
         \td = look(m, 1) or 1\n\
         \ta + d + array::length(b) + array::length(c)\n\
         }\n\
         pub fn main() { println(f([1], map::new())) }\n",
    );
    let all: Vec<&str> = core.split("\n\n").collect();
    let own = all.iter().find(|f| f.contains("fn main.f(")).expect("f");
    assert!(!own.contains("probe"), "{own}");
    for (name, op) in [("ArrayPut", "ArraySet"), ("MapGet", "MapGet")] {
        let wrapper = all.iter().find(|f| f.contains(&format!("fn main.{name}(")));
        let wrapper = wrapper.unwrap_or_else(|| panic!("a wrapper for {name}"));
        assert!(wrapper.contains(&format!("probe {op}(")), "{wrapper}");
    }
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
         \t\t_ -> 303\n\
         \t}\n\
         }\n\
         pub fn main() {\n\
         \tcls(3)\n\
         }\n",
        &[101, 202, 303],
    );
}

/// An alternative that always fits makes those after it dead, and no join is declared
/// that nothing jumps to. Those in front of it are tried all the same when they read a
/// size, which may call something, and not when their sizes are constants.
#[test]
fn discarded_alternative_leaves_no_unreachable_continuation() {
    let of = |first: &str| {
        lower(&format!(
            "fn g(b Binary, n Int) Int {{\n\
             \tmatch b {{\n\
             \t\t<<{first}>> | <<..>> | <<2>> -> n\n\
             \t}}\n\
             }}\n\
             pub fn main() {{\n\
             \tg(<<1>>, 8)\n\
             }}\n"
        ))
    };
    let constant = of("1");
    let constant = fn_body(&constant, 0);
    assert!(!constant.contains("join"), "{constant}");
    let core = of("1:size(n)");
    let body = fn_body(&core, 0);
    let mut declared = Vec::new();
    let mut targeted = Vec::new();
    for line in body.lines() {
        let t = line.trim();
        if let Some(label) = t
            .strip_prefix("join ")
            .and_then(|r| r.strip_suffix("() after"))
        {
            declared.push(label);
        } else if let Some(label) = t.strip_prefix("jump ").and_then(|r| r.strip_suffix("()")) {
            targeted.push(label);
        }
    }
    assert!(!declared.is_empty(), "expected join points in:\n{body}");
    for label in declared {
        assert!(
            targeted.contains(&label),
            "{label} is declared but nothing jumps to it:\n{body}"
        );
    }
}

// A match with no failure edge must lower to a single flat `Match`, with no
// needless jump.

/// Asserts there's no `join` or `jump` in the printed IR of a fn whose matches
/// can't fail.
fn assert_no_continuations(source: &str, fn_idx: usize) {
    let core = lower(source);
    let body = fn_body(&core, fn_idx);
    for needle in ["join ", "jump "] {
        assert!(
            !body.contains(needle),
            "infallible match lowered a `{}` continuation:\n{body}",
            needle.trim_end()
        );
    }
}

/// An exhaustive enum match with one arm per variant has no failure edge, so
/// it mints no continuation.
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
         pub fn main() {\n\
         \tarea(Circle(2))\n\
         }\n",
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
         pub fn main() {\n\
         \tsingle((1, 2))\n\
         }\n",
        0,
    );
}

/// In a ladder of literals with a wildcard, a miss falls to the next arm of
/// the same flat `Match`. That isn't a failure edge, because nothing resumes
/// mid-arm.
#[test]
fn literal_ladder_stays_flat() {
    assert_no_continuations(
        "fn lits(n Int) String {\n\
         \tmatch n {\n\
         \t\t1 -> \"one\"\n\
         \t\t2 -> \"two\"\n\
         \t\t_ -> \"many\"\n\
         \t}\n\
         }\n\
         pub fn main() {\n\
         \tlits(1)\n\
         }\n",
        0,
    );
}

/// An unlowerable program must be a diagnostic, never a panic.
///
/// The parser never produces an `ErrorNode` without also producing a parse
/// error, so we splice the node in by hand to reach the check walk's arm
/// directly. It becomes the whole body of an otherwise well-formed
/// `pub fn main()`, the one place a program's code can sit. Both entry points
/// must reject it with a diagnostic pointing at the bad form, and neither may
/// panic.
mod unlowerable {
    use scarlet::diagnostic::{DiagnosticCode, Severity};
    use scarlet::{ast, span::Span};

    const WELL_FORMED: &str = "pub fn main() {\n\tNil\n}\n";

    fn program_with_an_error_node() -> (ast::Expression, Span) {
        let at = Span::single_line(2, 5, 9);
        let mut program = crate::common::parse(WELL_FORMED);
        let ast::Expression::BlockExpression(module) = &mut program else {
            unreachable!("parse wraps the module in a block")
        };
        let [ast::Node::Statement(stmt)] = module.body.as_mut_slice() else {
            panic!("{WELL_FORMED:?} declares exactly `main`")
        };
        let ast::Statement::Declaration { decl, .. } = stmt.as_mut() else {
            panic!("`main` is a declaration")
        };
        let ast::Declaration::Function(main) = decl.as_mut() else {
            panic!("`main` is a function")
        };
        let ast::FnBody::Block(body) = &mut main.body else {
            panic!("`main` has a Scarlet body")
        };
        *body = ast::Expression::BlockExpression(ast::BlockExpression {
            body: vec![ast::Node::Expression(ast::Expression::ErrorNode(
                ast::ErrorNode {
                    error: scarlet::parser::ParseError::ScannerError,
                    span: at,
                },
            ))],
            span: body.span(),
        });
        (program, at)
    }

    fn compile(expr: ast::Expression, run: bool) -> scarlet::compile::Compiled {
        let ast::Expression::BlockExpression(ast) = expr else {
            unreachable!("parse wraps the module in a block")
        };
        let parsed = scarlet::build::Parsed {
            ast,
            doc: None,
            diagnostics: Vec::new(),
        };
        let mut opts = scarlet::compile::Options::program(None);
        opts.run = run;
        let mut b = scarlet::build::Build::default();
        scarlet::compile::compile(
            &mut b,
            &mut scarlet::sources::DiskSources::new(),
            "",
            parsed,
            &opts,
        )
    }

    fn assert_rejected(r: &scarlet::compile::Compiled, at: Span, what: &str) {
        assert!(!r.success(), "{what} accepted an unlowerable program");
        let d = r
            .diagnostics()
            .iter()
            .find(|d| d.severity() == Severity::Error)
            .unwrap_or_else(|| panic!("{what}: no error diagnostic: {:?}", r.diagnostics()));
        assert_eq!(
            d.code(),
            DiagnosticCode::ParseError,
            "{what}: an unreadable region is a parse error, not a compiler bug"
        );
        assert_eq!(d.span, at, "{what}: diagnostic must point at the bad form");
    }

    #[test]
    fn al_check_reports_it_rather_than_passing() {
        let (expr, at) = program_with_an_error_node();
        let r = compile(expr, false);
        assert_rejected(&r, at, "check");
    }

    #[test]
    fn al_run_reports_it_rather_than_panicking() {
        let (expr, at) = program_with_an_error_node();
        let r = compile(expr, true);
        assert_rejected(&r, at, "compile");
    }

    /// Rejecting the `ErrorNode` stops that module, not the whole pipeline. The
    /// same program with a readable `main` body still compiles to a program
    /// that starts at `main`.
    #[test]
    fn the_same_program_without_the_error_node_compiles() {
        let expr = crate::common::parse("pub fn main() {\n\t1 + 1\n}\n");
        let r = compile(expr, true);
        assert!(r.success(), "{:?}", r.diagnostics());
        let program = r.into_runnable().expect("a successful compile is runnable");
        assert!(program.main.is_some(), "a program with `main` starts there");
    }
}

/// Every constructor a program builds or matches on has its type's names in
/// `Program::types`, down to the variant. That holds for the prelude's, the
/// imported modules' and the entry file's own.
#[test]
fn every_constructor_has_names() {
    let src = "import scarlet/http\n\
               import scarlet/json\n\
               import scarlet/map\n\
               import scarlet/string\n\
               type Shape {\n\
               \tCircle(radius Int)\n\
               \tDot\n\
               }\n\
               pub fn main() {\n\
               \tprintln(string::inspect(Circle(radius: 1)))\n\
               }\n";
    let r = scarlet::compile::compile_text(src, None);
    assert!(r.success(), "{:?}", r.diagnostics());
    let program = r.into_runnable().expect("a successful compile is runnable");
    let bodies = (&program.fns)
        .into_iter()
        .chain(&program.inits)
        .chain([&program.toplevel]);
    let mut seen = 0;
    for f in bodies {
        f.core.body.for_each_variant(|v| {
            seen += 1;
            let names = program.types.get(&v.type_id).unwrap_or_else(|| {
                panic!("{}.{}: type {} has no names", f.module, f.name, v.type_id.0)
            });
            assert!(
                names.variants.get(usize::from(v.variant_idx)).is_some(),
                "{}.{}: {} has no variant {}",
                f.module,
                f.name,
                names.name,
                v.variant_idx
            );
        });
    }
    assert!(
        seen > 100,
        "only {seen} constructors: the stdlib did not compile in"
    );
    for v in program.abi.variants() {
        let names = program
            .types
            .get(&v.type_id)
            .expect("the VM's constructors have names");
        assert!(names.variants.get(usize::from(v.variant_idx)).is_some());
    }
    let option = program
        .types
        .get(&program.abi.some.type_id)
        .expect("Option");
    assert_eq!(option.name, "Option");
    assert_eq!(
        option
            .variants
            .get(usize::from(program.abi.none.variant_idx))
            .map(|v| v.name.as_str()),
        Some("None")
    );
    let shape = program
        .types
        .values()
        .find(|t| t.name == "Shape")
        .expect("the entry file's own type");
    let circle = shape.variants.first().expect("Circle");
    assert_eq!(
        (circle.name.as_str(), circle.fields.as_slice()),
        ("Circle", &["radius".to_string()][..])
    );
}
