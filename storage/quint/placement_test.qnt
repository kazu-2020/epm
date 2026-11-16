module placement_test {
  import placement_design.* from "./placement"
  import seqlog_design.* from "./seqlog"
  import placement(placementGuards = allPlacementGuards, scenario = baseScenario, logGuards = allGuards).* from "./placement"

  // The read that chose the old Assignee before the handover is concurrent with r2, so it may miss r2.
  run readAfterHandoverTest =
    handedOver
      .then(servedOne)
      .expect(placement.served == { epoch: 1, view: Set("r1"), mustSee: Set("r1"), otherWorkspace: false })
      .then(looksUp({ server: "B", epoch: 2 }))
      .then(servedOne)
      .expect(placement.served == { epoch: 2, view: Set("r1", "r2"), mustSee: Set("r1", "r2"), otherWorkspace: false })

  run warmHandoverTest =
    warmStarted
      .then(looksUpServing).then(servedOne)
      .expect(placement.served == { epoch: 1, view: Set("r1"), mustSee: Set("r1"), otherWorkspace: false })
      .then(sent("A", "r2")).then(onLog("A", Flush)).then(onLog("A", ApplyWrite)).then(onLog("A", ReceiveReply)).then(onLog("A", Resolve))
      .expect(acknowledged == Set("r1") and not(isReady("A")))
      .then(table(Record("B"))).then(committed("B", "r2"))
      .then(looksUpServing).then(servedOne)
      .expect(placement.served == { epoch: 2, view: Set("r1", "r2"), mustSee: Set("r1", "r2"), otherWorkspace: false })
}
