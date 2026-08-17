//! LSP の `Content-Length` フレーミング。
//!
//! nebula-protocol の「4 バイト長 + MessagePack」とは別物なので専用に実装する。
//! 書式は HTTP 風のヘッダ + 空行 + JSON 本体:
//!
//! ```text
//! Content-Length: 42\r\n
//! \r\n
//! {"jsonrpc":"2.0", ...}
//! ```
//!
//! 解析部を [`take_frame`] という純粋関数に切り出しているのは、
//! 「分割して届く」「1 回の read に複数メッセージが入る」という
//! 実運用で必ず起きる状況をプロセス無しでテストするため。

use nebula_protocol::ProtocolError;

/// ヘッダ部と本体を区切る空行。
const SEPARATOR: &[u8] = b"\r\n\r\n";

/// バッファ先頭から 1 メッセージ取り出す。
///
/// - 取り出せたぶんだけ `buf` の先頭から取り除く。
/// - まだメッセージが揃っていなければ `Ok(None)` を返し、`buf` は変更しない。
pub fn take_frame(buf: &mut Vec<u8>) -> Result<Option<Vec<u8>>, ProtocolError> {
    let Some(header_end) = find(buf, SEPARATOR) else {
        return Ok(None);
    };
    let header = std::str::from_utf8(&buf[..header_end])
        .map_err(|_| ProtocolError::external("LSP のヘッダが UTF-8 ではありません"))?;
    let length = content_length(header)?;

    let body_start = header_end + SEPARATOR.len();
    let body_end = body_start + length;
    if buf.len() < body_end {
        return Ok(None);
    }
    let body = buf[body_start..body_end].to_vec();
    buf.drain(..body_end);
    Ok(Some(body))
}

/// フレームを組み立てる。
pub fn encode_frame(body: &[u8]) -> Vec<u8> {
    let mut frame = format!("Content-Length: {}\r\n\r\n", body.len()).into_bytes();
    frame.extend_from_slice(body);
    frame
}

/// ヘッダから `Content-Length` を取り出す。
///
/// ヘッダ名の大小は区別せず、`Content-Type` のような未知のヘッダは読み飛ばす。
/// 仕様上 `Content-Type` を送ってくるサーバーが実在するため。
fn content_length(header: &str) -> Result<usize, ProtocolError> {
    header
        .lines()
        .find_map(|line| {
            let (name, value) = line.split_once(':')?;
            name.trim()
                .eq_ignore_ascii_case("content-length")
                .then(|| value.trim().parse::<usize>().ok())?
        })
        .ok_or_else(|| {
            ProtocolError::external(format!("LSP のヘッダに Content-Length がありません: {header:?}"))
        })
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(body: &str) -> Vec<u8> {
        encode_frame(body.as_bytes())
    }

    #[test]
    fn 単一のフレームを取り出せる() {
        let mut buf = frame(r#"{"id":1}"#);
        let body = take_frame(&mut buf).unwrap().unwrap();
        assert_eq!(body, br#"{"id":1}"#);
        assert!(buf.is_empty());
    }

    #[test]
    fn 分割して届いても取りこぼさない() {
        let full = frame(r#"{"id":1}"#);
        let mut buf = Vec::new();
        // ヘッダの途中まで。
        buf.extend_from_slice(&full[..8]);
        assert!(take_frame(&mut buf).unwrap().is_none());
        // ヘッダは揃ったが本体が足りない。
        buf.extend_from_slice(&full[8..full.len() - 3]);
        assert!(take_frame(&mut buf).unwrap().is_none());
        // 残りが届いて初めて取り出せる。
        buf.extend_from_slice(&full[full.len() - 3..]);
        assert_eq!(take_frame(&mut buf).unwrap().unwrap(), br#"{"id":1}"#);
    }

    #[test]
    fn 連結された複数メッセージを順に取り出せる() {
        let mut buf = frame(r#"{"id":1}"#);
        buf.extend_from_slice(&frame(r#"{"id":2}"#));
        buf.extend_from_slice(&frame(r#"{"id":3}"#));

        assert_eq!(take_frame(&mut buf).unwrap().unwrap(), br#"{"id":1}"#);
        assert_eq!(take_frame(&mut buf).unwrap().unwrap(), br#"{"id":2}"#);
        assert_eq!(take_frame(&mut buf).unwrap().unwrap(), br#"{"id":3}"#);
        assert!(take_frame(&mut buf).unwrap().is_none());
        assert!(buf.is_empty());
    }

    #[test]
    fn content_type_ヘッダがあっても読める() {
        let body = r#"{"ok":true}"#;
        let mut buf = format!(
            "Content-Length: {}\r\nContent-Type: application/vscode-jsonrpc; charset=utf-8\r\n\r\n{body}",
            body.len()
        )
        .into_bytes();
        assert_eq!(take_frame(&mut buf).unwrap().unwrap(), body.as_bytes());
    }

    #[test]
    fn ヘッダ名の大文字小文字は区別しない() {
        let body = "{}";
        let mut buf = format!("content-length: {}\r\n\r\n{body}", body.len()).into_bytes();
        assert_eq!(take_frame(&mut buf).unwrap().unwrap(), body.as_bytes());
    }

    #[test]
    fn 本体が_utf8_マルチバイトでもバイト長で数える() {
        let body = r#"{"m":"日本語🚀"}"#;
        let mut buf = frame(body);
        assert_eq!(take_frame(&mut buf).unwrap().unwrap(), body.as_bytes());
    }

    #[test]
    fn content_length_がなければエラー() {
        let mut buf = b"Content-Type: text/plain\r\n\r\n{}".to_vec();
        assert!(take_frame(&mut buf).is_err());
    }
}
