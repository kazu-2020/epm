
module s3_blind_write {
  import storage(Backend = "s3", Mutation = "blind_write", Servers = Set("A", "B"), Requests = Set("r1", "r2"), MaxEpoch = 3, MaxResends = 1).* from "./storage"
}

module s3_unknown_as_success {
  import storage(Backend = "s3", Mutation = "unknown_as_success", Servers = Set("A", "B"), Requests = Set("r1", "r2"), MaxEpoch = 3, MaxResends = 1).* from "./storage"
}

module s3_no_deduplication {
  import storage(Backend = "s3", Mutation = "no_deduplication", Servers = Set("A", "B"), Requests = Set("r1", "r2"), MaxEpoch = 3, MaxResends = 1).* from "./storage"
}

module s3_segment_key_without_epoch {
  import storage(Backend = "s3", Mutation = "segment_key_without_epoch", Servers = Set("A", "B"), Requests = Set("r1", "r2"), MaxEpoch = 3, MaxResends = 1).* from "./storage"
}

module s3_no_fence {
  import storage(Backend = "s3", Mutation = "no_fence", Servers = Set("A", "B"), Requests = Set("r1", "r2"), MaxEpoch = 3, MaxResends = 1).* from "./storage"
}

module s3_no_epoch_check {
  import storage(Backend = "s3", Mutation = "no_epoch_check", Servers = Set("A", "B"), Requests = Set("r1", "r2"), MaxEpoch = 3, MaxResends = 1).* from "./storage"
}

// A fixed scenario, because random simulation reaches this rollback only with some seeds.
module postgres_blind_write {
  import storage(Backend = "postgres", Mutation = "blind_write", Servers = Set("A"), Requests = Set("r1", "r2"), MaxEpoch = 1, MaxResends = 1).* from "./storage"

  action pick(requests: List[str], attempt: int): bool =
    nondet write = writesInFlight.filter(write => write.requests == requests and write.attempt == attempt).oneOf()
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
  import storage(Backend = "postgres", Mutation = "unknown_as_success", Servers = Set("A", "B"), Requests = Set("r1", "r2"), MaxEpoch = 3, MaxResends = 1).* from "./storage"
}

module postgres_no_deduplication {
  import storage(Backend = "postgres", Mutation = "no_deduplication", Servers = Set("A", "B"), Requests = Set("r1", "r2"), MaxEpoch = 3, MaxResends = 1).* from "./storage"
}

module postgres_segment_key_without_epoch {
  import storage(Backend = "postgres", Mutation = "segment_key_without_epoch", Servers = Set("A", "B"), Requests = Set("r1", "r2"), MaxEpoch = 3, MaxResends = 1).* from "./storage"
}

module postgres_no_epoch_check {
  import storage(Backend = "postgres", Mutation = "no_epoch_check", Servers = Set("A", "B"), Requests = Set("r1", "r2"), MaxEpoch = 3, MaxResends = 1).* from "./storage"
}

