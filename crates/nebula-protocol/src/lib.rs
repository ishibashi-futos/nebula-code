//! Nebula Code の IPC プロトコル。
//!
//! GUI 描画プロセス (`nebula`) と状態管理バックエンドプロセス (`nebula-backend`) は
//! Unix ドメインソケット上で本クレートが定義するメッセージを交換する。
//!
//! ワイヤ形式は「4 バイトのビッグエンディアン長 + MessagePack ペイロード」。
//! MessagePack を選ぶのは、1 フレームあたりのバイト数と直列化コストの双方が
//! JSON より小さく、ターミナル出力やハイライトスパンのような高頻度イベントで
//! 差が効くため。

mod codec;
mod error;
mod ids;
mod message;
mod types;

pub use codec::{FrameDecoder, MAX_FRAME_BYTES, encode_frame};
pub use error::{ProtocolError, ProtocolErrorKind};
pub use ids::{
    BufferId, CodexConversationId, LspRequestId, RequestId, SearchId, TerminalId, WorkspaceId,
};
pub use message::{
    ClientMessage, CodeAction, Event, LanguageConfig, NotificationLevel, Request, Response,
    ServerMessage,
};
pub use types::*;

/// GUI とバックエンドの互換性検査に使うプロトコル版数。
///
/// 双方は同一バイナリ配布物から起動される前提のため、不一致は即座に致命的として扱う。
pub const PROTOCOL_VERSION: u32 = 1;

/// バックエンドが待ち受けるソケットのパスを決定する。
///
/// `$XDG_RUNTIME_DIR` があればそれを、無ければ `$TMPDIR` を使う。ワークスペース単位ではなく
/// ユーザー単位で 1 デーモンを共有し、複数ウィンドウから同じバックエンドに接続する。
pub fn default_socket_path() -> std::path::PathBuf {
    let dir = std::env::var_os("XDG_RUNTIME_DIR")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    dir.join(format!("nebula-{}.sock", PROTOCOL_VERSION))
}
