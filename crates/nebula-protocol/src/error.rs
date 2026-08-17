use serde::{Deserialize, Serialize};
use std::fmt;

/// リクエスト失敗をワイヤ越しに運ぶためのエラー。
///
/// バックエンド内部のエラー型をそのまま直列化すると GUI 側が内部構造に依存してしまうため、
/// 分類 (`kind`) と人間可読なメッセージだけに落として送る。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProtocolError {
    pub kind: ProtocolErrorKind,
    pub message: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ProtocolErrorKind {
    /// 要求された ID (バッファ・ワークスペース等) が存在しない。
    NotFound,
    /// 入力が不正。リトライしても成功しない。
    InvalidRequest,
    /// I/O 失敗。
    Io,
    /// 外部プロセス (git / rg / LSP / codex) が失敗した。
    ExternalTool,
    /// 機能が未対応。
    Unsupported,
    /// 要求が取り消された。
    Cancelled,
    /// 上記に当てはまらない内部エラー。
    Internal,
}

impl ProtocolError {
    pub fn new(kind: ProtocolErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
        }
    }

    pub fn not_found(message: impl Into<String>) -> Self {
        Self::new(ProtocolErrorKind::NotFound, message)
    }

    pub fn invalid(message: impl Into<String>) -> Self {
        Self::new(ProtocolErrorKind::InvalidRequest, message)
    }

    pub fn io(message: impl Into<String>) -> Self {
        Self::new(ProtocolErrorKind::Io, message)
    }

    pub fn external(message: impl Into<String>) -> Self {
        Self::new(ProtocolErrorKind::ExternalTool, message)
    }

    pub fn unsupported(message: impl Into<String>) -> Self {
        Self::new(ProtocolErrorKind::Unsupported, message)
    }

    pub fn cancelled() -> Self {
        Self::new(ProtocolErrorKind::Cancelled, "要求は取り消されました")
    }

    pub fn internal(message: impl Into<String>) -> Self {
        Self::new(ProtocolErrorKind::Internal, message)
    }
}

impl fmt::Display for ProtocolError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:?}: {}", self.kind, self.message)
    }
}

impl std::error::Error for ProtocolError {}

impl From<std::io::Error> for ProtocolError {
    fn from(value: std::io::Error) -> Self {
        Self::io(value.to_string())
    }
}
