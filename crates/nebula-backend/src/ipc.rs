//! Unix ドメインソケット上の IPC サーバー。
//!
//! 1 接続 = 1 GUI ウィンドウ。接続ごとに読み取り・書き込み・イベント転送の
//! 3 タスクを持つ。要求の処理は個別タスクへ切り出すので、重い要求 (LSP の初期化など) が
//! 後続のキー入力を待たせることはない。

use crate::dispatch;
use crate::state::BackendState;
use nebula_protocol::{
    ClientMessage, Event, FrameDecoder, ProtocolError, RequestId, Request, ServerMessage,
    encode_frame,
};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{Mutex, broadcast, mpsc};
use tokio::task::AbortHandle;

/// バックエンドが 1 つだけ動くことを保証しつつ、ソケットを掴む。
///
/// 排他は `UnixListener::bind` そのものに任せる。bind はソケットファイルの作成と
/// 待ち受け開始を 1 回のシステムコールで行うので、複数プロセスが同時に呼んでも
/// 成功するのは 1 つだけ。
///
/// 別途ロックファイルを置く方式は使わない。「ロックがあるが接続できない = 残骸」
/// という判定が、勝者が bind する直前の一瞬にも成立してしまい、敗者がロックを
/// 奪って二重起動する。実際に GUI を 3 つ同時起動して再現した。
///
/// 異常終了でソケットファイルだけが残った場合は、接続できないことを 2 回
/// 確かめてから片付ける。1 回で判断しないのは、bind と listen の間の
/// ごく短い時間に接続が拒否されうるため。
async fn acquire_listener(socket_path: &Path) -> std::io::Result<Option<UnixListener>> {
    for _ in 0..3 {
        match UnixListener::bind(socket_path) {
            Ok(listener) => return Ok(Some(listener)),
            // 既にファイルがある場合のエラー種別は OS で違う。
            // macOS は EEXIST (AlreadyExists)、Linux は EADDRINUSE (AddrInUse)。
            Err(e)
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::AddrInUse | std::io::ErrorKind::AlreadyExists
                ) =>
            {
                if UnixStream::connect(socket_path).await.is_ok() {
                    return Ok(None);
                }
                // 待ち受け開始の直前かもしれないので、間を置いてもう一度だけ確かめる。
                tokio::time::sleep(std::time::Duration::from_millis(80)).await;
                if UnixStream::connect(socket_path).await.is_ok() {
                    return Ok(None);
                }
                std::fs::remove_file(socket_path)?;
            }
            Err(e) => return Err(e),
        }
    }
    Ok(None)
}

/// ソケットを作って接続を待ち受ける。
///
/// 既に別のバックエンドが動いていた場合は何もせずに `Ok(())` で戻る。
/// これは失敗ではなく「先客が居たので譲った」という正常な結末。
pub async fn serve(socket_path: &Path, state: Arc<BackendState>) -> std::io::Result<()> {
    let Some(listener) = acquire_listener(socket_path).await? else {
        eprintln!(
            "nebula-backend: {} で既に別のバックエンドが動作しているため終了します",
            socket_path.display()
        );
        return Ok(());
    };
    eprintln!("nebula-backend: {} で待機中", socket_path.display());

    let (shutdown_tx, mut shutdown_rx) = mpsc::channel::<()>(1);
    loop {
        tokio::select! {
            accepted = listener.accept() => {
                let (stream, _) = accepted?;
                let state = state.clone();
                let shutdown = shutdown_tx.clone();
                tokio::spawn(async move {
                    if let Err(e) = handle_connection(stream, state, shutdown).await {
                        eprintln!("nebula-backend: 接続処理でエラー: {e}");
                    }
                });
            }
            _ = shutdown_rx.recv() => {
                eprintln!("nebula-backend: 終了要求を受け取りました");
                break;
            }
        }
    }
    state.shutdown().await;
    let _ = std::fs::remove_file(socket_path);
    Ok(())
}

/// 1 接続ぶんの処理。
async fn handle_connection(
    stream: UnixStream,
    state: Arc<BackendState>,
    shutdown: mpsc::Sender<()>,
) -> std::io::Result<()> {
    let (mut reader, mut writer) = stream.into_split();
    // 応答とイベントを 1 本の書き込みタスクに集約する。フレームが混ざらないようにするため。
    let (out_tx, mut out_rx) = mpsc::unbounded_channel::<ServerMessage>();

    let writer_task = tokio::spawn(async move {
        while let Some(message) = out_rx.recv().await {
            let Ok(frame) = encode_frame(&message) else {
                continue;
            };
            if writer.write_all(&frame).await.is_err() {
                break;
            }
        }
    });

    // バックエンド全体のイベントをこの接続へ転送する。
    let event_task = tokio::spawn(forward_events(state.events.subscribe(), out_tx.clone()));

    // 進行中の要求。Cancel 要求で中断できるようにハンドルを持つ。
    let inflight: Arc<Mutex<HashMap<RequestId, AbortHandle>>> = Arc::new(Mutex::new(HashMap::new()));

    let mut decoder = FrameDecoder::new();
    let mut chunk = vec![0u8; 64 * 1024];
    let result = loop {
        let read = match reader.read(&mut chunk).await {
            Ok(0) => break Ok(()),
            Ok(n) => n,
            Err(e) => break Err(e),
        };
        decoder.feed(&chunk[..read]);
        loop {
            match decoder.next_message::<ClientMessage>() {
                Ok(Some(message)) => {
                    handle_client_message(message, &state, &out_tx, &inflight, &shutdown).await;
                }
                Ok(None) => break,
                Err(e) => {
                    eprintln!("nebula-backend: フレーム復号に失敗: {e}");
                    break;
                }
            }
        }
    };

    for handle in inflight.lock().await.values() {
        handle.abort();
    }
    drop(out_tx);
    event_task.abort();
    let _ = writer_task.await;
    result
}

async fn forward_events(
    mut events: broadcast::Receiver<Event>,
    out: mpsc::UnboundedSender<ServerMessage>,
) {
    loop {
        match events.recv().await {
            Ok(event) => {
                if out.send(ServerMessage::Event(event)).is_err() {
                    break;
                }
            }
            // 購読が追いつかず取りこぼした場合。GUI 側で整合を取り直せるよう通知する。
            Err(broadcast::error::RecvError::Lagged(skipped)) => {
                let _ = out.send(ServerMessage::Event(Event::Notification {
                    level: nebula_protocol::NotificationLevel::Warning,
                    message: format!("イベントを {skipped} 件取りこぼしました"),
                }));
            }
            Err(broadcast::error::RecvError::Closed) => break,
        }
    }
}

async fn handle_client_message(
    message: ClientMessage,
    state: &Arc<BackendState>,
    out: &mpsc::UnboundedSender<ServerMessage>,
    inflight: &Arc<Mutex<HashMap<RequestId, AbortHandle>>>,
    shutdown: &mpsc::Sender<()>,
) {
    match message {
        ClientMessage::Cancel { id } => {
            if let Some(handle) = inflight.lock().await.remove(&id) {
                handle.abort();
            }
        }
        ClientMessage::Request { id, request } => {
            if matches!(request, Request::Shutdown) {
                // この Ack は best-effort。送信待ち行列へ載せた直後に停止へ入るので、
                // 実際に書き出される前に接続が閉じることがある。
                // 停止を待ちたい側は Ack ではなくソケットファイルが消えるのを見ること
                // (GUI 側 `replace_backend` がそうしている)。
                let _ = out.send(ServerMessage::Response {
                    id,
                    result: Ok(nebula_protocol::Response::Ack),
                });
                let _ = shutdown.send(()).await;
                return;
            }
            let state = state.clone();
            let out = out.clone();
            let inflight_for_cleanup = inflight.clone();
            let task = tokio::spawn(async move {
                let result = dispatch::handle(&state, request).await;
                let _ = out.send(ServerMessage::Response { id, result });
                inflight_for_cleanup.lock().await.remove(&id);
            });
            inflight.lock().await.insert(id, task.abort_handle());
        }
    }
}

/// バックエンドを起動して接続可能になるまで待つ。GUI から呼ぶ。
///
/// 既に起動済みならそのまま接続する。冷間起動では GUI がウィンドウを出した後に
/// 非同期で呼ばれるため、ここでの待ち時間は初回フレームには影響しない。
pub async fn connect_or_spawn(
    socket_path: &Path,
    backend_binary: &Path,
) -> Result<UnixStream, ProtocolError> {
    if let Ok(stream) = UnixStream::connect(socket_path).await {
        return Ok(stream);
    }
    tokio::process::Command::new(backend_binary)
        .arg("--socket")
        .arg(socket_path)
        .stdin(std::process::Stdio::null())
        .spawn()
        .map_err(|e| ProtocolError::io(format!("バックエンドを起動できません: {e}")))?;

    // 起動を待つ。指数的に間隔を伸ばし、合計で約 5 秒待つ。
    let mut delay = std::time::Duration::from_millis(2);
    for _ in 0..24 {
        tokio::time::sleep(delay).await;
        if let Ok(stream) = UnixStream::connect(socket_path).await {
            return Ok(stream);
        }
        delay = (delay * 2).min(std::time::Duration::from_millis(500));
    }
    Err(ProtocolError::io(format!(
        "バックエンドに接続できませんでした: {}",
        socket_path.display()
    )))
}

/// バックエンド実行ファイルの位置を推定する。
///
/// GUI と同じディレクトリに置かれている前提。開発中の `target/debug` でも
/// 配布物の `Nebula.app/Contents/MacOS` でも同じ規則で解決できる。
pub fn backend_binary_path() -> PathBuf {
    std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.join("nebula-backend")))
        .unwrap_or_else(|| PathBuf::from("nebula-backend"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_socket(name: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "nebula-test-{name}-{}.sock",
            std::process::id()
        ));
        let _ = std::fs::remove_file(&path);
        path
    }

    #[tokio::test]
    async fn 二つ目のバックエンドは起動を譲る() {
        let socket = temp_socket("dup");
        let first = acquire_listener(&socket).await.unwrap();
        assert!(first.is_some(), "1 つ目はソケットを掴めるはず");
        let second = acquire_listener(&socket).await.unwrap();
        assert!(second.is_none(), "2 つ目は譲るはず");

        drop(first);
        let _ = std::fs::remove_file(&socket);
    }

    #[tokio::test]
    async fn 応答しない残骸は片付けて掴み直す() {
        let socket = temp_socket("stale");
        // 待ち受けていないのにソケット位置にファイルだけがある状態。
        std::fs::write(&socket, "").unwrap();

        let listener = acquire_listener(&socket).await.unwrap();
        assert!(listener.is_some(), "残骸は片付けて掴み直せるはず");

        drop(listener);
        let _ = std::fs::remove_file(&socket);
    }

    /// 同時起動で 1 つだけが勝つことを、実際に並行させて確かめる。
    ///
    /// 掴んだ待ち受けは最後まで保持する。途中で落とすとソケットファイルだけが残り、
    /// 後続が「残骸」とみなして掴み直してしまい、検査したい競合とは別の状況になる。
    #[tokio::test(flavor = "multi_thread", worker_threads = 8)]
    async fn 同時に起動しても勝者は一つだけ() {
        let socket = temp_socket("race");
        let mut handles = Vec::new();
        for _ in 0..8 {
            let socket = socket.clone();
            handles.push(tokio::spawn(async move {
                acquire_listener(&socket).await.unwrap()
            }));
        }
        let mut listeners = Vec::new();
        for handle in handles {
            if let Some(listener) = handle.await.unwrap() {
                listeners.push(listener);
            }
        }
        assert_eq!(listeners.len(), 1, "勝者が {} 個になった", listeners.len());
        drop(listeners);
        let _ = std::fs::remove_file(&socket);
    }
}
