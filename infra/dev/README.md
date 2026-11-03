# infra/dev

AWS アカウント epm-dev(東京リージョン)のリソースを、HCP Terraform のワークスペース `epm-dev` で管理する。今あるのは EC2 での測定(`.github/workflows/bench.yml`)に使うものだけで、名前は `bench` で始まり、`Purpose=bench` のタグが付く。

## 最初に手で行うこと

HCP Terraform が epm-dev に apply するには、HCP Terraform 用の OIDC プロバイダーとロールが先に要る。アカウントを払い出したあと、管理者の権限で次を1回だけ作る。

1. IAM の ID プロバイダーを作る。種類は OpenID Connect、URL は `https://app.terraform.io`、対象者は `aws.workload.identity`。
2. 次の信頼ポリシーでロール `terraform-cloud` を作り、`AdministratorAccess` を付ける。`<account>` は epm-dev のアカウント ID。

   ```json
   {
     "Version": "2012-10-17",
     "Statement": [{
       "Effect": "Allow",
       "Principal": { "Federated": "arn:aws:iam::<account>:oidc-provider/app.terraform.io" },
       "Action": "sts:AssumeRoleWithWebIdentity",
       "Condition": {
         "StringEquals": { "app.terraform.io:aud": "aws.workload.identity" },
         "StringLike": { "app.terraform.io:sub": "organization:matazou_organization:project:*:workspace:epm-dev:run_phase:*" }
       }
     }]
   }
   ```

3. ワークスペース `epm-dev` に環境変数を2つ足す。`TFC_AWS_PROVIDER_AUTH` は `true`、`TFC_AWS_RUN_ROLE_ARN` は手順2のロールの ARN。

## apply のあとに手で行うこと

GitHub の environment `bench`(Settings → Environments)に変数を2つ足す。値は apply の出力にある。

- `AWS_ROLE_ARN`:`bench_role_arn` の値
- `BENCH_BUCKET`:`bench_bucket` の値

## 測定の流れ

Actions の `bench` を手で起動する。SHA を空にすると main の先頭を測る。

1. GitHub のランナーで `compute_engine` の bench をビルドし、実行ファイルを S3 の `bin/` に置く。
2. 起動テンプレート `bench`(r7i.2xlarge、Ubuntu 24.04 の最新 AMI、インバウンドなし)で EC2 を1台起動する。user-data は `compute_engine/bench/ec2/measure.sh`。
3. EC2 は1億セルのデータを作り、`recalculate` を hyperfine で5回、`edit` を入力ごとに100回回して、結果を S3 の `results/` に置き、自分で終了する。90分たっても終わらない場合も自分で終了する。
4. ランナーは結果を取り、実行ページの要約と artifact に残す。S3 のファイルは7日で消える。
