# 親密度モデル

[English](affinity-model.md) · [中文](affinity-model.zh.md) · [日本語](affinity-model.ja.md)

## 1. 概要

Affinity(親密度)は、セッション(会話単位の関係)ごとに保持される6軸のベクトルである。各軸の値は `[0, 1]` の範囲に収まり、更新のたびにクランプされる。値が変化するのは、テキストチャンネルの `product_qa` 以外のチャットターンに限られる。voice チャンネルのターンと `product_qa` ターンでは、親密度イベントは一切書き込まれない。

ジャッジ(LLM 評価器)が返すのは、グレードやレベルといった離散的な区分による順序尺度のみである。実数値、ラベル、ティア遷移の判定は、すべてエンジンが担う。数値を決定する唯一の主体はエンジンである。

6軸は次の3グループに分かれる。本ドキュメントも、この3グループを軸に構成する。

- 基礎ペア(base pair): `warmth`(温かさ) / `patience`(忍耐力)
- Bond ライン(友情線): `trust` / `intrigue`
- Chemistry ライン(恋愛線): `intimacy` / `tension`

以下では、まずこの3グループをそれぞれ解説し(§2)、値が変化する仕組み(§3)、基礎ペアの導出に用いる定数(§4)、ティアとラベル(§5)、親密度の利用先(§6)、`affinity.rs` と `scope.rs` の対比(§7)、永続化と API(§8)、調整パラメータ(§9)、ソースマップ(§10)の順に説明する。

## 2. 6軸を3グループに分ける

### 2.1 基礎ペア — warmth と patience

基礎ペアは蓄積される状態ではない。判定対象のターンごとに、ジャッジは各エンドポイントの絶対的な**レベル**を1つ報告する。1 = 冷たい/短気、2 = ベースライン(判定の圧倒的多数を占める)、3 = 明確に温かい/入れ込んでいる。エンジンは、このレベルから連続値を導出する。

```
base(level)  = (level − 1) / 3                          ∈ {0, 1/3, 2/3}
B(x)         = 1 + λ·(x − 0.35)                         λ = (1.5−1)/(1−0.35) = 10/13
decay(Δt)    = max(FLOOR, 1 − RATE·days)                Δt since updated_at

warmth   = clamp01( max(base(w_level)·B(chemistry), φ·chemistry) × decay )
patience = clamp01( max(base(p_level)·B(bond),      φ·bond)      × decay )
```

この結合は**相関ではなく増幅を表す**。Chemistry が深いほど表現が温かくなり、Bond が深いほど忍耐力が高まる。

bond が低く chemistry が高い組み合わせからは、「ツンデレ」の語調(短気だが温かい)が自然に生まれる。bond が高く chemistry が低い組み合わせからは、「長年の友人」の語調(忍耐強いが素っ気ない)が生まれる。プロンプト側での場合分けは一切ない。どちらも、モデルがこの導出式に沿って値を計算した結果として自然に生まれる語調であり、語調ごとにプロンプトを書き分けているわけではない。

セッション開始直後のデフォルト値は、両軸とも ≈ 0.244(レベル2の減衰済み基礎値)である。見知らぬ相手に対しては、最初から忍耐力があまりない。これは意図した設計である。

基礎ペアはユーザーがターンごとに「体感する」値であり、以下に直接反映される。

- `[relationship]` プロンプトセクション: warmth × patience をそれぞれ 0.5 で分割した四象限。各セルの4語はプロンプト内の実際の文字列であり、そのまま引用する。

  | | patience ≥ 0.5 | patience < 0.5 |
  |---|---|---|
  | **warmth ≥ 0.5** | 好朋友 | 快被磨光耐心的朋友 |
  | **warmth < 0.5** | 没什么交情的人 | 死对头 |

  Bond/Chemistry のティアとは独立している。どちらかの軸がそのリクエストのスコープ外にある場合、または親密度の行がまだ存在しない場合は省略される。

- `[mood]` の cold ゲート(warmth ≤ 0.2 で cold-tone ディレクティブ、patience < 0.35 で impatient ディレクティブ)と、ターンごとのダイスの veto 判定(同じ下限を使う)。

- PDE ジャッジに提示される patience バンド: low [0, 0.35)、mid [0.35, 0.65)、high [0.65, 1]。

コード上では、これらを **derived endpoints(導出エンドポイント)** と呼ぶ。保存されている `warmth` / `patience` カラムは導出結果のマテリアライズドキャッシュであり、時間減衰が適用されるたびに再計算される。各行で正とするデータは、4本のライン軸 + 2つのジャッジレベル(`warmth_grade` / `patience_grade`、`1..=3`) + `updated_at` である。

### 2.2 Bond ライン — trust と intrigue

`trust` は話題の深さと自己開示の意思を表す。`intrigue` は好奇心、フォローアップの質問、ゴーストを避ける動機を表す。

```
bond = (trust + intrigue) / 2   ∈ [0, 1]
```

友情とは、信頼に持続的な関心が加わったものである。ティアは5段階あり、ラベルは `acquaintance` / `friend` / `close_friend` / `confidant` / `soulmate` である(snake_case でシリアライズされる)。

### 2.3 Chemistry ライン — intimacy と tension

`intimacy` は内輪ネタ、あだ名、以前の細かな内容への言及を表す。`tension` は駆け引き、遊び心のある摩擦、ツンデレの余地を表す。

```
chemistry = (intimacy + tension) / 2   ∈ [0, 1]
```

恋愛とは、親密さに緊張感が加わったものである。ラベルは `spark` / `flirtation` / `crush` / `lover` / `beloved` である。

構造上、どちらのラインにも基礎ペアは含まれない。ラインは基礎ペアの**入力**であり、その逆ではない。4.0 以降、両ラインに共有部分はない。

セッション開始直後は4本のライン軸がすべて 0 であり、bond = chemistry = 0、どちらもティア1となる。独立した「見知らぬ人」状態は存在せず、ティア1は `acquaintance` + `spark` と解釈できる。

## 3. 値が変化する仕組み

ライン軸と基礎ペアでは、値が変化する経路も、時間経過の扱いも異なる。以下では、ジャッジプロトコル(3.1)、ライン軸の書き込みパイプライン(3.2)、時間経過の扱い(3.3)、値が変化しないターン(3.4)の順に説明する。

### 3.1 ジャッジプロトコル(順序尺度のみ)

ライン軸については、軸ごとに整数の**グレード** 0〜4 と**方向**("up"/"down")を報告する。基礎ペアについては、エンドポイントごとに絶対的な**レベル** 1〜3 を報告する。

- **グレードの基準。** 0 = 何も起きなかった(判定の圧倒的多数を占める)。1 = 小さいが実質的な変化。2 = 明確な後押し、または明確な傷。3 = まれな重要な瞬間。4 = マイルストーン(極めてまれ)。

- **方向。** "up" または "down"。ネガティブな瞬間には積極的に判定を出すよう、プロンプトで指示されている。

- **レベルの基準(基礎ペア)。** 1 = 冷たい/短気、2 = ベースライン(判定の圧倒的多数を占める)、3 = 明確に温かい/入れ込んでいる。レベルはそのターンの状態を読み取った値であり、デルタではない。

判定 JSON の例:

```json
{
  "warmth":   2,
  "trust":    {"grade": 1, "direction": "up"},
  "intrigue": {"grade": 0, "direction": "up"},
  "intimacy": {"grade": 0, "direction": "up"},
  "tension":  {"grade": 2, "direction": "down"},
  "patience": 2,
  "reason": "…"
}
```

**順序尺度を使う理由。** モデルは順序を評価する役割では信頼できるが、較正された算術演算を行う役割では信頼できない。ユーザーが目にする連続的な分布は、離散的な区分からエンジン側の数式によって導出される。4.0 では、ジャッジ側に最後まで残っていた連続出力である0.1刻みの patience の読み取りが廃止された。本番環境で値が上限に張り付いていたためである。

**不正な判定。** 不正な判定は全体が拒否される(`parse_affinity_eval`)。パース不能な JSON、範囲外のグレード、未知の方向、不正なレベルのいずれであっても、すべてのグレードがゼロになり、レベルの読み取りは行われず、`reason` は空になる。そのターンのルールデルタは引き続き反映されるため、ジャッジの失敗によって親密度イベント自体が失われることはない。

救済ルールは次のとおりである。省略された軸や `null` の軸はグレード0として扱う。レベルが省略されている場合や `null` の場合は、保存済みレベルを保持する。引用符付きの整数("2")は救済される。

**バンド化された入力。** ジャッジには、4本のライン軸の現在値が粗いバンド(低/中/高、0.35 / 0.65 で区切る)として提示され、浮動小数点数で渡されることはない。現在の warmth/patience は意図的に注入されない。状態を持たずに絶対値を読み取らせ、アンカリングによるインフレを避けるためである。

**語調の管理。** ジャッジのプロンプトは一人称のキャラクター視点で書かれている。エンジンが管理し、設定では変更できない。`reason` ではシステム用語の使用を禁じている。`reason` は永続化され、後で `[emotional_context]` として再注入されるためである。

### 3.2 書き込みパイプライン(ライン軸のみが対象、基礎ペアは含まない)

```
grade → raw score → tier decay → cross-line penalty → threshold gate → clamp
                                                    → endpoint derivation
```

すべて `grade_turn` 内で、ターン開始前のスナップショットを基に計算され、行ロックの下で適用される。

1. **変換。** 符号付きグレード × ライン単位: `AFFINITY_GRADE_UNIT_BOND` 0.0786(trust/intrigue)、`AFFINITY_GRADE_UNIT_CHEM` 0.0266(intimacy/tension)。負の raw は `AFFINITY_NEG_FACTOR` 1.5 倍される(上昇は遅く、下降は速くなる)。デモセッション(`metadata.is_demo`)では、ジャッジの正の raw を `AFFINITY_DEMO_BOOST` 1.4 倍する。

   この約3倍のユニット差は、実測されたジャッジの採点の非対称性を反映している(tension は約半数のターンでグレード2以上に達する一方、trust は約80%でグレード0になる)。検討できるよう、差を明示している。PDE のルールナッジ(例: ユーザーの長文メッセージで intrigue +0.02)は、減衰前の raw score に加算される。

2. **ティア減衰(正のみ)。** 正の raw に、自ラインのティア係数 `AFFINITY_TIER_DECAY` = 1.0, 0.70, 0.45, 0.25, 0.10(ティア1〜5)を乗じる。負の raw は減衰せず、どのティアでも損失の全量が適用される。

3. **クロスラインペナルティ。** もう一方のラインの高さに応じて、変化にペナルティがかかる。ペナルティ量は、適用されたグレードに比例する。

```
penalty = κ_line × ((y − y₀)⁺ / (1 − y₀))² × (|g| / 4)
  y      = 相手ラインのスコア
  κ_line = AFFINITY_CROSS_PENALTY_RATIO × u_line   (ratio デフォルト 5/6)
  y₀     = AFFINITY_CROSS_PENALTY_START            (デフォルト 0.35)
```

   グレード0にはペナルティがかからない。このパイプラインのペナルティはイベントに対して課され、状態の維持に対して継続的に課されるものではない。ルールナッジを無視すると、この項は因数分解でき、括弧内に `g` も `u` も現れない形になる。そのため、ある位置での増減の符号はグレードが変わっても反転せず、損益分岐点の位置はユニットに依存しない。デフォルト値では、自ラインのティア5だけが実質的な損益分岐点を持つ(相手ライン ≈ 0.800)。それを超えると、どのグレードでも一様にマイナスになる。

4. **閾値ゲート。** 軸ごとに符号付きアキュムレータを持つ。累積値の絶対値が `AFFINITY_DELTA_THRESHOLD`(デフォルト 0 = 毎ターン確定)以上になった時点で確定し、それ以外は `pending_deltas` に蓄積される。

   確定したデルタは 1:1 で適用され、`[0,1]` にクランプされる。その後、そのターンでジャッジのレベルが読み取られていれば保存済みレベルを上書きし、両エンドポイントをターン後のライン値から再導出する。

### 3.3 時間

ライン軸のドリフトは遅延評価され、`updated_at` からの経過時間に基づいて load 時に計算される。`intrigue` は1日あたり −0.01、`tension` は1日あたり −0.005。`trust` と `intimacy` は関係の深い次元を表すため、減衰しない。

基礎ペアの不在減衰は、導出時に乗算で適用される。`AFFINITY_TIME_DECAY_RATE` は 0.02/日、下限の `AFFINITY_TIME_DECAY_FLOOR` は 0.5 である(7日で ×0.86、25日以上で ×0.5)。不在によって**冷めるが、決してゼロにはならない**。長く続いた関係はブーストによって下限を保つ(bond 0.9 で完全に減衰しても patience は ≈ 0.47 になる)。4.0 以前の patience の上方ドリフトは廃止された。

### 3.4 値が変化しないターン

eval がスキップされた場合(`eval_skip_reason`)や失敗した場合(`llm_attempts` / `gateway_errors` が空でない場合)は、保存済みレベルを保持し、現在のライン値と減衰からエンドポイントを再導出する。旧来のルールデルタへのフォールバックは廃止された。

ゴーストターンは `persist_with_event` に到達しない。変化するのは `ghost_streak` / `total_ghosts` / `last_ghost_at` のみであり、`record_ghost` はすべてゼロの `effective_deltas` を書き込む。

## 4. 基礎ペア導出の詳細(定数とその根拠)

すべての定数には根拠があり、恣意的に決めたものではない。

- ピボット 0.35 = ティア2の上限(同じ定数)。相手ラインがティア3に入った瞬間、ブーストがプラスに転じる。0.35/0.65 は、ジャッジの入力バンドと patience バンドの区切りでもある。

- B(1) = 1.5 により ⅔ × 1.5 = 1.0 となる。レベルと相手ラインがともに最大なら、ちょうど上限に達する。これはコード定数であり、調整パラメータではない(構造上の制約)。

- 下限 φ = 0.2(`AFFINITY_FLOOR_RATIO`)。レベル1の判定は、0 ではなく φ・相手ラインとして解釈される。深い関係が一時的に冷めても、残り火は残る(相手ライン 0.9 のとき 0.18)。見知らぬ相手なら ~0 になる。φ・x ≤ 0.2 < 0.244 = ⅓・B(0) であるため、この下限はレベル1にしか作用しない。

decay = 1 における到達可能値: レベル1 → [0.0, 0.2](相手ラインに対して連続)、レベル2 → [0.244, 0.5]、レベル3 → [0.487, 1.0]。レベルによってバンドが決まり、相手ラインによってそのバンド内の位置が決まる。

ターンごとのデルタも引き続き存在する。`effective_deltas.warmth` / `.patience` は、そのターン前後の導出結果の after − before であり、減衰適用後のスナップショットを基準に測定される(不在による差分が、そのターンの成果として計上されることはない)。

## 5. ティアとラベル

各ラインには5段階のティアがあり、上位ほどスコア範囲の幅が広がるが、最上位だけは狭くなる。

| Tier | Score range | Gap |
|------|-----------|-----|
| 1 | [0.00, 0.15) | 0.15 |
| 2 | [0.15, 0.35) | 0.20 |
| 3 | [0.35, 0.62) | 0.27 |
| 4 | [0.62, 0.90) | 0.28 |
| 5 | [0.90, 1.00] | 0.10 |

スコアはそのまま提供される(表示用カーブは適用されない)。序盤は進みやすく、終盤ほど進みにくくなるのは、実際の値の変化によるものである。書き込み時のティア減衰が、自ラインのティアに応じて正の伸びを減衰させるためである。

ラベル表(独立した2セット、それぞれ5つ、snake_case でシリアライズ):

Bond: `acquaintance` / `friend` / `close_friend` / `confidant` / `soulmate`

Chemistry: `spark` / `flirtation` / `crush` / `lover` / `beloved`

ティア番号は永続化される(`bond_tier` / `chem_tier` カラム)。これにより、SQL 側の利用者も正式なティアを直接取得できる。閾値は単一の関数(`tier_index`)にのみ定義されており、ティアを追加するには、この関数の変更とバックフィルが必要になる。

ターンごとのティア遷移は、イベント行の `label_changes` JSONB に記録される(`{bond: {from, to}, chemistry: {from, to}}`、どちらも変化しなかった場合は NULL)。

## 6. 親密度の読み取り先(利用先マップ)

親密度の値を実際に読み取る先は、次の7つである。

- `[relationship]` — 基礎ペアの象限を使う(§2.1参照)。

- `[mood]` — 軸ごとの閾値ゲートを使う(cold 禁止と warm 解禁)。

- `[feelings]` — 親密度の行に保存された、LLM が書いた感情節を使う(`feeling_clause`、変化があったターンで書き換えられる)。

- `[reply_length]` — スコープ複合値 `length_score` によって選ばれる3段階の固定上限を使う(閾値 0.25 / 0.55)。

- ターンごとのダイス(`TurnNudges`)の veto 判定 — `[mood]` と同じ cold 下限を使う(warmth ≤ 0.2 / trust < 0.3 / intrigue < 0.3)。

- PDE ジャッジのコンテキスト — intimacy rung(`max(bond, chemistry)` 上の 1..=3 の画像ゲート、rung3 は 0.76 で開き、設計上ティア4の内側に収まる)と patience バンドを使う。

- ゴーストスコアリング — `score = (1−intrigue)·0.4 + (1−patience)·0.4 + tension·0.2` を使い、ハード veto(最初の10メッセージ、streak ≥ 2、1時間クールダウン)と閾値 0.65(セッションが一度ゴーストした後は 0.85)を適用する。

## 7. affinity.rs と scope.rs

この節では、モデル本体である `affinity.rs` と、リクエストごとの注入ゲートである `scope.rs` を対比する。両者は名前も扱う軸グループも似ているため混同されやすいが、役割はまったく異なる。この違いは意図的なものである。

### それぞれの役割

`crates/eros-engine-core/src/affinity.rs` はモデルそのものである。状態構造体、書き込みパイプライン(`grade_turn`)、基礎ペアの導出、時間減衰、Bond/Chemistry のスコア、ティア、ラベルを持つ。

`crates/eros-engine-core/src/scope.rs` はリクエストごとの注入ゲートである。`AffinityScope`(6つの真偽値で、どの軸が「このリクエスト」のプロンプトに影響してよいかを表す)と `MemoryScope` を持つ。プロンプト注入と `length_score` のみを制御し、後処理の書き込み(インサイト抽出、メモリ書き込み、6軸評価)には影響しない。

### 共通点

どちらも core クレートにあり、6軸を半分ずつに分けた2つのグループに名前を付けている。scope の veto/ゲート判定とモデルの cold ディレクティブは同じ下限を参照するため、veto された軸と cold な軸の扱いは整合している。

### 相違点とその意図

1. **書き込みか読み取りか。** affinity.rs は状態とその変化を管理し、scope.rs は何も書き込まない。3.1 にあった書き込み側の scope steering は、4.0 で廃止された。`affinity_scope` は読み取り専用であり、エンドポイントの導出で scope を参照してはならない(B(x) がすでにすべてのライン変化をエンドポイントに伝えるため、導出層でも scope を参照すると同じリクエストを二重に反映してしまう。下記の名前の交差も、その理由の一つである)。

2. **グループ分けが異なる。** affinity.rs(2.0以降のライン): bond = trust + intrigue、chemistry = intimacy + tension、基礎ペアはどちらにも含まれない。scope.rs(1.0時代の分割): `AffinityScope::bond()` = warmth + intimacy + tension(朋友感)、`AffinityScope::chemistry()` = trust + intrigue + patience(暧昧感)。`length_score` は、有効な各三つ組の平均を /3 で求め、両方が有効なら両グループの平均を取る。

3. **scope の2つの名前は、2.0以降のラインと交差している。** 「bond」と呼ばれる三つ組には Chemistry ラインの軸が含まれ、その逆も同様である。構造上、1.0 の分割では、各エンドポイントを「現在それを増幅するライン」と同じグループにしていた(warmth は intimacy/tension と、patience は trust/intrigue と)。これは4.0 の結合で明示されている組み合わせと同じだが、名前だけが入れ替わっている。

4. **これは既知の、意図的に残された欠陥(wart)である。** scope の名前やグループ分けを変更すると `length_score` の入力が変わり、既存の呼び出し元で返信長にリグレッションが生じる。デフォルトの scope は `bond()`(warmth/intimacy/tension の三つ組)である。「修正」してはならない。scope に導出や書き込みパスを制御させてはならない。

## 8. 永続化と API

ここでは、DB のカラム定義とイベント行の構造、それらを外部に公開する BFF API の契約を扱う。

### 生成カラム

マイグレーション 0048 により、`bond` / `chemistry` は、ライン軸を基に計算する Postgres の `GENERATED ALWAYS … STORED` カラムとして定義される。

```sql
bond      GENERATED ALWAYS AS (LEAST(1, GREATEST(0, (trust    + intrigue) / 2))) STORED
chemistry GENERATED ALWAYS AS (LEAST(1, GREATEST(0, (intimacy + tension)  / 2))) STORED
```

DB が書き込みのたびに再計算するため、ライン軸の値との乖離は生じない。式は、コード側の式(`bond_score` / `chemistry_score`)と一致するよう維持する必要がある。

### エンドポイントレベル

`warmth_grade` / `patience_grade` は SMALLINT NOT NULL DEFAULT 2 であり、`1..=3` の範囲チェックが付く(マイグレーション 0048)。`warmth` / `patience` のキャッシュカラムは、レベル2の導出値でバックフィルされている。

### pending_deltas

`pending_deltas` は JSONB である(ライン軸のみ。旧行に残る `warmth` キーは無視され、順次排出される。NULL はすべてゼロとして読み取られる)。

### イベント行(`companion_affinity_events`)

デルタが生じたターンごとに1行が追加される。

- `deltas` = 生スコア(グレード変換 + ルールナッジ、減衰前)。ここでは `warmth`/`patience` は常に 0.0 である。

- `effective_deltas` = after − before の適用済み変化(基礎ペアについては、そのターンの導出デルタ)。

- `context` = `affinity_reason`、`eval_skip_reason`、判定どおりの符号付き `grades`、`pending_after`、エンドポイント監査情報(読み取られた場合の `warmth_grade` / `patience_grade`、`boost_warmth` / `boost_patience`、`decay_factor`、`units`)、ペナルティが課された場合の `cross_penalty_assessed`。

- `user_message_id`(マイグレーション 0056): `chat_messages` への実際の FK、`ON DELETE SET NULL`。`proactive` / `time_decay` 行および移行前の行では NULL であり、バックフィルはされない。

- `label_changes`(§5参照)、`effective_line_deltas`(ターンごとの正確な bond/chemistry デルタ、`effective_deltas_computed` として提供される)、`state_after`(ターン後の全ベクトル、マイグレーション 0049)。`state_before` は存在するが提供されない。リプレイは直接クエリで行う。

### API インターフェース

`GET /bff/v1/comp/affinity/{session_id}` は `AffinitySnapshot` を返し、`GET /bff/v1/comp/affinities/{user_id}` も項目ごとに `AffinitySnapshot` を返す。値は読み取り時に再計算される(`apply_time_decay` + `refresh_endpoints`)。

```json
{
  "warmth": 0.52,
  "trust": 0.08,
  "intrigue": 0.12,
  "intimacy": 0.05,
  "patience": 0.27,
  "tension": 0.04,
  "bond": 0.10,
  "chemistry": 0.045,
  "bond_tier": 1,
  "chem_tier": 1,
  "bond_label": "acquaintance",
  "chemistry_label": "spark",
  "ghost_streak": 0,
  "total_ghosts": 0,
  "updated_at": "2026-08-16T12:00:00.000000Z"
}
```

`GET /bff/v1/comp/affinity/{session_id}/event` はターンごとのイベントを返す。`effective_deltas`(軸ごとの適用済み変化、`after − before`。warmth/patience についてはターンごとの導出デルタ)に加え、次を含む。

```json
{
  "session_id": "…",
  "event": {
    "event_id": "…",
    "event_type": "message",
    "effective_deltas": {
      "warmth": 0.06, "trust": 0.02, "intrigue": 0.0,
      "intimacy": 0.0, "patience": 0.01, "tension": -0.02
    },
    "effective_deltas_computed": {
      "bond": 0.01,
      "chemistry": -0.01
    },
    "label_changes": {
      "bond": { "from": "acquaintance", "to": "friend" }
    },
    "state_after": {
      "warmth": 0.58, "trust": 0.21, "intrigue": 0.09,
      "intimacy": 0.04, "patience": 0.44, "tension": 0.02,
      "bond": 0.15, "chemistry": 0.03,
      "bond_tier": 2, "chem_tier": 1,
      "warmth_grade": 2, "patience_grade": 2,
      "ghost_streak": 0, "total_ghosts": 0,
      "updated_at": "…"
    },
    "created_at": "…"
  }
}
```

`effective_deltas_computed`、`label_changes`、`state_after` は、いずれもイベント行に保存された値である。対応する `state_before` はこのレスポンスには含まれない。範囲を指定したリプレイは、`engine.companion_affinity_events` への直接クエリで行う。

## 9. 調整パラメータ

以下はサーバー側の環境変数である。未設定の場合は、それぞれのデフォルト値が使われる。

| Env var | Default | Meaning |
|---------|---------|------|
| `AFFINITY_GRADE_UNIT_BOND` | `0.0786` | trust/intrigue における、グレード1段あたりの生スコア |
| `AFFINITY_GRADE_UNIT_CHEM` | `0.0266` | intimacy/tension における、グレード1段あたりの生スコア |
| `AFFINITY_NEG_FACTOR` | `1.5` | 負の raw への追加倍率――「上がるのは遅く、下がるのは速く」を維持する |
| `AFFINITY_TIER_DECAY` | `1.0,0.70,0.45,0.25,0.10` | ティア1〜5ごとの正デルタの減衰率(カンマ区切り。ちょうど5つの有限な非負値でない場合はデフォルト表全体を維持する) |
| `AFFINITY_CROSS_PENALTY_RATIO` | `0.8333` | κ_line = ratio × u_line――損益分岐点をユニット非依存に保つ |
| `AFFINITY_CROSS_PENALTY_START` | `0.35` | ペナルティのランプが始まる相手ラインのスコア(y₀) |
| `AFFINITY_DELTA_THRESHOLD` | `0.0` | 確定閾値 θ。0 は毎ターン確定を意味する |
| `AFFINITY_DEMO_BOOST` | `1.4` | `metadata.is_demo` セッションでジャッジの正の raw に掛かる倍率 |
| `AFFINITY_FLOOR_RATIO` | `0.2` | エンドポイントの下限 φ。`0.24` でドメインキャップされており、cold ではない verdict を上書きすることは決してない |
| `AFFINITY_TIME_DECAY_RATE` | `0.02` | エンドポイントの不在減衰(1日あたり) |
| `AFFINITY_TIME_DECAY_FLOOR` | `0.5` | エンドポイントの不在減衰の下限 |

すべてのスカラー値は、起動時に許容範囲がチェックされる。不正な値はログに記録され、デフォルト値が維持される。エンジンの起動は妨げない。0.35 のピボットと `B_MAX` 1.5 はコード定数であり、調整パラメータではない。

## 10. ソースマップ

実装を直接確認する場合の参照先は、次のとおりである。

- `crates/eros-engine-core/src/affinity.rs` — 型定義、`grade_turn` 書き込みパイプライン、エンドポイント導出、時間減衰、bond/chemistry スコア、ティア、ラベル、`diff_labels`
- `crates/eros-engine-core/src/scope.rs` — `AffinityScope` / `MemoryScope`、`length_score`(§7参照)
- `crates/eros-engine-store/src/affinity.rs` — `AffinityRepo`(persist_with_event、record_ghost)、マイグレーション 0048–0049
- `crates/eros-engine-server/src/pipeline/post_process.rs` — LLM 評価、grade/level のパース
- `crates/eros-engine-server/src/prompt.rs` — affinity → attitude ディレクティブ + eval プロンプト
- `crates/eros-engine-server/src/routes/dto.rs` — `AffinitySnapshot`(複合スコア + ラベル)
- `crates/eros-engine-server/src/routes/bff/affinity.rs` — BFF affinity インターフェース(value + event)
- 設計仕様書: `docs/superpowers/specs/2026-08-16-affinity-40-design.md` — ライン数式、エンドポイント導出、ティア
- 設計仕様書: `docs/superpowers/specs/2026-08-17-affinity-41-design.md` — 永続化されたティアカラム、イベントの状態スナップショット、value エンドポイント