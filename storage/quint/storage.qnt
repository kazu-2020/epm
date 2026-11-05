module design {
  type Backend = { segmentBeforeCommit: bool, leaseCheckedAtCommit: bool, newOwnerWritesFence: bool }
  type Guards = { manifestCondition: bool, readsBackUnknown: bool, deduplication: bool, epochInSegmentKey: bool, epochCheck: bool }

  pure val S3: Backend = { segmentBeforeCommit: true, leaseCheckedAtCommit: false, newOwnerWritesFence: true }
  // No fence write on PostgreSQL: the controller changes the lease epoch in the same database, and that already
  // stops the old owner.
  pure val PostgreSQL: Backend = { segmentBeforeCommit: false, leaseCheckedAtCommit: true, newOwnerWritesFence: false }
  pure val allGuards: Guards = { manifestCondition: true, readsBackUnknown: true, deduplication: true, epochInSegmentKey: true, epochCheck: true }
}

module storage {
  import change_log.* from "./change_log"
  import design.*

  // If-Match compares the whole manifest, not a counter: an ETag changes only when the content changes.
  const backend: Backend
  const guards: Guards

  pure val Servers = Set("A", "B")
  pure val Requests = Set("r1", "r2")
  pure val MaxEpoch = 3
  pure val MaxResends = 1

  type SegmentKey = { epoch: int, first: int }
  type Manifest = { epoch: int, segments: List[SegmentKey] }
  type Content = Fence | Segment({ key: SegmentKey, requests: List[str] })
  type Write = { server: str, expected: Manifest, next: Manifest, content: Content, attempt: int }
  type Reply = { write: Write, succeeded: bool }
  type Phase = Down | Loading | Ready | Waiting(Write) | Unknown(Write) | Stopped
  type Server = { phase: Phase, epoch: int, current: Manifest, view: List[str], batch: List[str] }
  type Store = { manifest: Manifest, objects: SegmentKey -> List[str], writesInFlight: Set[Write], replies: Set[Reply] }
  type Clients = { requestsInFlight: Set[{ server: str, request: str }], acknowledged: Set[str] }

  pure val emptyManifest: Manifest = { epoch: 0, segments: List() }
  pure val emptyStore: Store = { manifest: emptyManifest, objects: Map(), writesInFlight: Set(), replies: Set() }
  pure val down: Server = { phase: Down, epoch: 0, current: emptyManifest, view: List(), batch: List() }
  pure def loading(epoch: int): Server = { ...down, phase: Loading, epoch: epoch }
  pure def waiting(phase: Phase): Set[Write] = match phase { | Waiting(write) => Set(write) | _ => Set() }
  pure def unknown(phase: Phase): Set[Write] = match phase { | Unknown(write) => Set(write) | _ => Set() }

  var store: Store
  var servers: str -> Server
  var grantedEpoch: int
  var clients: Clients
  var previous: { manifest: Manifest, objects: SegmentKey -> List[str], acknowledged: Set[str] }

  pure def replay(objects: SegmentKey -> List[str], m: Manifest): List[str] =
    m.segments.foldl(List(), (commits, key) => if (objects.keys().contains(key)) commits.concat(objects.get(key)) else commits)

  pure def changeLogOf(objects: SegmentKey -> List[str], m: Manifest, acknowledged: Set[str]): ChangeLog =
    { commits: replay(objects, m), acknowledged: acknowledged }

  action init = all {
    store' = emptyStore,
    servers' = Servers.mapBy(_ => down),
    grantedEpoch' = 0,
    clients' = { requestsInFlight: Set(), acknowledged: Set() },
    previous' = { manifest: emptyManifest, objects: Map(), acknowledged: Set() },
  }

  action become(server: str, self: Server, phase: Phase): bool = all {
    servers' = servers.set(server, { ...self, phase: phase }),
    clients' = clients,
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

  action load(server: str): bool = {
    val self = servers.get(server)
    val m = store.manifest
    val fence = { server: server, expected: m, next: { ...m, epoch: self.epoch }, content: Fence, attempt: 0 }
    val loaded = { ...self, current: m, view: replay(store.objects, m) }
    all {
      self.phase == Loading,
      if (guards.epochCheck and m.epoch > self.epoch) all {
        become(server, self, Stopped), store' = store,
      } else if (backend.newOwnerWritesFence) all {
        become(server, loaded, Waiting(fence)), store' = { ...store, writesInFlight: store.writesInFlight.union(Set(fence)) },
      } else all {
        become(server, loaded, Ready), store' = store,
      },
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
    all {
      clients.requestsInFlight.contains(message),
      self.phase == Ready,
      if (guards.deduplication and self.view.toSet().contains(request)) all {
        servers' = servers, clients' = { ...received, acknowledged: clients.acknowledged.union(Set(request)) },
      } else if (self.batch.toSet().contains(request)) all {
        servers' = servers, clients' = received,
      } else all {
        servers' = servers.set(server, { ...self, batch: self.batch.append(request) }), clients' = received,
      },
      store' = store, grantedEpoch' = grantedEpoch,
    }
  }

  action flush(server: str): bool = {
    val self = servers.get(server)
    val key = { epoch: if (guards.epochInSegmentKey) self.epoch else 0, first: self.view.length() }
    val write = {
      server: server, expected: self.current, next: { epoch: self.epoch, segments: self.current.segments.append(key) },
      content: Segment({ key: key, requests: self.batch }), attempt: 0,
    }
    all {
      self.phase == Ready,
      self.batch.length() > 0,
      store' = {
        ...store,
        objects: if (backend.segmentBeforeCommit) store.objects.put(key, self.batch) else store.objects,
        writesInFlight: store.writesInFlight.union(Set(write)),
      },
      become(server, self, Waiting(write)),
      grantedEpoch' = grantedEpoch,
    }
  }

  // Not applied at send time: a store can apply a write after the server stopped waiting for it.
  action apply(write: Write, replyLost: bool): bool = {
    val matches = not(guards.manifestCondition) or store.manifest == write.expected
    val leaseHeld = not(backend.leaseCheckedAtCommit) or grantedEpoch == write.next.epoch
    val succeeded = matches and leaseHeld
    val objects = match write.content {
      | Segment(segment) => if (succeeded and not(backend.segmentBeforeCommit)) store.objects.put(segment.key, segment.requests) else store.objects
      | Fence => store.objects
    }
    all {
      store.writesInFlight.contains(write),
      store' = {
        manifest: if (succeeded) write.next else store.manifest,
        objects: objects,
        writesInFlight: store.writesInFlight.exclude(Set(write)),
        replies: if (replyLost) store.replies else store.replies.union(Set({ write: write, succeeded: succeeded })),
      },
      servers' = servers, grantedEpoch' = grantedEpoch, clients' = clients,
    }
  }

  action succeed(server: str, self: Server, pending: Write): bool = all {
    servers' = servers.set(server, { ...self, phase: Ready, current: pending.next, view: self.view.concat(self.batch), batch: List() }),
    clients' = { ...clients, acknowledged: clients.acknowledged.union(self.batch.toSet()) },
  }

  action receiveReply(reply: Reply): bool = {
    val server = reply.write.server
    val self = servers.get(server)
    all {
      store.replies.contains(reply),
      self.phase == Waiting(reply.write),
      if (reply.succeeded) succeed(server, self, reply.write) else become(server, self, Unknown(reply.write)),
      store' = { ...store, replies: store.replies.exclude(Set(reply)) },
      grantedEpoch' = grantedEpoch,
    }
  }

  action timeout(server: str): bool = {
    val self = servers.get(server)
    nondet pending = waiting(self.phase).oneOf()
    all {
      if (guards.readsBackUnknown) become(server, self, Unknown(pending)) else succeed(server, self, pending),
      store' = store, grantedEpoch' = grantedEpoch,
    }
  }

  // A failed condition is not proof of a fence: the failure can come from a resend of its own write that the
  // store already applied.
  action resolve(server: str): bool = {
    val self = servers.get(server)
    nondet pending = unknown(self.phase).oneOf()
    val notApplied = store.manifest == pending.expected
    val fenced = backend.leaseCheckedAtCommit and grantedEpoch != self.epoch
    val resend = notApplied and not(fenced) and pending.attempt < MaxResends
    val again = { ...pending, attempt: pending.attempt + 1 }
    all {
      if (store.manifest == pending.next) succeed(server, self, pending)
      else if (fenced) become(server, self, Stopped)
      else if (resend) become(server, self, Waiting(again))
      else if (notApplied) become(server, self, Waiting(pending))
      else all { servers' = servers.set(server, loading(self.epoch)), clients' = clients },
      store' = if (resend) { ...store, writesInFlight: store.writesInFlight.union(Set(again)) } else store,
      grantedEpoch' = grantedEpoch,
    }
  }

  action ghost = previous' = { manifest: store.manifest, objects: store.objects, acknowledged: clients.acknowledged }

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
    allowedStep(changeLogOf(previous.objects, previous.manifest, previous.acknowledged), changeLogOf(store.objects, store.manifest, clients.acknowledged))

  val epochNeverDecreases: bool = previous.manifest.epoch <= store.manifest.epoch

  // Checked apart from refinesChangeLog: a segment overwritten with the same requests leaves the log the same.
  val segmentsNeverChange: bool = previous.objects.keys().forall(key =>
    store.objects.keys().contains(key) and store.objects.get(key) == previous.objects.get(key))

  val staleEpochNeverCommits: bool = {
    val segments = store.manifest.segments
    segments.indices().forall(i => segments[i].epoch <= store.manifest.epoch and (i == 0 or segments[i - 1].epoch <= segments[i].epoch))
  }

  // replay skips a missing segment instead of failing, so a missing segment must be its own violation.
  val manifestSegmentsExist: bool = store.manifest.segments.indices().forall(i => store.objects.keys().contains(store.manifest.segments[i]))

  val newestOwnerSeesAcknowledged: bool = servers.keys().forall(server =>
    val self = servers.get(server)
    (self.phase == Ready and self.epoch == grantedEpoch) implies clients.acknowledged.subseteq(self.view.toSet()))

  val witnessNotTwoAcknowledged: bool = clients.acknowledged.size() < 2
  val witnessNoCommitAfterHandover: bool = not(store.manifest.segments.indices().exists(i => store.manifest.segments[i].epoch >= 2))
  val witnessNoOwnResendRejected: bool =
    not(store.replies.exists(reply => not(reply.succeeded) and reply.write.next == store.manifest and reply.write.next != reply.write.expected))
  val witnessNoOldOwnerStopped: bool = servers.keys().forall(server => servers.get(server).phase != Stopped)
}

module s3 {
  import design.*
  import storage(backend = S3, guards = allGuards).*
}

module postgres {
  import design.*
  import storage(backend = PostgreSQL, guards = allGuards).*
}
