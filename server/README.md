# server

The server owns one workspace. It writes each edit to a change log in S3 and recalculates the measures. Clients use JSON over HTTP.

Start only one process for each workspace at a time. The server has no authentication, so it listens on loopback by default.

## Start RustFS

RustFS is an S3-compatible store for local work. Start it with Docker and make a bucket:

```bash
docker compose -f ../storage/rustfs/compose.yaml up -d
curl --aws-sigv4 aws:amz:us-east-1:s3 --user demo:demo-secret -X PUT http://127.0.0.1:9000/epm-demo
```

## Set the variables

```bash
export EPM_S3_BUCKET=epm-demo
export AWS_ENDPOINT=http://127.0.0.1:9000
export AWS_ACCESS_KEY_ID=demo
export AWS_SECRET_ACCESS_KEY=demo-secret
export AWS_REGION=us-east-1
export AWS_ALLOW_HTTP=true
```

`AWS_ALLOW_HTTP=true` lets the server use plain HTTP to RustFS. If you use AWS S3, do not set `AWS_ENDPOINT` or `AWS_ALLOW_HTTP`.

## Run the server

```bash
cargo run --locked -- --listen 127.0.0.1:8080 --workspace demo
```

- `--listen`: the address. The default is `127.0.0.1:8080`.
- `--workspace`: the prefix of the change log in the bucket. The default is `demo`.
- `--epoch`: the epoch of this process. The default is the current time in milliseconds. Each start must use a larger epoch than the last start.

The workspace definition is fixed in the code (`src/main.rs`). When the owner stops, the process exits with code 1. Start a new process to continue. SIGINT (Ctrl-C) and SIGTERM stop the server. It finishes the requests in progress and exits with code 0. If the requests do not finish in 5 seconds, it exits with code 1.

## Run the demo

`demo.sh` builds the server, starts it, and checks these items:

1. An edit to an input changes a calculated value.
2. An edit to a calculated measure gets `invalid_argument` and a reason.
3. A resend of the same Request ID gets the same Commit Sequence.
4. After `kill -9`, a new process restores the values from the log.
5. After `kill -TERM`, the server exits with code 0.

```bash
./demo.sh
```

The script uses a new workspace for each run. If a check fails, it exits with code 1.

## Call the API

All calls are `POST` with a JSON body. `SubmitEdit` and `ReadCells` need the header `content-type: application/json`.

```bash
api=http://127.0.0.1:8080/epm.v1.Workspace
json='content-type: application/json'
curl -X POST "$api/ListDefinitions" -d '{}'
curl -X POST -H "$json" "$api/SubmitEdit" \
  -d '{"request_id":"r1","measure":"00000000000000000000000034","coordinate":[0,1,1,0,1],"value":3}'
curl -X POST -H "$json" "$api/ReadCells" \
  -d '{"at_least":0,"cells":[{"measure":"00000000000000000000000068","coordinate":[0,1,1,0,1]}]}'
curl -X POST "$api/GetLag" -d '{}'
```

- `SubmitEdit` returns `{"sequence": N}`. A `null` value clears the cell. If you send the same `request_id` again, you get the same sequence.
- `ReadCells` returns `{"recalculated": N, "values": [...]}`. The values include all commits up to `at_least` or later. If you leave out `at_least`, you get the values that are published now.
- A value is `null` if the cell is empty or if a member of the coordinate is not in its dimension.
- `GetLag` returns the last committed and the last recalculated sequence.
- `recalculated` and `committed` are `null` until the first commit is recalculated or committed.

After a start, the server reads the log. Until it reaches the end of the log, a `ReadCells` without `at_least` and `GetLag` do not show the commits of the log. To read your own edit, send the sequence from `SubmitEdit` as `at_least`.

The errors in this table have the body `{"code": ..., "message": ...}` with a Connect code. Other errors, such as an unknown path (404), a method that is not `POST` (405), or a body larger than 2 MB (413), do not have this body.

| Error | Code | HTTP status |
| --- | --- | --- |
| Malformed JSON, wrong content type, bad ULID, edit to a calculated measure, coordinate of the wrong length, member out of range in an edit | `invalid_argument` | 400 |
| Unknown measure in a read | `not_found` | 404 |
| The engine is too far behind | `resource_exhausted` | 429 |
| The sequence is not committed yet | `unavailable` | 503 |

If you get 429 or 503, try again later. When the owner stops, the process exits and the connection closes.
