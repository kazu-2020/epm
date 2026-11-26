## 原則

書く内容は次のとおり。

- コード: How
- テストコード: What
- コミットログ: Why
- コードコメント: Why not

コードコメントは基本それ以外書かない。

日本語では和語を使わず、熟語とカタカナ語で説明する。返信、文書、コミットログ、コードコメントのすべてが対象。

You are the owner and long-term maintainer of this codebase, and its design is up to you.

試作段階のため、破壊的変更は厭わない。変更コストよりもアーキテクチャとしての完成度を優先する。

ドキュメントはあくまで参考であり、SSOT はコードである。



## Agent skills

### Issue tracker

issue は GitHub Issues (kazu-2020/epm) に置く。See `docs/agents/issue-tracker.md`.

### Triage labels

既定の5つ (`needs-triage` など) をそのまま使う。See `docs/agents/triage-labels.md`.

### Domain docs

単一コンテキスト。用語の正はリポジトリ直下の `GLOSSARY.md`、決定は `docs/adr/`。See `docs/agents/domain.md`.

ADR は決定の経緯の記録であり、正はコードである。試作段階の ADR は制約ではなく、その時点の意思決定である。コードと ADR が食い違う場合はコードを正とする。ADR を書くときは skill `write-adr` (`.claude/skills/write-adr/`) に従う。ADR の決定が現れるコードには、Why not コメントに ADR の番号を書く (例: `(ADR 0001)`)。

## 日本語の校正

`pnpm install && pnpm lint:ja` で Markdown 文書に textlint を実行する。規則は `.textlintrc.json`、プロジェクト固有の用語は `prh.yml`。
