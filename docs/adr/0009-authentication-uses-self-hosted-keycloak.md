# 認証は自分でホストする Keycloak に任せ、アプリは OIDC だけを話す

2026-11-11

## 決めたこと

- **認証は Keycloak に任せる。** 自分の環境(東京リージョン)でホストする。顧客の IdP との SAML、パスワード、2段階認証(TOTP・passkey)は Keycloak が受け持つ。
- **realm は1つにし、テナントは Keycloak の organization で表す。** 顧客の SAML IdP は organization に紐付け、メールのドメインで振り分ける。
- **アプリと Keycloak の間は標準の OIDC だけにする。** アプリが ID token から使うのは `iss`・`sub`・`email`・`organization` だけにする。
- **ユーザー、テナントへの所属、ワークスペースのロール、データ範囲はアプリの PostgreSQL に持つ。** ユーザーは `(iss, sub)` で引く。Keycloak の groups や Authorization Services は使わない。
- **ログインのあとのセッションはアプリが発行する。** Keycloak の token をブラウザのセッションに使わない。
- **IP 制限はアプリの入口で毎リクエスト行う。** テナントごとに許可する CIDR を PostgreSQL に持つ。計算サーバーは外に出さない。
- **2段階認証は、パスワードでログインするテナントだけ Keycloak で必須にする。** SAML のテナントでは顧客の IdP が受け持つ。
- **SCIM は今は作らない。** 初回のログインでユーザーを作る。退職者の無効化は、後で SCIM か管理画面で足す。

## 理由

- SAML は XML 署名の検証で認証を回避される脆弱性が繰り返し出ている。少人数で自前の実装を追い続けられない。
- Keycloak は Apache 2.0 で、SAML・TOTP・passkey が標準で入っている。Organizations は 26.0 から、SCIM API は 26.8 から正式な機能である。
- 認証の情報も国内の自分の環境に置けるので、国内の顧客にデータの置き場所を説明しやすい。
- docker compose で、顧客の IdP の役も含めて手元で SAML のログインを確かめられる(`infra/auth/`)。
- テナントごとに realm を作ると、realm の数に比例して運用と性能の負担が増える。
- アプリが OIDC と `(iss, sub)` だけに依存していれば、IdP を替えるときのコードの変更は接続先と設定で済む。
- ログインのときだけ IP を見ると、持ち出されたセッションを止められない。

## 選ばなかった案

- **自前で実装する**: 上の SAML の脆弱性への対応と、TOTP・passkey・セッション管理の実装を全部持つことになる。
- **Zitadel**: 複数テナントの作りは素直だが、v3 から AGPL-3.0 で、大企業の導入審査で説明が要る。SCIM の対応も確かめられなかった。
- **Ory (Kratos + Polis)**: 自分でホストする Kratos では Organizations を使えず、SAML は Polis を別に立てる必要がある。
- **Authentik**: 外部の SAML をログインのフローに組み込む機能が有償版にある。
- **WorkOS・Auth0・Cognito**: WorkOS は SSO と SCIM がそれぞれ1接続 $125/月からで、顧客の数に比例して費用が増える。どれも手元だけでは確かめられない。Cognito は公式のローカル版がない。

## 影響

- Keycloak の更新、データベース、可用性の運用が増える。ログイン画面が攻撃の入口になる。
- 後で IdP を替えると、SAML の顧客ごとに先方の IdP の設定変更(entity ID・ACS URL・証明書)を頼むことになる。TOTP と passkey は登録し直しになることが多い。この負担を小さくするため、IdP は自社のドメインで公開し、Keycloak 固有の機能を使わない。
- 1人が複数の `(iss, sub)` を持てるように、外部 ID はユーザーとは別の表に持つ。IdP を並行して動かし、顧客ごとに切り替えられる。
