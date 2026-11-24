//! `scarlet dis` and the REPL's `:dis` print a compiled program's Core IR as text.
//!
//! The stdlib compiles into the same [`Program`] as the user's code, so a listing always picks
//! one of two sets: the entry file's own functions, or every function from any module whose name
//! matches.
//!
//! `scarlet dis --instrs` also prints, after each function's Core IR, the register instructions
//! the VM loads it as. Those are what the VM runs and what `scarlet run --counters` counts. A
//! counted run also counts each imported module's toplevel, which a listing does not show: it
//! has the entry file's toplevel only.

use std::collections::BTreeSet;
use std::fmt::Write as _;

use scarlet_vm::Listing;

use crate::core_ir::{CoreExpr, Program};
use crate::module::ModuleKey;

/// Which functions a listing shows.
#[derive(Clone, Copy)]
pub enum Filter<'a> {
    /// The entry file's functions, then its toplevel.
    Entry,
    /// Every function from any module whose name contains this.
    Named(&'a str),
}

/// The listing, or `None` when `filter` matches no function.
///
/// Each function is preceded by its `fn#N`, the number a `call fn#N` or
/// `closure fn#N` elsewhere in the listing refers to. The listing ends with
/// the names of the constructors it shows, so `ctor 512.0(%1)` can be read.
///
/// With `loaded`, each function's Core IR is followed by the instructions the
/// VM loaded it as.
pub fn listing(program: &Program, filter: Filter<'_>, loaded: Option<&Listing>) -> Option<String> {
    let entry = ModuleKey::main();
    let mut out = summary(program, &entry);
    let mut shown: Vec<&CoreExpr> = Vec::new();
    for (i, f) in (&program.fns).into_iter().enumerate() {
        let keep = match filter {
            Filter::Entry => f.module == entry.as_str(),
            Filter::Named(needle) => f.name.contains(needle),
        };
        if keep {
            let _ = write!(out, "\n; fn#{i}\n{f}");
            if let Some(instrs) = loaded.and_then(|l| l.function(i)) {
                out.push_str(instrs);
            }
            shown.push(&f.core.body);
        }
    }
    if let Filter::Entry = filter {
        let _ = write!(out, "\n; toplevel\n{}", program.toplevel);
        if let Some(loaded) = loaded {
            out.push_str(loaded.toplevel());
        }
        shown.push(&program.toplevel.core.body);
    }
    if shown.is_empty() {
        return None;
    }
    types(program, &shown, &mut out);
    Some(out)
}

/// One comment line per type a constructor in `bodies` belongs to:
/// `; 512 Option: .0 Some(value), .1 None`.
fn types(program: &Program, bodies: &[&CoreExpr], out: &mut String) {
    let mut ids = BTreeSet::new();
    for body in bodies {
        body.for_each_variant(|v| {
            ids.insert(v.type_id);
        });
    }
    if ids.is_empty() {
        return;
    }
    out.push_str("\n; types\n");
    for id in ids {
        let Some(t) = program.types.get(&id) else {
            let _ = writeln!(out, "; {} has no names", id.0);
            continue;
        };
        let _ = write!(out, "; {} {}:", id.0, t.name);
        for (i, v) in t.variants.iter().enumerate() {
            let sep = if i == 0 { " " } else { ", " };
            let _ = write!(out, "{sep}.{i} {}", v.name);
            if !v.fields.is_empty() {
                let _ = write!(out, "({})", v.fields.join(", "));
            }
        }
        out.push('\n');
    }
}

/// Writes one comment line about the program's shape, so a filtered listing still says what it
/// was taken from.
fn summary(program: &Program, entry: &ModuleKey) -> String {
    let total = (&program.fns).into_iter().count();
    let own = (&program.fns)
        .into_iter()
        .filter(|f| f.module == entry.as_str())
        .count();
    let start = match program.main {
        Some(main) => match program.fns.get(main) {
            Some(f) => format!("starts at {main} ({})", f.name),
            None => format!("starts at {main}, which is not a function in this program"),
        },
        None => "runs its toplevel".to_string(),
    };
    format!(
        "; {total} functions, {own} from this file; {} module inits; {} globals; {start}\n",
        program.inits.len(),
        program.globals,
    )
}

#[cfg(test)]
mod tests;
