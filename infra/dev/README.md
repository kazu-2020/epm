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
- epm-dev の On-Demand の vCPU の上限(Service Quotas の L-1216C47A)。r7i.2xlarge を1台起動するため、8 vCPU 以上にする。

## 測定の流れ

Actions の `bench` を手で起動する。SHA を空にすると、起動するときに選んだブランチの先頭を測る。environment `bench` は main だけに限っているので、ブランチは main を選ぶ。測るコミットは SHA で指定する。

1. `build` ジョブが GitHub のランナーで `compute_engine` の bench をビルドする。このジョブは OIDC トークンを取れない。
2. `measure` ジョブがビルドした実行ファイルを artifact から受け取り、S3 の `bin/` に置く。
3. `measure` ジョブが起動テンプレート `bench`(r7i.2xlarge、Ubuntu 24.04 の最新 AMI、インバウンドなし)で EC2 を1台起動する。user-data は `compute_engine/bench/ec2/measure.sh`。
4. EC2 は1億セルのデータを作り、`recalculate` を hyperfine で5回、`edit` を入力ごとに100回回して、結果を S3 の `results/` に置き、自分で終了する。計測が75分を超えたら打ち切り、そこまでの結果を置いて終了する。
5. `measure` ジョブは結果を取り、実行ページの要約と artifact に残す。S3 のファイルは7日で消える。
