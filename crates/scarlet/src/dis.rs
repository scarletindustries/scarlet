//! `scarlet dis` and the REPL's `:dis`: a compiled program's Core IR as text.
//!
//! The stdlib compiles into the same [`Program`] as the user's code, so a
//! listing always picks: the entry file's own functions, or every function
//! whose name matches, from any module.

use std::fmt::Write as _;

use crate::core_ir::Program;
use crate::module::ModuleKey;

/// Which functions a listing shows.
#[derive(Clone, Copy)]
pub enum Filter<'a> {
    /// The entry file's functions, then its toplevel.
    Entry,
    /// Every function, from any module, whose name contains this.
    Named(&'a str),
}

/// The listing, or `None` when `filter` matches no function.
///
/// Each function is preceded by its `fn#N`, the number a `call fn#N` or
/// `closure fn#N` elsewhere in the listing refers to.
pub fn listing(program: &Program, filter: Filter<'_>) -> Option<String> {
    let entry = ModuleKey::main();
    let mut out = summary(program, &entry);
    let mut shown = 0;
    for (i, f) in (&program.fns).into_iter().enumerate() {
        let keep = match filter {
            Filter::Entry => f.module == entry.as_str(),
            Filter::Named(needle) => f.name.contains(needle),
        };
        if keep {
            let _ = write!(out, "\n; fn#{i}\n{f}");
            shown += 1;
        }
    }
    if let Filter::Entry = filter {
        let _ = write!(out, "\n; toplevel\n{}", program.toplevel);
        shown += 1;
    }
    (shown > 0).then_some(out)
}

/// One comment line on the program's shape, so a filtered listing still says
/// what it was taken from.
fn summary(program: &Program, entry: &ModuleKey) -> String {
    let total = (&program.fns).into_iter().count();
    let own = (&program.fns)
        .into_iter()
        .filter(|f| f.module == entry.as_str())
        .count();
    let start = match program.main {
        Some(main) => format!("starts at {main} ({})", program.fns[main].name),
        None => "runs its toplevel".to_string(),
    };
    format!(
        "; {total} functions, {own} from this file; {} module inits; {} globals; {start}\n",
        program.inits.len(),
        program.globals,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn program(src: &str) -> Program {
        let mut scanner = crate::scanner::new_scanner(src.to_string());
        let parsed = crate::parser::new_parser(&mut scanner).parse_program();
        let result =
            crate::bytecode::compile(&crate::ast::Expression::BlockExpression(parsed.ast), None);
        assert!(result.success(), "{:?}", result.diagnostics);
        result.into_runnable().expect("a clean compile is runnable")
    }

    const SQUARE: &str = "fn square(x Int) Int { x * x }\n\
                          pub fn main() {\n\
                          \tprintln(square(3))\n\
                          }\n";

    #[test]
    fn the_entry_listing_is_this_files_functions_and_its_toplevel() {
        let text = listing(&program(SQUARE), Filter::Entry).expect("main has functions");
        assert!(text.contains("fn main.square(%0:"), "{text}");
        assert!(text.contains("IntMul(%0, %0)"), "{text}");
        assert!(text.contains("\n; toplevel\nfn main.__main__("), "{text}");
        assert!(
            !text.contains("fn scarlet."),
            "listed a stdlib function:\n{text}"
        );
    }

    #[test]
    fn a_named_listing_reaches_into_the_stdlib() {
        let src = "import scarlet/string\n\
                   pub fn main() {\n\
                   \tprintln(string.replace('a', 'a', 'b'))\n\
                   }\n";
        let text = listing(&program(src), Filter::Named("replace")).expect("replace exists");
        assert!(text.contains("fn scarlet/string.replace("), "{text}");
        assert!(!text.contains("; toplevel"), "{text}");
    }

    #[test]
    fn a_name_nothing_has_lists_nothing() {
        assert!(listing(&program(SQUARE), Filter::Named("nope")).is_none());
    }

    /// The header counts what the listing was taken from, so a filtered
    /// listing still says how big the program is and where it starts.
    #[test]
    fn the_summary_names_where_the_program_starts() {
        let text = listing(&program(SQUARE), Filter::Named("square")).expect("square exists");
        let first = text.lines().next().unwrap_or_default();
        assert!(first.starts_with("; "), "{first}");
        assert!(first.contains("2 from this file"), "{first}");
        assert!(first.ends_with("(main)"), "{first}");
    }
}
