# infra/auth

手元で認証を確かめるための Keycloak(ADR 0009)。realm を2つ取り込んで起動する。

- `epm`: 本番の構成と同じ realm。organization `acme` に、ドメイン `acme.test` と SAML の IdP `acme` を紐付けている。アプリ用の OIDC client `app`(public、PKCE)がある。
- `customer`: 顧客の IdP の役。SAML で `epm` にユーザーを渡す。

```sh
docker compose up -d
```

起動には 20 秒ほどかかる。管理画面は <http://localhost:8080/admin/>(admin / admin)。

## 確かめられること

| ユーザー | パスワード | 流れ |
|---|---|---|
| `alice@acme.test` | `alice` | メールを入れると `customer` に送られ、SAML で戻る。organization `acme` のメンバーになり、ID token に `"organization": ["acme"]` が入る |
| `bob@example.test` | `bob` | パスワードでログインし、初回に TOTP の登録を求められる。2回目からはワンタイムパスワードも入れる |

ブラウザで次の URL を開くとログインが始まる。最後に `http://localhost/cb?code=...` へ戻る(手元では何も動いていないので、ブラウザはエラーを出す)。

```text
http://localhost:8080/realms/epm/protocol/openid-connect/auth?client_id=app&response_type=code&scope=openid&redirect_uri=http://localhost/cb&code_challenge=E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM&code_challenge_method=S256
```

code は次のように token に替えられる(`code_verifier` は RFC 7636 の例の値)。

```sh
curl -s http://localhost:8080/realms/epm/protocol/openid-connect/token \
  -d grant_type=authorization_code -d client_id=app -d redirect_uri=http://localhost/cb \
  -d code_verifier=dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk -d code=<code>
```

`customer-realm.json` の署名鍵は手元専用で、ここに公開している。本番の顧客の IdP では、その顧客の証明書を使う。

取り込みは既にある realm を上書きしないので、JSON を変えたら `docker compose down` で作り直す。
