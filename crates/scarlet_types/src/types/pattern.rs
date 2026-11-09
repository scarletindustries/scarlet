use std::collections::HashSet;

use indexmap::IndexMap;

use super::infer::{InferEngine, Ty};
use scarlet_syntax::span::Span;

/// Which stage of an or-pattern a frame is in. The canonical set lives inside
/// `Checking` so it cannot be read before the first alternative established it.
enum OrPhase {
    /// Typing the first alternative; `seen` is what it binds so far. The first
    /// `enter_alternative` freezes those as canonical.
    Establishing { seen: HashSet<String> },
    /// Past the first alternative: every branch must re-bind exactly
    /// `canonical`.
    Checking {
        canonical: HashSet<String>,
        seen: HashSet<String>,
    },
}

/// One level of or-pattern nesting. `boundary` is `initial.len()` at `enter_or`
/// time; only the bottom frame uses it, to tell names bound before any
/// or-pattern from names its first alternative introduced.
struct OrFrame {
    boundary: usize,
    phase: OrPhase,
}

impl OrFrame {
    /// Names the current path through this or-pattern has bound.
    fn seen(&self) -> &HashSet<String> {
        match &self.phase {
            OrPhase::Establishing { seen } | OrPhase::Checking { seen, .. } => seen,
        }
    }

    fn seen_mut(&mut self) -> &mut HashSet<String> {
        match &mut self.phase {
            OrPhase::Establishing { seen } | OrPhase::Checking { seen, .. } => seen,
        }
    }
}

/// What recording one name a pattern binds came to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Bound {
    /// A new binding, the `index`th of the pattern.
    Fresh(usize),
    /// A later or-alternative binding the `index`th name again.
    Again(usize),
    /// A `_`-prefixed name: typed, but never readable.
    WriteOnly,
    /// A diagnostic has been reported.
    Rejected,
}

/// Accumulator for the variable bindings a pattern introduces. The compiler
/// records each name through [`sink`](Self::sink) as `type_pattern` reaches
/// it, and [`Bound`] tells it whether to make a new local. Nesting is handled
/// by the [`OrFrame`] stack, not by a save/restore token the caller carries.
pub struct PatternBindings {
    frames: Vec<OrFrame>,
    /// Each readable name the pattern binds, with its type, in the order the
    /// pattern binds them. A name's index here is what [`Bound`] reports.
    initial: IndexMap<String, Ty>,
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

    /// Has `name` already been bound on the current path through the pattern?
    /// Inside an or-pattern that means: bound before the outermost or, or
    /// earlier in an enclosing alternative being typed now.
    fn already_bound(&self, name: &str) -> bool {
        match self.frames.first() {
            None => self.initial.contains_key(name),
            Some(bottom) => {
                self.initial
                    .get_index_of(name)
                    .is_some_and(|i| i < bottom.boundary)
                    || self.frames.iter().any(|f| f.seen().contains(name))
            }
        }
    }

    /// The write view `type_pattern` records bindings through. `clear` and the
    /// result iterator stay on the owner, so pattern typing cannot reset the
    /// accumulator or read half-built results mid-pattern.
    pub fn sink(&mut self) -> PatternSink<'_> {
        PatternSink(self)
    }

    /// Record a binding. Pushes a diagnostic onto `engine` on duplicate-var,
    /// extra-var-in-alternative, or a type mismatch between alternatives.
    fn bind(&mut self, name: &str, ty: Ty, span: Span, engine: &mut InferEngine) -> Bound {
        if name.starts_with('_') {
            return Bound::WriteOnly;
        }
        if self.already_bound(name) {
            engine.error_at_span(
                format!("Variable '{name}' is bound more than once in this pattern"),
                span,
            );
            return Bound::Rejected;
        }
        // A binding is fresh only while every enclosing or-pattern is on its
        // first alternative. Once one is `Checking`, the name must be in that
        // frame's canonical set and unify with the type `initial` recorded for
        // its fresh binding.
        let innermost_checking = self.frames.iter().rev().find_map(|f| match &f.phase {
            OrPhase::Checking { canonical, .. } => Some(canonical),
            OrPhase::Establishing { .. } => None,
        });
        match innermost_checking {
            None => {
                let (index, _) = self.initial.insert_full(name.to_string(), ty);
                if let Some(top) = self.frames.last_mut() {
                    top.seen_mut().insert(name.to_string());
                }
                Bound::Fresh(index)
            }
            Some(canonical) => {
                if !canonical.contains(name) {
                    engine.error_at_span(
                        format!(
                            "Variable '{name}' is not bound in the first alternative of this pattern"
                        ),
                        span,
                    );
                    return Bound::Rejected;
                }
                let Some(index) = self.initial.get_index_of(name) else {
                    return Bound::Rejected;
                };
                let init_ty = self.initial[index];
                let last = self.frames.len() - 1;
                self.frames[last].seen_mut().insert(name.to_string());
                if engine.unify_at(init_ty, ty, span) {
                    Bound::Again(index)
                } else {
                    Bound::Rejected
                }
            }
        }
    }

    /// Begin typing an or-pattern. The returned [`OrScope`] owns the rest of
    /// the protocol and its `Drop` pops the frame, so an unbalanced
    /// enter/exit — including via `?` — cannot be spelt at the call site.
    fn enter_or(&mut self) -> OrScope<'_> {
        self.frames.push(OrFrame {
            boundary: self.initial.len(),
            phase: OrPhase::Establishing {
                seen: HashSet::new(),
            },
        });
        OrScope { b: self }
    }
}

/// Write-only view of a [`PatternBindings`]. Pattern typing recurses through
/// it; it cannot `clear` the accumulator, so emptying frames out from under a
/// live [`OrScope`] is unrepresentable.
pub struct PatternSink<'a>(&'a mut PatternBindings);

impl PatternSink<'_> {
    /// Record a binding. Pushes a diagnostic onto `engine` on duplicate-var,
    /// extra-var-in-alternative, or a type mismatch between alternatives.
    #[must_use = "a rejected binding means the pattern is ill-typed and must be propagated"]
    pub fn bind(&mut self, name: &str, ty: Ty, span: Span, engine: &mut InferEngine) -> Bound {
        self.0.bind(name, ty, span, engine)
    }

    /// Begin typing an or-pattern — see [`OrScope`] for the protocol.
    pub fn enter_or(&mut self) -> OrScope<'_> {
        self.0.enter_or()
    }
}

/// Scoped protocol handle for one or-pattern.
///
/// [`enter_alternative`](Self::enter_alternative) consumes the scope and only
/// [`Alt::finish`] hands it back, so an alternative can never be left
/// half-open. Dropping the scope pops the frame and folds its bound names into
/// the enclosing one; [`finish`](Self::finish) just spells that drop out.
pub struct OrScope<'a> {
    b: &'a mut PatternBindings,
}

impl<'a> OrScope<'a> {
    /// Write view of the accumulator, for typing a pattern inside this or.
    pub fn sink(&mut self) -> PatternSink<'_> {
        PatternSink(&mut *self.b)
    }

    /// This scope's frame. It is always the stack top when a method here runs:
    /// nested scopes borrow `self` and must be dropped first, and this scope's
    /// own drop is the only pop.
    #[allow(clippy::panic)]
    fn frame_mut(&mut self) -> &mut OrFrame {
        match self.b.frames.last_mut() {
            Some(f) => f,
            None => panic!("OrScope outlived its frame — compiler bug"),
        }
    }

    /// Switch to the next non-first alternative. The first call freezes the
    /// first alternative's bindings as the canonical set; later calls just
    /// reset `seen`. Consuming the scope makes skipping an alternative's
    /// completeness check a compile error rather than a runtime assert.
    pub fn enter_alternative(mut self) -> Alt<'a> {
        let f = self.frame_mut();
        f.phase = match std::mem::replace(
            &mut f.phase,
            OrPhase::Establishing {
                seen: HashSet::new(),
            },
        ) {
            OrPhase::Establishing { seen } => OrPhase::Checking {
                canonical: seen,
                seen: HashSet::new(),
            },
            OrPhase::Checking {
                canonical,
                mut seen,
            } => {
                seen.clear();
                OrPhase::Checking { canonical, seen }
            }
        };
        Alt { scope: self }
    }

    /// Finish typing this or-pattern. Explicit spelling of the scope's drop.
    pub fn finish(self) {}
}

/// The names this or-pattern bound fold into the enclosing alternative's
/// `seen`, so a sibling binding after the or-pattern still sees them for
/// duplicate detection.
impl Drop for OrScope<'_> {
    fn drop(&mut self) {
        // Never panic: this runs on unwind by design, and a panicking drop
        // during unwind aborts.
        let Some(inner) = self.b.frames.pop() else {
            debug_assert!(false, "OrScope outlived its frame — compiler bug");
            return;
        };
        let bound = match inner.phase {
            OrPhase::Establishing { seen } => seen,
            OrPhase::Checking { canonical, .. } => canonical,
        };
        if let Some(parent) = self.b.frames.last_mut() {
            parent.seen_mut().extend(bound);
        }
    }
}

/// Handle for typing one non-first alternative of an or-pattern. It owns the
/// scope, so the protocol cannot advance until [`finish`](Self::finish) has
/// run its completeness check and handed the scope back.
pub struct Alt<'a> {
    scope: OrScope<'a>,
}

impl<'a> Alt<'a> {
    /// Write view of the accumulator, for typing this alternative's pattern.
    pub fn sink(&mut self) -> PatternSink<'_> {
        PatternSink(&mut *self.scope.b)
    }

    /// Finish typing this alternative, handing the or-pattern's scope back.
    /// Reports any name from the or's canonical set that the branch failed to
    /// bind.
    #[allow(clippy::panic)]
    #[must_use = "a false result means the pattern is ill-typed and must be propagated"]
    pub fn finish(self, alt_span: Span, engine: &mut InferEngine) -> (OrScope<'a>, bool) {
        let scope = self.scope;
        // `enter_alternative` put the frame in `Checking`, and owning the
        // scope keeps nested scopes balanced above it, so the top frame is
        // ours.
        let Some(OrFrame {
            phase: OrPhase::Checking { canonical, seen },
            ..
        }) = scope.b.frames.last()
        else {
            panic!("Alt outlived its frame — compiler bug")
        };
        let mut ok = true;
        // Every canonical name is in `initial`, so iterating `initial` gives a
        // stable diagnostic order.
        for name in scope.b.initial.keys() {
            if canonical.contains(name) && !seen.contains(name) {
                engine.error_at_span(
                    format!("Variable '{name}' must be bound in every alternative of this pattern"),
                    alt_span,
                );
                ok = false;
            }
        }
        (scope, ok)
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

    // `_`-prefixed names may appear repeatedly: they are typed but write-only,
    // outside the canonical set.
    #[test]
    fn underscore_names_are_write_only() {
        let (mut e, int_ty, sp, mut b) = setup();
        assert!(b.initial.is_empty());

        assert_ne!(b.sink().bind("_", int_ty, sp, &mut e), Bound::Rejected);
        assert_ne!(b.sink().bind("_", int_ty, sp, &mut e), Bound::Rejected);
        assert_ne!(b.sink().bind("_x", int_ty, sp, &mut e), Bound::Rejected);
        assert!(
            b.initial.is_empty(),
            "'_'-prefixed names must not join the canonical set"
        );

        assert_ne!(b.sink().bind("x", int_ty, sp, &mut e), Bound::Rejected);
        assert_eq!(b.initial.len(), 1);
        assert!(e.diagnostics.is_empty(), "no diagnostics expected");
    }

    // Dropping an OrScope without `finish()` still pops its frame.
    #[test]
    fn dropping_or_scope_pops_frame() {
        let (mut e, int_ty, sp, mut b) = setup();
        {
            let mut s = b.sink();
            let mut or = s.enter_or();
            assert_ne!(or.sink().bind("x", int_ty, sp, &mut e), Bound::Rejected);
        }
        assert!(b.frames.is_empty(), "drop must pop the or frame");
        // `x` is still visible for duplicate detection.
        assert_eq!(b.sink().bind("x", int_ty, sp, &mut e), Bound::Rejected);
        assert_ne!(b.sink().bind("y", int_ty, sp, &mut e), Bound::Rejected);
    }

    // An alternative may re-bind the canonical name at a unifiable type, but
    // binding the same name twice within one alternative is an error.
    #[test]
    fn or_alternative_rebinds_canonical_name_and_rejects_duplicates() {
        let (mut e, int_ty, sp, mut b) = setup();
        let mut s = b.sink();
        let mut or = s.enter_or();
        assert_ne!(or.sink().bind("x", int_ty, sp, &mut e), Bound::Rejected);

        let mut alt = or.enter_alternative();
        assert_ne!(alt.sink().bind("x", int_ty, sp, &mut e), Bound::Rejected);
        // Re-binding `x` in the *same* alternative is a duplicate.
        assert_eq!(alt.sink().bind("x", int_ty, sp, &mut e), Bound::Rejected);
        let (or, complete) = alt.finish(sp, &mut e);
        assert!(complete);
        or.finish();

        assert_eq!(e.diagnostics.len(), 1, "exactly the duplicate is reported");
        assert!(
            e.diagnostics[0].message.contains("more than once"),
            "got: {}",
            e.diagnostics[0].message
        );
    }

    // Alternatives must bind the canonical names at unifiable types.
    #[test]
    fn or_alternative_type_conflict_rejected() {
        let (mut e, int_ty, sp, mut b) = setup();
        let str_ty = e.icon_string();
        let mut s = b.sink();
        let mut or = s.enter_or();
        assert_ne!(or.sink().bind("x", int_ty, sp, &mut e), Bound::Rejected); // canonical x : Int

        let mut alt = or.enter_alternative();
        assert_eq!(alt.sink().bind("x", str_ty, sp, &mut e), Bound::Rejected);
        let (or, complete) = alt.finish(sp, &mut e);
        assert!(complete, "`x` was bound (at the wrong type), so complete");
        or.finish();

        assert!(
            !e.diagnostics.is_empty(),
            "a type conflict across alternatives must be reported"
        );
    }

    // An or nested inside a non-first alternative is typed against the outer
    // canonical set, so re-binding the outer name is not a duplicate.
    #[test]
    fn nested_or_inside_non_first_alternative() {
        let (mut e, int_ty, sp, mut b) = setup();
        // Outer: A(a) | B( C(a) | D(a) )
        let mut s = b.sink();
        let mut outer = s.enter_or();
        assert_ne!(outer.sink().bind("a", int_ty, sp, &mut e), Bound::Rejected); // A(a)
        let mut oalt = outer.enter_alternative();
        let mut osink = oalt.sink();
        let mut inner = osink.enter_or();
        assert_ne!(inner.sink().bind("a", int_ty, sp, &mut e), Bound::Rejected); // C(a) — must NOT be a duplicate
        let mut ialt = inner.enter_alternative();
        assert_ne!(ialt.sink().bind("a", int_ty, sp, &mut e), Bound::Rejected); // D(a)
        let (inner, i_complete) = ialt.finish(sp, &mut e);
        assert!(i_complete);
        inner.finish();
        let (outer, o_complete) = oalt.finish(sp, &mut e);
        assert!(o_complete);
        outer.finish();

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
        let mut s = b.sink();
        let mut outer = s.enter_or();
        assert_ne!(outer.sink().bind("a", int_ty, sp, &mut e), Bound::Rejected);
        let mut oalt = outer.enter_alternative();
        assert_ne!(oalt.sink().bind("a", int_ty, sp, &mut e), Bound::Rejected); // B's first arg
        let mut osink = oalt.sink();
        let mut inner = osink.enter_or();
        assert_eq!(inner.sink().bind("a", int_ty, sp, &mut e), Bound::Rejected); // C(a) — dup of B's first arg
        inner.finish();
        let (outer, o_complete) = oalt.finish(sp, &mut e);
        assert!(o_complete);
        outer.finish();

        assert_eq!(e.diagnostics.len(), 1);
        assert!(
            e.diagnostics[0].message.contains("more than once"),
            "got: {}",
            e.diagnostics[0].message
        );
    }
}
