//! Unix ドメインソケットによる待ち受け。
//!
//! バックエンドが 1 つだけ動くことの保証を `UnixListener::bind` そのものに任せる。
//! bind はソケットファイルの作成と待ち受け開始を 1 回のシステムコールで行うので、
//! 複数プロセスが同時に呼んでも成功するのは 1 つだけ。

use std::path::Path;
use tokio::net::{UnixListener, UnixStream};

pub type Stream = UnixStream;
pub type ReadHalf = tokio::net::unix::OwnedReadHalf;
pub type WriteHalf = tokio::net::unix::OwnedWriteHalf;

pub struct Listener(UnixListener);

/// 待ち受けを掴む。既に別のバックエンドが掴んでいれば `None`。
///
/// 別途ロックファイルを置く方式は使わない。「ロックがあるが接続できない = 残骸」
/// という判定が、勝者が bind する直前の一瞬にも成立してしまい、敗者がロックを
/// 奪って二重起動する。実際に GUI を 3 つ同時起動して再現した。
///
/// 異常終了でソケットファイルだけが残った場合は、接続できないことを 2 回
/// 確かめてから片付ける。1 回で判断しないのは、bind と listen の間の
/// ごく短い時間に接続が拒否されうるため。
pub async fn acquire(endpoint: &Path) -> std::io::Result<Option<Listener>> {
    for _ in 0..3 {
        match UnixListener::bind(endpoint) {
            Ok(listener) => return Ok(Some(Listener(listener))),
            // 既にファイルがある場合のエラー種別は OS で違う。
            // macOS は EEXIST (AlreadyExists)、Linux は EADDRINUSE (AddrInUse)。
            Err(e)
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::AddrInUse | std::io::ErrorKind::AlreadyExists
                ) =>
            {
                if UnixStream::connect(endpoint).await.is_ok() {
                    return Ok(None);
                }
                // 待ち受け開始の直前かもしれないので、間を置いてもう一度だけ確かめる。
                tokio::time::sleep(std::time::Duration::from_millis(80)).await;
                if UnixStream::connect(endpoint).await.is_ok() {
                    return Ok(None);
                }
                std::fs::remove_file(endpoint)?;
            }
            Err(e) => return Err(e),
        }
    }
    Ok(None)
}

impl Listener {
    pub async fn accept(&mut self) -> std::io::Result<Stream> {
        let (stream, _) = self.0.accept().await?;
        Ok(stream)
    }
}

pub fn split(stream: Stream) -> (ReadHalf, WriteHalf) {
    stream.into_split()
}

/// 待ち受けを畳んだ後の後始末。
///
/// ソケットファイルは待ち受けを閉じても残るので、明示的に消す。GUI 側は
/// このファイルが消えることを「バックエンドが終わり切った」の合図に使う。
pub fn cleanup(endpoint: &Path) {
    let _ = std::fs::remove_file(endpoint);
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn temp_endpoint(name: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "nebula-test-{name}-{}.sock",
            std::process::id()
        ));
        let _ = std::fs::remove_file(&path);
        path
    }

    #[tokio::test]
    async fn 二つ目のバックエンドは起動を譲る() {
        let endpoint = temp_endpoint("dup");
        let first = acquire(&endpoint).await.unwrap();
        assert!(first.is_some(), "1 つ目はソケットを掴めるはず");
        let second = acquire(&endpoint).await.unwrap();
        assert!(second.is_none(), "2 つ目は譲るはず");

        drop(first);
        cleanup(&endpoint);
    }

    #[tokio::test]
    async fn 応答しない残骸は片付けて掴み直す() {
        let endpoint = temp_endpoint("stale");
        // 待ち受けていないのにソケット位置にファイルだけがある状態。
        std::fs::write(&endpoint, "").unwrap();

        let listener = acquire(&endpoint).await.unwrap();
        assert!(listener.is_some(), "残骸は片付けて掴み直せるはず");

        drop(listener);
        cleanup(&endpoint);
    }

    /// 同時起動で 1 つだけが勝つことを、実際に並行させて確かめる。
    ///
    /// 掴んだ待ち受けは最後まで保持する。途中で落とすとソケットファイルだけが残り、
    /// 後続が「残骸」とみなして掴み直してしまい、検査したい競合とは別の状況になる。
    #[tokio::test(flavor = "multi_thread", worker_threads = 8)]
    async fn 同時に起動しても勝者は一つだけ() {
        let endpoint = temp_endpoint("race");
        let mut handles = Vec::new();
        for _ in 0..8 {
            let endpoint = endpoint.clone();
            handles.push(tokio::spawn(
                async move { acquire(&endpoint).await.unwrap() },
            ));
        }
        let mut listeners = Vec::new();
        for handle in handles {
            if let Some(listener) = handle.await.unwrap() {
                listeners.push(listener);
            }
        }
        assert_eq!(listeners.len(), 1, "勝者が {} 個になった", listeners.len());
        drop(listeners);
        cleanup(&endpoint);
    }
}
