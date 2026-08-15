# Claude Code 機能リファレンス(ganja との比較)

> [!IMPORTANT]
> **本書は参照用インベントリであり、ロードマップではない。ここに記載した全ての
> 機能をポートするわけではない。** ganja の憲章は opencode v1.18.13 との
> 挙動パリティであり、Claude Code は別プロダクトとして比較のために目録化した
> だけである。表中の ❌ は観察であって、約束ではない。

ganja 側セルは 2026-08-15 に post-P22 のツリーへ更新済み(Claude 側調査は
2026-08-12 のまま)。スナップショット: 2026-08-12、Claude Code 2.1.x 世代を対象。Claude Code の
更新は速いので、古くなった行は「古い行」であって ganja の退行ではない。
*(低確度)* を付した行は公式ドキュメントではなくコミュニティ情報に依る。

セクション構成は 3 つのリファレンス(claude・codex・opencode)共通の
アウトラインに従う。同じトピックはどの文書でも同じセクション番号にある。

凡例: ✅ ganja に存在(パリティまたは近い等価物) · ⚠️ 部分的 · ❌ 不在。

## 1. TUI — Composer・入力

| 機能 | キー | ganja |
|---|---|---|
| [ファイルパスの Tab 補完](https://code.claude.com/docs/en/interactive-mode) | `@path` + Tab | ✅ `@`・`/` 両メニューで Tab 確定(`@` は Enter と同じ挿入、`/` は実行せず補完のみ);ディレクトリ降下(`@dir`→`@dir/`)は未実装 — ウォーカーがファイルのみを返すため |
| [スラッシュコマンド補完](https://code.claude.com/docs/en/slash-commands) | `/` | ✅ ドロップダウン+パレット |
| [ファイル mention](https://code.claude.com/docs/en/common-workflows) | `@` | ✅ `#行レンジ`・画像/PDF 添付を含む |
| [Vim モード](https://code.claude.com/docs/en/interactive-mode) | `/vim` | ❌ |
| [プロンプト履歴](https://code.claude.com/docs/en/interactive-mode) | ↑ / ↓ | ✅ 50件・重複抑止・自己修復ストア |
| [履歴の逆方向検索](https://code.claude.com/docs/en/interactive-mode) | Ctrl+R | ✅ ファジー絞込・新しい順・プレビュー付きの検索モーダル(upstream の Ctrl+R は無関係な `session_rename` で ganja では未割当) |
| [クリップボード画像ペースト](https://code.claude.com/docs/en/interactive-mode) | Ctrl+V | ✅ PNG をプロセス内エンコード(OS ツール呼出しなし)し、既存の `@` mention パイプライン経由で添付 |
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
| [メッセージキュー](https://code.claude.com/docs/en/interactive-mode) | 実行中に入力 | ✅ 実行中のターンへ次のステップ境界で steer(`Command::Steer`);steer できないもの(拒否・未消費・スラッシュコマンド)は再生 FIFO にフォールバック |
| [エージェント mention](https://code.claude.com/docs/en/sub-agents) | `@agent-…` | ❌ `@` はファイルのみ |
| [ドロップしたパスの mention 化](https://code.claude.com/docs/en/interactive-mode) | drag & drop | ✅ 既存パス・`file://` のドロップ/ペーストは `@` mention 化;一部でも解決できなければペーストはそのまま |
| [画面再描画](https://code.claude.com/docs/en/interactive-mode) | Ctrl+L | ✅ 次フレームを強制フル redraw(Claude Code 由来のバインド、upstream に対応なし) |
| [段階的な中断](https://code.claude.com/docs/en/interactive-mode) | Ctrl+C 1回/2回 | ⚠️ 一段のみ |

## 2. TUI — 大型サーフェス・keybind

*本改訂(2026-08-12)で新設。断りのない行は公式ドキュメントに依る。*

| 機能 | 補足 | ganja |
|---|---|---|
| トランスクリプト文法 *(スクリーンショット由来。文書化されていない)* | 返答ブロックとツール呼出しに `●`、結果マーカーに `⎿`(プレビューはその下に字下げ)、ユーザー自身のメッセージに `>`、thinking に `✻` | ✅ そのまま移植(D487): 同じ4字形、引数要約はヘッダ1行に凝縮(`● Tool(key: "value", …)`、上限付きで切り詰めを明示)、状態は角括弧の語ではなく色で示す — 2026-08-15 以降、確定した ● は成否だけを答える(成功=緑・失敗=赤、失敗行の見出しは通常色のまま)— クランプしたプレビューは ganja 自身の展開手段を指す(`… +N lines (ctrl+t to expand)` — Claude は ctrl+o を指す)。`read` は件数のみでプレビューを一切出さない(`● Read(/abs/path · lines A-B)` + `⎿ Read N lines`);`/copy` は意図的に upstream opencode の markdown 形状を維持する — 画面とクリップボードは読み手が違う |
| 作業中の行 *(スクリーンショット由来)* | ターン実行中、composer 直上に `✻ <verb>… (Ns · ↓ N tokens)` を固定し、その下に todo リスト | ⚠️ 2026-08-15 以降は配置も同じ — スクロール外の composer 直上ストリップに、専用オレンジ+左→右のシマー帯で描いた行と、実行中ターンの最新チェックリストをぶら下げる — 動詞は ganja 独自(Claude の語彙はあちらの声である);トークン値は Claude 自身の ↓ 矢印に乗るがセッション累計の出力トークンで、usage はリクエスト毎にしか届かないため、値が無いときはゼロを示さずセグメントごと落とす |
| thinking のストリーム表示 | 到着した thinking ブロックをトランスクリプトに描画 | ✅ `✻` マーカー・dim italic・ストリーム中は新しい側からクランプ;**表示専用** — どの wire も送り返さず、要約にも載らず、context メーターは 0 と数え、クリップボードにも出ない。upstream opencode は可読部と封緘部を1つの part に融合してリクエストに載せ返すが、ganja は分割し封緘側だけを送る |
| [verbose トランスクリプトビューア](https://code.claude.com/docs/en/interactive-mode) | Ctrl+O オーバーレイ: 全履歴・ツールペイロード・thinking ブロック | ✅ Ctrl+T インスペクタ — フルターミナル占有・3タブ(展開トランスクリプト・生イベントログ・ターン毎トークン表);表現は Codex CLI 自身のオーバーレイと Claude Code の Ctrl+O フッター文言を合成し、どのテーマでも Codex モノクロで描画、各タブは末尾固定で開きストリームに追従する(2026-08-15) |
| [Todo チェックリストパネル](https://code.claude.com/docs/en/interactive-mode) | Ctrl+T でタスクのサイドパネルを開閉 | ⚠️ 開閉パネルは無い(ganja の Ctrl+T はインスペクタ)が、実行中ターンの最新チェックリストは作業行の下・composer 直上に固定表示 — Claude 自身の配置 — され、各 `todowrite` 呼出しはチャット内で ☐/☒ 行として描かれる(2026-08-15) |
| [permission ダイアログ](https://code.claude.com/docs/en/iam) | ツール呼出しのプレビュー・承認/拒否・ダイアログ内モード切替 | ✅ upstream 由来のダイアログセマンティクス(`a`/`A`/`d`)、複数の子が同時に尋ねるとキュー化;ダイアログ内モード切替はなし(モード概念自体がない) |
| [trust ダイアログ](https://code.claude.com/docs/en/iam) | 初回起動時のディレクトリ信頼確認 | ❌ trust 層なし;すべて permission ルールが門番 |
| [ステータスラインのスクリプト化](https://code.claude.com/docs/en/statusline) | `/statusline`、セッション JSON を stdin で受ける `statusLine` コマンド | ⚠️ 代わりにネイティブな `tui.statusline` 要素ロースター(D469): ユーザー順の名前付きセグメント、HUD 形のメーター、任意の git 行・詳細行 — 外部スクリプトプロトコルは意図的に無し(描画ティックごとのサブプロセスを作らない) |
| [ターミナル設定](https://code.claude.com/docs/en/terminal-config) | `/terminal-setup`: キーバインド・ターミナルプロファイル調整 | ❌ 調整対象なし;bracketed paste と OSC 52 は無条件 |
| [スピナー tips](https://code.claude.com/docs/en/settings) | `spinnerTipsEnabled` | ❌ |
| [カスタム keybindings](https://code.claude.com/docs/en/interactive-mode) *(ファイルスキーマは低確度)* | `~/.claude/keybindings.json`: コンテキスト対応バインドとコード列 | ⚠️ ganja は `keybinds` config マップ — アクション毎にカンマ区切りの代替、空値で解除;コンテキストもコード列もなし |

## 3. モード・セッション操作

| 機能 | キー | ganja |
|---|---|---|
| [permission mode 切替](https://code.claude.com/docs/en/iam) | Shift+Tab | ❌ モード概念なし。plan agent が plan mode の近似 |
| [Extended Thinking 切替](https://code.claude.com/docs/en/interactive-mode) | Tab / Cmd+T | ❌(代わりに `/effort` がレベルを選ぶ) |
| [リワインド / チェックポイント](https://code.claude.com/docs/en/checkpointing) | Esc Esc・`/rewind` | ✅ `/rewind` + アイドル時の Esc Esc で二段階チェックポイントピッカー(Both/Conversation/Files スコープ、`Command::RevertTo`);upstream の part 単位アンカー(`partID`)は未移植 — チェックポイントはユーザーメッセージ単位 |
| [実行中タスクのバックグラウンド化](https://code.claude.com/docs/en/interactive-mode) | Ctrl+B | ⚠️ バックグラウンド実行自体は存在(`bash` の `run_in_background`、`bash_output`/`kill_shell`);実行中のフォアグラウンド呼出しを後からバックグラウンド化するジェスチャーはなし |
| [エージェント切替](https://code.claude.com/docs/en/sub-agents) | — | ✅ Tab で順繰り(ganja 独自既定)・逆順は ❌ |

## 4. スラッシュコマンド

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
| [`/mcp`](https://code.claude.com/docs/en/mcp) | MCP 管理・認証ダイアログ | ✅ 二段階ダイアログ: サーバー一覧(状態・ツール数・エラー)→ Reconnect/Login アクション、`ganja mcp` CLI 一覧も併存 |
| [`/memory`](https://code.claude.com/docs/en/memory) | メモリーファイル編集 | ❌ |
| [`/hooks`](https://code.claude.com/docs/en/hooks) | フック管理 | ⚠️ フック機構自体は✅(config 宣言・9イベント);閲覧・編集用の対話 UI はなし |
| [`/statusline`](https://code.claude.com/docs/en/statusline) | ステータスバーのスクリプト化 | ❌ スクリプト化コマンドは無し;バー自体は `tui.statusline` でネイティブに構成できる(D469) |
| [`/output-style`](https://code.claude.com/docs/en/output-styles) | 応答スタイル | ❌ |
| [`/context`](https://code.claude.com/docs/en/costs) | 文脈使用量の可視化グリッド | ✅ 圧縮見積もり器と同じ内訳でカテゴリ別グリッド+凡例を描画(D470);ウィンドウ未収載モデルは正直に合計表示へ縮退し、分母を発明しない |
| [`/todos`](https://code.claude.com/docs/en/interactive-mode) | タスクチェックリスト表示 | ⚠️ コマンドは無いが、リストはチャット内に ☐/☒ 行で描かれ、ターン実行中は composer 直上にも固定表示される(2026-08-15) |
| [`/usage`](https://code.claude.com/docs/en/costs) | 使用量・コスト内訳 | ✅ セッション合計・キャッシュ/推論の内訳・文脈 %・ターン別テーブル(D471)+「Current window」セクション(D484;パネルは「expired」を言葉で言い、HUD の `rate` メーターは最後に聞いた値を保持する — リクエスト系バケットの reset はミリ秒単位で来るため、時計に従うメーターは点滅か 0% 固定にしかならない)に加え、バックエンドが実際に送るプラン上限メーターを描画(D485): ChatGPT シートの 5h/weekly used-percent 窓と Copilot のクォータスナップショットを毎応答のヘッダから読む(文法は各ベンダー公式クライアントから引用);何も送らないクレデンシャル(Platform キー、Anthropic の Admin 専用 usage API)だけを honest-absence の末尾が名指しする |
| [`/doctor`](https://code.claude.com/docs/en/troubleshooting) | 自己診断 | ❌ |
| [`/export`](https://code.claude.com/docs/en/slash-commands) | 会話のエクスポート | ⚠️ `/copy` のみ |
| [`/cd`](https://code.claude.com/docs/en/slash-commands) *(低確度)* | 作業ディレクトリ変更 | ❌ 起動ディレクトリ固定は設計判断 |
| [`/add-dir`](https://code.claude.com/docs/en/common-workflows) | セッション中の追加ディレクトリ許可 | ❌ |
| [`/plugin`](https://code.claude.com/docs/en/plugins) | marketplace 追加・install・reload | ✅ インストールストア上の二段階ダイアログ(D474): プラグイン行ごとに Enable/Disable/Remove、その下に Add marketplace / Install / Reload;Reload は正直な分割 — hooks とスキルルートはセッション内で再構築、agents/MCP/LSP は restart required と明言 |
| [`/vim`](https://code.claude.com/docs/en/interactive-mode) | vim 編集 | ❌ |

## 5. 内蔵ツール

| ツール | 補足 | ganja |
|---|---|---|
| [`Read`](https://code.claude.com/docs/en/settings) | 行番号付きテキスト+画像・PDF(約20頁)・notebook | ⚠️ テキスト✅・画像/PDF は `@` 添付経由でモデルに届く(read ツールでは読めない) |
| [`Edit`](https://code.claude.com/docs/en/settings) | 厳密文字列置換・read-before-edit 強制 | ✅ 同じ規律(`FileTimes`) |
| [`Write`](https://code.claude.com/docs/en/settings) | 生成・上書き | ✅ +symlink 差替えに対する anchored I/O |
| [`NotebookEdit`](https://code.claude.com/docs/en/settings) | Jupyter セル操作 | ❌ |
| [`Glob`](https://code.claude.com/docs/en/settings) | パターンファイル検索 | ✅ in-process(ripgrep crates) |
| [`Grep`](https://code.claude.com/docs/en/settings) | 正規表現検索 | ✅ in-process |
| [`Bash`](https://code.claude.com/docs/en/settings) | チェーン対応の権限チェック付きシェル | ✅ "always" 用の arity 表を含む |
| [`BashOutput` / `KillShell`](https://code.claude.com/docs/en/settings) | バックグラウンドシェルの読取・停止 | ✅ `bash_output`(差分ポーリング+正規表現 `filter`)・`kill_shell` — upstream opencode に対応物なし(D454) |
| [`WebFetch`](https://code.claude.com/docs/en/settings) | URL 取得・解析 | ✅ `webfetch` |
| [`WebSearch`](https://code.claude.com/docs/en/settings) | web 検索 | ✅ `websearch`(Exa/Parallel) |
| [`Task`](https://code.claude.com/docs/en/sub-agents) | サブエージェント起動 | ✅ `task` — 実行中の行は子の直近呼出しを下にぶら下げ(watcher が書く上限付きログ;全量は Ctrl+T か `/copy` に)、後ろで順番待ちの呼出しは確定済み引数を名乗る(2026-08-15) |
| [`TodoWrite`](https://code.claude.com/docs/en/interactive-mode) | チェックリスト | ✅ `todowrite` |
| [`ExitPlanMode`](https://code.claude.com/docs/en/common-workflows) | 承認付き plan 離脱 | ✅ `plan_exit`(question ゲートの build 切替) |
| skill ツール | スキルの明示ロード | ✅ `skill` |
| question ツール | 構造化された質問 | ✅ `question`(自由入力含む) |

## 6. 権限

| 機能 | 補足 | ganja |
|---|---|---|
| [Bash コマンドパターン](https://code.claude.com/docs/en/iam) | `Bash(npm run *)`・前置/後置/複数ワイルドカード | ⚠️ パターンルールは存在(upstream 形)・ワイルドカード文法は別物 |
| [チェーン分解](https://code.claude.com/docs/en/iam) | `&&`/`;`/`\|` を分割し全段で判定 | ⚠️ arity 表によるコマンド種別解析・分解方式ではない |
| [gitignore 形式のパスルール](https://code.claude.com/docs/en/iam) | `Edit(src/**)`・`Read(.env)`・`//` 絶対パス | ❌ ツール別パス allow/deny なし |
| [MCP ツールパターン](https://code.claude.com/docs/en/iam) | `mcp__server__tool`・サーバー一括許可 | ✅ 同じ命名・MCP は既定で ask |
| [ドメイン限定 web ルール](https://code.claude.com/docs/en/iam) | `WebFetch(domain:github.com)` | ❌ |
| [deny → ask → allow(最厳優先)](https://code.claude.com/docs/en/iam) | | ⚠️ ganja は層状 tier の後勝ち — 別のピン済みセマンティクス |
| [設定スコープ](https://code.claude.com/docs/en/settings) | user / project / project-local / CLI フラグ / managed | ⚠️ builtin < agent < config < 保存回答。local 重ね・フラグ・managed なし |
| [保存される "always" 回答](https://code.claude.com/docs/en/iam) | 承認の永続化 | ✅ プロジェクト毎ストア・シェルは arity 対応 |
| [sandbox 実行](https://code.claude.com/docs/en/sandboxing) | OS/コンテナ隔離 | ❌ 権限ゲートのみ |

## 7. hooks・自動化

| 機能 | 補足 | ganja |
|---|---|---|
| [フックイベント](https://code.claude.com/docs/en/hooks) | PreToolUse・PostToolUse・UserPromptSubmit・Notification・Stop・SubagentStop・SessionStart・SessionEnd・PreCompact(+権限判定フック) | ✅ 9イベント全て、config 宣言(`hooks` キー、Claude 自身の `{matcher, hooks:[...]}` 形をそのまま採用)— upstream opencode に対応物なし(D456) |
| [フックプロトコル](https://code.claude.com/docs/en/hooks) | stdin に JSON・exit 2 でツール呼出をブロック・stdout で文脈注入 | ⚠️ 同じ envelope・exit code セマンティクス;ブロックは v1 では PreToolUse/UserPromptSubmit のみ(ブロックが意味を持つ2イベント)に限定;`transcript_path` なし(SQLite 保存のため、D457);`updatedInput` 書換えと Stop フックの強制継続は未実装 |
| [matcher](https://code.claude.com/docs/en/hooks) | ツール別正規表現(`Edit\|Write`) | ✅ 正規表現 matcher に加え、PreCompact/SessionStart 用の列挙語彙 |

## 8. ルール・カスタムコマンド・メモリー

| 機能 | 補足 | ganja |
|---|---|---|
| [コマンドファイル](https://code.claude.com/docs/en/slash-commands) | `.claude/commands/*.md` + グローバル | ✅ config 宣言コマンド + ファイル tier(D481): `<config home>/commands/*.md` と `<project root>/.ganja/commands/*.md`(frontmatter description/agent/model/argument-hint、本文がテンプレート、builtin < global < project < config の後勝ち) |
| [`$ARGUMENTS` / `$1`・`$2`](https://code.claude.com/docs/en/slash-commands) | 引数展開 | ✅ |
| [テンプレート内 `` !`cmd` ``](https://code.claude.com/docs/en/slash-commands) | 起動時のシェル出力埋込 | ✅(P8) |
| [テンプレート内 `@path`](https://code.claude.com/docs/en/slash-commands) | ファイル埋込 | ✅(P8・mention 級添付として) |
| [frontmatter: `allowed-tools`](https://code.claude.com/docs/en/slash-commands) | コマンド毎のツール制限 | ❌(コマンド毎 agent は✅) |
| [frontmatter: `model`・`argument-hint`](https://code.claude.com/docs/en/slash-commands) | コマンド毎モデル+ヒント | ✅ ファイル tier の frontmatter が両方を持つ(D481) |
| [CLAUDE.md 階層](https://code.claude.com/docs/en/memory) | グローバル→ルート→サブディレクトリを連結 | ✅ グローバル+プロジェクトの AGENTS.md 族に加え、ツールが実際に触れたサブツリーの AGENTS.md 族を lazy に次リクエストへ注入(touch 駆動・closest-last・clamp 付き、D480)— Claude の起動時連結とは方式が異なる |
| [メモリー内 `@path` import](https://code.claude.com/docs/en/memory) | インポート元相対で解決するモジュール分割 | ❌ |
| [自動メモリー](https://code.claude.com/docs/en/memory) | `~/.claude/projects/<hash>/memory/`(MEMORY.md 索引+トピックファイル)を自己保守 | ✅ opt-in(config `memory: true`、既定 off は意図的相違): プロジェクト毎データディレクトリの `memory/`(MEMORY.md 索引+トピックファイル)を prompt に合成、維持指示ブロックは合成文で秘密の保存を明示的に禁止(D478) |

## 9. エージェント・スキル

| 機能 | 補足 | ganja |
|---|---|---|
| [エージェント定義ファイル](https://code.claude.com/docs/en/sub-agents) | `.claude/agents/*.md`(name/description/model/tools) | ✅ config 宣言 agent に加えファイル tier(D482): `<config home>/agents/*.md` + `.ganja/agents/*.md`(name/description/model/tools、本文がプロンプト);`tools:` は permission ルールへ写像され未掲載ツールは隠されず拒否される — Claude の roster 隠しとの意図的相違。model はフル `provider/model` のみ(エイリアスは名指し拒否) | |
| [記述による自動委譲](https://code.claude.com/docs/en/sub-agents) | モデルがエージェントを選ぶ | ⚠️ task ツールが記述付き roster を提示 |
| [並列サブエージェント](https://code.claude.com/docs/en/sub-agents) | 同時実行 | ✅ 1アシスタントステップ内で連続する `task` 呼出しが並行 fan-out(`agents.concurrency` 上限、既定4)し完了順に fan-in;root ターンは引き続き直列(D462) |
| [`isolation: worktree`](https://code.claude.com/docs/en/sub-agents) | worktree 内で実行 | ❌ |
| [エージェントへの skill 事前ロード](https://code.claude.com/docs/en/sub-agents) | `skills:` | ❌ |
| [SKILL.md ロード](https://code.claude.com/docs/en/skills) | | ✅ ganja の2ホーム+`skills.paths` |
| [自動トリガー+`paths` スコープ](https://code.claude.com/docs/en/skills) | 記述・パスマッチ発動 | ❌ 明示のみ — モデルの `skill` ツールか composer の `$name` トークン(D491、Claude の `/name` に対し Codex CLI の文法) |
| [`context: fork`](https://code.claude.com/docs/en/skills) | fork したサブエージェントで実行し結果のみ返す | ❌ |
| [skill の `allowed-tools`](https://code.claude.com/docs/en/skills) | `mcp__*` ワイルドカード含む制限 | ❌ |
| [プラグイン: 5 コンポーネント](https://code.claude.com/docs/en/plugins) | skills・agents・hooks・MCP・LSP | ✅ 6 面すべてが config への寄与としてマージされる(D472/D473;P22 で `commands/` が集合を閉じた): hooks は追記、MCP は `plugin:<name>:<server>` に名前空間化され既定で確認要求、skills ルートは連結、`commands/*.md` は `<plugin>:<name>` としてコマンド表に加わり、agents/LSP はキー単位で明示 config が勝つ |
| [marketplace](https://code.claude.com/docs/en/plugins) | `marketplace.json`・`/plugin install`・`/reload-plugins` | ⚠️ `marketplace.json` をそのまま解釈、git URL かローカルパスから追加、インストールは `<plugin>@<marketplace>` 表記(D472);リモート source オブジェクト(`github:` 等)はパースのみでまだインストール不可、reload は restart-honest(D474) |

## 10. MCP・LSP

| 機能 | 補足 | ganja |
|---|---|---|
| [transport](https://code.claude.com/docs/en/mcp) | stdio・streamable HTTP・SSE | ✅ stdio+streamable HTTP・legacy SSE ❌ |
| [設定スコープ](https://code.claude.com/docs/en/mcp) | local(`~/.claude.json`)/ project(`.mcp.json`)/ user+優先順位 | ⚠️ グローバル+プロジェクト config・repo 毎 local スコープなし |
| [CLI 管理](https://code.claude.com/docs/en/mcp) | `claude mcp add/list --scope --transport` | ✅ `ganja mcp add/list/get/remove`(D483): ローダー自身の述語で検証してから `ganja.json` へ staged 書込み(未知キーはバイト意味不変に保存)、`ganja.jsonc` は CST 保存編集(コメント・整形・並びはバイト不変、jsonc-parser の cst)、`get` は由来ファイルと override を正直に報告 | |
| [OAuth](https://code.claude.com/docs/en/mcp) | PKCE・メタデータ発見・トークン更新 | ✅ RFC 8414 発見+RFC 7591 登録(フォールバック client id)+PKCE/loopback+401 時の refresh-then-redial;意図的に最小構成 — resource-metadata discovery なし・呼出し中の reactive re-auth なし(D466) |
| [project スコープの初回承認](https://code.claude.com/docs/en/mcp) | repo 注入サーバー対策 | ✅ より強い: 全 MCP ツールが既定で ask |
| [タイムアウト・出力上限](https://code.claude.com/docs/en/settings) | `MCP_TIMEOUT`・`MCP_TOOL_TIMEOUT`・`MAX_MCP_OUTPUT_TOKENS` | ⚠️ サーバー毎 `timeout`/`output_limit` config キー(バイト単位・トークンではない);グローバルな env var ノブはなし |
| 再接続 | 死んだサーバーの復帰 | ✅ `/mcp` ダイアログの手動 Reconnect(`Failed` な任意サーバー)+初回 dial が失敗したサーバー限定のセッション1回だけの自動リトライ(D463) |
| ToolSearch/遅延ツール | しきい値超過で MCP ツールの schema がコンテキストから外れ、名前はリマインダーに載り、ToolSearch ツールがオンデマンドで schema を読込む;未ロードの deferred ツールは呼出し不可 | ✅ D492: `tool_defer_threshold`(未指定=32・0=全サーバー defer・巨大=無効)を超えるとサーバー丸ごと大きい順に defer、毎 step の listing が deferred を列挙し、常駐 `tool_search`(`select:` 完全一致のバッチ優先・それ以外は nucleo キーワード)がセッション単位で activate — schema は次 step のリクエストに載る。意図的乖離が一つ: deferred ツールへの直接呼出しは**実行され**、実行が activate する(成功・失敗どちらでも);`+term` require モードは未実装 |
| [plugin 経由の LSP サーバー](https://code.claude.com/docs/en/plugins) | プラグインが LSP サーバーを同梱できる | ✅ プラグインの `.lsp.json` がファーストパーティの `lsp` テーブルへキー単位でマージされ、明示 config が勝ち、明示の `lsp: false` は決して覆されない(D473) |

## 11. モデル・プロバイダ・認証

| 機能 | 補足 | ganja |
|---|---|---|
| [モデルエイリアス](https://code.claude.com/docs/en/model-config) | `sonnet` / `opus` / `haiku` | ⚠️ カタログの完全 id のみ |
| [`opusplan`](https://code.claude.com/docs/en/model-config) | plan は Opus・実行は Sonnet の自動二相 | ❌ |
| [1M コンテキストエイリアス](https://code.claude.com/docs/en/model-config) | `sonnet[1m]`・`opus[1m]` | ❌ |
| [`MAX_THINKING_TOKENS`](https://code.claude.com/docs/en/settings) | thinking 予算上書き | ⚠️ カタログ由来の effort variant が予算を運ぶ |
| [自動圧縮しきい値の上書き](https://code.claude.com/docs/en/settings) *(低確度)* | 発火率の env 調整 | ❌ 固定しきい値 |
| [小型高速モデルへのルーティング](https://code.claude.com/docs/en/settings) | 背景処理を安価モデルへ | ⚠️ `small_model` がタイトル要求を担う(接頭辞の provider に束縛、D490);他の背景処理はセッションモデルのまま |
| [サブスクリプション OAuth / Console API キー](https://code.claude.com/docs/en/iam) | `/login` が claude.ai OAuth(PKCE)か従量 API キーを選ばせる | ⚠️ ganja の `anthropic` は API キーのみ(env または保存);サブスクリプション OAuth は upstream 仕様に存在しなかった(規約対応で撤去済み) |
| [`apiKeyHelper`](https://code.claude.com/docs/en/settings) | 要求時にキーを出力する settings 宣言コマンド | ❌ 最も近いのは config プロバイダの `key_env` |
| [`ANTHROPIC_AUTH_TOKEN`](https://code.claude.com/docs/en/settings) | ゲートウェイ・プロキシ用カスタム bearer | ❌ |
| [OS キーチェーンへの資格情報保存](https://code.claude.com/docs/en/iam) *(OS 毎の詳細は低確度)* | macOS Keychain / Credential Manager / libsecret | ⚠️ ganja は所有者限定パーミッションの `auth.json`;OS キーチェーン統合なし |
| 資格情報の優先順位 | env トークン > 保存ログイン、文書化された順で解決 | ✅ 同型: `ANTHROPIC_API_KEY`/`OPENAI_API_KEY` が保存資格情報に優先 |

## 12. 設定面

*本改訂(2026-08-12)で新設。*

| 機能 | 補足 | ganja |
|---|---|---|
| [settings のスコープと優先順位](https://code.claude.com/docs/en/settings) | managed > CLI フラグ > `.claude/settings.local.json` > `.claude/settings.json` > `~/.claude/settings.json` | ⚠️ JSONC 3層(グローバルホーム < `GANJA_CONFIG` < プロジェクトファイル)で後勝ち;local 重ね・フラグ・managed 層なし |
| [`$schema` 参照](https://code.claude.com/docs/en/settings) | エディタ補完用 JSON Schema | ✅ ganja は `schema/ganja-config.schema.json` を同梱し、ローダーとのドリフトをテストで検出 |
| [`permissions` ブロック](https://code.claude.com/docs/en/iam) | `allow`/`ask`/`deny` 配列+`defaultMode` | ⚠️ ganja の `permission` ブロックは upstream opencode の文法(§6) |
| [`env` ブロック](https://code.claude.com/docs/en/settings) | スコープ毎の環境変数注入 | ❌ |
| [`hooks` キー](https://code.claude.com/docs/en/hooks) | イベント名キーの matcher/handler 群 | ✅ Claude の形をそのまま採用(§7) |
| [`model` / `effortLevel` キー](https://code.claude.com/docs/en/model-config) | 既定モデルと推論深度 | ✅ `model` も `effort` も: `effort` は新規セッションのカタログ effort を播種し、採用時にモデルの行へ照合される — 既定であって上書きではないので、保存済みセッション自身の選択が勝つ(P17) |
| [`statusLine`・`outputStyle`・`spinnerTipsEnabled`](https://code.claude.com/docs/en/statusline) | 表示スクリプトとスタイル | ❌(ganja の表示ノブはテーマのみ) |
| [`attribution`](https://code.claude.com/docs/en/settings) *(低確度)* | コミット/PR トレーラー文言 | ❌ ganja は自分でコミットを書かない |
| [`claude config` CLI](https://code.claude.com/docs/en/settings) | `get`/`set`/`list`・`--global` | ❌ 設定ファイルのみ |
| [`--setting-sources`](https://code.claude.com/docs/en/settings) | 読み込む層の選択 | ❌ |
| ハウスキーピングキー | `cleanupPeriodDays`・`language`・`autoUpdatesChannel`・`companyAnnouncements` | ❌ まとめて不在 |

## 13. セッション・保存

| 機能 | 補足 | ganja |
|---|---|---|
| [`/add-dir` / `additionalDirectories`](https://code.claude.com/docs/en/common-workflows) | マルチディレクトリアクセス | ❌ 単一起動ディレクトリは設計判断 |
| [`--worktree`](https://code.claude.com/docs/en/common-workflows) | linked worktree でセッション実行 | ❌ |
| [セッショントランスクリプト](https://code.claude.com/docs/en/data-usage) | セッション毎 JSONL・resume 可能 | ✅ プロジェクト毎 SQLite・resume 可能 |
| [checkpoint ファイル履歴](https://code.claude.com/docs/en/checkpointing) | 編集前の内容ハッシュバックアップ | ⚠️ worktree スナップショット(`/undo`) |
| [shell スナップショット](https://code.claude.com/docs/en/settings) *(低確度)* | シェル環境の再現用キャプチャ | ❌ |

## 14. CLI・headless

| 機能 | 補足 | ganja |
|---|---|---|
| [print モード](https://code.claude.com/docs/en/cli-reference) | `claude -p` | ✅ `ganja run` |
| [ストリーミング JSON 出力](https://code.claude.com/docs/en/cli-reference) | `--output-format stream-json` | ✅ `--format json`(nd-JSON) |
| [セッション継続](https://code.claude.com/docs/en/cli-reference) | `--continue` / `--resume` | ✅ |
| [セッション分岐](https://code.claude.com/docs/en/cli-reference) | `--fork-session` | ❌ |
| [permission モード群](https://code.claude.com/docs/en/cli-reference) | dontAsk / acceptEdits / plan / bypass | ⚠️ 一段のみだが対話 TUI・headless 双方に `--auto`(隠し別名 `--yolo`/`--dangerously-skip-permissions`): Ask で上がるダイアログを「1回許可」で自動応答、deny 不変・question は聞き続ける(D479) |
| [呼出単位のツール許可](https://code.claude.com/docs/en/iam) | `--allowedTools` パターン | ❌ config rules のみ |
| [system prompt フラグ](https://code.claude.com/docs/en/cli-reference) | append/replace × inline/file | ❌ |
| [hermetic 実行](https://code.claude.com/docs/en/cli-reference) *(低確度)* | `--bare` | ❌ |
| [スキーマ制約出力](https://code.claude.com/docs/en/cli-reference) | `--json-schema` | ❌ |

## 15. サーバー面・SDK

| 機能 | 補足 | ganja |
|---|---|---|
| [Agent SDK](https://docs.claude.com/en/api/agent-sdk/overview) | TS/Python でのエンジン組込み | ❌ 最も近いのは `ganja-serve` + `ganja-client`(HTTP/SSE) |
| [MCP サーバーモード](https://code.claude.com/docs/en/mcp) | `claude mcp serve` — エンジンを MCP サーバーとして公開 | ❌ |
| HTTP サーバー面 | なし — Claude Code は自前の REST/SSE API を提供しない | n/a — ganja 側の優位: `ganja serve`(REST + SSE・Basic 認証)と型付き `ganja-client` |

## 16. 環境変数

*本改訂(2026-08-12)で新設。文書化された面は広いので、挙動を左右する行に
絞る。*

| 変数 | 意味 | ganja |
|---|---|---|
| [`ANTHROPIC_API_KEY`](https://code.claude.com/docs/en/settings) | API キー資格情報 | ✅ 同じ変数・保存キーに優先 |
| [`ANTHROPIC_BASE_URL`](https://code.claude.com/docs/en/settings) | エンドポイント上書き | ✅ 同じ変数・https か loopback 以外は拒否 |
| [`ANTHROPIC_AUTH_TOKEN`](https://code.claude.com/docs/en/settings) | ゲートウェイ用カスタム bearer | ❌ |
| [`ANTHROPIC_MODEL`](https://code.claude.com/docs/en/model-config) | 既定モデル上書き | ⚠️ `GANJA_MODEL`(カタログ済みプロバイダはカタログ検証付き) |
| [`ANTHROPIC_DEFAULT_*_MODEL` / `ANTHROPIC_SMALL_FAST_MODEL`](https://code.claude.com/docs/en/settings) | エイリアス固定・安価モデルルーティング | ⚠️ 最も近いのは `small_model` config キー |
| [`CLAUDE_CODE_USE_BEDROCK` / `_VERTEX` / `_FOUNDRY`](https://code.claude.com/docs/en/amazon-bedrock) | クラウド基盤ルーティング | ❌ |
| [`MAX_THINKING_TOKENS`](https://code.claude.com/docs/en/settings) | thinking 予算 | ⚠️ effort variant がカタログ予算を運ぶ |
| [`CLAUDE_CODE_MAX_OUTPUT_TOKENS`](https://code.claude.com/docs/en/settings) | 応答上限 | ❌ |
| [`BASH_DEFAULT_TIMEOUT_MS` / `BASH_MAX_TIMEOUT_MS` / `BASH_MAX_OUTPUT_LENGTH`](https://code.claude.com/docs/en/settings) | シェルツール予算 | ⚠️ ganja のシェルは固定既定+呼出し毎 `timeout` 引数;env ノブなし |
| [`MCP_TIMEOUT` / `MCP_TOOL_TIMEOUT` / `MAX_MCP_OUTPUT_TOKENS`](https://code.claude.com/docs/en/settings) | MCP 予算 | ⚠️ 代わりにサーバー毎 `timeout`/`output_limit` config キー |
| [`DISABLE_TELEMETRY` / `DISABLE_ERROR_REPORTING` / `DISABLE_AUTOUPDATER` / `CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC`](https://code.claude.com/docs/en/settings) | 外部送信スイッチ | n/a — ganja には無効化すべきテレメトリ・エラー報告・自己更新がない |
| [`HTTP_PROXY` / `HTTPS_PROXY` / `NO_PROXY`](https://code.claude.com/docs/en/network-config) | プロキシ経路 | ⚠️ reqwest は標準プロキシ変数を尊重;未検証面で ganja 側ドキュメントなし |
| [`CLAUDE_CODE_OAUTH_TOKEN`](https://code.claude.com/docs/en/cli-reference) *(低確度)* | headless 用 OAuth トークン | ❌ |

ganja 自身の `GANJA_*` 面(config ホーム・fake プロバイダスクリプト・
カタログノブ・serve 資格情報・websearch キー・テスト opt-in)はリポジトリ
ルートの `AGENTS.md` に文書化されており、Claude Code に対応物はない。

## 17. エンタープライズ・プラットフォーム

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
