module s3_blind_write {
  import design.* from "./storage"
  import storage(backend = S3, guards = { ...allGuards, manifestCondition: false }).* from "./storage"
}

module s3_unknown_as_success {
  import design.* from "./storage"
  import storage(backend = S3, guards = { ...allGuards, readsBackUnknown: false }).* from "./storage"
}

module s3_no_deduplication {
  import design.* from "./storage"
  import storage(backend = S3, guards = { ...allGuards, deduplication: false }).* from "./storage"
}

// A fixed scenario, because random simulation reaches this overwrite only with some seeds.
module s3_segment_key_without_epoch {
  import design.* from "./storage"
  import storage(backend = S3, guards = { ...allGuards, epochInSegmentKey: false }).* from "./storage"

  action applyOne: bool = nondet write = store.writesInFlight.oneOf() all { ghost, apply(write, false) }
  action replyOne: bool = nondet reply = store.replies.oneOf() all { ghost, receiveReply(reply) }

  run oldOwnerOverwritesTest =
    init
      .then(all { ghost, grant("A") })
      .then(all { ghost, load("A") })
      .then(applyOne)
      .then(replyOne)
      .then(all { ghost, grant("B") })
      .then(all { ghost, load("B") })
      .then(applyOne)
      .then(replyOne)
      .then(all { ghost, send("B", "r2") })
      .then(all { ghost, receive("B", "r2") })
      .then(all { ghost, flush("B") })
      .then(applyOne)
      .then(replyOne)
      .then(all { ghost, send("A", "r1") })
      .then(all { ghost, receive("A", "r1") })
      .expect(refinesChangeLog and clients.acknowledged == Set("r2"))
      .then(all { ghost, flush("A") })
      .expect(not(refinesChangeLog))
}

module s3_no_fence {
  import design.* from "./storage"
  import storage(backend = { ...S3, newOwnerWritesFence: false }, guards = allGuards).* from "./storage"
}

module s3_no_epoch_check {
  import design.* from "./storage"
  import storage(backend = S3, guards = { ...allGuards, epochCheck: false }).* from "./storage"
}

// A fixed scenario, because random simulation reaches this rollback only with some seeds.
module postgres_blind_write {
  import design.* from "./storage"
  import storage(backend = PostgreSQL, guards = { ...allGuards, manifestCondition: false }).* from "./storage"

  pure def requestsOf(content: Content): List[str] = match content { | Segment(segment) => segment.requests | Fence => List() }

  action pick(requests: List[str], attempt: int): bool =
    nondet write = store.writesInFlight.filter(write => requestsOf(write.content) == requests and write.attempt == attempt).oneOf()
    all { ghost, apply(write, true) }

  run lateResendRollsBackTest =
    init
      .then(all { ghost, grant("A") })
      .then(all { ghost, load("A") })
      .then(all { ghost, send("A", "r1") })
      .then(all { ghost, receive("A", "r1") })
      .then(all { ghost, flush("A") })
      .then(all { ghost, timeout("A") })
      .then(all { ghost, resolve("A") })
      .then(pick(List("r1"), 0))
      .then(all { ghost, timeout("A") })
      .then(all { ghost, resolve("A") })
      .then(all { ghost, send("A", "r2") })
      .then(all { ghost, receive("A", "r2") })
      .then(all { ghost, flush("A") })
      .then(pick(List("r2"), 0))
      .then(pick(List("r1"), 1))
      .expect(not(refinesChangeLog))
}

module postgres_unknown_as_success {
  import design.* from "./storage"
  import storage(backend = PostgreSQL, guards = { ...allGuards, readsBackUnknown: false }).* from "./storage"
}

module postgres_no_deduplication {
  import design.* from "./storage"
  import storage(backend = PostgreSQL, guards = { ...allGuards, deduplication: false }).* from "./storage"
}

module postgres_segment_key_without_epoch {
  import design.* from "./storage"
  import storage(backend = PostgreSQL, guards = { ...allGuards, epochInSegmentKey: false }).* from "./storage"
}

module postgres_no_epoch_check {
  import design.* from "./storage"
  import storage(backend = PostgreSQL, guards = { ...allGuards, epochCheck: false }).* from "./storage"
}
