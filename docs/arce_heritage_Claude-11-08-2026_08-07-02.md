# arce.ai から受け継いだもの

arce.ai（`keusidan/arce.ai`、1.2.0 の `dev`）のデザインと独自の処理のうち、CLI に移したもの。移した先と、CLI に合わせて変えた点を書く。アプリ側のファイル名は、そのリポジトリの中のパス。

## デザイン

| アプリ | CLI | 変えた点 |
|---|---|---|
| 色のトークン（`lib/ui/theme.dart` の `ArceColors.light` / `dark`。`bg`・`surface`〜`surface3`・`ink`・`muted`・`faint`・`line`・`accent`・`accent2`・`accentSoft`・`green`・`amber`・`red` など） | `src/theme.rs` の `Palette::LIGHT` / `DARK`（**同じ 16 進の値**） | 24 ビット色が出せない端末は 256 色の近い色。`NO_COLOR`・`theme = "none"` で色なし。背景が明るいか暗いかは `COLORFGBG` から（無ければ暗い方） |
| 返信に名前・アイコン・枠を付けない。思考・道具・本文・操作を上から並べる。ユーザーの発言だけが吹き出し | 返信は枠なし。`◆ 考えています…` → 思考の本文（薄い字）→ `◇ 思考（12.3秒 · 1,234 文字）`、道具は `● 名前 要約` と `⎿ 結果` の 2 行 | 吹き出しの代わりに `›` のプロンプト |
| 思考の見出し（`ThinkingHeadline`：最後の `**…**`・`## …`・最後の行） | `ui::thinking_headline`。`ui.show_thinking = "summary"` のとき、1 行で `◆ 考えています… 12秒 見出し` | |
| 返信の進み具合の行（`通信 5 回（プランナー 3・開発 2） · ツール 1 回 · 所要 1:24`、キャッシュの数） | `ui::RunMeter::line`。返信のあとに薄い字で 1 行 | 同じ書き方 |
| 「モデルに送信しています（返信を待っています）」と、長い指示を端末のモデルが読む間の注意 | 最初の 1 字が来るまでの待ちの行（秒数つき）。この端末（127.0.0.1 など）のモデルで、指示が約 3,000 トークン以上のときは注意を添える | |
| 送る前のトークン数（`約 12,340 トークン（会話 8,000・入力 340・指示 1,500・道具 2,500）／文脈 131,072（9%）`） | `/context`、設定 `ui.token_line` で毎回 | |
| Markdown（`markdown_lite.dart`：見出し・強調・取り消し線・コード・リンク・リスト・タスク・引用・区切り・表・コードブロック。HTML・画像・脚注は文字のまま） | `src/markdown.rs`（ストリーミングで行ごと、表は最後の行が来てから。幅は全角を 2 と数える） | コードブロックは `╭─ rust` 〜 `╰─ rust · 20 行`。`ui.code_fold_lines` で長いものの頭だけ |
| リンクは http(s) だけ、ユーザー名つきのアドレスは断る（`safeWebUri`） | `markdown::safe_web_url` | |
| 承認カード（「実行する」「この道具は次から確認しない」「やめる」、長い引数は切る、確認する理由を出す） | `ui::TerminalUi::approve`（`[y]` `[a]` `[n]`、`n 理由` で理由をモデルに返す、差分は色つき） | |
| `ask_user` の質問カード（選択肢） | 番号か自由な答え | |

## 独自の処理

| アプリ | CLI | 変えた点 |
|---|---|---|
| エージェント（General Chat / Coding / Research / Artifact / File Analysis。キーワードで選び、添付があれば File Analysis に寄せる。利用者が固定できる） | `src/agents.rs`（同じ 5 つ、同じ選び方。日本語のキーワードも同じ） | CLI の道具に合わせて各エージェントの道具を決め直した。何も当たらないときは前の返信のエージェント（はじめは Coding）。迷うときにモデルに選ばせる処理は移していない |
| 会話の圧縮（古い道具の結果を先に切り詰め、それでも足りなければ、直近の発言より前を組ごとに要約。要約は残す最初の発言の頭に入れる。切り目はユーザーの発言。失敗したら履歴を変えない） | `src/compaction.rs`・`Controller::make_room`（文言・既定値も同じ：80%・4 件・3000 字・1500 字） | `/compact` で手動 |
| トークンの事前チェック（指示と道具が文脈の 80% を超える、会話が入らない → 送らずにエラー。どの MCP が大きいかを言う） | `Controller::precheck` | |
| 同じ道具・同じ引数の繰り返しで止める、ラウンドの上限 30 | 同じ（`limits.repeat_limit`・`limits.max_tool_rounds`） | |
| 失敗でエラーにしない（使えない道具の呼び出しは、理由を伝えて書き直させる。空の返信はやり直す） | 同じ（`limits.retry_limit`） | |
| 秘密（`{{secret:ID}}`、送ってよいホスト、`SecretHosts.problem` の検査、`SecretRedaction` の伏せ字、`ARCE_SECRET_<ID>`） | `src/secrets.rs`（検査の規則と伏せる形を移植。テストも移した） | 保存先は所有者だけが読める `secrets.json`（アプリは端末の安全な領域） |
| `http_request`（道 A：アプリが差し込んで送る。最終の URL のホストを確かめる。同じホストの https にだけリダイレクト） | `tools::http_request` | |
| 機密情報以外はすべて設定。チャットから変えられる設定の一覧と、変えさせない設定（理由つき） | `settings::CHAT_CHANGEABLE`・`blocked_reason`、道具 `get_settings`・`update_settings`（毎回承認） | |
| プロバイダの追加ヘッダーに `Authorization` などを書かせない、`base_url` に `user:pass@`・`?`・`#` を書かせない | `ProviderConfig::problems` | |
| フォールバック（`provider|model`・`provider|*`・`*`、具体的なものが先、既定の状態 401・403・404・408・429・5xx・接続失敗、**最初の 1 字の前だけ**、「A が 403 のため B に切り替えました」） | `provider::fallback_targets`・`Controller::request` | |
| このキーで使えないモデルの分類（`limit: 0`・`insufficient_quota`・403 の権限・404 のモデル名など。1 分あたりの制限は「一時的」） | `provider::classify`（同じ判断と日本語の理由） | 印を保存する仕組みは移していない（エラー文に理由を出すだけ） |
| 思考の送り方（DeepSeek・Z.ai・chat template・OpenAI・OpenRouter・送らない）、速度⇔精度からの強さ（0.5 未満 low・0.85 未満 high・以上 max） | `provider::ThinkingConfig` | |
| 過去の思考を返す（DeepSeek・GLM で function calling のとき `reasoning_content`） | `ChatRequest::pass_reasoning` | |
| ツールの送り方（function calling／テキスト形式 `<tool_call>`）、Gemini の同じ `index` に別の `id`、`{…}{…}` の分割、思考署名（`extra_content`）を返す | `llm::CallAssembler`・`split_objects`・`wire_messages` | |
| プロンプトのキャッシュ用の印（`prompt_cache_key`、既定オフ、会話ごとの印、名前を挙げて断られたら外して送り直す） | 同じ（`providers.prompt_cache`・`prompt_cache_field`） | |
| プロンプトに時刻などの毎回変わる文字を入れない（キャッシュが当たるように） | `src/prompt.rs`（テストで確かめる） | |
| コーディングモード（プランナーが Mermaid の図で分け、開発モデルが並列・依存の順に実装。待つ仕事に結果を渡す。前が失敗したら実行しない。質問には 1 回だけ「自分で決めて」。失敗は別のモデルへ 1 回。結果は d1, d2 … で返し、`revise` で直させる） | `src/plan_graph.rs`・`src/delegation.rs`（指示の文も同じ） | 開発モデルが `File: パス` で書いたファイルを、作業フォルダに**承認つきで書く**（`coding.apply_files`）。アプリの「Linux 環境の CLI エージェントに渡す」案内は、参考ツールの名前を含むので移していない |
| MCP（道具が多いときは `mcp_find`・`mcp_describe`・`mcp_call`。許可記憶は MCP の道具だけ） | `src/mcp.rs`（stdio） | アプリは Streamable HTTP。CLI は stdio |
| 承認の許可記憶はコマンドに使わない。外部の結果を読んだあとのコマンドは確認する（`shellConfirmAfterExternal`） | `permissions.rs`（`approved` はコマンドを含まない、`confirm_after_external`） | |
| デモのエンジン（推論なしで画面と機能が動く） | `demo|arce-demo` | |
| セッションの題（最初の質問） | 最初の発言の 1 行目（`title_max_chars`） | モデルに題を考えさせる処理は移していない |
