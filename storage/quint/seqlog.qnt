module seqlog_design {
  type Guards = {
    ifNoneMatch: bool, readsBackUnknown: bool, deduplication: bool, inFlightDeduplication: bool, fence: bool,
    epochCheck: bool, oneInFlight: bool, headsAfterRejection: bool, processCheck: bool,
  }
  pure val allGuards: Guards = {
    ifNoneMatch: true, readsBackUnknown: true, deduplication: true, inFlightDeduplication: true, fence: true,
    epochCheck: true, oneInFlight: true, headsAfterRejection: true, processCheck: true,
  }
}

// The guards and the reasons for them are in docs/adr/0006-change-log-writes-are-conditional-and-read-back.md.
module seqlog {
  import change_log.* from "./change_log"
  import seqlog_design.*

  const guards: Guards

  pure val Servers = Set("A", "B")
  pure val Requests = Set("r1", "r2")
  pure val MaxEpoch = 3
  pure val MaxResends = 1
  pure val MaxInFlight = 2

  // The epoch is at the start of the segment body, not in the key: the slot number alone is what two assignees fight for.
  // Not the epoch alone in a fence: after a failover of the database, two processes can get the same epoch.
  type Slot = { epoch: int, process: int, requests: List[str] }
  type Write = { server: str, slot: int, epoch: int, process: int, requests: List[str], attempt: int }
  type Reply = { write: Write, succeeded: bool }
  type Pending = { write: Write, unknown: bool }
  type Phase = Down | Loading | Fencing | Ready | Stopped
  type Server = { phase: Phase, epoch: int, process: int, next: int, view: List[str], batch: List[str], pending: Set[Pending] }
  type Store = { slots: int -> Slot, writesInFlight: Set[Write], replies: Set[Reply] }
  type Clients = { requestsInFlight: Set[{ server: str, request: str }], acknowledged: Set[str] }

  pure val emptyStore: Store = { slots: Map(), writesInFlight: Set(), replies: Set() }
  pure val down: Server = { phase: Down, epoch: 0, process: 0, next: 0, view: List(), batch: List(), pending: Set() }
  pure def loading(epoch: int, process: int): Server = { ...down, phase: Loading, epoch: epoch, process: process }
  pure def isFence(write: Write): bool = write.requests.length() == 0
  pure def slotOf(write: Write): Slot = { epoch: write.epoch, process: write.process, requests: write.requests }

  var store: Store
  var servers: str -> Server
  var grantedEpoch: int
  var clients: Clients
  var previous: { slots: int -> Slot, acknowledged: Set[str] }

  pure def replay(slots: int -> Slot): List[str] =
    range(0, slots.keys().size()).foldl({ commits: List(), open: true }, (state, i) =>
      if (state.open and slots.keys().contains(i)) { commits: state.commits.concat(slots.get(i).requests), open: true }
      else { ...state, open: false }).commits

  pure def lastSlot(slots: int -> Slot): int = slots.keys().fold(-1, (highest, i) => if (i > highest) i else highest)

  type Outcome = { self: Server, acknowledged: Set[str], sent: Set[Write] }

  pure def keeps(self: Server): Outcome = { self: self, acknowledged: Set(), sent: Set() }

  pure def stops(self: Server): Outcome = keeps({ ...self, phase: Stopped, batch: List(), pending: Set() })

  pure def succeeds(self: Server, write: Write): Outcome = {
    self: {
      ...self,
      phase: if (isFence(write)) Ready else self.phase,
      view: self.view.concat(write.requests),
      pending: self.pending.filter(p => p.write != write),
    },
    acknowledged: write.requests.toSet(),
    sent: Set(),
  }

  pure def replaces(self: Server, write: Write, after: Pending): Server =
    { ...self, pending: self.pending.filter(p => p.write != write).union(Set(after)) }

  pure def isStale(self: Server, slots: int -> Slot): bool =
    guards.epochCheck and lastSlot(slots) >= 0 and slots.get(lastSlot(slots)).epoch >= self.epoch

  // Not a LIST followed by a read of the last segment: one step is enough, because a write between them ends in the 412 of the fence.
  pure def loads(server: str, self: Server, slots: int -> Slot): Outcome = {
    val free = lastSlot(slots) + 1
    val fence = { server: server, slot: free, epoch: self.epoch, process: self.process, requests: List(), attempt: 0 }
    val loaded = { ...self, next: free + 1, view: replay(slots) }
    if (isStale(self, slots)) stops(self)
    else if (guards.fence) { self: { ...loaded, phase: Fencing, pending: Set({ write: fence, unknown: false }) }, acknowledged: Set(), sent: Set(fence) }
    else keeps({ ...loaded, phase: Ready, next: free })
  }

  pure def receives(self: Server, request: str): Outcome = {
    val inFlight = if (guards.inFlightDeduplication) self.pending.map(p => p.write.requests.toSet()).flatten() else Set()
    if (guards.deduplication and self.view.toSet().contains(request)) { ...keeps(self), acknowledged: Set(request) }
    else if (self.batch.toSet().union(inFlight).contains(request)) keeps(self)
    else keeps({ ...self, batch: self.batch.append(request) })
  }

  // A 412 is not proof of a fence: the slot can hold this server's own write, from a resend or a late apply.
  pure def receivesReply(self: Server, reply: Reply): Outcome =
    if (reply.succeeded) succeeds(self, reply.write)
    else if (guards.headsAfterRejection) keeps(replaces(self, reply.write, { write: reply.write, unknown: true }))
    else stops(self)

  // Also the path of a 409 ConditionalRequestConflict: S3 returns it for concurrent conditional writes, and the server treats it as an unknown result.
  pure def timesOut(self: Server, p: Pending): Outcome =
    if (guards.readsBackUnknown) keeps(replaces(self, p.write, { ...p, unknown: true })) else succeeds(self, p.write)

  pure def isOwn(found: Slot, write: Write): bool =
    if (guards.processCheck) found == slotOf(write) else found.epoch == write.epoch and found.requests == write.requests

  pure def resolves(self: Server, p: Pending, slots: int -> Slot): Outcome = {
    val write = p.write
    if (not(slots.keys().contains(write.slot))) {
      val again = { ...write, attempt: write.attempt + 1 }
      if (write.attempt < MaxResends) { ...keeps(replaces(self, write, { write: again, unknown: false })), sent: Set(again) }
      else keeps(replaces(self, write, { write: write, unknown: false }))
    } else {
      val found = slots.get(write.slot)
      val refence = { ...write, slot: write.slot + 1, attempt: 0 }
      if (isOwn(found, write)) succeeds(self, write)
      else if (found.epoch < write.epoch and isFence(write)) {
        self: { ...self, next: refence.slot + 1, view: self.view.concat(found.requests), pending: Set({ write: refence, unknown: false }) },
        acknowledged: Set(),
        sent: Set(refence),
      }
      else stops(self)
    }
  }

  pure def applies(store: Store, write: Write, replyLost: bool): Store = {
    val succeeded = not(guards.ifNoneMatch) or not(store.slots.keys().contains(write.slot))
    {
      slots: if (succeeded) store.slots.put(write.slot, slotOf(write)) else store.slots,
      writesInFlight: store.writesInFlight.exclude(Set(write)),
      replies: if (replyLost) store.replies else store.replies.union(Set({ write: write, succeeded: succeeded })),
    }
  }

  action init = all {
    store' = emptyStore,
    servers' = Servers.mapBy(_ => down),
    grantedEpoch' = 0,
    clients' = { requestsInFlight: Set(), acknowledged: Set() },
    previous' = { slots: Map(), acknowledged: Set() },
  }

  // Not a stop of the other server: an old Assignee keeps running until it learns of the new epoch.
  action grant(server: str): bool = all {
    grantedEpoch < MaxEpoch,
    grantedEpoch' = grantedEpoch + 1,
    servers' = servers.set(server, loading(grantedEpoch + 1, grantedEpoch + 1)),
    store' = store, clients' = clients,
  }

  action crash(server: str): bool = all {
    servers.get(server).phase != Down,
    servers' = servers.set(server, down),
    store' = store, grantedEpoch' = grantedEpoch, clients' = clients,
  }

  action become(server: str, outcome: Outcome): bool = all {
    servers' = servers.set(server, outcome.self),
    clients' = { ...clients, acknowledged: clients.acknowledged.union(outcome.acknowledged) },
    store' = { ...store, writesInFlight: store.writesInFlight.union(outcome.sent) },
  }

  action load(server: str): bool = all {
    servers.get(server).phase == Loading,
    become(server, loads(server, servers.get(server), store.slots)),
    grantedEpoch' = grantedEpoch,
  }

  // A send is always allowed, not only before the acknowledgement, because the acknowledgement can be lost.
  action send(server: str, request: str): bool = all {
    clients' = { ...clients, requestsInFlight: clients.requestsInFlight.union(Set({ server: server, request: request })) },
    store' = store, servers' = servers, grantedEpoch' = grantedEpoch,
  }

  action receive(server: str, request: str): bool = {
    val message = { server: server, request: request }
    val outcome = receives(servers.get(server), request)
    all {
      clients.requestsInFlight.contains(message),
      servers.get(server).phase == Ready,
      servers' = servers.set(server, outcome.self),
      clients' = { requestsInFlight: clients.requestsInFlight.exclude(Set(message)), acknowledged: clients.acknowledged.union(outcome.acknowledged) },
      store' = store, grantedEpoch' = grantedEpoch,
    }
  }

  action flush(server: str): bool = {
    val self = servers.get(server)
    val write = { server: server, slot: self.next, epoch: self.epoch, process: 0, requests: self.batch, attempt: 0 }
    all {
      self.phase == Ready,
      self.batch.length() > 0,
      if (guards.oneInFlight) self.pending.size() == 0 else self.pending.size() < MaxInFlight,
      servers' = servers.set(server, { ...self, next: self.next + 1, batch: List(), pending: self.pending.union(Set({ write: write, unknown: false })) }),
      store' = { ...store, writesInFlight: store.writesInFlight.union(Set(write)) },
      grantedEpoch' = grantedEpoch, clients' = clients,
    }
  }

  // Not applied at send time: a store can apply a write after the server stopped waiting for it, a reply can be lost,
  // and a resend can arrive apart from the first send (nanashi tessera/docs/persistence.md).
  // If-None-Match writes only when the key is absent, and of concurrent conditional writes to one key only the first to finish succeeds:
  // https://docs.aws.amazon.com/AmazonS3/latest/userguide/conditional-writes.html, and RustFS 1.0.0 gave one winner in 50 rounds of 8 writers (storage/rustfs/conditional_write.py).
  action apply(write: Write, replyLost: bool): bool = all {
    store.writesInFlight.contains(write),
    store' = applies(store, write, replyLost),
    servers' = servers, grantedEpoch' = grantedEpoch, clients' = clients,
  }

  action receiveReply(reply: Reply): bool = {
    val server = reply.write.server
    val outcome = receivesReply(servers.get(server), reply)
    all {
      store.replies.contains(reply),
      servers.get(server).pending.contains({ write: reply.write, unknown: false }),
      servers' = servers.set(server, outcome.self),
      clients' = { ...clients, acknowledged: clients.acknowledged.union(outcome.acknowledged) },
      store' = { ...store, replies: store.replies.exclude(Set(reply)) },
      grantedEpoch' = grantedEpoch,
    }
  }

  action timeout(server: str): bool = {
    nondet p = servers.get(server).pending.filter(p => not(p.unknown)).oneOf()
    all { become(server, timesOut(servers.get(server), p)), grantedEpoch' = grantedEpoch }
  }

  action resolve(server: str): bool = {
    nondet p = servers.get(server).pending.filter(p => p.unknown).oneOf()
    all { become(server, resolves(servers.get(server), p, store.slots)), grantedEpoch' = grantedEpoch }
  }

  action ghost = previous' = { slots: store.slots, acknowledged: clients.acknowledged }

  // Not a send of a request that is already in flight: it changes no state and used about 70% of the steps of a random run.
  action step = all {
    ghost,
    any {
      nondet server = Servers.oneOf() any { grant(server), crash(server), load(server), flush(server), timeout(server), resolve(server) },
      nondet server = Servers.oneOf() nondet request = Requests.oneOf() any {
        all { not(clients.requestsInFlight.contains({ server: server, request: request })), send(server, request) },
        receive(server, request),
      },
      nondet write = store.writesInFlight.oneOf() nondet replyLost = Set(true, false).oneOf() apply(write, replyLost),
      nondet reply = store.replies.oneOf() receiveReply(reply),
    },
  }

  val refinesChangeLog: bool =
    allowedStep({ commits: replay(previous.slots), acknowledged: previous.acknowledged }, { commits: replay(store.slots), acknowledged: clients.acknowledged })

  // Not covered by refinesChangeLog: an overwrite that keeps the requests, or that lands past the first hole, does not change the log.
  val segmentsNeverChange: bool = previous.slots.keys().forall(i =>
    store.slots.keys().contains(i) and store.slots.get(i) == previous.slots.get(i))

  val staleEpochNeverCommits: bool = store.slots.keys().forall(i =>
    store.slots.keys().forall(j => i < j implies store.slots.get(i).epoch <= store.slots.get(j).epoch))

  val oneContentPerSlotPerEpoch: bool = {
    val everyWrite = store.writesInFlight.union(store.replies.map(r => r.write)).union(
      servers.keys().map(s => servers.get(s).pending.map(p => p.write)).flatten())
    and {
      everyWrite.forall(w => everyWrite.forall(v => (w.slot == v.slot and w.epoch == v.epoch) implies w.requests == v.requests)),
      store.slots.keys().forall(i => everyWrite.forall(w =>
        (w.slot == i and w.epoch == store.slots.get(i).epoch) implies w.requests == store.slots.get(i).requests)),
    }
  }

  val dataWriteNeverSeesOlderEpoch: bool = servers.keys().forall(s =>
    servers.get(s).pending.forall(p =>
      (p.unknown and not(isFence(p.write)) and store.slots.keys().contains(p.write.slot))
        implies store.slots.get(p.write.slot).epoch >= p.write.epoch))

  val slotsContiguous: bool = store.slots.keys().forall(i => i >= 0 and i < store.slots.keys().size())

  // Only the newest Assignee: an old one keeps accepting until it learns of the handover. placement.qnt checks reads.
  val newestAssigneeSeesAcknowledged: bool = servers.keys().forall(server =>
    val self = servers.get(server)
    (self.phase == Ready and self.epoch == grantedEpoch) implies clients.acknowledged.subseteq(self.view.toSet()))

  // Not covered by newestAssigneeSeesAcknowledged: a change of order does not show in the set of acknowledged requests.
  val viewIsLogPrefix: bool = servers.keys().forall(server =>
    val self = servers.get(server)
    val log = replay(store.slots)
    (self.phase == Ready and self.epoch == grantedEpoch) implies
      (if (self.view.length() > log.length()) false else log.slice(0, self.view.length()) == self.view))

  val witnessTwoAcknowledged: bool = clients.acknowledged.size() >= 2
  val witnessCommitAfterHandover: bool = store.slots.keys().exists(i => store.slots.get(i).epoch >= 2 and store.slots.get(i).requests.length() > 0)
  val witnessOwnResendRejected: bool = store.replies.exists(reply =>
    not(reply.succeeded) and store.slots.keys().contains(reply.write.slot) and store.slots.get(reply.write.slot).epoch == reply.write.epoch)
  val witnessOldAssigneeStopped: bool = servers.keys().exists(server => servers.get(server).phase == Stopped)

  action applyOne: bool = nondet write = store.writesInFlight.oneOf() all { ghost, apply(write, false) }
  action replyOne: bool = nondet reply = store.replies.oneOf() all { ghost, receiveReply(reply) }
}

module seqlog_designed {
  import seqlog_design.*
  import seqlog(guards = allGuards).*
}
