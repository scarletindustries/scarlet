//! What we tell someone when a run stops before its program finishes. `scarlet run` and the
//! REPL share this wording.

use scarlet_vm::Stop;

/// Says why `stop` ended the run, or `None` when there is nothing to say. Closed output, like a
/// pipe into `head`, means the reader has left, and that isn't a failure.
pub fn message(stop: &Stop) -> Option<String> {
    match stop {
        Stop::OutputClosed => None,
        Stop::NotBuiltYet(what) => Some(format!("cannot run: the new VM does not run {what} yet")),
        Stop::HeapFull => Some("the program ran out of heap".into()),
        Stop::FunctionTooLong { name } => Some(format!(
            "the function {name} is longer than the VM's jumps can reach"
        )),
        Stop::TooManyRegisters { name } => Some(format!(
            "the function {name} needs more registers than the VM can name"
        )),
        Stop::BadProgram(what) => Some(format!(
            "internal error: {what}. This is a bug in Scarlet, not in the program"
        )),
    }
}
