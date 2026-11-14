module placement_design {
  type PlacementGuards = {
    conditionalClaim: bool, startOnce: bool, identityCheck: bool, readyBeforeRead: bool, lookupPerRequest: bool,
  }
  pure val allPlacementGuards: PlacementGuards = {
    conditionalClaim: true, startOnce: true, identityCheck: true, readyBeforeRead: true, lookupPerRequest: true,
  }
}

// The placement table, the app servers and the launcher, on top of the change log of seqlog.
// The row of the workspace is (grantedEpoch, address). A server name is an address on a node.
module placement {
  import change_log.* from "./change_log"
  import seqlog_design.* from "./seqlog"
  import seqlog(guards = allGuards).* from "./seqlog"
  import placement_design.*

  const placementGuards: PlacementGuards

  pure val Apps = Set("X", "Y")

  type Start = { epoch: int, by: str }
  type Route = { server: str, epoch: int }
  type Read = { server: str, epoch: int, mustSee: Set[str] }
  type Served = { epoch: int, view: Set[str], mustSee: Set[str], otherWorkspace: bool }

  var address: str
  var seen: str -> int
  var starts: Set[Start]
  var spawned: List[int]
  var otherWorkspace: Set[str]
  var route: Route
  var reads: Set[Read]
  var served: Served
  var startLost: bool

  action keepPlacement = all {
    address' = address, seen' = seen, starts' = starts, spawned' = spawned, otherWorkspace' = otherWorkspace,
    route' = route, reads' = reads, served' = served, startLost' = startLost,
  }

  action keepLog = all { store' = store, servers' = servers, grantedEpoch' = grantedEpoch, clients' = clients }

  action placementInit = all {
    init,
    address' = "", seen' = Apps.mapBy(_ => 0), starts' = Set(), spawned' = List(), otherWorkspace' = Set(),
    route' = { server: "", epoch: 0 }, reads' = Set(),
    served' = { epoch: 0, view: Set(), mustSee: Set(), otherWorkspace: false }, startLost' = false,
  }

  action readRow(app: str): bool = all {
    seen' = seen.set(app, grantedEpoch),
    keepLog,
    address' = address, starts' = starts, spawned' = spawned, otherWorkspace' = otherWorkspace,
    route' = route, reads' = reads, served' = served, startLost' = startLost,
  }

  // An app server can claim at any time: a failed connection, a false suspicion and a start that takes too long look the same.
  action claim(app: str): bool = {
    val epoch = seen.get(app) + 1
    all {
      epoch <= MaxEpoch,
      placementGuards.conditionalClaim implies seen.get(app) == grantedEpoch,
      grantedEpoch' = epoch,
      address' = "",
      starts' = starts.union(Set({ epoch: epoch, by: app })),
      store' = store, servers' = servers, clients' = clients,
      seen' = seen, spawned' = spawned, otherWorkspace' = otherWorkspace,
      route' = route, reads' = reads, served' = served, startLost' = startLost,
    }
  }

  // The app server stopped after the claim, or the launcher did not get the request.
  action loseStart(start: Start): bool = all {
    starts.contains(start),
    starts' = starts.exclude(Set(start)),
    startLost' = true,
    keepLog,
    address' = address, seen' = seen, spawned' = spawned, otherWorkspace' = otherWorkspace,
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
    address' = address, seen' = seen, otherWorkspace' = otherWorkspace,
    route' = route, reads' = reads, served' = served, startLost' = startLost,
  }

  action record(server: str): bool = all {
    servers.get(server).phase != Down,
    servers.get(server).epoch == grantedEpoch,
    address' = server,
    keepLog,
    seen' = seen, starts' = starts, spawned' = spawned, otherWorkspace' = otherWorkspace,
    route' = route, reads' = reads, served' = served, startLost' = startLost,
  }

  action takeAddress(server: str): bool = all {
    servers.get(server).phase == Down,
    otherWorkspace' = otherWorkspace.union(Set(server)),
    keepLog,
    address' = address, seen' = seen, starts' = starts, spawned' = spawned,
    route' = route, reads' = reads, served' = served, startLost' = startLost,
  }

  action leaveAddress(server: str): bool = all {
    otherWorkspace.contains(server),
    otherWorkspace' = otherWorkspace.exclude(Set(server)),
    keepLog,
    address' = address, seen' = seen, starts' = starts, spawned' = spawned,
    route' = route, reads' = reads, served' = served, startLost' = startLost,
  }

  // mustSee is the set of acknowledged commits when the app server chose the route.
  action lookupWith(chosen: Route): bool = all {
    chosen.server != "",
    chosen == { server: address, epoch: grantedEpoch } or (not(placementGuards.lookupPerRequest) and chosen == route),
    reads' = reads.union(Set({ server: chosen.server, epoch: chosen.epoch, mustSee: clients.acknowledged })),
    route' = chosen,
    keepLog,
    address' = address, seen' = seen, starts' = starts, spawned' = spawned, otherWorkspace' = otherWorkspace,
    served' = served, startLost' = startLost,
  }

  action lookup: bool = {
    nondet chosen = Set({ server: address, epoch: grantedEpoch }, route).oneOf()
    lookupWith(chosen)
  }

  action serve(read: Read): bool = {
    val self = servers.get(read.server)
    val other = otherWorkspace.contains(read.server)
    val ready = if (placementGuards.readyBeforeRead) self.phase == Ready
      else self.phase == Ready or self.phase == Fencing or self.phase == Loading
    val accepted = if (placementGuards.identityCheck) not(other) and self.epoch == read.epoch else true
    all {
      reads.contains(read),
      reads' = reads.exclude(Set(read)),
      served' =
        if (accepted and other) { epoch: read.epoch, view: Set(), mustSee: read.mustSee, otherWorkspace: true }
        else if (accepted and ready) { epoch: self.epoch, view: self.view.toSet(), mustSee: read.mustSee, otherWorkspace: false }
        else served,
      keepLog,
      address' = address, seen' = seen, starts' = starts, spawned' = spawned, otherWorkspace' = otherWorkspace,
      route' = route, startLost' = startLost,
    }
  }

  action placementStep = all {
    ghost,
    any {
      all { keepPlacement, nondet server = Servers.oneOf() any { crash(server), load(server), flush(server), timeout(server), resolve(server) } },
      all { keepPlacement, nondet server = Servers.oneOf() nondet request = Requests.oneOf() any { send(server, request), receive(server, request) } },
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
  val epochNeverDecreases: bool = seen.keys().forall(app => seen.get(app) <= grantedEpoch)
  val readSeesAcknowledgedAtLookup: bool = served.mustSee.subseteq(served.view) or served.otherWorkspace
  val neverReadsOtherWorkspace: bool = not(served.otherWorkspace)

  val witnessNoReadAfterLostStart: bool = not(startLost and served.epoch >= 2)
  val witnessNoAddressReuse: bool = not(reads.exists(read =>
    otherWorkspace.contains(read.server) or (servers.get(read.server).phase != Down and servers.get(read.server).epoch != read.epoch)))

  // Fixed scenarios, because random simulation does not reach a read after a handover.
  action onLog(server: str, kind: str, request: str): bool = all {
    ghost, keepPlacement,
    any {
      all { kind == "load", load(server) },
      all { kind == "apply", nondet write = store.writesInFlight.filter(w => w.server == server).oneOf() apply(write, false) },
      all { kind == "reply", nondet reply = store.replies.filter(r => r.write.server == server).oneOf() receiveReply(reply) },
      all { kind == "send", send(server, request) },
      all { kind == "receive", receive(server, request) },
      all { kind == "flush", flush(server) },
      all { kind == "resolve", resolve(server) },
    },
  }

  action table(kind: str, app: str, server: str): bool = all {
    ghost,
    any {
      all { kind == "readRow", readRow(app) },
      all { kind == "claim", claim(app) },
      all { kind == "spawn", nondet start = starts.oneOf() spawn(start, server) },
      all { kind == "record", record(server) },
    },
  }
  action looksUp(chosen: Route): bool = all { ghost, lookupWith(chosen) }
  val acknowledged: Set[str] = clients.acknowledged
  val log: List[str] = replay(store.slots)
  def isReady(server: str): bool = servers.get(server).phase == Ready

  run started(app: str, server: str): bool =
    table("readRow", app, "").then(table("claim", app, "")).then(table("spawn", "", server)).then(table("record", "", server))

  run fenced(server: str): bool =
    onLog(server, "load", "").then(onLog(server, "apply", "")).then(onLog(server, "reply", ""))

  run committed(server: str, request: str): bool =
    onLog(server, "send", request)
      .then(onLog(server, "receive", request))
      .then(onLog(server, "flush", ""))
      .then(onLog(server, "apply", ""))
      .then(onLog(server, "reply", ""))

  run servedOne = nondet read = reads.oneOf() all { ghost, serve(read) }

  run handedOver =
    placementInit
      .then(started("X", "A")).then(fenced("A")).then(committed("A", "r1"))
      .then(looksUp({ server: "A", epoch: 1 }))
      .then(started("Y", "B")).then(fenced("B")).then(committed("B", "r2"))
      .expect(acknowledged == Set("r1", "r2") and isReady("A"))

  // The read that chose the old Assignee before the handover is concurrent with r2, so it may miss r2.
  run readAfterHandoverTest =
    handedOver
      .then(servedOne)
      .expect(served == { epoch: 1, view: Set("r1"), mustSee: Set("r1"), otherWorkspace: false })
      .then(looksUp({ server: "B", epoch: 2 }))
      .then(servedOne)
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
      .then(onLog("A", "send", "r1")).then(onLog("A", "receive", "r1")).then(onLog("A", "flush", ""))
      .then(onLog("B", "send", "r2")).then(onLog("B", "receive", "r2")).then(onLog("B", "flush", ""))
      .then(onLog("A", "apply", "")).then(onLog("B", "apply", ""))
      .then(onLog("A", "reply", "")).then(onLog("B", "reply", "")).then(onLog("B", "resolve", ""))
      .expect(acknowledged == Set("r1", "r2") and log == List("r1") and not(logRefinesChangeLog))
}

module placement_no_identity_check {
  import placement_design.*
  import placement(placementGuards = { ...allPlacementGuards, identityCheck: false }).*
}

module placement_read_while_loading {
  import placement_design.*
  import placement(placementGuards = { ...allPlacementGuards, readyBeforeRead: false }).*

  run readWhileLoadingMissesCommitTest =
    placementInit
      .then(started("X", "A")).then(fenced("A")).then(committed("A", "r1"))
      .then(started("Y", "B"))
      .then(looksUp({ server: "B", epoch: 2 }))
      .then(servedOne)
      .expect(served.mustSee == Set("r1") and served.view == Set() and not(readSeesAcknowledgedAtLookup))
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
