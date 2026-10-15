#!/usr/bin/env bash
# Smoke test against a real UmaDB: starts a throwaway container and the
# HTTP server over the playground's course-simple fixture, and checks
# each status the API answers with. Needs Docker, curl, `npm run build`,
# and `cargo build --release` in hosts/rust.
set -euo pipefail
cd "$(dirname "$0")/.."

STORE_PORT="${UMADB_PORT:-50061}"
HTTP_PORT="${HTTP_PORT:-18081}"
NAME="dcb-runtime-smoke-$$"
HOST=hosts/rust/target/release/dcb-runtime-host
MODEL=playground/fixtures/course-simple.json
API="http://127.0.0.1:$HTTP_PORT"
export UMADB_URL="http://localhost:$STORE_PORT"

docker run --rm -d --name "$NAME" --publish "$STORE_PORT:50051" umadb/umadb:latest >/dev/null
server=""
cleanup() {
  [[ -n "$server" ]] && kill "$server" 2>/dev/null || true
  docker rm -f "$NAME" >/dev/null 2>&1 || true
}
trap cleanup EXIT

"$HOST" serve "$MODEL" --listen "127.0.0.1:$HTTP_PORT" --instances 2 2>/dev/null &
server=$!
for _ in $(seq 1 100); do
  curl -sf "$API/health" >/dev/null 2>&1 && break
  sleep 0.2
done

failures=0
expect() {
  local want="$1" what="$2" got
  shift 2
  got="$(curl -s -o /dev/null -w '%{http_code}' "$@")"
  if [[ "$got" == "$want" ]]; then
    echo "  ✓ $what → $want"
  else
    echo "  ✗ $what: expected $want, got $got"; failures=$((failures + 1))
  fi
}

expect 200 'health' "$API/health"
expect 200 'contract' "$API/openapi.json"
expect 200 'docs' "$API/docs"
expect 200 'define c1' -X POST "$API/commands/DefineCourse" -H 'content-type: application/json' -d '{"courseId":"c1","capacity":1}'
expect 422 'define c1 again' -X POST "$API/commands/DefineCourse" -H 'content-type: application/json' -d '{"courseId":"c1","capacity":1}'
expect 400 'mistyped arguments' -X POST "$API/commands/DefineCourse" -H 'content-type: application/json' -d '{"courseId":"c2","capacity":"1"}'
expect 400 'an undeclared argument' -X POST "$API/commands/DefineCourse" -H 'content-type: application/json' -d '{"courseId":"c2","capacity":1,"x":1}'
expect 400 'no JSON' -X POST "$API/commands/DefineCourse" -H 'content-type: application/json' -d '{'
expect 404 'no such command' -X POST "$API/commands/Nope" -H 'content-type: application/json' -d '{}'
expect 200 'subscribe s1' -X POST "$API/commands/SubscribeStudentToCourse" -H 'content-type: application/json' -d '{"courseId":"c1","studentId":"s1"}'
expect 422 'subscribe s2, course full' -X POST "$API/commands/SubscribeStudentToCourse" -H 'content-type: application/json' -d '{"courseId":"c1","studentId":"s2"}'

if out="$("$HOST" run "$MODEL" DefineCourse '{"courseId":"c3","capacity":1}')" && [[ "$out" == *'"position"'* ]]; then
  echo "  ✓ run → published"
else
  echo "  ✗ run: $out"; failures=$((failures + 1))
fi

docker rm -f "$NAME" >/dev/null
expect 503 'health, store gone' "$API/health"
expect 503 'define, store gone' -X POST "$API/commands/DefineCourse" -H 'content-type: application/json' -d '{"courseId":"c4","capacity":1}'

[[ $failures -eq 0 ]] && echo "all passed" || { echo "$failures failed"; exit 1; }
