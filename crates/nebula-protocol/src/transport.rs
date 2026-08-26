//! バックエンドへ繋ぐクライアント側の接続。
//!
//! 実体は Unix ではドメインソケット、Windows では名前付きパイプ
//! (どこへ繋ぐかは `endpoint` モジュールが決める)。どちらも
//! `Read + Write` できる 1 本のバイトストリームとして同じ形に見せる。
//!
//! **同期 API である**ことが要件。GUI はブロッキング I/O を専用の OS スレッドで
//! 回しており、gpui 以外の非同期ランタイムを持ち込めない。バックエンド側の
//! 待ち受け (`nebula-backend` の `ipc`) が tokio なのと対になっている。
//!
//! GUI 本体と結合テストの双方がここを使う。実装が 1 つなので、CI が
//! 結合テストを走らせれば GUI が実際に使う経路がそのまま検査される。

#[cfg_attr(windows, path = "transport/windows.rs")]
#[cfg_attr(unix, path = "transport/unix.rs")]
mod platform;

pub use platform::{Stream, connect};
