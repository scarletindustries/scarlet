//! What a module shows its importers, made from its declarations once their
//! types are final.

use scarlet_syntax::span::Span;
use scarlet_types::engine::{LocalScheme, Types};
use scarlet_types::interface::{Interface, ValueDef, ValueKind};
use scarlet_types::name::Name;
use scarlet_types::ty::Scheme;

use crate::declare::Declared;
use crate::env::Env;
use crate::error::CheckError;
use crate::report::Report;
use crate::types::TypeDefs;

/// The interface, and an error for each public value whose type still holds
/// a variable nothing generalised. Such a value would have a different type
/// in every module that used it, so its type has to be written out.
pub(crate) fn interface(
    env: &Env<'_>,
    declared: &Declared<'_>,
    defs: &TypeDefs,
    schemes: &[LocalScheme],
    types: &Types,
) -> (Interface, Report) {
    let mut report = Report::new(env.input.source);
    let mut iface = Interface::new(env.key.clone());
    iface.doc = env.input.doc.clone();
    for (name, def) in &defs.types {
        if !def.public {
            iface.private_names.insert(name.clone());
        }
        iface.types.insert(name.clone(), def.clone());
    }
    let mut export = |name: &Name,
                      public: bool,
                      scheme: Scheme,
                      kind: ValueKind,
                      doc: &Option<String>,
                      span: Span| {
        if !public {
            iface.private_names.insert(name.clone());
            return;
        }
        if scheme.ty.vars().iter().any(|v| !scheme.vars.contains(v)) {
            report.add(
                span,
                CheckError::NotFullyKnown {
                    name: name.clone(),
                    ty: scheme.ty.clone(),
                },
            );
        }
        iface.values.insert(
            name.clone(),
            ValueDef {
                scheme: scheme.canonical(),
                kind,
                doc: doc.clone(),
                span,
            },
        );
    };
    for (decl, scheme) in declared.values.iter().zip(schemes) {
        export(
            &decl.name,
            decl.public,
            types.scheme(scheme),
            decl.kind(),
            &decl.doc,
            decl.span,
        );
    }
    for (name, c) in &defs.ctors {
        let kind = ValueKind::Constructor {
            ctor: c.ctor.clone(),
            name: name.clone(),
            labels: c.labels.clone(),
        };
        export(name, c.public, c.scheme.clone(), kind, &c.doc, c.span);
    }
    (iface, report)
}
