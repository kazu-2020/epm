module seqlog_test {
  import seqlog_design.* from "./seqlog"
  import seqlog(guards = allGuards).* from "./seqlog"

  // Also catches a refence that skips reading the slot: 50000 random runs did not reach it.
  run oldAssigneeTakesSlotFirstTest =
    init
      .then(all { ghost, grant("A") })
      .then(all { ghost, load("A") })
      .then(applyOne)
      .then(replyOne)
      .then(all { ghost, send("A", "r1") })
      .then(all { ghost, receive("A", "r1") })
      .then(all { ghost, flush("A") })
      .then(all { ghost, grant("B") })
      .then(all { ghost, load("B") })
      .expect(servers.get("B").pending.exists(p => p.write.slot == 1))
      .then(nondet write = store.writesInFlight.filter(w => w.server == "A").oneOf() all { ghost, apply(write, false) })
      .then(nondet write = store.writesInFlight.filter(w => w.server == "B").oneOf() all { ghost, apply(write, false) })
      .then(nondet reply = store.replies.filter(r => r.write.server == "A").oneOf() all { ghost, receiveReply(reply) })
      .then(nondet reply = store.replies.filter(r => r.write.server == "B").oneOf() all { ghost, receiveReply(reply) })
      .then(all { ghost, resolve("B") })
      .then(applyOne)
      .then(replyOne)
      .expect(and {
        servers.get("B").phase == Ready,
        servers.get("B").view == List("r1"),
        clients.acknowledged == Set("r1"),
        replay(store.slots) == List("r1"),
        store.slots.get(2) == { epoch: 2, process: 2, requests: List() },
        refinesChangeLog, newestAssigneeSeesAcknowledged, staleEpochNeverCommits,
      })

  run ownResendAcknowledgedTest =
    init
      .then(all { ghost, grant("A") })
      .then(all { ghost, load("A") })
      .then(applyOne)
      .then(replyOne)
      .then(all { ghost, send("A", "r1") })
      .then(all { ghost, receive("A", "r1") })
      .then(all { ghost, flush("A") })
      .then(all { ghost, timeout("A") })
      .then(all { ghost, resolve("A") })
      .then(nondet write = store.writesInFlight.filter(w => w.attempt == 0).oneOf() all { ghost, apply(write, true) })
      .then(nondet write = store.writesInFlight.filter(w => w.attempt == 1).oneOf() all { ghost, apply(write, false) })
      .then(replyOne)
      .then(all { ghost, resolve("A") })
      .expect(servers.get("A").phase == Ready and clients.acknowledged == Set("r1") and refinesChangeLog)
}
