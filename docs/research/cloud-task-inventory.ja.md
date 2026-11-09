# ganja-code の cloud 対応: task 一覧

計画ではなく調査。2026-11-09 に main `317e511` の時点で作成。

README は ganja を「subagent、teammate、並列 workstream が cloud で動き、ganja
がその control plane になる cloud-native な coding agent」と述べ、同じ箇所で
「そのどれもまだ実装されていない」と断っている（`README.md` の Design）。
この文書は、それを実現するのに要るもの、今すでに土台としてあるもの、そして
最初の 1 行を書く前に決めておくべきことを列挙する。「cloud」のもう半分、
cloud の hardware から配信される model に対して agent loop を動かす件は、
別の note で扱う。

出典: `README.md`（設計と「未実装」の注記）。GitHub の ideas index のうち、
theme #51（Cloud fleet）と idea #63、#64、#65（verdict *do*、size XL。#64 と
#65 は #63 に block される）、theme #52（Serve, attach and remote access）と
#66（*do*、M）、#67（*park*）、#68（*drop*）、#101。
`crates/ganja-core/src/teammate.rs`（`TeammateBackend`、`Spawned`、決定 D538 と
D539）、`crates/ganja-serve/AGENTS.md`、`crates/ganja-client/AGENTS.md`、
`crates/ganja-team/AGENTS.md`、`crates/ganja-teammate-local/AGENTS.md`、
`docs/decisions/crates.md`、`depgate.toml`。既存の決定 2 つ:
`ganja-teammate-local` の `-local` は cloud 側の兄弟 crate の名前を予約して
いる。2026-09-15 に、Anthropic の Remote Control protocol を transport として
再利用する案は退けられたので、transport は ganja 自身の wire になる。

## cloud 作業の土台として今あるもの

- **teammate backend の継ぎ目。** backend は 1 つの surface の factory で、
  `SpawnSpec` を `Spawned` に変える。registry は自分が何の種類を起動したかを
  知らない（`teammate.rs` の `TeammateBackend` の doc）。cloud backend は
  実装がもう 1 つ増えるだけ。
- **`ganja serve` と `ganja-client`。** HTTP と SSE 越しの engine。Basic 認証、
  既定は loopback、loopback 以外への bind は password 必須。session ごとの
  Unix socket と peer-uid の確認。`ganja run --attach <URL>` の裏にある型付き
  client。serve crate は pane backend を決して link しない（`depgate.toml` の
  規則）。
- **`ganja-team`。** ローカルの 1 directory 下にある file-backed な mailbox と
  共有 task list。mkdir による lock protocol を本物の `claude` process と
  共有する。
- **今日の隔離。** permission engine と、backend ごとの sandbox の下限。tool
  は engine の process 内でローカルの working tree に対して走る。session は
  project ごとのローカル SQLite に置かれる。

## task 表

| # | task | 今あるもの | 足りないもの | 規模 | 依存 | 先に要る判断 |
|---|---|---|---|---|---|---|
| T1 | hosting model を決める: hosted な control-plane service か、「自前で持ち込む」（利用者が cloud の VM や container で `ganja serve` を動かす）か | 今可能なのは後者のみ（serve と attach） | README は ganja を control plane と呼ぶが、誰が host するかは書かれていない（#63:「存在しない部品が要る: control-plane service」） | S（判断） | なし | あり: hosting、tenancy、課金 |
| T2 | terminal と remote worker の間の transport | `ganja-serve` の REST と SSE、`ganja-client`、Basic 認証、10 秒の SSE heartbeat | SSE stream の再接続と再開、Basic を超える認証（token、期限）、TLS の方針（serve は plain HTTP を bind する）、protocol version | M | T1 | token の形式、TLS 終端 |
| T3 | cloud teammate backend crate（`-local` が名前を予約している兄弟）。T2 の上で `TeammateBackend` と `Spawned` を実装 | 継ぎ目。参照となる形としての local backend 群 | spawn を「remote session の作成」として実装。liveness、retire、`Delivery` の種類。どの環境変数と credential を運ぶかの規則（local の `CARRIED_ENV` に remote の対応物はない） | L | T2、T5 | T1 以外になし |
| T4 | mailbox と task list をローカル filesystem から切り離す | `ganja-team` は mkdir lock 付きの file I/O で、`claude` と共有 | transport 裏打ちの postbox（socket route の `/team/{name}/message` と `/peer/receipt` はローカルにしかない。served session への宛先付けの第一歩は #66 の name registration）。ローカルの `claude` teammate 向けには file 形式を残す | L | T2 | cloud teammate が Claude の teams directory 互換を保つか |
| T5 | working tree の置き場所 | tool は lead の cwd に対して in-process で走る。`/undo` の snapshot はローカル | worker 側での repository の同期か checkout。結果を patch か commit で返す。read log と permission の worktree identity を machine 間で扱う | L | T1 | push/pull model か共有 volume か |
| T6 | 既定での隔離（#64） | permission rule と backend ごとの sandbox 下限。README は credential、SSH agent、home directory に手が届かないと約束している | cloud 側の sandbox 定義: 何を mount するか、どの環境を運ぶか、network egress の policy。全 tool の host 到達性の監査 | XL | T3、T5 | ideas index 自身の注記により #63 に block |
| T7 | scheduler と fleet（#65） | なし。並列度は pane と CPU に縛られる | queue、並列上限、配置、retry、cost 計上（「hardware ではなく予算の問題」） | XL | T3、T1（課金） | #63 に block |
| T8 | worker 上の credential | provider の key は lead の環境変数か `auth.json` | 短命の委譲 credential、または control plane 経由の proxy。worker が利用者の長命 key を持たないようにする（README 自身の隔離の主張） | M | T1、T2 | proxy か委譲 token か |
| T9 | wire 越しの permission dialog | ローカルの permission engine と TUI dialog。served session は REST で答える | 同意が要る remote の tool 呼び出しは cockpit に届いて待たねばならない。remote worker の timeout と `--auto` の意味 | M | T2、T3 | なし |
| T10 | fleet の observability | `/usage` の counter、TUI の live status、SSE event | worker ごとの状態、log の取得、cockpit での worker ごとの cost | M | T3、T7 | なし |
| T11 | test と CI | pane suite は本物の tmux が要る。live test は環境変数で gate | `ganja-testkit` の fake cloud backend。cloud の代役としてローカルの `ganja serve` を相手にする end-to-end suite。本物の cloud credential なしで回る CI | M | T3 | なし |
| T12 | docs と決定 | README の設計段落。ideas #63〜#65 | 上の境界ごとの決定 entry、新 crate の `AGENTS.md`、`depgate.toml` の規則（serve は引き続き pane backend を link しない。cloud crate 自身の allowlist） | 決定ごとに S | 各 task | なし |

## 根拠が示す順序

T1（判断）→ T2 → T3、T4、T5 を並列 → 最初の worker が動いたら T6 と T7 →
T8 と T9 は T2 と T3 を横断する。T11 と T12 は各 task に随伴する。

## repository からは分からないこと

- 意図が hosted service か self-hosted worker か（T1）。他のすべての形がこれに
  依存する。
- cloud teammate に Claude の teams directory 互換が要るか（T4）。
- 課金、tenancy、quota: tree のどこにも記述がない。
