module seqlog_blind_put {
  import seqlog_design.* from "./seqlog"
  import seqlog(guards = { ...allGuards, ifNoneMatch: false }).* from "./seqlog"
}

module seqlog_unknown_as_success {
  import seqlog_design.* from "./seqlog"
  import seqlog(guards = { ...allGuards, readsBackUnknown: false }).* from "./seqlog"
}

module seqlog_no_deduplication {
  import seqlog_design.* from "./seqlog"
  import seqlog(guards = { ...allGuards, deduplication: false }).* from "./seqlog"
}

module seqlog_no_inflight_deduplication {
  import seqlog_design.* from "./seqlog"
  import seqlog(guards = { ...allGuards, inFlightDeduplication: false }).* from "./seqlog"
}

module seqlog_no_fence {
  import seqlog_design.* from "./seqlog"
  import seqlog(guards = { ...allGuards, fence: false }).* from "./seqlog"
}

module seqlog_no_epoch_check {
  import seqlog_design.* from "./seqlog"
  import seqlog(guards = { ...allGuards, epochCheck: false }).* from "./seqlog"
}

// Pipelined PUTs break the log in two ways. Slot N+1 can land before N, so a batch is acknowledged but not in the log.
// The second PUT of an old Assignee can land past the fence of the new one.
// Replies in the reverse order of the slots also make the view differ from the log, which only viewIsLogPrefix catches.
// A fixed scenario, because random simulation does not reach this acknowledgement past a hole.
module seqlog_two_in_flight {
  import seqlog_design.* from "./seqlog"
  import seqlog(guards = { ...allGuards, oneInFlight: false }).* from "./seqlog"

  run pipelineAcknowledgesPastHoleTest =
    init
      .then(all { ghost, grant("A") })
      .then(all { ghost, load("A") })
      .then(applyOne)
      .then(replyOne)
      .then(all { ghost, send("A", "r1") })
      .then(all { ghost, receive("A", "r1") })
      .then(all { ghost, flush("A") })
      .then(all { ghost, send("A", "r2") })
      .then(all { ghost, receive("A", "r2") })
      .then(all { ghost, flush("A") })
      .then(nondet write = store.writesInFlight.filter(w => w.slot == 2).oneOf() all { ghost, apply(write, false) })
      .then(replyOne)
      .expect(clients.acknowledged == Set("r2") and replay(store.slots) == List() and not(refinesChangeLog))
}

// Not a guard of safety: a server that stops at a 412 without reading only fails to acknowledge its own write.
// Reading after a 412 keeps resends from stopping the Assignee, which is liveness.
module seqlog_rejected_as_fenced {
  import seqlog_design.* from "./seqlog"
  import seqlog(guards = { ...allGuards, headsAfterRejection: false }).* from "./seqlog"

  run ownResendRejectedTest =
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
      .expect(servers.get("A").phase == Stopped and replay(store.slots) == List("r1") and clients.acknowledged == Set())
}
