/**
 * Immutable Unicode strings.
 *
 * Strings are UTF-8 text values. Indexing is not offered; `length` and
 * `split` work in Unicode scalar values, so multi-byte characters are never
 * cut in half.
 */

// The debug rendering of any value — what `println` prints: composites
// (arrays, maps, constructors) show their structure, strings are inserted
// as-is. For a specific format, prefer the value's own `to_string`.
@vm(string__inspect)
pub fn inspect(x a) String

// The pieces of `s` between occurrences of `on`. Adjacent delimiters yield
// empty pieces; an empty `on` splits into one piece per Unicode scalar value.
@vm(string__split)
pub fn split(s String, on String) Array(String)

// The number of Unicode scalar values in `s` — not bytes. O(n): the string
// is walked, not measured.
@vm(string__length)
pub fn length(s String) Int

// Whether `needle` occurs as a substring of `s`. An empty needle is always
// found.
@vm(string__contains)
pub fn contains(s String, needle String) Bool

// `s` with Unicode whitespace removed from both ends.
@vm(string__trim)
pub fn trim(s String) String
