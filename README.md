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
| Windows | Visual Studio Build Tools (MSVC) と Windows SDK。DirectX と DirectWrite は SDK に含まれる |

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

## 更新

`nebula update` で GitHub Releases から最新版を取得し、実行中の `nebula` と
隣に置かれた `nebula-backend` の両方を自己置換する (`curl` が必要)。

```sh
nebula update          # 最新版があれば nebula / nebula-backend を両方更新する
nebula update --check  # 更新の有無だけ確認して終了する (何も書き換えない)
nebula update --force  # 同じか古いバージョンでも再インストールする
```

`nebula update` はウィンドウを開かず、結果を表示して終了する。終了コードは
`0` (最新版、または更新完了)・`1` (`--check` で新しいバージョンが見つかった)・
`2` (エラー) を使い分ける。

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

## CI / リリース

| ワークフロー | いつ動くか | すること |
|---|---|---|
| `.github/workflows/ci.yml` | push (`main` / `feat/**`)、pull request、手動 | macOS(arm64)・Linux(x64)・Windows(x64) で `cargo build --locked` と `cargo test --locked` |
| `.github/workflows/release.yml` | タグ push (`v*`)、手動 | 4 プラットフォーム向けにビルドし、チェックサムを添えて GitHub Release へ添付 |

ツールチェーンと GPUI のビルド依存 (Linux の Wayland/X11/Vulkan 一式、macOS の
Metal ツールチェーン) は `.github/actions/setup-rust` にまとめてあり、両方から使う。
Windows は追加で入れるものが無い (必要な DirectX / DirectWrite / `fxc.exe` は
ランナーに入っている Windows SDK が持っている)。

リリースが作る配布物:

```
nebula-darwin-arm64        nebula-backend-darwin-arm64
nebula-darwin-x64          nebula-backend-darwin-x64
nebula-linux-x64           nebula-backend-linux-x64
nebula-windows-x64.exe     nebula-backend-windows-x64.exe
SHA256SUMS
```

この名前は `nebula update` が探しに行く名前 (`crates/nebula/src/update.rs` の
`resolve_asset_name`) と完全に一致していなければならない。片方だけ変えると更新が
必ず失敗するので、`cargo test` がワークフローの中身を読んで突き合わせている。

**リリースを試すとき**は、いきなりタグを打たずに Actions から `Release` を
`workflow_dispatch` で流すこと。手動実行は必ず**下書き**として作られるので、
公開せずにパイプラインの成否だけ確かめられる。

`cargo fmt --check` と `cargo clippy -D warnings` は CI に入れていない。
既存コードに非準拠が 95 箇所・clippy の指摘が 25 件あり、入れた時点で赤になるため。
門番にするなら、先に整形と指摘の解消だけを行うコミットを分けて入れること。
