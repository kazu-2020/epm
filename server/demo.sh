#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")"
: "${EPM_S3_BUCKET:?set EPM_S3_BUCKET}"

listen=${DEMO_LISTEN:-127.0.0.1:8080}
base="http://$listen/epm.v1.Workspace"
workspace="demo-$(date +%s)"
cell='[0,1,1,0,1]'
server_pid=""
trap '[ -z "$server_pid" ] || kill -9 "$server_pid" 2>/dev/null || true' EXIT

fail() {
  echo "FAIL: $*" >&2
  exit 1
}

check() {
  [ "$1" = "$2" ] || fail "$3: expected $2, got $1"
  echo "  ok: $3"
}

call() {
  local body
  body=$(curl -sS --max-time 30 -w '\n%{http_code}' -X POST -H 'content-type: application/json' -d "$2" "$base/$1") || return 1
  echo "${body##*$'\n'} ${body%$'\n'*}"
}

status_of() { echo "${1%% *}"; }
body_of() { echo "${1#* }"; }

start() {
  # Not only the wait below: another server on the same address also answers GetLag, and the demo would write to that server.
  ! call GetLag '{}' >/dev/null 2>&1 || fail "another server listens on $listen"
  cargo run --locked --quiet -- --listen "$listen" --workspace "$workspace" &
  server_pid=$!
  for _ in $(seq 1 150); do
    kill -0 "$server_pid" 2>/dev/null || fail "the server exited at start"
    if call GetLag '{}' >/dev/null 2>&1; then
      return
    fi
    sleep 0.2
  done
  fail "the server did not start"
}

submit() {
  local request
  request=$(jq -nc --arg id "$1" --arg measure "$2" --argjson cell "$cell" --argjson value "$3" \
    '{request_id: $id, measure: $measure, coordinate: $cell, value: $value}')
  echo "  > SubmitEdit $request" >&2
  call SubmitEdit "$request"
}

read_cells() {
  local request
  request=$(jq -nc --argjson at "$1" --arg quantity "$quantity" --arg sales "$sales" --argjson cell "$cell" \
    '{at_least: $at, cells: [{measure: $quantity, coordinate: $cell}, {measure: $sales, coordinate: $cell}]}')
  echo "  > ReadCells $request" >&2
  call ReadCells "$request"
}

read_values() {
  local response
  response=$(read_cells "$1")
  check "$(status_of "$response")" 200 "ReadCells returns 200" >&2
  echo "  < $(body_of "$response")" >&2
  body_of "$response" | jq -c '.values'
}

sequence_of() {
  check "$(status_of "$1")" 200 "SubmitEdit returns 200" >&2
  echo "  < $(body_of "$1")" >&2
  body_of "$1" | jq '.sequence'
}

echo "== Build and start the server (workspace $workspace)"
cargo build --locked --quiet
start
definitions=$(body_of "$(call ListDefinitions '{}')")
quantity=$(jq -r '.measures[] | select(.name == "数量") | .ulid' <<<"$definitions")
sales=$(jq -r '.measures[] | select(.name == "売上") | .ulid' <<<"$definitions")
echo "  quantity (数量) is $quantity, sales (売上 = 数量 * 単価) is $sales, cell is $cell"

echo "== 1. An edit to an input changes a calculated value"
seed=$(read_values null)
echo "  seed values [quantity, sales]: $seed"
first=$(sequence_of "$(submit r1 "$quantity" 3)")
after_first=$(read_values "$first")
second=$(sequence_of "$(submit r2 "$quantity" 5)")
after_second=$(read_values "$second")
check "$(jq '.[0] == 5' <<<"$after_second")" true "the quantity is 5"
[ "$(jq '.[1]' <<<"$after_first")" != "$(jq '.[1]' <<<"$after_second")" ] ||
  fail "the sales did not change"
echo "  ok: the sales changed from $(jq '.[1]' <<<"$after_first") to $(jq '.[1]' <<<"$after_second")"

echo "== 2. An edit to a calculated measure is rejected with a reason"
rejected=$(submit r3 "$sales" 1)
echo "  < $(body_of "$rejected")"
check "$(status_of "$rejected")" 400 "SubmitEdit returns 400"
check "$(body_of "$rejected" | jq -r '.code')" invalid_argument "the code is invalid_argument"

echo "== 3. A resend of the same Request ID returns the same Commit Sequence"
resent=$(sequence_of "$(submit r2 "$quantity" 5)")
check "$resent" "$second" "the resend of r2 returns sequence $second"

echo "== 4. After kill -9, a new process with a new epoch restores the values from the log"
kill -9 "$server_pid"
wait "$server_pid" 2>/dev/null || true
echo "  killed process $server_pid"
start
for _ in $(seq 1 150); do
  restored=$(read_cells "$second" 2>/dev/null) || fail "the restarted server does not answer ReadCells"
  [ "$(status_of "$restored")" = 503 ] || break
  sleep 0.2
done
check "$(status_of "$restored")" 200 "ReadCells at sequence $second returns 200"
echo "  < $(body_of "$restored")"
check "$(body_of "$restored" | jq -c '.values')" "$after_second" "the values are the same as before the kill"

echo "== All checks passed"
