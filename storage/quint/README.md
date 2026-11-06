# ストレージ層の Quint モデル

変更ログを S3 に置く設計(設計ドキュメント「変更ログ」タブの「4. 保存の形」)が、担当サーバーの交代、クラッシュ、応答の消失、再送が重なっても壊れないかを確かめるモデル。比較のために、変更ログの正本を PostgreSQL に置く案も同じモデルで扱う。

範囲はまず「ログとフェンシング」だけにした。スナップショット、GC、複数のワークスペースは入れていない。

## ファイル

| ファイル | 中身 |
| --- | --- |
| `change_log.qnt` | 抽象の仕様。ワークスペースごとに追記だけの列が1本あり、1つの Request ID は高々1回だけ足され、ACK するのは列に入ったものだけ |
| `storage.qnt` | 実装のモデル。担当サーバー、配置コントローラー(エポックを渡す)、セグメント、マニフェスト、クライアントの再送。S3 と PostgreSQL の違い3つ(セグメントをコミットの前に書くか中で書くか、コミットでリースを検査するか、新しい担当がフェンスを書くか)を `design` モジュールの `S3` と `PostgreSQL` に、守りを `allGuards` に置く。サーバー数などの上限もここに1か所だけ書く |
| `mutations.qnt` | 守りを1つずつ外したモデル(`{ ...allGuards, deduplication: false }` など)。検査が実際に落ちることを示す。`check.sh` は、ここのモジュールがすべて `breaks`、`holds`、`scenarios` のどれかに入っているかを確かめる |
| `seqlog.qnt` | 方式 A のモデル。マニフェストを持たず、コミットごとに次の連番の位置を If-None-Match で取る。守りは `seqlog_design` の `allGuards` に置く |
| `seqlog_mutations.qnt` | 方式 A の守りを1つずつ外したモデル |
| `check.sh` | すべての検査をまとめて実行する |
| `../rustfs/conditional_write.py` | S3 互換サーバーで条件付き書き込みの応答を実測する |

## 実行

```sh
npm install -g @informalsystems/quint@0.33.1
storage/quint/check.sh          # 1モジュールあたり5万回のランダムシミュレーション。4コアで約6分
storage/quint/check.sh 5000     # 手早く確かめるとき。回数が少ないと変異が反例に届かず落ちることがあるので、そのときは既定の回数で実行し直す
```

終了コードが 1 なら、次のどれかが起きた。設計どおりのモデル(`s3`、`postgres`)で性質が破れた。到達するはずの状態に届かなかった。守りを外したモデルで、`check.sh` の `breaks` に書いた性質が破れなかった、または `holds` と `log_safe_without_guard` の性質が破れた。反例の手順は `quint run storage.qnt --main s3 --invariant <性質> --mbt` で見る(`mbt::actionTaken` に各ステップの操作が出る)。

モデルが前提にする条件付き書き込みの応答は、RustFS で次のように実測する。

```sh
AWS_ENDPOINT_URL=http://127.0.0.1:59000 AWS_ACCESS_KEY_ID=nanashi \
AWS_SECRET_ACCESS_KEY=nanashi-secret AWS_REGION=us-east-1 \
uv run --with boto3 storage/rustfs/conditional_write.py
```

## 検査する性質

| 名前 | 意味 |
| --- | --- |
| `refinesChangeLog` | ストアのどの1ステップも、抽象の仕様の1ステップか「何も起きない」に当たる。ACK したコミットが消える、同じ要求が2回入る、ACK したのに列にない、のどれも反例になる |
| `epochNeverDecreases` | マニフェストのエポックは減らない |
| `staleEpochNeverCommits` | マニフェストが指すセグメントのエポックは、並びの順に減らず、マニフェストのエポックを超えない。古いエポックの担当は、新しいエポックのあとに確定できない |
| `segmentsNeverChange` | 一度書いたセグメントの中身は変わらない。同じ中身での上書きはログを変えないので、`refinesChangeLog` とは別に見る |
| `manifestSegmentsExist` | マニフェストが指すセグメントはすべて存在する |
| `newestOwnerSeesAcknowledged` | いちばん新しいエポックを持つ受け付け中の担当は、ACK 済みのコミットをすべて見ている。縛るのはこの担当だけで、交代を知らずに受け付けを続ける古い担当は縛らない。読み取りの操作もモデルにない |

`witness` で始まる4つは、到達するはずの状態(2件の ACK、担当交代後の担当が書いたセグメント、自分の再送が先に通ったための 412、古い担当の停止)に本当に届くかの確認。届かないと、上の性質は何も調べずに通ってしまう。実際に PostgreSQL 案の誤りで1件もコミットできない状態を、この確認で見つけた。

## モデルが置く前提と出典

| 前提 | 出典 |
| --- | --- |
| If-Match は ETag が一致したときだけ書き、違えば 412 を返す。同じキーへの同時の条件付き書き込みは、最初に終わった1つだけが成功する | [AWS S3: conditional writes](https://docs.aws.amazon.com/AmazonS3/latest/userguide/conditional-writes.html)。RustFS 1.0.0 でも8台同時×50回で、勝者は常に1台、残りは 412 だった |
| マニフェストは最初から空の中身で存在し、If-Match で書ける。実際の S3 では最初の1回だけ `If-None-Match: *` で作る。意味は変わらない | モデルを小さくするための置き換え |
| ETag は中身で決まるので、マニフェストの中身が同じなら ETag も同じ。モデルでは If-Match をマニフェスト全体の比較で表す | 同上。マニフェストは毎回エポックかセグメントの一覧が変わるので、同じ中身に戻ることはない |
| 書き込みはサーバーが待つのをやめたあとでも適用されうる。応答は失われうる。同じ書き込みの再送は、先に送った分と別に届きうる | nanashi `tessera/docs/persistence.md`(PostgreSQL で接続が切れたあとに COMMIT が終わる例)。成功後に同じ ETag で再試行すると 412 になることは RustFS で確認した |
| 1つの担当は、同じセグメントのキーに別の中身を書かない。再起動した担当は、前と同じエポックを使わない(配置コントローラーから新しいエポックをもらう) | 設計の約束。モデルの担当は結果が分からなければ待ち続け、`segmentsNeverChange` でキーを使い回さないことを確かめた |
| 担当の交代は配置コントローラーがエポックを1つ増やして行う。古い担当は交代を知らないまま動き続けうる | 設計ドキュメント「変更ログ」タブ(エポック、システムのログ)。SlateDB の manifest RFC(writer epoch) |
| PostgreSQL 案では、ログの追記とリースのエポックの検査を1つのトランザクションで行う | nanashi の `PgJournal`(世代番号とリース) |
| 同時の条件付き書き込みで S3 が返す 409 ConditionalRequestConflict は、結果不明と同じに扱う(HEAD してから、空なら再送)。モデルでは応答の消失と同じ経路になる | [AWS S3: conditional writes](https://docs.aws.amazon.com/AmazonS3/latest/userguide/conditional-writes.html) |

## 結果(seed 1、5万回、最大30ステップ)

守りを外したときに破れる性質を書く。`check.sh` はこの性質だけを指定して実行し、破れることを確かめる。ほかの性質も破れている場合がある。「破れず」はすべての性質を指定して5万回で反例が見つからなかったという意味。

| 外した守り(モジュール名の後半、変える値) | S3 案 | PostgreSQL 案 |
| --- | --- | --- |
| なし(設計どおり) | 破れず | 破れず |
| マニフェストの If-Match、PostgreSQL では先頭の Commit Sequence の検査(`blind_write`、`manifestCondition`) | `refinesChangeLog` | `refinesChangeLog`(固定の手順で確認) |
| 結果不明を成功とみなさない(`unknown_as_success`、`readsBackUnknown`) | `refinesChangeLog` | `refinesChangeLog` |
| Request ID による重複排除(`no_deduplication`、`deduplication`) | `refinesChangeLog` | `refinesChangeLog` |
| セグメントのキーにエポックを入れる(`segment_key_without_epoch`、`epochInSegmentKey`) | `refinesChangeLog`(固定の手順で確認) | 対象外(行はコミットと同じトランザクションで入る) |
| 新しい担当がまずマニフェストを1回書く(`no_fence`、`S3` の `newOwnerWritesFence`) | `newestOwnerSeesAcknowledged` | 対象外(フェンスの書き込みがもともとない) |
| 読んだマニフェストのエポックが自分より新しければ止まる(`no_epoch_check`、`epochCheck`) | `staleEpochNeverCommits` | 破れず |

到達確認の4つは、両方の案で届いた。PostgreSQL 案の `blind_write` と S3 案の `segment_key_without_epoch` は、ランダムシミュレーションでは seed やモデルの書き方によって届いたり届かなかったりしたので、`mutations.qnt` の固定の手順(`lateResendRollsBackTest`、`oldOwnerOverwritesTest`)で確かめる。

## 方式 A(`seqlog.qnt`)

変更ログを `<ws>/log/<連番>` の位置の列として持つ。担当は次の位置に If-None-Match で PUT し、200 なら ACK する。グループコミットでは、1回の PUT に batch 全体を入れる。エポックはキーではなく中身(実際の S3 では metadata と中身の先頭)に入れる。新しい担当は LIST で末尾を探し、末尾の位置を HEAD してエポックが自分より新しければ止まる。そうでなければ、その次の位置に空のフェンスを置く。位置の順にエポックは減らないので、末尾のエポックを見れば足りる。412 を受けたら、その位置を HEAD でエポックを読む。自分のエポックなら自分の書き込みとして成功、自分より新しいエポックなら止まる、自分より古いエポックはフェンスのときだけ起こりうる。そのときは、取られた位置を GET して中身を自分の view と重複排除の集合に足してから、次の位置でフェンスをやり直す。HEAD だけでやり直すと、古い担当の最後のコミットを見ないまま受け付けを始める。データの書き込みで古いエポックを見たら止まる。1つのエポックの担当は1つで、1つの位置には1回しか書かない(再送は同じ中身)ので、エポックが同じなら自分の書き込みと言える。この前提は性質 `oneContentPerSlotPerEpoch` で確かめる。実装では、エポックと write-id を metadata に入れて HEAD で比べ、エポックが同じで write-id が違えば止めて alarm を出す。重複排除の集合は、確定したログの Request ID と、PUT 中(ACK 前)の Request ID の和。flush で batch を空にするので、PUT 中のものを入れないと同じ Request ID が次の位置にもう1回入る。変更ログは位置 0 から最初の穴までを並べたもの。

S3 の SDK や `object_store` crate が 5xx のあとに自動で再送し、1回目が入っていた場合も、この「412 のあとの HEAD で自分の書き込みと分かる」経路になる(`witnessNoOwnResendRejected` で届くことを確かめる)。

性質は、`refinesChangeLog`、`segmentsNeverChange`(一度書いた位置の中身は変わらない)、`newestOwnerSeesAcknowledged` を残し、`staleEpochNeverCommits` を「位置の順にエポックが減らない」に書き換え、`slotsContiguous`(最大の位置より下に穴がない)、`oneContentPerSlotPerEpoch`(1つのエポックは1つの位置に1つの中身しか書かない)、`dataWriteNeverSeesOlderEpoch`(データの書き込みが 412 のあとに自分より古いエポックを見ることはない。Rust ではここを alarm にする)を足した。マニフェストがないので `epochNeverDecreases` と `manifestSegmentsExist` はない。到達確認の4つは S3 案と同じ意味に写した。担当の交代の直後に古い担当の遅れた PUT が位置を先に取る場面は、固定の手順 `oldOwnerTakesSlotFirstTest` で確かめる。この手順は、フェンスのやり直しで GET を省く誤りも捕まえる(省いても5万回のランダムシミュレーションでは届かなかった)。自分の再送が 412 になり、HEAD で自分の書き込みと分かって ACK する場面は `ownResendAcknowledgedTest` で確かめる。

結果(seed 1、5万回、最大30ステップ):

| 外した守り(モジュール名、変える値) | 破れる性質 |
| --- | --- |
| なし(`seqlog_designed`) | 破れず。到達確認の4つは届いた |
| If-None-Match(`seqlog_blind_put`、`ifNoneMatch`) | `refinesChangeLog`、`segmentsNeverChange` |
| 結果不明を成功とみなさない(`seqlog_unknown_as_success`、`readsBackUnknown`) | `refinesChangeLog` |
| Request ID による重複排除(`seqlog_no_deduplication`、`deduplication`) | `refinesChangeLog` |
| PUT 中の Request ID も重複とみなす(`seqlog_no_inflight_deduplication`、`inFlightDeduplication`) | `refinesChangeLog` |
| 新しい担当がフェンスを置く(`seqlog_no_fence`、`fence`) | `newestOwnerSeesAcknowledged`。ログは壊れない |
| 起動時に末尾のエポックが自分より新しければ止まる(`seqlog_no_epoch_check`、`epochCheck`) | `staleEpochNeverCommits`。ログは壊れない |
| 1つの担当が同時に送る PUT は1つ(`seqlog_two_in_flight`、`oneInFlight`) | `refinesChangeLog`(固定の手順で確認)、`staleEpochNeverCommits`、`slotsContiguous` |
| 412 のあとに HEAD する(`seqlog_rejected_as_fenced`、`headsAfterRejection`) | 破れず |

`seqlog_two_in_flight` が方式 A に固有の守り。PUT を pipelining すると、2つの壊れ方がある。1つは、位置 N+1 が N より先に入り、N+1 の batch を ACK したのに、N が空いているのでログに載っていない(`pipelineAcknowledgesPastHoleTest`)。もう1つは、古い担当の2つ目の PUT が新しい担当のフェンスより後ろの位置に入る。前者は5万回のランダムシミュレーションでは届かなかったので、固定の手順で確かめる。`slotsContiguous` の反例も、5,000回では届かず5万回で届く。「同時に送る PUT は1つ」はコードの不変条件として書く。

`seqlog_rejected_as_fenced` は安全性を壊さない。412 で HEAD せずに止まると、自分の書き込みが入っていても ACK しないだけで、ログは壊れない。HEAD は、再送のたびに担当が止まらないためのもの(活性)で、安全性のためではない。止まった担当が確定済みのコミットを ACK しない場面は、固定の手順 `ownResendRejectedTest` で確かめる。

## 分かったこと

モデルから決めた守りと、その理由は `docs/adr/0006-change-log-writes-are-conditional-and-read-back.md` に置く。

## 入れていないもの

- スナップショット、GC、Computed Sequence(次のステップ)。
- 複数のワークスペース、テナントの混入。キーの命名、ワークスペースごとのデータ鍵、IAM で守る。
- 活性(いつか確定する、交代後にいつか書ける)。Apalache もランダムシミュレーションも活性を検査しない。
- グループコミットにまとめる件数、ハッシュの鎖、Event ID、3分の打ち切り。
- S3 の LIST。S3 案のモデルの担当はマニフェストだけを読み、セグメントを一覧から拾わない。方式 A のモデルは LIST と末尾の HEAD を、1つの操作として扱う。間に新しい担当が書いた場合は、フェンスの 412 の経路で止まる。
- 方式 A のスナップショット、`covers_seq`、Request ID の窓、GC。

網羅検査ではない。TLC で今の上限(サーバー2台、要求2つ、エポック3、再送1回)を網羅しようとしたところ、20分で4,000万状態を超えて終わらなかった。Apalache の上限つき検査(10ステップ)は5分半かかり、10ステップでは届かない反例がある。このため、ランダムシミュレーションを主に使う。反例が出なかったことは「5万回の試行で見つからなかった」という意味である。

## コードとの対応

ストレージ層のコードはまだない。モデルは設計ドキュメント(2026-11-03 時点の「変更ログ」タブと「計算エンジン」タブ)に対応する。コードができたら、`/pstack:create-formal-verification-skill` でモデルとコードを結びつける。
