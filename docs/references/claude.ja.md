# Claude Code 機能リファレンス(ganja との比較)

> [!IMPORTANT]
> **本書は参照用インベントリであり、ロードマップではない。ここに記載した全ての
> 機能をポートするわけではない。** ganja の憲章は opencode v1.18.13 との
> 挙動パリティであり、Claude Code は別プロダクトとして比較のために目録化した
> だけである。表中の ❌ は観察であって、約束ではない。

スナップショット: 2026-08-11、Claude Code 2.1.x 世代を対象。Claude Code の
更新は速いので、古くなった行は「古い行」であって ganja の退行ではない。
*(低確度)* を付した行は公式ドキュメントではなくコミュニティ情報に依る。

凡例: ✅ ganja に存在(パリティまたは近い等価物) · ⚠️ 部分的 · ❌ 不在。

## 1. Composer 入力

| 機能 | キー | ganja |
|---|---|---|
| [ファイルパスの Tab 補完](https://code.claude.com/docs/en/interactive-mode) | `@path` + Tab | ⚠️ `@` メニューはあるが Tab 確定なし(Enter のみ) |
| [スラッシュコマンド補完](https://code.claude.com/docs/en/slash-commands) | `/` | ✅ ドロップダウン+パレット |
| [ファイル mention](https://code.claude.com/docs/en/common-workflows) | `@` | ✅ `#行レンジ`・画像/PDF 添付を含む |
| [Vim モード](https://code.claude.com/docs/en/interactive-mode) | `/vim` | ❌ |
| [プロンプト履歴](https://code.claude.com/docs/en/interactive-mode) | ↑ / ↓ | ✅ 50件・重複抑止・自己修復ストア |
| [履歴の逆方向検索](https://code.claude.com/docs/en/interactive-mode) | Ctrl+R | ❌ |
| [クリップボード画像ペースト](https://code.claude.com/docs/en/interactive-mode) | Ctrl+V | ❌(添付は `@` mention 経由のみ) |
| [長文ペーストの折畳み](https://code.claude.com/docs/en/interactive-mode) | 自動 | ❌ |
| [外部エディタ](https://code.claude.com/docs/en/interactive-mode) | Ctrl+G | ⚠️ `/editor` コマンドのみ・キー直結なし |
| [複数行入力](https://code.claude.com/docs/en/interactive-mode) | Shift+Enter / Ctrl+J | ✅ upstream の4コード既定 |
| [bracketed paste](https://code.claude.com/docs/en/terminal-config) | ペースト | ✅ |
| [Bash モード](https://code.claude.com/docs/en/interactive-mode) | 行頭 `!` | ✅ |
| [メモリーショートカット](https://code.claude.com/docs/en/memory) | 行頭 `#` | ❌ |
| [入力の全消去](https://code.claude.com/docs/en/interactive-mode) | Ctrl+C | ❌(ganja の Ctrl+C は終了) |
| [テキスト選択](https://code.claude.com/docs/en/interactive-mode) | Shift+矢印 / Shift+Home/End | ❌ 選択機構ごと不在 |
| [視覚行単位の移動](https://code.claude.com/docs/en/interactive-mode) | Alt+A / Alt+E | ❌ |
| [入力のアンドゥ・リドゥ](https://code.claude.com/docs/en/interactive-mode) | Ctrl+- / Ctrl+. | ⚠️ textarea 内蔵のみ・rebind 不可 |
| [kill・word 操作の rebind](https://code.claude.com/docs/en/interactive-mode) | Ctrl+K/U、Alt+F/B 等 | ⚠️ 内蔵動作のみ・keybind 表の外 |
| [送信キーの付替え](https://code.claude.com/docs/en/settings) | 設定 | ❌ Enter 固定 |
| [メッセージキュー](https://code.claude.com/docs/en/interactive-mode) | 実行中に入力 | ❌ Busy 中は拒否 |
| [エージェント mention](https://code.claude.com/docs/en/sub-agents) | `@agent-…` | ❌ `@` はファイルのみ |
| [ドロップしたパスの mention 化](https://code.claude.com/docs/en/interactive-mode) | drag & drop | ❌ |
| [画面再描画](https://code.claude.com/docs/en/interactive-mode) | Ctrl+L | ❌ |
| [段階的な中断](https://code.claude.com/docs/en/interactive-mode) | Ctrl+C 1回/2回 | ⚠️ 一段のみ |

## 2. モード・セッション操作

| 機能 | キー | ganja |
|---|---|---|
| [permission mode 切替](https://code.claude.com/docs/en/iam) | Shift+Tab | ❌ モード概念なし。plan agent が plan mode の近似 |
| [Extended Thinking 切替](https://code.claude.com/docs/en/interactive-mode) | Tab / Cmd+T | ❌(代わりに `/effort` がレベルを選ぶ) |
| [リワインド / チェックポイント](https://code.claude.com/docs/en/checkpointing) | Esc Esc・`/rewind` | ⚠️ `/undo`・`/redo` はファイル復元のみ・会話巻戻しなし |
| [実行中タスクのバックグラウンド化](https://code.claude.com/docs/en/interactive-mode) | Ctrl+B | ❌ バックグラウンド実行自体なし |
| [トランスクリプト/verbose 切替](https://code.claude.com/docs/en/interactive-mode) | Ctrl+O | ❌ 表示は一種類 |
| [エージェント切替](https://code.claude.com/docs/en/sub-agents) | — | ✅ Tab で順繰り(ganja 独自既定)・逆順は ❌ |

## 3. スラッシュコマンド

| コマンド | 用途 | ganja |
|---|---|---|
| [`/help`](https://code.claude.com/docs/en/slash-commands) | コマンド一覧 | ✅ |
| [`/clear`](https://code.claude.com/docs/en/slash-commands) | 会話のリセット | ✅ `/new` |
| [`/model`](https://code.claude.com/docs/en/model-config) | モデル切替 | ✅ |
| [`/effort`](https://code.claude.com/docs/en/model-config) | 推論 effort | ✅ カタログ駆動 roster |
| [`/compact`](https://code.claude.com/docs/en/costs) | 手動圧縮 | ✅ 自動圧縮も |
| [`/resume`](https://code.claude.com/docs/en/common-workflows) | セッションピッカー | ✅ `/sessions`・`--continue`・`--session` |
| [`/copy`](https://code.claude.com/docs/en/slash-commands) | 出力のコピー | ✅ `/copy`・`/copy-message`(arboard + OSC 52) |
| [`/theme`](https://code.claude.com/docs/en/settings) | テーマ選択 | ✅ `/themes` + ロード可能テーマ |
| [`/agents`](https://code.claude.com/docs/en/sub-agents) | エージェント管理・作成 | ⚠️ 切替のみ・作成/編集 UI なし |
| [`/config`](https://code.claude.com/docs/en/settings) | 対話式設定 | ❌ 設定ファイルのみ |
| [`/permissions`](https://code.claude.com/docs/en/iam) | 権限の閲覧・編集 UI | ❌ 保存ルールに UI なし |
| [`/mcp`](https://code.claude.com/docs/en/mcp) | MCP 管理・認証ダイアログ | ❌ `ganja mcp` 一覧+ステータスバー通知のみ |
| [`/memory`](https://code.claude.com/docs/en/memory) | メモリーファイル編集 | ❌ |
| [`/hooks`](https://code.claude.com/docs/en/hooks) | フック管理 | ❌ フック機構ごと不在 |
| [`/statusline`](https://code.claude.com/docs/en/statusline) | ステータスバーのスクリプト化 | ❌ 固定 |
| [`/output-style`](https://code.claude.com/docs/en/output-styles) | 応答スタイル | ❌ |
| [`/context`](https://code.claude.com/docs/en/costs) | 文脈使用量の可視化グリッド | ❌ 合計のみ |
| [`/todos`](https://code.claude.com/docs/en/interactive-mode) | タスクチェックリスト表示 | ⚠️ チャット内描画のみ |
| [`/usage`](https://code.claude.com/docs/en/costs) | 使用量・コスト内訳 | ⚠️ セッション合計のみ |
| [`/doctor`](https://code.claude.com/docs/en/troubleshooting) | 自己診断 | ❌ |
| [`/export`](https://code.claude.com/docs/en/slash-commands) | 会話のエクスポート | ⚠️ `/copy` のみ |
| [`/cd`](https://code.claude.com/docs/en/slash-commands) *(低確度)* | 作業ディレクトリ変更 | ❌ 起動ディレクトリ固定は設計判断 |
| [`/add-dir`](https://code.claude.com/docs/en/common-workflows) | セッション中の追加ディレクトリ許可 | ❌ |
| [`/plugin`](https://code.claude.com/docs/en/plugins) | marketplace 追加・install・reload | ❌ |
| [`/vim`](https://code.claude.com/docs/en/interactive-mode) | vim 編集 | ❌ |

## 4. 内蔵ツール一覧

| ツール | 補足 | ganja |
|---|---|---|
| [`Read`](https://code.claude.com/docs/en/settings) | 行番号付きテキスト+画像・PDF(約20頁)・notebook | ⚠️ テキスト✅・画像/PDF は `@` 添付経由でモデルに届く(read ツールでは読めない) |
| [`Edit`](https://code.claude.com/docs/en/settings) | 厳密文字列置換・read-before-edit 強制 | ✅ 同じ規律(`FileTimes`) |
| [`Write`](https://code.claude.com/docs/en/settings) | 生成・上書き | ✅ +symlink 差替えに対する anchored I/O |
| [`NotebookEdit`](https://code.claude.com/docs/en/settings) | Jupyter セル操作 | ❌ |
| [`Glob`](https://code.claude.com/docs/en/settings) | パターンファイル検索 | ✅ in-process(ripgrep crates) |
| [`Grep`](https://code.claude.com/docs/en/settings) | 正規表現検索 | ✅ in-process |
| [`Bash`](https://code.claude.com/docs/en/settings) | チェーン対応の権限チェック付きシェル | ✅ "always" 用の arity 表を含む |
| [`BashOutput` / `KillShell`](https://code.claude.com/docs/en/settings) | バックグラウンドシェルの読取・停止 | ❌ バックグラウンドシェルなし |
| [`WebFetch`](https://code.claude.com/docs/en/settings) | URL 取得・解析 | ✅ `webfetch` |
| [`WebSearch`](https://code.claude.com/docs/en/settings) | web 検索 | ✅ `websearch`(Exa/Parallel) |
| [`Task`](https://code.claude.com/docs/en/sub-agents) | サブエージェント起動 | ✅ `task` |
| [`TodoWrite`](https://code.claude.com/docs/en/interactive-mode) | チェックリスト | ✅ `todowrite` |
| [`ExitPlanMode`](https://code.claude.com/docs/en/common-workflows) | 承認付き plan 離脱 | ✅ `plan_exit`(question ゲートの build 切替) |
| skill ツール | スキルの明示ロード | ✅ `skill` |
| question ツール | 構造化された質問 | ✅ `question`(自由入力含む) |

## 5. 権限システム詳細

| 機能 | 補足 | ganja |
|---|---|---|
| [Bash コマンドパターン](https://code.claude.com/docs/en/iam) | `Bash(npm run *)`・前置/後置/複数ワイルドカード | ⚠️ パターンルールは存在(upstream 形)・ワイルドカード文法は別物 |
| [チェーン分解](https://code.claude.com/docs/en/iam) | `&&`/`;`/`\|` を分割し全段で判定 | ⚠️ arity 表によるコマンド種別解析・分解方式ではない |
| [gitignore 形式のパスルール](https://code.claude.com/docs/en/iam) | `Edit(src/**)`・`Read(.env)`・`//` 絶対パス | ❌ ツール別パス allow/deny なし |
| [MCP ツールパターン](https://code.claude.com/docs/en/iam) | `mcp__server__tool`・サーバー一括許可 | ✅ 同じ命名・MCP は既定で ask |
| [ドメイン限定 web ルール](https://code.claude.com/docs/en/iam) | `WebFetch(domain:github.com)` | ❌ |
| [deny → ask → allow(最厳優先)](https://code.claude.com/docs/en/iam) | | ⚠️ ganja は層状 tier の後勝ち — 別のピン済みセマンティクス |
| [設定スコープ](https://code.claude.com/docs/en/settings) | user / project / project-local / CLI フラグ / managed | ⚠️ builtin < agent < config < 保存回答。local 重ね・フラグ・managed なし |
| [settings の `env` ブロック](https://code.claude.com/docs/en/settings) | スコープ毎の環境変数注入 | ❌ |
| [保存される "always" 回答](https://code.claude.com/docs/en/iam) | 承認の永続化 | ✅ プロジェクト毎ストア・シェルは arity 対応 |
| [sandbox 実行](https://code.claude.com/docs/en/sandboxing) | OS/コンテナ隔離 | ❌ 権限ゲートのみ |

## 6. hooks・自動化

| 機能 | 補足 | ganja |
|---|---|---|
| [フックイベント](https://code.claude.com/docs/en/hooks) | PreToolUse・PostToolUse・UserPromptSubmit・Notification・Stop・SubagentStop・SessionStart・SessionEnd・PreCompact(+権限判定フック) | ❌ 機構ごと不在 |
| [フックプロトコル](https://code.claude.com/docs/en/hooks) | stdin に JSON・exit 2 でツール呼出をブロック・stdout で文脈注入 | ❌ |
| [matcher](https://code.claude.com/docs/en/hooks) | ツール別正規表現(`Edit\|Write`) | ❌ |

## 7. カスタムコマンド・メモリー内部

| 機能 | 補足 | ganja |
|---|---|---|
| [コマンドファイル](https://code.claude.com/docs/en/slash-commands) | `.claude/commands/*.md` + グローバル | ✅ config 宣言コマンド |
| [`$ARGUMENTS` / `$1`・`$2`](https://code.claude.com/docs/en/slash-commands) | 引数展開 | ✅ |
| [テンプレート内 `` !`cmd` ``](https://code.claude.com/docs/en/slash-commands) | 起動時のシェル出力埋込 | ✅(P8) |
| [テンプレート内 `@path`](https://code.claude.com/docs/en/slash-commands) | ファイル埋込 | ✅(P8・mention 級添付として) |
| [frontmatter: `allowed-tools`](https://code.claude.com/docs/en/slash-commands) | コマンド毎のツール制限 | ❌(コマンド毎 agent は✅) |
| [frontmatter: `model`・`argument-hint`](https://code.claude.com/docs/en/slash-commands) | コマンド毎モデル+ヒント | ❌ |
| [CLAUDE.md 階層](https://code.claude.com/docs/en/memory) | グローバル→ルート→サブディレクトリを連結 | ⚠️ グローバル+プロジェクトの AGENTS.md 族・サブディレクトリ歩き込みなし |
| [メモリー内 `@path` import](https://code.claude.com/docs/en/memory) | インポート元相対で解決するモジュール分割 | ❌ |
| [自動メモリー](https://code.claude.com/docs/en/memory) | `~/.claude/projects/<hash>/memory/`(MEMORY.md 索引+トピックファイル)を自己保守 | ❌ |

## 8. subagents・skills・plugins

| 機能 | 補足 | ganja |
|---|---|---|
| [エージェント定義ファイル](https://code.claude.com/docs/en/sub-agents) | `.claude/agents/*.md`(name/description/model/tools) | ⚠️ config 宣言 agent(model+rules)・エージェント毎ツール許可なし |
| [記述による自動委譲](https://code.claude.com/docs/en/sub-agents) | モデルがエージェントを選ぶ | ⚠️ task ツールが記述付き roster を提示 |
| [並列サブエージェント](https://code.claude.com/docs/en/sub-agents) | 同時実行 | ❌ one-turn-at-a-time |
| [`isolation: worktree`](https://code.claude.com/docs/en/sub-agents) | worktree 内で実行 | ❌ |
| [エージェントへの skill 事前ロード](https://code.claude.com/docs/en/sub-agents) | `skills:` | ❌ |
| [SKILL.md ロード](https://code.claude.com/docs/en/skills) | | ✅ ganja の2ホーム+`skills.paths` |
| [自動トリガー+`paths` スコープ](https://code.claude.com/docs/en/skills) | 記述・パスマッチ発動 | ❌ 明示ロードのみ |
| [`context: fork`](https://code.claude.com/docs/en/skills) | fork したサブエージェントで実行し結果のみ返す | ❌ |
| [skill の `allowed-tools`](https://code.claude.com/docs/en/skills) | `mcp__*` ワイルドカード含む制限 | ❌ |
| [プラグイン: 5 コンポーネント](https://code.claude.com/docs/en/plugins) | skills・agents・hooks・MCP・LSP | ❌ |
| [marketplace](https://code.claude.com/docs/en/plugins) | `marketplace.json`・`/plugin install`・`/reload-plugins` | ❌ |

## 9. MCP 詳細

| 機能 | 補足 | ganja |
|---|---|---|
| [transport](https://code.claude.com/docs/en/mcp) | stdio・streamable HTTP・SSE | ✅ stdio+streamable HTTP・legacy SSE ❌ |
| [設定スコープ](https://code.claude.com/docs/en/mcp) | local(`~/.claude.json`)/ project(`.mcp.json`)/ user+優先順位 | ⚠️ グローバル+プロジェクト config・repo 毎 local スコープなし |
| [CLI 管理](https://code.claude.com/docs/en/mcp) | `claude mcp add/list --scope --transport` | ⚠️ `ganja mcp` は一覧のみ・追加は config 直書き |
| [OAuth](https://code.claude.com/docs/en/mcp) | PKCE・メタデータ発見・トークン更新 | ❌ config キーを明示拒否 |
| [project スコープの初回承認](https://code.claude.com/docs/en/mcp) | repo 注入サーバー対策 | ✅ より強い: 全 MCP ツールが既定で ask |
| [タイムアウト・出力上限](https://code.claude.com/docs/en/settings) | `MCP_TIMEOUT`・`MCP_TOOL_TIMEOUT`・`MAX_MCP_OUTPUT_TOKENS` | ❌ |
| 再接続 | 死んだサーバーの復帰 | ❌ 一度 dial したきり |

## 10. モデル・コンテキスト設定

| 機能 | 補足 | ganja |
|---|---|---|
| [モデルエイリアス](https://code.claude.com/docs/en/model-config) | `sonnet` / `opus` / `haiku` | ⚠️ カタログの完全 id のみ |
| [`opusplan`](https://code.claude.com/docs/en/model-config) | plan は Opus・実行は Sonnet の自動二相 | ❌ |
| [1M コンテキストエイリアス](https://code.claude.com/docs/en/model-config) | `sonnet[1m]`・`opus[1m]` | ❌ |
| [`MAX_THINKING_TOKENS`](https://code.claude.com/docs/en/settings) | thinking 予算上書き | ⚠️ カタログ由来の effort variant が予算を運ぶ |
| [自動圧縮しきい値の上書き](https://code.claude.com/docs/en/settings) *(低確度)* | 発火率の env 調整 | ❌ 固定しきい値 |
| [小型高速モデルへのルーティング](https://code.claude.com/docs/en/settings) | 背景処理を安価モデルへ | ⚠️ ganja のタイトル要求はセッションモデルに乗る |
| [環境変数面](https://code.claude.com/docs/en/settings) | `ANTHROPIC_MODEL`・`DISABLE_TELEMETRY`・proxy 等 | ⚠️ ganja は独自のより小さい `GANJA_*` 面 |

## 11. ワークスペース・セッション保存

| 機能 | 補足 | ganja |
|---|---|---|
| [`/add-dir` / `additionalDirectories`](https://code.claude.com/docs/en/common-workflows) | マルチディレクトリアクセス | ❌ 単一起動ディレクトリは設計判断 |
| [`--worktree`](https://code.claude.com/docs/en/common-workflows) | linked worktree でセッション実行 | ❌ |
| [セッショントランスクリプト](https://code.claude.com/docs/en/data-usage) | セッション毎 JSONL・resume 可能 | ✅ プロジェクト毎 SQLite・resume 可能 |
| [checkpoint ファイル履歴](https://code.claude.com/docs/en/checkpointing) | 編集前の内容ハッシュバックアップ | ⚠️ worktree スナップショット(`/undo`) |
| [shell スナップショット](https://code.claude.com/docs/en/settings) *(低確度)* | シェル環境の再現用キャプチャ | ❌ |

## 12. CLI・headless・SDK

| 機能 | 補足 | ganja |
|---|---|---|
| [print モード](https://code.claude.com/docs/en/cli-reference) | `claude -p` | ✅ `ganja run` |
| [ストリーミング JSON 出力](https://code.claude.com/docs/en/cli-reference) | `--output-format stream-json` | ✅ `--format json`(nd-JSON) |
| [セッション継続](https://code.claude.com/docs/en/cli-reference) | `--continue` / `--resume` | ✅ |
| [セッション分岐](https://code.claude.com/docs/en/cli-reference) | `--fork-session` | ❌ |
| [permission モード群](https://code.claude.com/docs/en/cli-reference) | dontAsk / acceptEdits / plan / bypass | ⚠️ `--auto` の一段のみ |
| [呼出単位のツール許可](https://code.claude.com/docs/en/iam) | `--allowedTools` パターン | ❌ config rules のみ |
| [system prompt フラグ](https://code.claude.com/docs/en/cli-reference) | append/replace × inline/file | ❌ |
| [hermetic 実行](https://code.claude.com/docs/en/cli-reference) *(低確度)* | `--bare` | ❌ |
| [スキーマ制約出力](https://code.claude.com/docs/en/cli-reference) | `--json-schema` | ❌ |
| [Agent SDK](https://docs.claude.com/en/api/agent-sdk/overview) | TS/Python でのエンジン組込み | ❌ 最近縁は `ganja-serve` + `ganja-client`(HTTP/SSE) |
| [MCP サーバーモード](https://code.claude.com/docs/en/mcp) | `claude mcp serve` | ❌ |

## 13. エンタープライズ・プラットフォーム

| 機能 | 補足 | ganja |
|---|---|---|
| [Amazon Bedrock](https://code.claude.com/docs/en/amazon-bedrock) | IAM 認証・リージョン制御 | ❌ |
| [Google Vertex AI](https://code.claude.com/docs/en/google-vertex-ai) | ADC/IAM・VPC-SC | ❌ |
| [managed settings](https://code.claude.com/docs/en/iam) | 組織強制ポリシー・MDM 配布 | ❌ |
| [OpenTelemetry export](https://code.claude.com/docs/en/monitoring-usage) | OTLP traces/metrics/logs | ❌ |
| [ネットワーク sandbox](https://code.claude.com/docs/en/network-config) | egress 許可リスト・プロキシマスキング | ❌ |
| [devcontainer feature](https://code.claude.com/docs/en/devcontainer) | 隔離コンテナの公式構成 | ❌ |
| [GitHub Actions](https://code.claude.com/docs/en/github-actions) | `@claude` メンション・自動レビュー | ❌ |
| [デスクトップアプリ](https://code.claude.com/docs/en/desktop) | セッション同期 | ❌ |
| [web / モバイルセッション](https://code.claude.com/docs/en/claude-code-on-the-web) | クラウド実行・`--teleport` 回収 | ❌ |
| [自動アップデート](https://code.claude.com/docs/en/setup) | 自己更新 | ❌ packaging 保留の設計判断 |
