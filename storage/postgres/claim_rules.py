import os

import psycopg

c = psycopg.connect(os.environ.get("EPM_PG", "postgresql://postgres@127.0.0.1:56433/epm"), autocommit=True, prepare_threshold=None)
W = "01KA0000000000000000000000"
c.execute("DROP TABLE IF EXISTS placement_rules")
c.execute("""CREATE TABLE placement_rules (workspace_global_id text PRIMARY KEY, epoch bigint NOT NULL DEFAULT 0, node text, claimed_at timestamptz,
  address text, address_epoch bigint NOT NULL DEFAULT 0, recorded_at timestamptz, boot_duration interval, consecutive_failures int NOT NULL DEFAULT 0)""")
c.execute("INSERT INTO placement_rules (workspace_global_id) VALUES (%s)", (W,))
CLAIM = """UPDATE placement_rules
SET epoch = epoch + 1, node = %(node)s, claimed_at = now(),
    consecutive_failures = CASE
      WHEN %(planned)s THEN consecutive_failures
      WHEN address_epoch = epoch
        AND (recorded_at IS NULL OR recorded_at <= now() - interval '10 minutes') THEN 0
      ELSE consecutive_failures + 1
    END
WHERE workspace_global_id = %(w)s AND epoch = %(seen)s
  AND (claimed_at IS NULL OR claimed_at + %(wait)s::interval <= now())
  AND (NOT %(planned)s OR address_epoch = epoch)
RETURNING epoch, consecutive_failures"""
RECORD = """UPDATE placement_rules
SET address = %(a)s, address_epoch = %(e)s, recorded_at = now(), boot_duration = now() - claimed_at
WHERE workspace_global_id = %(w)s AND epoch = %(e)s AND node = %(node)s RETURNING boot_duration"""
def claim(seen, planned=False, wait="0 seconds"):
    return c.execute(CLAIM, dict(node="n1", planned=planned, w=W, seen=seen, wait=wait)).fetchone()
def record(e, node="n1"): return c.execute(RECORD, dict(a="n1:1", e=e, w=W, node=node)).fetchone()
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
c.execute("UPDATE placement_rules SET recorded_at = now() - interval '11 minutes'")
assert claim(4) == (5, 0), "a replacement after 10 minutes resets the count"
c.execute("""UPDATE placement_rules SET epoch = epoch + 4294967296, address_epoch = epoch + 4294967296,
    address = NULL, recorded_at = NULL, claimed_at = NULL""")
e, ae, a = c.execute("SELECT epoch, address_epoch, address FROM placement_rules").fetchone()
assert e == ae == 5 + 4294967296 and a is None
assert claim(e) == (e + 1, 0), "the first claim after the correction is not a failure"
c.execute("DROP TABLE placement_rules")
print("ok")
