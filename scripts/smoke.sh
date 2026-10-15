#!/usr/bin/env bash
# Smoke test against a real event store: starts a throwaway container of
# it and the HTTP server over the playground's course-simple fixture, and
# checks each status the API answers with.
#
#   scripts/smoke.sh umadb|tephra
#
# Needs Docker, curl, `npm run build`, and `cargo build --release` in
# hosts/rust.
set -euo pipefail
cd "$(dirname "$0")/.."

STORE_PORT="${STORE_PORT:-50061}"
HTTP_PORT="${HTTP_PORT:-18081}"
NAME="dcb-runtime-smoke-$$"
HOST=hosts/rust/target/release/dcb-runtime-host
MODEL=playground/fixtures/course-simple.json
API="http://127.0.0.1:$HTTP_PORT"
# A store that went away is noticed by this much at the latest.
export DCB_STORE_TIMEOUT="${DCB_STORE_TIMEOUT:-3s}"

case "${1:-}" in
  umadb)  IMAGE=umadb/umadb:latest; PORT=50051; export DCB_STORE="http://localhost:$STORE_PORT" ;;
  tephra) IMAGE=git.tqwewe.com/tephra/tephra:latest; PORT=9000; export DCB_STORE="tephra://localhost:$STORE_PORT" ;;
  *) echo "usage: $0 umadb|tephra" >&2; exit 2 ;;
esac
echo "$1 ($IMAGE)"

docker run --rm -d --name "$NAME" --publish "$STORE_PORT:$PORT" "$IMAGE" >/dev/null
server=""
cleanup() {
  [[ -n "$server" ]] && kill "$server" 2>/dev/null || true
  docker rm -f "$NAME" >/dev/null 2>&1 || true
}
trap cleanup EXIT

"$HOST" serve "$MODEL" --listen "127.0.0.1:$HTTP_PORT" --instances 8 2>/dev/null &
server=$!
for _ in $(seq 1 100); do
  curl -sf "$API/health" >/dev/null 2>&1 && break
  # The server gives up when the store does not answer yet; try again.
  kill -0 "$server" 2>/dev/null || { "$HOST" serve "$MODEL" --listen "127.0.0.1:$HTTP_PORT" --instances 8 2>/dev/null & server=$!; }
  sleep 0.2
done

failures=0
expect() {
  local want="$1" what="$2" got
  shift 2
  got="$(curl -s --max-time 20 -o /dev/null -w '%{http_code}' "$@" || true)"
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

# A race for the last seat: the append condition must let exactly one
# subscription through; the others are refused (422) or, having lost
# every retry, told to retry (409) – never accepted twice.
curl -s --max-time 20 -o /dev/null -X POST "$API/commands/DefineCourse" -H 'content-type: application/json' -d '{"courseId":"race","capacity":1}'
statuses="$(for i in $(seq 1 40); do
  curl -s --max-time 20 -o /dev/null -w '%{http_code}\n' -X POST "$API/commands/SubscribeStudentToCourse" \
    -H 'content-type: application/json' -d "{\"courseId\":\"race\",\"studentId\":\"r$i\"}" &
done; wait)"
accepted="$(grep -c '^200$' <<<"$statuses" || true)"
others="$(grep -vc '^\(200\|409\|422\)$' <<<"$statuses" || true)"
if [[ "$accepted" == 1 && "$others" == 0 ]]; then
  echo "  ✓ 40 racing for one seat → 1 accepted ($(sort <<<"$statuses" | uniq -c | tr -s ' ' | paste -sd, -))"
else
  echo "  ✗ 40 racing for one seat: $(sort <<<"$statuses" | uniq -c | tr -s ' ' | paste -sd, -)"; failures=$((failures + 1))
fi

if out="$("$HOST" run "$MODEL" DefineCourse '{"courseId":"c3","capacity":1}')" && [[ "$out" == *'"position"'* ]]; then
  echo "  ✓ run → published"
else
  echo "  ✗ run: $out"; failures=$((failures + 1))
fi

docker rm -f "$NAME" >/dev/null
expect 503 'health, store gone' "$API/health"
expect 503 'define, store gone' -X POST "$API/commands/DefineCourse" -H 'content-type: application/json' -d '{"courseId":"c4","capacity":1}'

[[ $failures -eq 0 ]] && echo "all passed" || { echo "$failures failed"; exit 1; }
