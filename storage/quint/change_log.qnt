module change_log {
  type ChangeLog = { commits: List[str], acknowledged: Set[str] }

  pure def toSet(list: List[a]): Set[a] = list.foldl(Set(), (collected, element) => collected.union(Set(element)))

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
    after.acknowledged.subseteq(after.commits.toSet()),
  }
}
