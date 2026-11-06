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


def resend_after_success() -> int:
    key = f"segment-{uuid.uuid4()}"
    s3.put_object(Bucket=BUCKET, Key=key, Body=b"0", IfNoneMatch="*")
    return status(lambda: s3.put_object(Bucket=BUCKET, Key=key, Body=b"0", IfNoneMatch="*"))


def winner_is_read_back() -> bool:
    key = f"segment-{uuid.uuid4()}"
    bodies = [str(n).encode() for n in range(WRITERS)]
    replies = race([lambda body=body: s3.put_object(Bucket=BUCKET, Key=key, Body=body, IfNoneMatch="*") for body in bodies])
    winners = [body for body, reply in zip(bodies, replies) if reply == 200]
    return len(winners) == 1 and s3.get_object(Bucket=BUCKET, Key=key)["Body"].read() == winners[0]


def main() -> None:
    if status(lambda: s3.head_bucket(Bucket=BUCKET)) == 404:
        s3.create_bucket(Bucket=BUCKET)
    one_winner = {tuple(sorted([200] + [412] * (WRITERS - 1))): ROUNDS}
    results = {
        "If-None-Match race": dict(if_none_match_race()),
        "If-None-Match resend after a success": resend_after_success(),
        "The 1 winner of a race is the body that GET returns": all(winner_is_read_back() for _ in range(ROUNDS)),
    }
    for name, result in results.items():
        print(f"{name}: {result}")
    assert results["If-None-Match race"] == one_winner
    assert results["If-None-Match resend after a success"] == 412
    assert results["The 1 winner of a race is the body that GET returns"]


if __name__ == "__main__":
    main()
