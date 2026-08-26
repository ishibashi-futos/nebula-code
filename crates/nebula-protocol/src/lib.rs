//! Nebula Code の IPC プロトコル。
//!
//! GUI 描画プロセス (`nebula`) と状態管理バックエンドプロセス (`nebula-backend`) は
//! 1 本のバイトストリーム上で本クレートが定義するメッセージを交換する。
//! ストリームの実体は Unix ではドメインソケット、Windows では名前付きパイプ
//! (`endpoint` モジュールを参照)。
//!
//! ワイヤ形式は「4 バイトのビッグエンディアン長 + MessagePack ペイロード」。
//! MessagePack を選ぶのは、1 フレームあたりのバイト数と直列化コストの双方が
//! JSON より小さく、ターミナル出力やハイライトスパンのような高頻度イベントで
//! 差が効くため。

mod codec;
mod endpoint;
mod error;
mod ids;
mod message;
pub mod transport;
mod types;

pub use codec::{FrameDecoder, MAX_FRAME_BYTES, encode_frame};
pub use endpoint::{default_endpoint, endpoint_named};
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
