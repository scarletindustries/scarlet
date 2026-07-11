#!/usr/bin/env bash
# Interpreter-speed acceptance evidence for the `core_ir` totality work.
#
# The refactor is NOT compile-time-only, and assuming it was is why nobody
# measured it: jump operands became function-relative, `Op::IndexOr` replaced a
# deleted fused path, and `lower` stops inventing types, so `compile_binary`
# recovers typed opcodes (`AddInt` over `Add`) and `perceus`
# recovers `Drop`/`Reuse`. Any slowdown here needs explaining before the commit
# lands.
#
#   scripts/bench.sh                      # measure, print one line per program
#   scripts/bench.sh --save before.txt    # record a baseline
#   scripts/bench.sh --baseline before.txt  # measure and diff against it
#   scripts/bench.sh --reps 9 examples/bench_map.al   # override
#
# Reports the *best* of N wall-clock runs: the minimum is the least
# noise-contaminated estimate of the work actually done, and the thing that
# regresses when the VM gets slower.
set -euo pipefail

cd "$(dirname "$0")/.."

REPS=5
BASELINE=""
SAVE=""
PROGRAMS=()

while [[ $# -gt 0 ]]; do
  case "$1" in
    --reps)     REPS="$2"; shift 2 ;;
    --baseline) BASELINE="$2"; shift 2 ;;
    --save)     SAVE="$2"; shift 2 ;;
    --help|-h)  sed -n '2,16p' "$0"; exit 0 ;;
    --*)        echo "bench: unknown flag $1" >&2; exit 2 ;;
    *)          PROGRAMS+=("$1"); shift ;;
  esac
done

# The spec's interpreter-speed set: the heavy mixed workload, the monomorphic
# arithmetic loops that typed-opcode recovery should speed up, and the map
# workload that exercises the Perceus `Drop`/`Reuse` paths.
if [[ ${#PROGRAMS[@]} -eq 0 ]]; then
  PROGRAMS=(examples/bench_heavy.al examples/bench_typed.al examples/bench_map.al)
fi

cargo build --release >/dev/null 2>&1

# best-of-N wall clock, in seconds, for one program.
best_of() {
  local prog="$1" best="" t
  for _ in $(seq "$REPS"); do
    t=$( { /usr/bin/time -p ./target/release/al run "$prog" >/dev/null; } 2>&1 | awk '/^real/{print $2}')
    if [[ -z "$t" ]]; then
      echo "bench: $prog did not run" >&2
      exit 1
    fi
    if [[ -z "$best" ]] || awk -v t="$t" -v b="$best" 'BEGIN{exit !(t<b)}'; then
      best="$t"
    fi
  done
  printf '%s' "$best"
}

declare -a NAMES=() TIMES=()
for prog in "${PROGRAMS[@]}"; do
  [[ -f "$prog" ]] || { echo "bench: no such program $prog" >&2; exit 1; }
  secs="$(best_of "$prog")"
  NAMES+=("$prog")
  TIMES+=("$secs")
  echo "bench_best_real_seconds $prog $secs"
done

if [[ -n "$SAVE" ]]; then
  : >"$SAVE"
  for i in "${!NAMES[@]}"; do
    echo "${NAMES[$i]} ${TIMES[$i]}" >>"$SAVE"
  done
  echo "wrote $SAVE" >&2
fi

# A regression is a bug; a speedup is the intended win. Fail on >3% slower,
# which is comfortably outside best-of-5 noise on an idle machine.
if [[ -n "$BASELINE" ]]; then
  [[ -f "$BASELINE" ]] || { echo "bench: no such baseline $BASELINE" >&2; exit 2; }
  echo
  printf '%-28s %10s %10s %9s\n' program before after delta
  regressed=0
  for i in "${!NAMES[@]}"; do
    prog="${NAMES[$i]}" now="${TIMES[$i]}"
    was="$(awk -v p="$prog" '$1==p{print $2}' "$BASELINE")"
    if [[ -z "$was" ]]; then
      printf '%-28s %10s %10s %9s\n' "$prog" - "$now" new
      continue
    fi
    pct="$(awk -v a="$was" -v b="$now" 'BEGIN{printf "%+.2f", 100*(b-a)/a}')"
    printf '%-28s %10s %10s %8s%%\n' "$prog" "$was" "$now" "$pct"
    awk -v p="$pct" 'BEGIN{exit !(p>3.0)}' && regressed=1
  done
  if [[ $regressed -eq 1 ]]; then
    echo
    echo "bench: runtime regression >3% — this refactor is compile-time-only, so that is a bug" >&2
    exit 1
  fi
fi
