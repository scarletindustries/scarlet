//! The core IR's primitive operations: what the language's operators, literals
//! and pattern matching compile to, named without reference to any backend.
//!
//! A `PrimOp`'s operands are its atom's `args`, in the order listed on each
//! variant. Anything fixed at compile time, such as a field index, rides in
//! the variant itself. A variadic operation takes its count from `args.len()`.

/// One primitive operation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PrimOp {
    // --- Int ---
    /// `a + b`.
    IntAdd,
    /// `a - b`.
    IntSub,
    /// `a * b`.
    IntMul,
    /// `a / b`, truncating toward zero.
    IntDiv,
    /// `a % b`, the remainder, with the sign of `a`.
    IntRem,
    /// `-a`.
    IntNeg,
    /// `a == b`.
    IntEq,
    /// `a != b`.
    IntNe,
    /// `a < b`.
    IntLt,
    /// `a <= b`.
    IntLe,
    /// `a > b`.
    IntGt,
    /// `a >= b`.
    IntGe,

    // --- Float ---
    /// `a + b`.
    FloatAdd,
    /// `a - b`.
    FloatSub,
    /// `a * b`.
    FloatMul,
    /// `a / b`.
    FloatDiv,
    /// `-a`.
    FloatNeg,
    /// `a < b`.
    FloatLt,
    /// `a <= b`.
    FloatLe,
    /// `a > b`.
    FloatGt,
    /// `a >= b`.
    FloatGe,

    // --- Any type ---
    /// `a == b`, structural.
    Eq,
    /// `a != b`, structural.
    Ne,
    /// `!a` on a `Bool`.
    Not,

    // --- Operand type unresolved ---
    //
    // The operand's type is a constrained type variable elaboration could not
    // pin to one primitive (`fn add(a, b) { a + b }` is `addable`), so the
    // backend picks the operation from the values at run time.
    /// `a + b` on Int, Float or String.
    Add,
    /// `a - b` on Int or Float.
    Sub,
    /// `a * b` on Int or Float.
    Mul,
    /// `a / b` on Int or Float.
    Div,
    /// `a % b` on Int or Float.
    Rem,
    /// `-a` on Int or Float.
    Neg,
    /// `a < b` on Int or Float.
    Lt,
    /// `a <= b` on Int or Float.
    Le,
    /// `a > b` on Int or Float.
    Gt,
    /// `a >= b` on Int or Float.
    Ge,

    // --- String ---
    /// `a + b` on two strings.
    StringConcat,
    /// Every arg joined in order: an interpolated string's pieces.
    StringConcatMany,
    /// Any value's text, as an interpolated `${x}` shows it.
    ToString,

    // --- Tuples and records ---
    /// A tuple of every arg, in order.
    MakeTuple,
    /// `t.0`, `t.1` and so on: field `n` of a tuple.
    TupleField(u16),
    /// Field `n` of a constructor value whose variant is only known at run
    /// time (a projection out of a `..base` spread), checking the variant.
    Field(u16),
    /// `r.x`: field `n` of a constructor value whose variant the types have
    /// already proved.
    FieldUnchecked(u16),

    // --- Arrays ---
    /// An array of every arg, in order.
    MakeArray,
    /// `lo..hi` as a lazy range. Args: `lo`, `hi`.
    MakeRange,
    /// `[x, y, ..xs]`: the leading args pushed onto the front of the last arg.
    ArrayPrepend,
    /// `[..xs, x, y]`: the trailing args pushed onto the back of the first arg.
    ArrayAppend,
    /// `[..xs, ..ys]`. Args: `xs`, `ys`.
    ArrayConcat,
    /// `xs[i]`, as an `Option`. Args: `xs`, `i`.
    ArrayIndex,
    /// `xs[i] or d`, with no `Option` built. Args: `xs`, `i`, `d`, where `d` is
    /// already evaluated, so it must be pure.
    ArrayIndexOr,
    /// `xs[a..b]`. Args: `xs`, `a`, `b`.
    ArraySlice,
    /// The length of an array.
    ArrayLen,
    /// Element `n` of an array a pattern has already proved long enough.
    ArrayElem(u16),
    /// An array without its first `n` elements. Args: `xs`, `n`.
    ArrayDrop,

    // --- Binaries ---
    /// `<<v:size(b)>>`: the low `b` bits of Int `v`. Args: `v`, `b`.
    BinaryFromInt,
    /// `<<v:size(b)>>` on a Binary `v`: its first `b` bits. Args: `v`, `b`.
    BinaryTake,
    /// `<<s:utf8>>`: a String's bytes.
    BinaryFromString,
    /// Every arg joined in order: a binary literal's segments.
    BinaryConcatMany,
    /// A Binary's length in bits.
    BinaryBitSize,
    /// `len` bits of `bin` from bit `at`, shared rather than copied. Args:
    /// `bin`, `at`, `len`.
    BinaryView,
    /// Whether `bin` holds `prefix` at bit `at`. Args: `bin`, `at`, `prefix`.
    BinaryMatchPrefix,
    /// The UTF-8 code point at bit `at`, as `(code_point, bits_read)`, with
    /// `bits_read` 0 when no valid one starts there. Args: `bin`, `at`.
    BinaryReadUtf8,
    /// `width` bits of `bin` from bit `at`, as an Int. Args: `bin`, `at`,
    /// `width`.
    BinaryReadInt,
}
