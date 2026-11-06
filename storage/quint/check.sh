#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")"
samples="${1:-50000}"
invariants=(refinesChangeLog epochNeverDecreases segmentsNeverChange staleEpochNeverCommits manifestSegmentsExist newestOwnerSeesAcknowledged)
witnesses=(witnessNotTwoAcknowledged witnessNoCommitAfterHandover witnessNoOwnResendRejected witnessNoOldOwnerStopped)
breaks=(
  s3_blind_write:refinesChangeLog s3_unknown_as_success:refinesChangeLog s3_no_deduplication:refinesChangeLog
  s3_no_fence:newestOwnerSeesAcknowledged
  s3_no_epoch_check:staleEpochNeverCommits
  postgres_unknown_as_success:refinesChangeLog postgres_no_deduplication:refinesChangeLog
)
holds=(postgres_segment_key_without_epoch postgres_no_epoch_check)
log_safe_without_guard=(s3_no_fence s3_no_epoch_check)
scenarios=(postgres_blind_write s3_segment_key_without_epoch)

seqlog_invariants=(refinesChangeLog segmentsNeverChange staleEpochNeverCommits slotsContiguous newestOwnerSeesAcknowledged oneContentPerSlotPerEpoch dataWriteNeverSeesOlderEpoch)
seqlog_breaks=(
  seqlog_blind_put:refinesChangeLog seqlog_unknown_as_success:refinesChangeLog seqlog_no_deduplication:refinesChangeLog
  seqlog_no_inflight_deduplication:refinesChangeLog
  seqlog_no_fence:newestOwnerSeesAcknowledged
  seqlog_blind_put:segmentsNeverChange
  seqlog_no_epoch_check:staleEpochNeverCommits
  seqlog_two_in_flight:staleEpochNeverCommits seqlog_two_in_flight:slotsContiguous
)
seqlog_holds=(seqlog_rejected_as_fenced)
seqlog_log_safe_without_guard=(seqlog_no_fence seqlog_no_epoch_check)

listed=$(printf '%s\n' "${breaks[@]%%:*}" "${holds[@]}" "${scenarios[@]}" | sort -u)
defined=$(sed -n 's/^module \([a-z0-9_]*\).*/\1/p' mutations.qnt | sort)
if [[ "$listed" != "$defined" ]]; then
  echo "check.sh does not list the same modules as mutations.qnt:"
  diff <(echo "$listed") <(echo "$defined") || true
  exit 1
fi
listed=$(printf '%s\n' "${seqlog_breaks[@]%%:*}" "${seqlog_holds[@]}" | sort -u)
defined=$(sed -n 's/^module \([a-z0-9_]*\).*/\1/p' seqlog_mutations.qnt | sort)
if [[ "$listed" != "$defined" ]]; then
  echo "check.sh does not list the same modules as seqlog_mutations.qnt:"
  diff <(echo "$listed") <(echo "$defined") || true
  exit 1
fi

quint typecheck change_log.qnt
quint typecheck storage.qnt
quint typecheck mutations.qnt
quint typecheck seqlog.qnt
quint typecheck seqlog_mutations.qnt
quint test seqlog.qnt --main seqlog_designed
quint test seqlog_mutations.qnt --main seqlog_two_in_flight --match pipelineAcknowledgesPastHoleTest
quint test seqlog_mutations.qnt --main seqlog_rejected_as_fenced --match ownResendRejectedTest
for module in "${scenarios[@]}"; do
  quint test mutations.qnt --main "$module"
done

run() {
  quint run "$1" --main "$2" --invariants "${@:3}" --max-samples "$samples" --max-steps 30 --seed 1 2>&1 || true
}

broken() {
  grep -o '❌ .*' <<<"$1" | sed 's/❌ //' | paste -sd, - || true
}

status=0
report() {
  printf '%-36s %-6s %s\n' "$1" "$2" "$3"
  [[ "$2" == "$4" ]] || { echo "  expected: $4"; status=1; }
}

outcome() {
  if grep -q '^\[violation\]' <<<"$1"; then echo break
  elif grep -q '^\[ok\]' <<<"$1"; then echo hold
  else echo error; fi
}

for backend in s3 postgres; do
  output=$(run storage.qnt "$backend" "${invariants[@]}")
  report "$backend" "$(outcome "$output")" "$(broken "$output")" hold
  for witness in "${witnesses[@]}"; do
    report "$backend $witness" "$(outcome "$(run storage.qnt "$backend" "$witness")")" "" break
  done
done

for entry in "${breaks[@]}"; do
  module="${entry%%:*}"
  invariant="${entry##*:}"
  report "$module $invariant" "$(outcome "$(run mutations.qnt "$module" "$invariant")")" "" break
done
for module in "${holds[@]}"; do
  output=$(run mutations.qnt "$module" "${invariants[@]}")
  report "$module" "$(outcome "$output")" "$(broken "$output")" hold
done
for module in "${log_safe_without_guard[@]}"; do
  output=$(run mutations.qnt "$module" refinesChangeLog)
  report "$module refinesChangeLog" "$(outcome "$output")" "$(broken "$output")" hold
done
output=$(run seqlog.qnt seqlog_designed "${seqlog_invariants[@]}")
report seqlog_designed "$(outcome "$output")" "$(broken "$output")" hold
for witness in "${witnesses[@]}"; do
  report "seqlog_designed $witness" "$(outcome "$(run seqlog.qnt seqlog_designed "$witness")")" "" break
done
for entry in "${seqlog_breaks[@]}"; do
  module="${entry%%:*}"
  invariant="${entry##*:}"
  report "$module $invariant" "$(outcome "$(run seqlog_mutations.qnt "$module" "$invariant")")" "" break
done
for module in "${seqlog_holds[@]}"; do
  output=$(run seqlog_mutations.qnt "$module" "${seqlog_invariants[@]}")
  report "$module" "$(outcome "$output")" "$(broken "$output")" hold
done
for module in "${seqlog_log_safe_without_guard[@]}"; do
  output=$(run seqlog_mutations.qnt "$module" refinesChangeLog)
  report "$module refinesChangeLog" "$(outcome "$output")" "$(broken "$output")" hold
done
exit "$status"
