# server

The server owns one workspace. It writes each edit to a change log in S3 and recalculates the measures. Clients use the Connect protocol with JSON over HTTP. `proto/workspace.proto` is the contract.

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
cargo run --locked -- --listen 127.0.0.1:8080 --workspace 01JCQ7ZK3M5V8W9X0Y1Z2A3B4C --epoch 1
```

- `--workspace`: the WorkspaceGlobalId (a ULID). It is the prefix of the change log and the snapshots in the bucket. It is required. If it is not a ULID, the server does not start.
- `--listen`: the address. The default is `127.0.0.1:8080`.
- `--epoch`: the epoch of this process. It is required. Each start must use a larger epoch than the last start of the same workspace. The placement table issues it (ADR 0012).

The code fixes the workspace definition (`src/main.rs`). When the assignee stops, the process exits with code 1. Start a new process to continue. SIGINT (Ctrl-C) and SIGTERM stop the server. It finishes the requests in progress and exits with code 0. If the requests do not finish in 5 seconds, or a second signal comes first, it exits with code 1.

The server writes its log to stderr through `tracing`. The default level is `info`. To change the level, set `RUST_LOG` (for example `RUST_LOG=warn`).

## Run the demo

`demo.sh` builds the server, starts it, and checks these items:

1. An edit to an input changes a calculated value.
2. An edit to a calculated measure gets `invalid_argument` and a reason.
3. A resend of the same Request ID gets the same Commit Sequence.
4. After `kill -9`, a new process restores the values from the log.
5. A write for the old epoch gets `failed_precondition` from the new process, and nothing commits.
6. After `kill -TERM`, the server exits with code 0.

```bash
./demo.sh
```

The script uses a new workspace for each run. If a check fails, it exits with code 1.

## Call the API

All calls are `POST` with a JSON body. Each call needs these headers:

- `content-type: application/json`, or `application/proto` for the binary protobuf form. connectrpc also takes the gRPC and gRPC-Web content types (`application/grpc*`), and gives their errors in trailers with status 200. The server refuses other content types with 415, so a web page cannot send an edit to loopback without a CORS preflight.
- `epm-workspace`: the WorkspaceGlobalId of the server, as a ULID.
- `epm-epoch`: the epoch of the server, as a decimal number.

The server compares `epm-workspace` and `epm-epoch` with its own `--workspace` and `--epoch`. If a header is missing, is not a ULID or a number, or has another value, the call gets `failed_precondition` and does nothing. Thus a request for an old process does not commit in a new process on the same port (#195). The server makes this check before it reads the log. Thus a wrong process never answers `unavailable`.

```bash
api=http://127.0.0.1:8080/epm.v1.Workspace
headers=(-H 'content-type: application/json' -H 'epm-workspace: 01JCQ7ZK3M5V8W9X0Y1Z2A3B4C' -H 'epm-epoch: 1')
curl -X POST "${headers[@]}" "$api/ListDefinitions" -d '{}'
# One entry for each axis of the measure, with the ULIDs from ListDefinitions.
cell='[{"dimension":"<dimension ULID>","member":"<member ULID>"}]'
curl -X POST "${headers[@]}" "$api/SubmitEdit" \
  -d '{"request_id":"r1","measure":"00000000000000000000000034","coordinate":'"$cell"',"value":3}'
curl -X POST "${headers[@]}" "$api/ReadCells" \
  -d '{"at_least":0,"cells":[{"measure":"00000000000000000000000068","coordinate":'"$cell"'}]}'
curl -X POST "${headers[@]}" "$api/GetLag" -d '{}'
```

- A coordinate names one member of each axis of the measure by the ULIDs of the dimension and the member, in any order. `ListDefinitions` gives the axes of each measure and the ULIDs of the members. A position in the declared order is not a coordinate. The change log keeps the edit, and the declared order can change later (#184).
- The `kind` of a measure in `ListDefinitions` is `MEASURE_KIND_INPUT` or `MEASURE_KIND_CALCULATED`. Only an input measure takes an edit.
- `SubmitEdit` returns `{"sequence": "N"}`. A `null` value clears the cell. An edit without `value` is an error. If you send the same `request_id` again, you get the same sequence.
- `ReadCells` returns `{"recalculated": "N", "values": [...]}`. The values include all commits up to `at_least` or later. If you leave out `at_least`, you get the values that the server has recalculated so far.
- A value is `null` if the cell is empty. A member that is not in its dimension is an error, the same as in an edit.
- `GetLag` returns the last committed and the last recalculated sequence.
- The responses follow the JSON form of protobuf. A sequence is a string, because a JSON number cannot hold each 64-bit value. A request can send a sequence as a number or a string.
- A response leaves out a field that has no value. For example, `GetLag` returns `{}` before the first commit. `committed` appears after the first commit, and `recalculated` appears after the server recalculates it.
- The server refuses an unknown field in a JSON request with `invalid_argument`. Thus a misspelled `at_least` does not read old values.

After a start, the server listens at once. Then it reads the newest snapshot, reads the log after it, and writes its fence. Until the server writes the fence, the four calls above return `unavailable`. A `GetLag` that is not `unavailable` shows that the server is ready (ADR 0012). After that, a `ReadCells` without `at_least` can still miss the newest commits, because the recalculation can be behind. To read your own edit, send the sequence from `SubmitEdit` as `at_least`.

The errors in this table have the body `{"code": ..., "message": ...}` with a Connect code. A method that is not `POST` (405) and a content type that the server refuses (415) do not have this body.

| Error | Code | HTTP status |
| --- | --- | --- |
| Missing or wrong `epm-workspace` or `epm-epoch` | `failed_precondition` | 400 |
| Malformed JSON, unknown field, missing `value`, a value that is not a finite number or null, bad ULID, edit to a calculated measure, coordinate that does not name each axis once, member that is not in its dimension | `invalid_argument` | 400 |
| Unknown measure in a read | `not_found` | 404 |
| Unknown path | `unimplemented` | 404 |
| The engine is too far behind, or the body is larger than 4 MB | `resource_exhausted` | 429 |
| The server is loading the log, or the sequence is not committed yet | `unavailable` | 503 |
| The request has `connect-timeout-ms`, and the time passed before the answer | `deadline_exceeded` | 504 |

If you get 429 or 503, try again later. If a `SubmitEdit` gets `deadline_exceeded`, a timeout on your side, or a closed connection, the edit can still commit. Send it again with the same `request_id`. Then the server gives the same sequence and does not apply the edit two times. If you get `failed_precondition`, find the current process of the workspace and send the request there. When the assignee stops, the process exits and the connection closes.
