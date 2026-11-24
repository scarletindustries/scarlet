use super::*;

fn program(src: &str) -> Program {
    let result = crate::compile::compile_text(src, None);
    assert!(result.success(), "{:?}", result.diagnostics());
    result.into_runnable().expect("a clean compile is runnable")
}

const SQUARE: &str = "fn square(x Int) Int { x * x }\n\
                          pub fn main() {\n\
                          \tprintln(square(3))\n\
                          }\n";

#[test]
fn the_entry_listing_is_this_files_functions_and_its_toplevel() {
    let text = listing(&program(SQUARE), Filter::Entry, None).expect("main has functions");
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
                   \tprintln(string::replace('a', 'a', 'b'))\n\
                   }\n";
    let text = listing(&program(src), Filter::Named("replace"), None).expect("replace exists");
    assert!(text.contains("fn scarlet/string.replace("), "{text}");
    assert!(!text.contains("; toplevel"), "{text}");
}

/// A listing ends with the names of the constructors it shows, and only
/// those.
#[test]
fn the_listing_names_its_constructors() {
    let src = "type Shape {\n\
                   \tCircle(radius Int)\n\
                   \tDot\n\
                   }\n\
                   fn area(s Shape) Int {\n\
                   \tmatch s {\n\
                   \t\tCircle(r) -> r * r\n\
                   \t\tDot -> 0\n\
                   \t}\n\
                   }\n\
                   pub fn main() {\n\
                   \tprintln(area(Circle(radius: 2)))\n\
                   }\n";
    let text = listing(&program(src), Filter::Entry, None).expect("main has functions");
    let (_, types) = text.split_once("\n; types\n").expect("a types section");
    let lines: Vec<&str> = types.lines().collect();
    assert_eq!(lines.len(), 1, "{types}");
    assert!(
        lines[0].ends_with(" Shape: .0 Circle(radius), .1 Dot"),
        "{types}"
    );
}

#[test]
fn a_name_nothing_has_lists_nothing() {
    assert!(listing(&program(SQUARE), Filter::Named("nope"), None).is_none());
}

/// The header counts what the listing was taken from, so a filtered
/// listing still says how big the program is and where it starts.
#[test]
fn the_summary_names_where_the_program_starts() {
    let text = listing(&program(SQUARE), Filter::Named("square"), None).expect("square exists");
    let first = text.lines().next().unwrap_or_default();
    assert!(first.starts_with("; "), "{first}");
    assert!(first.contains("2 from this file"), "{first}");
    assert!(first.ends_with("(main)"), "{first}");
}
