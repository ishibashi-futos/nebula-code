//! Unix ドメインソケットによるクライアント接続。

use std::io::{Read, Write};
use std::path::Path;
use std::time::Duration;

/// バックエンドとの接続 1 本。
///
/// `UnixStream` をそのまま公開せず包むのは、Windows 側と同じ形の API
/// (`try_clone` / `set_read_timeout`) だけを見せて、利用側に
/// プラットフォーム差を持ち込ませないため。
pub struct Stream(std::os::unix::net::UnixStream);

pub fn connect(endpoint: &Path) -> std::io::Result<Stream> {
    std::os::unix::net::UnixStream::connect(endpoint).map(Stream)
}

impl Stream {
    /// 読み取り用と書き込み用に分けるための複製。
    pub fn try_clone(&self) -> std::io::Result<Self> {
        self.0.try_clone().map(Stream)
    }

    /// 読み取りが返らないまま止まらないようにする上限。
    ///
    /// `None` は無制限。GUI は応答をいつまでも待ってよいので `None` のまま使い、
    /// 結合テストだけが上限を設ける。
    ///
    /// 期限切れのエラー種別はプラットフォームで違う (ここでは `WouldBlock`、
    /// Windows では `TimedOut`)。利用側は種別で分岐せず、読み取りが `Err` に
    /// なったことだけを見ること。
    pub fn set_read_timeout(&mut self, timeout: Option<Duration>) -> std::io::Result<()> {
        self.0.set_read_timeout(timeout)
    }
}

impl Read for Stream {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        self.0.read(buf)
    }
}

impl Write for Stream {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.write(buf)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.0.flush()
    }
}
