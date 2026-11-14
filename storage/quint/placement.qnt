module placement_design {
  type PlacementGuards = {
    conditionalClaim: bool, startOnce: bool, workspaceCheck: bool, epochCheck: bool, lookupPerRequest: bool, routedWrites: bool,
  }
  pure val allPlacementGuards: PlacementGuards = {
    conditionalClaim: true, startOnce: true, workspaceCheck: true, epochCheck: true, lookupPerRequest: true, routedWrites: true,
  }
}

module placement {
  import change_log.* from "./change_log"
  import seqlog_design.* from "./seqlog"
  import seqlog(guards = allGuards).* from "./seqlog"
  import placement_design.*

  const placementGuards: PlacementGuards

  pure val Apps = Set("X", "Y")

  type Start = { epoch: int, by: str }
  type Route = { server: str, epoch: int }
  type RoutedWrite = { server: str, epoch: int, request: str }
  type Read = { server: str, epoch: int, mustSee: Set[str] }
  type Served = { epoch: int, view: Set[str], mustSee: Set[str], otherWorkspace: bool }

  var serving: Route
  var writes: Set[RoutedWrite]
  var seen: str -> int
  var starts: Set[Start]
  var spawned: List[int]
  var otherWorkspace: Set[str]
  var route: Route
  var reads: Set[Read]
  var served: Served
  var startLost: int
  var previousGrantedEpoch: int

  action keepPlacement = all {
    serving' = serving, writes' = writes, seen' = seen, starts' = starts, spawned' = spawned, otherWorkspace' = otherWorkspace,
    route' = route, reads' = reads, served' = served, startLost' = startLost,
  }

  action keepLog = all { store' = store, servers' = servers, grantedEpoch' = grantedEpoch, clients' = clients }

  action placementInit = all {
    init,
    serving' = { server: "", epoch: 0 }, writes' = Set(), seen' = Apps.mapBy(_ => 0), starts' = Set(), spawned' = List(), otherWorkspace' = Set(),
    route' = { server: "", epoch: 0 }, reads' = Set(),
    served' = { epoch: 0, view: Set(), mustSee: Set(), otherWorkspace: false }, startLost' = 0, previousGrantedEpoch' = 0,
  }

  action readRow(app: str): bool = all {
    seen' = seen.set(app, grantedEpoch),
    keepLog,
    serving' = serving, writes' = writes, starts' = starts, spawned' = spawned, otherWorkspace' = otherWorkspace,
    route' = route, reads' = reads, served' = served, startLost' = startLost,
  }

  // An app server can claim at any time: a failed connection, a false suspicion and a start that takes too long look the same.
  action claim(app: str): bool = {
    val epoch = seen.get(app) + 1
    all {
      epoch <= MaxEpoch,
      placementGuards.conditionalClaim implies seen.get(app) == grantedEpoch,
      grantedEpoch' = epoch,
      serving' = serving, writes' = writes,
      starts' = starts.union(Set({ epoch: epoch, by: app })),
      store' = store, servers' = servers, clients' = clients,
      seen' = seen, spawned' = spawned, otherWorkspace' = otherWorkspace,
      route' = route, reads' = reads, served' = served, startLost' = startLost,
    }
  }

  action loseStart(start: Start): bool = all {
    starts.contains(start),
    starts' = starts.exclude(Set(start)),
    startLost' = if (start.epoch > startLost) start.epoch else startLost,
    keepLog,
    serving' = serving, writes' = writes, seen' = seen, spawned' = spawned, otherWorkspace' = otherWorkspace,
    route' = route, reads' = reads, served' = served,
  }

  action spawn(start: Start, server: str): bool = all {
    starts.contains(start),
    servers.get(server).phase == Down,
    not(otherWorkspace.contains(server)),
    servers' = servers.set(server, loading(start.epoch)),
    starts' = if (placementGuards.startOnce) starts.exclude(Set(start)) else starts,
    spawned' = spawned.append(start.epoch),
    store' = store, grantedEpoch' = grantedEpoch, clients' = clients,
    serving' = serving, writes' = writes, seen' = seen, otherWorkspace' = otherWorkspace,
    route' = route, reads' = reads, served' = served, startLost' = startLost,
  }

  action record(server: str): bool = all {
    servers.get(server).phase == Ready,
    servers.get(server).epoch == grantedEpoch,
    serving' = { server: server, epoch: grantedEpoch }, writes' = writes,
    keepLog,
    seen' = seen, starts' = starts, spawned' = spawned, otherWorkspace' = otherWorkspace,
    route' = route, reads' = reads, served' = served, startLost' = startLost,
  }

  action takeAddress(server: str): bool = all {
    servers.get(server).phase == Down,
    otherWorkspace' = otherWorkspace.union(Set(server)),
    keepLog,
    serving' = serving, writes' = writes, seen' = seen, starts' = starts, spawned' = spawned,
    route' = route, reads' = reads, served' = served, startLost' = startLost,
  }

  action leaveAddress(server: str): bool = all {
    otherWorkspace.contains(server),
    otherWorkspace' = otherWorkspace.exclude(Set(server)),
    keepLog,
    serving' = serving, writes' = writes, seen' = seen, starts' = starts, spawned' = spawned,
    route' = route, reads' = reads, served' = served, startLost' = startLost,
  }

  action lookupWith(chosen: Route): bool = all {
    chosen.server != "",
    chosen == serving or (not(placementGuards.lookupPerRequest) and chosen == route),
    reads' = reads.union(Set({ server: chosen.server, epoch: chosen.epoch, mustSee: clients.acknowledged })),
    route' = chosen,
    keepLog,
    serving' = serving, writes' = writes, seen' = seen, starts' = starts, spawned' = spawned, otherWorkspace' = otherWorkspace,
    served' = served, startLost' = startLost,
  }

  action lookup: bool = {
    nondet chosen = Set(serving, route).oneOf()
    lookupWith(chosen)
  }

  action serve(read: Read): bool = {
    val self = servers.get(read.server)
    val other = otherWorkspace.contains(read.server)
    val ready = self.phase == Ready
    // Not the epoch of the process of the other workspace: it can be equal by chance, so only the workspace check stops that request.
    val accepted = (not(other) or not(placementGuards.workspaceCheck)) and (other or self.epoch == read.epoch or not(placementGuards.epochCheck))
    all {
      reads.contains(read),
      reads' = reads.exclude(Set(read)),
      served' =
        if (accepted and other) { epoch: read.epoch, view: Set(), mustSee: read.mustSee, otherWorkspace: true }
        else if (accepted and ready) { epoch: self.epoch, view: self.view.toSet(), mustSee: read.mustSee, otherWorkspace: false }
        else served,
      keepLog,
      serving' = serving, writes' = writes, seen' = seen, starts' = starts, spawned' = spawned, otherWorkspace' = otherWorkspace,
      route' = route, startLost' = startLost,
    }
  }

  // A request in flight to a process ends with the process, because the connection ends.
  action stopProcess(server: str): bool = all {
    servers.get(server).phase != Down,
    servers' = servers.set(server, down),
    clients' = { ...clients, requestsInFlight: clients.requestsInFlight.filter(message => message.server != server) },
    store' = store, grantedEpoch' = grantedEpoch,
  }

  action routeWriteTo(chosen: Route, request: str): bool = all {
    chosen.server != "",
    chosen == serving or (not(placementGuards.routedWrites) and chosen.epoch == servers.get(chosen.server).epoch),
    writes' = writes.union(Set({ server: chosen.server, epoch: chosen.epoch, request: request })),
    keepLog,
    serving' = serving, seen' = seen, starts' = starts, spawned' = spawned, otherWorkspace' = otherWorkspace,
    route' = route, reads' = reads, served' = served, startLost' = startLost,
  }

  action routeWrite(request: str): bool = {
    nondet server = Servers.oneOf()
    nondet chosen = Set(serving, { server: server, epoch: servers.get(server).epoch }).oneOf()
    routeWriteTo(chosen, request)
  }

  action deliverWrite(write: RoutedWrite): bool = {
    val self = servers.get(write.server)
    val other = otherWorkspace.contains(write.server)
    val accepted = (not(other) or not(placementGuards.workspaceCheck)) and (other or self.epoch == write.epoch or not(placementGuards.epochCheck))
    all {
      writes.contains(write),
      writes' = writes.exclude(Set(write)),
      if (accepted and self.phase != Down) send(write.server, write.request) else keepLog,
      serving' = serving, seen' = seen, starts' = starts, spawned' = spawned, otherWorkspace' = otherWorkspace,
      route' = route, reads' = reads, served' = served, startLost' = startLost,
    }
  }

  action placementGhost = all { ghost, previousGrantedEpoch' = grantedEpoch }

  action placementStep = all {
    placementGhost,
    any {
      all { keepPlacement, nondet server = Servers.oneOf() any { stopProcess(server), load(server), flush(server), timeout(server), resolve(server) } },
      all { keepPlacement, nondet server = Servers.oneOf() nondet request = Requests.oneOf() receive(server, request) },
      nondet request = Requests.oneOf() routeWrite(request),
      nondet write = writes.oneOf() deliverWrite(write),
      all { keepPlacement, nondet write = store.writesInFlight.oneOf() nondet replyLost = Set(true, false).oneOf() apply(write, replyLost) },
      all { keepPlacement, nondet reply = store.replies.oneOf() receiveReply(reply) },
      nondet app = Apps.oneOf() any { readRow(app), claim(app) },
      nondet start = starts.oneOf() nondet server = Servers.oneOf() any { loseStart(start), spawn(start, server) },
      nondet server = Servers.oneOf() any { record(server), takeAddress(server), leaveAddress(server) },
      lookup,
      nondet read = reads.oneOf() serve(read),
    },
  }

  // An instance does not export what it imports, so the properties of the change log get a name here.
  val logRefinesChangeLog: bool = refinesChangeLog
  val logSegmentsNeverChange: bool = segmentsNeverChange
  val logStaleEpochNeverCommits: bool = staleEpochNeverCommits
  val logSlotsContiguous: bool = slotsContiguous
  val logOneContentPerSlotPerEpoch: bool = oneContentPerSlotPerEpoch
  val logDataWriteNeverSeesOlderEpoch: bool = dataWriteNeverSeesOlderEpoch

  val epochStartsOneProcess: bool = spawned.indices().forall(i => spawned.indices().forall(j => i != j implies spawned[i] != spawned[j]))
  val epochNeverDecreases: bool = previousGrantedEpoch <= grantedEpoch
  val readSeesAcknowledgedAtLookup: bool = served.mustSee.subseteq(served.view) or served.otherWorkspace
  val neverReadsOtherWorkspace: bool = not(served.otherWorkspace)

  val witnessNoReadAfterLostStart: bool = not(startLost > 0 and served.epoch > startLost)
  val witnessNoAddressReuse: bool = not(reads.exists(read =>
    otherWorkspace.contains(read.server) or (servers.get(read.server).phase != Down and servers.get(read.server).epoch != read.epoch)))

  // Fixed scenarios, because random simulation does not reach a read after a handover.
  action onLog(server: str, kind: str, request: str): bool = all {
    placementGhost, keepPlacement,
    any {
      all { kind == "load", load(server) },
      all { kind == "apply", nondet write = store.writesInFlight.filter(w => w.server == server).oneOf() apply(write, false) },
      all { kind == "reply", nondet reply = store.replies.filter(r => r.write.server == server).oneOf() receiveReply(reply) },
      all { kind == "receive", receive(server, request) },
      all { kind == "flush", flush(server) },
      all { kind == "stop", stopProcess(server) },
      all { kind == "resolve", resolve(server) },
    },
  }

  action table(kind: str, app: str, server: str): bool = all {
    placementGhost,
    any {
      all { kind == "readRow", readRow(app) },
      all { kind == "claim", claim(app) },
      all { kind == "spawn", nondet start = starts.oneOf() spawn(start, server) },
      all { kind == "record", record(server) },
    },
  }
  action routes(chosen: Route, request: str): bool = all { placementGhost, routeWriteTo(chosen, request) }
  action delivers: bool = { nondet write = writes.oneOf() all { placementGhost, deliverWrite(write) } }
  action looksUp(chosen: Route): bool = all { placementGhost, lookupWith(chosen) }
  val acknowledged: Set[str] = clients.acknowledged
  val log: List[str] = replay(store.slots)
  def isReady(server: str): bool = servers.get(server).phase == Ready

  run started(app: str, server: str): bool =
    table("readRow", app, "").then(table("claim", app, "")).then(table("spawn", "", server)).then(fenced(server)).then(table("record", "", server))

  run fenced(server: str): bool =
    onLog(server, "load", "").then(onLog(server, "apply", "")).then(onLog(server, "reply", ""))

  run sent(server: str, request: str): bool = routes(serving, request).then(delivers).then(onLog(server, "receive", request))

  run committed(server: str, request: str): bool =
    sent(server, request)
      .then(onLog(server, "flush", ""))
      .then(onLog(server, "apply", ""))
      .then(onLog(server, "reply", ""))

  run servedOne = nondet read = reads.oneOf() all { placementGhost, serve(read) }

  run handedOver =
    placementInit
      .then(started("X", "A")).then(committed("A", "r1"))
      .then(looksUp({ server: "A", epoch: 1 }))
      .then(started("Y", "B")).then(committed("B", "r2"))
      .expect(acknowledged == Set("r1", "r2") and isReady("A"))

  // The read that chose the old Assignee before the handover is concurrent with r2, so it may miss r2.
  run readAfterHandoverTest =
    handedOver
      .then(servedOne)
      .expect(served == { epoch: 1, view: Set("r1"), mustSee: Set("r1"), otherWorkspace: false })
      .then(looksUp({ server: "B", epoch: 2 }))
      .then(servedOne)
      .expect(served == { epoch: 2, view: Set("r1", "r2"), mustSee: Set("r1", "r2"), otherWorkspace: false })

  run warmStarted =
    placementInit
      .then(started("X", "A")).then(committed("A", "r1"))
      .then(table("readRow", "Y", "")).then(table("claim", "Y", "")).then(table("spawn", "", "B")).then(fenced("B"))
      .expect(isReady("A") and isReady("B") and serving == { server: "A", epoch: 1 })

  run warmHandoverTest =
    warmStarted
      .then(looksUp(serving)).then(servedOne)
      .expect(served == { epoch: 1, view: Set("r1"), mustSee: Set("r1"), otherWorkspace: false })
      .then(sent("A", "r2")).then(onLog("A", "flush", "")).then(onLog("A", "apply", "")).then(onLog("A", "reply", "")).then(onLog("A", "resolve", ""))
      .expect(acknowledged == Set("r1") and not(isReady("A")))
      .then(table("record", "", "B")).then(committed("B", "r2"))
      .then(looksUp(serving)).then(servedOne)
      .expect(served == { epoch: 2, view: Set("r1", "r2"), mustSee: Set("r1", "r2"), otherWorkspace: false })
}

module placement_designed {
  import placement_design.*
  import placement(placementGuards = allPlacementGuards).*
}

module placement_blind_claim {
  import placement_design.*
  import placement(placementGuards = { ...allPlacementGuards, conditionalClaim: false }).*
}

// A fixed scenario, because random simulation does not reach the acknowledgement of the second process.
module placement_start_resent {
  import placement_design.*
  import placement(placementGuards = { ...allPlacementGuards, startOnce: false }).*

  run sameEpochAcknowledgesLostCommitTest =
    placementInit
      .then(table("readRow", "X", "")).then(table("claim", "X", ""))
      .then(table("spawn", "", "A")).then(table("spawn", "", "B"))
      .then(onLog("A", "load", "")).then(onLog("B", "load", ""))
      .then(onLog("A", "apply", "")).then(onLog("B", "apply", ""))
      .then(onLog("A", "reply", "")).then(onLog("B", "reply", "")).then(onLog("B", "resolve", ""))
      .expect(isReady("A") and isReady("B"))
      .then(table("record", "", "A")).then(sent("A", "r1")).then(onLog("A", "flush", ""))
      .then(table("record", "", "B")).then(sent("B", "r2")).then(onLog("B", "flush", ""))
      .then(onLog("A", "apply", "")).then(onLog("B", "apply", ""))
      .then(onLog("A", "reply", "")).then(onLog("B", "reply", "")).then(onLog("B", "resolve", ""))
      .expect(acknowledged == Set("r1", "r2") and log == List("r1") and not(logRefinesChangeLog))
}

module placement_no_workspace_check {
  import placement_design.*
  import placement(placementGuards = { ...allPlacementGuards, workspaceCheck: false }).*
}

// A fixed scenario, because random simulation does not reach a write in flight to an address that a later epoch takes.
module placement_no_epoch_check {
  import placement_design.*
  import placement(placementGuards = { ...allPlacementGuards, epochCheck: false }).*

  run staleWriteReachesNewerProcessTest =
    placementInit
      .then(started("X", "A"))
      .then(routes({ server: "A", epoch: 1 }, "r1"))
      .then(onLog("A", "stop", ""))
      .then(started("Y", "B"))
      .then(table("readRow", "X", "")).then(table("claim", "X", "")).then(table("spawn", "", "A")).then(fenced("A"))
      .then(delivers).then(onLog("A", "receive", "r1")).then(onLog("A", "flush", "")).then(onLog("A", "apply", "")).then(onLog("A", "reply", ""))
      .expect(acknowledged == Set("r1") and isReady("B"))
      .then(looksUp({ server: "B", epoch: 2 })).then(servedOne)
      .expect(served.epoch == 2 and served.mustSee == Set("r1") and not(readSeesAcknowledgedAtLookup))
}

module placement_cached_route {
  import placement_design.*
  import placement(placementGuards = { ...allPlacementGuards, lookupPerRequest: false }).*

  run cachedRouteMissesCommitTest =
    handedOver
      .then(servedOne)
      .then(looksUp({ server: "A", epoch: 1 }))
      .then(servedOne)
      .expect(served.epoch == 1 and served.mustSee == Set("r1", "r2") and not(readSeesAcknowledgedAtLookup))
}

module placement_unrouted_write {
  import placement_design.*
  import placement(placementGuards = { ...allPlacementGuards, routedWrites: false }).*

  run unroutedWriteMissesCommitTest =
    warmStarted
      .then(routes({ server: "B", epoch: 2 }, "r2")).then(delivers).then(onLog("B", "receive", "r2"))
      .then(onLog("B", "flush", "")).then(onLog("B", "apply", "")).then(onLog("B", "reply", ""))
      .then(looksUp({ server: "A", epoch: 1 })).then(servedOne)
      .expect(served.epoch == 1 and served.mustSee == Set("r1", "r2") and not(readSeesAcknowledgedAtLookup))
}
