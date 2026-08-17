//! ファイルパスと `file:` URI の相互変換。
//!
//! `lsp_types::Uri` は文字列を検証するだけでパスとの変換を持たないため自前で書く。
//! 日本語ファイル名や空白を含むパスはパーセントエンコードしないと URI として不正になり、
//! サーバー側で解釈できない。

use lsp_types::Uri;
use nebula_protocol::ProtocolError;
use std::path::{Path, PathBuf};
use std::str::FromStr;

/// 絶対パスを `file:///...` 形式の URI にする。
pub fn path_to_uri(path: &Path) -> Result<Uri, ProtocolError> {
    let text = path
        .to_str()
        .ok_or_else(|| ProtocolError::invalid(format!("UTF-8 でないパスです: {}", path.display())))?;
    if !path.is_absolute() {
        return Err(ProtocolError::invalid(format!(
            "LSP には絶対パスが要ります: {text}"
        )));
    }
    let encoded = format!("file://{}", encode(text));
    Uri::from_str(&encoded)
        .map_err(|e| ProtocolError::internal(format!("URI を組み立てられません ({encoded}): {e}")))
}

/// `file:` URI をパスに戻す。`file:` 以外や壊れた URI は `None`。
pub fn uri_to_path(uri: &Uri) -> Option<PathBuf> {
    let rest = uri.as_str().strip_prefix("file://")?;
    // `file://host/path` のような権限部つきはローカルパスに落とせない。
    // 正当な形は権限部が空の `file:///path` だけ。
    if !rest.starts_with('/') {
        return None;
    }
    // クエリやフラグメントは file URI では意味を持たないので捨てる。
    let path = rest.split(['?', '#']).next().unwrap_or(rest);
    Some(PathBuf::from(decode(path)?))
}

/// RFC 3986 の unreserved 文字とパス区切りだけをそのまま残し、他はすべて `%XX` にする。
///
/// 残せる文字を最小限にしているのは、どの言語サーバーの URI 実装でも確実に
/// 解釈できる形にするため。過剰にエンコードしても意味は変わらない。
fn encode(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for byte in text.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' | b'/' => {
                out.push(byte as char)
            }
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

fn decode(text: &str) -> Option<String> {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            let hex = text.get(i + 1..i + 3)?;
            out.push(u8::from_str_radix(hex, 16).ok()?);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8(out).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn roundtrip(path: &str) {
        let uri = path_to_uri(Path::new(path)).unwrap();
        assert_eq!(uri_to_path(&uri).unwrap(), PathBuf::from(path));
    }

    #[test]
    fn 通常のパスが往復する() {
        roundtrip("/Users/foo/src/main.rs");
    }

    #[test]
    fn 空白と日本語を含むパスが往復する() {
        roundtrip("/Users/foo/My Documents/設計 メモ.md");
    }

    #[test]
    fn 絵文字を含むパスが往復する() {
        roundtrip("/tmp/🚀/a.rs");
    }

    #[test]
    fn 空白はパーセントエンコードされる() {
        let uri = path_to_uri(Path::new("/a b/c.rs")).unwrap();
        assert_eq!(uri.as_str(), "file:///a%20b/c.rs");
    }

    #[test]
    fn 相対パスは拒否される() {
        assert!(path_to_uri(Path::new("src/main.rs")).is_err());
    }

    #[test]
    fn file_以外のスキームはパスにならない() {
        let uri = Uri::from_str("https://example.com/a.rs").unwrap();
        assert!(uri_to_path(&uri).is_none());
    }

    #[test]
    fn 権限部つきの_file_uri_はパスにならない() {
        let uri = Uri::from_str("file://host/a.rs").unwrap();
        assert!(uri_to_path(&uri).is_none());
    }
}
