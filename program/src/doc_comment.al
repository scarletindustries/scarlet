/** This is a doc comment */
fn documented() { 42 }

result = documented()

/* This is a regular block comment */
fn undocumented() { 0 }

/** The mathematical constant PI */
const PI = 3.14159

/** The answer to life, the universe, and everything */
const ANSWER = 42

/** A random number */
random = 50

/** A user in the system */
struct User {
    /** The user's display name */
    name String,
    /** Age in years */
    age Int,
}

user = User { name: 'Alice', age: 30 }

name = user.name

/** Represents the status of an operation */
enum Status {
    /** Operation completed successfully */
    Success
    /** Operation is still in progress */
    Pending
    /** Operation failed with an error */
    Failed
}

status = Status.Success
