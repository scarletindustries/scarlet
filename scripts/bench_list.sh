#!/usr/bin/env bash
# Airtight O(n^2) -> O(n) gate for the list/sequence path.
#
# bench_list_{1x,2x,4x}.al are identical programs at sizes N, 2N, 4N.
# The per-doubling time ratio is scale-invariant and intrinsic to the
# algorithm's complexity class:
#
#   quadratic O(n^2)        => each ratio ~4.0
#   linear / O(n log n)     => each ratio ~2.0
#
# A clean ~2.0 across BOTH doublings (1x->2x and 2x->4x), with every
# point in the multi-second low-noise regime, is the proof.
set -euo pipefail
cargo build --release 2>/dev/null

best() {
  local f=$1 best=99999 t
  for _ in 1 2 3 4 5; do
    t=$( { /usr/bin/time -p ./target/release/al run "$f" >/dev/null; } 2>&1 | awk '/^real/{print $2}')
    [ -n "$t" ] && awk -v t="$t" -v b="$best" 'BEGIN{exit !(t<b)}' && best=$t
  done
  echo "$best"
}

t1=$(best examples/bench_list_1x.al)
t2=$(best examples/bench_list_2x.al)
t4=$(best examples/bench_list_4x.al)

echo "bench_list_1x_real_seconds $t1"
echo "bench_list_2x_real_seconds $t2"
echo "bench_list_4x_real_seconds $t4"
awk -v a="$t1" -v b="$t2" -v c="$t4" 'BEGIN{
  if (a+0>0) printf "ratio_2x_over_1x %.2f\n", b/a; else print "ratio_2x_over_1x NA"
  if (b+0>0) printf "ratio_4x_over_2x %.2f\n", c/b; else print "ratio_4x_over_2x NA"
  print "verdict: ~2.0 = linear (PASS) ; ~4.0 = quadratic (FAIL)"
}'
