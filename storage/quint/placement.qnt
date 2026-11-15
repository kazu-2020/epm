module placement_design {
  type PlacementGuards = {
    conditionalClaim: bool, startOnce: bool, workspaceCheck: bool, epochCheck: bool, lookupPerRequest: bool, routedWrites: bool,
    recordChecksNode: bool, rejectsDuplicateStart: bool,
    // Not guards: the size of the scenario. One node with every address shows what a node does with two starts of one epoch.
    oneNode: bool, maxFailovers: int,
  }
  pure val allPlacementGuards: PlacementGuards = {
    conditionalClaim: true, startOnce: true, workspaceCheck: true, epochCheck: true, lookupPerRequest: true, routedWrites: true,
    recordChecksNode: true, rejectsDuplicateStart: true,
    oneNode: false, maxFailovers: 0,
  }
}

module placement {
  import change_log.* from "./change_log"
  import seqlog_design.* from "./seqlog"
  import seqlog(guards = logGuards).* from "./seqlog"
  import placement_design.*

  const placementGuards: PlacementGuards
  const logGuards: Guards

  pure val Apps = Set("X", "Y")

  type Start = { epoch: int, by: str, node: str }
  type Option[a] = Some(a) | None
  type Replica = { epoch: int, node: Option[str], serving: Option[Route] }
  type Row = { node: Option[str], replica: Replica, failovers: int }
  type Route = { server: str, epoch: int }
  type RoutedWrite = { server: str, epoch: int, request: str }
  type Read = { server: str, epoch: int, mustSee: Set[str] }
  type Served = { epoch: int, view: Set[str], mustSee: Set[str], otherWorkspace: bool }
  type Placement = {
    serving: Option[Route], writes: Set[RoutedWrite], seen: str -> int, starts: Set[Start], spawned: List[int], otherWorkspace: Set[str],
    route: Option[Route], reads: Set[Read], served: Served, startLost: int, row: Row,
  }

  var placement: Placement
  var previousGrantedEpoch: int

  pure def present(options: Set[Option[a]]): Set[a] = options.fold(Set(), (found, option) => match option {
    | Some(value) => found.union(Set(value))
    | None => found
  })

  def nodeOf(server: str): str = if (placementGuards.oneNode) "n" else server
  def isRunning(server: str): bool = servers.get(server).phase != Down and servers.get(server).phase != Stopped

  action keepLog = all { store' = store, servers' = servers, grantedEpoch' = grantedEpoch, clients' = clients }

  action placementInit = all {
    init,
    placement' = {
      serving: None, writes: Set(), seen: Apps.mapBy(_ => 0), starts: Set(), spawned: List(), otherWorkspace: Set(),
      route: None, reads: Set(),
      served: { epoch: 0, view: Set(), mustSee: Set(), otherWorkspace: false }, startLost: 0,
      row: { node: None, replica: { epoch: 0, node: None, serving: None }, failovers: 0 },
    },
    previousGrantedEpoch' = 0,
  }

  action readRow(app: str): bool = all {
    placement' = { ...placement, seen: placement.seen.set(app, grantedEpoch) },
    keepLog,
  }

  // An app server can claim at any time: a failed connection, a false suspicion and a start that takes too long look the same.
  action claimOn(app: str, node: str): bool = {
    val epoch = placement.seen.get(app) + 1
    all {
      epoch <= MaxEpoch,
      placementGuards.conditionalClaim implies placement.seen.get(app) == grantedEpoch,
      grantedEpoch' = epoch,
      placement' = {
        ...placement,
        starts: placement.starts.union(Set({ epoch: epoch, by: app, node: node })),
        row: { ...placement.row, node: Some(node) },
      },
      store' = store, servers' = servers, clients' = clients,
    }
  }

  action claim(app: str): bool = {
    nondet server = Servers.oneOf()
    claimOn(app, nodeOf(server))
  }

  action replicate: bool = all {
    placementGuards.maxFailovers > 0,
    placement' = { ...placement, row: { ...placement.row, replica: { epoch: grantedEpoch, node: placement.row.node, serving: placement.serving } } },
    keepLog,
  }

  action failover: bool = {
    val row = placement.row
    all {
      row.failovers < placementGuards.maxFailovers,
      row.replica != { epoch: grantedEpoch, node: row.node, serving: placement.serving },
      grantedEpoch' = row.replica.epoch,
      placement' = { ...placement, serving: row.replica.serving, row: { ...row, node: row.replica.node, failovers: row.failovers + 1 } },
      store' = store, servers' = servers, clients' = clients,
    }
  }

  action loseStart(start: Start): bool = all {
    placement.starts.contains(start),
    placement' = {
      ...placement,
      starts: placement.starts.exclude(Set(start)),
      startLost: if (start.epoch > placement.startLost) start.epoch else placement.startLost,
    },
    keepLog,
  }

  action spawn(start: Start, server: str): bool = all {
    placement.starts.contains(start),
    servers.get(server).phase == Down,
    not(placement.otherWorkspace.contains(server)),
    nodeOf(server) == start.node,
    placementGuards.rejectsDuplicateStart implies not(Servers.exists(other =>
      nodeOf(other) == start.node and isRunning(other) and servers.get(other).epoch == start.epoch)),
    servers' = servers.set(server, loading(start.epoch, placement.spawned.length() + 1)),
    placement' = {
      ...placement,
      starts: if (placementGuards.startOnce) placement.starts.exclude(Set(start)) else placement.starts,
      spawned: placement.spawned.append(start.epoch),
    },
    store' = store, grantedEpoch' = grantedEpoch, clients' = clients,
  }

  action record(server: str): bool = all {
    servers.get(server).phase == Ready,
    servers.get(server).epoch == grantedEpoch,
    placementGuards.recordChecksNode implies placement.row.node == Some(nodeOf(server)),
    placement' = { ...placement, serving: Some({ server: server, epoch: grantedEpoch }) },
    keepLog,
  }

  action takeAddress(server: str): bool = all {
    servers.get(server).phase == Down,
    placement' = { ...placement, otherWorkspace: placement.otherWorkspace.union(Set(server)) },
    keepLog,
  }

  action leaveAddress(server: str): bool = all {
    placement.otherWorkspace.contains(server),
    placement' = { ...placement, otherWorkspace: placement.otherWorkspace.exclude(Set(server)) },
    keepLog,
  }

  action lookupWith(chosen: Route): bool = all {
    placement.serving == Some(chosen) or (not(placementGuards.lookupPerRequest) and placement.route == Some(chosen)),
    placement' = {
      ...placement,
      reads: placement.reads.union(Set({ server: chosen.server, epoch: chosen.epoch, mustSee: clients.acknowledged })),
      route: Some(chosen),
    },
    keepLog,
  }

  action lookup: bool = {
    nondet chosen = present(Set(placement.serving, placement.route)).oneOf()
    lookupWith(chosen)
  }

  // Not the epoch of the process of the other workspace: it can be equal by chance, so only the workspace check stops that request.
  pure def accepts(other: bool, processEpoch: int, epoch: int): bool =
    (not(other) or not(placementGuards.workspaceCheck)) and (other or processEpoch == epoch or not(placementGuards.epochCheck))

  pure def serves(self: Server, other: bool, read: Read, current: Served): Served =
    if (accepts(other, self.epoch, read.epoch) and other) { epoch: read.epoch, view: Set(), mustSee: read.mustSee, otherWorkspace: true }
    else if (accepts(other, self.epoch, read.epoch) and self.phase == Ready) { epoch: self.epoch, view: self.view.toSet(), mustSee: read.mustSee, otherWorkspace: false }
    else current

  action serve(read: Read): bool = all {
    placement.reads.contains(read),
    placement' = {
      ...placement,
      reads: placement.reads.exclude(Set(read)),
      served: serves(servers.get(read.server), placement.otherWorkspace.contains(read.server), read, placement.served),
    },
    keepLog,
  }

  // A request in flight to a process ends with the process, because the connection ends.
  action stopProcess(server: str): bool = all {
    servers.get(server).phase != Down,
    servers' = servers.set(server, down),
    clients' = { ...clients, requestsInFlight: clients.requestsInFlight.filter(message => message.server != server) },
    store' = store, grantedEpoch' = grantedEpoch,
  }

  action routeWriteTo(chosen: Route, request: str): bool = all {
    placement.serving == Some(chosen) or (not(placementGuards.routedWrites) and chosen.epoch == servers.get(chosen.server).epoch),
    placement' = { ...placement, writes: placement.writes.union(Set({ server: chosen.server, epoch: chosen.epoch, request: request })) },
    keepLog,
  }

  action routeWrite(request: str): bool = {
    nondet server = Servers.oneOf()
    nondet chosen = present(Set(placement.serving)).union(Set({ server: server, epoch: servers.get(server).epoch })).oneOf()
    routeWriteTo(chosen, request)
  }

  action deliverWrite(write: RoutedWrite): bool = {
    val self = servers.get(write.server)
    all {
      placement.writes.contains(write),
      placement' = { ...placement, writes: placement.writes.exclude(Set(write)) },
      if (accepts(placement.otherWorkspace.contains(write.server), self.epoch, write.epoch) and self.phase != Down) send(write.server, write.request)
      else keepLog,
    }
  }

  action placementGhost = all { ghost, previousGrantedEpoch' = grantedEpoch }

  // Not the steps that change no state, such as a second readRow of the same epoch: they used most of the steps of a random run.
  action placementStep = all {
    placementGhost,
    any {
      all { placement' = placement, nondet server = Servers.oneOf() any { stopProcess(server), load(server), flush(server), timeout(server), resolve(server) } },
      all { placement' = placement, nondet server = Servers.oneOf() nondet request = Requests.oneOf() receive(server, request) },
      nondet request = Requests.oneOf() routeWrite(request),
      nondet write = placement.writes.oneOf() deliverWrite(write),
      all { placement' = placement, nondet write = store.writesInFlight.oneOf() nondet replyLost = Set(true, false).oneOf() apply(write, replyLost) },
      all { placement' = placement, nondet reply = store.replies.oneOf() receiveReply(reply) },
      nondet app = Apps.oneOf() any { all { placement.seen.get(app) != grantedEpoch, readRow(app) }, claim(app) },
      nondet start = placement.starts.oneOf() nondet server = Servers.oneOf() any { loseStart(start), spawn(start, server) },
      nondet server = Servers.oneOf() any {
        record(server), all { not(placement.otherWorkspace.contains(server)), takeAddress(server) }, leaveAddress(server),
      },
      lookup,
      nondet read = placement.reads.oneOf() serve(read),
      all { placement.row.replica != { epoch: grantedEpoch, node: placement.row.node, serving: placement.serving }, replicate },
      failover,
    },
  }

  // Not placementStep: its random runs reach a read after an acknowledgement in no run of 50000.
  // This step finishes each start before the next claim, and leaves out address reuse, lost starts, crashes and failovers.
  action placementReadWriteStep = all {
    placementGhost,
    any {
      all { placement' = placement, nondet server = Servers.oneOf() any { load(server), flush(server), timeout(server), resolve(server) } },
      all { placement' = placement, nondet server = Servers.oneOf() nondet request = Requests.oneOf() receive(server, request) },
      nondet request = Requests.oneOf() routeWrite(request),
      nondet write = placement.writes.oneOf() deliverWrite(write),
      all { placement' = placement, nondet write = store.writesInFlight.oneOf() nondet replyLost = Set(true, false).oneOf() apply(write, replyLost) },
      all { placement' = placement, nondet reply = store.replies.oneOf() receiveReply(reply) },
      nondet app = Apps.oneOf() any {
        all { placement.seen.get(app) != grantedEpoch, readRow(app) },
        all { placement.starts.size() == 0, grantedEpoch == 0 or present(Set(placement.serving)).exists(route => route.epoch == grantedEpoch), claim(app) },
      },
      nondet start = placement.starts.oneOf() nondet server = Servers.oneOf() spawn(start, server),
      nondet server = Servers.oneOf() record(server),
      lookup,
      nondet read = placement.reads.oneOf() serve(read),
    },
  }

  // An instance does not export what it imports, so the properties of the change log get a name here.
  val logRefinesChangeLog: bool = refinesChangeLog
  val logSegmentsNeverChange: bool = segmentsNeverChange
  val logStaleEpochNeverCommits: bool = staleEpochNeverCommits
  val logSlotsContiguous: bool = slotsContiguous
  val logOneContentPerSlotPerEpoch: bool = oneContentPerSlotPerEpoch
  val logDataWriteNeverSeesOlderEpoch: bool = dataWriteNeverSeesOlderEpoch

  val epochStartsOneProcess: bool = {
    val spawned = placement.spawned
    spawned.indices().forall(i => spawned.indices().forall(j => i != j implies spawned[i] != spawned[j]))
  }
  val epochNeverDecreases: bool = previousGrantedEpoch <= grantedEpoch
  val readSeesAcknowledgedAtLookup: bool = placement.served.mustSee.subseteq(placement.served.view) or placement.served.otherWorkspace
  val neverReadsOtherWorkspace: bool = not(placement.served.otherWorkspace)

  val witnessReadAfterLostStart: bool = placement.startLost > 0 and placement.served.epoch > placement.startLost
  val witnessReadSeesAcknowledged: bool = placement.served.mustSee.size() > 0 and not(placement.served.otherWorkspace)
  val witnessAddressReuse: bool = placement.reads.exists(read =>
    placement.otherWorkspace.contains(read.server) or (servers.get(read.server).phase != Down and servers.get(read.server).epoch != read.epoch))

  // Steps of the fixed scenarios in placement_test.qnt and placement_mutations.qnt, because random simulation does not reach a read after a handover.
  type LogStep = Load | ApplyWrite | ReceiveReply | Receive(str) | Flush | StopProcess | Resolve
  type TableStep = ReadRow(str) | Claim({ app: str, server: str }) | Replicate | Failover | Spawn(str) | Record(str)

  action onLog(server: str, logStep: LogStep): bool = all {
    placementGhost, placement' = placement,
    match logStep {
      | Load => load(server)
      | ApplyWrite => nondet write = store.writesInFlight.filter(w => w.server == server).oneOf() apply(write, false)
      | ReceiveReply => nondet reply = store.replies.filter(r => r.write.server == server).oneOf() receiveReply(reply)
      | Receive(request) => receive(server, request)
      | Flush => flush(server)
      | StopProcess => stopProcess(server)
      | Resolve => resolve(server)
    },
  }

  action table(tableStep: TableStep): bool = all {
    placementGhost,
    match tableStep {
      | ReadRow(app) => readRow(app)
      | Claim(target) => claimOn(target.app, nodeOf(target.server))
      | Replicate => replicate
      | Failover => failover
      | Spawn(server) => nondet start = placement.starts.oneOf() spawn(start, server)
      | Record(server) => record(server)
    },
  }
  action routes(chosen: Route, request: str): bool = all { placementGhost, routeWriteTo(chosen, request) }
  action delivers: bool = { nondet write = placement.writes.oneOf() all { placementGhost, deliverWrite(write) } }
  action looksUp(chosen: Route): bool = all { placementGhost, lookupWith(chosen) }
  action routesToServing(request: str): bool = nondet chosen = present(Set(placement.serving)).oneOf() routes(chosen, request)
  action looksUpServing: bool = nondet chosen = present(Set(placement.serving)).oneOf() looksUp(chosen)
  val acknowledged: Set[str] = clients.acknowledged
  val log: List[str] = replay(store.slots)
  def isReady(server: str): bool = servers.get(server).phase == Ready
  def isStopped(server: str): bool = servers.get(server).phase == Stopped

  run launched(app: str, server: str): bool =
    table(ReadRow(app)).then(table(Claim({ app: app, server: server }))).then(table(Spawn(server)))

  run started(app: str, server: str): bool =
    launched(app, server).then(fenced(server)).then(table(Record(server)))

  run fenced(server: str): bool =
    onLog(server, Load).then(onLog(server, ApplyWrite)).then(onLog(server, ReceiveReply))

  run fencedTogether: bool =
    onLog("A", Load).then(onLog("B", Load))
      .then(onLog("A", ApplyWrite)).then(onLog("B", ApplyWrite))
      .then(onLog("A", ReceiveReply)).then(onLog("B", ReceiveReply)).then(onLog("B", Resolve))

  run launchedAcrossFailover: bool =
    placementInit
      .then(launched("X", "A"))
      .then(table(Failover))
      .then(launched("Y", "B"))
      .then(fencedTogether)

  run sent(server: str, request: str): bool = routesToServing(request).then(delivers).then(onLog(server, Receive(request)))

  run committed(server: str, request: str): bool =
    sent(server, request)
      .then(onLog(server, Flush))
      .then(onLog(server, ApplyWrite))
      .then(onLog(server, ReceiveReply))

  run servedOne = nondet read = placement.reads.oneOf() all { placementGhost, serve(read) }

  run handedOver =
    placementInit
      .then(started("X", "A")).then(committed("A", "r1"))
      .then(looksUp({ server: "A", epoch: 1 }))
      .then(started("Y", "B")).then(committed("B", "r2"))
      .expect(acknowledged == Set("r1", "r2") and isReady("A"))

  run warmStarted =
    placementInit
      .then(started("X", "A")).then(committed("A", "r1"))
      .then(launched("Y", "B")).then(fenced("B"))
      .expect(isReady("A") and isReady("B") and placement.serving == Some({ server: "A", epoch: 1 }))

  run lateRecordMissesCommit: bool =
    launchedAcrossFailover
      .then(table(Record("B"))).then(committed("B", "r1"))
      .then(table(Record("A")))
      .then(looksUpServing).then(servedOne)
      .expect(placement.served.epoch == 1 and placement.served.mustSee == Set("r1") and not(readSeesAcknowledgedAtLookup))
}

module placement_designed {
  import placement_design.*
  import seqlog_design.* from "./seqlog"
  import placement(placementGuards = allPlacementGuards, logGuards = allGuards).*
}
