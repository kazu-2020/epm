module seqlog_blind_put {
  import seqlog_design.* from "./seqlog"
  import seqlog(guards = { ...allGuards, ifNoneMatch: false }).* from "./seqlog"
}

module seqlog_unknown_as_success {
  import seqlog_design.* from "./seqlog"
  import seqlog(guards = { ...allGuards, readsBackUnknown: false }).* from "./seqlog"
}

module seqlog_no_deduplication {
  import seqlog_design.* from "./seqlog"
  import seqlog(guards = { ...allGuards, deduplication: false }).* from "./seqlog"
}

module seqlog_no_fence {
  import seqlog_design.* from "./seqlog"
  import seqlog(guards = { ...allGuards, fence: false }).* from "./seqlog"
}

module seqlog_no_epoch_check {
  import seqlog_design.* from "./seqlog"
  import seqlog(guards = { ...allGuards, epochCheck: false }).* from "./seqlog"
}

module seqlog_two_in_flight {
  import seqlog_design.* from "./seqlog"
  import seqlog(guards = { ...allGuards, oneInFlight: false }).* from "./seqlog"
}

module seqlog_rejected_as_fenced {
  import seqlog_design.* from "./seqlog"
  import seqlog(guards = { ...allGuards, headsAfterRejection: false }).* from "./seqlog"
}
