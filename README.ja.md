# eros-engine

**本物の人間のように感じられる AI コンパニオンのための、オープンソース Rust エンジン。永続記憶、進展する関係モデル、そして数千ターンにわたってペルソナを一貫させる意思決定エンジンを備えています。**

[![CI](https://github.com/etherfunlab/eros-engine/actions/workflows/ci.yml/badge.svg)](https://github.com/etherfunlab/eros-engine/actions/workflows/ci.yml)
[![License: AGPL v3](https://img.shields.io/badge/License-AGPL_v3-blue.svg)](https://www.gnu.org/licenses/agpl-3.0)
[![Crates.io: core](https://img.shields.io/crates/v/eros-engine-core.svg?label=eros-engine-core)](https://crates.io/crates/eros-engine-core)
[![Crates.io: store](https://img.shields.io/crates/v/eros-engine-store.svg?label=eros-engine-store)](https://crates.io/crates/eros-engine-store)
[![Crates.io: llm](https://img.shields.io/crates/v/eros-engine-llm.svg?label=eros-engine-llm)](https://crates.io/crates/eros-engine-llm)
[![GHCR: eros-engine](https://img.shields.io/badge/ghcr.io-etherfunlab%2Feros--engine-blue)](https://github.com/etherfunlab/eros-engine/pkgs/container/eros-engine)

[English](README.md) · [中文](README.zh.md) · **日本語**

## ハイライト

多くの AI キャラクターアプリは、やがてあなたを忘れます。関係性はプロンプトに収まる文章へ戻り、会話が長くなるほど人物像もぶれていきます。`eros-engine` は、そこを持続する状態にします。コンパニオンはセッションをまたいであなたを覚え、交流とともに関係が変わり、汎用アシスタントの即興ではなく、その人物らしい判断から返信します。

土台となるのは次の 5 つです。

- 🧠 **2 層の記憶** — 安定したユーザー情報と、共有した出来事、過去への言及、続きのある話題をそれぞれ保持します。→ [Memory layers](docs/memory-layers.md)
- 💞 **変化する親密度** — 6 つの関係軸が滑らかに変化し、時間とともに減衰します。口調や会話の深さ、返信するかどうかにも影響します。→ [Affinity model](docs/affinity-model.md) · [Ghost mechanics](docs/ghost-mechanics.md)
- 🎭 **Persona Decision Engine（PDE）** — 生成前に、そのターンの行動と内面状態を選びます。標準はルールベースで、LLM judge も任意で使えます。→ [Model config](docs/model-config.md)
- 🧩 **構造化されたユーザー理解** — 検索可能なプロフィールを育て、導入体験、パーソナライズ、分析などに活用できます。→ [API reference](docs/api-reference.md)
- ⚡ **一通りそろったチャット経路** — SSE ストリーミング、画像理解と生成要求、`prompt_traits`、タスク別モデル選択、フォールバック、呼び出し監査を備えます。OpenRouter が標準ですが、`[providers]` から OpenAI 互換のチャット・embedding 提供元を追加できます。→ [API reference](docs/api-reference.md) · [Model config](docs/model-config.md)

汎用エージェントフレームワークではありません。同じ人物が同じユーザーを時間をかけて知っていくプロダクトのための、状態を持つ中核です。AI コンパニオン、日記、コーチ、語学チューター、キャラクターチャットに向いています。

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

4 つの crate が、ドメインロジック、モデル接続、永続化、HTTP サービスを分担します。`eros-engine-server` を API として動かすことも、`core + llm + store` を独自の Rust サービスへ組み込むこともできます。境界とデータフローは [Architecture](docs/architecture.md) を参照してください。

## ライブラリ利用

3 つのライブラリ crate は crates.io で公開されています（[core](https://crates.io/crates/eros-engine-core) · [store](https://crates.io/crates/eros-engine-store) · [llm](https://crates.io/crates/eros-engine-llm)）。

```bash
cargo add eros-engine-core eros-engine-store eros-engine-llm
```

```toml
[dependencies]
eros-engine-core  = "1.0"
eros-engine-store = "1.0"   # optional: Postgres + pgvector persistence
eros-engine-llm   = "1.0"   # optional: model and embedding clients
```

`eros-engine-server` は crates.io では公開していません。Docker イメージで実行してください。

## Docker イメージ

各 `v*` タグについて、複数アーキテクチャ対応のイメージを GitHub Container Registry へ公開しています。

```bash
docker pull ghcr.io/etherfunlab/eros-engine:1.0.1
# Or follow the latest tagged release
docker pull ghcr.io/etherfunlab/eros-engine:latest
```

```bash
docker run --rm -p 8080:8080 --env-file .env \
  ghcr.io/etherfunlab/eros-engine:1.0.1 serve
```

Postgres と `.env` は利用者側で用意してください。同じ `docker/Dockerfile` を任意のコンテナ環境へ配備できます。詳細は [Deploying](docs/deploying.md) を参照してください。

## ドキュメント

- [Architecture](docs/architecture.md) — crate の境界、処理段階、データフロー。
- [Affinity model](docs/affinity-model.md) — 関係軸、平滑化、減衰、関係ラベル。
- [Ghost mechanics](docs/ghost-mechanics.md) — コンパニオンが沈黙する条件と理由。
- [Memory layers](docs/memory-layers.md) — プロフィール記憶と関係記憶、embedding、検索。
- [World system](docs/world-system.md) — 実験的な World Memories、World Town、World Stories のシミュレーション。
- [Model config](docs/model-config.md) — タスク、モデル選択、フォールバック、`[providers]` による複数提供元へのルーティング。
- [Prompt traits](docs/prompt-traits.md) — リクエストごとのプロンプト調整と tier の許可リスト。
- [LLM / OpenRouter audit](docs/llm-audit.md) — ユーザー・セッション単位の帰属情報。
- [Deploying](docs/deploying.md) — Docker、Postgres、認証、運用。
- [API reference](docs/api-reference.md) — ルート、リクエスト schema、SSE frame。

## クイックスタート

Rust、`pgvector` を導入した Postgres 16+、OpenRouter API キー、認証元が 1 つ必要です。標準の embedding 経路は Voyage を使います。embedding は別の提供元にも振り分けられ、読み取りと書き込みの両方を Voyage から外した場合に限り `VOYAGE_API_KEY` は不要です。

```bash
git clone https://github.com/etherfunlab/eros-engine
cd eros-engine
cp .env.example .env   # Set DATABASE_URL, OPENROUTER_API_KEY, VOYAGE_API_KEY, and one auth source

cargo run -p eros-engine-server -- migrate
cargo run -p eros-engine-server -- seed-personas examples/personas
cargo run -p eros-engine-server -- serve
```

サーバーは標準で `0.0.0.0:8080` を使用し、Scalar API ドキュメントは `/docs` にあります。公式 Eros Chat Web クライアントは非公開です。独自の UI を用意するか、crate を別のサービスへ組み込んでください。

## API 概要

基本の流れは、ペルソナとのセッションを作り、SSE ストリーミングのエンドポイントへ会話を送るだけです。履歴、セッション、プロフィール、任意の親密度デバッグ用ルートもあります。標準認証は Supabase JWT で、`AuthValidator` により差し替えられます。パス、ペイロード、ストリームの各 frame は [API reference](docs/api-reference.md) を参照してください。

## 設定

最低限、`DATABASE_URL`、認証元を 1 つ、そして `OPENROUTER_API_KEY` を設定します。OpenRouter は組み込みの標準提供元で、その API キーは起動時に必ず必要です。`[providers]` では OpenAI 互換のチャット・embedding エンドポイントをそれぞれのキーで追加できます。標準の Voyage embedding 構成には `VOYAGE_API_KEY` が必要ですが、`[tasks.embedding]` で読み取りと書き込みを両方とも別の提供元へ振り分けた場合は不要です。

環境変数の全一覧は [`.env.example`](.env.example)、運用情報は [Deploying](docs/deploying.md)、ルーティングの詳細は [Model config](docs/model-config.md) にあります。

## ロードマップ

- [ ] **複数ペルソナの実験環境** — 同じセッションで複数の AI ペルソナが互いに、またユーザーと会話する仕組み。
- [ ] **音声メッセージ**と**ネイティブ音声 I/O** — 低遅延の音声ターン API は提供済みで、STT/TTS は現在呼び出し側の担当です。
- [ ] **動画生成** — コンパニオンから短い動画を送信。

## スコープ外

このリポジトリが扱うのは、会話、記憶、関係状態の中核です。マッチング、ソーシャルプロダクト全体の体験、ペルソナの流通・来歴管理は対象外です。再利用の中心となるのは、親密度、記憶、ユーザー理解の処理系です。

## コンテンツに関する注意

`examples/personas/` のサンプルは成人向けキャラクターチャットです。関係が深まれば誘惑や欲求を表すことがありますが、敬意を欠く行為や境界を越える行為は拒否します。SFW を標準にする場合は、配備前にこれらのペルソナを置き換えてください。

リクエストごとの振る舞いは [`prompt_traits`](docs/prompt-traits.md) でも調整できます。エンジンはその文字列を解釈しないため、方針はフロントエンドまたはミドルウェア側で定義します。

## コントリビューション

[`CONTRIBUTING.md`](CONTRIBUTING.md) をお読みください。初回 PR では cla-assistant.io を通じて [`CLA`](CLA.md) への同意が必要です。

## ライセンス

`eros-engine` は AGPL-3.0-only です。商用ライセンスについては `henrylin@etherfun.xyz` までお問い合わせください。
