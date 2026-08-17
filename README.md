# Nebula Code

Rust と [GPUI](https://www.gpui.rs/) のネイティブ描画性能を使ったコードエディタ。
黒 + ネオン + 宇宙をイメージした **Cyber-Cosmic** テーマ。

軽量な GUI 描画プロセスと、状態を持つバックエンドプロセスを IPC で分離した
マルチプロセス構成。設計の詳細は [ARCHITECTURE.md](ARCHITECTURE.md) を参照。

## 必要なもの

| | |
|---|---|
| Rust | 1.85 以上 (edition 2024) |
| macOS | Xcode + Metal Toolchain (`xcodebuild -downloadComponent MetalToolchain`) |
| Linux | Vulkan ドライバ、X11 または Wayland の開発パッケージ |

以下は **あれば使う** もの。無くても起動でき、該当機能だけが無効になる。

| ツール | 使う機能 |
|---|---|
| `git` | ソース管理パネル、行ガターの diff、blame |
| `rg` (ripgrep) | ワークスペース全文検索 |
| `rust-analyzer` | Rust の補完・定義ジャンプ・診断 |
| `typescript-language-server` | TypeScript / JavaScript の言語支援 |
| `pyright-langserver` | Python の言語支援 |
| `codex` | Codex パネル (`codex login` が必要) |

## ビルドと起動

```sh
cargo build --release
./target/release/nebula path/to/folder
```

GUI を起動するとバックエンド (`nebula-backend`) が自動で立ち上がる。
バックエンドはユーザーあたり 1 プロセスで、複数ウィンドウから共有される。

起動時間を測るには:

```sh
NEBULA_TRACE_STARTUP=1 ./target/release/nebula .
```

## キーバインド

| 操作 | キー |
|---|---|
| コマンドパレット | `⌘⇧P` |
| ファイルを開く (クイックオープン) | `⌘P` |
| フォルダを開く | `⌘O` |
| サイドバーの表示切り替え | `⌘B` |
| パネルの表示切り替え | `⌘J` |
| ターミナル | `⌃\`` |
| エクスプローラー / 検索 / Git / Codex | `⌘⇧E` / `⌘⇧F` / `⌘⇧G` / `⌘⇧A` |
| 保存 / 名前を付けて保存 | `⌘S` / `⌘⇧S` |
| タブを閉じる / 切り替え | `⌘W` / `⌃Tab` |
| 右に分割 | `⌘\` |
| ワークスペース切り替え | `⌘⇧]` |

エディタ内:

| 操作 | キー |
|---|---|
| 取り消し / やり直し | `⌘Z` / `⌘⇧Z` |
| 単語移動 | `⌥←` / `⌥→` |
| 行の複製 / 削除 | `⌘⇧D` / `⌘⇧K` |
| 行の移動 | `⌥↑` / `⌥↓` |
| コメント切り替え | `⌘/` |
| カーソルを追加 | `⌘⌥↑` / `⌘⌥↓` / `⌥クリック` |
| 次の同じ語を選択 | `⌘D` |
| 補完 | `⌃Space` (確定は `Tab` / `Enter`) |
| ホバー情報 | `⌘K ⌘I` |
| 定義へジャンプ | `F12` |
| 整形 | `⌥⇧F` |

## 構成

```
crates/
├── nebula-protocol/   IPC のメッセージ定義とフレーミング
├── nebula-core/       Rope バッファ・カーソル・Tree-sitter
├── nebula-backend/    状態管理プロセス (fs / 検索 / git / LSP / PTY / Codex)
└── nebula/            GUI プロセス (gpui のビューとテーマ)
```

## 対応言語 (構文着色)

Rust, TypeScript, TSX, JavaScript, Python, Go, JSON, TOML, Markdown, HTML, CSS

## テスト

```sh
cargo test --workspace
```

外部プロセスに依存するテスト (rust-analyzer の起動など) は `#[ignore]` を付けてある。
まとめて動かすには `cargo test --workspace -- --ignored`。
