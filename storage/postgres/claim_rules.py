import os

import psycopg

DSN = os.environ.get("EPM_PG", "postgresql://postgres@127.0.0.1:56433/epm")
WORKSPACE = "01KA0000000000000000000000"
NODE = "n1"
ADDRESS = "n1:1"
CORRECTION = 4294967296

CREATE = """CREATE TABLE placement_rules (
    workspace_global_id text PRIMARY KEY,
    epoch bigint NOT NULL DEFAULT 0,
    node text,
    claimed_at timestamptz,
    address text,
    address_epoch bigint NOT NULL DEFAULT 0,
    recorded_at timestamptz,
    boot_duration interval,
    consecutive_failures int NOT NULL DEFAULT 0
)"""
CLAIM = """UPDATE placement_rules
SET epoch = epoch + 1, node = %(node)s, claimed_at = now(),
    consecutive_failures = CASE
      WHEN %(planned)s THEN consecutive_failures
      WHEN address_epoch = epoch
        AND (recorded_at IS NULL OR recorded_at <= now() - interval '10 minutes') THEN 0
      ELSE consecutive_failures + 1
    END
WHERE workspace_global_id = %(workspace)s AND epoch = %(seen)s
  AND (claimed_at IS NULL OR claimed_at + %(wait)s::interval <= now())
  AND (NOT %(planned)s OR address_epoch = epoch)
RETURNING epoch, consecutive_failures"""
RECORD = """UPDATE placement_rules
SET address = %(address)s, address_epoch = %(epoch)s, recorded_at = now(), boot_duration = now() - claimed_at
WHERE workspace_global_id = %(workspace)s AND epoch = %(epoch)s AND node = %(node)s
RETURNING boot_duration"""
CORRECT = """UPDATE placement_rules
SET epoch = epoch + %(correction)s, address_epoch = epoch + %(correction)s,
    address = NULL, recorded_at = NULL, claimed_at = NULL"""


def main() -> None:
    # No prepared statements: PgBouncer in the transaction mode gives each statement any server connection.
    connection = psycopg.connect(DSN, autocommit=True, prepare_threshold=None)

    def claim(seen: int, planned: bool = False, wait: str = "0 seconds"):
        parameters = dict(node=NODE, planned=planned, workspace=WORKSPACE, seen=seen, wait=wait)
        return connection.execute(CLAIM, parameters).fetchone()

    def record(epoch: int, node: str = NODE):
        parameters = dict(address=ADDRESS, epoch=epoch, workspace=WORKSPACE, node=node)
        return connection.execute(RECORD, parameters).fetchone()

    connection.execute("DROP TABLE IF EXISTS placement_rules")
    connection.execute(CREATE)
    connection.execute("INSERT INTO placement_rules (workspace_global_id) VALUES (%s)", (WORKSPACE,))
    assert claim(0) == (1, 0), "the first claim of a new row is not a failure"
    assert claim(1, wait="1 hour") is None, "a booting row waits for the boot deadline"
    assert claim(1, planned=True) is None, "a planned replacement does not interrupt a booting row"
    assert claim(1) == (2, 1), "a claim that follows an unrecorded epoch is a failure"
    assert record(1) is None, "an old epoch is not recorded"
    assert record(2, node="n2") is None, "a process on another node is not recorded"
    assert record(2)[0] is not None
    assert claim(2, planned=True) == (3, 1), "a planned replacement keeps the count"
    record(3)
    assert claim(3) == (4, 2), "a replacement within 10 minutes of the record is a failure"
    record(4)
    connection.execute("UPDATE placement_rules SET recorded_at = now() - interval '11 minutes'")
    assert claim(4) == (5, 0), "a replacement after 10 minutes resets the count"
    connection.execute(CORRECT, dict(correction=CORRECTION))
    epoch, address_epoch, address = connection.execute(
        "SELECT epoch, address_epoch, address FROM placement_rules"
    ).fetchone()
    assert epoch == address_epoch == 5 + CORRECTION and address is None
    assert claim(epoch) == (epoch + 1, 0), "the first claim after the correction is not a failure"
    connection.execute("DROP TABLE placement_rules")
    print("ok")


if __name__ == "__main__":
    main()
