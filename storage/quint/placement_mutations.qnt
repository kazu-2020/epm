module placement_blind_claim {
  import placement_design.* from "./placement"
  import seqlog_design.* from "./seqlog"
  import placement(placementGuards = { ...allPlacementGuards, conditionalClaim: false }, scenario = baseScenario, logGuards = allGuards).* from "./placement"
}

module placement_start_resent {
  import placement_design.* from "./placement"
  import seqlog_design.* from "./seqlog"
  import placement(placementGuards = { ...allPlacementGuards, startOnce: false, rejectsDuplicateStart: false }, scenario = { ...baseScenario, oneNode: true }, logGuards = allGuards).* from "./placement"

  run sameEpochOtherProcessStopsTest =
    placementInit
      .then(launched("X", "A")).then(table(Spawn("B")))
      .then(fencedTogether)
      .expect(isReady("A") and isStopped("B"))
      .then(table(Record("A"))).then(committed("A", "r1"))
      .expect(acknowledged == Set("r1") and log == List("r1") and logRefinesChangeLog)
}

module placement_start_resent_no_process_check {
  import placement_design.* from "./placement"
  import seqlog_design.* from "./seqlog"
  import placement(placementGuards = { ...allPlacementGuards, startOnce: false, rejectsDuplicateStart: false }, scenario = { ...baseScenario, oneNode: true }, logGuards = { ...allGuards, processCheck: false }).* from "./placement"

  run sameEpochServesStaleReadTest =
    placementInit
      .then(launched("X", "A")).then(table(Spawn("B")))
      .then(fencedTogether)
      .expect(isReady("A") and isReady("B"))
      .then(table(Record("A"))).then(committed("A", "r1"))
      .then(table(Record("B")))
      .then(looksUpServing).then(servedOne)
      .expect(placement.served.mustSee == Set("r1") and not(readSeesAcknowledgedAtLookup))
      .then(sent("B", "r2")).then(onLog("B", Flush)).then(onLog("B", ApplyWrite))
      .then(onLog("B", ReceiveReply)).then(onLog("B", Resolve))
      .expect(isStopped("B") and acknowledged == Set("r1") and logRefinesChangeLog)
}

module placement_no_workspace_check {
  import placement_design.* from "./placement"
  import seqlog_design.* from "./seqlog"
  import placement(placementGuards = { ...allPlacementGuards, workspaceCheck: false }, scenario = baseScenario, logGuards = allGuards).* from "./placement"
}

module placement_no_epoch_check {
  import placement_design.* from "./placement"
  import seqlog_design.* from "./seqlog"
  import placement(placementGuards = { ...allPlacementGuards, epochCheck: false }, scenario = baseScenario, logGuards = allGuards).* from "./placement"

  run staleWriteReachesNewerProcessTest =
    placementInit
      .then(started("X", "A"))
      .then(routes({ server: "A", epoch: 1 }, "r1"))
      .then(onLog("A", StopProcess))
      .then(started("Y", "B"))
      .then(launched("X", "A")).then(fenced("A"))
      .then(delivers).then(onLog("A", Receive("r1"))).then(onLog("A", Flush)).then(onLog("A", ApplyWrite)).then(onLog("A", ReceiveReply))
      .expect(acknowledged == Set("r1") and isReady("B"))
      .then(looksUp({ server: "B", epoch: 2 })).then(servedOne)
      .expect(placement.served.epoch == 2 and placement.served.mustSee == Set("r1") and not(readSeesAcknowledgedAtLookup))
}

module placement_cached_route {
  import placement_design.* from "./placement"
  import seqlog_design.* from "./seqlog"
  import placement(placementGuards = { ...allPlacementGuards, lookupPerRequest: false }, scenario = baseScenario, logGuards = allGuards).* from "./placement"

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
  import placement(placementGuards = { ...allPlacementGuards, routedWrites: false }, scenario = baseScenario, logGuards = allGuards).* from "./placement"

  run unroutedWriteMissesCommitTest =
    warmStarted
      .then(routes({ server: "B", epoch: 2 }, "r2")).then(delivers).then(onLog("B", Receive("r2")))
      .then(onLog("B", Flush)).then(onLog("B", ApplyWrite)).then(onLog("B", ReceiveReply))
      .then(looksUp({ server: "A", epoch: 1 })).then(servedOne)
      .expect(placement.served.epoch == 1 and placement.served.mustSee == Set("r1", "r2") and not(readSeesAcknowledgedAtLookup))
}

module placement_failover {
  import placement_design.* from "./placement"
  import seqlog_design.* from "./seqlog"
  import placement(placementGuards = allPlacementGuards, scenario = { ...baseScenario, maxFailovers: 2 }, logGuards = allGuards).* from "./placement"

  run reclaimedEpochOtherProcessStopsTest =
    launchedAcrossFailover
      .expect(isReady("A") and isStopped("B"))
      .then(table(Failover))
      .then(table(ReadRow("X"))).then(table(Claim({ app: "X", server: "A" })))
      .then(table(Record("A"))).then(committed("A", "r1"))
      .then(looksUpServing).then(servedOne)
      .expect(placement.served == { epoch: 1, view: Set("r1"), mustSee: Set("r1"), otherWorkspace: false } and logRefinesChangeLog)

  // No guard of this design stops this read: the fence is in the store, and the row no longer shows the later epoch.
  run failoverReturnsToFencedAssigneeTest =
    placementInit
      .then(started("X", "A")).then(table(Replicate))
      .then(started("Y", "B")).then(committed("B", "r1"))
      .then(table(Failover))
      .expect(placement.serving == Some({ server: "A", epoch: 1 }) and isReady("A"))
      .then(looksUpServing).then(servedOne)
      .expect(placement.served.epoch == 1 and placement.served.mustSee == Set("r1") and not(readSeesAcknowledgedAtLookup))
}

module placement_failover_no_process_check {
  import placement_design.* from "./placement"
  import seqlog_design.* from "./seqlog"
  import placement(placementGuards = allPlacementGuards, scenario = { ...baseScenario, maxFailovers: 2 }, logGuards = { ...allGuards, processCheck: false }).* from "./placement"

  run secondFailoverRecordsStaleProcessTest =
    launchedAcrossFailover
      .expect(isReady("A") and isReady("B"))
      .then(table(Record("B"))).then(committed("B", "r1"))
      .then(table(Failover))
      .then(table(ReadRow("X"))).then(table(Claim({ app: "X", server: "A" })))
      .then(table(Record("A")))
      .then(looksUpServing).then(servedOne)
      .expect(placement.served.epoch == 1 and placement.served.mustSee == Set("r1") and not(readSeesAcknowledgedAtLookup))
}

// This module and the next also turn off processCheck: with it on, the second process of one epoch stops at the fence, and no violation shows.
module placement_failover_no_node_check {
  import placement_design.* from "./placement"
  import seqlog_design.* from "./seqlog"
  import placement(placementGuards = { ...allPlacementGuards, recordChecksNode: false }, scenario = { ...baseScenario, maxFailovers: 1 }, logGuards = { ...allGuards, processCheck: false }).* from "./placement"

  run lateRecordMissesCommitTest = lateRecordMissesCommit
}

module placement_failover_duplicate_start {
  import placement_design.* from "./placement"
  import seqlog_design.* from "./seqlog"
  import placement(placementGuards = { ...allPlacementGuards, rejectsDuplicateStart: false }, scenario = { ...baseScenario, oneNode: true, maxFailovers: 1 }, logGuards = { ...allGuards, processCheck: false }).* from "./placement"

  run lateRecordMissesCommitTest = lateRecordMissesCommit
}
