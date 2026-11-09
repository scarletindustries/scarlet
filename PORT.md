# Language gaps

These are the gaps a port from Gleam to Scarlet runs into. Each row is a ticket against this
repo. The cost is what the workaround costs, not what the feature would cost.

## Costly but not blocking

| gap | ticket | what it costs to route around |
|---|---|---|
| No multi-scrutinee `match` | T-181 | Gleam's `case a, b { pattern, pattern -> ... }` matches several scrutinees at once. Scarlet's `match` takes one, so `match a, b {` gives `Expected '{', but found ','`. The workaround is a tuple, which compiles and works: `match (a, b) { (3, _) -> 0 ... }`. Counted over Glyde at the revision we read, leaving out `case f(x, y) {` (a single scrutinee whose commas are inside parens), there are **34 sites: 30 in `src/` across 16 files, and 4 in `test/` across 3 files.** `websocket/frame.gleam` has a three-scrutinee one (`case codepoint < 0x80, codepoint < 0x800, codepoint < 0x10000`). It costs a tuple allocation per match and a pair of parens per arm, and it doesn't block any wave. |
