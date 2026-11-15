#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")"
samples="${1:-50000}"
invariants=(refinesChangeLog segmentsNeverChange staleEpochNeverCommits slotsContiguous newestAssigneeSeesAcknowledged oneContentPerSlotPerEpoch dataWriteNeverSeesOlderEpoch viewIsLogPrefix)
witnesses=(witnessTwoAcknowledged witnessCommitAfterHandover witnessOwnResendRejected witnessOldAssigneeStopped)
breaks=(
  seqlog_blind_put:refinesChangeLog seqlog_unknown_as_success:refinesChangeLog seqlog_no_deduplication:refinesChangeLog
  seqlog_no_inflight_deduplication:refinesChangeLog
  seqlog_no_fence:newestAssigneeSeesAcknowledged
  seqlog_blind_put:segmentsNeverChange
  seqlog_no_epoch_check:staleEpochNeverCommits
  seqlog_two_in_flight:staleEpochNeverCommits seqlog_two_in_flight:slotsContiguous
  seqlog_blind_put:viewIsLogPrefix seqlog_unknown_as_success:viewIsLogPrefix seqlog_two_in_flight:viewIsLogPrefix
)
holds=(seqlog_rejected_as_fenced)
log_safe_without_guard=(seqlog_no_fence seqlog_no_epoch_check)

lists_every_module() {
  local listed defined
  listed=$(printf '%s\n' "${@:2}" | sort -u)
  defined=$(sed -n 's/^module \([a-z0-9_]*\).*/\1/p' "$1" | sort)
  if [[ "$listed" != "$defined" ]]; then
    echo "check.sh does not list the same modules as $1:"
    diff <(echo "$listed") <(echo "$defined") || true
    exit 1
  fi
}

lists_every_module seqlog_mutations.qnt "${breaks[@]%%:*}" "${holds[@]}"

quint typecheck change_log.qnt
quint typecheck seqlog.qnt
quint typecheck seqlog_mutations.qnt
quint typecheck seqlog_replay.qnt
quint typecheck seqlog_test.qnt
quint test seqlog_test.qnt --main seqlog_test
quint test seqlog_mutations.qnt --main seqlog_two_in_flight --match pipelineAcknowledgesPastHoleTest
quint test seqlog_mutations.qnt --main seqlog_rejected_as_fenced --match ownResendRejectedTest

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

missed() {
  local witness
  for witness in "${@:2}"; do
    grep -q "^$witness was witnessed in [1-9]" <<<"$1" || echo "$witness"
  done | paste -sd, -
}

witnessed() {
  if [[ -z "$(missed "$@")" ]]; then echo reach; else echo miss; fi
}

outcome() {
  if grep -q '^\[violation\]' <<<"$1"; then echo break
  elif grep -q '^\[ok\]' <<<"$1"; then echo hold
  else echo error; fi
}

output=$(run seqlog.qnt seqlog_designed "${invariants[@]}" --witnesses "${witnesses[@]}")
report seqlog_designed "$(outcome "$output")" "$(broken "$output")" hold
report "seqlog_designed witnesses" "$(witnessed "$output" "${witnesses[@]}")" "$(missed "$output" "${witnesses[@]}")" reach
for entry in "${breaks[@]}"; do
  module="${entry%%:*}"
  invariant="${entry##*:}"
  report "$module $invariant" "$(outcome "$(run seqlog_mutations.qnt "$module" "$invariant")")" "" break
done
for module in "${holds[@]}"; do
  output=$(run seqlog_mutations.qnt "$module" "${invariants[@]}")
  report "$module" "$(outcome "$output")" "$(broken "$output")" hold
done
for module in "${log_safe_without_guard[@]}"; do
  output=$(run seqlog_mutations.qnt "$module" refinesChangeLog)
  report "$module refinesChangeLog" "$(outcome "$output")" "$(broken "$output")" hold
done
placement_invariants=(logRefinesChangeLog logSegmentsNeverChange logStaleEpochNeverCommits logSlotsContiguous logOneContentPerSlotPerEpoch logDataWriteNeverSeesOlderEpoch epochStartsOneProcess epochNeverDecreases readSeesAcknowledgedAtLookup neverReadsOtherWorkspace)
placement_witnesses=(witnessReadAfterLostStart witnessAddressReuse)
placement_breaks=(
  placement_blind_claim:epochStartsOneProcess placement_blind_claim:epochNeverDecreases
  placement_start_resent:epochStartsOneProcess
  placement_no_workspace_check:neverReadsOtherWorkspace
)
# Random simulation does not reach these violations, so a fixed scenario shows each one.
placement_scenarios=(
  placement_start_resent:sameEpochOtherProcessStopsTest
  placement_start_resent_no_process_check:sameEpochServesStaleReadTest
  placement_no_epoch_check:staleWriteReachesNewerProcessTest
  placement_cached_route:cachedRouteMissesCommitTest
  placement_unrouted_write:unroutedWriteMissesCommitTest
  placement_failover:reclaimedEpochOtherProcessStopsTest
  placement_failover:failoverReturnsToFencedAssigneeTest
  placement_failover_no_process_check:secondFailoverRecordsStaleProcessTest
  placement_failover_no_node_check:lateRecordMissesCommitTest
  placement_failover_duplicate_start:lateRecordMissesCommitTest
)
# A failover makes the epoch decrease and returns the row to a fenced Assignee, so only the properties of the log are checked.
placement_failover_invariants=(logRefinesChangeLog logSegmentsNeverChange logStaleEpochNeverCommits logSlotsContiguous logOneContentPerSlotPerEpoch logDataWriteNeverSeesOlderEpoch neverReadsOtherWorkspace)

lists_every_module placement_mutations.qnt "${placement_breaks[@]%%:*}" "${placement_scenarios[@]%%:*}"

quint typecheck placement.qnt
quint typecheck placement_mutations.qnt
quint typecheck placement_test.qnt
quint test placement_test.qnt --main placement_test
for entry in "${placement_scenarios[@]}"; do
  quint test placement_mutations.qnt --main "${entry%%:*}" --match "${entry##*:}"
done

run_placement() {
  quint run "$1" --main "$2" --init placementInit --step placementStep --invariants "${@:3}" --max-samples "$samples" --max-steps 40 --seed 1 2>&1 || true
}

output=$(run_placement placement.qnt placement_designed "${placement_invariants[@]}" --witnesses "${placement_witnesses[@]}")
report placement_designed "$(outcome "$output")" "$(broken "$output")" hold
report "placement_designed witnesses" "$(witnessed "$output" "${placement_witnesses[@]}")" "$(missed "$output" "${placement_witnesses[@]}")" reach
output=$(run_placement placement_mutations.qnt placement_failover "${placement_failover_invariants[@]}")
report placement_failover "$(outcome "$output")" "$(broken "$output")" hold
for entry in "${placement_breaks[@]}"; do
  report "${entry%%:*} ${entry##*:}" "$(outcome "$(run_placement placement_mutations.qnt "${entry%%:*}" "${entry##*:}")")" "" break
done
exit "$status"
