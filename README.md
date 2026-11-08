# ARCE.AI CLI

ARCE.AI を、ターミナルで使うコーディング／チャットのアシスタントにしたものです。Rust 製の 1 つの実行ファイル `arce` で、Linux・macOS・Windows で動きます。

- OpenAI 互換の窓口ならどのモデルでも（クラウドの提供元、この端末の Ollama・llama.cpp server など）。設定するまでは、ネットワークを使わないデモのモデルが答えます。
- 作業フォルダのファイルを読み書きし、コマンドを動かし、HTTP を送る道具を持ち、危ない操作は**承認のモード**（ask / auto-edit / plan / yolo）と **allow / ask / deny の規則**で止めます。
- 対話（REPL(Read-Eval-Print Loop)）と非対話（`arce -p`、出力は text / json / stream-json）。
- セッションの保存と再開、ターンごとの**取り消し**（`/undo`）、長い会話の**自動の要約**、**フォールバック**、MCP(Model Context Protocol) サーバー、フック、自作のスラッシュコマンド。
- **コーディングモード**（プランナーが Mermaid の図で仕事を分け、複数の開発モデルが並列・依存の順に実装）と、**秘密の扱い**（`{{secret:ID}}`、送ってよいホストの検査、伏せ字）。

## 入れる・はじめる
```bash
cargo install --path .
arce                                    # デモのモデルで試す
arce providers add --preset openrouter
arce secrets set openrouter             # キーは画面に出さずに入れる
arce providers add-model openrouter deepseek/deepseek-chat --context 131072
arce config set model "openrouter|deepseek/deepseek-chat"
arce
```

```bash
arce -p "README の誤字を直して" --mode auto-edit
git diff | arce -p "この差分をレビューして" --output-format json
```

コマンドの一覧は `arce --help`、対話の中では `/help`。

## 開発
```bash
cargo build
cargo test                       # 単体テストと、偽のサーバーに対する結合テスト（tests/e2e.rs）
cargo clippy --all-targets       # 警告 0 に保つ
cargo fmt
```
