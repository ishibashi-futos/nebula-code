use nebula_protocol::{ProtocolError, ProtocolErrorKind};
use std::fmt;

pub type Result<T> = std::result::Result<T, CoreError>;

#[derive(Debug)]
pub enum CoreError {
    /// 指定オフセットがバッファ長を超えている。
    OutOfBounds {
        offset: usize,
        len: usize,
    },
    /// 1 回の適用に渡された編集どうしが重なっている。
    OverlappingEdits,
    /// 編集の基準版数が現在の版数と一致しない。
    VersionMismatch {
        expected: u64,
        actual: u64,
    },
    /// 構文解析に失敗した。
    Parse(String),
    Io(std::io::Error),
}

impl fmt::Display for CoreError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CoreError::OutOfBounds { offset, len } => {
                write!(f, "オフセット {offset} はバッファ長 {len} を超えています")
            }
            CoreError::OverlappingEdits => write!(f, "編集範囲が重複しています"),
            CoreError::VersionMismatch { expected, actual } => {
                write!(f, "版数が不一致です (期待 {expected}, 実際 {actual})")
            }
            CoreError::Parse(msg) => write!(f, "構文解析に失敗しました: {msg}"),
            CoreError::Io(e) => write!(f, "入出力エラー: {e}"),
        }
    }
}

impl std::error::Error for CoreError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            CoreError::Io(e) => Some(e),
            _ => None,
        }
    }
}

impl From<std::io::Error> for CoreError {
    fn from(value: std::io::Error) -> Self {
        CoreError::Io(value)
    }
}

impl From<CoreError> for ProtocolError {
    fn from(value: CoreError) -> Self {
        let kind = match value {
            CoreError::OutOfBounds { .. }
            | CoreError::OverlappingEdits
            | CoreError::VersionMismatch { .. } => ProtocolErrorKind::InvalidRequest,
            CoreError::Parse(_) => ProtocolErrorKind::Internal,
            CoreError::Io(_) => ProtocolErrorKind::Io,
        };
        ProtocolError::new(kind, value.to_string())
    }
}
