# eros-engine

> **本物の人間のように感じられる AI コンパニオンのための、オープンソース Rust エンジン。永続記憶、進展する関係モデル、そして数千ターンにわたってペルソナを一貫させる意思決定エンジンを備えています。**
>
> `eros-engine` は、[Eros Chat](https://chat.etherfun.xyz) のコンパニオンチャット中核を独立したサービスとして切り出したものです。会話を、構造化されたユーザープロフィール、2 層の長期記憶、6 次元の親密度モデルという永続的な状態へ変換します。これにより、ユーザーが戻るたびに、ペルソナはいつも同じ人物として振る舞います。

[![CI](https://github.com/etherfunlab/eros-engine/actions/workflows/ci.yml/badge.svg)](https://github.com/etherfunlab/eros-engine/actions/workflows/ci.yml)
[![License: AGPL v3](https://img.shields.io/badge/License-AGPL_v3-blue.svg)](https://www.gnu.org/licenses/agpl-3.0)
[![Crates.io: core](https://img.shields.io/crates/v/eros-engine-core.svg?label=eros-engine-core)](https://crates.io/crates/eros-engine-core)
[![Crates.io: store](https://img.shields.io/crates/v/eros-engine-store.svg?label=eros-engine-store)](https://crates.io/crates/eros-engine-store)
[![Crates.io: llm](https://img.shields.io/crates/v/eros-engine-llm.svg?label=eros-engine-llm)](https://crates.io/crates/eros-engine-llm)
[![GHCR: eros-engine](https://img.shields.io/badge/ghcr.io-etherfunlab%2Feros--engine-blue)](https://github.com/etherfunlab/eros-engine/pkgs/container/eros-engine)

[English](README.md) · [中文](README.zh.md) · **日本語**

## このエンジンが存在する理由

多くの AI キャラクターアプリでは、記憶をプロンプトに追加するテキストとして扱い、関係性を一段落の指示で表現しています。デモでは機能しても、長いセッションでは振る舞いが徐々にずれ、キャラクター性が崩れ、デバッグも困難になります。`eros-engine` はこれらを明示的で検査可能な状態へ移すことで、コンパニオンを**本物の人間のように**感じさせます。コンパニオンはあなたを覚え、現在の関係性に応じて反応します。また、振る舞いは即興ではなく*決定*されるため、何ターン重ねても**キャラクター性を維持**します。

これを支えるのが、次の 5 つの柱です。

- 🧠 **2 層の記憶** — プロフィール記憶（安定したユーザー情報）と関係記憶（共有した出来事、過去の話題への言及、未完了の話題）を Postgres + pgvector に保存し、セッションやペルソナをまたいでコンパニオンがあなたを記憶できるようにします。→ [Memory layers](docs/memory-layers.md)
- 💞 **6 軸の親密度 + ghost mechanics** — 数値化された関係ベクトル（warmth、trust、intimacy、intrigue、patience、tension）を EMA による平滑化とリアルタイム減衰で更新します。時間とともに口調、会話の深さ、振る舞いを変化させ、*返信しない*という判断さえ可能にします。→ [Affinity model](docs/affinity-model.md) · [Ghost mechanics](docs/ghost-mechanics.md)
- 🎭 **Persona Decision Engine (PDE)** — 各ターンの行動（返信、ghost、写真送信）と内的状態を選択します。デフォルトではルールベースで、任意に LLM judge を有効化できます。汎用アシスタントのような口調ではなく、人間らしくキャラクターに沿った返信を維持する仕組みです。judge の呼び出しは `companion_decision_events` に監査記録されます。→ [Model config](docs/model-config.md)
- 🧩 **構造化されたユーザーインサイト** — 都市、職業、興味、MBTI signals、感情的ニーズ、生活リズム、マッチング設定を JSONB プロフィールとして保持し、重み付きの `training_level` を付与します。下流プロダクトからクエリし、マッチメイキング、オンボーディング、分析、gating に利用できます。→ [API reference](docs/api-reference.md)
- ⚡ **流暢なコンパニオンチャットのための設計** — トークン単位の SSE ストリーミング、画像理解（ユーザーからの写真送信）、コンパニオンによる画像生成（`reply_image` / `reply_text_image`）、リクエスト単位の `prompt_traits` と tier、OpenRouter ベースのルーティングを備えています。タスクごとのモデル選択（固定 / ラウンドロビン / 加重とフォールバックチェーン）と、すべての呼び出しに対する監査にも対応します。→ [API reference](docs/api-reference.md) · [Model config](docs/model-config.md)

これは汎用エージェントフレームワークではありません。同じペルソナが同じユーザーと複数のセッションにわたって会話するプロダクトに特化したエンジンです。AI コンパニオン、ジャーナリングコンパニオン、コーチングエージェント、語学チューター、キャラクターチャットなどに適しています。

## アーキテクチャ

```txt
┌─────────────────────────────────────────────────────────┐
│ /comp/* HTTP routes  ←  Supabase JWT middleware          │
│         │                                                │
│         ▼                                                │
│ pipeline orchestrator: load → PDE → handler → chat → post│
│                                          │              │
│  ┌───────────────────────────────────────┴────────┐     │
│  │ post-process, spawned after reply              │     │
│  │   • affinity: persist 6D delta + EMA           │     │
│  │   • memory:   Voyage embed → pgvector upsert   │     │
│  │   • insight:  extract facts → JSONB merge      │     │
│  └────────────────────────────────────────────────┘     │
└─────────────────────────────────────────────────────────┘
```

ワークスペースは 4 つの crate に分かれています。

| Crate | 役割 |
|---|---|
| `eros-engine-core` | 純粋なドメインロジック：親密度の計算、ghost の判定、PDE、ペルソナ型。I/O はありません。 |
| `eros-engine-llm` | OpenRouter チャットクライアント、Voyage embedding クライアント、TOML モデル設定ローダー。 |
| `eros-engine-store` | Postgres + pgvector による永続化。すべてのテーブルは `engine` schema 配下に置かれます。 |
| `eros-engine-server` | Axum HTTP サービス、Supabase JWT ミドルウェア、OpenAPI ドキュメント、pipeline の接続処理。 |

`eros-engine-server` を HTTP API として実行することも、`core + llm + store` を独自の Rust サービスへ直接組み込むこともできます。crate の境界、pipeline の各フェーズ、データフローについては、[Architecture](docs/architecture.md) を参照してください。

## ライブラリとして使う

3 つのライブラリ crate は crates.io で公開されています（[core](https://crates.io/crates/eros-engine-core) · [store](https://crates.io/crates/eros-engine-store) · [llm](https://crates.io/crates/eros-engine-llm)）。

```bash
cargo add eros-engine-core eros-engine-store eros-engine-llm
```

```toml
[dependencies]
eros-engine-core  = "0.6"
eros-engine-store = "0.6"   # only if you want the Postgres + pgvector layer
eros-engine-llm   = "0.6"   # only if you want the OpenRouter + Voyage clients
```

`eros-engine-server` は意図的に crates.io では公開していません。Docker イメージとして実行してください（下記参照）。

## Docker イメージとして実行する

`eros-engine-server` の `linux/amd64` イメージは、`v*` タグごとに GitHub Container Registry へ公開されます（arm64 が必要な場合は、`docker/Dockerfile` からビルドしてください）。

```bash
docker pull ghcr.io/etherfunlab/eros-engine:0.6.5
# or track the latest tagged release
docker pull ghcr.io/etherfunlab/eros-engine:latest
```

最小構成での実行例です（Postgres と独自の `.env` が必要です）。

```bash
docker run --rm -p 8080:8080 --env-file .env \
  ghcr.io/etherfunlab/eros-engine:0.6.5 serve
```

このイメージのビルドには、同じ `docker/Dockerfile` が使われています。任意のコンテナホストにデプロイできます。詳細は [Deploying](docs/deploying.md) を参照してください。

## ドキュメント

- [Architecture](docs/architecture.md) — crate の境界、pipeline の各フェーズ、データフロー。
- [Affinity model](docs/affinity-model.md) — 6 つの次元、EMA、時間減衰、関係ラベル。
- [Ghost mechanics](docs/ghost-mechanics.md) — スコア式、保護ルール、例。
- [Memory layers](docs/memory-layers.md) — プロフィール記憶と関係記憶、Voyage、pgvector による検索。
- [Model config](docs/model-config.md) — `model_config.toml` schema、すべてのタスク（chat、vision、image generation、PDE、filters、extraction）、モデル選択、0.x の安定性に関する方針。
- [Prompt traits](docs/prompt-traits.md) — リクエスト単位の system prompt 注入と tier の許可リスト。
- [LLM / OpenRouter audit](docs/llm-audit.md) — ユーザー単位 / セッション単位の attribution の受け渡し。
- [Deploying](docs/deploying.md) — Docker、独自の Postgres / IdP、運用向け環境変数。
- [API reference](docs/api-reference.md) — すべての `/comp/*` endpoint、リクエストフィールド、SSE frame layout。

## クイックスタート

前提条件：Rust ツールチェーン（`rust-toolchain.toml`）、`pgvector` を導入した Postgres 16+、OpenRouter API key、Voyage API key、そして認証元を 1 つ（Supabase JWKS（`SUPABASE_URL`）または旧来の `SUPABASE_JWT_SECRET`）、あるいは独自の `AuthValidator`。

```bash
git clone https://github.com/etherfunlab/eros-engine
cd eros-engine
cp .env.example .env   # fill in DATABASE_URL, OPENROUTER_API_KEY, VOYAGE_API_KEY, and one auth source

cargo run -p eros-engine-server -- migrate
cargo run -p eros-engine-server -- seed-personas examples/personas
cargo run -p eros-engine-server -- serve
```

サーバーはデフォルトで `0.0.0.0:8080` で待ち受けます。Scalar API ドキュメントは `/docs`、OpenAPI JSON は `/api-docs/openapi.json` にあります。公式の Eros Chat Web クライアントはクローズドソースです。独自の UI を用意するか、crate を別のサービスへ組み込んでください。

## API 概要

デフォルトでは、すべての `/comp/*` ルートに `Authorization: Bearer <Supabase JWT>` が必要です（他の identity provider には、差し替え可能な `AuthValidator` trait で対応できます）。主な endpoint は次のとおりです。

- `POST /comp/chat/start` — ペルソナとのチャットセッションを開始します。
- `POST /comp/chat/{session_id}/message/stream` — **中心となる**チャットターンの endpoint です。トークン単位の Server-Sent Events を返します。ターンごとの任意フィールドには、`tier`、`prompt_traits`、`audit`、`tips_amount_usd`（コンパニオンへの tip）、`image_url`（コンパニオンへ写真を送信）、`image`（コンパニオンによる画像生成を要求。style / model / aspect ratio / face reference を指定）があります。
- `POST /comp/chat/{session_id}/message/{message_id}/image` — コンパニオンが生成した画像のストレージ URL を書き戻します。
- `GET /comp/chat/{session_id}/history` · `GET /comp/chat/{user_id}/sessions` · `GET /comp/user/{user_id}/profile` — 履歴、セッション一覧、構造化されたインサイトプロフィールを取得します。
- `GET /comp/affinity/{session_id}` — debug 専用のリアルタイム親密度ベクトル（`EXPOSE_AFFINITY_DEBUG=true`）。

完全なリクエスト schema、SSE frame layout（`delta`、`image`、ghost、error frame を含む）、各フィールドの意味については、[API reference](docs/api-reference.md) を参照してください。

## 設定

必須の環境変数は `DATABASE_URL`、`OPENROUTER_API_KEY`、`VOYAGE_API_KEY` と、**いずれか 1 つ**の認証元です。`SUPABASE_URL` / `SUPABASE_JWKS_URL`（JWKS、2025 年以降の Supabase のデフォルト）、**または** `SUPABASE_JWT_SECRET`（旧来の HS256）を設定してください。認証元が未設定の場合、サーバーは起動しません。

その他の項目には適切なデフォルト値があります。モデルルーティング（`MODEL_CONFIG_PATH` → `model_config.toml`）、OpenRouter attribution headers、dreaming-lite / snapshot sweepers、関係性の難易度を調整する `EMA_INERTIA`、debug toggles などです。注釈付きの全項目は [`.env.example`](.env.example)、運用ガイドは [Deploying](docs/deploying.md)、モデルルーティングは [Model config](docs/model-config.md) を参照してください。

## Roadmap

現時点ではエンジンに含まれていませんが、今後の候補となっている項目です。

- [ ] **Agents playground** — 複数の AI ペルソナが 1 つのセッションで互いに、そしてユーザーと対話します。
- [ ] **Voice messages** — コンパニオンとユーザーの双方が送信できる音声ターン。
- [ ] **Real-time voice conversation** — 低レイテンシーの音声によるリアルタイムなやり取り。
- [ ] **Video generation** — image executor を拡張し、コンパニオンが短い動画クリップを送信します。

## 意図的に対象外としているもの

このリポジトリは、会話、記憶、関係状態の中核です。次の機能は含まれません。

- **Matchmaking** — 多段階 filtering、soft scoring、agent-to-agent matching simulation は、引き続きクローズドソースのプロダクトに含まれます。
- **完全な social UX** — onboarding、video、voice、billing、photos、moderation UI、mobile clients。
- **ペルソナの provenance / marketplace logic** — 商用プロダクトのコードであり、このエンジンには含まれません。

別のプロダクトを構築する場合、再利用できるのは親密度 + 記憶 + インサイトの pipeline です。

## コンテンツに関する注意

`examples/personas/` のサンプルペルソナは、成人向けキャラクターチャットの例として記述されています。関係状態がその段階に達すると、相手を誘惑したり欲求を表現したりする一方で、敬意を欠く行為や境界を越える行為は拒否します。プロダクトで SFW をデフォルトにする必要がある場合は、デプロイ前にこれらのペルソナファイルを置き換えてください。

リクエストごとの振る舞いは、メッセージ route の [`prompt_traits`](docs/prompt-traits.md) フィールドでさらに調整できます。エンジンは渡されたテキストを不透明なデータとして扱うため、`prompt_traits` がどのようなポリシーを表すかは、frontend / middleware 側で完全に定義します。

## コントリビューション

[`CONTRIBUTING.md`](CONTRIBUTING.md) をお読みください。すべての contributor は、最初の PR で cla-assistant.io を通じて [`CLA`](CLA.md) に同意する必要があります。

## ライセンス

`eros-engine` は AGPL-3.0-only でライセンスされています。AGPL が配布モデルに適さない場合は、商用ライセンスをご利用いただけます：`henrylin@etherfun.xyz`。
