//! GUI プロセス側の IPC クライアント。
//!
//! ソケットの読み書きは専用の OS スレッドで行う。gpui の executor 上で
//! ブロッキング I/O を回すと描画タスクの実行枠を奪うため、描画とは物理的に分ける。
//! スレッドと GUI の間は smol のチャネルでつなぐので、ビュー側からは
//! `client.request(...).await` と書くだけで済む。

use nebula_protocol::{
    ClientMessage, Event, ExecutableIdentity, FrameDecoder, PROTOCOL_VERSION, ProtocolError,
    Request, RequestId, Response, ServerMessage, encode_frame,
};
use smol::channel::{Receiver, Sender, bounded, unbounded};
use std::collections::HashMap;
use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

type PendingMap = Arc<Mutex<HashMap<RequestId, Sender<Result<Response, ProtocolError>>>>>;

/// バックエンドへの接続。複製して各ビューに配れる。
#[derive(Clone)]
pub struct BackendClient {
    outgoing: Sender<ClientMessage>,
    pending: PendingMap,
    events: Receiver<Event>,
    /// 接続が切れたら真になる。GUI は再接続を促す表示に切り替える。
    disconnected: Arc<Mutex<bool>>,
}

impl BackendClient {
    /// バックエンドに接続する。未起動なら起動して待つ。
    ///
    /// ブロッキングする。呼び出し側はバックグラウンドスレッドから呼ぶこと。
    pub fn connect_blocking() -> Result<Self, ProtocolError> {
        let socket_path = nebula_protocol::default_socket_path();
        let stream = connect_or_spawn(&socket_path)?;
        let client = Self::from_stream(stream)?;

        // 接続できただけでは「正しいバックエンドか」は分からない。相乗り先が
        // 古いビルドの生き残りだと、直したはずのバグが再発することになる
        // (「古いバックエンドプロセスが生き残っていると、新しい GUI がそちらに
        // 相乗りして古いコードで動き続ける」)。ハンドシェイクで報告される
        // 実行ファイルの同一性を、自分がこれから起動するはずのものと突き合わせる。
        let handshake = smol::block_on(client.handshake())?;
        let expected = ExecutableIdentity::from_path(&backend_binary_path()).map_err(|e| {
            ProtocolError::io(format!("自分の実行ファイルの情報を読めません: {e}"))
        })?;
        if is_same_executable(&expected, &handshake.executable) {
            return Ok(client);
        }

        eprintln!(
            "nebula: 実行ファイルが一致しない古いバックエンド (pid {}) を終了させて起動し直します",
            handshake.pid
        );
        replace_backend(&client, handshake.pid, &socket_path);

        let stream = connect_or_spawn(&socket_path)?;
        Self::from_stream(stream)
    }

    fn from_stream(stream: UnixStream) -> Result<Self, ProtocolError> {
        let read_half = stream
            .try_clone()
            .map_err(|e| ProtocolError::io(format!("ソケットを複製できません: {e}")))?;
        let (outgoing_tx, outgoing_rx) = unbounded::<ClientMessage>();
        let (events_tx, events_rx) = unbounded::<Event>();
        let pending: PendingMap = Arc::new(Mutex::new(HashMap::new()));
        let disconnected = Arc::new(Mutex::new(false));

        spawn_writer(stream, outgoing_rx);
        spawn_reader(read_half, pending.clone(), events_tx, disconnected.clone());

        let client = Self {
            outgoing: outgoing_tx,
            pending,
            events: events_rx,
            disconnected,
        };
        Ok(client)
    }

    /// 版数の握手。接続直後に必ず呼ぶ。
    pub async fn handshake(&self) -> Result<nebula_protocol::HandshakeInfo, ProtocolError> {
        match self
            .request(Request::Handshake {
                protocol_version: PROTOCOL_VERSION,
            })
            .await?
        {
            Response::Handshake(info) => Ok(info),
            other => Err(ProtocolError::internal(format!(
                "握手への応答が不正です: {other:?}"
            ))),
        }
    }

    /// 要求を送り、応答を待つ。
    pub async fn request(&self, request: Request) -> Result<Response, ProtocolError> {
        let id = RequestId::next();
        let (tx, rx) = bounded(1);
        self.pending
            .lock()
            .expect("保留中要求のロック")
            .insert(id, tx);

        if self
            .outgoing
            .send(ClientMessage::Request { id, request })
            .await
            .is_err()
        {
            self.pending.lock().expect("保留中要求のロック").remove(&id);
            return Err(ProtocolError::io("バックエンドとの接続が切れています"));
        }
        rx.recv()
            .await
            .unwrap_or_else(|_| Err(ProtocolError::io("応答を受け取れませんでした")))
    }

    /// 応答を必要としない取り消し要求。
    pub fn cancel(&self, id: RequestId) {
        let _ = self.outgoing.try_send(ClientMessage::Cancel { id });
    }

    /// バックエンドからのイベント列。
    pub fn events(&self) -> Receiver<Event> {
        self.events.clone()
    }

    pub fn is_disconnected(&self) -> bool {
        *self.disconnected.lock().expect("接続状態のロック")
    }
}

fn spawn_writer(mut stream: UnixStream, outgoing: Receiver<ClientMessage>) {
    std::thread::Builder::new()
        .name("nebula-ipc-writer".into())
        .spawn(move || {
            while let Ok(message) = smol::block_on(outgoing.recv()) {
                let Ok(frame) = encode_frame(&message) else {
                    continue;
                };
                if stream.write_all(&frame).is_err() {
                    break;
                }
            }
        })
        .expect("IPC 書き込みスレッドを起動できません");
}

fn spawn_reader(
    mut stream: UnixStream,
    pending: PendingMap,
    events: Sender<Event>,
    disconnected: Arc<Mutex<bool>>,
) {
    std::thread::Builder::new()
        .name("nebula-ipc-reader".into())
        .spawn(move || {
            let mut decoder = FrameDecoder::new();
            let mut chunk = vec![0u8; 64 * 1024];
            loop {
                let read = match stream.read(&mut chunk) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => n,
                };
                decoder.feed(&chunk[..read]);
                loop {
                    match decoder.next_message::<ServerMessage>() {
                        Ok(Some(ServerMessage::Response { id, result })) => {
                            if let Some(tx) =
                                pending.lock().expect("保留中要求のロック").remove(&id)
                            {
                                let _ = tx.try_send(result);
                            }
                        }
                        Ok(Some(ServerMessage::Event(event))) => {
                            if events.try_send(event).is_err() {
                                break;
                            }
                        }
                        Ok(None) => break,
                        Err(e) => {
                            eprintln!("nebula: フレーム復号に失敗: {e}");
                            break;
                        }
                    }
                }
            }
            *disconnected.lock().expect("接続状態のロック") = true;
            // 待たせたままの要求を全て失敗させる。
            for (_, tx) in pending.lock().expect("保留中要求のロック").drain() {
                let _ = tx.try_send(Err(ProtocolError::io("バックエンドが切断しました")));
            }
        })
        .expect("IPC 読み取りスレッドを起動できません");
}

/// 既存のバックエンドに繋ぐ。無ければ起動して待つ。
fn connect_or_spawn(socket_path: &Path) -> Result<UnixStream, ProtocolError> {
    if let Ok(stream) = UnixStream::connect(socket_path) {
        return Ok(stream);
    }
    let binary = backend_binary_path();
    let child = std::process::Command::new(&binary)
        .arg("--socket")
        .arg(socket_path)
        .stdin(std::process::Stdio::null())
        .spawn()
        .map_err(|e| {
            ProtocolError::io(format!(
                "バックエンド ({}) を起動できません: {e}",
                binary.display()
            ))
        })?;
    spawn_reaper(child);

    // 起動待ち。合計で約 5 秒。
    let mut delay = std::time::Duration::from_millis(2);
    for _ in 0..24 {
        std::thread::sleep(delay);
        if let Ok(stream) = UnixStream::connect(socket_path) {
            return Ok(stream);
        }
        delay = (delay * 2).min(std::time::Duration::from_millis(400));
    }
    Err(ProtocolError::io(format!(
        "バックエンドに接続できませんでした: {}",
        socket_path.display()
    )))
}

/// バックエンド実行ファイルの位置。GUI と同じディレクトリに置く前提。
fn backend_binary_path() -> PathBuf {
    std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.join("nebula-backend")))
        .unwrap_or_else(|| PathBuf::from("nebula-backend"))
}

/// 起動した子プロセス (バックエンド) を回収する。
///
/// `Child` を保持せず drop すると、終了時に GUI プロセスの下へ zombie として
/// 残ってしまう (`Child` の drop は kill も wait もしない、というドキュメント
/// どおりの挙動)。`wait()` は同期的な `waitpid` を呼ぶので、GUI プロセス自身の
/// SIGCHLD マスク・ハンドラの状態に関係なく回収できる。
fn spawn_reaper(mut child: std::process::Child) {
    std::thread::Builder::new()
        .name("nebula-backend-reaper".into())
        .spawn(move || {
            let _ = child.wait();
        })
        .expect("バックエンド回収スレッドを起動できません");
}

/// 期待する実行ファイルと、バックエンドが報告した実行ファイルが同じビルドかを
/// 判定する純粋関数。
///
/// 「新しい方が勝つ」のような判断はしない。厳密な一致/不一致だけを見る。cargo は
/// 実際に変更があったときだけ実行ファイルを書き直すので、(path, mtime, size) が
/// 一致していればそのまま使ってよい、が正しい信号になる。
fn is_same_executable(expected: &ExecutableIdentity, actual: &ExecutableIdentity) -> bool {
    expected.path == actual.path && expected.mtime == actual.mtime && expected.size == actual.size
}

/// 実行ファイルが一致しない古いバックエンドを終わらせる。
///
/// `Request::Shutdown` の正常経路 (バックエンド側で各サービスの後始末をしてから
/// ソケットファイルを消す) を優先する。それでも一定時間でソケットファイルが
/// 消えない場合に限り、最終手段として SIGTERM を送る。
fn replace_backend(client: &BackendClient, pid: u32, socket_path: &Path) {
    // `request()` で応答を待つと、応答を返せないくらい壊れている相手には
    // この呼び出し自体が無期限にハングしてしまい、そのために用意した
    // 「一定時間で見切って SIGTERM」という最終手段へ辿り着けなくなる。
    // 送るだけ送って応答は待たず、後続のポーリングとタイムアウトに判断を委ねる。
    let id = RequestId::next();
    let _ = client.outgoing.try_send(ClientMessage::Request {
        id,
        request: Request::Shutdown,
    });

    if wait_for_socket_gone(socket_path, std::time::Duration::from_secs(2)) {
        return;
    }

    eprintln!("nebula: Shutdown に応答しないため SIGTERM で終了させます (pid {pid})");
    // SAFETY: pid はハンドシェイクで得た実在のプロセス ID。SIGTERM は対象プロセスに
    // 既定の終了処理を促すだけで、こちらのメモリには一切触れない。
    unsafe {
        libc::kill(pid as libc::pid_t, libc::SIGTERM);
    }
    // ソケットファイルが残っていても構わない。SIGTERM は既定の動作 (即終了) を
    // 起こすだけでバックエンド側の後始末は走らないため、ファイルは残ることが多い。
    // その残骸は次の `connect_or_spawn` が起動する新しいバックエンドの
    // `acquire_listener` 側で「応答しない残骸」として片付けられる。
}

/// ソケットファイルが消えるまで短い間隔でポーリングする。
///
/// バックエンドは `Request::Shutdown` を受けて後始末を終えると、待ち受けていた
/// ソケットファイルを削除する。それを「終了し切った」の合図として使う。
fn wait_for_socket_gone(socket_path: &Path, timeout: std::time::Duration) -> bool {
    let deadline = std::time::Instant::now() + timeout;
    while std::time::Instant::now() < deadline {
        if !socket_path.exists() {
            return true;
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    !socket_path.exists()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn identity(path: &str, mtime_secs: u64, size: u64) -> ExecutableIdentity {
        ExecutableIdentity {
            path: PathBuf::from(path),
            mtime: std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(mtime_secs),
            size,
        }
    }

    #[test]
    fn パスと更新時刻とサイズが一致すれば同じ実行ファイルとみなす() {
        let a = identity("/opt/nebula/nebula-backend", 1_000, 4096);
        let b = identity("/opt/nebula/nebula-backend", 1_000, 4096);
        assert!(is_same_executable(&a, &b));
    }

    /// 実機で実際に踏んだケース: release ビルドで起動し続けているデーモンに、
    /// パスの違う debug ビルドの GUI がぶら下がる。
    #[test]
    fn パスが違えば別の実行ファイルとみなす() {
        let expected = identity("/opt/nebula/debug/nebula-backend", 1_000, 4096);
        let actual = identity("/opt/nebula/release/nebula-backend", 1_000, 4096);
        assert!(!is_same_executable(&expected, &actual));
    }

    #[test]
    fn 更新時刻が違えば別の実行ファイルとみなす() {
        let expected = identity("/opt/nebula/nebula-backend", 2_000, 4096);
        let actual = identity("/opt/nebula/nebula-backend", 1_000, 4096);
        assert!(!is_same_executable(&expected, &actual));
    }

    #[test]
    fn サイズが違えば別の実行ファイルとみなす() {
        let expected = identity("/opt/nebula/nebula-backend", 1_000, 4096);
        let actual = identity("/opt/nebula/nebula-backend", 1_000, 4097);
        assert!(!is_same_executable(&expected, &actual));
    }

    /// git のコミットハッシュ方式では検知できない、このリポジトリで実際に
    /// 起こりうる状況: HEAD は動かさずワーキングツリーだけ変えて再ビルドした
    /// 場合でも、mtime と size のどちらかは変わる (cargo は内容が変わらない限り
    /// 実行ファイルを書き直さないので、逆に両方一致していれば安全に使い回せる)。
    #[test]
    fn 一部だけ違っても別の実行ファイルとみなす() {
        let expected = identity("/opt/nebula/nebula-backend", 1_000, 4096);
        let path_only = identity("/opt/nebula/other/nebula-backend", 1_000, 4096);
        let mtime_only = identity("/opt/nebula/nebula-backend", 999, 4096);
        let size_only = identity("/opt/nebula/nebula-backend", 1_000, 1);
        assert!(!is_same_executable(&expected, &path_only));
        assert!(!is_same_executable(&expected, &mtime_only));
        assert!(!is_same_executable(&expected, &size_only));
    }
}
