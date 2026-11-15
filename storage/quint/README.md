# ストレージ層の Quint モデル

変更ログの S3 への書き方が、Assignee の交代、クラッシュ、応答の消失、再送が重なっても壊れないかを確かめるモデル。変更ログは、`<ws>/log/<Slot>` に Slot の順に並ぶセグメントの列である。Assignee は次の Slot に If-None-Match で PUT し、200 なら ACK する。

`seqlog.qnt` の範囲は「ログとフェンシング」だけにした。snapshot と GC は入れていない。`placement.qnt` は、配置表、App server、読み取りを加えたモデルで、ほかのワークスペースの Assignee がアドレスを再利用する場合も含む。

## ファイル

| ファイル | 中身 |
| --- | --- |
| `change_log.qnt` | 抽象の仕様。ワークスペースごとに追記だけの列が1本あり、1つの Request ID は高々1回だけ足され、ACK するのは列に入ったものだけ |
| `seqlog.qnt` | 変更ログの書き方のモデル。Assignee、配置コントローラー(エポックを渡す)、Slot、クライアントの再送。守りは `seqlog_design` の `allGuards` に、サーバー数などの上限は `seqlog` の `pure val` に置く |
| `seqlog_mutations.qnt` | 守りを1つずつ外したモデル(`{ ...allGuards, deduplication: false }` など)。検査が実際に落ちることを示す。`check.sh` は、ここのモジュールがすべて `breaks` か `holds` に入っているかを確かめる |
| `seqlog_test.qnt` | 設計どおりのモデルの固定の手順(`quint test`)。手順の部品(`applyOne` など)は、変異のモジュールも使うので `seqlog.qnt` に置く |
| `seqlog_replay.qnt` | Rust の実装に再生するトレースを作るための `replayStep`。`step` の一部の手順だけを許し、送信と停止にステップを使い切らないようにした |
| `placement.qnt` | 配置表、App server、ランチャー、読み取り、書き込みの転送を `seqlog.qnt` に追加したモデル。PostgreSQL の切り替え(遅延するレプリカへの復帰)も含む。規則は `placement_design` の `allPlacementGuards` に置く(ADR 0012) |
| `placement_test.qnt` | 設計どおりの配置のモデルの固定の手順。手順の部品(`onLog`、`table`、`started` など)は、変異のモジュールも使うので `placement.qnt` に置く |
| `placement_mutations.qnt` | 規則を1個ずつ除外した module と、その違反を示す固定の手順。`check.sh` は、ここのモジュールがすべて `placement_breaks` か `placement_scenarios` に入っているかを確かめる。`placement_failover_no_node_check` と `placement_failover_duplicate_start` は `processCheck` も除外する。`processCheck` が有効だと同一エポックの2個目のプロセスがフェンスで停止し、どちらの規則の除外も違反にならないため |
| `check.sh` | すべての検査をまとめて実行する |
| `../postgres/epoch_race.py` | PostgreSQL と PgBouncer で、エポックを発行する UPDATE の同時実行を実測する |
| `../postgres/claim_rules.py` | エポックの発行と記録の UPDATE の規則(連続失敗の計数、待機時間、計画的な入れ替え、復元後の補正)を1個の接続で確認する |
| `../rustfs/conditional_write.py` | S3 互換サーバーで If-None-Match の応答を実測する |

## 実行

```sh
npm ci --prefix storage/quint
export PATH="$PWD/storage/quint/node_modules/.bin:$PATH"
storage/quint/check.sh          # 1モジュールあたり5万回のランダムシミュレーション
storage/quint/check.sh 5000     # 手早く確かめるとき。回数が少ないと変異が反例に届かず落ちることがあるので、そのときは既定の回数で実行し直す
```

終了コードが 1 なら、次のどれかが起きた。設計どおりのモデル(`seqlog_designed`)で性質が破れた。到達するはずの状態(witness)に1回も届かなかった。守りを外したモデルで、`check.sh` の `breaks` に書いた性質が破れなかった、または `holds` と `log_safe_without_guard` の性質が破れた。

placement の失敗は次の4種類である。

- 設計どおりのモデル(`placement_designed`)か、PostgreSQL の切り替えを含むモデル(`placement_failover`)で性質が破れた。
- `placement_designed` の witness が届かなかった。
- 固定の手順(`quint test`)が失敗した。
- `placement_breaks` か `placement_read_write_breaks` の性質が破れなかった。

反例の手順は `quint run seqlog.qnt --main seqlog_designed --invariant <性質> --mbt` で確認する。placement では、`--init placementInit --step placementStep` を付けて `placement.qnt` を実行する。読み書きの経路の反例は `--step placementReadWriteStep` で確認する。`mbt::actionTaken` に各ステップの操作が出る。

モデルが前提にする条件付き書き込みの応答は、RustFS で次のように実測する。

```sh
docker compose -f storage/rustfs/compose.yaml up -d
AWS_ENDPOINT_URL=http://127.0.0.1:9000 AWS_ACCESS_KEY_ID=demo \
AWS_SECRET_ACCESS_KEY=demo-secret AWS_REGION=us-east-1 \
uv run --with boto3 storage/rustfs/conditional_write.py
```

配置表が前提にする条件付きの UPDATE は、PostgreSQL と PgBouncer で次のように実測する。

```sh
docker compose -f storage/postgres/compose.yaml up -d --wait
uv run --with 'psycopg[binary]' storage/postgres/epoch_race.py
uv run --with 'psycopg[binary]' storage/postgres/claim_rules.py
```

## Assignee の動き

Assignee の守りと理由は `docs/adr/0006-change-log-writes-are-conditional-and-read-back.md` にある。その動作は `seqlog.qnt` の action が記述する。変更ログは Slot 0 から最初の穴までを並べたものとして扱う。

## 検査する性質

| 名前 | 意味 |
| --- | --- |
| `refinesChangeLog` | ストアのどの1ステップも、抽象の仕様の1ステップか「何も起きない」に当たる。ACK したコミットが消える、同じ要求が2回入る、ACK したのに列にない、のどれも反例になる |
| `segmentsNeverChange` | 一度書いた Slot の中身は変わらない。同じ中身での上書きはログを変えないので、`refinesChangeLog` とは別に見る |
| `staleEpochNeverCommits` | Slot の順にエポックが減らない。古いエポックの Assignee は、新しいエポックのあとに確定できない |
| `slotsContiguous` | 最大の Slot より下に穴がない |
| `newestAssigneeSeesAcknowledged` | いちばん新しいエポックを持つ受理中の Assignee は、ACK 済みのコミットをすべて見ている。縛るのはこの Assignee だけで、交代を知らずに受理を続ける古い Assignee は縛らない。`seqlog.qnt` に読み取りの操作はなく、読み取りは `placement.qnt` の `readSeesAcknowledgedAtLookup` が検査する |
| `oneContentPerSlotPerEpoch` | 1つのエポックは1つの Slot に1つの中身しか書かない |
| `dataWriteNeverSeesOlderEpoch` | データの書き込みが 412 のあとに自分より古いエポックを見ることはない |
| `epochStartsOneProcess` | `placement.qnt`。1個のエポックで起動するプロセスは高々1個である |
| `epochNeverDecreases` | `placement.qnt`。配置表のエポックは減少しない |
| `readSeesAcknowledgedAtLookup` | `placement.qnt`。読み取りの応答は、App server が配置表を参照した時点で ACK 済みのコミットをすべて含む。`placementStep` のランダムシミュレーションは ACK 後の読み取りにほぼ届かない(5万回で0回)。そのため、各起動を記録まで済ませてから次のエポックを発行する `placementReadWriteStep` でも検査する。この step は、アドレスの再利用、`Start` の消失、プロセスの停止、切り替えを含まない。それらは `placementStep` が扱う |
| `neverReadsOtherWorkspace` | `placement.qnt`。要求は、ほかのワークスペースの Assignee から応答を受領しない |
| `viewIsLogPrefix` | いちばん新しいエポックを持つ受理中の Assignee の `view`(Rust の `Effect::Committed` で渡した列)は、ログの先頭部分に一致する。ACK 済みの集合では見えない、順序の入れ替わりも反例になる |

`placement.qnt` の `log` で始まる性質(`logRefinesChangeLog` など)は、上の同名の性質を配置のモデルで検査する。

`witness` で始まる4つは、到達するはずの状態に本当に届くかの確認。その状態は、2件の ACK、交代後の Assignee が書いた Slot、自分の再送が先に通ったための 412、古い Assignee の停止である。届かないと、上の性質は何も調べずに通ってしまう。`check.sh` は性質と同じ実行に `--witnesses` で渡し、各 witness に届いたトレースが1個以上あることを確かめる。

固定の手順は3つある。`oldAssigneeTakesSlotFirstTest` は、Assignee の交代の直後に古い Assignee の遅れた PUT が Slot を先に取る場面。フェンスのやり直しで中身を読むのを省く誤りも捕まえる(省いても5万回のランダムシミュレーションでは届かなかった)。`ownResendAcknowledgedTest` は、自分の再送が 412 になり、読んで自分の書き込みと分かって ACK する場面。`pipelineAcknowledgesPastHoleTest` は下の `seqlog_two_in_flight` の場面。

## モデルが置く前提と出典

| 前提 | 出典 |
| --- | --- |
| If-None-Match はキーがないときだけ書き、あれば 412 を返す。同じキーへの同時の条件付き書き込みは、最初に終わった1つだけが成功する | [AWS S3: conditional writes](https://docs.aws.amazon.com/AmazonS3/latest/userguide/conditional-writes.html)。RustFS 1.0.0 でも8台同時×50回で、勝者は常に1台、残りは 412 だった |
| 同時の条件付き書き込みで S3 が返す 409 ConditionalRequestConflict は、結果不明と同じに扱う(読んでから、空なら再送)。モデルでは応答の消失と同じ経路になる | 同上 |
| 書き込みはサーバーが待つのをやめたあとでも適用されうる。応答は失われうる。同じ書き込みの再送は、先に送った分と別に届きうる | nanashi `tessera/docs/persistence.md` |
| Assignee の交代は配置コントローラーがエポックを1つ増やして行う。古い Assignee は交代を知らないまま動き続けうる。再起動した Assignee も、読み込み直す Assignee も、新しいエポックをもらう | 設計ドキュメント「変更ログ」タブ。SlateDB も開くたびに `writer_epoch` を1つ上げる |
| LIST と末尾の読み取りを、1つの操作として扱う。間に新しい Assignee が書いた場合は、フェンスの 412 の経路で止まる | モデルを小さくするための置き換え |

## 守りを外したときに破れる性質

どの変異でどの性質が破れるかは `check.sh` の `breaks` に、破れないものは `holds` にある。2つの守りには、コードから分からない理由がある。

`seqlog_two_in_flight` は、Slot の列へ書くこの方式固有の守り。PUT を pipelining すると、2つの壊れ方がある。1つは、Slot N+1 が N より先に入る場合である。N+1 の batch を ACK したのに、N が空いているので、その batch はログに載っていない(`pipelineAcknowledgesPastHoleTest`)。もう1つは、古い Assignee の2つ目の PUT が新しい Assignee のフェンスより後ろの Slot に入る。前者は5万回のランダムシミュレーションでは届かなかったので、固定の手順で確かめる。また、2つの応答が Slot と逆の順に届くと、両方ともログに入っていても `view` の順がログと食い違う。これは `viewIsLogPrefix` だけが捕まえる。

`seqlog_rejected_as_fenced` は安全性を壊さない。412 で読まずに止まると、自分の書き込みが入っていても ACK しないだけで、ログは壊れない。412 のあとに読むのは、再送のたびに Assignee が止まらないためのもの(活性)で、安全性のためではない。止まった Assignee が確定済みのコミットを ACK しない場面は、固定の手順 `ownResendRejectedTest` で確かめる。

## 入れていないもの

- snapshot、GC、再計算済み番号、Request ID の窓。
- 複数のワークスペースの変更ログ、テナントの混入。キーの命名、ワークスペースごとのデータ鍵、IAM で守る。`placement.qnt` が扱うのは、アドレスの再利用でほかのワークスペースの Assignee へ要求が届く場合だけである。
- 活性(いつか確定する、交代後にいつか書ける)。ランダムシミュレーションは活性を検査しない。
- グループコミットにまとめる件数、ハッシュの鎖、Event ID、3分の打ち切り。

網羅検査ではない。反例が出なかったことは「5万回の試行で見つからなかった」という意味である。

## コードとの対応

Rust の実装は `storage/change_log/` にある。Assignee の状態機械 `Assignee::step` が Quint の action に対応する。`mutations.sh` は `check.sh` と同じ変異を Rust のシミュレーションで検証する。

`storage/change_log/src/assignee/replay.rs` は、`seqlog_replay.qnt` のトレースを `Assignee::step` に再生する。再生には [quint-connect](https://crates.io/crates/quint-connect) を使う。各ステップの後でモデルと状態を比較する。モデルで起きない手順(`two_in_flight` など)は確かめず、`mutations.sh` が捕まえる。`cargo test` で動き、`quint` が PATH に要る。失敗したら表示される `QUINT_SEED=<値>` を付けて再生する。
