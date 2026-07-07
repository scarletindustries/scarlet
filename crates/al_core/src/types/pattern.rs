use std::collections::HashSet;

use indexmap::IndexMap;

use super::infer::{InferEngine, Ty};
use crate::span::Span;

/// One level of or-pattern nesting on the [`PatternBindings`] stack. While
/// `establishing` is true the first alternative is being typed and `seen`
/// accumulates the names it binds; the first `enter_alternative` freezes those
/// as `canonical` and every subsequent alternative must re-bind exactly that
/// set. `boundary` is `initial.len()` at `enter_or` time, used only on the
/// bottom frame to distinguish names bound *before* any or-pattern from names
/// an or-pattern's first alternative introduced.
struct OrFrame {
    boundary: usize,
    establishing: bool,
    canonical: HashSet<String>,
    seen: HashSet<String>,
}

/// Accumulator for variable bindings introduced by a pattern. The compiler
/// runs `type_pattern` against this, then reads [`bindings`](Self::bindings)
/// to allocate locals and populate the type environment before compiling the
/// arm body. Or-patterns push an [`OrFrame`] so nesting — including an
/// or-pattern inside a *non-first* alternative of an enclosing or — is handled
/// by the stack rather than by the caller carrying a save/restore token.
pub struct PatternBindings {
    frames: Vec<OrFrame>,
    initial: IndexMap<String, (Ty, Span)>,
}

impl PatternBindings {
    pub fn new() -> Self {
        Self {
            frames: Vec::new(),
            initial: IndexMap::new(),
        }
    }

    /// Reset for reuse across match arms, retaining allocated capacity.
    pub fn clear(&mut self) {
        self.frames.clear();
        self.initial.clear();
    }

    /// Iterate the canonical bindings this pattern introduces, in insertion
    /// order. This is the read-only view the compiler uses to allocate locals
    /// once `type_pattern` has finished.
    pub fn bindings(&self) -> impl Iterator<Item = (&str, &(Ty, Span))> {
        self.initial.iter().map(|(k, v)| (k.as_str(), v))
    }

    /// Has `name` already been bound on the current path through the pattern?
    /// Outside any or-pattern that is simply "is it in `initial`"; inside, it
    /// is "was it bound before the outermost or, or earlier in any enclosing
    /// alternative currently being typed".
    fn already_bound(&self, name: &str) -> bool {
        match self.frames.first() {
            None => self.initial.contains_key(name),
            Some(bottom) => {
                self.initial
                    .get_index_of(name)
                    .is_some_and(|i| i < bottom.boundary)
                    || self.frames.iter().any(|f| f.seen.contains(name))
            }
        }
    }

    /// Record a binding. Returns `false` (and pushes a diagnostic onto
    /// `engine`) on duplicate-var, extra-var-in-alternative, or a type
    /// mismatch between alternatives.
    pub fn bind(&mut self, name: &str, ty: Ty, span: Span, engine: &mut InferEngine) -> bool {
        if name == "_" {
            return true;
        }
        if self.already_bound(name) {
            engine.error_at_span(
                format!("Variable '{name}' is bound more than once in this pattern"),
                span,
            );
            return false;
        }
        // A binding is *fresh* (goes into `initial`) only when every enclosing
        // or-pattern is still on its first alternative. As soon as any frame is
        // past its first alternative, the name must belong to that frame's
        // canonical set and we unify against the type `initial` recorded when
        // it was first (freshly) bound.
        match self.frames.iter().rposition(|f| !f.establishing) {
            None => {
                self.initial.insert(name.to_string(), (ty, span));
                if let Some(top) = self.frames.last_mut() {
                    top.seen.insert(name.to_string());
                }
                true
            }
            Some(cf) => {
                if !self.frames[cf].canonical.contains(name) {
                    engine.error_at_span(
                        format!(
                            "Variable '{name}' is not bound in the first alternative of this pattern"
                        ),
                        span,
                    );
                    return false;
                }
                let init_ty = self.initial[name].0;
                let last = self.frames.len() - 1;
                self.frames[last].seen.insert(name.to_string());
                engine.unify_at(init_ty, ty, span)
            }
        }
    }

    /// Begin typing an or-pattern. Pushes a fresh frame in establishing mode;
    /// the first alternative's bindings become this or's canonical set on the
    /// first [`enter_alternative`](Self::enter_alternative).
    pub fn enter_or(&mut self) {
        self.frames.push(OrFrame {
            boundary: self.initial.len(),
            establishing: true,
            canonical: HashSet::new(),
            seen: HashSet::new(),
        });
    }

    /// Switch to the next non-first alternative of the innermost or-pattern.
    /// The first call freezes the first alternative's bindings as the
    /// canonical set; subsequent calls just reset `seen`.
    pub fn enter_alternative(&mut self) {
        let Some(f) = self.frames.last_mut() else {
            debug_assert!(false, "enter_alternative outside enter_or");
            return;
        };
        if f.establishing {
            f.canonical = std::mem::take(&mut f.seen);
            f.establishing = false;
        } else {
            f.seen.clear();
        }
    }

    /// Call after typing each non-first alternative. Reports any name from the
    /// innermost or's canonical set that this branch failed to bind.
    pub fn finish_alternative(&self, alt_span: Span, engine: &mut InferEngine) -> bool {
        let Some(f) = self.frames.last() else {
            debug_assert!(false, "finish_alternative outside enter_or");
            return true;
        };
        debug_assert!(
            !f.establishing,
            "finish_alternative called before enter_alternative"
        );
        let mut ok = true;
        // Every canonical name is in `initial` (it was inserted while all
        // enclosing frames were establishing), so iterate `initial` for a
        // stable diagnostic order.
        for name in self.initial.keys() {
            if f.canonical.contains(name) && !f.seen.contains(name) {
                engine.error_at_span(
                    format!("Variable '{name}' must be bound in every alternative of this pattern"),
                    alt_span,
                );
                ok = false;
            }
        }
        ok
    }

    /// Finish typing the innermost or-pattern. The names it bound (its
    /// canonical set, or `seen` if it never left establishing mode) become
    /// part of the enclosing alternative's `seen` so a sibling binding after
    /// the or-pattern still sees them for duplicate detection.
    pub fn exit_or(&mut self) {
        let Some(inner) = self.frames.pop() else {
            debug_assert!(false, "exit_or without enter_or");
            return;
        };
        let bound = if inner.establishing {
            inner.seen
        } else {
            inner.canonical
        };
        if let Some(parent) = self.frames.last_mut() {
            parent.seen.extend(bound);
        }
    }
}

impl Default for PatternBindings {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::super::infer::new_engine;
    use super::*;

    fn setup() -> (InferEngine, Ty, Span, PatternBindings) {
        let mut e = new_engine();
        let int_ty = e.icon_int();
        (e, int_ty, Span::DUMMY, PatternBindings::new())
    }

    // `_` is never recorded as a binding (it can appear repeatedly), and a
    // freshly-defaulted accumulator starts empty.
    #[test]
    fn wildcard_is_not_bound_and_default_is_empty() {
        let (mut e, int_ty, sp, mut b) = setup();
        assert!(b.initial.is_empty());

        assert!(b.bind("_", int_ty, sp, &mut e));
        assert!(
            b.initial.is_empty(),
            "'_' must not be recorded as a binding"
        );

        assert!(b.bind("x", int_ty, sp, &mut e));
        assert_eq!(b.initial.len(), 1);
        assert!(e.diagnostics.is_empty(), "no diagnostics expected");
    }

    // An or-pattern alternative may re-bind the canonical name at a unifiable
    // type (the success path), but binding the same name twice within one
    // alternative is an error.
    #[test]
    fn or_alternative_rebinds_canonical_name_and_rejects_duplicates() {
        let (mut e, int_ty, sp, mut b) = setup();
        b.enter_or();
        // First alternative establishes the canonical binding `x`.
        assert!(b.bind("x", int_ty, sp, &mut e));

        // Second alternative re-binds `x` (canonical, unifiable) — accepted.
        b.enter_alternative();
        assert!(b.bind("x", int_ty, sp, &mut e));
        // Re-binding `x` again in the *same* alternative is a duplicate.
        assert!(!b.bind("x", int_ty, sp, &mut e));
        // `x` was bound, so this alternative is complete.
        assert!(b.finish_alternative(sp, &mut e));
        b.exit_or();

        assert_eq!(e.diagnostics.len(), 1, "exactly the duplicate is reported");
        assert!(
            e.diagnostics[0].message.contains("more than once"),
            "got: {}",
            e.diagnostics[0].message
        );
    }

    // Every alternative of an or-pattern must bind the canonical names at
    // *unifiable* types; a conflicting type is rejected at the bind site.
    #[test]
    fn or_alternative_type_conflict_rejected() {
        let (mut e, int_ty, sp, mut b) = setup();
        let str_ty = e.icon_string();
        b.enter_or();
        assert!(b.bind("x", int_ty, sp, &mut e)); // canonical x : Int

        b.enter_alternative();
        // Binding x : String in another alternative cannot unify with Int.
        assert!(!b.bind("x", str_ty, sp, &mut e));
        b.exit_or();

        assert!(
            !e.diagnostics.is_empty(),
            "a type conflict across alternatives must be reported"
        );
    }

    // Regression: an or-pattern nested inside a *non-first* alternative of an
    // enclosing or used to reset to Initial mode, so re-binding the outer
    // canonical name in the inner's first branch tripped the duplicate check.
    // With the frame stack the inner or is typed against the outer's canonical
    // set and no spurious diagnostic is produced.
    #[test]
    fn nested_or_inside_non_first_alternative() {
        let (mut e, int_ty, sp, mut b) = setup();
        // Outer: A(a) | B( C(a) | D(a) )
        b.enter_or();
        assert!(b.bind("a", int_ty, sp, &mut e)); // A(a)
        b.enter_alternative();
        b.enter_or();
        assert!(b.bind("a", int_ty, sp, &mut e)); // C(a) — must NOT be a duplicate
        b.enter_alternative();
        assert!(b.bind("a", int_ty, sp, &mut e)); // D(a)
        assert!(b.finish_alternative(sp, &mut e));
        b.exit_or();
        assert!(b.finish_alternative(sp, &mut e));
        b.exit_or();

        assert!(
            e.diagnostics.is_empty(),
            "no diagnostics expected, got: {:?}",
            e.diagnostics.iter().map(|d| &d.message).collect::<Vec<_>>()
        );
        assert_eq!(b.initial.len(), 1);
    }

    // A name already bound in the enclosing alternative (before a nested or)
    // is still a duplicate if the nested or binds it again.
    #[test]
    fn nested_or_rejects_duplicate_from_enclosing_alt() {
        let (mut e, int_ty, sp, mut b) = setup();
        // Outer: A(a) | B(a, C(a) | D(a)) — B's first arg and the inner or both bind `a`.
        b.enter_or();
        assert!(b.bind("a", int_ty, sp, &mut e));
        b.enter_alternative();
        assert!(b.bind("a", int_ty, sp, &mut e)); // B's first arg
        b.enter_or();
        assert!(!b.bind("a", int_ty, sp, &mut e)); // C(a) — dup of B's first arg
        b.exit_or();
        b.exit_or();

        assert_eq!(e.diagnostics.len(), 1);
        assert!(
            e.diagnostics[0].message.contains("more than once"),
            "got: {}",
            e.diagnostics[0].message
        );
    }
}
