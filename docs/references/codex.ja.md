# OpenAI Codex CLI 機能リファレンス(ganja との比較)

> [!IMPORTANT]
> **本書は参照用インベントリであり、ロードマップではない。ここに記載した全ての
> 機能をポートするわけではない。** ganja の憲章は opencode v1.18.22 との挙動
> パリティであり、Codex CLI は第三のプロダクトとして比較のために目録化した
> だけである。表中の ❌ は観察であって、約束ではない。

ganja 側セルは 2026-08-15 に post-P22 のツリーへ更新済み(Codex 側調査は
2026-08-12 のまま)。スナップショット: 2026-08-12、Codex CLI の main ブランチを対象(本リポジトリに
Codex のピンは存在しない — upstream の変化とともに行は古くなる)。
*(低確度)* を付した行は公式ドキュメントではなくコミュニティ情報に依る。

セクション構成は 3 つのリファレンス(claude・codex・opencode)共通の
アウトラインに従う。同じトピックはどの文書でも同じセクション番号にある。

凡例: ✅ ganja に存在(パリティまたは近い等価物) · ⚠️ 部分的 · ❌ 不在。

## 1. TUI — Composer・入力

| 機能 | キー | ganja |
|---|---|---|
| [`@` ファジー検索+ Tab 確定](https://developers.openai.com/codex/cli) | `@` + Tab | ✅ Tab で確定・Enter と同一挙動;ディレクトリ降下(`@dir`→`@dir/`)は未実装 — ganja のウォーカーはファイルのみを返す |
| [Esc-Esc バックトラック](https://developers.openai.com/codex/cli) | Esc Esc | ✅ アイドル時の Esc Esc がバックトラックウォークに入る(D467): 最新のユーザーメッセージがトランスクリプト上でハイライトされ、さらに Esc で一つずつ古い方へ、Enter でその直前まで巻き戻し**かつそのプロンプトを composer へ書き戻して編集できる**;他のキーは何も巻き戻さず抜け、実行中の Esc は従来通りキャンセル、`/rewind` は二段階スコープピッカーのまま |
| [メッセージキュー](https://developers.openai.com/codex/cli) | 実行中に Enter | ✅ 実行中のターンへ次のステップ境界で steer(`Command::Steer`)— Codex 自身の `input_queue`/`inject` と同じ形;steer できないもの(拒否・未消費・スラッシュコマンド)は再生キューにフォールバック(Codex の `queued_user_messages` 側に相当) |
| [クリップボード画像ペースト](https://developers.openai.com/codex/cli) | Ctrl+V | ✅ PNG をプロセス内エンコード(OS ツール呼出しなし)し、`@` mention パイプライン経由で添付 |
| [スラッシュコマンド補完](https://developers.openai.com/codex/cli) | `/` | ✅ |
| [reasoning effort のホットキー](https://github.com/openai/codex/blob/main/docs/config.md) | Alt+, / Alt+. | ❌(`/effort` リスト選択は✅) |
| プロンプト履歴 | ↑ / ↓ | ✅ |
| 複数行入力 | Shift+Enter 等 | ✅ |
| 外部エディタ | — | ✅ `/editor`(ganja 側の優位) |

## 2. TUI — 大型サーフェス・keybind

*本改訂(2026-08-12)で独立セクション化。トランスクリプトとステータスライン
の行を §1 から移し、残りは調査による。*

| 機能 | 補足 | ganja |
|---|---|---|
| [トランスクリプトオーバーレイ](https://developers.openai.com/codex/cli) | Ctrl+T | ✅ 同じキー、3タブ(完全な tool/MCP 入出力を含む展開トランスクリプト・生イベントログ・ターン毎トークン表);フルターミナル占有とバナーはこのオーバーレイ独自の表現、フッター文言は Claude Code の Ctrl+O から — 2026-08-15 以降は塗りも Codex 自身のモノクロ(どのテーマでも文字色 on 端末背景)で、各タブは末尾固定で開きストリームに追従する;スクロールは矢印・j/k・Page キーに加え vim の Ctrl+U/Ctrl+D 半ページ対(2026-08-25) |
| [ステータスライン構成](https://github.com/openai/codex/blob/main/docs/config.md) | `[tui] status_line = […]` | ✅ `tui.statusline` の要素ロースター(D469): ユーザー順の名前付き要素、幅対応、OMC HUD の描画形(メーター・git 行・任意の詳細行);要素語彙は Codex の id リストではなく ganja 自身のもので、未知の名前はロード時に拒否 |
| [オンボーディングフロー](https://developers.openai.com/codex/cli) | 初回起動時の認証選択(ChatGPT OAuth / API キー)・config 初期化 | ❌ ganja はステータスバー通知付きで fake プロバイダ起動;`auth login` は別の CLI 手順 |
| [承認ダイアログ](https://github.com/openai/codex/blob/main/docs/getting-started.md) | 実行前のコマンド/パッチのプレビュー;承認・セッション内承認・フィードバック付き拒否 | ⚠️ ganja の permission ダイアログ(allow / always / deny)— "always" はセッションと共に消えず、プロジェクト毎ストアに永続化 |
| [ネイティブ diff 描画](https://developers.openai.com/codex/cli) | `apply_patch` の変更を適用前に色付き unified diff で表示 | ⚠️ 編集毎のインライン unified diff;適用前プレビュー段はなし(permission ダイアログが呼出しを運ぶ) |
| [デスクトップ/ターミナル通知](https://github.com/openai/codex/blob/main/docs/config.md) | `[tui] notifications`(turn-complete・approval-requested)・`notification_method` osc9/bel | ✅ `tui.notifications` は bool か同じイベントフィルタ、`notification_method` は osc9/bel、端末自身のフォーカスイベントでゲートされ注視中の端末は決して鳴らない(D468) |
| keybind カスタマイズ *(低確度)* | config 経由の限定的な付替え | ⚠️ ganja の `keybinds` マップは6アクション・カンマ区切り代替・空値で解除 |

## 3. モード・実行

| 機能 | 補足 | ganja |
|---|---|---|
| [OS カーネル sandbox](https://github.com/openai/codex/blob/main/docs/sandbox.md) | macOS Seatbelt / Linux Landlock+seccomp | ❌ 権限エンジンのみ・隔離なし |
| [approval policy 多段](https://github.com/openai/codex/blob/main/docs/getting-started.md) | read-only / workspace-write / full-access ×  on-request / untrusted / never | ⚠️ ルールベース allow/ask/deny + `--auto` 一段 |
| [書込モードのネットワーク遮断](https://github.com/openai/codex/blob/main/docs/sandbox.md) | workspace-write 中は既定で `network_access = false` | ❌ 概念なし |
| [プロジェクト trust レベル](https://github.com/openai/codex/blob/main/docs/config.md) | `[projects."path"] trust_level`・未信頼ディレクトリで確認 | ❌ |
| [`shell_environment_policy`](https://github.com/openai/codex/blob/main/docs/config.md) | サブシェル環境の all/core/none 継承+include/exclude パターン | ❌ ツールはプロセス環境をそのまま継承 |
| [`--yolo` バイパス](https://github.com/openai/codex/blob/main/docs/sandbox.md) | sandbox+承認の全スキップ | ✅ 対話 TUI・`run` 双方が `--auto`+隠し `--yolo`/`--dangerously-skip-permissions` を持つ(D479): Ask ダイアログを「1回許可」で自動応答、deny は不変 — バイパスすべき sandbox が無い点は変わらず |
| [コンテナ姿勢](https://github.com/openai/codex/blob/main/docs/sandbox.md) | Docker/devcontainer 用の縮退フラグ | ❌ |

## 4. スラッシュコマンド

| コマンド | 補足 | ganja |
|---|---|---|
| [`/model`](https://developers.openai.com/codex/cli) | モデルと reasoning effort を**同一メニュー**で選択 | ⚠️ `/model`✅ + `/effort` 別コマンド・統合メニューなし |
| [`/review`](https://developers.openai.com/codex/cli) | プリセット: 未コミット / コミット指定 / ベースブランチ差分+カスタム観点 | ❌ |
| [`/diff`](https://developers.openai.com/codex/cli) | セッション全変更のビューア | ❌(編集毎のインライン diff は✅) |
| [`/compact`](https://developers.openai.com/codex/cli) | 会話の要約圧縮 | ✅ +自動圧縮 |
| [`/prompts` → Agent Skills](https://developers.openai.com/codex/cli) *(中確度)* | テンプレートは SKILL.md 標準へ移行 | ⚠️ skills は✅・テンプレ一覧 UI ❌ |
| [`/status`](https://developers.openai.com/codex/cli) | モデル・トークン・文脈・コストのダッシュボード | ⚠️ `/usage`(セッション合計・キャッシュ/推論内訳・ベンダー rate 窓・プラン上限メーター)と `/context`(カテゴリ別グリッド)+ステータスバーに分かれる;単一ダッシュボードコマンドは無い |
| [`/init`](https://developers.openai.com/codex/cli) | AGENTS.md 生成 | ✅ |
| [`/resume`](https://developers.openai.com/codex/cli) | TUI 内セッションピッカー | ✅ `/sessions` |
| [`/feedback`](https://developers.openai.com/codex/cli) | サニタイズ済み診断のベンダー送信 | ❌(テレメトリチャネル自体なし) |
| `/new` / `/quit` | セッション制御 | ✅ 相当 |
| [`/mcp`](https://github.com/openai/codex/blob/main/docs/config.md) | MCP 接続状態 | ✅ `/mcp` ダイアログ(状態・ツール数・Reconnect/Login アクション)+ `ganja mcp` CLI 一覧 |
| `/login` / `/logout` | TUI 内の資格情報切替 | ⚠️ `auth` CLI のみ |

## 5. 内蔵ツール

| 機能 | 補足 | ganja |
|---|---|---|
| [`apply_patch`](https://github.com/openai/codex/blob/main/docs/getting-started.md) | 構造化 unified diff の主編集ツール・ハーネス層で intercept(`unified_exec`) | ❌ ganja は upstream 準拠の `edit`/`write`・名前は権限表のみ |
| [`unified_exec`](https://developers.openai.com/codex/cli) *(低確度)* | 統合実行サブシステム・byte 上限付きストリーム | ⚠️ ganja のシェルにも spill/truncation 規律あり |
| [`update_plan`(plan mode)](https://developers.openai.com/codex/cli) | ライブチェックリストの描画・更新 | ⚠️ `todowrite` が最も近い |
| [`web_search` ツール](https://github.com/openai/codex/blob/main/docs/config.md) | opt-in ライブ検索 | ✅ `websearch`(Exa/Parallel) |
| [`view_image` ツール](https://github.com/openai/codex/blob/main/docs/config.md) | モデルが自発的にローカル画像をパス指定で読む | ❌ 画像文脈はユーザー添付のみ |
| シェル実行 | | ✅ `bash` |
| best-of-N *(低確度)* | N 並列生成→比較選択 | ❌ |

## 6. 権限

*本改訂(2026-08-12)で独立セクション化。モードレベルの姿勢は §3 に。*

| 機能 | 補足 | ganja |
|---|---|---|
| [対話式の承認選択肢](https://github.com/openai/codex/blob/main/docs/getting-started.md) | 1回許可 / このセッションは許可 / フィードバック付き拒否 | ⚠️ ganja: allow / always / deny — 拒否はエラーテキストとしてモデルが読む、同じループ姿勢 |
| [セッションスコープの承認記憶](https://github.com/openai/codex/blob/main/docs/getting-started.md) | "don't ask again" はメモリ上のみ・セッションと共に消える | ⚠️ ganja の "always" はプロジェクト毎に永続(シェルは arity 対応)— 意図した、ピン済みの相違 |
| [ネットワークアクセス昇格](https://github.com/openai/codex/blob/main/docs/sandbox.md) | `network_access = false` 下でネットワークが要る命令は専用の承認を上げる | ❌ ネットワークゲート概念なし |
| [未信頼プロジェクトの config 隔離](https://github.com/openai/codex/blob/main/docs/config.md) | project の `.codex/config.toml` はディレクトリ信頼後にのみ読む | ❌ ganja はプロジェクト config を無条件に読む;キュレーション済みキー拒否は別種のガード |
| [粒度付き `approval_policy` テーブル](https://github.com/openai/codex/blob/main/docs/config.md) *(低確度)* | カテゴリ毎のプロンプト規則(sandbox・MCP elicitation 等) | ❌ |
| [`/permissions` TUI 内エディタ](https://developers.openai.com/codex/cli) *(低確度)* | 有効ポリシーの点検・調整 | ❌ 保存ルールに UI なし |

## 7. hooks・自動化

*本改訂(2026-08-12)で新設。*

| 機能 | 補足 | ganja |
|---|---|---|
| [`notify` フック](https://github.com/openai/codex/blob/main/docs/config.md) | `agent-turn-complete` で JSON ペイロード付き外部プログラムを起動 | ⚠️ ganja の `hooks` は `Stop`/`Notification` で stdin に JSON envelope を渡してコマンド実行 — 同じ仕事を Claude の形で(D456) |
| [ライフサイクルフック機構](https://github.com/openai/codex/blob/main/docs/config.md) *(中確度 — 実験的)* | `[features] hooks = true` + `hooks.json`: Claude Code 形のイベント群(PreToolUse ブロック・PostToolUse・SessionStart/End 等) | ⚠️ ganja はその形を安定 config キーとして搭載: 9イベント・PreToolUse/UserPromptSubmit ブロック・正規表現 matcher |
| `PermissionRequest` / `PostCompact` イベント *(低確度)* | Codex 独自の追加ライフサイクル点 | ❌ 最も近いのは permission 待ちを挟む ganja の `Notification` |

## 8. ルール・カスタムコマンド・メモリー

| 機能 | 補足 | ganja |
|---|---|---|
| [AGENTS.md(プロジェクト+グローバル)](https://agents.md) | `~/.codex/AGENTS.md` + repo ルート | ✅ ganja も家族+グローバル層を読む |
| [ネストした AGENTS.md](https://agents.md) | サブディレクトリ毎の指示・再帰スコープ | ✅ lazy 歩き込み(D480): ツールが触れたファイルの親チェーン上の AGENTS.md 族を次リクエストへ closest-last で注入;listing(glob/grep)は touch ではない |
| [カスタムプロンプト](https://developers.openai.com/codex/cli) | `~/.codex/prompts/*.md` + プロジェクトスコープ・引数補間 | ⚠️ config 宣言コマンド(`$ARGUMENTS`/`!`/`@` 展開)が最も近い |

## 9. エージェント・スキル

*本改訂(2026-08-12)で新設。マルチエージェント面は upstream で実験的、
変化が速い。*

| 機能 | 補足 | ganja |
|---|---|---|
| [マルチエージェント編成](https://developers.openai.com/codex/cli) *(中確度)* | `multi_agent` フィーチャ: 並列サブエージェントスレッド(`[agents] max_threads`・`max_depth`)・`/agent` インスペクタ | ⚠️ ganja は `task` ツール+連続呼出しの並行 fan-out(`agents.concurrency` 上限、既定4);再帰深度もスレッドインスペクタもなし |
| [エージェント定義ファイル](https://developers.openai.com/codex/cli) *(低確度)* | `~/.codex/agents/*.toml`・エージェント毎の model/reasoning/sandbox | ⚠️ config 宣言 agent(model・prompt・permission ルール);エージェント毎 sandbox なし |
| [Skills(SKILL.md)](https://developers.openai.com/codex/cli) | クロスツール標準・progressive disclosure | ✅ ganja の2ホーム+`skills.paths` |
| [スキル探索パス](https://developers.openai.com/codex/cli) | `$CODEX_HOME/skills` + repo `.codex/skills` + `.agents/skills` | ⚠️ ganja は config ホーム+`.ganja/skills`+設定パスを走査;外来物は既定で発見しない |
| [`/skills` 一覧・`$skill-name` 起動](https://developers.openai.com/codex/cli) *(2026-08-15 確認済)* | TUI 内一覧と明示起動 | ✅ `$` 打鍵でセレクタ、Tab/Enter が `$name` を補完、送信時にエンジンが `skill` ツール自身のレンダリングへ展開;`/skills` ダイアログと `ganja skills` が一覧(D491) |

## 10. MCP・LSP

*MCP の行はツールセクションから移設。CLI の行は本改訂(2026-08-12)の
調査による。*

| 機能 | 補足 | ganja |
|---|---|---|
| [MCP クライアント](https://github.com/openai/codex/blob/main/docs/config.md) | stdio(`command`/`args`/`env`)+ streamable HTTP(`url`/`bearer_token_env_var`)・サーバー毎 enable+タイムアウト・OAuth ストア(keyring/file) | ✅ stdio+HTTP・サーバー毎 `enabled`/`timeout`/`output_limit`・静的 `headers`(bearer もここに書く);OAuth も追加(RFC 8414 発見+RFC 7591 登録+PKCE、`mcp:<server>` 予約キーに保存、D466) |
| [`codex mcp add/list/get/remove`](https://developers.openai.com/codex/cli) | `config.toml` を書き換える CLI 管理 | ✅ `ganja mcp add/list/get/remove`(D483): `ganja.toml`(D536 以降は同じ形式)への検証付き staged 書込み、toml_edit の CST で保存編集(コメント不変)、退役した `ganja.jsonc`/`ganja.json` があるディレクトリは `ganja config migrate` を案内して拒否、`get` は由来 tier を報告 |
| [`codex mcp login`](https://developers.openai.com/codex/cli) | リモートサーバーの OAuth フロー | ✅ `ganja mcp login <server>` |
| [Codex の MCP サーバー化](https://developers.openai.com/codex/cli) | エンジンを MCP として公開 | ❌ |
| 言語サーバー | なし — Codex に LSP サブシステムはない | n/a — ganja 側の優位: config 宣言 LSP(rust/gopls 内蔵+カスタム)、edit/write 結果への診断付記 |

## 11. モデル・プロバイダ・認証

*本改訂(2026-08-12)でここに集約: 設定セクションのプロバイダ行、CLI
セクションのログイン行、加えて調査行。*

| 機能 | 補足 | ganja |
|---|---|---|
| [カスタム `model_providers`](https://github.com/openai/codex/blob/main/docs/config.md) | `base_url`+`env_key`+`http_headers`+モデルリスト・`wire_api = "responses"` のみ | ✅ 強いパリティ: ganja の `provider` テーブル(dialect/base_url/key_env/headers)— しかも ganja は **2 dialect** を話す(Codex は1つに絞った) |
| [モデル選択](https://github.com/openai/codex/blob/main/docs/config.md) | `model`・`model_reasoning_effort`・`model_reasoning_summary` | ⚠️ config キー `model` と `effort`(effort は新規セッションを播種、採用時にカタログ照合;保存済みセッション自身の選択が勝つ — P17)+ `/model`・`/effort`;summary ノブなし |
| [ChatGPT OAuth / API キー](https://github.com/openai/codex/blob/main/docs/authentication.md) | 二資格情報 | ✅ 同型(ganja の `openai`) |
| [`codex login --device-auth`](https://github.com/openai/codex/blob/main/docs/authentication.md) | headless デバイスコード認証 | ✅ ganja の grok・ChatGPT ログインともデバイスフロー保持 |
| [資格情報の優先順位](https://github.com/openai/codex/blob/main/docs/authentication.md) | `CODEX_API_KEY` > `OPENAI_API_KEY` > `auth.json` | ✅ 同型: env キーが保存ログインに優先 |
| [`forced_login_method` / `forced_chatgpt_workspace_id`](https://github.com/openai/codex/blob/main/docs/config.md) *(低確度)* | エンタープライズのログイン固定 | ❌ |

## 12. 設定面(`config.toml`)

| 機能 | 補足 | ganja |
|---|---|---|
| [設定の場所+優先順位](https://github.com/openai/codex/blob/main/docs/config.md) | `$CODEX_HOME/config.toml` + 信頼済みプロジェクトの `.codex/config.toml`(セキュリティキーは project 側で上書き不可) | ⚠️ 3層 jsonc マージは✅・セキュリティキー隔離なし |
| [名前付き `[profiles]`](https://github.com/openai/codex/blob/main/docs/config.md) | `--profile` でプリセット切替 | ❌ |
| [履歴永続化の設定](https://github.com/openai/codex/blob/main/docs/config.md) *(低確度)* | sqlite/file/disabled・パス変更・上限 | ⚠️ プロジェクト毎 SQLite 固定・無効化/移設ノブなし |
| [`personality`](https://github.com/openai/codex/blob/main/docs/config.md) | pragmatic / friendly / none | ❌ |
| [表示設定](https://github.com/openai/codex/blob/main/docs/config.md) | `hide_agent_reasoning`・`model_verbosity`・TUI テーマ/マウス/行番号 | ⚠️ テーマ✅・他❌ |
| [コンテキスト上書き](https://github.com/openai/codex/blob/main/docs/config.md) | `model_context_window`・`model_auto_compact_token_limit` | ❌ カタログ駆動サイズ・固定しきい値 |
| [feature フラグ](https://github.com/openai/codex/blob/main/docs/config.md) *(低確度)* | 実験的 `[features]`: multi_agent・memories・goals・hooks・shell_snapshot・unified_exec 等 | ❌ |
| [shell completions](https://developers.openai.com/codex/cli) | bash/zsh/fish/powershell | ❌(clap で可能だが未配線) |

## 13. セッション・保存

| 機能 | 補足 | ganja |
|---|---|---|
| [rollout ファイル](https://developers.openai.com/codex/cli) | `~/.codex/sessions/…` の追記専用 JSONL+SQLite 索引 | ✅ 形は違うが同じ保証: プロジェクト毎 SQLite への write-through |
| [`codex resume` / `--last` / インラインプロンプト](https://developers.openai.com/codex/cli) | 再開+即続行 | ✅ `--continue`/`--session` + `run --continue "…"` |
| セッション fork | 会話の分岐 | ❌ |
| [`codex doctor`](https://developers.openai.com/codex/cli) *(中確度)* | config・認証・接続の診断 | ❌ |
| [`/feedback` 診断](https://developers.openai.com/codex/cli) | サニタイズ済みログのベンダー送信 | ❌ 設計として — ganja はどこにも発信しない |

## 14. CLI・headless

| 機能 | 補足 | ganja |
|---|---|---|
| [`codex exec`](https://github.com/openai/codex/blob/main/docs/exec.md) | headless 実行 | ✅ `ganja run` |
| [`exec --json`](https://github.com/openai/codex/blob/main/docs/exec.md) | JSONL イベントストリーム | ✅ `--format json` |
| [`exec --output-schema`](https://github.com/openai/codex/blob/main/docs/exec.md) | JSON Schema 制約の最終メッセージ | ❌ |
| [`exec --output-last-message <file>`](https://github.com/openai/codex/blob/main/docs/exec.md) | 最終応答をファイルへ | ❌(リダイレクトで代替) |
| [`--image <path>`](https://developers.openai.com/codex/cli) | CLI からの画像添付 | ❌ |
| 更新通知 | | ❌ |

## 15. サーバー面・SDK

| 機能 | 補足 | ganja |
|---|---|---|
| [IDE 拡張(`app-server` 経由)](https://developers.openai.com/codex/ide) | TUI・VS Code・デスクトップを1つの RPC で駆動 | ❌(ganja-serve は自クライアント向け REST+SSE で IDE プロトコルではない) |
| [TypeScript SDK](https://developers.openai.com/codex/cli) *(低確度)* | `@openai/codex-sdk` によるエージェント組込み | ❌(`ganja-client` は ganja-serve 用の手書きクライアントで組込み SDK ではない) |
| HTTP サーバー面 | なし — app-server プロトコルは公開 REST API ではない | n/a — ganja 側の優位: `ganja serve`(REST + SSE・Basic 認証)+型付き `ganja-client` |

## 16. 環境変数

*本改訂(2026-08-12)で新設。*

| 変数 | 意味 | ganja |
|---|---|---|
| [`CODEX_HOME`](https://github.com/openai/codex/blob/main/docs/config.md) | 状態・設定のホーム(`~/.codex`) | ✅ `GANJA_CONFIG_HOME` — マージではなく1ホーム |
| [`OPENAI_API_KEY`](https://github.com/openai/codex/blob/main/docs/authentication.md) | API キー資格情報 | ✅ 同じ変数 |
| [`CODEX_API_KEY`](https://github.com/openai/codex/blob/main/docs/authentication.md) *(中確度)* | Codex 専用のキー上書き | ❌ ganja 専用キー変数は意図して持たない |
| [`RUST_LOG` / `LOG_FORMAT`](https://github.com/openai/codex/blob/main/docs/config.md) *(中確度)* | tracing フィルタ+形式・ログは `$CODEX_HOME/log/` | ⚠️ `RUST_LOG` を尊重(`-v` の既定フィルタより優先)、データホームの `log/` に**ローカル**日付名の日次ファイル・7 件保持;`LOG_FORMAT` ノブは無い |
| `OPENAI_BASE_URL` | エンドポイント上書き | ✅ 同じ変数 — ただし ganja は *Responses* クライアントを向ける・https か loopback 以外は拒否 |

ganja 自身の `GANJA_*` 面はリポジトリルートの `AGENTS.md` に文書化。
Codex の `shell_environment_policy`(サブシェル環境フィルタ)は §3 に目録。

## 17. エンタープライズ・プラットフォーム・統合

*本改訂(2026-08-12)でここに集約: 旧 CLI セクションのクラウド・CI 行と
調査によるエンタープライズ行。*

| 機能 | 補足 | ganja |
|---|---|---|
| [`codex cloud` + `codex apply`](https://developers.openai.com/codex/cloud) | クラウド委譲と diff のローカル適用 | ❌ 対象外領域 |
| [GitHub Action](https://github.com/openai/codex-action) | CI 内レビュー・修正 | ❌ |
| [管理コンソール](https://developers.openai.com/codex/cli) *(中確度)* | 組織のモデル既定・ログイン固定・クレジット/使用量分析 | ❌ |
| [分析・コンプライアンス API](https://developers.openai.com/codex/cli) *(低確度)* | 使用量エクスポート・SIEM 統合 | ❌ — ganja は統合すべき発信を行わない |

参考(採点ではなく視点として): ganja が Codex に対して持つ面 — ロード可能な
TUI テーマ、`/editor`、`!` シェルパススルー、arity 対応の permission "always"、
**2 dialect のカスタムプロバイダ**、serve/attach の HTTP+SSE 面、
ファーストパーティの LSP 診断、golden differential 級のテスト規律。
