module seqlog_design {
  type Guards = {
    ifNoneMatch: bool, readsBackUnknown: bool, deduplication: bool, inFlightDeduplication: bool, fence: bool,
    epochCheck: bool, oneInFlight: bool, headsAfterRejection: bool,
  }
  pure val allGuards: Guards = {
    ifNoneMatch: true, readsBackUnknown: true, deduplication: true, inFlightDeduplication: true, fence: true,
    epochCheck: true, oneInFlight: true, headsAfterRejection: true,
  }
}

module seqlog {
  import change_log.* from "./change_log"
  import seqlog_design.*

  const guards: Guards

  pure val Servers = Set("A", "B")
  pure val Requests = Set("r1", "r2")
  pure val MaxEpoch = 3
  pure val MaxResends = 1
  pure val MaxInFlight = 2

  // The epoch is at the start of the segment body, not in the key: the slot number alone is what two owners fight for.
  // One epoch has one owner, so the epoch also tells whose write it is.
  type Slot = { epoch: int, requests: List[str] }
  type Write = { server: str, slot: int, epoch: int, requests: List[str], attempt: int }
  type Reply = { write: Write, succeeded: bool }
  type Pending = { write: Write, unknown: bool }
  type Phase = Down | Loading | Fencing | Ready | Stopped
  type Server = { phase: Phase, epoch: int, next: int, view: List[str], batch: List[str], pending: Set[Pending] }
  type Store = { slots: int -> Slot, writesInFlight: Set[Write], replies: Set[Reply] }
  type Clients = { requestsInFlight: Set[{ server: str, request: str }], acknowledged: Set[str] }

  pure val emptyStore: Store = { slots: Map(), writesInFlight: Set(), replies: Set() }
  pure val down: Server = { phase: Down, epoch: 0, next: 0, view: List(), batch: List(), pending: Set() }
  pure def loading(epoch: int): Server = { ...down, phase: Loading, epoch: epoch }
  pure def isFence(write: Write): bool = write.requests.length() == 0
  pure def slotOf(write: Write): Slot = { epoch: write.epoch, requests: write.requests }

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

  action init = all {
    store' = emptyStore,
    servers' = Servers.mapBy(_ => down),
    grantedEpoch' = 0,
    clients' = { requestsInFlight: Set(), acknowledged: Set() },
    previous' = { slots: Map(), acknowledged: Set() },
  }

  action grant(server: str): bool = all {
    grantedEpoch < MaxEpoch,
    grantedEpoch' = grantedEpoch + 1,
    servers' = servers.set(server, loading(grantedEpoch + 1)),
    store' = store, clients' = clients,
  }

  action crash(server: str): bool = all {
    servers.get(server).phase != Down,
    servers' = servers.set(server, down),
    store' = store, grantedEpoch' = grantedEpoch, clients' = clients,
  }

  action stop(server: str): bool = all {
    servers' = servers.set(server, { ...servers.get(server), phase: Stopped, batch: List(), pending: Set() }),
    clients' = clients,
  }

  action load(server: str): bool = {
    val self = servers.get(server)
    val slots = store.slots
    val free = lastSlot(slots) + 1
    val fence = { server: server, slot: free, epoch: self.epoch, requests: List(), attempt: 0 }
    val loaded = { ...self, next: free + 1, view: replay(slots) }
    val stale = guards.epochCheck and lastSlot(slots) >= 0 and slots.get(lastSlot(slots)).epoch >= self.epoch
    all {
      self.phase == Loading,
      if (stale) stop(server)
      else if (guards.fence) all {
        servers' = servers.set(server, { ...loaded, phase: Fencing, pending: Set({ write: fence, unknown: false }) }),
        clients' = clients,
      } else all {
        servers' = servers.set(server, { ...loaded, phase: Ready, next: free }), clients' = clients,
      },
      store' = if (guards.fence and not(stale))
        { ...store, writesInFlight: store.writesInFlight.union(Set(fence)) }
      else store,
      grantedEpoch' = grantedEpoch,
    }
  }

  // A send is always allowed, not only before the acknowledgement, because the acknowledgement can be lost.
  action send(server: str, request: str): bool = all {
    clients' = { ...clients, requestsInFlight: clients.requestsInFlight.union(Set({ server: server, request: request })) },
    store' = store, servers' = servers, grantedEpoch' = grantedEpoch,
  }

  action receive(server: str, request: str): bool = {
    val self = servers.get(server)
    val message = { server: server, request: request }
    val received = { ...clients, requestsInFlight: clients.requestsInFlight.exclude(Set(message)) }
    val inFlight = if (guards.inFlightDeduplication) self.pending.map(p => p.write.requests.toSet()).flatten() else Set()
    val held = self.batch.toSet().union(inFlight)
    all {
      clients.requestsInFlight.contains(message),
      self.phase == Ready,
      if (guards.deduplication and self.view.toSet().contains(request)) all {
        servers' = servers, clients' = { ...received, acknowledged: clients.acknowledged.union(Set(request)) },
      } else if (held.contains(request)) all {
        servers' = servers, clients' = received,
      } else all {
        servers' = servers.set(server, { ...self, batch: self.batch.append(request) }), clients' = received,
      },
      store' = store, grantedEpoch' = grantedEpoch,
    }
  }

  action flush(server: str): bool = {
    val self = servers.get(server)
    val write = { server: server, slot: self.next, epoch: self.epoch, requests: self.batch, attempt: 0 }
    all {
      self.phase == Ready,
      self.batch.length() > 0,
      if (guards.oneInFlight) self.pending.size() == 0 else self.pending.size() < MaxInFlight,
      servers' = servers.set(server, { ...self, next: self.next + 1, batch: List(), pending: self.pending.union(Set({ write: write, unknown: false })) }),
      store' = { ...store, writesInFlight: store.writesInFlight.union(Set(write)) },
      grantedEpoch' = grantedEpoch, clients' = clients,
    }
  }

  // Not applied at send time: a store can apply a write after the server stopped waiting for it.
  action apply(write: Write, replyLost: bool): bool = {
    val succeeded = not(guards.ifNoneMatch) or not(store.slots.keys().contains(write.slot))
    all {
      store.writesInFlight.contains(write),
      store' = {
        slots: if (succeeded) store.slots.put(write.slot, slotOf(write)) else store.slots,
        writesInFlight: store.writesInFlight.exclude(Set(write)),
        replies: if (replyLost) store.replies else store.replies.union(Set({ write: write, succeeded: succeeded })),
      },
      servers' = servers, grantedEpoch' = grantedEpoch, clients' = clients,
    }
  }

  action succeed(server: str, self: Server, write: Write): bool = all {
    servers' = servers.set(server, {
      ...self,
      phase: if (isFence(write)) Ready else self.phase,
      view: self.view.concat(write.requests),
      pending: self.pending.filter(p => p.write != write),
    }),
    clients' = { ...clients, acknowledged: clients.acknowledged.union(write.requests.toSet()) },
  }

  action replace(server: str, self: Server, write: Write, after: Pending): bool = all {
    servers' = servers.set(server, { ...self, pending: self.pending.filter(p => p.write != write).union(Set(after)) }),
    clients' = clients,
  }

  // A 412 is not proof of a fence: the slot can hold this server's own write, from a resend or a late apply.
  action receiveReply(reply: Reply): bool = {
    val write = reply.write
    val server = write.server
    val self = servers.get(server)
    all {
      store.replies.contains(reply),
      self.pending.contains({ write: write, unknown: false }),
      if (reply.succeeded) succeed(server, self, write)
      else if (guards.headsAfterRejection) replace(server, self, write, { write: write, unknown: true })
      else stop(server),
      store' = { ...store, replies: store.replies.exclude(Set(reply)) },
      grantedEpoch' = grantedEpoch,
    }
  }

  action timeout(server: str): bool = {
    val self = servers.get(server)
    nondet p = self.pending.filter(p => not(p.unknown)).oneOf()
    all {
      if (guards.readsBackUnknown) replace(server, self, p.write, { ...p, unknown: true }) else succeed(server, self, p.write),
      store' = store, grantedEpoch' = grantedEpoch,
    }
  }

  action resolve(server: str): bool = {
    val self = servers.get(server)
    nondet p = self.pending.filter(p => p.unknown).oneOf()
    val write = p.write
    val taken = store.slots.keys().contains(write.slot)
    val found = store.slots.get(write.slot)
    val again = { ...write, attempt: write.attempt + 1 }
    val refence = { ...write, slot: write.slot + 1, attempt: 0 }
    val resend = not(taken) and write.attempt < MaxResends
    val own = taken and found.epoch == write.epoch
    val oldOwnerTookFenceSlot = taken and found.epoch < write.epoch and isFence(write)
    all {
      if (not(taken)) replace(server, self, write, { write: if (resend) again else write, unknown: false })
      else if (own) succeed(server, self, write)
      else if (oldOwnerTookFenceSlot) all {
        servers' = servers.set(server, {
          ...self, next: refence.slot + 1, view: self.view.concat(found.requests), pending: Set({ write: refence, unknown: false }),
        }),
        clients' = clients,
      }
      else stop(server),
      store' =
        if (resend) { ...store, writesInFlight: store.writesInFlight.union(Set(again)) }
        else if (oldOwnerTookFenceSlot) { ...store, writesInFlight: store.writesInFlight.union(Set(refence)) }
        else store,
      grantedEpoch' = grantedEpoch,
    }
  }

  action ghost = previous' = { slots: store.slots, acknowledged: clients.acknowledged }

  action step = all {
    ghost,
    any {
      nondet server = Servers.oneOf() any { grant(server), crash(server), load(server), flush(server), timeout(server), resolve(server) },
      nondet server = Servers.oneOf() nondet request = Requests.oneOf() any { send(server, request), receive(server, request) },
      nondet write = store.writesInFlight.oneOf() nondet replyLost = Set(true, false).oneOf() apply(write, replyLost),
      nondet reply = store.replies.oneOf() receiveReply(reply),
    },
  }

  val refinesChangeLog: bool =
    allowedStep({ commits: replay(previous.slots), acknowledged: previous.acknowledged }, { commits: replay(store.slots), acknowledged: clients.acknowledged })

  val segmentsNeverChange: bool = previous.slots.keys().forall(i =>
    store.slots.keys().contains(i) and store.slots.get(i) == previous.slots.get(i))

  val staleEpochNeverCommits: bool = store.slots.keys().forall(i =>
    store.slots.keys().forall(j => i < j implies store.slots.get(i).epoch <= store.slots.get(j).epoch))

  // A 412 is resolved by the epoch alone, so one epoch must never write two contents to one slot.
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

  val newestOwnerSeesAcknowledged: bool = servers.keys().forall(server =>
    val self = servers.get(server)
    (self.phase == Ready and self.epoch == grantedEpoch) implies clients.acknowledged.subseteq(self.view.toSet()))

  val viewIsLogPrefix: bool = servers.keys().forall(server =>
    val self = servers.get(server)
    val log = replay(store.slots)
    (self.phase == Ready and self.epoch == grantedEpoch) implies
      (if (self.view.length() > log.length()) false else log.slice(0, self.view.length()) == self.view))

  val witnessNotTwoAcknowledged: bool = clients.acknowledged.size() < 2
  val witnessNoCommitAfterHandover: bool = not(store.slots.keys().exists(i => store.slots.get(i).epoch >= 2 and store.slots.get(i).requests.length() > 0))
  val witnessNoOwnResendRejected: bool = not(store.replies.exists(reply =>
    not(reply.succeeded) and store.slots.get(reply.write.slot).epoch == reply.write.epoch))
  val witnessNoOldOwnerStopped: bool = servers.keys().forall(server => servers.get(server).phase != Stopped)

  action applyOne: bool = nondet write = store.writesInFlight.oneOf() all { ghost, apply(write, false) }
  action replyOne: bool = nondet reply = store.replies.oneOf() all { ghost, receiveReply(reply) }

  run oldOwnerTakesSlotFirstTest =
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
        store.slots.get(2) == { epoch: 2, requests: List() },
        refinesChangeLog, newestOwnerSeesAcknowledged, staleEpochNeverCommits,
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

module seqlog_designed {
  import seqlog_design.*
  import seqlog(guards = allGuards).*
}
