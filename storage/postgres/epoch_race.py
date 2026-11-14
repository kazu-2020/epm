import collections
import concurrent.futures
import os
import statistics
import threading
import time

import psycopg

DSN = os.environ.get("EPM_PG", "postgresql://postgres@127.0.0.1:56433/epm")
WRITERS = 8
ROUNDS = 50
LOOKUPS = 2000
WORKSPACE = "01KA0000000000000000000000"

CONDITIONAL = "UPDATE placement SET epoch = epoch + 1 WHERE workspace_global_id = %s AND epoch = %s RETURNING epoch"
BLIND = "UPDATE placement SET epoch = %s + 1 WHERE workspace_global_id = %s RETURNING epoch"
LOOKUP = "SELECT epoch FROM placement WHERE workspace_global_id = %s"


def connect() -> psycopg.Connection:
    # No prepared statements: PgBouncer in the transaction mode gives each statement any server connection.
    return psycopg.connect(DSN, autocommit=True, prepare_threshold=None)


def race(connections: list[psycopg.Connection], claim) -> collections.Counter[int]:
    issued: collections.Counter[int] = collections.Counter()
    for _ in range(ROUNDS):
        barrier = threading.Barrier(WRITERS, timeout=10)

        def writer(connection: psycopg.Connection) -> int | None:
            (seen,) = connection.execute(LOOKUP, (WORKSPACE,)).fetchone()
            barrier.wait()
            row = claim(connection, seen).fetchone()
            return row and row[0]

        with concurrent.futures.ThreadPoolExecutor(WRITERS) as pool:
            issued.update(epoch for epoch in pool.map(writer, connections) if epoch is not None)
    return issued


def main() -> None:
    connections = [connect() for _ in range(WRITERS)]
    control = connections[0]
    control.execute("CREATE TABLE IF NOT EXISTS placement (workspace_global_id text PRIMARY KEY, epoch bigint NOT NULL)")

    def reset() -> None:
        control.execute(
            "INSERT INTO placement VALUES (%s, 0) ON CONFLICT (workspace_global_id) DO UPDATE SET epoch = 0", (WORKSPACE,)
        )

    reset()
    conditional = race(connections, lambda connection, seen: connection.execute(CONDITIONAL, (WORKSPACE, seen)))
    print(f"conditional update: {len(conditional)} epochs issued in {ROUNDS} rounds, most issues of one epoch: {max(conditional.values())}")
    assert sorted(conditional) == list(range(1, ROUNDS + 1)), "each round issues exactly the next epoch"
    assert max(conditional.values()) == 1, "an epoch is issued once"

    reset()
    blind = race(connections, lambda connection, seen: connection.execute(BLIND, (seen, WORKSPACE)))
    print(f"blind update: {len(blind)} epochs issued in {ROUNDS} rounds, most issues of one epoch: {max(blind.values())}")
    assert sorted(blind) == list(range(1, ROUNDS + 1)), "each round issues the next epoch"
    assert max(blind.values()) == WRITERS, "every writer of a round issues the same epoch"

    times = []
    for _ in range(LOOKUPS):
        start = time.perf_counter()
        control.execute(LOOKUP, (WORKSPACE,)).fetchone()
        times.append((time.perf_counter() - start) * 1000)
    cuts = statistics.quantiles(times, n=100)
    print(f"lookup over one connection: p50 {cuts[49]:.2f} ms, p99 {cuts[98]:.2f} ms ({LOOKUPS} lookups)")


main()
