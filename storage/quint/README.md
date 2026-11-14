# ストレージ層の Quint モデル

変更ログの S3 への書き方が、担当の交代、クラッシュ、応答の消失、再送が重なっても壊れないかを確かめるモデル。変更ログは、`<ws>/log/<Slot>` に Slot の順に並ぶセグメントの列である。担当は次の Slot に If-None-Match で PUT し、200 なら ACK する。

範囲は「ログとフェンシング」だけにした。スナップショット、GC、複数のワークスペースは入れていない。

## ファイル

| ファイル | 中身 |
| --- | --- |
| `change_log.qnt` | 抽象の仕様。ワークスペースごとに追記だけの列が1本あり、1つの Request ID は高々1回だけ足され、ACK するのは列に入ったものだけ |
| `seqlog.qnt` | 変更ログの書き方のモデル。担当、配置コントローラー(エポックを渡す)、Slot、クライアントの再送。守りは `seqlog_design` の `allGuards` に、サーバー数などの上限は `seqlog` の `pure val` に置く |
| `seqlog_mutations.qnt` | 守りを1つずつ外したモデル(`{ ...allGuards, deduplication: false }` など)。検査が実際に落ちることを示す。`check.sh` は、ここのモジュールがすべて `breaks` か `holds` に入っているかを確かめる |
| `seqlog_replay.qnt` | Rust の実装に再生するトレースを作るための `replayStep`。`step` の一部の手順だけを許し、送信と停止にステップを使い切らないようにした |
| `check.sh` | すべての検査をまとめて実行する |
| `../rustfs/conditional_write.py` | S3 互換サーバーで If-None-Match の応答を実測する |

## 実行

```sh
npm ci --prefix storage/quint
export PATH="$PWD/storage/quint/node_modules/.bin:$PATH"
storage/quint/check.sh          # 1モジュールあたり5万回のランダムシミュレーション
storage/quint/check.sh 5000     # 手早く確かめるとき。回数が少ないと変異が反例に届かず落ちることがあるので、そのときは既定の回数で実行し直す
```

終了コードが 1 なら、次のどれかが起きた。設計どおりのモデル(`seqlog_designed`)で性質が破れた。到達するはずの状態に届かなかった。守りを外したモデルで、`check.sh` の `breaks` に書いた性質が破れなかった、または `holds` と `log_safe_without_guard` の性質が破れた。反例の手順は `quint run seqlog.qnt --main seqlog_designed --invariant <性質> --mbt` で確認する。`mbt::actionTaken` に各ステップの操作が出る。

モデルが前提にする条件付き書き込みの応答は、RustFS で次のように実測する。

```sh
docker compose -f storage/rustfs/compose.yaml up -d
AWS_ENDPOINT_URL=http://127.0.0.1:9000 AWS_ACCESS_KEY_ID=demo \
AWS_SECRET_ACCESS_KEY=demo-secret AWS_REGION=us-east-1 \
uv run --with boto3 storage/rustfs/conditional_write.py
```

## 担当の動き

担当の守りと理由は `docs/adr/0006-change-log-writes-are-conditional-and-read-back.md` にある。その動作は `seqlog.qnt` の action が記述する。変更ログは Slot 0 から最初の穴までを並べたものとして扱う。

## 検査する性質

| 名前 | 意味 |
| --- | --- |
| `refinesChangeLog` | ストアのどの1ステップも、抽象の仕様の1ステップか「何も起きない」に当たる。ACK したコミットが消える、同じ要求が2回入る、ACK したのに列にない、のどれも反例になる |
| `segmentsNeverChange` | 一度書いた Slot の中身は変わらない。同じ中身での上書きはログを変えないので、`refinesChangeLog` とは別に見る |
| `staleEpochNeverCommits` | Slot の順にエポックが減らない。古いエポックの担当は、新しいエポックのあとに確定できない |
| `slotsContiguous` | 最大の Slot より下に穴がない |
| `newestOwnerSeesAcknowledged` | いちばん新しいエポックを持つ受理中の担当は、ACK 済みのコミットをすべて見ている。縛るのはこの担当だけで、交代を知らずに受理を続ける古い担当は縛らない。読み取りの操作もモデルにない |
| `oneContentPerSlotPerEpoch` | 1つのエポックは1つの Slot に1つの中身しか書かない |
| `dataWriteNeverSeesOlderEpoch` | データの書き込みが 412 のあとに自分より古いエポックを見ることはない |
| `viewIsLogPrefix` | いちばん新しいエポックを持つ受理中の担当の `view`(Rust の `Effect::Committed` で渡した列)は、ログの先頭部分に一致する。ACK 済みの集合では見えない、順序の入れ替わりも反例になる |

`witness` で始まる4つは、到達するはずの状態(2件の ACK、担当交代後の担当が書いた Slot、自分の再送が先に通ったための 412、古い担当の停止)に本当に届くかの確認。届かないと、上の性質は何も調べずに通ってしまう。

固定の手順は3つある。`oldOwnerTakesSlotFirstTest` は、担当の交代の直後に古い担当の遅れた PUT が Slot を先に取る場面。フェンスのやり直しで中身を読むのを省く誤りも捕まえる(省いても5万回のランダムシミュレーションでは届かなかった)。`ownResendAcknowledgedTest` は、自分の再送が 412 になり、読んで自分の書き込みと分かって ACK する場面。`pipelineAcknowledgesPastHoleTest` は下の `seqlog_two_in_flight` の場面。

## モデルが置く前提と出典

| 前提 | 出典 |
| --- | --- |
| If-None-Match はキーがないときだけ書き、あれば 412 を返す。同じキーへの同時の条件付き書き込みは、最初に終わった1つだけが成功する | [AWS S3: conditional writes](https://docs.aws.amazon.com/AmazonS3/latest/userguide/conditional-writes.html)。RustFS 1.0.0 でも8台同時×50回で、勝者は常に1台、残りは 412 だった |
| 同時の条件付き書き込みで S3 が返す 409 ConditionalRequestConflict は、結果不明と同じに扱う(読んでから、空なら再送)。モデルでは応答の消失と同じ経路になる | 同上 |
| 書き込みはサーバーが待つのをやめたあとでも適用されうる。応答は失われうる。同じ書き込みの再送は、先に送った分と別に届きうる | nanashi `tessera/docs/persistence.md` |
| 担当の交代は配置コントローラーがエポックを1つ増やして行う。古い担当は交代を知らないまま動き続けうる。再起動した担当も、読み込み直す担当も、新しいエポックをもらう | 設計ドキュメント「変更ログ」タブ。SlateDB も開くたびに `writer_epoch` を1つ上げる |
| LIST と末尾の読み取りを、1つの操作として扱う。間に新しい担当が書いた場合は、フェンスの 412 の経路で止まる | モデルを小さくするための置き換え |

## 守りを外したときに破れる性質

どの変異でどの性質が破れるかは `check.sh` の `breaks` に、破れないものは `holds` にある。2つの守りには、コードから分からない理由がある。

`seqlog_two_in_flight` は、Slot の列へ書くこの方式固有の守り。PUT を pipelining すると、2つの壊れ方がある。1つは、Slot N+1 が N より先に入る場合である。N+1 の batch を ACK したのに、N が空いているので、その batch はログに載っていない(`pipelineAcknowledgesPastHoleTest`)。もう1つは、古い担当の2つ目の PUT が新しい担当のフェンスより後ろの Slot に入る。前者は5万回のランダムシミュレーションでは届かなかったので、固定の手順で確かめる。また、2つの応答が Slot と逆の順に届くと、両方ともログに入っていても `view` の順がログと食い違う。これは `viewIsLogPrefix` だけが捕まえる。

`seqlog_rejected_as_fenced` は安全性を壊さない。412 で読まずに止まると、自分の書き込みが入っていても ACK しないだけで、ログは壊れない。412 のあとに読むのは、再送のたびに担当が止まらないためのもの(活性)で、安全性のためではない。止まった担当が確定済みのコミットを ACK しない場面は、固定の手順 `ownResendRejectedTest` で確かめる。

## 入れていないもの

- スナップショット、GC、再計算済み番号、Request ID の窓。
- 複数のワークスペース、テナントの混入。キーの命名、ワークスペースごとのデータ鍵、IAM で守る。
- 活性(いつか確定する、交代後にいつか書ける)。ランダムシミュレーションは活性を検査しない。
- グループコミットにまとめる件数、ハッシュの鎖、Event ID、3分の打ち切り。

網羅検査ではない。反例が出なかったことは「5万回の試行で見つからなかった」という意味である。

## コードとの対応

Rust の実装は `storage/change_log/` にある。担当の状態機械 `Owner::step` が Quint の action に対応する。`mutations.sh` は `check.sh` と同じ変異を Rust のシミュレーションで検証する。

`storage/change_log/src/owner/replay.rs` は、`seqlog_replay.qnt` のトレースを `Owner::step` に再生する。再生には [quint-connect](https://crates.io/crates/quint-connect) を使う。各ステップの後でモデルと状態を比較する。モデルで起きない手順(`two_in_flight` など)は確かめず、`mutations.sh` が捕まえる。`cargo test` で動き、`quint` が PATH に要る。失敗したら表示される `QUINT_SEED=<値>` を付けて再生する。
