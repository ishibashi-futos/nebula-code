//! GUI プロセス側の IPC クライアント。
//!
//! ソケットの読み書きは専用の OS スレッドで行う。gpui の executor 上で
//! ブロッキング I/O を回すと描画タスクの実行枠を奪うため、描画とは物理的に分ける。
//! スレッドと GUI の間は smol のチャネルでつなぐので、ビュー側からは
//! `client.request(...).await` と書くだけで済む。

use nebula_protocol::{
    ClientMessage, Event, FrameDecoder, PROTOCOL_VERSION, ProtocolError, Request, RequestId,
    Response, ServerMessage, encode_frame,
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
    std::process::Command::new(&binary)
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
