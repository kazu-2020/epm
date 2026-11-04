# epm のセキュリティルール

予実管理（EPM/FP&A）SaaS の PoC。計算エンジン（`compute_engine/`、Rust）、ストレージの検証（`storage/`）、ベンチ用の AWS（`infra/dev/`、`.github/workflows/bench.yml`）がある。

## GitHub Actions と AWS

- OIDC の `id-token: write` は `bench.yml` の `measure` ジョブだけに置く。任意の SHA をビルドする `build` ジョブに置かない（ビルドスクリプトがトークンに触れられないようにするため）。
- `measure` ジョブは `environment: bench`（main 限定）で動かす。environment を外す変更、`pull_request` などの手動以外のトリガーを足す変更は指摘する。
- `bench-github` ロールの信頼ポリシーの `sub` は `local.github_environment`（owner/repository ID 付きの形）と完全一致させる。`StringLike` やワイルドカードに緩める変更は指摘する。
- IAM は最小権限。`Resource = "*"` は `ec2:DescribeInstances` だけ（リソースレベル権限がないため）。ほかで `*` や `Action = "*"` を足す変更は指摘する。
- `ec2:RunInstances` は起動テンプレート `bench` とインスタンスタイプの条件付き、`ec2:TerminateInstances` は `Purpose = bench` タグの条件付きに保つ。
- ベンチのインスタンスにインバウンドの穴を開けない（セキュリティグループに ingress、SSH 鍵、IMDSv1）。`http_tokens = "required"` と EBS の暗号化を保つ。
- S3 バケットの public access block は全部オンに保つ。インスタンスのロールは `bin/*` の読み取りと `results/*` の書き込みだけ。
- サードパーティのアクションはコミット SHA で固定する（タグ参照にしない）。`actions/checkout` は `persist-credentials: false`。
- `${{ inputs.* }}` や `${{ github.event.* }}` を `run:` のシェルに直接埋め込まない（インジェクション）。`env:` で渡す。

## 秘密情報

- AWS のアカウント ID、ロール ARN、GitHub の owner/repository ID は秘密ではない（既にコミット済み）。指摘しない。
- `storage/rustfs/` の `nanashi` / `nanashi-secret` はローカルの RustFS（開発用 compose）の既定の資格情報。指摘しない。
- それ以外のアクセスキー、トークン、Terraform Cloud の資格情報、`*.tfstate` をコミットする変更は指摘する。

## 計算エンジン（将来の多テナント SaaS の中心）

- 数式は利用者の入力。パーサーとコンパイラーでパニックしない（`unwrap`/`expect`/添字の範囲外をユーザー入力の経路に置かない）。エラーは `Result` で返す。
- 数式の木の高さの上限 128 を保つ（スタックあふれ対策）。上限を外す、または再帰の深さを入力で決められるようにする変更は指摘する。
- セル数、メンバー数、キーのビット幅（64 ビット）の上限検査を外す変更は指摘する。入力で決まる大きさのメモリ確保は事前に上限を検査する。
- 外から入る値（入力、取り込み）の NaN/Inf は空、-0 は 0 にしてから保存する。この正規化を経ない入口は指摘する（エンジン内部で空の印に NaN を使うのは問題ない）。
- `unsafe` を足す変更は、安全である理由のコメントがなければ指摘する。

## ストレージ（変更ログと S3）

- 変更ログへの書き込みは条件付き書き込み（`If-None-Match` / `If-Match`）とエポックでフェンシングする。条件なしの上書き（PUT）で変更ログやマニフェストを書く変更は指摘する。
- 変更ログは消さない。過去のエントリを削除・書き換えする経路は指摘する。

## 将来の多テナント化で守ること

- テナントとワークスペースの境界をまたいでデータを読む経路は、テナント ID の検査なしに書かない。
- ワークスペースの ID やメンバーの ULID は外部から来る。存在と所有者を確かめずに使わない（IDOR）。
