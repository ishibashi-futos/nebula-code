//! IPC サーバー。
//!
//! 1 接続 = 1 GUI ウィンドウ。接続ごとに読み取り・書き込み・イベント転送の
//! 3 タスクを持つ。要求の処理は個別タスクへ切り出すので、重い要求 (LSP の初期化など) が
//! 後続のキー入力を待たせることはない。
//!
//! 待ち受けの実体はプラットフォームで分かれる (Unix ドメインソケット / 名前付きパイプ)。
//! 分岐は `unix` / `windows` サブモジュールに閉じ込め、ここから下は
//! 「バイトストリームを受け付けて読み書きする」以上のことを知らない。

#[cfg_attr(windows, path = "ipc/windows.rs")]
#[cfg_attr(unix, path = "ipc/unix.rs")]
mod platform;

use crate::dispatch;
use crate::state::BackendState;
use nebula_protocol::{
    ClientMessage, Event, FrameDecoder, Request, RequestId, ServerMessage, encode_frame,
};
use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::{Mutex, broadcast, mpsc};
use tokio::task::AbortHandle;

/// 待ち受けを開始して接続を待つ。
///
/// 既に別のバックエンドが動いていた場合は何もせずに `Ok(())` で戻る。
/// これは失敗ではなく「先客が居たので譲った」という正常な結末。
pub async fn serve(endpoint: &Path, state: Arc<BackendState>) -> std::io::Result<()> {
    let Some(mut listener) = platform::acquire(endpoint).await? else {
        eprintln!(
            "nebula-backend: {} で既に別のバックエンドが動作しているため終了します",
            endpoint.display()
        );
        return Ok(());
    };
    eprintln!("nebula-backend: {} で待機中", endpoint.display());

    let (shutdown_tx, mut shutdown_rx) = mpsc::channel::<()>(1);
    loop {
        tokio::select! {
            accepted = listener.accept() => {
                let stream = accepted?;
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
    platform::cleanup(endpoint);
    Ok(())
}

/// 1 接続ぶんの処理。
async fn handle_connection(
    stream: platform::Stream,
    state: Arc<BackendState>,
    shutdown: mpsc::Sender<()>,
) -> std::io::Result<()> {
    let (mut reader, mut writer) = platform::split(stream);
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
    let inflight: Arc<Mutex<HashMap<RequestId, AbortHandle>>> =
        Arc::new(Mutex::new(HashMap::new()));

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
                // 停止を待ちたい側は Ack ではなくプロセスそのものの終了を見ること
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
