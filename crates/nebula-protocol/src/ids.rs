use serde::{Deserialize, Serialize};
use std::fmt;
use std::sync::atomic::{AtomicU64, Ordering};

/// 用途ごとに別型の ID を生成するマクロ。
///
/// すべて `u64` の新型で、取り違えをコンパイル時に防ぐ。ID の採番は生成側 (GUI かバックエンドか)
/// が決まっているものは `next()` を使い、そうでないものは相手から受け取った値を包むだけにする。
macro_rules! define_id {
    ($(#[$meta:meta])* $name:ident) => {
        $(#[$meta])*
        #[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
        #[serde(transparent)]
        pub struct $name(pub u64);

        impl $name {
            /// プロセス内で単調増加する新しい ID を採番する。
            pub fn next() -> Self {
                static COUNTER: AtomicU64 = AtomicU64::new(1);
                Self(COUNTER.fetch_add(1, Ordering::Relaxed))
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(f, "{}", self.0)
            }
        }
    };
}

define_id!(
    /// リクエストとレスポンスを対応付ける ID。GUI 側が採番する。
    RequestId
);
define_id!(
    /// 開いているワークスペース (フォルダ) の ID。バックエンドが採番する。
    WorkspaceId
);
define_id!(
    /// 開いているバッファの ID。バックエンドが採番する。
    BufferId
);
define_id!(
    /// 進行中の検索セッションの ID。バックエンドが採番する。
    SearchId
);
define_id!(
    /// PTY セッションの ID。バックエンドが採番する。
    TerminalId
);
define_id!(
    /// Codex 会話の ID。バックエンドが採番する。
    CodexConversationId
);
define_id!(
    /// LSP 要求の ID。バックエンドが採番し、キャンセルに使う。
    LspRequestId
);
