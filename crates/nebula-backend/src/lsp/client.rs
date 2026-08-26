//! 言語サーバーとの JSON-RPC 通信 (stdio)。
//!
//! 応答の待ち合わせ (要求 ID → oneshot) だけをここで面倒を見る。
//! LSP の意味づけ (initialize や各要求の型) は上位の [`crate::lsp`] が持ち、
//! この層は「JSON を送って JSON を待つ」ことに専念する。

use super::framing;
use nebula_protocol::ProtocolError;
use serde_json::{Value, json};
use std::collections::HashMap;
use std::path::Path;
use std::process::Stdio;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::process::{Child, ChildStdout, Command};
use tokio::sync::{mpsc, oneshot};

/// サーバー発の要求または通知。
pub struct Incoming {
    pub method: String,
    /// 要求なら応答に使う ID。通知なら `None`。
    pub id: Option<Value>,
    pub params: Value,
}

type Pending = Arc<Mutex<HashMap<i64, oneshot::Sender<Result<Value, ProtocolError>>>>>;

pub struct Client {
    outgoing: mpsc::UnboundedSender<Vec<u8>>,
    pending: Pending,
    next_id: AtomicI64,
    /// 停止のために保持する。`kill_on_drop` を付けてあるので取りこぼしても孤児にはならない。
    child: Mutex<Option<Child>>,
}

impl Client {
    /// 言語サーバーを起動する。
    ///
    /// 実行ファイルが無い場合は [`ProtocolErrorKind::NotFound`] を返す。
    /// 呼び出し側はこれを「未インストール」として扱い、エラーで止めない。
    ///
    /// [`ProtocolErrorKind::NotFound`]: nebula_protocol::ProtocolErrorKind::NotFound
    pub fn spawn(
        program: &str,
        args: &[&str],
        root: &Path,
    ) -> Result<(Self, mpsc::UnboundedReceiver<Incoming>), ProtocolError> {
        let mut child = Command::new(program)
            .args(args)
            .current_dir(root)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            // 標準エラーは捨てる。診断に必要な情報は window/logMessage で受け取る。
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .map_err(|e| match e.kind() {
                std::io::ErrorKind::NotFound => {
                    ProtocolError::not_found(format!("{program} が見つかりません"))
                }
                _ => ProtocolError::external(format!("{program} を起動できません: {e}")),
            })?;

        let stdin = child.stdin.take().expect("stdin を piped で開いた");
        let stdout = child.stdout.take().expect("stdout を piped で開いた");

        let (outgoing, mut rx) = mpsc::unbounded_channel::<Vec<u8>>();
        tokio::spawn(async move {
            let mut stdin = stdin;
            while let Some(frame) = rx.recv().await {
                if stdin.write_all(&frame).await.is_err() || stdin.flush().await.is_err() {
                    break;
                }
            }
        });

        let pending: Pending = Arc::new(Mutex::new(HashMap::new()));
        let (incoming_tx, incoming_rx) = mpsc::unbounded_channel();
        tokio::spawn(read_loop(stdout, pending.clone(), incoming_tx));

        Ok((
            Self {
                outgoing,
                pending,
                next_id: AtomicI64::new(1),
                child: Mutex::new(Some(child)),
            },
            incoming_rx,
        ))
    }

    /// 要求を送り、応答を待つ。
    ///
    /// `timeout` を必ず取るのは、応答が返らない要求で GUI の操作が固まらないようにするため。
    pub async fn request(
        &self,
        method: &str,
        params: Value,
        timeout: Duration,
    ) -> Result<Value, ProtocolError> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        self.pending().insert(id, tx);

        let frame = encode(&json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        }))?;
        if self.outgoing.send(frame).is_err() {
            self.pending().remove(&id);
            return Err(ProtocolError::external(
                "言語サーバーへの書き込み経路が閉じています",
            ));
        }

        match tokio::time::timeout(timeout, rx).await {
            Ok(Ok(result)) => result,
            // 送信側が落ちた = 読み取りループが終了した (サーバーが死んだ)。
            Ok(Err(_)) => Err(ProtocolError::external(
                "言語サーバーが応答を返さずに終了しました",
            )),
            Err(_) => {
                // 待機表から外さないと、応答が来ない要求のぶんだけ表が膨らみ続ける。
                self.pending().remove(&id);
                // サーバー側の計算も止めさせる。重い要求ほど効く。
                self.notify("$/cancelRequest", json!({ "id": id }));
                Err(ProtocolError::external(format!(
                    "{method} が {} 秒以内に応答しませんでした",
                    timeout.as_secs()
                )))
            }
        }
    }

    /// 通知を送る (応答は返らない)。
    pub fn notify(&self, method: &str, params: Value) {
        let Ok(frame) = encode(&json!({
            "jsonrpc": "2.0",
            "method": method,
            "params": params,
        })) else {
            return;
        };
        let _ = self.outgoing.send(frame);
    }

    /// サーバー発の要求に応答する。
    ///
    /// 応答しないまま放置すると待ち続けて停止するサーバーがあるため、
    /// 内容を処理しない要求にも必ず成功応答を返す。
    pub fn respond(&self, id: Value, result: Value) {
        let Ok(frame) = encode(&json!({
            "jsonrpc": "2.0",
            "id": id,
            "result": result,
        })) else {
            return;
        };
        let _ = self.outgoing.send(frame);
    }

    /// プロセスを終了させる。
    pub fn kill(&self) {
        if let Some(mut child) = self.child.lock().expect("子プロセスのロック").take() {
            let _ = child.start_kill();
            // 終了状態を回収してゾンビを残さない。待ちはバックグラウンドで行う。
            tokio::spawn(async move {
                let _ = child.wait().await;
            });
        }
        fail_all(&self.pending, "言語サーバーを停止しました");
    }

    fn pending(&self) -> std::sync::MutexGuard<'_, HashMap<i64, oneshot::Sender<Result<Value, ProtocolError>>>> {
        self.pending.lock().expect("待機中要求のロック")
    }
}

fn encode(message: &Value) -> Result<Vec<u8>, ProtocolError> {
    serde_json::to_vec(message)
        .map(|body| framing::encode_frame(&body))
        .map_err(|e| ProtocolError::internal(format!("LSP メッセージを組み立てられません: {e}")))
}

/// 標準出力を読み続け、応答は待機中の要求へ、要求と通知は `incoming` へ流す。
async fn read_loop(
    mut stdout: ChildStdout,
    pending: Pending,
    incoming: mpsc::UnboundedSender<Incoming>,
) {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 16 * 1024];
    loop {
        loop {
            match framing::take_frame(&mut buf) {
                Ok(Some(body)) => deliver(&body, &pending, &incoming),
                Ok(None) => break,
                // ヘッダが壊れている時点でストリームの同期が失われている。
                // 読み進めても復帰できないので打ち切る。
                Err(_) => return fail_all(&pending, "言語サーバーの出力が壊れています"),
            }
        }
        match stdout.read(&mut chunk).await {
            Ok(0) | Err(_) => break,
            Ok(n) => buf.extend_from_slice(&chunk[..n]),
        }
    }
    // 出力が閉じた = サーバーが死んだ。待機中の要求をタイムアウトまで待たせない。
    fail_all(&pending, "言語サーバーが終了しました");
}

fn deliver(body: &[u8], pending: &Pending, incoming: &mpsc::UnboundedSender<Incoming>) {
    let Ok(value) = serde_json::from_slice::<Value>(body) else {
        return;
    };
    if let Some(method) = value.get("method").and_then(Value::as_str) {
        let _ = incoming.send(Incoming {
            method: method.to_string(),
            id: value.get("id").cloned(),
            params: value.get("params").cloned().unwrap_or(Value::Null),
        });
        return;
    }
    let Some(id) = value.get("id").and_then(Value::as_i64) else {
        return;
    };
    let Some(sender) = pending.lock().expect("待機中要求のロック").remove(&id) else {
        return;
    };
    let result = match value.get("error") {
        Some(error) => Err(ProtocolError::external(format!(
            "言語サーバーがエラーを返しました: {}",
            error
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or("詳細不明")
        ))),
        None => Ok(value.get("result").cloned().unwrap_or(Value::Null)),
    };
    let _ = sender.send(result);
}

fn fail_all(pending: &Pending, reason: &str) {
    let waiting = std::mem::take(&mut *pending.lock().expect("待機中要求のロック"));
    for (_, sender) in waiting {
        let _ = sender.send(Err(ProtocolError::external(reason.to_string())));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use nebula_protocol::ProtocolErrorKind;

    #[tokio::test]
    async fn 実行ファイルが無い場合は_notfound_を返す() {
        // 呼び出し側はこの分類だけを見て「未インストール」と判断する。
        let error = Client::spawn("nebula-definitely-not-a-real-lsp", &[], Path::new("/"))
            .err()
            .expect("起動は失敗するはず");
        assert_eq!(error.kind, ProtocolErrorKind::NotFound);
    }

    /// テストが起動するダミー言語サーバーの種類。OS ごとに実行ファイル名も文法も
    /// 違うため、分岐は `dummy_program` の 1 箇所へ集める。
    enum DummyKind {
        /// 標準入力を読み続けるだけで JSON-RPC の応答は返さない (`cat` 相当)。
        Silent,
        /// 何もせずすぐ成功して終わる (`true` 相当)。
        ExitImmediately,
    }

    fn dummy_program(kind: DummyKind) -> (&'static str, &'static [&'static str]) {
        match (kind, cfg!(windows)) {
            // `more` は標準入力から読み続けてページングするだけで、こちらから何も
            // 書き込まなければ何も応答を返さない。`cat` の代わりとして十分。
            (DummyKind::Silent, true) => ("cmd", &["/C", "more"]),
            (DummyKind::Silent, false) => ("cat", &[]),
            (DummyKind::ExitImmediately, true) => ("cmd", &["/C", "exit", "0"]),
            (DummyKind::ExitImmediately, false) => ("true", &[]),
        }
    }

    #[tokio::test]
    async fn 応答しない相手への要求はタイムアウトする() {
        // ダミーサーバーは送ったフレームをそのまま返す/何も返さないだけで、
        // JSON-RPC の応答は返さない。待ち続けないことを確かめる。
        let (program, args) = dummy_program(DummyKind::Silent);
        // `/` は Windows では起動先を特定できないおそれがあるため、OS を問わず
        // 必ず存在する一時ディレクトリを作業ディレクトリにする。
        let (client, _incoming) = Client::spawn(program, args, &std::env::temp_dir())
            .expect("ダミーサーバーの起動");
        let error = client
            .request("textDocument/hover", Value::Null, Duration::from_millis(200))
            .await
            .expect_err("応答は返らない");
        assert_eq!(error.kind, ProtocolErrorKind::ExternalTool);
        // 待機表に残したままだと要求のたびに漏れていく。
        assert!(client.pending().is_empty());
    }

    #[tokio::test]
    async fn 相手が終了すると待機中の要求は即座に失敗する() {
        // ダミーサーバーは何も出力せずすぐ終わる。標準出力が閉じた時点で失敗すべき。
        let (program, args) = dummy_program(DummyKind::ExitImmediately);
        let (client, _incoming) = Client::spawn(program, args, &std::env::temp_dir())
            .expect("ダミーサーバーの起動");
        let error = client
            // タイムアウトを長く取っても、待たされずに失敗する。
            .request("initialize", Value::Null, Duration::from_secs(30))
            .await
            .expect_err("応答は返らない");
        assert_eq!(error.kind, ProtocolErrorKind::ExternalTool);
    }
}
