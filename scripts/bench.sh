#!/usr/bin/env bash
set -euo pipefail
cargo build --release 2>/dev/null
best=999
for i in 1 2 3 4 5; do
  t=$( { /usr/bin/time -p ./target/release/al run examples/bench_heavy.al >/dev/null; } 2>&1 | awk '/^real/{print $2}')
  awk -v t="$t" -v b="$best" 'BEGIN{exit !(t<b)}' && best=$t
done
echo "bench_best_real_seconds $best"
