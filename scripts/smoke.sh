#!/usr/bin/env bash
# Smoke test against a real event store: starts a throwaway container of
# it and the HTTP server over the playground's course-simple fixture, and
# checks each status the API answers with, and paging through /read.
#
#   scripts/smoke.sh umadb|tephra|postgres|axonserver|eventsourcingdb|memory
#   scripts/smoke.sh compose
#
# `memory` needs no container. `compose` runs examples/docker-compose.yml
# instead of the host built here – the image named by DCB_IMAGE, the
# published one by default – with the model it fetches by URL.
#
# Needs Docker, curl, jq, `npm run build`, and `cargo build --release` in
# hosts/rust.
set -euo pipefail
cd "$(dirname "$0")/.."

STORE_PORT="${STORE_PORT:-50061}"
HTTP_PORT="${HTTP_PORT:-18081}"
NAME="dcb-runtime-smoke-$$"
HOST=hosts/rust/target/release/dcb-runtime-host
MODEL=playground/fixtures/course-simple.dcb.json
API="http://127.0.0.1:$HTTP_PORT"
# A store that went away is noticed by this much at the latest.
export DCB_STORE_TIMEOUT="${DCB_STORE_TIMEOUT:-3s}"

DOCKER_ARGS=()
IMAGE_ARGS=()
KIND="${1:-}"
case "${1:-}" in
  umadb)  IMAGE=umadb/umadb:latest; PORT=50051; export DCB_STORE="umadb://localhost:$STORE_PORT" ;;
  tephra) IMAGE=git.tqwewe.com/tephra/tephra:latest; PORT=9000; export DCB_STORE="tephra://localhost:$STORE_PORT" ;;
  postgres)
    IMAGE=postgres:17; PORT=5432; DOCKER_ARGS=(--env POSTGRES_PASSWORD=smoke)
    export DCB_STORE="postgres://postgres:smoke@localhost:$STORE_PORT/postgres" ;;
  axonserver)
    IMAGE=axoniq/axonserver:latest; PORT=8124; DOCKER_ARGS=(--env AXONIQ_AXONSERVER_STANDALONE_DCB=true)
    export DCB_STORE="axonserver://localhost:$STORE_PORT" ;;
  eventsourcingdb)
    IMAGE=thenativeweb/eventsourcingdb:latest; PORT=3000
    IMAGE_ARGS=(run --api-token smoke --data-directory-temporary --http-enabled --https-enabled=false)
    export DCB_STORE="eventsourcingdb://smoke@localhost:$STORE_PORT" ;;
  memory) IMAGE=""; export DCB_STORE="memory://" ;;
  compose) IMAGE="${DCB_IMAGE:-ghcr.io/bwaidelich/dcb-runtime:latest}"; KIND=postgres ;;
  *) echo "usage: $0 umadb|tephra|postgres|axonserver|eventsourcingdb|memory|compose" >&2; exit 2 ;;
esac
echo "$1 (${IMAGE:-in memory})"

server=""
OVERRIDE=""
cleanup() {
  [[ -n "$server" ]] && kill "$server" 2>/dev/null || true
  if [[ -n "$OVERRIDE" ]]; then
    docker compose -p "$NAME" -f examples/docker-compose.yml -f "$OVERRIDE" down -v >/dev/null 2>&1 || true
    rm -f "$OVERRIDE"
  else
    docker rm -f "$NAME" >/dev/null 2>&1 || true
  fi
}
trap cleanup EXIT

if [[ "$1" == compose ]]; then
  OVERRIDE="$(mktemp)"
  cat >"$OVERRIDE" <<YAML
services:
  runtime:
    image: $IMAGE
    environment:
      DCB_INSTANCES: "8"
      DCB_WITH_READ: "true"
    ports: !override
      - "127.0.0.1:$HTTP_PORT:8080"
YAML
  compose() { docker compose -p "$NAME" -f examples/docker-compose.yml -f "$OVERRIDE" "$@"; }
  compose up -d --quiet-pull >/dev/null 2>&1
  # The image compiles the component on its first start.
  for _ in $(seq 1 300); do
    curl -sf "$API/health" >/dev/null 2>&1 && break
    sleep 0.2
  done
else
  [[ -n "$IMAGE" ]] && docker run --rm -d --name "$NAME" --publish "$STORE_PORT:$PORT" ${DOCKER_ARGS[@]+"${DOCKER_ARGS[@]}"} "$IMAGE" ${IMAGE_ARGS[@]+"${IMAGE_ARGS[@]}"} >/dev/null
  "$HOST" serve "$MODEL" --listen "127.0.0.1:$HTTP_PORT" --instances 8 --with-docs --with-read 2>/dev/null &
  server=$!
  for _ in $(seq 1 100); do
    curl -sf "$API/health" >/dev/null 2>&1 && break
    # The server gives up when the store does not answer yet; try again.
    kill -0 "$server" 2>/dev/null || { "$HOST" serve "$MODEL" --listen "127.0.0.1:$HTTP_PORT" --instances 8 --with-docs --with-read 2>/dev/null & server=$!; }
    sleep 0.2
  done
fi

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

# What a jq filter makes of a response.
shows() {
  local want="$1" what="$2" filter="$3" got
  shift 3
  got="$(curl -s --max-time 20 "$@" | jq -r "$filter" 2>/dev/null || true)"
  if [[ "$got" == "$want" ]]; then
    echo "  ✓ $what → $want"
  else
    echo "  ✗ $what: expected $want, got $got"; failures=$((failures + 1))
  fi
}

# The positions a read returns, and its `next`: "1,2 next=null".
page() {
  local want="$1" what="$2" got
  shift 2
  got="$(curl -s --max-time 20 -X POST "$@" | jq -r '"\([.events[].position] | join(",")) next=\(.next)"' 2>/dev/null || true)"
  if [[ "$got" == "$want" ]]; then
    echo "  ✓ $what → $want"
  else
    echo "  ✗ $what: expected $want, got $got"; failures=$((failures + 1))
  fi
}

expect 200 'health' "$API/health"
shows "ok $KIND null" 'health, empty store' '"\(.status) \(.store.kind) \(.store.head)"' "$API/health"
shows 'true' 'health, durationMs' '.store.durationMs | type == "number"' "$API/health"
expect 200 'contract' "$API/openapi.json"
shows 'true' 'contract describes /health' '.paths["/health"].get != null' "$API/openapi.json"
expect 200 'docs' "$API/docs"
expect 200 'define c1' -X POST "$API/commands/DefineCourse" -H 'content-type: application/json' -d '{"courseId":"c1","capacity":1}'
expect 422 'define c1 again' -X POST "$API/commands/DefineCourse" -H 'content-type: application/json' -d '{"courseId":"c1","capacity":1}'
expect 400 'mistyped arguments' -X POST "$API/commands/DefineCourse" -H 'content-type: application/json' -d '{"courseId":"c2","capacity":"1"}'
expect 400 'an undeclared argument' -X POST "$API/commands/DefineCourse" -H 'content-type: application/json' -d '{"courseId":"c2","capacity":1,"x":1}'
expect 400 'no JSON' -X POST "$API/commands/DefineCourse" -H 'content-type: application/json' -d '{'
expect 404 'no such command' -X POST "$API/commands/Nope" -H 'content-type: application/json' -d '{}'
expect 200 'subscribe s1' -X POST "$API/commands/SubscribeStudentToCourse" -H 'content-type: application/json' -d '{"courseId":"c1","studentId":"s1"}'
expect 422 'subscribe s2, course full' -X POST "$API/commands/SubscribeStudentToCourse" -H 'content-type: application/json' -d '{"courseId":"c1","studentId":"s2"}'

# The store now holds CourseDefined c1 at 1, StudentSubscribedToCourse s1 at 2.
if curl -s "$API/openapi.json" | jq -e '.paths["/read"].post' >/dev/null; then
  echo "  ✓ contract describes /read"
else
  echo "  ✗ contract does not describe /read"; failures=$((failures + 1))
fi
page '1,2 next=null' 'read all' "$API/read"
shows '2' 'health, head' '.store.head' "$API/health"
page '1,2 next=null' 'read all, {}' "$API/read" -H 'content-type: application/json' -d '{}'
page '1 next=1' 'read a page' "$API/read?limit=1"
page '2 next=2' 'read the next page' "$API/read?limit=1&after=1"
page ' next=null' 'read past the end' "$API/read?after=2"
page '2 next=null' 'read by type' "$API/read" -H 'content-type: application/json' -d '{"items":[{"types":["StudentSubscribedToCourse"]}]}'
page '2 next=null' 'read by tags' "$API/read" -H 'content-type: application/json' -d '{"items":[{"tags":["CourseId:c1","StudentId:s1"]}]}'
page '1,2 next=null' 'read by items, ORed' "$API/read" -H 'content-type: application/json' \
  -d '{"items":[{"types":["CourseDefined"],"tags":["CourseId:c1"]},{"types":["StudentSubscribedToCourse"]}]}'
if [[ "$1" == axonserver ]]; then
  expect 501 'read backwards' -X POST "$API/read?backwards=true"
else
  page '2,1 next=null' 'read backwards' "$API/read?backwards=true"
  page '2 next=2' 'read backwards, a page' "$API/read?backwards=true&limit=1"
  page '1 next=1' 'read backwards, the next page' "$API/read?backwards=true&limit=1&after=2"
  page ' next=null' 'read backwards past the start' "$API/read?backwards=true&after=1"
fi
expect 400 'read, limit 0' -X POST "$API/read?limit=0"
expect 400 'read, limit too high' -X POST "$API/read?limit=1001"
expect 400 'read, after no position' -X POST "$API/read?after=x"
expect 400 'read, backwards no boolean' -X POST "$API/read?backwards=yes"
expect 400 'read, an unknown parameter' -X POST "$API/read?from=1"
expect 400 'read, no items' -X POST "$API/read" -H 'content-type: application/json' -d '{"items":[]}'
expect 400 'read, an empty item' -X POST "$API/read" -H 'content-type: application/json' -d '{"items":[{}]}'
expect 400 'read, empty types' -X POST "$API/read" -H 'content-type: application/json' -d '{"items":[{"types":[]}]}'
expect 400 'read, no JSON' -X POST "$API/read" -H 'content-type: application/json' -d '{'

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

if [[ "$1" != compose ]]; then
  if out="$("$HOST" run "$MODEL" DefineCourse '{"courseId":"c3","capacity":1}')" && [[ "$out" == *'"position"'* ]]; then
    echo "  ✓ run → published"
  else
    echo "  ✗ run: $out"; failures=$((failures + 1))
  fi
fi

if [[ "$1" != memory ]]; then
  if [[ "$1" == compose ]]; then compose stop postgres >/dev/null 2>&1; else docker rm -f "$NAME" >/dev/null; fi
  expect 503 'health, store gone' "$API/health"
  shows "$KIND" 'health, store gone, kind' '.store.kind' "$API/health"
  expect 503 'read, store gone' -X POST "$API/read"
  expect 503 'define, store gone' -X POST "$API/commands/DefineCourse" -H 'content-type: application/json' -d '{"courseId":"c4","capacity":1}'
fi

[[ $failures -eq 0 ]] && echo "all passed" || { echo "$failures failed"; exit 1; }
