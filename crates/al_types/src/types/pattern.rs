use std::collections::HashSet;

use indexmap::IndexMap;

use super::infer::{InferEngine, Ty};
use al_syntax::span::Span;

/// Which stage of an or-pattern a frame is in. The canonical set only exists
/// once the first alternative has been fully typed, so it lives inside
/// `Checking` — reading it while still establishing is a compile error, not a
/// silently-empty `HashSet`.
enum OrPhase {
    /// The first alternative is being typed; `seen` accumulates the names it
    /// binds. The first `enter_alternative` freezes them as canonical.
    Establishing { seen: HashSet<String> },
    /// Past the first alternative: every branch must re-bind exactly
    /// `canonical`; `seen` tracks what the current branch has bound so far.
    Checking {
        canonical: HashSet<String>,
        seen: HashSet<String>,
    },
}

/// One level of or-pattern nesting on the [`PatternBindings`] stack.
/// `boundary` is `initial.len()` at `enter_or` time, used only on the bottom
/// frame to distinguish names bound *before* any or-pattern from names an
/// or-pattern's first alternative introduced.
struct OrFrame {
    boundary: usize,
    phase: OrPhase,
}

impl OrFrame {
    /// Names the current path through this or-pattern has bound, regardless
    /// of phase.
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

/// Accumulator for variable bindings introduced by a pattern. The compiler
/// runs `type_pattern` against the write view ([`sink`](Self::sink)), then
/// reads [`bindings`](Self::bindings) to allocate locals and populate the
/// type environment before compiling the arm body. Or-patterns push an
/// [`OrFrame`] so nesting — including an or-pattern inside a *non-first*
/// alternative of an enclosing or — is handled by the stack rather than by
/// the caller carrying a save/restore token.
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
                    || self.frames.iter().any(|f| f.seen().contains(name))
            }
        }
    }

    /// The write view `type_pattern` records bindings through. It exposes
    /// only [`bind`](PatternSink::bind) and [`enter_or`](PatternSink::enter_or)
    /// — `clear` and the result iterator stay here on the owner, so pattern
    /// typing cannot reset the accumulator (or read half-built results)
    /// mid-pattern.
    pub fn sink(&mut self) -> PatternSink<'_> {
        PatternSink(self)
    }

    /// Record a binding. Returns `false` (and pushes a diagnostic onto
    /// `engine`) on duplicate-var, extra-var-in-alternative, or a type
    /// mismatch between alternatives.
    fn bind(&mut self, name: &str, ty: Ty, span: Span, engine: &mut InferEngine) -> bool {
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
        // past its first alternative (`Checking`), the name must belong to the
        // innermost such frame's canonical set and we unify against the type
        // `initial` recorded when it was first (freshly) bound.
        let innermost_checking = self.frames.iter().rev().find_map(|f| match &f.phase {
            OrPhase::Checking { canonical, .. } => Some(canonical),
            OrPhase::Establishing { .. } => None,
        });
        match innermost_checking {
            None => {
                self.initial.insert(name.to_string(), (ty, span));
                if let Some(top) = self.frames.last_mut() {
                    top.seen_mut().insert(name.to_string());
                }
                true
            }
            Some(canonical) => {
                if !canonical.contains(name) {
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
                self.frames[last].seen_mut().insert(name.to_string());
                engine.unify_at(init_ty, ty, span)
            }
        }
    }

    /// Begin typing an or-pattern. Pushes a fresh frame in establishing phase
    /// and returns the scoped handle that owns the rest of the protocol —
    /// alternatives are entered/finished through the [`OrScope`], and the
    /// frame is popped by the scope's `Drop`. The frame's push and pop are
    /// tied to one value's lifetime, so "alternative outside an or-pattern",
    /// "exit without enter", and "enter without exit" (including via `?` or
    /// early return while the scope is live) are all unrepresentable at the
    /// call site.
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

/// Write-only view of a [`PatternBindings`], obtained from
/// [`PatternBindings::sink`], [`OrScope::sink`], or [`Alt::sink`]. Pattern
/// typing recurses through this view, which can record bindings and open
/// or-pattern scopes but can neither `clear` the accumulator nor read the
/// result iterator — so emptying frames out from under a live [`OrScope`] is
/// unrepresentable.
pub struct PatternSink<'a>(&'a mut PatternBindings);

impl PatternSink<'_> {
    /// Record a binding. Returns `false` (and pushes a diagnostic onto
    /// `engine`) on duplicate-var, extra-var-in-alternative, or a type
    /// mismatch between alternatives.
    #[must_use = "a false result means the pattern is ill-typed and must be propagated"]
    pub fn bind(&mut self, name: &str, ty: Ty, span: Span, engine: &mut InferEngine) -> bool {
        self.0.bind(name, ty, span, engine)
    }

    /// Begin typing an or-pattern — see [`OrScope`] for the protocol.
    pub fn enter_or(&mut self) -> OrScope<'_> {
        self.0.enter_or()
    }
}

/// Scoped protocol handle for one or-pattern, obtained from
/// [`PatternSink::enter_or`]. Sub-patterns are typed through
/// [`sink`](Self::sink) (a nested or-pattern pushes and pops its own frame
/// there, in balanced fashion). Non-first alternatives are entered by
/// [`enter_alternative`](Self::enter_alternative), which consumes the scope;
/// the scope only comes back from [`Alt::finish`], so an alternative can
/// never be left half-open. Dropping the scope pops this or's frame and folds
/// its bound names into the enclosing frame, so an early return cannot leave
/// a stale frame behind. [`finish`](Self::finish) is the explicit spelling of
/// that drop.
pub struct OrScope<'a> {
    b: &'a mut PatternBindings,
}

impl<'a> OrScope<'a> {
    /// Write view of the accumulator, for typing a pattern inside this or.
    pub fn sink(&mut self) -> PatternSink<'_> {
        PatternSink(&mut *self.b)
    }

    /// The frame [`PatternBindings::enter_or`] pushed for this scope. It is
    /// the top of the stack whenever a method here runs: nested scopes borrow
    /// `self` through [`bindings`](Self::bindings) and must be dropped before
    /// this one is usable again, and this scope's own drop is the only pop.
    #[allow(clippy::panic)]
    fn frame_mut(&mut self) -> &mut OrFrame {
        match self.b.frames.last_mut() {
            Some(f) => f,
            None => panic!("OrScope outlived its frame — compiler bug"),
        }
    }

    /// Switch to the next non-first alternative of this or-pattern. Consumes
    /// the scope: it is held by the returned [`Alt`] and only handed back by
    /// [`Alt::finish`], so skipping an alternative's completeness check —
    /// entering the next alternative, or finishing the or, with the previous
    /// alternative half-open — fails to compile rather than hitting a runtime
    /// assert. The first call freezes the first alternative's bindings as the
    /// canonical set; subsequent calls just reset `seen`.
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

    /// Finish typing this or-pattern. Explicit spelling of the scope's drop —
    /// see the `Drop` impl for what popping the frame entails.
    pub fn finish(self) {}
}

/// Popping the frame is tied to the scope's lifetime rather than a method the
/// caller must remember to reach on every path: the names this or-pattern
/// bound (its canonical set, or `seen` if it never left the establishing
/// phase) become part of the enclosing alternative's `seen` so a sibling
/// binding after the or-pattern still sees them for duplicate detection.
impl Drop for OrScope<'_> {
    fn drop(&mut self) {
        // Never panic here: a panicking drop during unwind aborts, and this
        // runs on unwind by design. The frame's existence is guaranteed
        // structurally — this drop is the only pop, one per `enter_or` push.
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

/// Handle for typing one non-first alternative of an or-pattern, obtained
/// from [`OrScope::enter_alternative`]. It owns the scope, so the or-pattern
/// protocol cannot advance (next alternative, finish) until this
/// alternative's completeness check ([`finish`](Self::finish)) has run and
/// handed the scope back. Dropping the handle drops the scope, so the
/// early-return path still pops the frame.
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
        // `enter_alternative` put the frame in `Checking` and this handle's
        // ownership keeps every nested scope balanced above it, so the top
        // frame is ours and checking.
        let Some(OrFrame {
            phase: OrPhase::Checking { canonical, seen },
            ..
        }) = scope.b.frames.last()
        else {
            panic!("Alt outlived its frame — compiler bug")
        };
        let mut ok = true;
        // Every canonical name is in `initial` (it was inserted while all
        // enclosing frames were establishing), so iterate `initial` for a
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

    // `_` is never recorded as a binding (it can appear repeatedly), and a
    // freshly-defaulted accumulator starts empty.
    #[test]
    fn wildcard_is_not_bound_and_default_is_empty() {
        let (mut e, int_ty, sp, mut b) = setup();
        assert!(b.initial.is_empty());

        assert!(b.sink().bind("_", int_ty, sp, &mut e));
        assert!(
            b.initial.is_empty(),
            "'_' must not be recorded as a binding"
        );

        assert!(b.sink().bind("x", int_ty, sp, &mut e));
        assert_eq!(b.initial.len(), 1);
        assert!(e.diagnostics.is_empty(), "no diagnostics expected");
    }

    // Dropping an OrScope without an explicit `finish()` — the early-return
    // path — still pops its frame, so later pattern typing is unaffected.
    #[test]
    fn dropping_or_scope_pops_frame() {
        let (mut e, int_ty, sp, mut b) = setup();
        {
            let mut s = b.sink();
            let mut or = s.enter_or();
            assert!(or.sink().bind("x", int_ty, sp, &mut e));
        }
        assert!(b.frames.is_empty(), "drop must pop the or frame");
        // `x` is still visible for duplicate detection; fresh names still bind.
        assert!(!b.sink().bind("x", int_ty, sp, &mut e));
        assert!(b.sink().bind("y", int_ty, sp, &mut e));
    }

    // An or-pattern alternative may re-bind the canonical name at a unifiable
    // type (the success path), but binding the same name twice within one
    // alternative is an error.
    #[test]
    fn or_alternative_rebinds_canonical_name_and_rejects_duplicates() {
        let (mut e, int_ty, sp, mut b) = setup();
        let mut s = b.sink();
        let mut or = s.enter_or();
        // First alternative establishes the canonical binding `x`.
        assert!(or.sink().bind("x", int_ty, sp, &mut e));

        // Second alternative re-binds `x` (canonical, unifiable) — accepted.
        let mut alt = or.enter_alternative();
        assert!(alt.sink().bind("x", int_ty, sp, &mut e));
        // Re-binding `x` again in the *same* alternative is a duplicate.
        assert!(!alt.sink().bind("x", int_ty, sp, &mut e));
        // `x` was bound, so this alternative is complete.
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

    // Every alternative of an or-pattern must bind the canonical names at
    // *unifiable* types; a conflicting type is rejected at the bind site.
    #[test]
    fn or_alternative_type_conflict_rejected() {
        let (mut e, int_ty, sp, mut b) = setup();
        let str_ty = e.icon_string();
        let mut s = b.sink();
        let mut or = s.enter_or();
        assert!(or.sink().bind("x", int_ty, sp, &mut e)); // canonical x : Int

        let mut alt = or.enter_alternative();
        // Binding x : String in another alternative cannot unify with Int.
        assert!(!alt.sink().bind("x", str_ty, sp, &mut e));
        // The scope is only reachable back through the alternative's finish.
        let (or, complete) = alt.finish(sp, &mut e);
        assert!(complete, "`x` was bound (at the wrong type), so complete");
        or.finish();

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
        let mut s = b.sink();
        let mut outer = s.enter_or();
        assert!(outer.sink().bind("a", int_ty, sp, &mut e)); // A(a)
        let mut oalt = outer.enter_alternative();
        let mut osink = oalt.sink();
        let mut inner = osink.enter_or();
        assert!(inner.sink().bind("a", int_ty, sp, &mut e)); // C(a) — must NOT be a duplicate
        let mut ialt = inner.enter_alternative();
        assert!(ialt.sink().bind("a", int_ty, sp, &mut e)); // D(a)
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
        assert!(outer.sink().bind("a", int_ty, sp, &mut e));
        let mut oalt = outer.enter_alternative();
        assert!(oalt.sink().bind("a", int_ty, sp, &mut e)); // B's first arg
        let mut osink = oalt.sink();
        let mut inner = osink.enter_or();
        assert!(!inner.sink().bind("a", int_ty, sp, &mut e)); // C(a) — dup of B's first arg
        inner.finish();
        // `outer` is unreachable until `oalt` is finished — the scope only
        // comes back through the alternative's completeness check.
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
