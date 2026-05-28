// Integers outside the 48-bit small-int range spill to BigInt and print exactly.

// 140737488355327 is SMALL_INT_MAX (2^47 - 1); adding 1 crosses into BigInt.
println(140737488355327 + 1)

// Small-int operands whose product overflows the 48-bit range spill on multiply.
println(1000000000000000 * 1000)

// The largest representable Int literal (i64::MAX) parses and prints verbatim.
println(9223372036854775807)
