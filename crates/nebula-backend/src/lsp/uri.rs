//! ファイルパスと `file:` URI の相互変換。
//!
//! `lsp_types::Uri` は文字列を検証するだけでパスとの変換を持たないため自前で書く。
//! 日本語ファイル名や空白を含むパスはパーセントエンコードしないと URI として不正になり、
//! サーバー側で解釈できない。
//!
//! Unix と Windows の両方に対応する。Windows にはドライブレター絶対パス (`C:\...`) と
//! UNC パス (`\\server\share\...`) という Unix にない形があるため、パスの区切りが `\` か、
//! 先頭が `X:` かといった文字列の「形」で判定する。この判定とエンコード/デコードの本体は
//! `&str` を受け取る純粋関数にしてあり、`cfg(windows)` に依存しない。そのため macOS/Linux 上の
//! `cargo test` でも Windows 形式パスの変換を検証できる。実際にどちらの OS 向けに解釈するかを
//! 選ぶ箇所（`uri_to_path` の中）だけが `cfg(windows)` の境目になる。

use lsp_types::Uri;
use nebula_protocol::ProtocolError;
use std::path::{Path, PathBuf};
use std::str::FromStr;

/// 絶対パスを `file:` URI にする。
///
/// Unix の絶対パス (`/home/foo`)、Windows のドライブレターパス (`C:\Users\foo`)、
/// UNC パス (`\\server\share`) のいずれも受け付ける。実際にどの形かは `path_str_to_uri`
/// が文字列の形だけで判定するので、ここでは呼び出すだけでよい。
pub fn path_to_uri(path: &Path) -> Result<Uri, ProtocolError> {
    let text = path
        .to_str()
        .ok_or_else(|| ProtocolError::invalid(format!("UTF-8 でないパスです: {}", path.display())))?;
    if !path.is_absolute() {
        return Err(ProtocolError::invalid(format!(
            "LSP には絶対パスが要ります: {text}"
        )));
    }
    let encoded = path_str_to_uri(text);
    Uri::from_str(&encoded)
        .map_err(|e| ProtocolError::internal(format!("URI を組み立てられません ({encoded}): {e}")))
}

/// `file:` URI をパスに戻す。`file:` 以外や壊れた URI は `None`。
///
/// 権限部（ホスト名）の扱いは OS で意味が異なる。Unix にはネットワークパスという概念がないため
/// 権限部を持つ URI はローカルパスとして解釈できず `None` にする。Windows では権限部は UNC の
/// サーバー名を表すので、`\\server\share\...` として復元する。この分岐だけが実行環境の OS に
/// 依存する部分で、それ以外の文字列変換は `uri_str_to_unix_path` / `uri_str_to_windows_path` という
/// 純粋関数に閉じてある。
///
/// Windows ではさらに、ドライブレターの無い URI（`file:///tmp/a.rs` のような Unix 形式）も
/// `None` にする。理由は `uri_str_to_windows_path` のコメントを参照。
pub fn uri_to_path(uri: &Uri) -> Option<PathBuf> {
    let rest = uri.as_str().strip_prefix("file://")?;
    // クエリやフラグメントは file URI では意味を持たないので捨てる。
    let rest = rest.split(['?', '#']).next().unwrap_or(rest);
    let path = if cfg!(windows) {
        uri_str_to_windows_path(rest)?
    } else {
        uri_str_to_unix_path(rest)?
    };
    Some(PathBuf::from(path))
}

/// パス文字列を `file:` URI 文字列に変換する（純粋関数）。
///
/// UNC (`\\server\share\...`) → ドライブレター (`C:\...` / `C:/...`) → それ以外（Unix の絶対パス）
/// の順に文字列の形で判定する。ドライブレターのコロンは URI 上そのまま残す必要がある
/// （`%3A` にすると多くの言語サーバー実装がドライブパスとして認識できない）ため、
/// ドライブレター部分だけ `encode` を通さず生で連結する。
fn path_str_to_uri(text: &str) -> String {
    if let Some(unc_rest) = text.strip_prefix(r"\\") {
        // UNC パス: \\server\share\a.rs → file://server/share/a.rs
        format!("file://{}", encode(&unc_rest.replace('\\', "/")))
    } else if let Some(drive) = windows_drive_prefix(text) {
        // ドライブレターパス: C:\Users\foo\main.rs → file:///C:/Users/foo/main.rs
        let rest = text[drive.len()..].replace('\\', "/");
        format!("file:///{drive}{}", encode(&rest))
    } else {
        // Unix の絶対パス: /home/foo/main.rs → file:///home/foo/main.rs
        format!("file://{}", encode(text))
    }
}

/// 先頭が `<英字1文字>:` ならドライブレター部分（例: `"C:"`）を返す。
fn windows_drive_prefix(text: &str) -> Option<&str> {
    let bytes = text.as_bytes();
    (bytes.len() >= 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':').then(|| &text[..2])
}

/// `file://` を取り除いた残り部分を Unix のパス文字列に戻す（純粋関数）。
///
/// 正当な形は権限部が空の `/path` だけ。権限部つき（`host/path`）はローカルパスとして
/// 意味を持たないため `None` にする。
fn uri_str_to_unix_path(rest: &str) -> Option<String> {
    if !rest.starts_with('/') {
        return None;
    }
    decode(rest)
}

/// `file://` を取り除いた残り部分を Windows のパス文字列に戻す（純粋関数）。
///
/// 先頭が `/` ならドライブレターパス（`/C:/...` → `C:\...`）、それ以外（権限部つき）は
/// UNC パス（`server/share/...` → `\\server\share\...`）として復元する。
///
/// ドライブレターが無い場合（`file:///tmp/a.rs` のような Unix 形式の URI）は `None` にする。
/// 先頭の `/` を単純に落として `\tmp\a.rs` を返す案もあるが、それは `Path::is_absolute` が
/// 偽になる非絶対パスであり（`path_to_uri` 自身が絶対パス以外を拒否しているのと矛盾する）、
/// LSP サーバーに渡すとカレントドライブ基準で解決されて意図しないファイルを指しかねない。
/// 変換できないことを `None` で伝え、呼び出し側にエラーとして扱わせる方が安全。
/// ドライブレターの判定はパーセントデコードした後の文字列に対して行う。ドライブレターの
/// コロンをエンコードして送ってくるクライアントがいても取りこぼさないため。
///
/// ドライブレターの直後が `/` でない場合（`file:///C:foo` → `C:foo`）も同じ理由で `None` に
/// する。`C:foo` はドライブ相対パスであり、これも `Path::is_absolute` が偽になる非絶対パス。
fn uri_str_to_windows_path(rest: &str) -> Option<String> {
    if let Some(body) = rest.strip_prefix('/') {
        let decoded = decode(body)?;
        windows_drive_prefix(&decoded)?;
        if decoded.as_bytes().get(2) != Some(&b'/') {
            return None;
        }
        Some(decoded.replace('/', "\\"))
    } else if !rest.is_empty() {
        Some(format!(r"\\{}", decode(rest)?.replace('/', "\\")))
    } else {
        None
    }
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

    /// Unix 形式の絶対パス文字列を、実行環境の OS で実際に絶対パスとして通る形に直す。
    ///
    /// `path_to_uri` は `Path::is_absolute()` で絶対パスかどうかを判定するため、
    /// Windows ではドライブレターの無い `/Users/foo/...` は絶対パス扱いされない。
    /// end-to-end のテストは `path_to_uri` / `uri_to_path` の実物を通したいので、
    /// 入力の方を実行環境の OS に合わせて出し分ける。
    fn native_absolute(unix_style: &str) -> String {
        if cfg!(windows) {
            let rest = unix_style.trim_start_matches('/').replace('/', "\\");
            format!(r"C:\{rest}")
        } else {
            unix_style.to_string()
        }
    }

    fn roundtrip(unix_style_path: &str) {
        let path = native_absolute(unix_style_path);
        let uri = path_to_uri(Path::new(&path)).unwrap();
        assert_eq!(uri_to_path(&uri).unwrap(), PathBuf::from(&path));
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
        // エンコードの形自体は OS に依存しないので、`Path` 経由の絶対パス判定を
        // 挟まない純粋関数 `path_str_to_uri` を直接検証する。
        assert_eq!(path_str_to_uri("/a b/c.rs"), "file:///a%20b/c.rs");
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
        if cfg!(windows) {
            // Windows では権限部が UNC のサーバー名として解釈できるので None にはならない。
            assert_eq!(uri_to_path(&uri), Some(PathBuf::from(r"\\host\a.rs")));
        } else {
            // Unix にはネットワークパスという概念が無いため解釈できない。
            assert!(uri_to_path(&uri).is_none());
        }
    }

    // ここから下は Windows 形式パスの変換テスト。実行環境の OS に関わらず検証できるよう、
    // OS 判定を挟む `path_to_uri` / `uri_to_path` ではなく、文字列だけを扱う純粋関数
    // (`path_str_to_uri` / `uri_str_to_windows_path`) を直接呼ぶ。

    #[test]
    fn ドライブレター付きパスを_uri_へ変換できる() {
        let uri = path_str_to_uri(r"C:\Users\foo\main.rs");
        assert_eq!(uri, "file:///C:/Users/foo/main.rs");
    }

    #[test]
    fn uri_からドライブレター付きパスへ往復できる() {
        let original = r"C:\Users\foo\main.rs";
        let uri = path_str_to_uri(original);
        let rest = uri.strip_prefix("file://").unwrap();
        assert_eq!(uri_str_to_windows_path(rest).unwrap(), original);
    }

    #[test]
    fn 空白や日本語を含む_windows_パスがエンコードデコード往復する() {
        let original = r"C:\Users\foo\My Documents\設計 メモ.md";
        let uri = path_str_to_uri(original);
        assert_eq!(
            uri,
            "file:///C:/Users/foo/My%20Documents/%E8%A8%AD%E8%A8%88%20%E3%83%A1%E3%83%A2.md"
        );
        let rest = uri.strip_prefix("file://").unwrap();
        assert_eq!(uri_str_to_windows_path(rest).unwrap(), original);
    }

    #[test]
    fn ドライブレターの無い_uri_は_windows_ではパスにならない() {
        // file:///tmp/a.rs のような Unix 形式の URI をそのまま Windows パスとして
        // 復元すると `\tmp\a.rs` という、絶対パスにならない文字列になってしまう。
        // それを避けて None を返すことを、実行環境の OS に関わらず検証する。
        assert!(uri_str_to_windows_path("/tmp/a.rs").is_none());
    }

    #[test]
    fn ドライブ相対パスの_uri_は_windows_ではパスにならない() {
        // file:///C:foo のようにドライブレターの直後が `/` でない URI は、
        // 復元すると `C:foo` というドライブ相対パス（非絶対パス）になってしまう。
        // これも同じ理由で None にする。
        assert!(uri_str_to_windows_path("/C:foo").is_none());
        assert!(uri_str_to_windows_path("/C:").is_none());
    }

    #[test]
    fn unc_パスが往復する() {
        let original = r"\\server\share\a.rs";
        let uri = path_str_to_uri(original);
        assert_eq!(uri, "file://server/share/a.rs");
        let rest = uri.strip_prefix("file://").unwrap();
        assert_eq!(uri_str_to_windows_path(rest).unwrap(), original);
    }

    #[test]
    fn unix_パスの往復はこれまで通り動く() {
        let original = "/Users/foo/My Documents/設計 メモ.md";
        let uri = path_str_to_uri(original);
        let rest = uri.strip_prefix("file://").unwrap();
        assert_eq!(uri_str_to_unix_path(rest).unwrap(), original);
    }
}
