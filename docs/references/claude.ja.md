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
| [`/vim`](https://code.claude.com/docs/en/interactive-mode) | vim 編集 | ❌ |

## 4. コアエージェント機能

| 機能 | 補足 | ganja |
|---|---|---|
| [プロジェクトメモリー](https://code.claude.com/docs/en/memory) | CLAUDE.md 階層 | ✅ AGENTS.md 族・三層 |
| [スコープ付き rules](https://code.claude.com/docs/en/memory) *(低確度)* | glob 発火の `.claude/rules/*.md` | ❌ |
| [自動メモリー](https://code.claude.com/docs/en/memory) | セッション横断の MEMORY.md | ❌ |
| [hooks](https://code.claude.com/docs/en/hooks) | 決定論的ライフサイクルスクリプト | ❌ |
| [subagents](https://code.claude.com/docs/en/sub-agents) | 分離コンテキストへの委譲 | ✅ `task` ツール・子トランスクリプト分離 |
| [subagents の並列実行](https://code.claude.com/docs/en/sub-agents) | 同時実行 | ❌ one-turn-at-a-time |
| [カスタムエージェント定義](https://code.claude.com/docs/en/sub-agents) | `.claude/agents/*` ファイル | ⚠️ config 宣言 agent は✅・エージェント別ツール許可なし |
| [skills](https://code.claude.com/docs/en/skills) | SKILL.md ロード | ✅ ganja の2ホーム + `skills.paths` |
| [skill の自動トリガー](https://code.claude.com/docs/en/skills) | 記述マッチで発動 | ❌ 明示ロードのみ |
| [skill の fork 実行](https://code.claude.com/docs/en/skills) *(低確度)* | `context: fork` | ❌ |
| [プラグイン+marketplace](https://code.claude.com/docs/en/plugins) | skills/agents/hooks/MCP のバンドル | ❌ |
| [checkpointing](https://code.claude.com/docs/en/checkpointing) | 編集前スナップショット+会話復元 | ⚠️ worktree スナップショット(`/undo`)のみ |
| [バックグラウンドタスク](https://code.claude.com/docs/en/interactive-mode) | 非同期実行・完了通知 | ❌ |
| [自動圧縮](https://code.claude.com/docs/en/costs) | 上限前の要約 | ✅ |
| [権限システム](https://code.claude.com/docs/en/iam) | allow/ask/deny+保存回答 | ✅ 後勝ちルール・arity 対応 "always" |
| [sandbox 実行](https://code.claude.com/docs/en/sandboxing) | OS/コンテナ隔離 | ❌ 権限ゲートのみ |
| [MCP stdio + HTTP](https://code.claude.com/docs/en/mcp) | クライアント transport | ✅ |
| [MCP の CLI 管理](https://code.claude.com/docs/en/mcp) | `claude mcp add/list` | ⚠️ `ganja mcp` は一覧のみ・追加は config 直書き |
| [MCP OAuth](https://code.claude.com/docs/en/mcp) | リモートサーバー認証 | ❌ config キーを明示拒否 |
| [MCP 再接続](https://code.claude.com/docs/en/mcp) | 死んだサーバーの復帰 | ❌ 一度 dial したきり |
| [web search / fetch ツール](https://code.claude.com/docs/en/settings) | 内蔵 web ツール | ✅ `websearch`(Exa/Parallel)・`webfetch` |
| [todo ツール](https://code.claude.com/docs/en/interactive-mode) | タスク管理 | ✅ `todowrite` |
| [LSP 診断](https://code.claude.com/docs/en/troubleshooting) | エディタ級フィードバック | ✅ opt-in LSP・編集結果に付加 |

## 5. CLI・headless・SDK

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

## 6. エンタープライズ・プラットフォーム

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
