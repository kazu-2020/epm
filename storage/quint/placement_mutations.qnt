module placement_blind_claim {
  import placement_design.* from "./placement"
  import seqlog_design.* from "./seqlog"
  import placement(placementGuards = { ...allPlacementGuards, conditionalClaim: false }, logGuards = allGuards).* from "./placement"
}

// A fixed scenario, because random simulation does not reach two processes of one epoch that both load before either fence.
module placement_start_resent {
  import placement_design.* from "./placement"
  import seqlog_design.* from "./seqlog"
  import placement(placementGuards = { ...allPlacementGuards, startOnce: false, rejectsDuplicateStart: false, oneNode: true }, logGuards = allGuards).* from "./placement"

  run sameEpochOtherProcessStopsTest =
    placementInit
      .then(launched("X", "A")).then(table("spawn", "", "B"))
      .then(fencedTogether)
      .expect(isReady("A") and isStopped("B"))
      .then(table("record", "", "A")).then(committed("A", "r1"))
      .expect(acknowledged == Set("r1") and log == List("r1") and logRefinesChangeLog)
}

// A fixed scenario, because random simulation does not reach two processes of one epoch that both load before either fence.
module placement_start_resent_no_process_check {
  import placement_design.* from "./placement"
  import seqlog_design.* from "./seqlog"
  import placement(placementGuards = { ...allPlacementGuards, startOnce: false, rejectsDuplicateStart: false, oneNode: true }, logGuards = { ...allGuards, processCheck: false }).* from "./placement"

  run sameEpochServesStaleReadTest =
    placementInit
      .then(launched("X", "A")).then(table("spawn", "", "B"))
      .then(fencedTogether)
      .expect(isReady("A") and isReady("B"))
      .then(table("record", "", "A")).then(committed("A", "r1"))
      .then(table("record", "", "B"))
      .then(looksUp(placement.serving)).then(servedOne)
      .expect(placement.served.mustSee == Set("r1") and not(readSeesAcknowledgedAtLookup))
      .then(sent("B", "r2")).then(onLog("B", "flush", "")).then(onLog("B", "apply", ""))
      .then(onLog("B", "reply", "")).then(onLog("B", "resolve", ""))
      .expect(isStopped("B") and acknowledged == Set("r1") and logRefinesChangeLog)
}

module placement_no_workspace_check {
  import placement_design.* from "./placement"
  import seqlog_design.* from "./seqlog"
  import placement(placementGuards = { ...allPlacementGuards, workspaceCheck: false }, logGuards = allGuards).* from "./placement"
}

// A fixed scenario, because random simulation does not reach a write in flight to an address that a later epoch takes.
module placement_no_epoch_check {
  import placement_design.* from "./placement"
  import seqlog_design.* from "./seqlog"
  import placement(placementGuards = { ...allPlacementGuards, epochCheck: false }, logGuards = allGuards).* from "./placement"

  run staleWriteReachesNewerProcessTest =
    placementInit
      .then(started("X", "A"))
      .then(routes({ server: "A", epoch: 1 }, "r1"))
      .then(onLog("A", "stop", ""))
      .then(started("Y", "B"))
      .then(launched("X", "A")).then(fenced("A"))
      .then(delivers).then(onLog("A", "receive", "r1")).then(onLog("A", "flush", "")).then(onLog("A", "apply", "")).then(onLog("A", "reply", ""))
      .expect(acknowledged == Set("r1") and isReady("B"))
      .then(looksUp({ server: "B", epoch: 2 })).then(servedOne)
      .expect(placement.served.epoch == 2 and placement.served.mustSee == Set("r1") and not(readSeesAcknowledgedAtLookup))
}

module placement_cached_route {
  import placement_design.* from "./placement"
  import seqlog_design.* from "./seqlog"
  import placement(placementGuards = { ...allPlacementGuards, lookupPerRequest: false }, logGuards = allGuards).* from "./placement"

  run cachedRouteMissesCommitTest =
    handedOver
      .then(servedOne)
      .then(looksUp({ server: "A", epoch: 1 }))
      .then(servedOne)
      .expect(placement.served.epoch == 1 and placement.served.mustSee == Set("r1", "r2") and not(readSeesAcknowledgedAtLookup))
}

module placement_unrouted_write {
  import placement_design.* from "./placement"
  import seqlog_design.* from "./seqlog"
  import placement(placementGuards = { ...allPlacementGuards, routedWrites: false }, logGuards = allGuards).* from "./placement"

  run unroutedWriteMissesCommitTest =
    warmStarted
      .then(routes({ server: "B", epoch: 2 }, "r2")).then(delivers).then(onLog("B", "receive", "r2"))
      .then(onLog("B", "flush", "")).then(onLog("B", "apply", "")).then(onLog("B", "reply", ""))
      .then(looksUp({ server: "A", epoch: 1 })).then(servedOne)
      .expect(placement.served.epoch == 1 and placement.served.mustSee == Set("r1", "r2") and not(readSeesAcknowledgedAtLookup))
}

module placement_failover {
  import placement_design.* from "./placement"
  import seqlog_design.* from "./seqlog"
  import placement(placementGuards = { ...allPlacementGuards, maxFailovers: 2 }, logGuards = allGuards).* from "./placement"

  run reclaimedEpochOtherProcessStopsTest =
    launchedAcrossFailover
      .expect(isReady("A") and isStopped("B"))
      .then(table("failover", "", ""))
      .then(table("readRow", "X", "")).then(table("claim", "X", "A"))
      .then(table("record", "", "A")).then(committed("A", "r1"))
      .then(looksUp(placement.serving)).then(servedOne)
      .expect(placement.served == { epoch: 1, view: Set("r1"), mustSee: Set("r1"), otherWorkspace: false } and logRefinesChangeLog)

  // No guard of this design stops this read: the fence is in the store, and the row no longer shows the later epoch.
  run failoverReturnsToFencedAssigneeTest =
    placementInit
      .then(started("X", "A")).then(table("replicate", "", ""))
      .then(started("Y", "B")).then(committed("B", "r1"))
      .then(table("failover", "", ""))
      .expect(placement.serving == { server: "A", epoch: 1 } and isReady("A"))
      .then(looksUp(placement.serving)).then(servedOne)
      .expect(placement.served.epoch == 1 and placement.served.mustSee == Set("r1") and not(readSeesAcknowledgedAtLookup))
}

module placement_failover_no_process_check {
  import placement_design.* from "./placement"
  import seqlog_design.* from "./seqlog"
  import placement(placementGuards = { ...allPlacementGuards, maxFailovers: 2 }, logGuards = { ...allGuards, processCheck: false }).* from "./placement"

  run secondFailoverRecordsStaleProcessTest =
    launchedAcrossFailover
      .expect(isReady("A") and isReady("B"))
      .then(table("record", "", "B")).then(committed("B", "r1"))
      .then(table("failover", "", ""))
      .then(table("readRow", "X", "")).then(table("claim", "X", "A"))
      .then(table("record", "", "A"))
      .then(looksUp(placement.serving)).then(servedOne)
      .expect(placement.served.epoch == 1 and placement.served.mustSee == Set("r1") and not(readSeesAcknowledgedAtLookup))
}

module placement_failover_no_node_check {
  import placement_design.* from "./placement"
  import seqlog_design.* from "./seqlog"
  import placement(placementGuards = { ...allPlacementGuards, recordChecksNode: false, maxFailovers: 1 }, logGuards = { ...allGuards, processCheck: false }).* from "./placement"

  run lateRecordMissesCommitTest = lateRecordMissesCommit
}

module placement_failover_duplicate_start {
  import placement_design.* from "./placement"
  import seqlog_design.* from "./seqlog"
  import placement(placementGuards = { ...allPlacementGuards, rejectsDuplicateStart: false, oneNode: true, maxFailovers: 1 }, logGuards = { ...allGuards, processCheck: false }).* from "./placement"

  run lateRecordMissesCommitTest = lateRecordMissesCommit
}
