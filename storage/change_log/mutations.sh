#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")"
export DST_SEEDS="${1:-2000}"

declare -A quint_violations=(
  [blind_put]=refinesChangeLog,segmentsNeverChange
  [unknown_as_success]=refinesChangeLog
  [no_deduplication]=refinesChangeLog
  [no_inflight_deduplication]=refinesChangeLog
  [no_fence]=newestOwnerSeesAcknowledged
  [no_epoch_check]=staleEpochNeverCommits
  [two_in_flight]=refinesChangeLog,staleEpochNeverCommits,slotsContiguous
  [rejected_as_fenced]=
)
breaks=(blind_put unknown_as_success no_deduplication no_inflight_deduplication no_fence no_epoch_check two_in_flight)
holds=(rejected_as_fenced)
log_safe_without_guard=(no_fence no_epoch_check)

run() {
  cargo test --locked --features "$1" --test dst seeds_keep_the_invariants -- --nocapture 2>&1 || true
}

broken() {
  grep -o '^violated [A-Za-z]* in [0-9]*' <<<"$1" | sed 's/violated \([A-Za-z]*\) in \([0-9]*\)/\1:\2/' | paste -sd, - || true
}

outcome() {
  if grep -q '^violated ' <<<"$1"; then echo break
  elif grep -q '^test result: ok' <<<"$1"; then echo hold
  else echo error; fi
}

status=0
report() {
  printf '%-26s %-6s %s\n%33s model: %s\n' "$1" "$2" "$3" "" "$5"
  [[ "$2" == "$4" ]] || { echo "  expected: $4"; status=1; }
}

output=$(run "")
report designed "$(outcome "$output")" "$(broken "$output")" hold holds
for mutation in "${breaks[@]}" "${holds[@]}"; do
  output=$(run "$mutation")
  expected=break
  [[ " ${holds[*]} " == *" $mutation "* ]] && expected=hold
  report "$mutation" "$(outcome "$output")" "$(broken "$output")" "$expected" "${quint_violations[$mutation]:-holds}"
  if [[ " ${log_safe_without_guard[*]} " == *" $mutation "* && ",$(broken "$output")" == *",refinesChangeLog:"* ]]; then
    echo "  expected: refinesChangeLog holds"
    status=1
  fi
done
exit "$status"
