# infra/dev

AWS アカウント epm-dev(東京リージョン)のリソースを、HCP Terraform のワークスペース `epm-dev` で管理する。今あるのは EC2 での測定(`.github/workflows/bench.yml`)に使うものだけで、名前は `bench` で始まり、`Purpose=bench` のタグが付く。

## Terraform の外にあるもの

次のものは、このディレクトリのコードが前提にしているが、Terraform では管理していない。アカウントを作り直すときや、ほかのアカウントに同じ構成を作るときは、先に手で作る。

- IAM の Identity provider `app.terraform.io`。Audience は `aws.workload.identity`。
- IAM ロール `terraform-cloud`。`AdministratorAccess` を付け、信頼ポリシーで次の `sub` だけを許す。plan と apply で同じロールを使う。

  ```text
  organization:matazou_organization:project:*:workspace:epm-dev:run_phase:*
  ```

- HCP Terraform のワークスペース `epm-dev` の環境変数。`TFC_AWS_PROVIDER_AUTH` は `true`、`TFC_AWS_RUN_ROLE_ARN` は `terraform-cloud` の ARN。
- GitHub の environment `bench`。Deployment branches は `main` だけに限る。変数 `AWS_ROLE_ARN` には apply の出力の `bench_role_arn`、`BENCH_BUCKET` には `bench_bucket` を入れる。
- epm-dev の On-Demand の vCPU の上限(Service Quotas の L-1216C47A)。r7i.16xlarge を1台起動するため、64 vCPU 以上にする。

## 測定の流れ

Actions の `bench` を手で起動する。SHA を空にすると、起動するときに選んだブランチの先頭を測る。environment `bench` は main だけに限っているので、ブランチは main を選ぶ。測るコミットは SHA で指定する。

1. `build` ジョブが GitHub のランナーで `compute_engine` の bench をビルドする。このジョブは OIDC トークンを取れない。
2. `measure` ジョブがビルドした実行ファイルを artifact から受け取り、S3 の `bin/` に置く。
3. `measure` ジョブが起動テンプレート `bench`(Ubuntu 24.04 の最新 AMI、インバウンドなし)で EC2 を1台起動する。インスタンスは入力で r7i.2xlarge(既定)か r7i.16xlarge を選ぶ。ディスクの大きさも入力で変える。user-data は共通の前半 `.github/ec2/prelude.sh` と `compute_engine/bench/ec2/measure.sh` をつないだもの。
4. EC2 は入力の規模(代表モデルの数量のセルの倍数、1倍で全体約1億セル)ごとにデータを作る。1倍では `recalculate` を hyperfine で5回、`edit` を入力ごとに100回回す。どの規模でも、物理コアに固定した 1〜32 スレッドで `recalculate` を回し、入力 `baseline` があればそのコミットも同じ順に交互に回す。STREAM と `perf` の結果も置く。結果は S3 の `results/` に置き、自分で終了する。時間の上限は、計測の `timeout`(入力の分数 − 15分から準備にかかった時間を引いた分)< EC2 の `shutdown`(入力の分数、30〜140分)< 結果を待つステップ(150分)< `measure` ジョブ(170分)< ロールのセッション(180分)の順にする。どれかを変えるときは、この順を保つ。
   `sha` には PR #44 以降のコミットを指定する。`stream.c` と、`generate` の倍率の引数をそのコミットから使うため。`baseline` は `recalculate` だけを使うので、古いコミットでもよい。ただし古い `recalculate` は、`sha` の `generate` が書いた Arrow ファイルを自分の datagen の定義で読む。メジャーの ULID、ディメンション、数式、Arrow IPC の形が両方のコミットで同じときだけ動く。
5. `measure` ジョブは結果を取り、実行ページの要約と artifact に残す。S3 のファイルは7日で消える。

## EC2 での測定の共通の手順

`bench` と `s3-latency` の EC2 を使うジョブは、どちらも再利用 workflow `.github/workflows/ec2.yml` を呼ぶ。このジョブは S3 の `bin/` へのファイルの配置、user-data の組み立て、起動、結果待ち、終了、要約と artifact を受け持つ。EC2 の前半(`shutdown`、結果を S3 に置く `finish`、AWS CLI の準備、`bin/` の取得、shutdown までの残り時間の計算)は `.github/ec2/prelude.sh` にまとめ、測定ごとのスクリプトはその続きだけを書く。`ec2.yml` は、入力の分数が30分以上で結果を待つ時間より10分以上短いこと、結果を待つ時間 < ジョブの時間 < 180分(ロールのセッション)であることを確かめる。3つ目の測定を足すときは、この workflow を呼ぶ。

## S3 のレイテンシの測定

Actions の `s3-latency` を main で手で起動する。変更ログが使う S3 の要求を、東京の EC2 から測る。`storage/s3/latency.py` を同じ起動テンプレートの EC2 で動かし、バケットの `latency/` に書いて読む。結果は `bench` と同じく S3 の `results/` に置き、実行ページの要約と artifact に残す。

- 測るもの: 新しいキーへの If-None-Match の PUT(1KB・100KB・10MB、各1,000回)と GET、4KB の PUT を毎秒1回と10回で各5分、既にあるキーへの PUT の 412 とそのあとの HEAD、8 writer が同じキーに同時に書いたときの結果、`start-after` 付きの LIST(1,000 と 10,000 キー)、1GB と 10GB の range GET の速さ。
- SDK の再試行は切る。小さい要求の1回の timeout は1秒にする。
- 時間の上限は、測定の `timeout`(入力の分数 − 10分)< EC2 の `shutdown`(入力の分数、30〜90分)< 結果を待つステップ(100分)< ジョブ(120分)の順にする。

スクリプトだけを手元の RustFS で確かめるときは次のように動かす。数字は参考にならない。

```sh
AWS_ENDPOINT_URL=http://127.0.0.1:59000 AWS_ACCESS_KEY_ID=nanashi \
AWS_SECRET_ACCESS_KEY=nanashi-secret AWS_REGION=us-east-1 \
uv run --with boto3 storage/s3/latency.py <bucket> <prefix> <out dir> --quick
```

## 変更ログの S3 テスト

Actions の `storage-change-log` を main で手で起動すると、`aws` ジョブが `storage/change_log/tests/s3.rs` を東京のバケットに対して流す。EC2 は使わず、GitHub のランナーから直接 S3 を読み書きする。キーは `change-log-test/<実行番号>-<試行>/` の下に置き、ほかのファイルと同じく7日で消える。

ロール `bench-github` には、`change-log-test/*` の `s3:PutObject` と `s3:GetObject` と、そのプレフィックスでの `s3:ListBucket` が要る(`bench.tf`)。apply する前に起動すると、テストは 403 で失敗する。
