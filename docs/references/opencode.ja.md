# opencode 機能リファレンス(ganja との比較)

> [!IMPORTANT]
> **本書は参照用インベントリであり、ロードマップではない。ここに記載した全ての
> 機能をポートするわけではない。** ganja の憲章は opencode **v1.18.13**(ピン)
> との挙動パリティであって、動き続ける最新版とのパリティではない。本改訂は
> スナップショット時点の最新リリース **v1.18.16** を調査対象とし、ピン以降に
> 動いた分は §18 に隔離した — ピン外は意図的な re-pin まで完全に憲章外である。
> 表中の ❌ は観察であって約束ではない。

スナップショット: 2026-08-12、v1.18.16 に対して調査。ソースレベルの行は
ganja が仕様として読むピン済みタグ(`anomalyco/opencode@v1.18.13`)へリンクする
— v1.18.14–16 の差分はバグフィックスと Desktop のみ(§18)。ドキュメント化
済みの機能は opencode.ai へリンクする。

セクション構成は 3 つのリファレンス(claude・codex・opencode)共通の
アウトラインに従う。同じトピックはどの文書でも同じセクション番号にあり、
§18 は本書だけの付録である。

凡例: ✅ ganja に存在(パリティまたは近い等価物) · ⚠️ 部分的 · ❌ 不在。

## 1. TUI — Composer・prompt モジュール

| モジュール | 補足 | ganja |
|---|---|---|
| [`frecency.tsx`](https://github.com/anomalyco/opencode/blob/v1.18.13/packages/tui/src/component/prompt/frecency.tsx) | 頻度+新しさの補完ランキング | ❌ |
| [`stash.tsx`](https://github.com/anomalyco/opencode/blob/v1.18.13/packages/tui/src/component/prompt/stash.tsx) | 下書き stash | ❌ |
| [`move.tsx` / `workspace.tsx` / `cwd.ts`](https://github.com/anomalyco/opencode/tree/v1.18.13/packages/tui/src/component/prompt) | セッション移動・workspace・cwd | ❌ |
| [`local-attachment.ts`](https://github.com/anomalyco/opencode/blob/v1.18.13/packages/tui/src/component/prompt/local-attachment.ts) | mime 添付 | ✅ |
| [`history.tsx`](https://github.com/anomalyco/opencode/blob/v1.18.13/packages/tui/src/prompt/history.tsx) | プロンプト履歴 | ✅ |
| [`autocomplete.tsx`](https://github.com/anomalyco/opencode/blob/v1.18.13/packages/tui/src/component/prompt/autocomplete.tsx) | `@`/`/` 補完+`#行レンジ` | ✅ |

## 2. TUI — 大型サーフェス・keybind

### 大型サーフェス

| サーフェス | 補足 | ganja |
|---|---|---|
| [セッション rename / tag / move / export ダイアログ](https://github.com/anomalyco/opencode/tree/v1.18.13/packages/tui/src/component) | | ❌ |
| [タイムライン+任意時点フォーク](https://github.com/anomalyco/opencode/blob/v1.18.13/packages/tui/src/component/dialog-timeline.tsx) | `<leader>g` | ⚠️ チェックポイント一覧の半分は移植済み — `/rewind` + アイドル時の Esc Esc がユーザーメッセージを新しい順に列挙、Timeline のピッカーと同じ形;過去時点からのセッション fork は未移植(セッション fork ❌、§14) |
| [メッセージ inspect](https://github.com/anomalyco/opencode/blob/v1.18.13/packages/tui/src/component/dialog-message.tsx) | | ⚠️ Revert は `/rewind` の第二段(`Command::RevertTo`、Both/Conversation/Files スコープ — upstream の単一 revert の上位互換)として移植済み;Fork は未移植(セッション fork なし)、Copy はこのピッカーからは呼べない(`/copy-message` が独立コマンドとして存在) |
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

### keybind 全レジストリ

ポート済み・rebind 可能(6): [`app_exit`・`command_list`・`session_list`・`theme_list`・`agent_cycle`・`input_newline`](https://github.com/anomalyco/opencode/blob/v1.18.13/packages/tui/src/config/keybind.ts)。
以下は同レジストリの残り。*upstream にも `@`/`/` メニュー自身の Tab 補完が
存在する。独立した `prompt.autocomplete.complete` バインドで(下記6つのポート済み
トップレベルアクションにも下表の行にも含まれない)、upstream の Tab はディレクト
リなら選択前にその場で展開もする(`autocomplete.tsx:618-627`)。ganja も両メ
ニューで Tab 確定するようになった(`@` は Enter と同一挙動、`/` は実行せず
補完のみで Claude Code 由来の提示上の divergence、D446)が、ganja の `@` ウォー
カーはファイルのみを返すためディレクトリ降下は未移植。両メニューが閉じている
とき Tab は引き続き `agent_cycle`(ポート済み)を兼ねる。*

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
| `session_move` / `session_timeline` / `session_fork` / `session_rename` / `session_delete` / `session_share` / `session_unshare` | ctrl+r・ctrl+d 等 | ⚠️ `session_timeline` の一覧機能は移植済み(`/rewind` + アイドル時の Esc Esc がユーザーメッセージのチェックポイントを新しい順に列挙、Timeline と同じ形)だが `<leader>g` キー自体はなし;残り6つ(move/fork/rename/delete/share/unshare)は❌のまま |
| `session_new` / `session_compact` / `session_interrupt` | `<leader>n` / `<leader>c` / escape | ⚠️ `/new`・`/compact`・Esc キャンセルは✅・rebind 不可 |
| `session_background` | ctrl+b | ❌ |
| `session_toggle_timestamps` / `_generic_tool_output` | none | ❌ |
| `session_queued_prompts` | `<leader>q` | ⚠️ ganja のキュー欄が composer 上部に steer 未消費のエントリを表示し、Up で最新のものを呼戻し・撤回できる — 発想は同じだが専用の leader キー一覧ダイアログはなし |
| `session_child_first/child_cycle/child_cycle_reverse/parent` | `<leader>down`・right・left・up | ❌ |
| `session_pin_toggle` / `session_quick_switch_1..9` | ctrl+f / `<leader>1-9` | ❌ |
| `stash_delete` | ctrl+d | ❌ |
| `model_provider_list` / `model_favorite_toggle` / `model_cycle_recent(_reverse)` / `model_cycle_favorite(_reverse)` | ctrl+a・ctrl+f・f2 | ❌(`/models` は✅) |
| `mcp_list` / `provider_connect` / `console_org_switch` | none | ⚠️ `mcp_list` のサーバー状態閲覧は `/mcp` コマンドとして移植(P13、専用キーなし)/ ❌ / ❌ |
| `agent_list` / `agent_cycle_reverse` | `<leader>a` / shift+tab | ⚠️ `/agents` ✅ / ❌ |
| `variant_cycle` / `variant_list` | ctrl+t / none | ❌ 順繰り(`/effort` リストは✅・カタログ合成 roster) |
| `messages_page_up/…/half_page_down`(6) | pageup 等 | ⚠️ スクロールは✅・rebind 不可 |
| `messages_first/last/next/previous/last_user` | ctrl+g・home 等 | ❌ メッセージ単位ナビ |
| `messages_copy` / `messages_undo` / `messages_redo` / `messages_toggle_conceal` | `<leader>y/u/r/h` | ⚠️ `/copy-message`・`/undo`・`/redo` ✅・キーと conceal ❌ |
| `tool_details` / `display_thinking` | none | ❌ |
| `prompt_submit` / `prompt_editor_context_clear` / `prompt_skills` / `prompt_stash(_pop/_list)` / `workspace_set` | none | ❌ |
| `input_clear` / `input_paste` | ctrl+c / ctrl+v | ❌ / ⚠️ bracketed paste(自動・テキスト)は✅;ctrl+v 自体もクリップボード画像ペースト用に配線済み(PNG・プロセス内エンコード、D449)— 画像には bracketed 経路がないため |
| `input_submit` / `input_move_*` / `input_backspace` / `input_delete` | return・矢印 等 | ⚠️ 動作は✅・rebind 不可(Up/Down は履歴接続✅) |
| `input_select_*`(left/right/up/down/line/buffer/visual-line、10種) | shift+系 | ❌ 選択機構ごと不在 |
| `input_line_home/end` / `input_visual_line_home/end` / `input_buffer_home/end` | ctrl+a/e・alt+a/e・home/end | ⚠️ 一部内蔵・視覚行❌ |
| `input_delete_line` / `input_delete_to_line_end` / `input_delete_to_line_start` | ctrl+shift+d・ctrl+k・ctrl+u | ⚠️ k/u は内蔵・rebind 不可 |
| `input_undo` / `input_redo` / `input_word_*` | ctrl+-・ctrl+.・alt+f/b 等 | ⚠️ 内蔵のみ |

## 3. モード・実行

*本改訂(2026-08-12)で新設: opencode にエージェント以外のモード機構は
なく、共通アウトラインのこの位置でそれを明記する。*

| 機能 | 補足 | ganja |
|---|---|---|
| [モードとしてのエージェント](https://opencode.ai/docs/agents) | `build`/`plan` のプライマリがモード概念そのもの・Tab で巡回 | ✅ 同じ形 — Tab が ganja のプライマリを巡回 |
| [plan エージェントの姿勢](https://opencode.ai/docs/agents) | plan は既定で編集とシェルを拒否・`plan_exit` が build へハンドオフ | ✅ question ゲートの `plan_exit` 含む |
| [セッション開始エージェント](https://opencode.ai/docs/cli) | `--agent`・`default_agent` 設定 | ✅ 両方 |
| [sandbox 分離なし](https://opencode.ai/docs/permissions) | ホスト上で直接実行・permission エンジンが境界の全て | ✅ 同じ姿勢、意図的に |

## 4. コマンドとスキル

| 機能 | 補足 | ganja |
|---|---|---|
| [`/init` ビルトイン](https://github.com/anomalyco/opencode/blob/v1.18.13/packages/opencode/src/command/index.ts) | `AGENTS.md` のガイド付きセットアップ | ✅ テンプレート逐語 |
| [`/review` ビルトイン](https://github.com/anomalyco/opencode/blob/v1.18.13/packages/opencode/src/command/index.ts) | `[commit\|branch\|pr]`、サブタスクとして実行 | ❌ |
| [Markdown コマンドファイル](https://opencode.ai/docs/commands) | 両スコープの `command/` または `commands/`、ファイル名がコマンド名 | ❌ config 宣言のみ |
| [frontmatter](https://opencode.ai/docs/commands) | `description`・`agent`・`model` | ✅ config の等価物 |
| [`subtask: true`](https://github.com/anomalyco/opencode/blob/v1.18.13/packages/opencode/src/command/index.ts) | コマンドを子セッションで実行 | ❌ |
| [`$ARGUMENTS` / `$1..$N`](https://opencode.ai/docs/commands) | 全文、または位置トークン(最大番号のプレースホルダが残り全部を取る) | ✅ クォート付きトークン含む |
| [`` !`cmd` `` 置換](https://opencode.ai/docs/commands) | シェル出力をテンプレートへ挿入 | ✅ プロジェクトルートで spawn。stderr 合流と失敗自己報告は命名済み deviation |
| [`@file` 参照](https://opencode.ai/docs/commands) | composer メンション同様に添付 | ✅ |
| [MCP prompts のコマンド化](https://github.com/anomalyco/opencode/blob/v1.18.13/packages/opencode/src/command/index.ts) | サーバーの prompts が slash コマンドとして現れる | ❌ MCP はツールのみ |
| [スキル(SKILL.md)](https://opencode.ai/docs/skills) | config home + プロジェクト + `skills.paths`、`skill` ツールが要求時に読込 | ✅ |
| [外部スキル探索](https://github.com/anomalyco/opencode/blob/v1.18.13/packages/opencode/src/skill/index.ts) | `.claude/` と `.agents/` も走査(`OPENCODE_DISABLE_EXTERNAL_SKILLS` で停止) | ❌ standing ruling: 外部由来は一切探索しない。`skills.paths` 一行で届く |

## 5. ツール

| ツール | 補足 | ganja |
|---|---|---|
| [`plan_enter`](https://github.com/anomalyco/opencode/blob/v1.18.13/packages/opencode/src/tool/plan.ts) | plan モード入場 | ❌ 名前だけで実体なし |
| [`plan_exit`](https://github.com/anomalyco/opencode/blob/v1.18.13/packages/opencode/src/tool/plan.ts) | build へのハンドオフ | ✅ |
| [`lsp`](https://github.com/anomalyco/opencode/blob/v1.18.13/packages/opencode/src/tool/lsp.ts) | hover/シンボルをモデルへ公開 | ❌ deviation `lsp-tool-unported` |
| [`apply_patch`](https://github.com/anomalyco/opencode/blob/v1.18.13/packages/opencode/src/tool/apply_patch.ts) | OpenAI 系モデル限定のパッチ編集 | ❌ 権限表に名前のみ |
| [`execute`(code-mode)](https://github.com/anomalyco/opencode/blob/v1.18.13/packages/codemode) | MCP ツールをスクリプトで束ねる | ❌ パッケージごと対象外 |
| [`doom_loop`](https://github.com/anomalyco/opencode/tree/v1.18.13/packages/opencode/src/tool) | 実験的 | ❌ |
| [スピル/切り詰め規律](https://github.com/anomalyco/opencode/blob/v1.18.13/packages/opencode/src/tool/truncate.ts) | 巨大なツール出力を stale 掃除つきスピルディレクトリへ切り詰め | ✅ ganja 自前のスピルファイル・テスト時はリダイレクト |
| [read / edit / write / glob / grep / bash / todowrite / webfetch / websearch / skill / question / task](https://github.com/anomalyco/opencode/tree/v1.18.13/packages/opencode/src/tool) | 実働セット | ✅ anchored write・read-before-write・権限ゲート含む |

## 6. パーミッション文法

| 機能 | 補足 | ganja |
|---|---|---|
| [3 アクション](https://opencode.ai/docs/permissions) | ツール毎、またはツール配下のパターン毎に `allow` / `ask` / `deny` | ✅ |
| [末尾一致優先](https://github.com/anomalyco/opencode/blob/v1.18.13/packages/opencode/src/permission/index.ts) | ルールセットを `findLast`。呼び出しの全パターンが allow でなければ確認 | ✅ エンジンの中核規則 |
| [レイヤリング](https://opencode.ai/docs/permissions) | ビルトイン既定 < エージェントのルール < config ルール < 保存済み回答 | ✅ |
| [bash パターン](https://opencode.ai/docs/permissions) | コマンド文字列へのワイルドカード。"always" 回答は arity 表でコマンドの*種類*を記憶 | ✅ |
| [edit グループ](https://github.com/anomalyco/opencode/blob/v1.18.13/packages/opencode/src/permission/index.ts) | `edit` が `edit`・`write` **かつ** `apply_patch` を統べる | ✅ 権限表に記名 |
| [`~/` と `$HOME` の展開](https://github.com/anomalyco/opencode/blob/v1.18.13/packages/opencode/src/permission/index.ts) | パスパターン内 | ✅ |
| [エージェント毎の上書き](https://opencode.ai/docs/permissions) | エージェントのルールがグローバルの後ろへ付く | ✅ |
| サブエージェントの継承 | | ✅ ただし文書化済み divergence: 拒否のみ継承し、許可は決して継承しない |
| [`OPENCODE_PERMISSION`](https://opencode.ai/docs/permissions) | 環境変数からのインライン JSON ルールセット | ❌ |

## 7. hooks・自動化

*本改訂(2026-08-12)で新設: ピンにフック機構はなく、共通アウトラインの
この位置はその逆転 — upstream にないものを ganja が持つ — を記録する。*

| 機能 | 補足 | ganja |
|---|---|---|
| ライフサイクル時点でのコマンドフック | ピンには存在しない(v1.18.16 も同様)。upstream の拡張の継ぎ目は JS プラグインランタイム(§17) | ✅ ganja 側の追加(D456): `hooks` config キーが 9 つの Claude 型イベントでコマンドを実行・PreToolUse/UserPromptSubmit はブロック可能・正規表現 matcher |
| [プラグインのフックポイント](https://opencode.ai/docs/plugins) | `tool.execute.before/after`・`permission.ask`・`chat.message`・イベントバス — コマンドではなく JS 関数 | ❌ JS ランタイムは対象外;ganja のコマンドフックはツール前後と permission 待ちの時点を別の形でカバーする |
| ターン終了時の通知 | デスクトップ通知(§2) | ⚠️ `Stop`/`Notification` フックで任意の通知コマンドを実行できる;組込みのデスクトップ通知はなし |

## 8. ルールとインストラクション

| 機能 | 補足 | ganja |
|---|---|---|
| [グローバル `AGENTS.md`](https://opencode.ai/docs/rules) | 設定ディレクトリ直下 | ✅ ganja の config home |
| [`~/.claude/CLAUDE.md` フォールバック](https://github.com/anomalyco/opencode/blob/v1.18.13/packages/opencode/src/session/instruction.ts) | Claude Code 互換。`OPENCODE_DISABLE_CLAUDE_CODE_PROMPT` で停止 | ✅ グローバルのフォールバックとして読む。停止ノブは ❌ |
| [プロジェクト遡上](https://github.com/anomalyco/opencode/blob/v1.18.13/packages/opencode/src/session/instruction.ts) | `AGENTS.md` → `CLAUDE.md` → `CONTEXT.md`(deprecated)、各階層で先勝ち・祖先を積み上げない | ✅ |
| [`instructions` 設定](https://opencode.ai/docs/rules) | 追加ファイル・glob を追記 | ✅ |

## 9. エージェント

| 機能 | 補足 | ganja |
|---|---|---|
| [ビルトイン](https://github.com/anomalyco/opencode/blob/v1.18.13/packages/opencode/src/agent/agent.ts) | `build`・`plan`・`general`・`explore` | ✅ 4 つ全て、explore のプロンプトは逐語 |
| [隠し内部エージェント](https://github.com/anomalyco/opencode/blob/v1.18.13/packages/opencode/src/agent/agent.ts) | `compaction`・`title`・`summary` をエージェントとしてモデル化 | ❌ ganja は同じ仕事をエージェント名簿の外でやる |
| [Markdown エージェントファイル](https://opencode.ai/docs/agents) | `~/.config/opencode/agent/*.md` + `.opencode/agent/*.md`、frontmatter+本文がプロンプト | ❌ config 宣言のみ |
| [config フィールド](https://opencode.ai/docs/agents) | `description`・`mode`(`primary`/`subagent`/`all`)・`hidden`・`disable`・`model`・`prompt`・`permission` | ✅ 7 つ全て |
| [サンプリング系フィールド](https://github.com/anomalyco/opencode/blob/v1.18.13/packages/opencode/src/agent/agent.ts) | `temperature`・`top_p`・`color`・`steps` | ❌ |
| エージェント内の未知フィールド | 拒否せず運ぶ(upstream が許容) | ✅ 同じ姿勢 |
| Tab 巡回 / `<leader>a` 一覧 | | ⚠️ Tab ✅、一覧は `/agents` |

## 10. MCP と LSP

| 機能 | 補足 | ganja |
|---|---|---|
| [ローカル MCP サーバー](https://opencode.ai/docs/mcp-servers) | `command[]`・`environment`・`enabled` | ✅ |
| [リモート MCP サーバー](https://opencode.ai/docs/mcp-servers) | `url`・`headers`・`enabled` | ✅ |
| `<mcp_instructions>`・tools/list_changed | | ✅ |
| [MCP prompts / resources](https://opencode.ai/docs/mcp-servers) | prompts はコマンド化、resources は列挙可能 | ❌ |
| MCP 再接続 / 動的有効・無効 | | ⚠️ 再接続は✅(P13、upstream 対応物なし — `Failed` サーバー向け手動 `/mcp` Reconnect + セッション1回限りの自動リトライ、D463);実行時の動的有効・無効はまだ❌、`enabled` は config ファイルのみ |
| [LSP ビルトインの広さ](https://opencode.ai/docs/lsp) | typescript・pyright・gopls・rust-analyzer・clangd・zls・elixir-ls 等 | ⚠️ `rust` と `gopls` の 2 つのみ |
| [LSP 自動インストール](https://opencode.ai/docs/lsp) | サーバーをダウンロード(`OPENCODE_DISABLE_LSP_DOWNLOAD` で停止) | ❌ 決してインストールしない |
| [カスタム LSP サーバー](https://opencode.ai/docs/lsp) | `command`・`extensions`・`env`・`initialization`・エントリ毎 `disabled` | ✅ |
| edit/write 時の診断 | push + pull、エラーのみ、単一の継ぎ目で追記 | ✅ |
| 残りの診断 pull・[`lsp` ツール](https://github.com/anomalyco/opencode/blob/v1.18.13/packages/opencode/src/tool/lsp.ts) | | ❌ |

## 11. 認証とプロバイダ

| 機能 | 補足 | ganja |
|---|---|---|
| [models.dev プロバイダカタログ](https://opencode.ai/docs/providers) | 75+ プロバイダを id 解決 | ❌ ビルトイン 6 + compat 2 dialect |
| [OpenCode Zen](https://opencode.ai/docs/zen) | ホスト型ゲートウェイ。`opencode/` プレフィクス・単一キー・無料モデルのローテーション | ❌ 対象外 |
| [npm `@ai-sdk/*` プロバイダローダ](https://opencode.ai/docs/providers) | 任意の Vercel AI SDK パッケージをプロバイダ化 | ❌ ganja の `compat` は固定 2 dialect を話す |
| [プロバイダ options](https://opencode.ai/docs/providers) | `baseURL`・`apiKey`・`headers` | ✅ `base_url`・`key_env`・`headers` として |
| [モデル毎のカタログ上書き](https://opencode.ai/docs/providers) | `models.<id>.name` / `limit.context` / `limit.output` | ❌ config プロバイダは uncataloged のまま |
| [モデル毎の options](https://opencode.ai/docs/models) | `reasoningEffort`・`textVerbosity`・`thinking.budgetTokens` の素通し | ⚠️ effort roster が reasoning options を合成(budget 演算含む)。`textVerbosity` と素通しは ❌ |
| [Variants](https://opencode.ai/docs/models) | カタログ宣言 + プロバイダ毎のハードコード。`--variant`・`variant_cycle` ctrl+t | ⚠️ 同じ合成 roster を `/effort` として提供。CLI フラグと巡回キーはなし |
| [`small_model`](https://opencode.ai/docs/config) | タイトルと要約 | ✅ |
| Anthropic subscription OAuth(Pro/Max) | upstream は Anthropic の規約遵守のため削除。コミュニティプラグインは自己責任で存在 | n/a — ganja は最初から持たず、ピン時点に仕様もなかった |
| xAI device-code ログイン | [v1.18.14](https://github.com/anomalyco/opencode/releases/tag/v1.18.14) から単一フロー | ✅ ganja の grok ログインは元から device flow — 両者が収斂 |
| MCP OAuth | リモート MCP 認証 | ✅ P13 の追加、upstream に対応物なし — v1.18.13 チェックアウトは今も `oauth` キーを明示拒否: RFC 8414 発見+RFC 7591 登録+PKCE/loopback+401 時の refresh-then-redial を `mcp:<server>` 予約キーに保存(D466) |
| [`providers login/list/logout`](https://opencode.ai/docs/providers) | 統一資格情報 UI | ⚠️ `auth` は ganja のプロバイダのみ |
| anthropic / openai(両資格情報)/ grok / copilot / cursor / fake + compat | ganja の名簿。cursor は ganja 独自で upstream に対応物なし | ✅ OAuth ログインと credential-travel 境界含む |

## 12. 設定サーフェス

まず機構、次に `opencode.json(c)` のトップレベルキー。

| 機構 | 補足 | ganja |
|---|---|---|
| [配置と優先順位](https://opencode.ai/docs/config) | remote `.well-known/opencode` < グローバル `~/.config/opencode/opencode.json(c)` < `OPENCODE_CONFIG` < プロジェクトルートと `.opencode/`(git ルートまで遡上)< `OPENCODE_CONFIG_CONTENT` < managed/enterprise | ⚠️ 3 層のみ: グローバルホーム < `GANJA_CONFIG` < プロジェクトファイル — remote・inline・managed 層なし |
| [`$schema`](https://opencode.ai/docs/config) | エディタ補完 | ✅ 受理(して無視) |
| [`{env:VAR}` / `{file:path}` 置換](https://opencode.ai/docs/config) | 任意の文字列値で動的展開 | ❌ 意図的 divergence として `config.rs` に明記 — 代わりに `key_env` が変数名を持つ |
| JSONC 方言 | コメント・末尾カンマ | ✅ 文書順を保って解読 |
| 未知のトップレベルキー | ピンは設定パースを**失敗**させる。**v1.18.16 は無視する**([release](https://github.com/anomalyco/opencode/releases/tag/v1.18.16)) | ✅ ganja はピンの姿勢を維持: 意図的に名指しで拒否 |
| [`tui.json`](https://github.com/anomalyco/opencode/blob/v1.18.13/packages/tui/src/config/index.tsx)(`OPENCODE_TUI_CONFIG`) | 独立した TUI 設定: `theme`・`keybinds`・`leader_timeout`・`scroll_speed`・`scroll_acceleration`・`diff_style`・`mouse`・`attention` | ❌ 第二のファイルなし — ganja の `theme`/`keybinds` は `ganja.jsonc` 内、scroll/diff/mouse 系ノブは不在 |

| トップレベルキー | 補足 | ganja |
|---|---|---|
| [`model`](https://opencode.ai/docs/config) | `provider/model` 既定 | ✅ |
| [`small_model`](https://opencode.ai/docs/config) | タイトル・要約用の安価なモデル | ✅ |
| [`username`](https://opencode.ai/docs/config) | 表示名 | ❌ |
| [`autoupdate`](https://opencode.ai/docs/config) | `true` / `false` / `"notify"` | ❌ 自己更新機構ごと不在 |
| [`share`](https://opencode.ai/docs/share) | `manual` / `auto` / `disabled` | ❌ share サブシステムなし |
| [`disabled_providers`](https://opencode.ai/docs/config) | プロバイダの全体無効化 | ❌ |
| [`instructions`](https://opencode.ai/docs/rules) | 追加ルールファイル・glob 可 | ✅(置換の行のとおり `{file:}` は不可) |
| [`permission`](https://opencode.ai/docs/permissions) | §6 | ✅ |
| [`provider`](https://opencode.ai/docs/providers) | §11 | ✅ npm ベースでなく dialect ベース |
| [`agent`](https://opencode.ai/docs/agents) | §9 | ✅ config テーブルのみ |
| [`command`](https://opencode.ai/docs/commands) | §4 | ✅ config テーブルのみ |
| [`mcp`](https://opencode.ai/docs/mcp-servers) | §10 | ✅ |
| [`formatter`](https://opencode.ai/docs/formatters) | §17 | ❌ |
| [`lsp`](https://opencode.ai/docs/lsp) | §10 | ✅ |
| [`plugin`](https://opencode.ai/docs/plugins) | npm 指定 + ローカル `{plugin,plugins}/*.{ts,js}` | ❌ 対象外 |
| [`snapshot`](https://opencode.ai/docs/config) | undo/redo スナップショットの切替 | ✅ |
| [`watcher.ignore`](https://opencode.ai/docs/config) | ファイルウォッチャの除外 | ❌ ウォッチャは設定不可 |
| `layout` | ピン時点で deprecated | n/a |
| `enterprise` / `experimental` | 管理ポリシー・フィーチャーフラグ | ❌ |

## 13. セッション・保存

*本改訂(2026-08-12)で新設: サブシステム節にあったストレージ行に調査を
足して独立させた。*

| 機能 | 補足 | ganja |
|---|---|---|
| [ストレージ配置](https://opencode.ai/docs/config) | XDG data ディレクトリ: `auth.json`・`log/`・`project/<slug>/storage/` にセッション毎・メッセージ毎の JSON(git リポジトリ外は `global/`) | ⚠️ `auth.json` ✅ 同じ発想;セッションはプロジェクト毎の SQLite 1 つで、初回オープン時に旧ファイルツリーから変換 |
| 再開 | `--continue` / `--session <id>` | ✅ 同じ 2 フラグ・相互排他 |
| セッション fork(`--fork`) | 継続しつつ会話を分岐 | ❌ |
| [スナップショット](https://opencode.ai/docs/config) | ステップ毎の git ツリーオブジェクト・コミットは汚さない・`snapshot: false` で停止 | ✅ ganja のスナップショットストア+同じ config キー |
| [`/undo` / `/redo`](https://opencode.ai/docs/config) | 会話とファイルを復元・シェルの副作用は残る | ✅ 同じセマンティクスと注意書き |
| ログ保持 | `log/` にタイムスタンプ付き・新しい 10 件を保持・`--log-level` | ❌ 文書化されたログ面なし |
| 管理バイナリ(`bin/`) | 自己インストールした補助ツールがデータの隣に置かれる | ❌ ganja は何もインストールしない |

## 14. CLI サブコマンド

| コマンド | 補足 | ganja |
|---|---|---|
| [`export`](https://opencode.ai/docs/cli) | セッション → JSON、`--sanitize` | ❌ |
| [`import`](https://opencode.ai/docs/cli) | ファイル/共有 URL から取込 | ❌(`config import-opencode` は設定のみ) |
| [`stats`](https://opencode.ai/docs/cli) | `--days/--models/--tools` 分析 | ❌ |
| [`github install` / `github run`](https://opencode.ai/docs/github) | Actions workflow + `/oc` メンション | ❌ |
| [`pr <number>`](https://opencode.ai/docs/cli) | PR を checkout して起動 | ❌ |
| [`acp`](https://opencode.ai/docs/cli) | IDE 向け Agent Client Protocol サーバー | ❌ |
| [`agent create`](https://opencode.ai/docs/agents) | 対話的エージェント生成 | ❌ |
| [`upgrade` / `uninstall`](https://opencode.ai/docs/cli) | 自己更新・自己削除 | ❌ |
| [`attach <url>`](https://opencode.ai/docs/cli) | 実行中サーバーへ TUI をアタッチ | ⚠️ headless `run --attach` のみ |
| [`web`](https://opencode.ai/docs/cli) | Web UI | ❌ |
| [`account` / `db` / `plug` / `generate` / `debug/*`](https://github.com/anomalyco/opencode/tree/v1.18.13/packages/opencode/src/cli/cmd) | アカウント・DB・プラグイン・デバッグ | ❌ |
| [`run --fork`](https://opencode.ai/docs/cli) | 継続時のセッション分岐 | ❌ |
| [`run -f <file>`](https://opencode.ai/docs/cli) | CLI からのファイル・画像添付 | ❌(プロンプト内 `@` は✅) |
| [`run --command <name>`](https://github.com/anomalyco/opencode/blob/v1.18.13/packages/opencode/src/cli/cmd/run.ts) | slash コマンドを headless 実行 | ❌ |
| [`run --share` / `--title`](https://github.com/anomalyco/opencode/blob/v1.18.13/packages/opencode/src/cli/cmd/run.ts) | セッション共有・命名 | ❌ |
| [`run --variant <v>`](https://github.com/anomalyco/opencode/blob/v1.18.13/packages/opencode/src/cli/cmd/run.ts) | CLI からの reasoning-effort variant 指定 | ❌(TUI の `/effort` は✅) |
| [`serve --cors` / `--mdns`](https://opencode.ai/docs/server) | CORS 許可・mDNS 発見 | ❌(serve 自体は✅) |
| [`run` / `serve` / `auth` / `models` / `sessions` / `mcp`](https://opencode.ai/docs/cli) | 実働セット | ✅ nd-JSON 出力・`--continue`/`--session`・Basic 認証 serve 含む |

## 15. サーバー表面・SDK

| ルート/挙動 | 補足 | ganja |
|---|---|---|
| [question 応答ルート](https://opencode.ai/docs/server) | HTTP 経由の回答 | ❌ named follow-up 記録済み |
| [file/find 系ルート](https://opencode.ai/docs/server) | ファイル読・テキスト/シンボル検索 | ❌ |
| [`/api/provider`・`/api/integration`・`/api/credential`](https://opencode.ai/docs/server) | プロバイダ・統合・資格情報 API | ❌ |
| [`/api/mcp` 系](https://opencode.ai/docs/server) | サーバー側 MCP 管理+resources | ❌ |
| [`/tui` ブリッジ](https://opencode.ai/docs/server) | TUI 制御チャネル | ❌ |
| [OpenAPI 3.1 スペック(`/doc`)](https://opencode.ai/docs/server) | ライブ Swagger。`@opencode-ai/sdk` はここから生成される | ❌ |
| [`/api/generate`](https://opencode.ai/docs/server) | one-shot 生成 | ❌ |
| WebSocket / mDNS / マルチディレクトリ | | ❌ 単一起動ディレクトリはピン済み divergence |
| [share 系ルート](https://opencode.ai/docs/share) | 公開・撤回 | ❌ |
| [`@opencode-ai/sdk`](https://opencode.ai/docs/sdk) | サーバーの OpenAPI スペックから生成されるクライアント SDK | ❌(`ganja-client` は ganja-serve 相手の手書き) |
| legacy `/session` REST + `/event` SSE + `/permission` | | ✅ 非 loopback 無パスワード拒否の姿勢つき |

## 16. 環境変数

ピンは約 70 の `OPENCODE_*` 変数を持つ。挙動を左右する主要なもの:

| 変数 | 意味 | ganja |
|---|---|---|
| `OPENCODE_CONFIG` | 追加の設定ファイル | ✅ `GANJA_CONFIG` |
| `OPENCODE_CONFIG_DIR` | config home | ✅ `GANJA_CONFIG_HOME`(マージでなく単一ホーム) |
| `OPENCODE_CONFIG_CONTENT` | インライン JSON 設定 | ❌ |
| `OPENCODE_TUI_CONFIG` | 別の `tui.json` | ❌ 第二の設定ファイルなし |
| `OPENCODE_PERMISSION` | インライン権限ルールセット | ❌ |
| `OPENCODE_DISABLE_AUTOUPDATE` / `OPENCODE_ALWAYS_NOTIFY_UPDATE` | 更新機構のノブ | n/a — 自己更新機構なし |
| `OPENCODE_DISABLE_AUTOCOMPACT` | 自動コンパクションの停止 | ❌ |
| `OPENCODE_DISABLE_PROJECT_CONFIG` | プロジェクト設定の無視 | ❌ |
| `OPENCODE_DISABLE_CLAUDE_CODE(_PROMPT/_SKILLS)` | Claude Code のファイルを読まない | ⚠️ ganja は `~/.claude/CLAUDE.md` フォールバックを無条件に読む。スキルはそもそも外部由来を読まない |
| `OPENCODE_DISABLE_EXTERNAL_SKILLS` | `.claude`/`.agents` スキル走査の停止 | n/a — 最初から探索しない |
| `OPENCODE_SERVER_PASSWORD` / `_USERNAME` | serve の Basic 認証 | ✅ `GANJA_SERVER_PASSWORD` / `_USERNAME` |
| `OPENCODE_WEBSEARCH_PROVIDER` | Exa / Parallel の選択 | ✅ `GANJA_WEBSEARCH_PROVIDER` |
| `OPENCODE_ENABLE_EXA` / `_PARALLEL` | 検索バックエンドの有効化 | ⚠️ ganja は `EXA_API_KEY`/`PARALLEL_API_KEY` の有無で決める |
| `OPENCODE_AUTO_SHARE` / `OPENCODE_DISABLE_SHARE` | share の挙動 | ❌ share なし |
| `OPENCODE_LOG_LEVEL` / `OPENCODE_PRINT_LOGS` | ロギング | ❌ |
| `OPENCODE_AUTH_CONTENT` | インライン資格情報 | ❌ |
| `OPENCODE_DISABLE_LSP_DOWNLOAD` | LSP サーバーを入れない | n/a — ganja は決して入れない |
| `OPENCODE_DISABLE_PRUNE` | stale スピル/切り詰めファイルの保持 | ❌ |
| `OPENCODE_GIT_BASH_PATH` | windows のシェル | ❌ |
| `OPENCODE_EXPERIMENTAL_*`(約 15 フラグ) | background subagents・code mode・plan mode・websockets・workspaces・lsp tool 等 | ❌ 一括で |

ganja 独自の変数(`GANJA_MODEL`・`GANJA_FAKE_SCRIPT`・`GANJA_MODELS_URL`/`_PATH`・
`GANJA_DISABLE_MODELS_FETCH`・`GANJA_AUTH_ISSUER`・`GANJA_OPENCODE_DIR`・
`GANJA_LIVE_TEST`)には upstream の対応物がなく、リポジトリルートの
`AGENTS.md` に記載がある。

## 17. サブシステムと兄弟プロダクト

| サブシステム | 補足 | ganja |
|---|---|---|
| [プラグイン](https://opencode.ai/docs/plugins) | JS ランタイム。npm 指定 + ローカル `{plugin,plugins}/*.{ts,js}`。フック(`tool.execute.before/after`・`permission.ask`・`chat.message`・イベントバス)。`@opencode-ai/plugin` の型。ctx は SDK クライアントと Bun の `$` を運ぶ | ❌ JS プラグインランタイム自体は対象外;ganja 独自の `hooks` config キー(P13、§7)は**別物の Claude 型機構**— 9つの名前付きイベント・`sh -c` コマンドハンドラ・JS ランタイムなし・イベントバスなし — この行の移植ではない(D456) |
| [share](https://opencode.ai/docs/share) | `opencode.ai/s/<id>` 公開・`/share` `/unshare`・`manual`/`auto`/`disabled` | ❌ 対象外 |
| [フォーマッタ](https://github.com/anomalyco/opencode/blob/v1.18.13/packages/opencode/src/format/formatter.ts) | 26+ のビルトイン(gofmt・prettier・biome・ruff・rustfmt・shfmt・terraform・clang-format 等)が編集後に走る。フォーマッタ毎の無効化またはカスタム `command`+`extensions`+`environment` | ❌ |
| [バックグラウンドエージェント](https://github.com/anomalyco/opencode/tree/v1.18.13/packages/opencode/src/background) | 非同期派遣・要約・通知 | ❌ |
| [worktree](https://github.com/anomalyco/opencode/tree/v1.18.13/packages/opencode/src/worktree) | エージェント毎の git worktree 分離 | ❌ |
| [画像パイプライン](https://github.com/anomalyco/opencode/tree/v1.18.13/packages/opencode/src/image) | 画像添付 | ⚠️ `@` 添付✅・クリップボード取込❌ |
| account / sync / control-plane | クラウドアカウント機構 | ❌ 対象外 |
| installation(自己更新) | | ❌ |
| [IDE / ACP](https://opencode.ai/docs/ide) | エディタ拡張・サイドバーチャット | ❌ 対象外 |
| codemode | `execute` 実行基盤 | ❌ |
| desktop / web / console / slack / enterprise / identity / containers / session-ui | 兄弟プロダクト — 1.18.15/16 のリリース作業の大半は Desktop に着地 | ❌ 対象外 |

## 18. ピン以降に動いたもの — v1.18.14 → v1.18.16

リリースノートに基づく差分の全量。ピンは不変で、以下のいずれも憲章の
作業ではない。

| 変更 | リリース | ganja |
|---|---|---|
| xAI ログインを単一の device-code フローへ統合 | [v1.18.14](https://github.com/anomalyco/opencode/releases/tag/v1.18.14) | ✅ 元から ganja の形 — 両者が収斂 |
| 構造化された mid-stream プロバイダエラーを保持し、対応プロバイダがリトライ | v1.18.14 | ❌ 時間が生んだ divergence: ganja は最初のバイト前のみリトライ(ピンの規則) |
| 一時的なプロバイダ/ネットワークエラーのリトライ範囲拡大 | v1.18.14 | ⚠️ 同じく first-byte 前のみの姿勢 |
| ACP 使用量にキャッシュ書込を計上・キュー済み ACP 更新をターン終了前に待機 | v1.18.14 | n/a — ACP なし |
| リモート workspace 修正(host `directory` 不転送・5xx 本文のログ) | v1.18.14 | n/a — 対象外 |
| import 済み/レガシー id でも時系列順を維持。revert/fork を実時系列で | [v1.18.15](https://github.com/anomalyco/opencode/releases/tag/v1.18.15) | ✅ 構造的に無縁 — ganja の id は生成順に整列する |
| 切り詰めの掃除がファイルのタイムスタンプで stale を除去 | v1.18.15 | ⚠️ ganja のスピル規律は独自 |
| 反復コンパクションが過去のツール呼び出し履歴を要約に保持 | v1.18.15 | ❌ ganja のコンパクションはピンの挙動 |
| tmux `set-clipboard on` 下の ssh コピー | v1.18.15 | ✅ 既にカバー — OSC 52 をシステムクリップボードより先に無条件でキュー |
| カーソルスタイル設定(`tui.json`) | v1.18.15 | ❌ |
| 未知のトップレベル設定キーを失敗でなく無視 | [v1.18.16](https://github.com/anomalyco/opencode/releases/tag/v1.18.16) | ❌ **意図的に** — ganja はピンの名指し拒否を維持 |
| Home からのプロジェクト登録。Desktop のロケール/メニュー/macOS ライフサイクル | v1.18.15–16 | n/a — 兄弟プロダクト |

本書の旧版はピン外の推測テーブル(v2 beta・FFF・queue-vs-steer)を
持っていた。それらは 1.18 系より先の話か既にピン内の機能であり、退役
させた。Desktop は §17 に載っている。
