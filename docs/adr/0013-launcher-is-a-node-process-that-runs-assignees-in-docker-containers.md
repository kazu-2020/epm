# ランチャーはノードごとの独立したプロセスで、Assignee を Docker のコンテナで起動し、状態を Docker に保存する

- Status: Accepted
- Date: 2026-11-20
- Related: #196、#263

Assignee を App server の子プロセスにすると、App server の更新のたびに全ワークスペースが停止からの起動になる。この問題に対し、ノードごとの独立したランチャーが Assignee を1個ずつ Docker のコンテナで起動し、管理の状態を Docker に保存する方式を採用した。App server 内で子プロセスを起動する案と、App server 内から Docker を操作する案は不採用とした。App server とランチャーの再起動で Assignee は停止しない。一方、オンプレ環境での Docker の必須化と、App server の再起動による残存の回収の消失を受け入れた。

## Context

[ADR 0012](0012-placement-table-issues-epochs-and-a-process-serves-one-epoch.md) は、[配置表](../../GLOSSARY.md)(PostgreSQL のワークスペースごとの1行)が[エポック](../../GLOSSARY.md)を発行し、1個の [Assignee](../../GLOSSARY.md) のプロセスは1個のエポックだけを処理する、と決定した。App server はエポックを発行してから、[ノード](../../GLOSSARY.md)の[ランチャー](../../GLOSSARY.md)に Assignee の起動を依頼し、準備の確認の後にアドレスを配置表に記録する。ランチャーは PostgreSQL と S3 のどちらにも接続しない。PostgreSQL の認証情報を App server だけに配布するためである。

ADR 0012 の最初の実装では、ランチャーは App server 内にあり、Assignee を子プロセスとして起動した。この構成では、App server の再起動で全 Assignee が停止する。記録の前に App server が終了した場合の残存(配置表に記録されないまま稼働を続ける Assignee)も、この全停止で回収していた。issue #196 の目的は、人の手作業による Assignee の起動を、App server の指示による起動に置き換えることである。

## Decision

各項目の末尾の【未検証】は、根拠が推論だけであることを示す。

- **ランチャーは、ノードごとに1個の独立したプロセスである**。 App server の再起動と更新で、Assignee は停止しない。ランチャー自身の終了と再起動でも、Assignee のコンテナは稼働を継続する。コンテナは Docker が管理し、ランチャーの子プロセスではないからである。【未検証】
- **Assignee は、1個ずつ Docker のコンテナで起動し、再起動の方針を `no` にする**。 同一のエポックのプロセスを再起動すると、1個のプロセスが1個のエポックだけを処理する規則(ADR 0012)に違反するからである。コンテナごとにメモリの上限を設定する。【未検証】
- **ランチャーは状態を自分で保持せず、Docker のコンテナの属性に保存する**。 WorkspaceGlobalId、エポック、採用の有無など、管理に必要な値をコンテナに保存する。ランチャーの再起動後は、Docker のコンテナの一覧から管理を復元する。独自の永続的な台帳を持たないので、ランチャーの再起動で状態が消失しない。【未検証】
- **同一の WorkspaceGlobalId とエポックのコンテナが存在すれば、稼働中か終了済みかを問わず起動を拒否する**。 終了したコンテナは、終了の記録の後にランチャーが削除する。この拒否は削除前のコンテナにしか作用しないので、同一のエポックの起動を累計1回にする保証には単独で足りない。【未検証】

範囲外: ランチャーの契約、仮起動と[採用](../../GLOSSARY.md)、残存の回収、同一のエポックの起動を累計1回にする保証([ADR 0014](0014-assignees-start-provisionally-and-are-confirmed-after-recording.md))。ノードの選択と複数ノード(#240)。App server とランチャーの間の認証。メモリの上限値と、上限の合計による起動の受け入れ(#196)。

検証: 実装は存在しない。ランチャーの強制終了と再起動の後に管理が復元され、採用済みのコンテナが稼働を継続することの試験は、#196 で追加する。

## Alternatives Considered

<details>
<summary>App server 内で子プロセスを起動する: App server の更新で全 Assignee が停止し、コンテナへの移行で実装が廃棄になる</summary>

ADR 0012 の最初の実装である。App server の再起動による全停止で、記録前の残存を回収できる。しかし App server の更新のたびに全ワークスペースが停止からの起動になる。親の終了の通知(`PR_SET_PDEATHSIG`)とゾンビの回収の実装も必要で、コンテナへの移行でこれらは全部廃棄になる。

</details>

<details>
<summary>App server 内のランチャーが Docker を操作する: 1個のプロセスに PostgreSQL と Docker の権限が集中し、二重起動で相互のコンテナを削除する</summary>

ランチャーとの通信の契約の決定を延期できる。しかし1個のプロセスが PostgreSQL の認証情報と Docker のソケットの権限を保持し、ADR 0012 の分離に反する。App server が二重に起動すると、相互のコンテナを残存と判定して削除する。

</details>

## Consequences

- オンプレ環境の必須構成に Docker が加わる。ランチャーの契約は Docker の機能でなく意味として定義する(ADR 0014)ので、ほかの実行環境(Kubernetes の Pod など)による実装も可能である。
- App server の再起動による全停止がなくなるので、記録前の残存を回収する別の方法が必要になる。その方法は ADR 0014 で決定した。
- Docker かノードが再起動すると、再起動の方針が `no` なので Assignee は復帰しない。そのノードのワークスペースは、App server の交代で停止からの起動になる(ADR 0012)。【未検証】
- 未解決: Docker のコンテナの属性の不変性と、採用の有無を保存する方法(名前の変更の原子性を含む)は未確認である(#196)。

見直す条件: Docker 以外の実行環境を必須にする顧客が現れた場合。Docker のコンテナの属性で管理の状態を表現できないと判明した場合。
