# OpenAI Codex CLI 機能リファレンス(ganja との比較)

> [!IMPORTANT]
> **本書は参照用インベントリであり、ロードマップではない。ここに記載した全ての
> 機能をポートするわけではない。** ganja の憲章は opencode v1.18.13 との挙動
> パリティであり、Codex CLI は第三のプロダクトとして比較のために目録化した
> だけである。表中の ❌ は観察であって、約束ではない。

スナップショット: 2026-08-11、Codex CLI の main ブランチを対象(本リポジトリに
Codex のピンは存在しない — upstream の変化とともに行は古くなる)。
*(低確度)* を付した行は公式ドキュメントではなくコミュニティ情報に依る。

凡例: ✅ ganja に存在(パリティまたは近い等価物) · ⚠️ 部分的 · ❌ 不在。

## 1. TUI — Composer・マイクロ機能

| 機能 | キー | ganja |
|---|---|---|
| [`@` ファジー検索+ Tab 確定](https://developers.openai.com/codex/cli) | `@` + Tab | ✅ Tab で確定・Enter と同一挙動;ディレクトリ降下(`@dir`→`@dir/`)は未実装 — ganja のウォーカーはファイルのみを返す |
| [Esc-Esc バックトラック](https://developers.openai.com/codex/cli) | Esc Esc | ⚠️ アイドル時の Esc Esc は ganja 自前のリワインドピッカーを開く — 過去のユーザーメッセージをチェックポイントとして選び、Both/Conversation/Files のスコープを選択;Codex と異なり composer へ過去プロンプトを書き戻して編集する機構はなく、アイドル時限定(実行中の Esc は従来通りキャンセル) |
| [トランスクリプトオーバーレイ](https://developers.openai.com/codex/cli) | Ctrl+T | ✅ 同じキー、3タブ(完全な tool/MCP 入出力を含む展開トランスクリプト・生イベントログ・ターン毎トークン表);フルターミナル占有とバナーはこのオーバーレイ独自の表現、フッター文言は Claude Code の Ctrl+O から |
| [メッセージキュー](https://developers.openai.com/codex/cli) | 実行中に Enter | ✅ 実行中のターンへ次のステップ境界で steer(`Command::Steer`)— Codex 自身の `input_queue`/`inject` と同じ形;steer できないもの(拒否・未消費・スラッシュコマンド)は再生キューにフォールバック(Codex の `queued_user_messages` 側に相当) |
| [クリップボード画像ペースト](https://developers.openai.com/codex/cli) | Ctrl+V | ✅ PNG をプロセス内エンコード(OS ツール呼出しなし)し、`@` mention パイプライン経由で添付 |
| [スラッシュコマンド補完](https://developers.openai.com/codex/cli) | `/` | ✅ |
| [reasoning effort のホットキー](https://github.com/openai/codex/blob/main/docs/config.md) | Alt+, / Alt+. | ❌(`/effort` リスト選択は✅) |
| [ステータスライン構成](https://github.com/openai/codex/blob/main/docs/config.md) | `[tui] status_line = […]` | ❌ 固定ステータスバー(テーマは✅) |
| プロンプト履歴 | ↑ / ↓ | ✅ |
| 複数行入力 | Shift+Enter 等 | ✅ |
| 外部エディタ | — | ✅ `/editor`(ganja 側の優位) |

## 2. スラッシュコマンド

| コマンド | 補足 | ganja |
|---|---|---|
| [`/model`](https://developers.openai.com/codex/cli) | モデルと reasoning effort を**同一メニュー**で選択 | ⚠️ `/model`✅ + `/effort` 別コマンド・統合メニューなし |
| [`/review`](https://developers.openai.com/codex/cli) | プリセット: 未コミット / コミット指定 / ベースブランチ差分+カスタム観点 | ❌ |
| [`/diff`](https://developers.openai.com/codex/cli) | セッション全変更のビューア | ❌(編集毎のインライン diff は✅) |
| [`/compact`](https://developers.openai.com/codex/cli) | 会話の要約圧縮 | ✅ +自動圧縮 |
| [`/prompts` → Agent Skills](https://developers.openai.com/codex/cli) *(中確度)* | テンプレートは SKILL.md 標準へ移行 | ⚠️ skills は✅・テンプレ一覧 UI ❌ |
| [`/status`](https://developers.openai.com/codex/cli) | モデル・トークン・文脈・コストのダッシュボード | ⚠️ ステータスバー+Totals のみ |
| [`/init`](https://developers.openai.com/codex/cli) | AGENTS.md 生成 | ✅ |
| [`/resume`](https://developers.openai.com/codex/cli) | TUI 内セッションピッカー | ✅ `/sessions` |
| [`/feedback`](https://developers.openai.com/codex/cli) | サニタイズ済み診断のベンダー送信 | ❌(テレメトリチャネル自体なし) |
| `/new` / `/quit` | セッション制御 | ✅ 相当 |
| [`/mcp`](https://github.com/openai/codex/blob/main/docs/config.md) | MCP 接続状態 | ✅ `/mcp` ダイアログ(状態・ツール数・Reconnect/Login アクション)+ `ganja mcp` CLI 一覧 |
| `/login` / `/logout` | TUI 内の資格情報切替 | ⚠️ `auth` CLI のみ |

## 3. セキュリティ・実行モード

| 機能 | 補足 | ganja |
|---|---|---|
| [OS カーネル sandbox](https://github.com/openai/codex/blob/main/docs/sandbox.md) | macOS Seatbelt / Linux Landlock+seccomp | ❌ 権限エンジンのみ・隔離なし |
| [approval policy 多段](https://github.com/openai/codex/blob/main/docs/getting-started.md) | read-only / workspace-write / full-access ×  on-request / untrusted / never | ⚠️ ルールベース allow/ask/deny + `--auto` 一段 |
| [書込モードのネットワーク遮断](https://github.com/openai/codex/blob/main/docs/sandbox.md) | workspace-write 中は既定で `network_access = false` | ❌ 概念なし |
| [プロジェクト trust レベル](https://github.com/openai/codex/blob/main/docs/config.md) | `[projects."path"] trust_level`・未信頼ディレクトリで確認 | ❌ |
| [`shell_environment_policy`](https://github.com/openai/codex/blob/main/docs/config.md) | サブシェル環境の all/core/none 継承+include/exclude パターン | ❌ ツールはプロセス環境をそのまま継承 |
| [`--yolo` バイパス](https://github.com/openai/codex/blob/main/docs/sandbox.md) | sandbox+承認の全スキップ | ⚠️ `--auto` は deny 以外許可・バイパスすべき sandbox が無い |
| [コンテナ姿勢](https://github.com/openai/codex/blob/main/docs/sandbox.md) | Docker/devcontainer 用の縮退フラグ | ❌ |

## 4. 設定面(`config.toml`)

| 機能 | 補足 | ganja |
|---|---|---|
| [設定の場所+優先順位](https://github.com/openai/codex/blob/main/docs/config.md) | `$CODEX_HOME/config.toml` + 信頼済みプロジェクトの `.codex/config.toml`(セキュリティキーは project 側で上書き不可) | ⚠️ 3層 jsonc マージは✅・セキュリティキー隔離なし |
| [名前付き `[profiles]`](https://github.com/openai/codex/blob/main/docs/config.md) | `--profile` でプリセット切替 | ❌ |
| [カスタム `model_providers`](https://github.com/openai/codex/blob/main/docs/config.md) | `base_url`+`env_key`+`http_headers`+モデルリスト・`wire_api = "responses"` のみ | ✅ 強いパリティ: ganja の `provider` テーブル(dialect/base_url/key_env/headers)— しかも ganja は **2 dialect** を話す(Codex は1つに絞った) |
| [`notify` フック](https://github.com/openai/codex/blob/main/docs/config.md) | 完了・承認要求時のコマンド実行 | ❌ |
| [履歴永続化の設定](https://github.com/openai/codex/blob/main/docs/config.md) *(低確度)* | sqlite/file/disabled・パス変更・上限 | ⚠️ プロジェクト毎 SQLite 固定・無効化/移設ノブなし |
| [`personality`](https://github.com/openai/codex/blob/main/docs/config.md) | pragmatic / friendly / none | ❌ |
| [表示設定](https://github.com/openai/codex/blob/main/docs/config.md) | `hide_agent_reasoning`・`model_verbosity`・TUI テーマ/マウス/行番号 | ⚠️ テーマ✅・他❌ |
| [コンテキスト上書き](https://github.com/openai/codex/blob/main/docs/config.md) | `model_context_window`・`model_auto_compact_token_limit` | ❌ カタログ駆動サイズ・固定しきい値 |
| [feature フラグ](https://github.com/openai/codex/blob/main/docs/config.md) *(低確度)* | 実験的 `[features]`: multi_agent・memories・goals・hooks・shell_snapshot・unified_exec 等 | ❌ |
| [shell completions](https://developers.openai.com/codex/cli) | bash/zsh/fish/powershell | ❌(clap で可能だが未配線) |

## 5. コンテキストファイル・プロンプト

| 機能 | 補足 | ganja |
|---|---|---|
| [AGENTS.md(プロジェクト+グローバル)](https://agents.md) | `~/.codex/AGENTS.md` + repo ルート | ✅ ganja も家族+グローバル層を読む |
| [ネストした AGENTS.md](https://agents.md) | サブディレクトリ毎の指示・再帰スコープ | ❌ サブディレクトリ歩き込みなし |
| [カスタムプロンプト](https://developers.openai.com/codex/cli) | `~/.codex/prompts/*.md` + プロジェクトスコープ・引数補間 | ⚠️ config 宣言コマンド(`$ARGUMENTS`/`!`/`@` 展開)が近縁 |
| [Skills(SKILL.md)](https://developers.openai.com/codex/cli) | クロスツール標準 | ✅ ganja の2ホーム+`skills.paths` |

## 6. ツール・エージェント機構

| 機能 | 補足 | ganja |
|---|---|---|
| [`apply_patch`](https://github.com/openai/codex/blob/main/docs/getting-started.md) | 構造化 unified diff の主編集ツール・ハーネス層で intercept(`unified_exec`) | ❌ ganja は upstream 準拠の `edit`/`write`・名前は権限表のみ |
| [`unified_exec`](https://developers.openai.com/codex/cli) *(低確度)* | 統合実行サブシステム・byte 上限付きストリーム | ⚠️ ganja のシェルにも spill/truncation 規律あり |
| [`update_plan`(plan mode)](https://developers.openai.com/codex/cli) | ライブチェックリストの描画・更新 | ⚠️ `todowrite` が最近縁 |
| [`web_search` ツール](https://github.com/openai/codex/blob/main/docs/config.md) | opt-in ライブ検索 | ✅ `websearch`(Exa/Parallel) |
| [`view_image` ツール](https://github.com/openai/codex/blob/main/docs/config.md) | モデルが自発的にローカル画像をパス指定で読む | ❌ 画像文脈はユーザー添付のみ |
| シェル実行 | | ✅ `bash` |
| best-of-N *(低確度)* | N 並列生成→比較選択 | ❌ |
| [MCP クライアント](https://github.com/openai/codex/blob/main/docs/config.md) | stdio(`command`/`args`/`env`)+ streamable HTTP(`url`/`bearer_token_env_var`)・サーバー毎 enable+タイムアウト・OAuth ストア(keyring/file) | ✅ stdio+HTTP・サーバー毎 `enabled`/`timeout`/`output_limit`・静的 `headers`(bearer もここに書く);OAuth も追加(RFC 8414 発見+RFC 7591 登録+PKCE、`mcp:<server>` 予約キーに保存、D466) |
| [Codex の MCP サーバー化](https://developers.openai.com/codex/cli) | エンジンを MCP として公開 | ❌ |

## 7. セッション・保存・診断

| 機能 | 補足 | ganja |
|---|---|---|
| [rollout ファイル](https://developers.openai.com/codex/cli) | `~/.codex/sessions/…` の追記専用 JSONL+SQLite 索引 | ✅ 形は違うが同じ保証: プロジェクト毎 SQLite への write-through |
| [`codex resume` / `--last` / インラインプロンプト](https://developers.openai.com/codex/cli) | 再開+即続行 | ✅ `--continue`/`--session` + `run --continue "…"` |
| セッション fork | 会話の分岐 | ❌ |
| [`/feedback` 診断](https://developers.openai.com/codex/cli) | サニタイズ済みログのベンダー送信 | ❌ 設計として — ganja はどこにも発信しない |

## 8. CLI・headless・クラウド・統合

| 機能 | 補足 | ganja |
|---|---|---|
| [`codex exec`](https://github.com/openai/codex/blob/main/docs/exec.md) | headless 実行 | ✅ `ganja run` |
| [`exec --json`](https://github.com/openai/codex/blob/main/docs/exec.md) | JSONL イベントストリーム | ✅ `--format json` |
| [`exec --output-schema`](https://github.com/openai/codex/blob/main/docs/exec.md) | JSON Schema 制約の最終メッセージ | ❌ |
| [`exec --output-last-message <file>`](https://github.com/openai/codex/blob/main/docs/exec.md) | 最終応答をファイルへ | ❌(リダイレクトで代替) |
| [`codex login --device-auth`](https://github.com/openai/codex/blob/main/docs/authentication.md) | headless デバイスコード認証 | ✅ ganja の grok・ChatGPT ログインともデバイスフロー保持 |
| [ChatGPT OAuth / API キー](https://github.com/openai/codex/blob/main/docs/authentication.md) | 二資格情報 | ✅ 同型(ganja の `openai`) |
| [`codex cloud` + `codex apply`](https://developers.openai.com/codex/cloud) | クラウド委譲と diff のローカル適用 | ❌ 対象外領域 |
| [IDE 拡張(`app-server` 経由)](https://developers.openai.com/codex/ide) | TUI・VS Code・デスクトップを1つの RPC で駆動 | ❌(ganja-serve は自クライアント向け REST+SSE で IDE プロトコルではない) |
| [GitHub Action](https://github.com/openai/codex-action) | CI 内レビュー・修正 | ❌ |
| [`--image <path>`](https://developers.openai.com/codex/cli) | CLI からの画像添付 | ❌ |
| 更新通知 | | ❌ |

参考(採点ではなく視点として): ganja が Codex に対して持つ面 — ロード可能な
TUI テーマ、`/editor`、`!` シェルパススルー、arity 対応の permission "always"、
**2 dialect のカスタムプロバイダ**、serve/attach の HTTP+SSE 面、golden
differential 級のテスト規律。
