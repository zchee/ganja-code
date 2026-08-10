# opencode 機能リファレンス(ganja との比較)

> [!IMPORTANT]
> **本書は参照用インベントリであり、ロードマップではない。ここに記載した全ての
> 機能をポートするわけではない。** ganja の憲章は opencode **v1.18.13**(ピン)
> との挙動パリティであって、動き続ける最新版とのパリティではない。表中の ❌ は
> 観察であって約束ではなく、末尾のピン外テーブルは意図的な re-pin まで完全に
> 憲章外である。

スナップショット: 2026-08-11。ソースレベルの行はピン済みタグ
(`anomalyco/opencode@v1.18.13`)へ、ドキュメント化済みの機能は opencode.ai へ
リンクする。凡例: ✅ ganja に存在(パリティまたは近い等価物) · ⚠️ 部分的 · ❌ 不在。

## 1. ツール

| ツール | 補足 | ganja |
|---|---|---|
| [`plan_enter`](https://github.com/anomalyco/opencode/blob/v1.18.13/packages/opencode/src/tool/plan.ts) | plan モード入場 | ❌ 名前だけで実体なし |
| [`plan_exit`](https://github.com/anomalyco/opencode/blob/v1.18.13/packages/opencode/src/tool/plan.ts) | build へのハンドオフ | ✅ |
| [`lsp`](https://github.com/anomalyco/opencode/blob/v1.18.13/packages/opencode/src/tool/lsp.ts) | hover/シンボルをモデルへ公開 | ❌ deviation `lsp-tool-unported` |
| [`apply_patch`](https://github.com/anomalyco/opencode/blob/v1.18.13/packages/opencode/src/tool/apply_patch.ts) | OpenAI 系モデル限定のパッチ編集 | ❌ 権限表に名前のみ |
| [`execute`(code-mode)](https://github.com/anomalyco/opencode/blob/v1.18.13/packages/codemode) | MCP ツールをスクリプトで束ねる | ❌ パッケージごと対象外 |
| [`doom_loop`](https://github.com/anomalyco/opencode/tree/v1.18.13/packages/opencode/src/tool) | 実験的 | ❌ |
| [read / edit / write / glob / grep / bash / todowrite / webfetch / websearch / skill / question / task](https://github.com/anomalyco/opencode/tree/v1.18.13/packages/opencode/src/tool) | 実働セット | ✅ anchored write・read-before-write・権限ゲート含む |

## 2. CLI サブコマンド

| コマンド | 補足 | ganja |
|---|---|---|
| [`export`](https://opencode.ai/docs/cli) | セッション → JSON、`--sanitize` | ❌ |
| [`import`](https://opencode.ai/docs/cli) | ファイル/共有 URL から取込 | ❌(`config import-opencode` は設定のみ) |
| [`stats`](https://opencode.ai/docs/cli) | `--days/--models/--tools` 分析 | ❌ |
| [`github install` / `github run`](https://opencode.ai/docs/github) | Actions workflow + `/oc` メンション | ❌ |
| [`pr <number>`](https://opencode.ai/docs/cli) | PR を checkout して起動 | ❌ |
| [`acp`](https://opencode.ai/docs/cli) | IDE 向け Agent Client Protocol サーバー | ❌ |
| [`agent create`](https://opencode.ai/docs/agents) | 対話的エージェント生成 | ❌ |
| [`upgrade`](https://opencode.ai/docs/cli) | 自己更新 | ❌ |
| [`attach <url>`](https://opencode.ai/docs/cli) | 実行中サーバーへ TUI をアタッチ | ⚠️ headless `run --attach` のみ |
| [`web`](https://opencode.ai/docs/cli) | Web UI | ❌ |
| [`account` / `db` / `debug/*`](https://opencode.ai/docs/cli) | アカウント・DB・デバッグ | ❌ |
| [`run --fork`](https://opencode.ai/docs/cli) | 継続時のセッション分岐 | ❌ |
| [`run -f <file>`](https://opencode.ai/docs/cli) | CLI からのファイル・画像添付 | ❌(プロンプト内 `@` は✅) |
| [`serve --cors` / `--mdns`](https://opencode.ai/docs/server) | CORS 許可・mDNS 発見 | ❌(serve 自体は✅) |
| [`run` / `serve` / `auth` / `models` / `sessions` / `mcp`](https://opencode.ai/docs/cli) | 実働セット | ✅ nd-JSON 出力・`--continue`/`--session`・Basic 認証 serve 含む |

## 3. サーバー表面

| ルート/挙動 | 補足 | ganja |
|---|---|---|
| [question 応答ルート](https://opencode.ai/docs/server) | HTTP 経由の回答 | ❌ named follow-up 記録済み |
| [file/find 系ルート](https://opencode.ai/docs/server) | ファイル読・テキスト/シンボル検索 | ❌ |
| [`/api/provider`・`/api/integration`・`/api/credential`](https://opencode.ai/docs/server) | プロバイダ・統合・資格情報 API | ❌ |
| [`/api/mcp` 系](https://opencode.ai/docs/server) | サーバー側 MCP 管理+resources | ❌ |
| [`/tui` ブリッジ](https://opencode.ai/docs/server) | TUI 制御チャネル | ❌ |
| [OpenAPI スペック(`/doc`)](https://opencode.ai/docs/server) | ライブ Swagger | ❌ |
| [`/api/generate`](https://opencode.ai/docs/server) | one-shot 生成 | ❌ |
| WebSocket / mDNS / マルチディレクトリ | | ❌ 単一起動ディレクトリはピン済み divergence |
| [share 系ルート](https://opencode.ai/docs/share) | 公開・撤回 | ❌ |
| legacy `/session` REST + `/event` SSE + `/permission` | | ✅ 非 loopback 無パスワード拒否の姿勢つき |

## 4. サブシステム(ピン内)

| サブシステム | 補足 | ganja |
|---|---|---|
| [プラグイン](https://opencode.ai/docs/plugins) | JS ランタイム・npm+ローカル・ライフサイクルフック | ❌ 対象外 |
| [share](https://opencode.ai/docs/share) | `opencode.ai/s/<id>` 公開・`/unshare` | ❌ 対象外 |
| [フォーマッタ](https://github.com/anomalyco/opencode/tree/v1.18.13/packages/opencode/src/format) | 編集後の言語別自動整形 | ❌ |
| [バックグラウンドエージェント](https://github.com/anomalyco/opencode/tree/v1.18.13/packages/opencode/src/background) | 非同期派遣・要約・通知 | ❌ |
| [worktree](https://github.com/anomalyco/opencode/tree/v1.18.13/packages/opencode/src/worktree) | エージェント毎の git worktree 分離 | ❌ |
| [画像パイプライン](https://github.com/anomalyco/opencode/tree/v1.18.13/packages/opencode/src/image) | 画像添付 | ⚠️ `@` 添付✅・クリップボード取込❌ |
| account / sync / control-plane | クラウドアカウント機構 | ❌ 対象外 |
| installation(自己更新) | | ❌ |
| [IDE / ACP](https://opencode.ai/docs/ide) | エディタ拡張・サイドバーチャット | ❌ 対象外 |
| codemode | `execute` 実行基盤 | ❌ |
| desktop / web / console / slack / enterprise / identity / containers / session-ui | 兄弟プロダクト | ❌ 対象外 |

## 5. 認証・プロバイダ

| 機能 | 補足 | ganja |
|---|---|---|
| Anthropic subscription OAuth(Pro/Max) | | ❌ **dropped** — ピン時点に仕様なし |
| [models.dev プロバイダカタログ](https://opencode.ai/docs/providers) | 75+ プロバイダ | ❌ ビルトイン6+compat 2 dialect |
| MCP OAuth | リモート MCP 認証 | ❌ config キーを明示拒否 |
| [`providers login/list/logout`](https://opencode.ai/docs/providers) | 統一資格情報 UI | ⚠️ `auth` は ganja のプロバイダのみ |
| anthropic / openai(両資格情報)/ grok / copilot / cursor / fake + compat | | ✅ OAuth ログインと credential-travel 境界含む |

## 6. MCP・LSP の部分

| 機能 | ganja |
|---|---|
| [MCP prompts / resources](https://opencode.ai/docs/mcp-servers) | ❌ |
| MCP 再接続 | ❌ 一度 dial したきり |
| MCP の動的有効/無効 | ❌ |
| [モデル向け `lsp` ツール](https://github.com/anomalyco/opencode/blob/v1.18.13/packages/opencode/src/tool/lsp.ts) | ❌ |
| [LSP サーバー自動インストール](https://opencode.ai/docs/lsp) | ❌ 決してインストールしない |
| ビルトイン LSP の広さ(pyright・tsserver 等) | ⚠️ `rust`・`gopls` の2つのみ |
| 残りの診断 pull | ❌ |
| MCP stdio+remote HTTP・`<mcp_instructions>`・tools/list_changed / LSP push+pull 診断 | ✅ |

## 7. TUI — 大型サーフェス

| サーフェス | 補足 | ganja |
|---|---|---|
| [セッション rename / tag / move / export ダイアログ](https://github.com/anomalyco/opencode/tree/v1.18.13/packages/tui/src/component) | | ❌ |
| [タイムライン+任意時点フォーク](https://github.com/anomalyco/opencode/blob/v1.18.13/packages/tui/src/component/dialog-timeline.tsx) | `<leader>g` | ❌ |
| [メッセージ inspect](https://github.com/anomalyco/opencode/blob/v1.18.13/packages/tui/src/component/dialog-message.tsx) | | ❌ |
| [workspace UI](https://github.com/anomalyco/opencode/tree/v1.18.13/packages/tui/src/component) | create/list/file-changes/destination | ❌ 設計ごと対象外 |
| [サイドバー](https://github.com/anomalyco/opencode/tree/v1.18.13/packages/tui/src/feature-plugins/sidebar) | context/files/lsp/mcp/todo ペイン | ❌ |
| [diff ビューア](https://github.com/anomalyco/opencode/tree/v1.18.13/packages/tui/src/component/diff-viewer) | ファイルツリー・split/unified・hunk ナビ | ❌ インライン unified のみ |
| [サブエージェントビューア](https://github.com/anomalyco/opencode/blob/v1.18.13/packages/tui/src/component/dialog-subagent.tsx) | 子トランスクリプト閲覧 | ❌ 進捗メタデータのみ |
| [provider / MCP / skill / status / debug ピッカー](https://github.com/anomalyco/opencode/tree/v1.18.13/packages/tui/src/component) | | ❌(`/effort` ピッカーは✅) |
| 削除失敗・リトライ回復ダイアログ | | ❌ |
| [デスクトップ通知](https://github.com/anomalyco/opencode/blob/v1.18.13/packages/tui/src/notifications.ts) | | ❌ |
| toast オーバーレイ | | ⚠️ ステータスバー通知に適合・文言は逐語 |
| logo / 起動アニメーション / tips | | ❌ |
| TUI プラグイン機構 | | ❌ |
| チャット+ストリーミング・permission ダイアログ(`a`/`A`/`d`)・question(自由入力含む)・palette+メニュー・テーマ・markdown・`/undo` マーカー | | ✅ |

## 8. TUI — keybind 全レジストリ

ポート済み・rebind 可能(6): [`app_exit`・`command_list`・`session_list`・`theme_list`・`agent_cycle`・`input_newline`](https://github.com/anomalyco/opencode/blob/v1.18.13/packages/tui/src/config/keybind.ts)。
以下は同レジストリの残り。*upstream の composer に Tab 補完は存在しない —
Tab は `agent_cycle`(ポート済み)、補完メニューはフィルタ+Enter(ポート済み)。*

| アクション | 既定 | ganja |
|---|---|---|
| [`leader` + which-key](https://opencode.ai/docs/keybinds) | ctrl+x | ❌ リーダー機構全体 |
| `app_debug` / `app_console` / `app_heap_snapshot` | none | ❌ |
| `app_toggle_animations` / `_file_context` / `_diffwrap` / `_paste_summary` / `_session_directory_filter` | none | ❌ |
| `help_show` / `docs_open` | none | ❌(`/help` ✅) |
| `diff_*` — open/close/toggle/expand/expand_all/collapse/switch_focus/next_hunk/previous_hunk/next_file/previous_file/toggle_file_tree/single_patch/switch_source/toggle_view/help | esc,q · enter · `]` `[` · n p b s d v ? | ❌ 全16(diff ビューア自体なし) |
| `editor_open` | `<leader>e` | ⚠️ `/editor` ✅・キー❌ |
| `theme_switch_mode` / `theme_mode_lock` | none | ❌ |
| `sidebar_toggle` / `scrollbar_toggle` / `status_view` / `debug_view` | `<leader>b`,`<leader>s` | ❌ |
| `session_export` / `session_copy` | `<leader>x` / none | ❌ / ⚠️ `/copy` ✅ |
| `session_move` / `session_timeline` / `session_fork` / `session_rename` / `session_delete` / `session_share` / `session_unshare` | ctrl+r・ctrl+d 等 | ❌ |
| `session_new` / `session_compact` / `session_interrupt` | `<leader>n` / `<leader>c` / escape | ⚠️ `/new`・`/compact`・Esc キャンセルは✅・rebind 不可 |
| `session_background` | ctrl+b | ❌ |
| `session_toggle_timestamps` / `_generic_tool_output` | none | ❌ |
| `session_queued_prompts` | `<leader>q` | ❌ |
| `session_child_first/child_cycle/child_cycle_reverse/parent` | `<leader>down`・right・left・up | ❌ |
| `session_pin_toggle` / `session_quick_switch_1..9` | ctrl+f / `<leader>1-9` | ❌ |
| `stash_delete` | ctrl+d | ❌ |
| `model_provider_list` / `model_favorite_toggle` / `model_cycle_recent(_reverse)` / `model_cycle_favorite(_reverse)` | ctrl+a・ctrl+f・f2 | ❌(`/models` は✅) |
| `mcp_list` / `provider_connect` / `console_org_switch` | none | ❌ |
| `agent_list` / `agent_cycle_reverse` | `<leader>a` / shift+tab | ⚠️ `/agents` ✅ / ❌ |
| `variant_cycle` / `variant_list` | ctrl+t / none | ❌ 順繰り(`/effort` リストは✅・カタログ合成 roster) |
| `messages_page_up/…/half_page_down`(6) | pageup 等 | ⚠️ スクロールは✅・rebind 不可 |
| `messages_first/last/next/previous/last_user` | ctrl+g・home 等 | ❌ メッセージ単位ナビ |
| `messages_copy` / `messages_undo` / `messages_redo` / `messages_toggle_conceal` | `<leader>y/u/r/h` | ⚠️ `/copy-message`・`/undo`・`/redo` ✅・キーと conceal ❌ |
| `tool_details` / `display_thinking` | none | ❌ |
| `prompt_submit` / `prompt_editor_context_clear` / `prompt_skills` / `prompt_stash(_pop/_list)` / `workspace_set` | none | ❌ |
| `input_clear` / `input_paste` | ctrl+c / ctrl+v | ❌ / ⚠️ bracketed paste のみ |
| `input_submit` / `input_move_*` / `input_backspace` / `input_delete` | return・矢印 等 | ⚠️ 動作は✅・rebind 不可(Up/Down は履歴接続✅) |
| `input_select_*`(left/right/up/down/line/buffer/visual-line、10種) | shift+系 | ❌ 選択機構ごと不在 |
| `input_line_home/end` / `input_visual_line_home/end` / `input_buffer_home/end` | ctrl+a/e・alt+a/e・home/end | ⚠️ 一部内蔵・視覚行❌ |
| `input_delete_line` / `input_delete_to_line_end` / `input_delete_to_line_start` | ctrl+shift+d・ctrl+k・ctrl+u | ⚠️ k/u は内蔵・rebind 不可 |
| `input_undo` / `input_redo` / `input_word_*` | ctrl+-・ctrl+.・alt+f/b 等 | ⚠️ 内蔵のみ |

## 9. prompt モジュール

| モジュール | 補足 | ganja |
|---|---|---|
| [`frecency.tsx`](https://github.com/anomalyco/opencode/blob/v1.18.13/packages/tui/src/component/prompt/frecency.tsx) | 頻度+新しさの補完ランキング | ❌ |
| [`stash.tsx`](https://github.com/anomalyco/opencode/blob/v1.18.13/packages/tui/src/component/prompt/stash.tsx) | 下書き stash | ❌ |
| [`move.tsx` / `workspace.tsx` / `cwd.ts`](https://github.com/anomalyco/opencode/tree/v1.18.13/packages/tui/src/component/prompt) | セッション移動・workspace・cwd | ❌ |
| [`local-attachment.ts`](https://github.com/anomalyco/opencode/blob/v1.18.13/packages/tui/src/component/prompt/local-attachment.ts) | mime 添付 | ✅ |
| [`history.tsx`](https://github.com/anomalyco/opencode/blob/v1.18.13/packages/tui/src/prompt/history.tsx) | プロンプト履歴 | ✅ |
| [`autocomplete.tsx`](https://github.com/anomalyco/opencode/blob/v1.18.13/packages/tui/src/component/prompt/autocomplete.tsx) | `@`/`/` 補完+`#行レンジ` | ✅ |

## 10. ピン外(v1.18.13 以降 — 意図的な re-pin まで憲章外)

| 機能 | 補足 |
|---|---|
| [Queue vs Steer](https://opencode.ai/docs) | プロンプトキュー+実行中ターンへの mid-run 進路修正注入 |
| FFF 検索エンジン | rg spawn を frecency 付き in-process 検索へ置換 |
| [OpenCode Desktop](https://opencode.ai/download) | ネイティブ GUI・worktree ドロワー・通知 |
| OpenCode v2 beta | アーキテクチャ世代交代 |
| worktree ドロワー UI / agent manager | 並列エージェントの worktree 管理 |
| `opencode.jsonc` v1.0.210+ カタログ形式 | variant 宣言の統合形式 |
