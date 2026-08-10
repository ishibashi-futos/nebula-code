Rust/GPUIのネイティブ描画性能を最大限に活かしたエディタ **Nebula Code**

### Architecture

* GUI Framework: GPUI
* TextCore: Ropeデータ構造
* 構文解析・着色: Tree-sitter

GPUI, ropey, LSP(Server-Side), libghostty, tokioなどは既存のクレートを使用しても良いが、軽い作業のためなどにクレート導入はNG。

## Requirements

* 軽量GUI描画プロセスと状態管理バックグラウンドプロセスをIPCで分離するマルチプロセスアーキテクチャを採用する
