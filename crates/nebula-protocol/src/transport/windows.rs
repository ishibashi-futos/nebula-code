//! 名前付きパイプによるクライアント接続。
//!
//! ここだけは Unix 側のように std へ委ねられない。std には名前付きパイプの
//! クライアントが無く、`std::fs::File` で `\\.\pipe\...` を開く手はあるものの、
//! **同期ハンドルでは読み取りと書き込みが直列化される**ため使えない。
//! Windows の I/O マネージャは、同期用に開かれたファイルオブジェクトへの要求を
//! オブジェクト単位のロックで直列化する。GUI は応答待ちで読み取りをブロックした
//! まま次の要求を書き込むので、同期ハンドルだとその書き込みが読み取りの完了を
//! 待ってしまい、事実上のデッドロックになる。
//!
//! そこで `FILE_FLAG_OVERLAPPED` で開き、1 操作ごとにイベントを添えて完了を待つ。
//! 呼び出し側からは素朴なブロッキング I/O に見えたまま、読み取りと書き込みが
//! 互いを止めない。

use std::ffi::OsStr;
use std::io::{Read, Write};
use std::os::windows::ffi::OsStrExt;
use std::path::Path;
use std::ptr;
use std::sync::Arc;
use std::time::Duration;

use windows_sys::Win32::Foundation::{
    CloseHandle, ERROR_BROKEN_PIPE, ERROR_IO_PENDING, GENERIC_READ, GENERIC_WRITE, GetLastError,
    HANDLE, INVALID_HANDLE_VALUE, WAIT_TIMEOUT,
};
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, FILE_FLAG_OVERLAPPED, OPEN_EXISTING, ReadFile, WriteFile,
};
use windows_sys::Win32::System::IO::{
    CancelIoEx, GetOverlappedResult, GetOverlappedResultEx, OVERLAPPED,
};
use windows_sys::Win32::System::Threading::{CreateEventW, INFINITE, ResetEvent};

/// カーネルオブジェクトのハンドル。drop で閉じる。
///
/// `HANDLE` は生ポインタなので既定では `Send`/`Sync` にならないが、実体は
/// プロセス内で共有してよいカーネルオブジェクトの識別子なので手当てする。
struct OwnedHandle(HANDLE);

impl Drop for OwnedHandle {
    fn drop(&mut self) {
        // SAFETY: 自分が開いたハンドルを、他に参照が無くなった時点で 1 度だけ閉じる。
        unsafe {
            CloseHandle(self.0);
        }
    }
}

// SAFETY: ハンドルはプロセス内のどのスレッドからでも同じオブジェクトを指す。
// 重畳 I/O なら同一ハンドルへの同時発行も許されている (操作ごとに別の
// OVERLAPPED とイベントを使う限り。それは `Stream` が保証している)。
unsafe impl Send for OwnedHandle {}
unsafe impl Sync for OwnedHandle {}

/// バックエンドとの接続 1 本。
pub struct Stream {
    pipe: Arc<OwnedHandle>,
    /// この `Stream` 専用の完了待ちイベント。
    ///
    /// 重畳 I/O は 1 操作につき 1 つのイベントを要求する。読み取りスレッドと
    /// 書き込みスレッドが同じイベントを共有すると、片方の完了でもう片方が
    /// 起こされて転送量を取り違える。だから `try_clone` では必ず作り直す。
    event: OwnedHandle,
    read_timeout: Option<Duration>,
}

pub fn connect(endpoint: &Path) -> std::io::Result<Stream> {
    let name = wide(endpoint.as_os_str());
    // SAFETY: name は終端 NUL 付きの UTF-16 列。他のポインタ引数は、
    // この API が省略を許している箇所にのみ null を渡している。
    let handle = unsafe {
        CreateFileW(
            name.as_ptr(),
            GENERIC_READ | GENERIC_WRITE,
            // 1 本の接続を 1 つの GUI が占有する。共有する相手が居ない。
            0,
            ptr::null(),
            OPEN_EXISTING,
            FILE_FLAG_OVERLAPPED,
            ptr::null_mut(),
        )
    };
    if handle == INVALID_HANDLE_VALUE {
        return Err(std::io::Error::last_os_error());
    }
    Ok(Stream {
        pipe: Arc::new(OwnedHandle(handle)),
        event: create_event()?,
        read_timeout: None,
    })
}

impl Stream {
    /// 読み取り用と書き込み用に分けるための複製。
    ///
    /// ハンドルは複製しない。`DuplicateHandle` で増やしても同じファイル
    /// オブジェクトを指すだけで、重畳 I/O では 1 本のままでも同時に発行できる。
    /// 作り直すのは取り違えを避けたいイベントの方だけ。
    pub fn try_clone(&self) -> std::io::Result<Self> {
        Ok(Self {
            pipe: self.pipe.clone(),
            event: create_event()?,
            read_timeout: self.read_timeout,
        })
    }

    /// 読み取りが返らないまま止まらないようにする上限。
    ///
    /// `None` は無制限。GUI は応答をいつまでも待ってよいので `None` のまま使い、
    /// 結合テストだけが上限を設ける。
    ///
    /// 期限切れのエラー種別はプラットフォームで違う (ここでは `TimedOut`、
    /// Unix では `WouldBlock`)。利用側は種別で分岐せず、読み取りが `Err` に
    /// なったことだけを見ること。
    pub fn set_read_timeout(&mut self, timeout: Option<Duration>) -> std::io::Result<()> {
        self.read_timeout = timeout;
        Ok(())
    }

    /// 重畳 I/O を 1 回発行して、完了するまで待つ。
    ///
    /// 同期的に完了した場合も待ち合わせを通す。分岐を増やさない方が
    /// 「転送量は必ず 1 か所から得る」という不変を保ちやすい。
    fn perform(
        &self,
        timeout: Option<Duration>,
        issue: impl FnOnce(HANDLE, *mut OVERLAPPED) -> windows_sys::core::BOOL,
    ) -> std::io::Result<usize> {
        // SAFETY: overlapped はこの関数が返るまで生存する。待ち合わせから
        // 抜ける経路は「完了した」か「取り消しの完了まで待った」かのどちらかしか
        // 無いので、カーネルがこの領域へ書き込む余地を残したまま戻ることはない。
        unsafe {
            if ResetEvent(self.event.0) == 0 {
                return Err(std::io::Error::last_os_error());
            }
            let mut overlapped: OVERLAPPED = std::mem::zeroed();
            overlapped.hEvent = self.event.0;

            if issue(self.pipe.0, &mut overlapped) == 0 {
                match GetLastError() {
                    // 相手が閉じた。読み取りでは EOF と同じ意味になる。
                    ERROR_BROKEN_PIPE => return Ok(0),
                    ERROR_IO_PENDING => {}
                    code => return Err(std::io::Error::from_raw_os_error(code as i32)),
                }
            }

            let mut transferred: u32 = 0;
            if GetOverlappedResultEx(
                self.pipe.0,
                &overlapped,
                &mut transferred,
                millis(timeout),
                0,
            ) == 0
            {
                return match GetLastError() {
                    ERROR_BROKEN_PIPE => Ok(0),
                    WAIT_TIMEOUT => Err(self.cancel(&overlapped)),
                    code => Err(std::io::Error::from_raw_os_error(code as i32)),
                };
            }
            Ok(transferred as usize)
        }
    }

    /// 期限切れになった操作を取り消し、**完了するまで待ってから**戻る。
    ///
    /// 待たずに戻ると、カーネルがまだ書き込みうる `OVERLAPPED` をスタック
    /// ごと捨てることになる。取り消し要求は非同期なので、要求しただけでは
    /// まだ操作は終わっていない。
    ///
    /// # Safety
    /// `overlapped` はこのハンドルへ発行済みの操作のもので、呼び出し元の
    /// スタックにまだ生きていること。
    unsafe fn cancel(&self, overlapped: &OVERLAPPED) -> std::io::Error {
        unsafe {
            // ハンドル全体ではなくこの操作だけを取り消す (だから CancelIo ではなく
            // CancelIoEx)。反対方向の操作を巻き添えにしない。
            CancelIoEx(self.pipe.0, overlapped);
            let mut discarded: u32 = 0;
            GetOverlappedResult(self.pipe.0, overlapped, &mut discarded, 1);
        }
        std::io::Error::new(
            std::io::ErrorKind::TimedOut,
            "名前付きパイプの読み取りが期限内に終わりませんでした",
        )
    }
}

impl Read for Stream {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let ptr = buf.as_mut_ptr();
        let len = clamp_len(buf.len());
        // 転送量は待ち合わせ側から取るので、ここでは受け取らない
        // (重畳 I/O では発行時点の値は当てにならない)。
        self.perform(self.read_timeout, |handle, overlapped| unsafe {
            ReadFile(handle, ptr, len, ptr::null_mut(), overlapped)
        })
    }
}

impl Write for Stream {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let ptr = buf.as_ptr();
        let len = clamp_len(buf.len());
        // 書き込みに上限は設けない。相手が読み進めていれば必ず捌けるし、
        // 途中で打ち切るとフレームが半端に届いて経路全体が壊れる。
        self.perform(None, |handle, overlapped| unsafe {
            WriteFile(handle, ptr, len, ptr::null_mut(), overlapped)
        })
    }

    /// パイプは書き込みごとに相手へ渡るので、溜め込んでいるものは無い。
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// 1 回の I/O で扱う長さ。Win32 は 32 ビットしか受け取れない。
///
/// 上限で切り詰めても `Read`/`Write` の契約 (「一部だけ処理してよい」) に
/// 収まる。呼び出し側は返った長さで進める。
fn clamp_len(len: usize) -> u32 {
    len.min(u32::MAX as usize) as u32
}

/// 待ち合わせに渡すミリ秒。`None` は無制限。
///
/// `INFINITE` はちょうど `u32::MAX` なので、有限の待ち時間がそこへ丸め込まれて
/// 無制限に化けないよう 1 つ手前で止める。
fn millis(timeout: Option<Duration>) -> u32 {
    match timeout {
        None => INFINITE,
        Some(d) => d.as_millis().min(INFINITE as u128 - 1) as u32,
    }
}

fn create_event() -> std::io::Result<OwnedHandle> {
    // 手動リセットにして、操作のたびに明示的に落とす。自動リセットだと
    // 待たずに済んだ操作の後に signaled のまま残り、次の操作が完了したと
    // 誤認されうる。
    // SAFETY: 名前も継承もセキュリティ属性も要らないので全て省略する。
    let handle = unsafe { CreateEventW(ptr::null(), 1, 0, ptr::null()) };
    if handle.is_null() {
        return Err(std::io::Error::last_os_error());
    }
    Ok(OwnedHandle(handle))
}

/// Win32 の `W` 系 API へ渡す、終端 NUL 付きの UTF-16 列にする。
fn wide(value: &OsStr) -> Vec<u16> {
    value.encode_wide().chain(std::iter::once(0)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 待ち時間を指定しなければ無制限になる() {
        assert_eq!(millis(None), INFINITE);
    }

    #[test]
    fn 指定した待ち時間はミリ秒へ変換される() {
        assert_eq!(millis(Some(Duration::from_secs(2))), 2_000);
    }

    /// `INFINITE` と同じ値へ丸め込まれると、有限の指定が無制限に化ける。
    #[test]
    fn 長すぎる待ち時間でも無制限にはならない() {
        assert!(millis(Some(Duration::from_secs(u64::MAX / 1000))) < INFINITE);
    }
}
