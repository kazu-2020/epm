module change_log {
  type ChangeLog = { commits: List[str], acknowledged: Set[str] }

  pure def committedRequests(log: ChangeLog): Set[str] =
    log.commits.foldl(Set(), (requests, request) => requests.union(Set(request)))

  pure def isPrefix(shorter: List[str], longer: List[str]): bool =
    shorter.length() <= longer.length() and shorter.indices().forall(i => shorter[i] == longer[i])

  pure def appendsOnlyNewRequests(before: List[str], after: List[str]): bool =
    val added = after.slice(before.length(), after.length())
    added.indices().forall(i =>
      not(before.indices().exists(j => before[j] == added[i]))
      and not(added.indices().exists(j => j != i and added[j] == added[i])))

  pure def allowedStep(before: ChangeLog, after: ChangeLog): bool = and {
    isPrefix(before.commits, after.commits),
    appendsOnlyNewRequests(before.commits, after.commits),
    before.acknowledged.subseteq(after.acknowledged),
    after.acknowledged.subseteq(committedRequests(after)),
  }
}

module change_log_machine {
  import change_log.*

  const Requests: Set[str]

  var log: ChangeLog

  action init = log' = { commits: List(), acknowledged: Set() }

  action commit(request: str): bool = all {
    not(committedRequests(log).contains(request)),
    log' = { ...log, commits: log.commits.append(request) },
  }

  action acknowledge(request: str): bool = all {
    committedRequests(log).contains(request),
    log' = { ...log, acknowledged: log.acknowledged.union(Set(request)) },
  }

  action step = nondet request = Requests.oneOf() any { commit(request), acknowledge(request) }
}

module change_log_check {
  import change_log_machine(Requests = Set("r1", "r2")).*
}
