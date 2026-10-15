#!/usr/bin/env bash
# Smoke test against a real UmaDB: starts a throwaway container, drives
# the playground's course-simple fixture through the Rust host, and
# checks each outcome. Needs Docker, `npm run build`, and
# `cargo build --release` in hosts/rust.
set -euo pipefail
cd "$(dirname "$0")/.."

PORT="${UMADB_PORT:-50061}"
NAME="dcb-runtime-smoke-$$"
HOST=hosts/rust/target/release/dcb-runtime-host
MODEL=playground/fixtures/course-simple.json
export UMADB_URL="http://localhost:$PORT"

docker run --rm -d --name "$NAME" --publish "$PORT:50051" umadb/umadb:latest >/dev/null
trap 'docker rm -f "$NAME" >/dev/null 2>&1 || true' EXIT
for _ in $(seq 1 50); do
  "$HOST" "$MODEL" DefineCourse '{"courseId":"probe","capacity":1}' >/dev/null 2>&1 && break
  sleep 0.2
done

failures=0
expect() {
  local want="$1"; shift
  local got
  got="$("$HOST" "$MODEL" "$@")"
  if [[ "$got" == *"\"outcome\":\"$want\""* ]]; then
    echo "  ✓ $1 → $want"
  else
    echo "  ✗ $1: expected $want, got $got"; failures=$((failures + 1))
  fi
}

expect published DefineCourse '{"courseId":"c1","capacity":1}'
expect rejected  DefineCourse '{"courseId":"c1","capacity":1}'
expect published SubscribeStudentToCourse '{"courseId":"c1","studentId":"s1"}'
expect rejected  SubscribeStudentToCourse '{"courseId":"c1","studentId":"s2"}'
expect error     NoSuchCommand '{}'

[[ $failures -eq 0 ]] && echo "all passed" || { echo "$failures failed"; exit 1; }
