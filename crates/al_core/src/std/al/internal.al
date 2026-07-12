/**
 * VM introspection hooks; unstable, for debugging — not a stable API.
 */

// The number of stack frames in the calling process — see examples/tco.al.
@vm(internal__stack_depth)
pub fn stack_depth() Int
