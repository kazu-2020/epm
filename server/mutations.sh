#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")"
export DST_SEEDS="${1:-2000}"

# Not a test of the history checker: skip_commit, apply_twice and swap_commits break it only through the order guard in Engine::apply_batch.
mutations=(apply_before_commit skip_commit apply_twice swap_commits no_admission_check snapshot_start_after snapshot_without_request_ids snapshot_ahead change_log/no_fence change_log/no_epoch_check change_log/commit_rejected)
declare -A must_break=(
  [apply_before_commit]="engineAppliesEveryCommit recalculatedGrowsWithinLog"
  [skip_commit]="engineAppliesEveryCommit"
  [apply_twice]="engineAppliesEveryCommit"
  [swap_commits]="engineAppliesEveryCommit"
  [no_admission_check]="engineAppliesEveryCommit"
  [snapshot_start_after]="noAlarm"
  [snapshot_without_request_ids]="snapshotMatchesLog"
  [snapshot_ahead]="snapshotMatchesLog"
  [change_log/no_fence]="dataWriteNeverSeesOlderEpoch"
  [change_log/no_epoch_check]="staleEpochNeverCommits"
  [change_log/commit_rejected]="invalidNeverCommits"
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
