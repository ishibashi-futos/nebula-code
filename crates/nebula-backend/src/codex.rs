//! OpenAI Codex 連携。
//!
//! `codex app-server` を **プロセス 1 つだけ** 起動し、その上に複数の会話 (thread) を
//! 多重化する。会話ごとにプロセスを立てると、初期化 (MCP サーバーの起動を含む) が
//! 会話のたびに数秒かかるうえ、常駐メモリも会話数に比例してしまうため。
//!
//! app-server とのやり取りは 1 行 1 JSON の JSON-RPC。読み取りは専用のタスクが
//! 1 本だけ回し、応答は要求 ID で待ち合わせ、通知は `Event::Codex` へ翻訳して流す。
//! プロトコルは版で変わるので、翻訳できない通知は `CodexEvent::Raw` として素通しし、
//! 未知のイベントで会話が壊れないようにする。

mod protocol;
mod translate;

use nebula_protocol::{
    CodexApprovalDecision, CodexConversationId, CodexEvent, CodexSessionSpec, CodexTokenUsage,
    DetectedTools, Event, ProtocolError,
};
use protocol::Incoming;
use serde_json::{Value, json};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};
use tokio::sync::{Mutex as AsyncMutex, broadcast, oneshot};

/// 要求 1 件の待ち時間の上限。
///
/// app-server が生きたまま応答を返さなくなると dispatch が固まるため、必ず打ち切る。
/// 実測では会話開始 (MCP サーバーの起動を含む) でも 2 秒程度なので十分な余裕がある。
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

pub struct CodexService {
    tools: DetectedTools,
    /// app-server は最初に使われたときに起動する。二重起動を防ぐため非同期ロックで包む。
    client: AsyncMutex<Option<Arc<Client>>>,
    shared: Arc<Shared>,
}

impl CodexService {
    pub fn new(events: broadcast::Sender<Event>, tools: DetectedTools) -> Self {
        Self {
            tools,
            client: AsyncMutex::new(None),
            shared: Arc::new(Shared {
                events,
                state: Mutex::new(SharedState::default()),
            }),
        }
    }

    pub async fn new_conversation(
        &self,
        spec: CodexSessionSpec,
    ) -> Result<CodexConversationId, ProtocolError> {
        let client = self.ensure_client().await?;
        let result = client
            .request("thread/start", translate::thread_start_params(&spec))
            .await?;

        let thread = result
            .get("thread")
            .and_then(|t| t.get("id"))
            .and_then(Value::as_str)
            .ok_or_else(|| ProtocolError::external("thread/start が会話 ID を返しませんでした"))?
            .to_string();
        let model = result
            .get("model")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        let rollout_path = result
            .get("thread")
            .and_then(|t| t.get("path"))
            .and_then(Value::as_str)
            .map(PathBuf::from);

        let conversation = CodexConversationId::next();
        self.shared.state().register(conversation, thread);
        self.shared.emit(CodexEvent::SessionConfigured {
            conversation,
            model,
            rollout_path,
        });
        Ok(conversation)
    }

    pub async fn send_message(
        &self,
        conversation: CodexConversationId,
        text: &str,
        attachments: &[PathBuf],
    ) -> Result<(), ProtocolError> {
        let thread = self.shared.state().thread_of(conversation)?;
        let client = self.ensure_client().await?;

        let mut input = vec![json!({ "type": "text", "text": text })];
        input.extend(attachments.iter().map(|p| translate::attachment_input(p)));

        let result = client
            .request("turn/start", json!({ "threadId": thread, "input": input }))
            .await?;
        // 中断にはターン ID が要るので、開始応答で受け取った時点で控える。
        if let Some(turn) = result
            .get("turn")
            .and_then(|t| t.get("id"))
            .and_then(Value::as_str)
        {
            self.shared
                .state()
                .set_active_turn(conversation, Some(turn.to_string()));
        }
        Ok(())
    }

    pub async fn respond_approval(
        &self,
        conversation: CodexConversationId,
        request_id: &str,
        decision: CodexApprovalDecision,
    ) -> Result<(), ProtocolError> {
        let pending = self
            .shared
            .state()
            .take_approval(request_id, conversation)?;
        let client = self.ensure_client().await?;
        client
            .send(protocol::response_frame(
                &pending.id,
                translate::decision_result(decision),
            ))
            .await
    }

    pub async fn interrupt(&self, conversation: CodexConversationId) -> Result<(), ProtocolError> {
        let (thread, turn) = {
            let state = self.shared.state();
            (
                state.thread_of(conversation)?,
                state.active_turn(conversation),
            )
        };
        // 実行中のターンが無ければ中断するものが無いので何もしない。
        let Some(turn) = turn else {
            return Ok(());
        };
        let client = self.ensure_client().await?;
        client
            .request(
                "turn/interrupt",
                json!({ "threadId": thread, "turnId": turn }),
            )
            .await
            .map(|_| ())
    }

    pub async fn close(&self, conversation: CodexConversationId) -> Result<(), ProtocolError> {
        let thread = self.shared.state().thread_of(conversation)?;
        let client = self.ensure_client().await?;
        let result = client
            .request("thread/unsubscribe", json!({ "threadId": thread }))
            .await;
        // 購読解除が失敗しても Nebula 側の対応表は畳む。残しても届く先が無い。
        self.shared.state().unregister(conversation);
        result.map(|_| ())
    }

    pub async fn list_models(&self) -> Result<Vec<String>, ProtocolError> {
        let client = self.ensure_client().await?;
        let result = client.request("model/list", json!({})).await?;
        Ok(result
            .get("data")
            .and_then(Value::as_array)
            .map(|models| {
                models
                    .iter()
                    .filter_map(|m| m.get("id").and_then(Value::as_str))
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default())
    }

    /// ログイン状態と、分かればアカウント名を返す。
    ///
    /// `~/.codex/auth.json` の有無ではなく `account/read` を使う。API キー運用と
    /// ChatGPT ログインの両方を app-server が同じ形で答えてくれるため。
    pub async fn auth_status(&self) -> Result<(bool, Option<String>), ProtocolError> {
        let client = self.ensure_client().await?;
        let result = client.request("account/read", json!({})).await?;
        let account = result.get("account").filter(|a| !a.is_null());
        let name = account
            .and_then(|a| a.get("email"))
            .and_then(Value::as_str)
            .map(str::to_string);
        Ok((account.is_some(), name))
    }

    pub async fn shutdown(&self) {
        if let Some(client) = self.client.lock().await.take() {
            client.shutdown().await;
        }
    }

    /// app-server を必要になった時点で 1 つだけ起動する。
    ///
    /// プロセスが落ちていた場合は次の要求で起動し直す。GUI から見ると
    /// 「Codex ビューをもう一度使えば復帰する」挙動になる。
    async fn ensure_client(&self) -> Result<Arc<Client>, ProtocolError> {
        if self.tools.codex.is_none() {
            return Err(ProtocolError::unsupported(
                "codex コマンドが見つかりません。Codex 連携は使えません",
            ));
        }
        let mut slot = self.client.lock().await;
        if let Some(client) = slot.as_ref().filter(|c| c.is_alive()) {
            return Ok(client.clone());
        }
        let client = Client::start(self.shared.clone()).await?;
        *slot = Some(client.clone());
        Ok(client)
    }
}

// ---------------------------------------------------------------------------
// 会話の対応表
// ---------------------------------------------------------------------------

/// 読み取りタスクと API 呼び出しの双方から触る状態。
struct Shared {
    events: broadcast::Sender<Event>,
    state: Mutex<SharedState>,
}

impl Shared {
    fn state(&self) -> std::sync::MutexGuard<'_, SharedState> {
        self.state.lock().expect("Codex 状態のロック")
    }

    fn emit(&self, event: CodexEvent) {
        let _ = self.events.send(Event::Codex(event));
    }
}

#[derive(Default)]
struct SharedState {
    conversations: HashMap<CodexConversationId, Conversation>,
    /// app-server 側の thread ID から Nebula の会話 ID を引く逆引き表。
    /// 通知には thread ID しか入っていないため必要。
    by_thread: HashMap<String, CodexConversationId>,
    /// 未応答の承認要求。GUI が返してくる文字列 ID から元の JSON-RPC ID を引く。
    approvals: HashMap<String, PendingApproval>,
    /// ファイル変更の差分。承認要求 params に diff が無いので item/started で控える。
    patch_previews: HashMap<String, String>,
}

struct Conversation {
    thread: String,
    active_turn: Option<String>,
    token_usage: Option<CodexTokenUsage>,
}

struct PendingApproval {
    /// app-server が使った要求 ID。文字列と整数のどちらもあり得るので原型を保つ。
    id: Value,
    conversation: CodexConversationId,
}

impl SharedState {
    fn register(&mut self, conversation: CodexConversationId, thread: String) {
        self.by_thread.insert(thread.clone(), conversation);
        self.conversations.insert(
            conversation,
            Conversation {
                thread,
                active_turn: None,
                token_usage: None,
            },
        );
    }

    fn unregister(&mut self, conversation: CodexConversationId) {
        if let Some(entry) = self.conversations.remove(&conversation) {
            self.by_thread.remove(&entry.thread);
        }
        self.approvals.retain(|_, a| a.conversation != conversation);
    }

    fn thread_of(&self, conversation: CodexConversationId) -> Result<String, ProtocolError> {
        self.conversations
            .get(&conversation)
            .map(|c| c.thread.clone())
            .ok_or_else(|| {
                ProtocolError::not_found(format!("Codex 会話 {conversation} は開かれていません"))
            })
    }

    fn conversation_of(&self, thread: Option<&str>) -> Option<CodexConversationId> {
        self.by_thread.get(thread?).copied()
    }

    fn active_turn(&self, conversation: CodexConversationId) -> Option<String> {
        self.conversations
            .get(&conversation)
            .and_then(|c| c.active_turn.clone())
    }

    fn set_active_turn(&mut self, conversation: CodexConversationId, turn: Option<String>) {
        if let Some(entry) = self.conversations.get_mut(&conversation) {
            entry.active_turn = turn;
        }
    }

    fn set_token_usage(&mut self, conversation: CodexConversationId, usage: CodexTokenUsage) {
        if let Some(entry) = self.conversations.get_mut(&conversation) {
            entry.token_usage = Some(usage);
        }
    }

    fn token_usage(&self, conversation: CodexConversationId) -> Option<CodexTokenUsage> {
        self.conversations
            .get(&conversation)
            .and_then(|c| c.token_usage)
    }

    /// 承認要求を取り出す。取り違えを防ぐため会話も突き合わせ、
    /// 一致しないときは取り出さずに残す (正しい会話からの応答をまだ受けられるように)。
    fn take_approval(
        &mut self,
        request_id: &str,
        conversation: CodexConversationId,
    ) -> Result<PendingApproval, ProtocolError> {
        match self.approvals.get(request_id) {
            Some(pending) if pending.conversation == conversation => Ok(self
                .approvals
                .remove(request_id)
                .expect("直前に存在を確認した")),
            Some(_) => Err(ProtocolError::invalid(format!(
                "承認要求 {request_id} は会話 {conversation} のものではありません"
            ))),
            None => Err(ProtocolError::not_found(format!(
                "承認要求 {request_id} は応答済みか存在しません"
            ))),
        }
    }
}

// ---------------------------------------------------------------------------
// app-server プロセス
// ---------------------------------------------------------------------------

/// 送受信の口。読み取りタスクも承認応答を書くため、`Client` とは別に共有する。
struct Wire {
    stdin: AsyncMutex<ChildStdin>,
    next_id: AtomicI64,
    pending: Mutex<HashMap<i64, oneshot::Sender<Result<Value, ProtocolError>>>>,
    closed: AtomicBool,
}

impl Wire {
    async fn send(&self, frame: String) -> Result<(), ProtocolError> {
        let mut stdin = self.stdin.lock().await;
        stdin
            .write_all(frame.as_bytes())
            .await
            .map_err(|e| ProtocolError::external(format!("codex app-server へ書けません: {e}")))?;
        stdin
            .flush()
            .await
            .map_err(|e| ProtocolError::external(format!("codex app-server へ書けません: {e}")))
    }

    async fn request(&self, method: &str, params: Value) -> Result<Value, ProtocolError> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        self.pending
            .lock()
            .expect("待ち行列のロック")
            .insert(id, tx);

        if let Err(error) = self.send(protocol::request_frame(id, method, params)).await {
            self.pending.lock().expect("待ち行列のロック").remove(&id);
            return Err(error);
        }

        match tokio::time::timeout(REQUEST_TIMEOUT, rx).await {
            Ok(Ok(result)) => result,
            // 送信側が落ちた = プロセスが終わった。
            Ok(Err(_)) => Err(ProtocolError::external("codex app-server が終了しました")),
            Err(_) => {
                self.pending.lock().expect("待ち行列のロック").remove(&id);
                Err(ProtocolError::external(format!(
                    "codex app-server が {method} に応答しません"
                )))
            }
        }
    }

    fn resolve(&self, id: i64, result: Result<Value, ProtocolError>) {
        if let Some(tx) = self.pending.lock().expect("待ち行列のロック").remove(&id) {
            let _ = tx.send(result);
        }
    }

    /// プロセス終了時に待っている要求をすべて起こす。送信端を捨てるだけでよい。
    fn close(&self) {
        self.closed.store(true, Ordering::Release);
        self.pending.lock().expect("待ち行列のロック").clear();
    }
}

struct Client {
    wire: Arc<Wire>,
    child: AsyncMutex<Child>,
    reader: tokio::task::JoinHandle<()>,
}

impl Client {
    async fn start(shared: Arc<Shared>) -> Result<Arc<Self>, ProtocolError> {
        let mut child = Command::new("codex")
            .arg("app-server")
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            // 診断ログは Nebula では使わない。溜めるとパイプが詰まる。
            .stderr(std::process::Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .map_err(|e| {
                ProtocolError::unsupported(format!("codex app-server を起動できません: {e}"))
            })?;

        let stdin = child.stdin.take().expect("stdin を piped で起動した");
        let stdout = child.stdout.take().expect("stdout を piped で起動した");
        let wire = Arc::new(Wire {
            stdin: AsyncMutex::new(stdin),
            next_id: AtomicI64::new(1),
            pending: Mutex::new(HashMap::new()),
            closed: AtomicBool::new(false),
        });

        let reader = tokio::spawn(read_loop(stdout, wire.clone(), shared));
        let client = Arc::new(Self {
            wire,
            child: AsyncMutex::new(child),
            reader,
        });
        client.initialize().await?;
        Ok(client)
    }

    /// 仕様どおり initialize の応答を待ってから initialized を送る。
    async fn initialize(&self) -> Result<(), ProtocolError> {
        self.wire
            .request(
                "initialize",
                json!({
                    "clientInfo": {
                        "name": "nebula",
                        "title": "Nebula Code",
                        "version": env!("CARGO_PKG_VERSION"),
                    }
                }),
            )
            .await?;
        self.wire
            .send(protocol::notification_frame("initialized", json!({})))
            .await
    }

    fn is_alive(&self) -> bool {
        !self.wire.closed.load(Ordering::Acquire)
    }

    async fn request(&self, method: &str, params: Value) -> Result<Value, ProtocolError> {
        self.wire.request(method, params).await
    }

    async fn send(&self, frame: String) -> Result<(), ProtocolError> {
        self.wire.send(frame).await
    }

    async fn shutdown(&self) {
        self.reader.abort();
        let _ = self.child.lock().await.kill().await;
        self.wire.close();
    }
}

/// app-server の標準出力を 1 行ずつ処理し続ける。
async fn read_loop(stdout: ChildStdout, wire: Arc<Wire>, shared: Arc<Shared>) {
    let mut lines = BufReader::new(stdout).lines();
    while let Ok(Some(line)) = lines.next_line().await {
        match protocol::classify(&line) {
            Some(Incoming::Response { id, result }) => wire.resolve(id, Ok(result)),
            Some(Incoming::Failure { id, message }) => {
                wire.resolve(id, Err(ProtocolError::external(message)))
            }
            Some(Incoming::ServerRequest { id, method, params }) => {
                handle_server_request(&wire, &shared, id, &method, params).await
            }
            Some(Incoming::Notification { method, params }) => {
                handle_notification(&shared, &method, &params)
            }
            None => {}
        }
    }
    wire.close();
}

fn handle_notification(shared: &Shared, method: &str, params: &Value) {
    let mut state = shared.state();
    let conversation = state.conversation_of(translate::thread_id(params));

    if let Some(conversation) = conversation {
        // ターン完了イベントに載せる使用量は別の通知で届くので、先に控える。
        if method == "thread/tokenUsage/updated"
            && let Some(usage) = translate::parse_token_usage(params)
        {
            state.set_token_usage(conversation, usage);
        }
        if method == "turn/completed" {
            state.set_active_turn(conversation, None);
        }
        // 承認要求より先に届く item/started から差分を控えておく。
        // 承認が不要な設定 (approvalPolicy = never や自動承認) では要求が来ないまま
        // 変更が完了するので、完了通知で必ず捨てる。放置すると差分文字列が溜まり続ける。
        if let Some(item) = params.get("item")
            && let Some((item_id, diff)) = translate::patch_preview(item)
        {
            match method {
                "item/started" => {
                    state.patch_previews.insert(item_id, diff);
                }
                "item/completed" => {
                    state.patch_previews.remove(&item_id);
                }
                _ => {}
            }
        }
    }

    let token_usage = conversation.and_then(|c| state.token_usage(c));
    drop(state);

    shared.emit(translate::translate_notification(
        method,
        params,
        conversation,
        token_usage,
    ));
}

async fn handle_server_request(
    wire: &Wire,
    shared: &Shared,
    id: Value,
    method: &str,
    params: Value,
) {
    // JSON-RPC の ID は文字列か整数。GUI へは文字列で渡す約束なので、
    // 原型を残したまま `Value` の表記を鍵にする ("1" と 1 が衝突しない)。
    let request_id = id.to_string();
    // ロックを握ったまま await するとこのタスクが Send でなくなるため、
    // 状態の更新はこのブロックで閉じる。
    let (conversation, approval) = {
        let mut state = shared.state();
        let conversation = state.conversation_of(translate::thread_id(&params));
        let preview = params
            .get("itemId")
            .and_then(Value::as_str)
            .and_then(|item_id| state.patch_previews.remove(item_id));
        let approval = conversation.and_then(|conversation| {
            let request = translate::translate_approval(method, &params, &request_id, preview)?;
            state.approvals.insert(
                request_id.clone(),
                PendingApproval {
                    id: id.clone(),
                    conversation,
                },
            );
            Some(request)
        });
        (conversation, approval)
    };

    match (conversation, approval) {
        (Some(conversation), Some(request)) => shared.emit(CodexEvent::ApprovalRequested {
            conversation,
            request,
        }),
        _ => {
            // 未対応の要求を黙殺すると app-server が応答待ちで止まる。
            // 内容は落とさず Raw で GUI にも見せる。
            shared.emit(CodexEvent::Raw {
                conversation,
                method: method.to_string(),
                payload: params.to_string(),
            });
            let _ = wire
                .send(protocol::method_not_found_frame(&id, method))
                .await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn service(codex: Option<&str>) -> CodexService {
        let (events, _rx) = broadcast::channel(64);
        CodexService::new(
            events,
            DetectedTools {
                codex: codex.map(str::to_string),
                ..DetectedTools::default()
            },
        )
    }

    #[tokio::test]
    async fn codex_が無ければ未対応を返す() {
        let service = service(None);
        let error = service.list_models().await.unwrap_err();
        assert_eq!(error.kind, nebula_protocol::ProtocolErrorKind::Unsupported);
    }

    #[tokio::test]
    async fn 未知の会話への送信は見つからない扱い() {
        let service = service(Some("0.147.0"));
        let error = service
            .send_message(CodexConversationId(999), "hello", &[])
            .await
            .unwrap_err();
        assert_eq!(error.kind, nebula_protocol::ProtocolErrorKind::NotFound);
    }

    /// 応答の待ち合わせだけを試すための `Wire`。
    /// 相手には何も喋らない `cat` を置き、応答は試験側から `resolve` で流し込む。
    async fn wire_with_dummy_child() -> (Arc<Wire>, Child) {
        let mut child = Command::new("cat")
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .expect("cat の起動");
        let stdin = child.stdin.take().expect("stdin を piped で起動した");
        let wire = Arc::new(Wire {
            stdin: AsyncMutex::new(stdin),
            next_id: AtomicI64::new(1),
            pending: Mutex::new(HashMap::new()),
            closed: AtomicBool::new(false),
        });
        (wire, child)
    }

    #[tokio::test]
    async fn 複数の要求は要求識別子ごとに応答が振り分けられる() {
        let (wire, _child) = wire_with_dummy_child().await;
        let first = tokio::spawn({
            let wire = wire.clone();
            async move { wire.request("model/list", json!({})).await }
        });
        let second = tokio::spawn({
            let wire = wire.clone();
            async move { wire.request("account/read", json!({})).await }
        });

        // 2 件とも送信され待ち行列に載るまで待つ。
        while wire.pending.lock().unwrap().len() < 2 {
            tokio::task::yield_now().await;
        }

        // 応答は要求と逆順に返す。id で対応付けているので取り違えない。
        wire.resolve(2, Ok(json!("二番目")));
        wire.resolve(1, Ok(json!("一番目")));

        assert_eq!(first.await.unwrap().unwrap(), json!("一番目"));
        assert_eq!(second.await.unwrap().unwrap(), json!("二番目"));
    }

    #[tokio::test]
    async fn プロセス終了で待機中の要求が起こされる() {
        let (wire, _child) = wire_with_dummy_child().await;
        let waiting = tokio::spawn({
            let wire = wire.clone();
            async move { wire.request("model/list", json!({})).await }
        });
        while wire.pending.lock().unwrap().is_empty() {
            tokio::task::yield_now().await;
        }

        wire.close();

        let error = waiting.await.unwrap().unwrap_err();
        assert_eq!(error.kind, nebula_protocol::ProtocolErrorKind::ExternalTool);
        // 落ちたプロセスは使い回さず、次の要求で起動し直す。
        assert!(wire.closed.load(Ordering::Acquire));
    }

    #[test]
    fn 会話の対応表は双方向に引ける() {
        let mut state = SharedState::default();
        let a = CodexConversationId(1);
        let b = CodexConversationId(2);
        state.register(a, "thread-a".to_string());
        state.register(b, "thread-b".to_string());

        assert_eq!(state.thread_of(a).unwrap(), "thread-a");
        assert_eq!(state.conversation_of(Some("thread-b")), Some(b));
        assert_eq!(state.conversation_of(Some("thread-x")), None);

        state.unregister(a);
        assert!(state.thread_of(a).is_err());
        assert_eq!(state.conversation_of(Some("thread-a")), None);
        // 片方を閉じてももう片方は生きている。
        assert_eq!(state.thread_of(b).unwrap(), "thread-b");
    }

    #[test]
    fn 承認は一度しか応答できない() {
        let mut state = SharedState::default();
        let conversation = CodexConversationId(1);
        state.approvals.insert(
            "\"req-1\"".to_string(),
            PendingApproval {
                id: json!("req-1"),
                conversation,
            },
        );
        // 別の会話からの応答では取り出せない。
        assert!(
            state
                .take_approval("\"req-1\"", CodexConversationId(2))
                .is_err()
        );
        let taken = state.take_approval("\"req-1\"", conversation).unwrap();
        assert_eq!(taken.id, json!("req-1"));
        assert!(state.take_approval("\"req-1\"", conversation).is_err());
    }

    /// 通知を処理すると対応する `Event::Codex` が流れることを、
    /// プロセスを起こさずに確かめる。
    #[test]
    fn 通知は会話識別子を解決して配信される() {
        let (events, mut rx) = broadcast::channel(64);
        let shared = Shared {
            events,
            state: Mutex::new(SharedState::default()),
        };
        let conversation = CodexConversationId(7);
        shared.state().register(conversation, "th-1".to_string());

        // 使用量は先に控えられ、ターン完了に載る。
        handle_notification(
            &shared,
            "thread/tokenUsage/updated",
            &serde_json::from_str(
                r#"{"threadId":"th-1","turnId":"t","tokenUsage":{"total":{"totalTokens":10,"inputTokens":8,"cachedInputTokens":4,"outputTokens":2,"reasoningOutputTokens":0},"last":{"totalTokens":1,"inputTokens":1,"cachedInputTokens":0,"outputTokens":0,"reasoningOutputTokens":0}}}"#,
            )
            .unwrap(),
        );
        assert!(matches!(
            rx.try_recv().unwrap(),
            Event::Codex(CodexEvent::Raw { .. })
        ));

        handle_notification(
            &shared,
            "turn/completed",
            &serde_json::from_str(
                r#"{"threadId":"th-1","turn":{"id":"t","items":[],"status":"completed"}}"#,
            )
            .unwrap(),
        );
        let Event::Codex(CodexEvent::TurnComplete { token_usage, .. }) = rx.try_recv().unwrap()
        else {
            panic!("ターン完了になりませんでした");
        };
        assert_eq!(token_usage.unwrap().total_tokens, 10);
    }

    #[test]
    fn 控えた差分は変更の完了で捨てられる() {
        let (events, _rx) = broadcast::channel(64);
        let shared = Shared {
            events,
            state: Mutex::new(SharedState::default()),
        };
        shared
            .state()
            .register(CodexConversationId(7), "th-1".to_string());
        let item = r#"{"type":"fileChange","id":"exec-1","changes":[{"path":"/tmp/a.txt","kind":{"type":"add"},"diff":"+a\n"}],"status":"STATUS"}"#;

        let started = format!(
            r#"{{"threadId":"th-1","turnId":"t","item":{}}}"#,
            item.replace("STATUS", "inProgress")
        );
        handle_notification(
            &shared,
            "item/started",
            &serde_json::from_str(&started).unwrap(),
        );
        assert_eq!(shared.state().patch_previews.len(), 1);

        // 承認要求が来ないまま完了する構成 (approvalPolicy = never) でも溜め込まない。
        let completed = format!(
            r#"{{"threadId":"th-1","turnId":"t","item":{}}}"#,
            item.replace("STATUS", "completed")
        );
        handle_notification(
            &shared,
            "item/completed",
            &serde_json::from_str(&completed).unwrap(),
        );
        assert!(shared.state().patch_previews.is_empty());
    }

    /// `codex app-server` を実際に起動し、初期化まで通ることを確かめる。
    /// ネットワークもログインも要らない経路だけを叩く。
    #[tokio::test]
    async fn app_server_を起動して初期化できる() {
        if which_codex().is_none() {
            eprintln!("codex が PATH に無いので飛ばします");
            return;
        }
        let service = service(Some("test"));
        // 起動と initialize / initialized のやり取りがここで走る。
        let client = service.ensure_client().await.expect("app-server の起動");
        assert!(client.is_alive());

        // 会話を 1 つ開いて、多重化の入口 (thread/start) まで通ることを見る。
        let spec = CodexSessionSpec {
            workspace: nebula_protocol::WorkspaceId(1),
            model: None,
            approval_policy: nebula_protocol::CodexApprovalPolicy::OnRequest,
            sandbox_policy: nebula_protocol::CodexSandboxPolicy::ReadOnly,
            cwd: Some(std::env::temp_dir()),
        };
        let conversation = service.new_conversation(spec).await.expect("会話の開始");
        let thread = service
            .shared
            .state()
            .thread_of(conversation)
            .expect("会話の対応表");

        service.close(conversation).await.expect("会話を閉じる");
        // 試験で作った会話をユーザーの ~/.codex に残さない。
        let _ = client
            .request("thread/delete", json!({ "threadId": thread }))
            .await;
        service.shutdown().await;
    }

    /// 実際に 1 ターン走らせて、応答が `CodexEvent` として流れてくることを見る。
    /// ログインとネットワークが要るので通常のテストからは外す。
    #[tokio::test]
    #[ignore = "codex へのログインとネットワークが必要"]
    async fn 実際のターンで応答イベントが流れる() {
        let (events, mut rx) = broadcast::channel(1024);
        let service = CodexService::new(
            events,
            DetectedTools {
                codex: Some("test".to_string()),
                ..DetectedTools::default()
            },
        );
        let conversation = service
            .new_conversation(CodexSessionSpec {
                workspace: nebula_protocol::WorkspaceId(1),
                model: None,
                approval_policy: nebula_protocol::CodexApprovalPolicy::Never,
                sandbox_policy: nebula_protocol::CodexSandboxPolicy::ReadOnly,
                cwd: Some(std::env::temp_dir()),
            })
            .await
            .expect("会話の開始");
        service
            .send_message(conversation, "Reply with exactly: PONG", &[])
            .await
            .expect("メッセージの送信");

        let mut saw_message = false;
        let deadline = tokio::time::Instant::now() + Duration::from_secs(120);
        while let Ok(Ok(event)) = tokio::time::timeout_at(deadline, rx.recv()).await {
            match event {
                Event::Codex(CodexEvent::AgentMessage { .. }) => saw_message = true,
                Event::Codex(CodexEvent::TurnComplete { .. }) => break,
                _ => {}
            }
        }
        service.shutdown().await;
        assert!(saw_message, "アシスタントの応答が届きませんでした");
    }

    fn which_codex() -> Option<PathBuf> {
        std::env::var_os("PATH")
            .map(|paths| std::env::split_paths(&paths).collect::<Vec<_>>())?
            .into_iter()
            .map(|dir| dir.join("codex"))
            .find(|candidate| candidate.is_file())
    }
}
