//! GUI プロセス側の IPC クライアント。
//!
//! 読み書きは専用の OS スレッドで行う。gpui の executor 上で
//! ブロッキング I/O を回すと描画タスクの実行枠を奪うため、描画とは物理的に分ける。
//! スレッドと GUI の間は smol のチャネルでつなぐので、ビュー側からは
//! `client.request(...).await` と書くだけで済む。
//!
//! 接続そのもの (Unix ドメインソケット / 名前付きパイプ) は
//! `nebula_protocol::transport` が両プラットフォームぶん面倒を見る。
//! ここの `platform` サブモジュールに残るのは、GUI にしか要らない
//! 「古いバックエンドをどう終わらせ、どう終了を見届けるか」だけ。

#[cfg_attr(windows, path = "ipc_client/windows.rs")]
#[cfg_attr(unix, path = "ipc_client/unix.rs")]
mod platform;

use nebula_protocol::{
    ClientMessage, Event, ExecutableIdentity, FrameDecoder, PROTOCOL_VERSION, ProtocolError,
    Request, RequestId, Response, ServerMessage, encode_frame,
};
use smol::channel::{Receiver, Sender, bounded, unbounded};
use std::collections::HashMap;
use nebula_protocol::transport::{Stream, connect as connect_endpoint};
use std::io::{Read, Write};
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
        let endpoint = nebula_protocol::default_endpoint();
        let stream = connect_or_spawn(&endpoint)?;
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
        replace_backend(&client, handshake.pid, &endpoint);

        let stream = connect_or_spawn(&endpoint)?;
        Self::from_stream(stream)
    }

    fn from_stream(stream: Stream) -> Result<Self, ProtocolError> {
        let read_half = stream
            .try_clone()
            .map_err(|e| ProtocolError::io(format!("接続を複製できません: {e}")))?;
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

fn spawn_writer(mut stream: Stream, outgoing: Receiver<ClientMessage>) {
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
    mut stream: Stream,
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
fn connect_or_spawn(endpoint: &Path) -> Result<Stream, ProtocolError> {
    if let Ok(stream) = connect_endpoint(endpoint) {
        return Ok(stream);
    }
    let binary = backend_binary_path();
    let child = std::process::Command::new(&binary)
        .arg("--endpoint")
        .arg(endpoint)
        .stdin(std::process::Stdio::null())
        .spawn()
        .map_err(|e| {
            ProtocolError::io(format!(
                "バックエンド ({}) を起動できません: {e}",
                binary.display()
            ))
        })?;
    spawn_reaper(child);

    // 起動待ち。合計で約 5 秒。ここでは失敗の種別を見ない。「まだ待ち受けが
    // 無い」も「一瞬だけ埋まっていた」も、待って繰り返せば解ける点で同じ。
    let mut delay = std::time::Duration::from_millis(2);
    for _ in 0..24 {
        std::thread::sleep(delay);
        if let Ok(stream) = connect_endpoint(endpoint) {
            return Ok(stream);
        }
        delay = (delay * 2).min(std::time::Duration::from_millis(400));
    }
    Err(ProtocolError::io(format!(
        "バックエンドに接続できませんでした: {}",
        endpoint.display()
    )))
}

/// バックエンド実行ファイルの位置。GUI と同じディレクトリに置く前提。
fn backend_binary_path() -> PathBuf {
    let name = format!("nebula-backend{}", std::env::consts::EXE_SUFFIX);
    std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.join(&name)))
        .unwrap_or_else(|| PathBuf::from(name))
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
/// 待ち受けを畳む) を優先する。それでも一定時間で終わらない場合に限り、
/// 最終手段として強制終了する。
fn replace_backend(client: &BackendClient, pid: u32, endpoint: &Path) {
    // `request()` で応答を待つと、応答を返せないくらい壊れている相手には
    // この呼び出し自体が無期限にハングしてしまい、そのために用意した
    // 「一定時間で見切って強制終了」という最終手段へ辿り着けなくなる。
    // 送るだけ送って応答は待たず、後続の待ち合わせとタイムアウトに判断を委ねる。
    let id = RequestId::next();
    let _ = client.outgoing.try_send(ClientMessage::Request {
        id,
        request: Request::Shutdown,
    });

    if platform::wait_for_backend_gone(endpoint, pid, std::time::Duration::from_secs(2)) {
        return;
    }

    eprintln!("nebula: Shutdown に応答しないため強制終了させます (pid {pid})");
    platform::terminate(pid);
    // 強制終了ではバックエンド側の後始末が走らないため、Unix ではソケット
    // ファイルが残ることが多い。その残骸は次の `connect_or_spawn` が起動する
    // 新しいバックエンドの `acquire` 側で「応答しない残骸」として片付けられる。
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
