"""Measure the S3 requests that the change log (scheme A) uses.

Run it on EC2 in the region of the bucket. `.github/workflows/s3-latency.yml` does that.
Local run against RustFS (only to check the script, not the numbers):

    AWS_ENDPOINT_URL=http://127.0.0.1:59000 AWS_ACCESS_KEY_ID=nanashi \
    AWS_SECRET_ACCESS_KEY=nanashi-secret AWS_REGION=us-east-1 \
    uv run --with boto3 storage/s3/latency.py <bucket> <prefix> <out dir> --quick
"""

import collections
import concurrent.futures
import json
import math
import pathlib
import sys
import time
import uuid

import boto3
import boto3.s3.transfer
import botocore.config
import botocore.exceptions

BUCKET, PREFIX, OUT = sys.argv[1], sys.argv[2].rstrip("/"), pathlib.Path(sys.argv[3])
QUICK = "--quick" in sys.argv
COUNT = 20 if QUICK else 1000
RATE_SECONDS = 5 if QUICK else 300
RACE_ROUNDS = 5 if QUICK else 50
LIST_KEYS = (100, 300) if QUICK else (1_000, 10_000)
LIST_ROUNDS = 10 if QUICK else 200
BIG_MB = (8, 16) if QUICK else (1024, 10 * 1024)

# No retries: a retry hides the tail, and a retried write whose first attempt succeeded returns 412.
fast = boto3.client(
    "s3",
    config=botocore.config.Config(
        retries={"total_max_attempts": 1}, connect_timeout=1, read_timeout=1, max_pool_connections=64
    ),
)
slow = boto3.client("s3", config=botocore.config.Config(retries={"total_max_attempts": 1}, max_pool_connections=128))
setup_with_retries = boto3.client(
    "s3", config=botocore.config.Config(retries={"mode": "standard"}, max_pool_connections=32)
)
results: dict[str, dict] = {}


def key(*parts) -> str:
    return "/".join([PREFIX, *map(str, parts)])


def position(n: int) -> str:
    return f"{n:020d}.seg"


def timed(call) -> tuple[float, int | str]:
    start = time.perf_counter()
    try:
        call()
        code = 200
    except botocore.exceptions.ClientError as error:
        code = error.response["ResponseMetadata"]["HTTPStatusCode"]
    except botocore.exceptions.BotoCoreError:
        code = 0
    return (time.perf_counter() - start) * 1000, code


def percentile(sorted_values: list[float], p: float) -> float:
    return sorted_values[max(0, math.ceil(p * len(sorted_values)) - 1)]


def record(name: str, samples: list[tuple[float, int | str]], **extra) -> None:
    times = sorted(ms for ms, _ in samples)
    results[name] = {
        "count": len(samples),
        "codes": dict(collections.Counter(str(code) for _, code in samples)),
        **{
            f"p{label}": round(percentile(times, p), 1)
            for label, p in [("50", 0.5), ("90", 0.9), ("99", 0.99), ("999", 0.999)]
        },
        "max": round(times[-1], 1),
        **extra,
        "ms": [round(ms, 2) for ms, _ in samples],
    }
    save()
    print(name, {k: v for k, v in results[name].items() if k != "ms"}, flush=True)


def save() -> None:
    (OUT / "latency.json").write_text(json.dumps(results))
    lines = [
        "| case | count | codes | p50 ms | p90 ms | p99 ms | p99.9 ms | max ms | note |",
        "|---|---|---|---|---|---|---|---|---|",
    ]
    for name, r in results.items():
        if "ms" in r:
            lines.append(
                f"| {name} | {r['count']} | {r['codes']} | {r['p50']} | {r['p90']} | {r['p99']} | {r['p999']} | {r['max']} | {r.get('note', '')} |"
            )
        else:
            lines.append(f"| {name} | | | | | | | | {r['note']} |")
    (OUT / "latency.md").write_text("\n".join(lines) + "\n")


def put_new(client, k: str, body: bytes):
    return lambda: client.put_object(Bucket=BUCKET, Key=k, Body=body, IfNoneMatch="*")


def sizes() -> None:
    for label, size in [("1KB", 1024), ("100KB", 100 * 1024), ("10MB", 10 * 1024 * 1024)]:
        client = slow if size > 1024 * 1024 else fast
        body = b"x" * size
        keys = [key("size", label, position(n)) for n in range(COUNT)]
        record(
            f"put-if-none-match-{label}",
            [timed(put_new(client, k, body)) for k in keys],
            note="new keys, one at a time",
        )

        record(
            f"get-{label}", [timed(lambda k=k: client.get_object(Bucket=BUCKET, Key=k)["Body"].read()) for k in keys]
        )


def rates() -> None:
    body = b"x" * 4096
    for per_second in (1, 10):
        total = per_second * RATE_SECONDS
        keys = [key("rate", per_second, position(n)) for n in range(total)]
        start = time.perf_counter()

        def scheduled(n: int):
            time.sleep(max(0.0, start + n / per_second - time.perf_counter()))
            return timed(put_new(fast, keys[n], body))

        with concurrent.futures.ThreadPoolExecutor(32) as pool:
            samples = list(pool.map(scheduled, range(total)))
        record(f"put-if-none-match-4KB-{per_second}-per-second", samples, note=f"{RATE_SECONDS} s, new keys")


def race() -> None:
    outcomes: collections.Counter[str] = collections.Counter()
    winner_read_back = 0
    for _ in range(RACE_ROUNDS):
        k = key("race", uuid.uuid4())
        bodies = [str(n).encode() for n in range(8)]
        with concurrent.futures.ThreadPoolExecutor(8) as pool:
            replies = list(pool.map(lambda b: timed(put_new(fast, k, b))[1], bodies))
        outcomes[str(sorted(replies))] += 1
        if (
            replies.count(200) == 1
            and setup_with_retries.get_object(Bucket=BUCKET, Key=k)["Body"].read() == bodies[replies.index(200)]
        ):
            winner_read_back += 1
    results["race-8-writers-same-key"] = {
        "note": f"{RACE_ROUNDS} rounds, outcomes {dict(outcomes)}, rounds with one 200 and its body stored: {winner_read_back}",
    }
    save()
    print(results["race-8-writers-same-key"], flush=True)


def conflict_then_head() -> None:
    keys = [key("size", "1KB", position(n)) for n in range(COUNT)]
    conflicts, heads, both = [], [], []
    for k in keys:
        conflict = timed(put_new(fast, k, b"y"))
        head = timed(lambda: fast.head_object(Bucket=BUCKET, Key=k))
        conflicts.append(conflict)
        heads.append(head)
        both.append((conflict[0] + head[0], f"{conflict[1]}+{head[1]}"))
    record("put-if-none-match-existing-key", conflicts, note="expect 412")
    record("head-after-412", heads)
    record("412-then-head", both, note="sum of the two, the in-doubt path")


def lists() -> None:
    body = b"x" * 100
    for size in LIST_KEYS:
        keys = [key("list", size, position(n)) for n in range(size)]
        with concurrent.futures.ThreadPoolExecutor(32) as pool:
            list(pool.map(lambda k: setup_with_retries.put_object(Bucket=BUCKET, Key=k, Body=body), keys))
        prefix = key("list", size) + "/"
        tail = keys[-11].removeprefix(prefix)

        def list_tail():
            reply = fast.list_objects_v2(Bucket=BUCKET, Prefix=prefix, StartAfter=prefix + tail)
            assert reply["KeyCount"] == 10, reply["KeyCount"]

        def list_all():
            found = 0
            token = {}
            while True:
                reply = fast.list_objects_v2(Bucket=BUCKET, Prefix=prefix, **token)
                found += reply["KeyCount"]
                if not reply["IsTruncated"]:
                    break
                token = {"ContinuationToken": reply["NextContinuationToken"]}
            assert found == size, found

        record(
            f"list-{size}-keys-start-after-tail",
            [timed(list_tail) for _ in range(LIST_ROUNDS)],
            note="returns the last 10 keys",
        )
        record(
            f"list-{size}-keys-all-pages",
            [timed(list_all) for _ in range(LIST_ROUNDS)],
            note="all pages, one after another",
        )


def range_get() -> None:
    part = 16 * 1024 * 1024
    transfer = boto3.s3.transfer.TransferConfig(multipart_chunksize=64 * 1024 * 1024, max_concurrency=32)
    for mb in BIG_MB:
        k = key("big", f"{mb}MB")
        path = pathlib.Path(f"/tmp/big-{mb}")
        with path.open("wb") as file:
            file.truncate(mb * 1024 * 1024)
        start = time.perf_counter()
        slow.upload_file(str(path), BUCKET, k, Config=transfer)
        upload_seconds = time.perf_counter() - start
        path.unlink()
        size = mb * 1024 * 1024
        ranges = [(offset, min(offset + part, size) - 1) for offset in range(0, size, part)]

        def read(r):
            return len(slow.get_object(Bucket=BUCKET, Key=k, Range=f"bytes={r[0]}-{r[1]}")["Body"].read())

        rates_mb = []
        for _ in range(3):
            start = time.perf_counter()
            with concurrent.futures.ThreadPoolExecutor(64) as pool:
                assert sum(pool.map(read, ranges)) == size
            rates_mb.append(round(mb / (time.perf_counter() - start)))
        results[f"range-get-{mb}MB"] = {
            "note": f"16MB ranges, 64 threads, MB/s of 3 runs {rates_mb}; upload {round(mb / upload_seconds)} MB/s",
        }
        save()
        print(results[f"range-get-{mb}MB"], flush=True)


OUT.mkdir(parents=True, exist_ok=True)
for step in (sizes, conflict_then_head, rates, race, lists, range_get):
    step()
