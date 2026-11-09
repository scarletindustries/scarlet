#!/usr/bin/env bash
# Puts load on examples/bench_service.sca and prints requests per second and p99
# latency for each endpoint.
#
#   scripts/bench_service.sh ./target/release/scarlet [duration] [connections]
#
# The scarlet binary is $1, so you can compare two builds without rebuilding.
# This starts the server and kills it on every exit path, so nothing is left
# behind. Run this OUTSIDE any sandbox, because binding a port needs it off.
set -euo pipefail

BIN=${1:?usage: bench_service.sh <path-to-scarlet-binary> [duration] [connections]}
DURATION=${2:-5s}
CONNS=${3:-32}

[[ -x $BIN ]] || { echo "not executable: $BIN" >&2; exit 1; }

ROOT=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
SERVICE=$ROOT/examples/bench_service.sca
OHA=${OHA:-/opt/homebrew/bin/oha}
[[ -x $OHA ]] || OHA=$(command -v oha)

SRV_PID=""
cleanup() {
  if [[ -n $SRV_PID ]]; then
    kill "$SRV_PID" 2>/dev/null || true
    wait "$SRV_PID" 2>/dev/null || true
  fi
  pkill -f 'bench_service\.sca' 2>/dev/null || true
}
trap cleanup EXIT INT TERM

# A leaked server from an earlier run would answer on the port we are about to
# probe and silently benchmark the wrong binary.
pkill -f 'bench_service\.sca' 2>/dev/null || true
sleep 0.3

# Ask the kernel for a free port.
PORT=$(python3 -c 'import socket;s=socket.socket();s.bind(("127.0.0.1",0));print(s.getsockname()[1]);s.close()')
BASE=http://127.0.0.1:$PORT

"$BIN" run "$SERVICE" "$PORT" >/dev/null 2>&1 &
SRV_PID=$!

for _ in $(seq 1 100); do
  curl -fsS -o /dev/null "$BASE/" 2>/dev/null && break
  kill -0 "$SRV_PID" 2>/dev/null || { echo "server died on startup" >&2; exit 1; }
  sleep 0.1
done
curl -fsS -o /dev/null "$BASE/" || { echo "server never accepted on $PORT" >&2; exit 1; }

# Get a real, signed cookie, so /me runs the success path and not the 401 path.
COOKIE=$(curl -fsS -i -X POST -d 'user=user7&password=pw-7' "$BASE/login" \
  | sed -n 's/^Set-Cookie: \([^;]*\).*/\1/p' | tr -d '\r')
[[ -n $COOKIE ]] || { echo "no session cookie from /login" >&2; exit 1; }

# Check the server works first, because a benchmark of a broken server measures nothing.
[[ $(curl -fsS -o /dev/null -w '%{http_code}' -H "Cookie: $COOKIE" "$BASE/me") == 200 ]] \
  || { echo "/me rejected a valid cookie" >&2; exit 1; }
[[ $(curl -sS -o /dev/null -w '%{http_code}' "$BASE/me") == 401 ]] \
  || { echo "/me accepted a missing cookie" >&2; exit 1; }

# Pull rps and p99 out of oha's JSON without a jq dependency.
report() {
  local label=$1; shift
  local json
  json=$("$OHA" --no-tui --output-format json -z "$DURATION" -c "$CONNS" "$@")
  python3 - "$label" <<PY
import json, sys
d = json.loads('''$json''')
rps = d["summary"]["requestsPerSec"]
p99 = d["latencyPercentiles"]["p99"] * 1000.0
p50 = d["latencyPercentiles"]["p50"] * 1000.0
codes = d.get("statusCodeDistribution", {})
bad = sum(v for k, v in codes.items() if not k.startswith("2"))
print(f"{sys.argv[1]:<18} rps={rps:.1f}\tp50={p50:.3f}ms\tp99={p99:.3f}ms\tnon2xx={bad}")
PY
}

echo "# binary=$BIN port=$PORT duration=$DURATION connections=$CONNS"
report "GET /"          "$BASE/"
report "POST /login"    -m POST -d 'user=user7&password=pw-7' "$BASE/login"
report "GET /me"        -H "Cookie: $COOKIE" "$BASE/me"
report "GET /users/<id>" "$BASE/users/42"
report "POST /logout"   -m POST "$BASE/logout"
