use std::collections::HashSet;

use indexmap::IndexMap;

use super::infer::{InferEngine, Ty};
use crate::span::Span;

/// Tracks which phase of an or-pattern is being typed. The first alternative
/// establishes the canonical binding set; every subsequent alternative must
/// bind exactly the same names at unifiable types.
pub enum PatternMode {
    Initial,
    /// Typing a non-first alternative of an or-pattern. `boundary` is the index
    /// into `initial` at or past which entries are *this* or-pattern's canonical
    /// bindings (the names introduced by its first alternative); earlier entries
    /// belong to an enclosing pattern. `seen` accumulates the names this branch
    /// has bound so far.
    Alternative {
        seen: HashSet<String>,
        boundary: usize,
    },
}

/// Accumulator for variable bindings introduced by a pattern. The compiler
/// runs `type_pattern` against this, then reads `initial` to allocate locals
/// and populate the type environment before compiling the arm body.
pub struct PatternBindings {
    pub mode: PatternMode,
    pub initial: IndexMap<String, (Ty, Span)>,
}

/// State captured when an or-pattern is entered so its canonical binding set
/// stays scoped to that or-pattern alone. Returned by [`PatternBindings::enter_or`]
/// and handed back to [`PatternBindings::exit_or`] once every alternative has
/// been typed. Because an or-pattern can be nested inside another pattern (and
/// even inside another or-pattern's alternative), the surrounding mode must be
/// saved and restored rather than assumed to be `Initial`.
pub struct OrPatternScope {
    saved_mode: PatternMode,
    boundary: usize,
}

impl PatternBindings {
    pub fn new() -> Self {
        Self {
            mode: PatternMode::Initial,
            initial: IndexMap::new(),
        }
    }

    /// Record a binding. Returns `false` (and pushes a diagnostic onto
    /// `engine`) on duplicate-var, extra-var-in-alternative, or a type
    /// mismatch between alternatives.
    pub fn bind(&mut self, name: &str, ty: Ty, span: Span, engine: &mut InferEngine) -> bool {
        if name == "_" {
            return true;
        }
        match &mut self.mode {
            PatternMode::Initial => {
                if self.initial.contains_key(name) {
                    engine.error_at_span(
                        format!("Variable '{name}' is bound more than once in this pattern"),
                        span,
                    );
                    return false;
                }
                self.initial.insert(name.to_string(), (ty, span));
                true
            }
            PatternMode::Alternative { seen, boundary } => {
                let boundary = *boundary;
                // The name must be one the first alternative of *this*
                // or-pattern introduced: present in `initial` at or past
                // `boundary`. A name below the boundary belongs to an enclosing
                // pattern and is not part of this or-pattern's binding set, so
                // re-binding it here is just as invalid as binding a brand-new
                // name no other alternative bound.
                let canonical = self
                    .initial
                    .get_full(name)
                    .filter(|&(idx, _, _)| idx >= boundary);
                let Some((_, _, (init_ty, _))) = canonical else {
                    engine.error_at_span(
                        format!(
                            "Variable '{name}' is not bound in the first alternative of this pattern"
                        ),
                        span,
                    );
                    return false;
                };
                if seen.contains(name) {
                    engine.error_at_span(
                        format!("Variable '{name}' is bound more than once in this pattern"),
                        span,
                    );
                    return false;
                }
                let init_ty = *init_ty;
                seen.insert(name.to_string());
                engine.unify_at(init_ty, ty, span)
            }
        }
    }

    /// Begin typing an or-pattern. The first alternative is typed in `Initial`
    /// mode so its bindings land in `initial` and become this or-pattern's
    /// canonical set; the returned scope records the surrounding mode and the
    /// `initial` boundary so both can be restored once every alternative has
    /// been typed.
    pub fn enter_or(&mut self) -> OrPatternScope {
        let boundary = self.initial.len();
        let saved_mode = std::mem::replace(&mut self.mode, PatternMode::Initial);
        OrPatternScope {
            saved_mode,
            boundary,
        }
    }

    /// Switch to alternative mode for the next branch of an or-pattern, scoped
    /// to the canonical binding boundary recorded in `scope`.
    pub fn enter_alternative(&mut self, scope: &OrPatternScope) {
        self.mode = PatternMode::Alternative {
            seen: HashSet::new(),
            boundary: scope.boundary,
        };
    }

    /// Call after typing each non-first alternative. Reports any canonical
    /// binding (a name introduced by this or-pattern's first alternative, i.e.
    /// living in `initial` at or past the recorded boundary) that this branch
    /// failed to bind. Returns `false` if any were missing.
    pub fn finish_alternative(&self, alt_span: Span, engine: &mut InferEngine) -> bool {
        let PatternMode::Alternative { seen, boundary } = &self.mode else {
            return true;
        };
        let mut ok = true;
        for name in self.initial.keys().skip(*boundary) {
            if !seen.contains(name) {
                engine.error_at_span(
                    format!("Variable '{name}' must be bound in every alternative of this pattern"),
                    alt_span,
                );
                ok = false;
            }
        }
        ok
    }

    /// Finish typing an or-pattern, restoring the mode that was in effect when
    /// [`enter_or`](Self::enter_or) was called.
    pub fn exit_or(&mut self, scope: OrPatternScope) {
        self.mode = scope.saved_mode;
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

    // `_` is never recorded as a binding (it can appear repeatedly), and a
    // freshly-defaulted accumulator starts empty; `finish_alternative` outside
    // an or-pattern (Initial mode) is a no-op that always succeeds.
    #[test]
    fn wildcard_is_not_bound_and_default_is_empty() {
        let mut e = new_engine();
        let int_ty = e.icon_int();
        let sp = Span::DUMMY;

        let mut b = PatternBindings::default();
        assert!(b.initial.is_empty());

        assert!(b.bind("_", int_ty, sp, &mut e));
        assert!(
            b.initial.is_empty(),
            "'_' must not be recorded as a binding"
        );

        assert!(b.bind("x", int_ty, sp, &mut e));
        assert_eq!(b.initial.len(), 1);

        // Initial mode: finish_alternative is a no-op that reports success.
        assert!(b.finish_alternative(sp, &mut e));
        assert!(e.diagnostics.is_empty(), "no diagnostics expected");
    }

    // An or-pattern alternative may re-bind the canonical name at a unifiable
    // type (the success path), but binding the same name twice within one
    // alternative is an error.
    #[test]
    fn or_alternative_rebinds_canonical_name_and_rejects_duplicates() {
        let mut e = new_engine();
        let int_ty = e.icon_int();
        let sp = Span::DUMMY;

        let mut b = PatternBindings::new();
        let scope = b.enter_or();
        // First alternative establishes the canonical binding `x`.
        assert!(b.bind("x", int_ty, sp, &mut e));

        // Second alternative re-binds `x` (canonical, unifiable) — accepted.
        b.enter_alternative(&scope);
        assert!(b.bind("x", int_ty, sp, &mut e));
        // Re-binding `x` again in the *same* alternative is a duplicate.
        assert!(!b.bind("x", int_ty, sp, &mut e));
        // `x` was bound, so this alternative is complete.
        assert!(b.finish_alternative(sp, &mut e));
        b.exit_or(scope);

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
        let mut e = new_engine();
        let int_ty = e.icon_int();
        let str_ty = e.icon_string();
        let sp = Span::DUMMY;

        let mut b = PatternBindings::new();
        let scope = b.enter_or();
        assert!(b.bind("x", int_ty, sp, &mut e)); // canonical x : Int

        b.enter_alternative(&scope);
        // Binding x : String in another alternative cannot unify with Int.
        assert!(!b.bind("x", str_ty, sp, &mut e));
        b.exit_or(scope);

        assert!(
            !e.diagnostics.is_empty(),
            "a type conflict across alternatives must be reported"
        );
    }
}
