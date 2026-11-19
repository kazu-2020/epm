# ストレージ層の Quint モデル

変更ログを S3 へ書き込む方式(`seqlog.qnt`)と、配置表・App server・読み取り(`placement.qnt`、ADR 0012)のモデル。Assignee の交代、クラッシュ、応答の消失、再送が同時に発生しても、ACK 済みのコミットが消失しないことと、読み取りが ACK 済みのコミットを含むことを検査する。モデルの前提と出典は、対応する action のコメントに記載する。

`*_mutations.qnt` は、守りを1個ずつ除外したモデルである。`check.sh` は、各モデルで `breaks` に記載した性質の違反を確認する。違反を検出しなければ、検査そのものが機能していない。

## 実行

```sh
npm ci --prefix storage/quint
export PATH="$PWD/storage/quint/node_modules/.bin:$PATH"
storage/quint/check.sh          # 1モジュールあたり5万回のランダムシミュレーション
storage/quint/check.sh 5000     # 短時間で確認する場合。回数が不足すると変異の反例に到達せず失敗することがあるので、失敗した場合は既定の回数で再実行する
```

終了コードが 1 の場合、次のいずれかが発生した。

- 設計どおりのモデル(`seqlog_designed`、`placement_designed`)で性質に違反した。
- PostgreSQL の切り替えを含むモデル(`placement_failover`)で性質に違反した。
- witness(到達を期待する状態)に1回も到達しなかった。
- 固定の手順(`quint test`)が失敗した。
- 守りを除外したモデルで、`breaks`、`placement_breaks`、`placement_read_write_breaks` に記載した性質の違反を検出しなかった。または、`holds` と `log_safe_without_guard` の性質に違反した。

反例の手順は、守りを除外したモジュールで `quint run seqlog_mutations.qnt --main <モジュール> --invariant <性質> --mbt` を実行して確認する(例: `--main seqlog_no_fence --invariant newestAssigneeSeesAcknowledged`)。設計どおりのモデルはすべての性質を満たすので、反例を出力しない。placement の場合は、`--init placementInit --step placementStep` を指定して `placement_mutations.qnt` を実行する。読み取りと書き込みの経路の反例は `--step placementReadWriteStep` で確認する。`mbt::actionTaken` に各ステップの操作を出力する。

モデルの前提である条件付き書き込みの応答は、RustFS で以下のとおり実測する。

```sh
docker compose -f storage/rustfs/compose.yaml up -d
AWS_ENDPOINT_URL=http://127.0.0.1:9000 AWS_ACCESS_KEY_ID=demo \
AWS_SECRET_ACCESS_KEY=demo-secret AWS_REGION=us-east-1 \
uv run --with boto3 storage/rustfs/conditional_write.py
```

配置表の前提である条件付きの UPDATE は、`storage/placement` の統合テストが PostgreSQL と PgBouncer で検査する。

```sh
docker compose -f storage/postgres/compose.yaml up -d --wait
psql -v ON_ERROR_STOP=1 -f storage/placement/migrations/0001_placement.sql postgres://postgres@127.0.0.1:55433/epm
(cd storage/placement && PLACEMENT_POSTGRES_URL=postgres://postgres@127.0.0.1:56433/epm cargo test)
```

`seqlog_replay.qnt` のトレースは、Rust の `Assignee::step` で再生する。再生の試験は `storage/change_log/src/assignee/replay.rs` にある。`cargo test` で実行し、PATH に `quint` が必要である。失敗した場合は、表示される `QUINT_SEED=<値>` を指定して再実行する。

## 範囲外

- snapshot、GC、再計算済み番号、Request ID のウィンドウ。
- 複数のワークスペースの変更ログと、テナントの混入。これらは、キーの命名規則、ワークスペースごとのデータ鍵、IAM で保護する。`placement.qnt` の対象は、アドレスの再利用で他のワークスペースの Assignee へ要求が到達する場合だけである。
- 活性(最終的に確定すること、交代後に書き込みが可能になること)。ランダムシミュレーションは活性を検査しない。
- グループコミットに集約する件数、ハッシュチェーン、Event ID、3分のタイムアウト。

網羅検査ではない。反例がないことは、「5万回の試行で検出しなかった」という意味である。
