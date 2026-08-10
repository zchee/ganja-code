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
| [`@` ファジー検索+ Tab 確定](https://developers.openai.com/codex/cli) | `@` + Tab | ⚠️ `@` メニューは✅・Tab 確定なし(Enter のみ) |
| [Esc-Esc バックトラック](https://developers.openai.com/codex/cli) | Esc Esc | ❌ 過去プロンプトを選択・編集し、以降のターンを巻き戻す |
| [トランスクリプトオーバーレイ](https://developers.openai.com/codex/cli) | Ctrl+T | ❌ 生ログ・ターン毎トークン・tool/MCP 展開 |
| [メッセージキュー](https://developers.openai.com/codex/cli) | 実行中に Enter | ❌ Busy 中は拒否 |
| [クリップボード画像ペースト](https://developers.openai.com/codex/cli) | Ctrl+V | ❌(`@` 添付は✅) |
| [スラッシュコマンド補完](https://developers.openai.com/codex/cli) | `/` | ✅ |
| [reasoning effort のホットキー](https://github.com/openai/codex/blob/main/docs/config.md) | Alt+, / Alt+. | ❌(`/effort` リスト選択は✅) |
| プロンプト履歴 | ↑ / ↓ | ✅ |
| 複数行入力 | Shift+Enter 等 | ✅ |
| 外部エディタ | — | ✅ `/editor`(ganja 側の優位) |

## 2. スラッシュコマンド

| コマンド | 補足 | ganja |
|---|---|---|
| [`/model`](https://developers.openai.com/codex/cli) | モデルと reasoning effort を**同一メニュー**で選択 | ⚠️ `/model`✅ + `/effort` 別コマンド・統合メニューなし |
| [`/review`](https://developers.openai.com/codex/cli) | 未コミット/コミット/ブランチ差分の自動レビュー | ❌ |
| [`/diff`](https://developers.openai.com/codex/cli) | セッション全変更のビューア | ❌(編集毎のインライン diff は✅) |
| [`/compact`](https://developers.openai.com/codex/cli) | 会話の要約圧縮 | ✅ +自動圧縮 |
| [`/prompts` → Agent Skills](https://developers.openai.com/codex/cli) *(中確度)* | テンプレートは SKILL.md 標準へ移行 | ⚠️ skills は✅(SKILL.md 互換)・テンプレ一覧 UI ❌ |
| [`/status`](https://developers.openai.com/codex/cli) | モデル・トークン・文脈・コストのダッシュボード | ⚠️ ステータスバー+Totals のみ |
| [`/init`](https://developers.openai.com/codex/cli) | AGENTS.md 生成 | ✅ |
| `/new` / `/quit` | セッション制御 | ✅ 相当 |
| [`/mcp`](https://github.com/openai/codex/blob/main/docs/config.md) | MCP 接続状態 | ⚠️ `ganja mcp` CLI 一覧のみ |
| `/login` / `/logout` | TUI 内の資格情報切替 | ⚠️ `auth` CLI のみ |

## 3. セキュリティ・実行モード

| 機能 | 補足 | ganja |
|---|---|---|
| [OS カーネル sandbox](https://github.com/openai/codex/blob/main/docs/sandbox.md) | macOS Seatbelt / Linux Landlock+seccomp | ❌ 権限エンジンのみ・隔離なし |
| [approval policy 多段](https://github.com/openai/codex/blob/main/docs/getting-started.md) | read-only / workspace-write / full-access / on-request | ⚠️ ルールベース allow/ask/deny + `--auto` 一段 |
| [書込モードのネットワーク遮断](https://github.com/openai/codex/blob/main/docs/sandbox.md) | workspace-write 中は既定で `network_access = false` | ❌ 概念なし |
| [`--yolo` バイパス](https://github.com/openai/codex/blob/main/docs/sandbox.md) | sandbox+承認の全スキップ | ⚠️ `--auto` は deny 以外許可・バイパスすべき sandbox が無い |
| [コンテナ姿勢](https://github.com/openai/codex/blob/main/docs/sandbox.md) | Docker/devcontainer 用の縮退フラグ | ❌ |

## 4. 設定・コンテキスト

| 機能 | 補足 | ganja |
|---|---|---|
| [`config.toml` + 名前付き `[profiles]`](https://github.com/openai/codex/blob/main/docs/config.md) | `--profile` でプリセット切替 | ⚠️ 3層 config は✅・名前付きプロファイル❌ |
| [AGENTS.md(プロジェクト+グローバル)](https://agents.md) | `~/.codex/AGENTS.md` 自動ロード | ✅ ganja も家族+グローバル層を読む |
| [`personality`](https://github.com/openai/codex/blob/main/docs/config.md) | pragmatic / friendly / none | ❌ |
| [`notify` フック](https://github.com/openai/codex/blob/main/docs/config.md) | 完了・承認要求時のコマンド実行 | ❌ |
| ライフサイクルフック *(低確度)* | イベント駆動スクリプト | ❌ |
| [表示設定](https://github.com/openai/codex/blob/main/docs/config.md) | `hide_agent_reasoning` 等 | ❌ |
| [shell completions](https://developers.openai.com/codex/cli) | bash/zsh/fish/powershell | ❌(clap で可能だが未配線) |

## 5. ツール・エージェント機構

| 機能 | 補足 | ganja |
|---|---|---|
| [`apply_patch`](https://github.com/openai/codex/blob/main/docs/getting-started.md) | 構造化 unified diff による主編集ツール | ❌ ganja は upstream 準拠の `edit`/`write`・名前は権限表のみ |
| [`update_plan`(plan mode)](https://developers.openai.com/codex/cli) | ライブチェックリストの描画・更新 | ⚠️ `todowrite` が最近縁・plan 専用ツールなし |
| [`web_search` ツール](https://github.com/openai/codex/blob/main/docs/config.md) | opt-in のライブ検索 | ✅ `websearch`(Exa/Parallel) |
| シェル実行 | | ✅ `bash` |
| best-of-N *(低確度)* | N 並列生成→比較選択 | ❌ |
| [MCP クライアント](https://github.com/openai/codex/blob/main/docs/config.md) | `[mcp_servers.*]` | ✅ |
| [Codex の MCP サーバー化](https://developers.openai.com/codex/cli) | エンジンを MCP として公開 | ❌ |

## 6. CLI・headless・クラウド・統合

| 機能 | 補足 | ganja |
|---|---|---|
| [`codex exec`](https://github.com/openai/codex/blob/main/docs/exec.md) | headless 実行・`-o <file>` | ✅ `ganja run`(`-o` なし・リダイレクトで代替) |
| [`codex resume` / `--last` / インラインプロンプト](https://developers.openai.com/codex/cli) | 再開+即続行 | ✅ `--continue`/`--session` + `run --continue "…"` |
| セッション fork | | ❌ |
| [`codex cloud` + `codex apply`](https://developers.openai.com/codex/cloud) | クラウド委譲と diff のローカル適用 | ❌ 対象外領域 |
| [IDE 拡張](https://developers.openai.com/codex/ide) | VS Code/JetBrains(app server 経由) | ❌ 対象外 |
| [GitHub Action](https://github.com/openai/codex-action) | CI 内レビュー・修正 | ❌ |
| [`--image <path>`](https://developers.openai.com/codex/cli) | CLI からの画像添付 | ❌ |
| [ChatGPT OAuth / API キーのログイン](https://github.com/openai/codex/blob/main/docs/authentication.md) | | ✅ 同型の二資格情報(ganja の `openai`) |
| 更新通知 | | ❌ |

参考(採点ではなく視点として): ganja が Codex に対して持つ面 — ロード可能な
TUI テーマ、`/editor`、`!` シェルパススルー、arity 対応の permission "always"、
serve/attach の HTTP+SSE 面、golden differential 級のテスト規律。
