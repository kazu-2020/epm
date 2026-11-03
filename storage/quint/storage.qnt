module storage {
  import change_log.* from "./change_log"

  // If-Match compares the whole manifest, not a counter: an ETag changes only when the content changes.
  const Backend: str
  const Mutation: str
  const Servers: Set[str]
  const Requests: Set[str]
  const MaxEpoch: int
  const MaxResends: int

  type SegmentKey = { epoch: int, first: int }
  type Manifest = { epoch: int, segments: List[SegmentKey] }
  type Write = { server: str, expected: Manifest, next: Manifest, segment: SegmentKey, requests: List[str], attempt: int }
  type Reply = { write: Write, succeeded: bool }
  type Server = { phase: str, epoch: int, current: Manifest, view: List[str], batch: List[str], pending: Write }

  pure val emptyManifest: Manifest = { epoch: 0, segments: List() }
  pure val noWrite: Write = { server: "", expected: emptyManifest, next: emptyManifest, segment: { epoch: -1, first: -1 }, requests: List(), attempt: 0 }
  pure val down: Server = { phase: "down", epoch: 0, current: emptyManifest, view: List(), batch: List(), pending: noWrite }

  var manifest: Manifest
  var objects: SegmentKey -> List[str]
  var writesInFlight: Set[Write]
  var replies: Set[Reply]
  var servers: str -> Server
  var grantedEpoch: int
  var requestsInFlight: Set[{ server: str, request: str }]
  var acknowledged: Set[str]
  var previous: ChangeLog
  var previousEpoch: int
  var previousObjects: SegmentKey -> List[str]

  pure def has(requests: List[str], request: str): bool = requests.indices().exists(i => requests[i] == request)
  pure def toSet(requests: List[str]): Set[str] = requests.foldl(Set(), (collected, request) => collected.union(Set(request)))

  def replay(m: Manifest): List[str] =
    m.segments.foldl(List(), (commits, key) => if (objects.keys().contains(key)) commits.concat(objects.get(key)) else commits)

  val changeLog: ChangeLog = { commits: replay(manifest), acknowledged: acknowledged }

  action init = all {
    manifest' = emptyManifest,
    objects' = Map(),
    writesInFlight' = Set(),
    replies' = Set(),
    servers' = Servers.mapBy(_ => down),
    grantedEpoch' = 0,
    requestsInFlight' = Set(),
    acknowledged' = Set(),
    previous' = { commits: List(), acknowledged: Set() },
    previousEpoch' = 0,
    previousObjects' = Map(),
  }

  action unchangedStore = all {
    manifest' = manifest, objects' = objects, writesInFlight' = writesInFlight, replies' = replies,
  }

  action grant(server: str): bool = all {
    grantedEpoch < MaxEpoch,
    grantedEpoch' = grantedEpoch + 1,
    servers' = servers.set(server, { ...down, phase: "loading", epoch: grantedEpoch + 1 }),
    unchangedStore, requestsInFlight' = requestsInFlight, acknowledged' = acknowledged,
  }

  action crash(server: str): bool = all {
    servers.get(server).phase != "down",
    servers' = servers.set(server, down),
    unchangedStore, grantedEpoch' = grantedEpoch, requestsInFlight' = requestsInFlight, acknowledged' = acknowledged,
  }

  // No fence write on PostgreSQL: the controller changes the lease epoch in the same database, and that already
  // stops the old owner.
  action load(server: str): bool = {
    val self = servers.get(server)
    val fence = { ...noWrite, server: server, expected: manifest, next: { ...manifest, epoch: self.epoch } }
    val loaded = { ...self, current: manifest, view: replay(manifest) }
    all {
      self.phase == "loading",
      if (manifest.epoch > self.epoch and Mutation != "no_epoch_check") all {
        servers' = servers.set(server, { ...self, phase: "stopped" }),
        writesInFlight' = writesInFlight,
      } else if (Backend == "s3" and Mutation != "no_fence") all {
        servers' = servers.set(server, { ...loaded, phase: "waiting", pending: fence }),
        writesInFlight' = writesInFlight.union(Set(fence)),
      } else all {
        servers' = servers.set(server, { ...loaded, phase: "ready" }),
        writesInFlight' = writesInFlight,
      },
      manifest' = manifest, objects' = objects, replies' = replies,
      grantedEpoch' = grantedEpoch, requestsInFlight' = requestsInFlight, acknowledged' = acknowledged,
    }
  }

  // A send is always allowed, not only before the acknowledgement, because the acknowledgement can be lost.
  action send(server: str, request: str): bool = all {
    requestsInFlight' = requestsInFlight.union(Set({ server: server, request: request })),
    unchangedStore, servers' = servers, grantedEpoch' = grantedEpoch, acknowledged' = acknowledged,
  }

  action receive(server: str, request: str): bool = {
    val self = servers.get(server)
    val message = { server: server, request: request }
    all {
      requestsInFlight.contains(message),
      self.phase == "ready",
      requestsInFlight' = requestsInFlight.exclude(Set(message)),
      if (self.view.has(request) and Mutation != "no_deduplication") all {
        servers' = servers, acknowledged' = acknowledged.union(Set(request)),
      } else if (self.batch.has(request)) all {
        servers' = servers, acknowledged' = acknowledged,
      } else all {
        servers' = servers.set(server, { ...self, batch: self.batch.append(request) }), acknowledged' = acknowledged,
      },
      unchangedStore, grantedEpoch' = grantedEpoch,
    }
  }

  action flush(server: str): bool = {
    val self = servers.get(server)
    val key = { epoch: if (Mutation == "segment_key_without_epoch") 0 else self.epoch, first: self.view.length() }
    val write = { server: server, expected: self.current, next: { epoch: self.epoch, segments: self.current.segments.append(key) }, segment: key, requests: self.batch, attempt: 0 }
    all {
      self.phase == "ready",
      self.batch.length() > 0,
      objects' = if (Backend == "s3") objects.put(key, self.batch) else objects,
      writesInFlight' = writesInFlight.union(Set(write)),
      servers' = servers.set(server, { ...self, phase: "waiting", pending: write }),
      manifest' = manifest, replies' = replies,
      grantedEpoch' = grantedEpoch, requestsInFlight' = requestsInFlight, acknowledged' = acknowledged,
    }
  }

  // Not applied at send time: a store can apply a write after the server stopped waiting for it.
  action apply(write: Write, replyLost: bool): bool = {
    val matches = Mutation == "blind_write" or manifest == write.expected
    val leaseHeld = Backend != "postgres" or grantedEpoch == write.next.epoch
    val succeeded = matches and leaseHeld
    all {
      writesInFlight.contains(write),
      writesInFlight' = writesInFlight.exclude(Set(write)),
      manifest' = if (succeeded) write.next else manifest,
      objects' = if (Backend == "postgres" and succeeded and write.requests.length() > 0) objects.put(write.segment, write.requests) else objects,
      replies' = if (replyLost) replies else replies.union(Set({ write: write, succeeded: succeeded })),
      servers' = servers, grantedEpoch' = grantedEpoch, requestsInFlight' = requestsInFlight, acknowledged' = acknowledged,
    }
  }

  action succeed(server: str, self: Server): bool = all {
    servers' = servers.set(server, { ...self, phase: "ready", current: self.pending.next, view: self.view.concat(self.batch), batch: List(), pending: noWrite }),
    acknowledged' = acknowledged.union(toSet(self.batch)),
  }

  action receiveReply(reply: Reply): bool = {
    val self = servers.get(reply.write.server)
    all {
      replies.contains(reply),
      self.phase == "waiting",
      self.pending == reply.write,
      replies' = replies.exclude(Set(reply)),
      if (reply.succeeded) succeed(reply.write.server, self)
      else all { servers' = servers.set(reply.write.server, { ...self, phase: "unknown" }), acknowledged' = acknowledged },
      manifest' = manifest, objects' = objects, writesInFlight' = writesInFlight,
      grantedEpoch' = grantedEpoch, requestsInFlight' = requestsInFlight,
    }
  }

  action timeout(server: str): bool = {
    val self = servers.get(server)
    all {
      self.phase == "waiting",
      if (Mutation == "unknown_as_success") succeed(server, self)
      else all { servers' = servers.set(server, { ...self, phase: "unknown" }), acknowledged' = acknowledged },
      unchangedStore, grantedEpoch' = grantedEpoch, requestsInFlight' = requestsInFlight,
    }
  }

  // A failed condition is not proof of a fence: the failure can come from a resend of its own write that the
  // store already applied.
  action resolve(server: str): bool = {
    val self = servers.get(server)
    val notApplied = manifest == self.pending.expected
    val fenced = Backend == "postgres" and grantedEpoch != self.epoch
    val resend = notApplied and not(fenced) and self.pending.attempt < MaxResends
    val again = { ...self.pending, attempt: self.pending.attempt + 1 }
    all {
      self.phase == "unknown",
      if (manifest == self.pending.next) succeed(server, self)
      else if (fenced) all { servers' = servers.set(server, { ...self, phase: "stopped" }), acknowledged' = acknowledged }
      else if (resend) all { servers' = servers.set(server, { ...self, phase: "waiting", pending: again }), acknowledged' = acknowledged }
      else if (notApplied) all { servers' = servers.set(server, { ...self, phase: "waiting" }), acknowledged' = acknowledged }
      else all { servers' = servers.set(server, { ...down, phase: "loading", epoch: self.epoch }), acknowledged' = acknowledged },
      writesInFlight' = if (resend) writesInFlight.union(Set(again)) else writesInFlight,
      manifest' = manifest, objects' = objects, replies' = replies,
      grantedEpoch' = grantedEpoch, requestsInFlight' = requestsInFlight,
    }
  }

  action ghost = all { previous' = changeLog, previousEpoch' = manifest.epoch, previousObjects' = objects }

  action step = all {
    ghost,
    any {
      nondet server = Servers.oneOf() any { grant(server), crash(server), load(server), flush(server), timeout(server), resolve(server) },
      nondet server = Servers.oneOf() nondet request = Requests.oneOf() any { send(server, request), receive(server, request) },
      nondet write = writesInFlight.oneOf() nondet replyLost = Set(true, false).oneOf() apply(write, replyLost),
      nondet reply = replies.oneOf() receiveReply(reply),
    },
  }

  val refinesChangeLog: bool = allowedStep(previous, changeLog)

  val epochNeverDecreases: bool = previousEpoch <= manifest.epoch

  // Checked apart from refinesChangeLog: a segment overwritten with the same requests leaves the log the same.
  val segmentsNeverChange: bool = previousObjects.keys().forall(key =>
    objects.keys().contains(key) and objects.get(key) == previousObjects.get(key))

  val staleEpochNeverCommits: bool = manifest.segments.indices().forall(i =>
    manifest.segments[i].epoch <= manifest.epoch and (i == 0 or manifest.segments[i - 1].epoch <= manifest.segments[i].epoch))

  // replay skips a missing segment instead of failing, so a missing segment must be its own violation.
  val manifestSegmentsExist: bool = manifest.segments.indices().forall(i => objects.keys().contains(manifest.segments[i]))

  val newestOwnerSeesAcknowledged: bool = servers.keys().forall(server =>
    val self = servers.get(server)
    (self.phase == "ready" and self.epoch == grantedEpoch) implies acknowledged.subseteq(toSet(self.view)))

  val witnessNotTwoAcknowledged: bool = acknowledged.size() < 2
  val witnessNoCommitAfterHandover: bool = not(manifest.segments.indices().exists(i => manifest.segments[i].epoch >= 2))
  val witnessNoOwnResendRejected: bool = not(replies.exists(reply => not(reply.succeeded) and reply.write.next == manifest and reply.write.next != reply.write.expected))
  val witnessNoOldOwnerStopped: bool = servers.keys().forall(server => servers.get(server).phase != "stopped")
}

module s3 {
  import storage(Backend = "s3", Mutation = "none", Servers = Set("A", "B"), Requests = Set("r1", "r2"), MaxEpoch = 3, MaxResends = 1).*
}

module postgres {
  import storage(Backend = "postgres", Mutation = "none", Servers = Set("A", "B"), Requests = Set("r1", "r2"), MaxEpoch = 3, MaxResends = 1).*
}
