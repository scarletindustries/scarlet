//! What to tell a person when a run stops before its program does: one
//! wording for `scarlet run` and the REPL.

use scarlet_vm::Stop;

/// Why `stop` ended the run, or `None` when there is nothing to say: output
/// closed, like a pipe into `head`, is the reader leaving and not a failure.
pub fn message(stop: &Stop) -> Option<String> {
    match stop {
        Stop::OutputClosed => None,
        Stop::NotBuiltYet(what) => Some(format!("cannot run: the new VM does not run {what} yet")),
        Stop::HeapFull => Some("the program ran out of heap".into()),
        Stop::BadProgram(what) => Some(format!(
            "internal error: {what}. This is a bug in the compiler, not in the program"
        )),
    }
}
