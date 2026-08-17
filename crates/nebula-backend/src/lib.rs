//! Nebula Code の状態管理バックエンド。
//!
//! GUI から切り離された別プロセスとして動き、ファイル・検索・git・LSP・PTY・Codex
//! といった「重い、または外部プロセスを伴う」状態をすべて引き受ける。
//! GUI はこの結果を描画するだけでよく、描画スレッドが外部要因で止まらない。

pub mod buffers;
pub mod codex;
pub mod dispatch;
pub mod fsops;
pub mod git;
pub mod ipc;
pub mod lsp;
pub mod search;
pub mod state;
pub mod terminal;
pub mod tools;
pub mod watch;
pub mod workspace;

pub use state::BackendState;
