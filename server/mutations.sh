#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")"
export DST_SEEDS="${1:-2000}"

mutations=(apply_before_commit skip_commit apply_twice swap_commits change_log/no_fence change_log/no_epoch_check)
declare -A must_break=(
  [apply_before_commit]="engineAppliesEveryCommit recalculatedGrowsWithinLog"
  [skip_commit]="engineAppliesEveryCommit newestOwnerRecalculatesAcknowledged"
  [apply_twice]="engineAppliesEveryCommit"
  [swap_commits]="engineAppliesEveryCommit"
  [change_log/no_fence]="dataWriteNeverSeesOlderEpoch"
  [change_log/no_epoch_check]="staleEpochNeverCommits"
)

run() {
  cargo test --locked --features "$1" --test dst seeds_keep_the_invariants -- --nocapture 2>&1 || true
}

broken() {
  grep -o '^violated [A-Za-z]* in [0-9]*' <<<"$1" | sed 's/violated \([A-Za-z]*\) in \([0-9]*\)/\1:\2/' | paste -sd, - || true
}

status=0
output=$(run "")
printf '%-26s %s\n' designed "$(broken "$output")"
if grep -q '^violated ' <<<"$output" || ! grep -q '^test result: ok' <<<"$output"; then
  echo "  expected: every property holds"
  status=1
fi
for mutation in "${mutations[@]}"; do
  output=$(run "$mutation")
  printf '%-26s %s\n' "$mutation" "$(broken "$output")"
  for property in ${must_break[$mutation]}; do
    if ! grep -q "^violated $property in " <<<"$output"; then
      echo "  expected: $property breaks"
      status=1
    fi
  done
done
exit "$status"
