module seqlog_replay {
  import seqlog_design.* from "./seqlog"
  import seqlog(guards = allGuards).* from "./seqlog"

  // Not `step`: in its random runs, sends and crashes use most of the steps, and few runs reach an acknowledgement.
  // This step allows a subset of the runs of `step`, so a replay of it also replays the design.
  action replayStep = all {
    ghost,
    any {
      nondet server = Servers.oneOf() any {
        grant(server), all { grantedEpoch < MaxEpoch, crash(server) }, load(server), flush(server), timeout(server), resolve(server),
      },
      nondet server = Servers.oneOf() nondet request = Requests.oneOf() any {
        all { not(clients.requestsInFlight.contains({ server: server, request: request })), send(server, request) },
        receive(server, request),
      },
      nondet write = store.writesInFlight.oneOf() nondet replyLost = Set(true, false).oneOf() apply(write, replyLost),
      nondet reply = store.replies.oneOf() receiveReply(reply),
    },
  }
}
