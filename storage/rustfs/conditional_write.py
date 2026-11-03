"""Measure the replies of conditional writes on an S3 compatible server.

The Quint model (storage/quint/) assumes these replies. Run this script to check them:

    AWS_ENDPOINT_URL=http://127.0.0.1:59000 AWS_ACCESS_KEY_ID=nanashi \
    AWS_SECRET_ACCESS_KEY=nanashi-secret AWS_REGION=us-east-1 \
    uv run --with boto3 storage/rustfs/conditional_write.py
"""

import collections
import concurrent.futures
import uuid

import boto3
import botocore.config
import botocore.exceptions

BUCKET = "epm-conditional-write"
WRITERS = 8
ROUNDS = 50

# No retries: a retried write whose first attempt succeeded returns 412 and looks like a lost race.
s3 = boto3.client("s3", config=botocore.config.Config(retries={"total_max_attempts": 1}))


def status(call) -> int:
    try:
        call()
        return 200
    except botocore.exceptions.ClientError as error:
        return error.response["ResponseMetadata"]["HTTPStatusCode"]


def race(calls) -> list[int]:
    with concurrent.futures.ThreadPoolExecutor(len(calls)) as pool:
        return list(pool.map(status, calls))


def if_match_race() -> collections.Counter[tuple[int, ...]]:
    outcomes: collections.Counter[tuple[int, ...]] = collections.Counter()
    for _ in range(ROUNDS):
        key = f"manifest-{uuid.uuid4()}"
        etag = s3.put_object(Bucket=BUCKET, Key=key, Body=b"0")["ETag"]
        replies = race(
            [
                lambda body=str(n).encode(): s3.put_object(Bucket=BUCKET, Key=key, Body=body, IfMatch=etag)
                for n in range(1, WRITERS + 1)
            ]
        )
        outcomes[tuple(sorted(replies))] += 1
    return outcomes


def if_none_match_race() -> collections.Counter[tuple[int, ...]]:
    outcomes: collections.Counter[tuple[int, ...]] = collections.Counter()
    for _ in range(ROUNDS):
        key = f"segment-{uuid.uuid4()}"
        replies = race(
            [
                lambda body=str(n).encode(): s3.put_object(Bucket=BUCKET, Key=key, Body=body, IfNoneMatch="*")
                for n in range(WRITERS)
            ]
        )
        outcomes[tuple(sorted(replies))] += 1
    return outcomes


def retry_after_success() -> int:
    key = f"manifest-{uuid.uuid4()}"
    etag = s3.put_object(Bucket=BUCKET, Key=key, Body=b"0")["ETag"]
    s3.put_object(Bucket=BUCKET, Key=key, Body=b"1", IfMatch=etag)
    return status(lambda: s3.put_object(Bucket=BUCKET, Key=key, Body=b"1", IfMatch=etag))


def if_match_missing_key() -> int:
    return status(lambda: s3.put_object(Bucket=BUCKET, Key=f"missing-{uuid.uuid4()}", Body=b"0", IfMatch='"0"'))


def winner_is_read_back() -> bool:
    key = f"manifest-{uuid.uuid4()}"
    etag = s3.put_object(Bucket=BUCKET, Key=key, Body=b"0")["ETag"]
    bodies = [str(n).encode() for n in range(1, WRITERS + 1)]
    replies = race([lambda body=body: s3.put_object(Bucket=BUCKET, Key=key, Body=body, IfMatch=etag) for body in bodies])
    winners = [body for body, reply in zip(bodies, replies) if reply == 200]
    return len(winners) == 1 and s3.get_object(Bucket=BUCKET, Key=key)["Body"].read() == winners[0]


def main() -> None:
    if status(lambda: s3.head_bucket(Bucket=BUCKET)) == 404:
        s3.create_bucket(Bucket=BUCKET)
    one_winner = {tuple(sorted([200] + [412] * (WRITERS - 1))): ROUNDS}
    results = {
        "If-Match race": dict(if_match_race()),
        "If-None-Match race": dict(if_none_match_race()),
        "If-Match retry with the old ETag after a success": retry_after_success(),
        "If-Match on a missing key": if_match_missing_key(),
        "The 1 winner of a race is the body that GET returns": all(winner_is_read_back() for _ in range(ROUNDS)),
    }
    for name, result in results.items():
        print(f"{name}: {result}")
    assert results["If-Match race"] == one_winner
    assert results["If-None-Match race"] == one_winner
    assert results["If-Match retry with the old ETag after a success"] == 412
    assert results["If-Match on a missing key"] == 404
    assert results["The 1 winner of a race is the body that GET returns"]


if __name__ == "__main__":
    main()
