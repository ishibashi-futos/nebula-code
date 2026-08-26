//! 名前付きパイプによる待ち受け。
//!
//! Unix 側がソケットファイルの `bind` に頼っている「1 プロセスだけが勝つ」性質を、
//! ここでは `first_pipe_instance` が担う。この指定を付けた `create` は、同名の
//! パイプインスタンスが既に 1 つでも存在すると `ERROR_ACCESS_DENIED` で失敗する。
//! 判定が 1 回のシステムコールで閉じている点も bind と同じで、勝者が待ち受けを
//! 開始する直前に敗者が横取りする隙が無い。
//!
//! Unix 側にある「残骸のソケットファイルを片付けて掴み直す」経路はここには無い。
//! パイプは最後のハンドルが閉じると名前ごと消えるため、掴めないなら相手は
//! 生きている、と言い切れる。

use std::ffi::{OsStr, OsString};
use std::path::Path;
use tokio::net::windows::named_pipe::{NamedPipeServer, PipeMode, ServerOptions};

pub type Stream = NamedPipeServer;
pub type ReadHalf = tokio::io::ReadHalf<NamedPipeServer>;
pub type WriteHalf = tokio::io::WriteHalf<NamedPipeServer>;

pub struct Listener {
    name: OsString,
    /// 次の接続を受けるために用意済みのインスタンス。
    ///
    /// 名前付きパイプは「待ち受け」と「1 本の接続」が同じオブジェクトなので、
    /// 接続が確定するたびに次のインスタンスを作り直す必要がある。
    next: NamedPipeServer,
}

/// 待ち受けを掴む。既に別のバックエンドが掴んでいれば `None`。
pub async fn acquire(endpoint: &Path) -> std::io::Result<Option<Listener>> {
    let name = endpoint.as_os_str().to_owned();
    match create(&name, true) {
        Ok(next) => Ok(Some(Listener { name, next })),
        // ERROR_ACCESS_DENIED。先客が first_pipe_instance を握っている。
        Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => Ok(None),
        Err(e) => Err(e),
    }
}

/// パイプインスタンスを 1 つ作る。
///
/// `first` は最初の 1 つにだけ立てる。2 つ目以降にも立てると、自分が作った
/// 1 つ目の存在によって `ERROR_ACCESS_DENIED` になってしまう。
fn create(name: &OsStr, first: bool) -> std::io::Result<NamedPipeServer> {
    ServerOptions::new()
        // フレーム境界は長さ接頭辞 (nebula-protocol の codec) が持つので、
        // パイプ側のメッセージ境界は要らない。Unix ソケットと同じ
        // 「ただのバイト列」として扱う。
        .pipe_mode(PipeMode::Byte)
        .first_pipe_instance(first)
        .create(name)
}

impl Listener {
    pub async fn accept(&mut self) -> std::io::Result<Stream> {
        self.next.connect().await?;
        // 今の接続を返す前に次のインスタンスを作る。後回しにすると、この接続を
        // 処理している間に来た 2 つ目の GUI が「パイプが無い」と見て起動を諦める。
        let next = create(&self.name, false)?;
        Ok(std::mem::replace(&mut self.next, next))
    }
}

pub fn split(stream: Stream) -> (ReadHalf, WriteHalf) {
    tokio::io::split(stream)
}

/// 待ち受けを畳んだ後の後始末。
///
/// 名前付きパイプはファイルシステム上に実体を残さず、最後のハンドルが閉じた
/// 時点で名前ごと消える。消すべきものが無いので何もしない。
pub fn cleanup(_endpoint: &Path) {}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn temp_endpoint(name: &str) -> PathBuf {
        PathBuf::from(format!(
            r"\\.\pipe\nebula-test-{name}-{}",
            std::process::id()
        ))
    }

    #[tokio::test]
    async fn 二つ目のバックエンドは起動を譲る() {
        let endpoint = temp_endpoint("dup");
        let first = acquire(&endpoint).await.unwrap();
        assert!(first.is_some(), "1 つ目はパイプを掴めるはず");
        let second = acquire(&endpoint).await.unwrap();
        assert!(second.is_none(), "2 つ目は譲るはず");
    }

    /// 掴んでいた側が終われば、同じ名前をもう一度掴み直せる。
    /// Unix 側の「残骸を片付けて掴み直す」に対応する性質を、パイプでは
    /// ハンドルの解放だけで得られることを確かめる。
    #[tokio::test]
    async fn 先客が居なくなれば掴み直せる() {
        let endpoint = temp_endpoint("reacquire");
        let first = acquire(&endpoint).await.unwrap();
        assert!(first.is_some());
        drop(first);

        let again = acquire(&endpoint).await.unwrap();
        assert!(again.is_some(), "先客が消えた後は掴めるはず");
    }

    /// 同時起動で 1 つだけが勝つことを、実際に並行させて確かめる。
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
    }
}
