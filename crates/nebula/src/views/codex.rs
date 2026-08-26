//! Codex パネル。
//!
//! バックエンドが `codex app-server` と話し、GUI には [`CodexEvent`] だけが流れてくる。
//! このビューはそのイベント列を会話の見た目へ組み立てるだけで、Codex のプロトコルには
//! 一切触れない。
//!
//! 設計上の要点は 3 つ。
//!
//! 1. **会話はパネルを開いた時点では作らない。** `codex app-server` の起動は数百 ms
//!    かかるため、最初のメッセージ送信まで遅らせる。
//! 2. **Delta 系イベントは末尾の吹き出しへ追記する。** ただしターンが終わった吹き出しは
//!    「封をする」。封をしないと、次のターンの 1 文字目が前のターンの応答に続いてしまう。
//! 3. **状態遷移は描画から切り離した純粋関数に置く。** gpui を起動せずに単体テストできる。

use crate::assets::Icon;
use crate::ipc_client::BackendClient;
use crate::theme::{metrics, theme};
use crate::ui::format_keystroke;
use crate::ui::{
    TextInput, TextInputEvent, h_flex, icon, list_row, panel_header, truncate_middle, v_flex,
};
use gpui::prelude::*;
use gpui::{
    AnyElement, Context, ElementId, Entity, Hsla, MouseButton, ScrollHandle, Subscription, Window,
    div, px, relative,
};
use nebula_protocol::{
    CodexApprovalDecision, CodexApprovalKind, CodexApprovalPolicy, CodexApprovalRequest,
    CodexConversationId, CodexEvent, CodexSandboxPolicy, CodexSessionSpec, CodexTokenUsage, Event,
    Request, Response, WorkspaceInfo,
};
use std::path::{Path, PathBuf};

/// 等幅で出すブロックの書体。書体名は theme.rs の metrics に集約してある。
const MONO_FONT: &str = metrics::MONO_FONT_FAMILY;
/// 入力欄が伸びる上限 (折り返し後の行数)。これを超えたぶんはカーソル追従でスクロールする。
const MAX_INPUT_ROWS: usize = 8;

// ---------------------------------------------------------------------------
// 会話の要素
// ---------------------------------------------------------------------------

/// 会話に並ぶ 1 要素。
///
/// `sealed` は「この吹き出しはもう伸びない」印。ターン完了か確定メッセージの受信で立てる。
#[derive(Debug, Clone, PartialEq, Eq)]
enum ChatItem {
    User(String),
    Assistant {
        text: String,
        sealed: bool,
    },
    Reasoning {
        text: String,
        sealed: bool,
        collapsed: bool,
    },
    Exec {
        call_id: String,
        command: Vec<String>,
        cwd: PathBuf,
        output: String,
        exit_code: Option<i32>,
    },
    Patch {
        files: Vec<PathBuf>,
    },
    Approval {
        request: CodexApprovalRequest,
        decision: Option<CodexApprovalDecision>,
    },
    Error(String),
    /// セッション確立などの短い案内。
    Notice(String),
    Raw {
        method: String,
        payload: String,
        expanded: bool,
    },
}

/// まだ伸びている途中の吹き出しを探す。
///
/// 末尾だけを見ないのは、バックエンドが翻訳できない通知を [`ChatItem::Raw`] として
/// 素通しするため。増分の合間に 1 つ挟まるだけで末尾が変わり、続きが別の吹き出しへ
/// 分かれたり、確定文が二重に出たりする。
///
/// 探索は直前の [`ChatItem::User`] で打ち切る。中断などで封をされないまま残った
/// 前ターンの吹き出しに、次のターンの応答を書き足さないため。
fn open_bubble(items: &[ChatItem], is_open: fn(&ChatItem) -> bool) -> Option<usize> {
    for (index, item) in items.iter().enumerate().rev() {
        if matches!(item, ChatItem::User(_)) {
            return None;
        }
        if is_open(item) {
            return Some(index);
        }
    }
    None
}

fn is_open_assistant(item: &ChatItem) -> bool {
    matches!(item, ChatItem::Assistant { sealed: false, .. })
}

fn is_open_reasoning(item: &ChatItem) -> bool {
    matches!(item, ChatItem::Reasoning { sealed: false, .. })
}

/// アシスタント応答の増分を足す。封のされた吹き出しには足さない。
fn push_assistant_delta(items: &mut Vec<ChatItem>, delta: &str) {
    match open_bubble(items, is_open_assistant).and_then(|index| items.get_mut(index)) {
        Some(ChatItem::Assistant { text, .. }) => text.push_str(delta),
        _ => items.push(ChatItem::Assistant {
            text: delta.to_string(),
            sealed: false,
        }),
    }
}

/// 確定したアシスタント応答。増分で組み立てた本文を全文で置き換える。
///
/// 置き換えるのは、増分と確定文の両方が届く実装でも二重に出さないため。
fn finish_assistant_message(items: &mut Vec<ChatItem>, full: String) {
    match open_bubble(items, is_open_assistant).and_then(|index| items.get_mut(index)) {
        Some(ChatItem::Assistant { text, sealed }) => {
            *text = full;
            *sealed = true;
        }
        _ => items.push(ChatItem::Assistant {
            text: full,
            sealed: true,
        }),
    }
}

fn push_reasoning_delta(items: &mut Vec<ChatItem>, delta: &str) {
    match open_bubble(items, is_open_reasoning).and_then(|index| items.get_mut(index)) {
        Some(ChatItem::Reasoning { text, .. }) => text.push_str(delta),
        _ => items.push(ChatItem::Reasoning {
            text: delta.to_string(),
            sealed: false,
            collapsed: false,
        }),
    }
}

/// ターンの区切り。以降の増分は新しい吹き出しになる。
///
/// 推論はここで畳む。読み終わったターンの思考過程が開いたままだと、
/// 会話を遡るときに本文が埋もれる。
fn seal_turn(items: &mut [ChatItem]) {
    for item in items.iter_mut() {
        match item {
            ChatItem::Assistant { sealed, .. } => *sealed = true,
            ChatItem::Reasoning {
                sealed, collapsed, ..
            } => {
                if !*sealed {
                    *collapsed = true;
                }
                *sealed = true;
            }
            _ => {}
        }
    }
}

fn begin_exec(items: &mut Vec<ChatItem>, call_id: String, command: Vec<String>, cwd: PathBuf) {
    items.push(ChatItem::Exec {
        call_id,
        command,
        cwd,
        output: String::new(),
        exit_code: None,
    });
}

/// 実行中コマンドを `call_id` で探す。
///
/// 「末尾の要素」ではなく ID で引くのは、複数のコマンドが並行して走ることがあるため。
fn find_exec<'a>(items: &'a mut [ChatItem], call_id: &str) -> Option<&'a mut ChatItem> {
    items
        .iter_mut()
        .rev()
        .find(|item| matches!(item, ChatItem::Exec { call_id: id, .. } if id == call_id))
}

fn append_exec_output(items: &mut [ChatItem], call_id: &str, chunk: &str) {
    if let Some(ChatItem::Exec { output, .. }) = find_exec(items, call_id) {
        output.push_str(chunk);
    }
}

fn finish_exec(items: &mut [ChatItem], call_id: &str, code: i32) {
    if let Some(ChatItem::Exec { exit_code, .. }) = find_exec(items, call_id) {
        *exit_code = Some(code);
    }
}

/// 承認カードに結果を書き込む。既に応答済みなら何もしない (二重送信を防ぐ)。
fn resolve_approval(
    items: &mut [ChatItem],
    request_id: &str,
    answer: CodexApprovalDecision,
) -> bool {
    for item in items.iter_mut().rev() {
        if let ChatItem::Approval { request, decision } = item
            && request.request_id == request_id
        {
            if decision.is_some() {
                return false;
            }
            *decision = Some(answer);
            return true;
        }
    }
    false
}

/// イベントが属する会話。`None` はどの会話にも紐づかない生イベント。
fn conversation_of(event: &CodexEvent) -> Option<CodexConversationId> {
    match event {
        CodexEvent::SessionConfigured { conversation, .. }
        | CodexEvent::AgentMessageDelta { conversation, .. }
        | CodexEvent::AgentMessage { conversation, .. }
        | CodexEvent::ReasoningDelta { conversation, .. }
        | CodexEvent::ExecBegin { conversation, .. }
        | CodexEvent::ExecOutput { conversation, .. }
        | CodexEvent::ExecEnd { conversation, .. }
        | CodexEvent::PatchApplied { conversation, .. }
        | CodexEvent::ApprovalRequested { conversation, .. }
        | CodexEvent::TurnComplete { conversation, .. }
        | CodexEvent::Error { conversation, .. } => Some(*conversation),
        CodexEvent::Raw { conversation, .. } => *conversation,
    }
}

/// 未応答の承認が残っているか。残っていると Codex 側の処理は止まったままになる。
fn has_pending_approval(items: &[ChatItem]) -> bool {
    items
        .iter()
        .any(|item| matches!(item, ChatItem::Approval { decision: None, .. }))
}

// ---------------------------------------------------------------------------
// Markdown の最小限の分解
// ---------------------------------------------------------------------------

/// 応答本文の断片。Markdown のうちコードブロックだけを区別する。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MarkdownSegment {
    Text(String),
    Code {
        language: Option<String>,
        body: String,
    },
}

/// 本文を「通常の文」と「コードブロック」の列に分ける。
///
/// 閉じていないフェンスは末尾までをコードとして扱う。応答をストリーミングしている
/// 途中は必ずこの形になるので、捨てると書きかけのコードが画面から消える。
pub fn split_code_blocks(text: &str) -> Vec<MarkdownSegment> {
    let mut segments = Vec::new();
    let mut text_lines: Vec<&str> = Vec::new();
    let mut code: Option<(Option<String>, Vec<&str>)> = None;

    for line in text.split('\n') {
        let trimmed = line.trim();
        if let Some((_, body)) = code.as_mut() {
            if trimmed == "```" {
                let (language, body) = code.take().expect("直前に存在を確認している");
                segments.push(MarkdownSegment::Code {
                    language,
                    body: body.join("\n"),
                });
            } else {
                body.push(line);
            }
        } else if let Some(rest) = trimmed.strip_prefix("```") {
            flush_text(&mut segments, &mut text_lines);
            let language = (!rest.trim().is_empty()).then(|| rest.trim().to_string());
            code = Some((language, Vec::new()));
        } else {
            text_lines.push(line);
        }
    }

    match code {
        Some((language, body)) => segments.push(MarkdownSegment::Code {
            language,
            body: body.join("\n"),
        }),
        None => flush_text(&mut segments, &mut text_lines),
    }
    segments
}

fn flush_text(segments: &mut Vec<MarkdownSegment>, lines: &mut Vec<&str>) {
    let joined = lines.join("\n");
    lines.clear();
    let trimmed = joined.trim_matches('\n');
    if !trimmed.trim().is_empty() {
        segments.push(MarkdownSegment::Text(trimmed.to_string()));
    }
}

// ---------------------------------------------------------------------------
// 表示用の整形
// ---------------------------------------------------------------------------

/// 実行コマンドを 1 行にする。空白を含む引数は引用符でくくる。
pub fn format_command(command: &[String]) -> String {
    command
        .iter()
        .map(|arg| {
            if arg.is_empty() || arg.chars().any(char::is_whitespace) {
                format!("\"{arg}\"")
            } else {
                arg.clone()
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// ヘッダに出すトークン使用量。桁が大きいので 1000 単位で丸める。
pub fn format_token_usage(usage: &CodexTokenUsage) -> String {
    format!(
        "↑{} ↓{} 計{}",
        compact_count(usage.input_tokens),
        compact_count(usage.output_tokens),
        compact_count(usage.total_tokens)
    )
}

fn compact_count(value: u64) -> String {
    match value {
        0..=999 => value.to_string(),
        1_000..=999_999 => format!("{:.1}k", value as f64 / 1000.0),
        _ => format!("{:.1}M", value as f64 / 1_000_000.0),
    }
}

fn approval_policy_label(policy: CodexApprovalPolicy) -> &'static str {
    match policy {
        CodexApprovalPolicy::Untrusted => "未信頼のみ確認",
        CodexApprovalPolicy::OnFailure => "失敗時に確認",
        CodexApprovalPolicy::OnRequest => "要求時に確認",
        CodexApprovalPolicy::Never => "確認しない",
    }
}

fn sandbox_policy_label(policy: CodexSandboxPolicy) -> &'static str {
    match policy {
        CodexSandboxPolicy::ReadOnly => "読み取りのみ",
        CodexSandboxPolicy::WorkspaceWrite => "ワークスペース書込可",
        CodexSandboxPolicy::DangerFullAccess => "制限なし (危険)",
    }
}

fn decision_label(decision: CodexApprovalDecision) -> &'static str {
    match decision {
        CodexApprovalDecision::Approve => "許可しました",
        CodexApprovalDecision::ApproveForSession => "このセッションは常に許可します",
        CodexApprovalDecision::Deny => "拒否しました",
        CodexApprovalDecision::Abort => "中止しました",
    }
}

fn approval_kind_label(kind: CodexApprovalKind) -> &'static str {
    match kind {
        CodexApprovalKind::ExecCommand => "コマンド実行の承認",
        CodexApprovalKind::ApplyPatch => "ファイル変更の承認",
        CodexApprovalKind::Other => "承認",
    }
}

// ---------------------------------------------------------------------------
// ビュー本体
// ---------------------------------------------------------------------------

/// ヘッダで開いているドロップダウン。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Dropdown {
    Model,
    Approval,
    Sandbox,
}

pub struct CodexView {
    client: Option<BackendClient>,
    workspace: Option<WorkspaceInfo>,
    conversation: Option<CodexConversationId>,

    items: Vec<ChatItem>,
    models: Vec<String>,
    selected_model: Option<String>,
    approval_policy: CodexApprovalPolicy,
    sandbox_policy: CodexSandboxPolicy,
    dropdown: Option<Dropdown>,
    /// ログイン状態。未取得なら `None`。
    logged_in: Option<bool>,
    account: Option<String>,
    token_usage: Option<CodexTokenUsage>,

    /// 会話生成の要求が飛んでいる。二重に作らないための鍵。
    starting: bool,
    /// ターンが進行中。停止ボタンの出し分けに使う。
    running: bool,
    /// 会話の確立を待っている送信文。会話生成中に続けて送られると複数たまる。
    pending_sends: Vec<String>,
    /// モデル一覧と認証状態を一度だけ取りにいくための印。
    bootstrapped: bool,

    input: Entity<TextInput>,
    scroll: ScrollHandle,
    /// 購読は保持しないと即座に解除される。
    _subscriptions: Vec<Subscription>,
}

impl CodexView {
    pub fn new(cx: &mut Context<Self>) -> Self {
        // Enter で送信、Shift+Enter で改行。伸びても MAX_INPUT_ROWS 行で止める。
        let input =
            cx.new(|cx| TextInput::multi_line("Codex に指示を出す…", 1, Some(MAX_INPUT_ROWS), cx));
        let subscriptions = vec![cx.subscribe(&input, Self::on_input_event)];
        Self {
            client: None,
            workspace: None,
            conversation: None,
            items: Vec::new(),
            models: Vec::new(),
            selected_model: None,
            approval_policy: CodexApprovalPolicy::OnRequest,
            sandbox_policy: CodexSandboxPolicy::WorkspaceWrite,
            dropdown: None,
            logged_in: None,
            account: None,
            token_usage: None,
            starting: false,
            running: false,
            pending_sends: Vec::new(),
            bootstrapped: false,
            input,
            scroll: ScrollHandle::new(),
            _subscriptions: subscriptions,
        }
    }

    pub fn set_client(&mut self, client: BackendClient, cx: &mut Context<Self>) {
        self.client = Some(client);
        cx.notify();
    }

    pub fn set_workspace(&mut self, workspace: WorkspaceInfo, cx: &mut Context<Self>) {
        self.workspace = Some(workspace);
        cx.notify();
    }

    pub fn handle_event(&mut self, event: &Event, cx: &mut Context<Self>) {
        let Event::Codex(codex) = event else {
            return;
        };
        if !self.apply_codex_event(codex) {
            return;
        }
        // 新しい出来事は常に末尾に足すので、追従してスクロールする。
        self.scroll.scroll_to_bottom();
        cx.notify();
    }

    /// このパネルの会話のイベントか。
    ///
    /// バックエンドはユーザーあたり 1 つで、イベントは全ウィンドウに流れる。
    /// ID が一致しないものを取り込むと、別ウィンドウの会話が混ざる。
    fn accepts(&self, conversation: CodexConversationId) -> bool {
        self.conversation == Some(conversation)
    }

    /// 取り込んだら `true`。他の会話のイベントなら `false`。
    fn apply_codex_event(&mut self, event: &CodexEvent) -> bool {
        // `SessionConfigured` は要求の応答より先に届くことがある。
        // 自分が会話生成を頼んでいる最中に限り、ここで ID を採用する。
        if let CodexEvent::SessionConfigured { conversation, .. } = event
            && self.conversation.is_none()
            && self.starting
        {
            self.conversation = Some(*conversation);
        }
        // 会話に紐づかない生イベントは、どの会話の話か判らないので常に取り込む。
        if let Some(conversation) = conversation_of(event)
            && !self.accepts(conversation)
        {
            return false;
        }

        match event {
            CodexEvent::SessionConfigured { model, .. } => {
                self.selected_model = Some(model.clone());
                self.items
                    .push(ChatItem::Notice(format!("セッション開始 — {model}")));
            }
            CodexEvent::AgentMessageDelta { delta, .. } => {
                push_assistant_delta(&mut self.items, delta)
            }
            CodexEvent::AgentMessage { text, .. } => {
                finish_assistant_message(&mut self.items, text.clone())
            }
            CodexEvent::ReasoningDelta { delta, .. } => {
                push_reasoning_delta(&mut self.items, delta)
            }
            CodexEvent::ExecBegin {
                call_id,
                command,
                cwd,
                ..
            } => begin_exec(
                &mut self.items,
                call_id.clone(),
                command.clone(),
                cwd.clone(),
            ),
            CodexEvent::ExecOutput { call_id, chunk, .. } => {
                append_exec_output(&mut self.items, call_id, chunk)
            }
            CodexEvent::ExecEnd {
                call_id, exit_code, ..
            } => finish_exec(&mut self.items, call_id, *exit_code),
            CodexEvent::PatchApplied { files, .. } => self.items.push(ChatItem::Patch {
                files: files.clone(),
            }),
            CodexEvent::ApprovalRequested { request, .. } => self.items.push(ChatItem::Approval {
                request: request.clone(),
                decision: None,
            }),
            CodexEvent::TurnComplete { token_usage, .. } => {
                self.running = false;
                if let Some(usage) = token_usage {
                    self.token_usage = Some(*usage);
                }
                seal_turn(&mut self.items);
            }
            CodexEvent::Error { message, .. } => {
                self.running = false;
                self.items.push(ChatItem::Error(message.clone()));
            }
            CodexEvent::Raw {
                method, payload, ..
            } => self.items.push(ChatItem::Raw {
                method: method.clone(),
                payload: payload.clone(),
                expanded: false,
            }),
        }
        true
    }

    // -- バックエンドへの要求 --

    /// モデル一覧と認証状態を取りにいく。
    ///
    /// `set_client` ではなく最初の描画で行う。`set_client` は接続時に全ビューへ配られるため、
    /// Codex パネルを一度も開かない利用でも `codex` を起動してしまう。
    fn bootstrap(&mut self, cx: &mut Context<Self>) {
        if self.bootstrapped {
            return;
        }
        let Some(client) = self.client.clone() else {
            return;
        };
        self.bootstrapped = true;
        cx.spawn(async move |this, cx| {
            let auth = client.request(Request::CodexAuthStatus).await;
            let models = client.request(Request::CodexListModels).await;
            this.update(cx, |this, cx| {
                match auth {
                    Ok(Response::CodexAuth { logged_in, account }) => {
                        this.logged_in = Some(logged_in);
                        this.account = account;
                    }
                    // 取得できない = codex が無い。未ログインと同じ案内に寄せる。
                    Err(_) => this.logged_in = Some(false),
                    Ok(_) => {}
                }
                if let Ok(Response::CodexModels(models)) = models {
                    if this.selected_model.is_none() {
                        this.selected_model = models.first().cloned();
                    }
                    this.models = models;
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn submit(&mut self, cx: &mut Context<Self>) {
        let text = self.input.read(cx).text().to_string();
        if text.trim().is_empty() {
            return;
        }
        if self.workspace.is_none() {
            self.items.push(ChatItem::Error(
                "フォルダを開いてから送信してください".into(),
            ));
            cx.notify();
            return;
        }
        self.input.update(cx, |input, cx| input.clear(cx));
        self.items.push(ChatItem::User(text.clone()));
        self.running = true;
        self.scroll.scroll_to_bottom();

        match self.conversation {
            Some(conversation) => self.dispatch_send(conversation, text, cx),
            None => {
                // 会話ができるまでの間に続けて送られたぶんも順に取っておく。
                self.pending_sends.push(text);
                self.start_conversation(cx);
            }
        }
        cx.notify();
    }

    fn start_conversation(&mut self, cx: &mut Context<Self>) {
        if self.starting {
            return;
        }
        let (Some(client), Some(workspace)) = (self.client.clone(), self.workspace.clone()) else {
            self.fail_send("バックエンドに接続していません");
            return;
        };
        self.starting = true;
        let spec = CodexSessionSpec {
            workspace: workspace.id,
            model: self.selected_model.clone(),
            approval_policy: self.approval_policy,
            sandbox_policy: self.sandbox_policy,
            cwd: Some(workspace.root.clone()),
        };
        cx.spawn(async move |this, cx| {
            let result = client.request(Request::CodexNewConversation { spec }).await;
            this.update(cx, |this, cx| {
                this.starting = false;
                match result {
                    Ok(Response::CodexConversation { conversation }) => {
                        this.conversation = Some(conversation);
                        for text in std::mem::take(&mut this.pending_sends) {
                            this.dispatch_send(conversation, text, cx);
                        }
                    }
                    Ok(_) => this.fail_send("会話を開始できませんでした"),
                    Err(e) => this.fail_send(&format!("会話を開始できません: {e}")),
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn dispatch_send(
        &mut self,
        conversation: CodexConversationId,
        text: String,
        cx: &mut Context<Self>,
    ) {
        let Some(client) = self.client.clone() else {
            self.fail_send("バックエンドに接続していません");
            return;
        };
        cx.spawn(async move |this, cx| {
            let result = client
                .request(Request::CodexSendMessage {
                    conversation,
                    text,
                    attachments: Vec::new(),
                })
                .await;
            if let Err(e) = result {
                this.update(cx, |this, cx| {
                    this.fail_send(&format!("送信できません: {e}"));
                    cx.notify();
                })
                .ok();
            }
        })
        .detach();
    }

    fn fail_send(&mut self, message: &str) {
        self.running = false;
        self.pending_sends.clear();
        self.items.push(ChatItem::Error(message.to_string()));
    }

    fn interrupt(&mut self, cx: &mut Context<Self>) {
        let (Some(client), Some(conversation)) = (self.client.clone(), self.conversation) else {
            return;
        };
        self.running = false;
        cx.spawn(async move |_this, _cx| {
            client
                .request(Request::CodexInterrupt { conversation })
                .await
                .ok();
        })
        .detach();
        cx.notify();
    }

    fn respond_approval(
        &mut self,
        request_id: String,
        decision: CodexApprovalDecision,
        cx: &mut Context<Self>,
    ) {
        if !resolve_approval(&mut self.items, &request_id, decision) {
            return;
        }
        cx.notify();
        let (Some(client), Some(conversation)) = (self.client.clone(), self.conversation) else {
            return;
        };
        cx.spawn(async move |this, cx| {
            let result = client
                .request(Request::CodexRespondApproval {
                    conversation,
                    request_id,
                    decision,
                })
                .await;
            if let Err(e) = result {
                this.update(cx, |this, cx| {
                    this.items
                        .push(ChatItem::Error(format!("承認を送れません: {e}")));
                    cx.notify();
                })
                .ok();
            }
        })
        .detach();
    }

    fn on_input_event(
        &mut self,
        _entity: Entity<TextInput>,
        event: &TextInputEvent,
        cx: &mut Context<Self>,
    ) {
        match event {
            TextInputEvent::Submit => self.submit(cx),
            // 送信ボタンの活性と枠のネオンをこのビューが描くので、都度描き直す。
            TextInputEvent::Changed | TextInputEvent::FocusChanged => cx.notify(),
            TextInputEvent::Cancel => {}
        }
    }

    /// 設定変更は次の会話から効く。会話生成時に app-server へ渡す値だから。
    fn note_setting_change(&mut self) {
        if self.conversation.is_some() {
            self.items.push(ChatItem::Notice(
                "設定は次に開始する会話から反映されます".into(),
            ));
        }
    }

    fn toggle_dropdown(&mut self, which: Dropdown, cx: &mut Context<Self>) {
        self.dropdown = if self.dropdown == Some(which) {
            None
        } else {
            Some(which)
        };
        cx.notify();
    }

    // -- 描画 --

    fn render_header(&self, cx: &mut Context<Self>) -> AnyElement {
        let theme = theme(cx).clone();
        let usage = self
            .token_usage
            .as_ref()
            .map(format_token_usage)
            .unwrap_or_default();

        v_flex()
            .flex_none()
            .child(
                panel_header("codex", cx).child(
                    h_flex()
                        .gap(px(6.))
                        .child(icon(Icon::Sparkles, px(13.), theme.accent_tertiary))
                        .child(
                            div()
                                .text_size(px(10.5))
                                .text_color(theme.text_faint)
                                .child(usage),
                        ),
                ),
            )
            .child(
                h_flex()
                    .px(px(8.))
                    .pb(px(6.))
                    .gap(px(4.))
                    .flex_wrap()
                    .child(
                        self.render_dropdown_button(
                            Dropdown::Model,
                            self.selected_model
                                .clone()
                                .unwrap_or_else(|| "モデル自動".to_string()),
                            cx,
                        ),
                    )
                    .child(self.render_dropdown_button(
                        Dropdown::Approval,
                        approval_policy_label(self.approval_policy).to_string(),
                        cx,
                    ))
                    .child(self.render_dropdown_button(
                        Dropdown::Sandbox,
                        sandbox_policy_label(self.sandbox_policy).to_string(),
                        cx,
                    )),
            )
            .children(self.render_dropdown_list(cx))
            .into_any_element()
    }

    fn render_dropdown_button(
        &self,
        which: Dropdown,
        label: String,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let theme = theme(cx).clone();
        let open = self.dropdown == Some(which);
        let id: ElementId = match which {
            Dropdown::Model => "dd-model".into(),
            Dropdown::Approval => "dd-approval".into(),
            Dropdown::Sandbox => "dd-sandbox".into(),
        };
        h_flex()
            .id(id)
            .h(px(20.))
            .px(px(6.))
            .gap(px(3.))
            .rounded(px(4.))
            .border_1()
            .border_color(if open {
                theme.accent_tertiary
            } else {
                theme.border
            })
            .bg(if open {
                theme.bg_overlay
            } else {
                theme.bg_surface
            })
            .text_size(px(10.5))
            .text_color(theme.text_muted)
            .cursor_pointer()
            .hover(|s| s.text_color(theme.text))
            .child(truncate_middle(&label, 18))
            .child(icon(
                if open {
                    Icon::ChevronDown
                } else {
                    Icon::ChevronRight
                },
                px(9.),
                theme.text_faint,
            ))
            .on_click(cx.listener(move |this, _event, _window, cx| this.toggle_dropdown(which, cx)))
            .into_any_element()
    }

    /// 開いているドロップダウンの選択肢。
    ///
    /// 浮かせず、ヘッダの下に押し広げて出す。サイドバーは狭く、重ねると
    /// 下の会話を隠してしまうため。
    fn render_dropdown_list(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let theme = theme(cx).clone();
        let which = self.dropdown?;
        let rows: Vec<AnyElement> = match which {
            Dropdown::Model => {
                if self.models.is_empty() {
                    vec![
                        div()
                            .px(px(8.))
                            .py(px(4.))
                            .text_size(px(11.))
                            .text_color(theme.text_faint)
                            .child("モデル一覧を取得できませんでした")
                            .into_any_element(),
                    ]
                } else {
                    self.models
                        .iter()
                        .enumerate()
                        .map(|(index, model)| {
                            let selected = self.selected_model.as_deref() == Some(model.as_str());
                            let value = model.clone();
                            list_row(("model", index), selected, cx)
                                .child(model.clone())
                                .on_click(cx.listener(move |this, _e, _w, cx| {
                                    this.selected_model = Some(value.clone());
                                    this.dropdown = None;
                                    this.note_setting_change();
                                    cx.notify();
                                }))
                                .into_any_element()
                        })
                        .collect()
                }
            }
            Dropdown::Approval => [
                CodexApprovalPolicy::Untrusted,
                CodexApprovalPolicy::OnFailure,
                CodexApprovalPolicy::OnRequest,
                CodexApprovalPolicy::Never,
            ]
            .into_iter()
            .enumerate()
            .map(|(index, policy)| {
                list_row(("approval", index), self.approval_policy == policy, cx)
                    .child(approval_policy_label(policy))
                    .on_click(cx.listener(move |this, _e, _w, cx| {
                        this.approval_policy = policy;
                        this.dropdown = None;
                        this.note_setting_change();
                        cx.notify();
                    }))
                    .into_any_element()
            })
            .collect(),
            Dropdown::Sandbox => [
                CodexSandboxPolicy::ReadOnly,
                CodexSandboxPolicy::WorkspaceWrite,
                CodexSandboxPolicy::DangerFullAccess,
            ]
            .into_iter()
            .enumerate()
            .map(|(index, policy)| {
                list_row(("sandbox", index), self.sandbox_policy == policy, cx)
                    .child(sandbox_policy_label(policy))
                    .on_click(cx.listener(move |this, _e, _w, cx| {
                        this.sandbox_policy = policy;
                        this.dropdown = None;
                        this.note_setting_change();
                        cx.notify();
                    }))
                    .into_any_element()
            })
            .collect(),
        };

        Some(
            v_flex()
                .flex_none()
                .mx(px(8.))
                .mb(px(6.))
                .py(px(3.))
                .rounded(px(5.))
                .bg(theme.bg_overlay)
                .border_1()
                .border_color(theme.border)
                .children(rows)
                .into_any_element(),
        )
    }

    /// 未ログインの案内。Codex が使えない理由が判らないと詰まるので必ず出す。
    fn render_auth_banner(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let theme = theme(cx).clone();
        if self.logged_in != Some(false) {
            return None;
        }
        Some(
            v_flex()
                .flex_none()
                .mx(px(8.))
                .mb(px(6.))
                .p(px(8.))
                .gap(px(3.))
                .rounded(px(5.))
                .bg(theme.bg_overlay)
                .border_1()
                .border_color(theme.warning)
                .child(
                    h_flex()
                        .gap(px(5.))
                        .child(icon(Icon::Warning, px(12.), theme.warning))
                        .child(
                            div()
                                .text_size(px(11.5))
                                .text_color(theme.warning)
                                .child("Codex にログインしていません"),
                        ),
                )
                .child(
                    div()
                        .text_size(px(11.))
                        .text_color(theme.text_muted)
                        .child("端末で codex login を実行してください"),
                )
                .into_any_element(),
        )
    }

    fn render_messages(&self, cx: &mut Context<Self>) -> AnyElement {
        let theme = theme(cx).clone();
        if self.items.is_empty() {
            return v_flex()
                .flex_1()
                .items_center()
                .justify_center()
                .p(px(20.))
                .gap(px(8.))
                .child(icon(Icon::Sparkles, px(26.), theme.accent_tertiary))
                .child(
                    div()
                        .text_size(px(11.5))
                        .text_color(theme.text_faint)
                        .text_center()
                        .child("最初のメッセージを送ると会話が始まります"),
                )
                .into_any_element();
        }

        let rows: Vec<AnyElement> = self
            .items
            .iter()
            .enumerate()
            .map(|(index, item)| self.render_item(index, item, cx))
            .collect();

        v_flex()
            .id("codex-messages")
            .flex_1()
            // flex 子要素は既定で内容ぶんの高さを主張するため、下限を 0 にしないと
            // スクロールせずパネルを押し広げてしまう。
            .min_h(px(0.))
            .w_full()
            .overflow_y_scroll()
            .track_scroll(&self.scroll)
            .px(px(8.))
            .pb(px(6.))
            .gap(px(8.))
            .children(rows)
            .into_any_element()
    }

    fn render_item(&self, index: usize, item: &ChatItem, cx: &mut Context<Self>) -> AnyElement {
        match item {
            ChatItem::User(text) => self.render_user(text, cx),
            ChatItem::Assistant { text, .. } => self.render_assistant(index, text, cx),
            ChatItem::Reasoning {
                text, collapsed, ..
            } => self.render_reasoning(index, text, *collapsed, cx),
            ChatItem::Exec {
                command,
                cwd,
                output,
                exit_code,
                ..
            } => self.render_exec(command, cwd, output, *exit_code, cx),
            ChatItem::Patch { files } => self.render_patch(files, cx),
            ChatItem::Approval { request, decision } => {
                self.render_approval(index, request, *decision, cx)
            }
            ChatItem::Error(message) => self.render_error(message, cx),
            ChatItem::Notice(message) => self.render_notice(message, cx),
            ChatItem::Raw {
                method,
                payload,
                expanded,
            } => self.render_raw(index, method, payload, *expanded, cx),
        }
    }

    fn render_user(&self, text: &str, cx: &mut Context<Self>) -> AnyElement {
        let theme = theme(cx).clone();
        h_flex()
            .w_full()
            .justify_end()
            .child(
                div()
                    .max_w(relative(0.88))
                    .px(px(9.))
                    .py(px(6.))
                    .rounded(px(8.))
                    .bg(theme.bg_overlay)
                    .border_1()
                    .border_color(theme.border)
                    .text_size(px(12.))
                    .text_color(theme.text)
                    .child(text.to_string()),
            )
            .into_any_element()
    }

    fn render_assistant(&self, index: usize, text: &str, cx: &mut Context<Self>) -> AnyElement {
        let theme = theme(cx).clone();
        let blocks: Vec<AnyElement> = split_code_blocks(text)
            .into_iter()
            .enumerate()
            .map(|(part, segment)| match segment {
                MarkdownSegment::Text(body) => div()
                    .text_size(px(12.))
                    .text_color(theme.text)
                    .child(body)
                    .into_any_element(),
                MarkdownSegment::Code { language, body } => {
                    self.render_code_block(("code", index * 64 + part), language, body, cx)
                }
            })
            .collect();

        v_flex()
            .w_full()
            .gap(px(5.))
            .children(blocks)
            .into_any_element()
    }

    fn render_code_block(
        &self,
        id: impl Into<ElementId>,
        language: Option<String>,
        body: String,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let theme = theme(cx).clone();
        v_flex()
            .id(id)
            .w_full()
            .rounded(px(5.))
            .overflow_hidden()
            .bg(theme.bg_void)
            .border_1()
            .border_color(theme.border)
            .when_some(language, |el, language| {
                el.child(
                    div()
                        .px(px(7.))
                        .py(px(2.))
                        .text_size(px(9.5))
                        .text_color(theme.accent_tertiary)
                        .bg(theme.bg_surface)
                        .child(language),
                )
            })
            .child(
                div()
                    .px(px(7.))
                    .py(px(5.))
                    .font_family(MONO_FONT)
                    .text_size(px(11.))
                    .text_color(theme.text)
                    .child(body),
            )
            .into_any_element()
    }

    fn render_reasoning(
        &self,
        index: usize,
        text: &str,
        collapsed: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let theme = theme(cx).clone();
        v_flex()
            .w_full()
            .gap(px(2.))
            .child(
                h_flex()
                    .id(("reasoning", index))
                    .gap(px(4.))
                    .cursor_pointer()
                    .child(icon(
                        if collapsed {
                            Icon::ChevronRight
                        } else {
                            Icon::ChevronDown
                        },
                        px(10.),
                        theme.text_faint,
                    ))
                    .child(
                        div()
                            .text_size(px(10.5))
                            .text_color(theme.text_faint)
                            .child("推論"),
                    )
                    .on_click(cx.listener(move |this, _e, _w, cx| {
                        if let Some(ChatItem::Reasoning { collapsed, .. }) =
                            this.items.get_mut(index)
                        {
                            *collapsed = !*collapsed;
                            cx.notify();
                        }
                    })),
            )
            .when(!collapsed, |el| {
                el.child(
                    div()
                        .pl(px(14.))
                        .italic()
                        .text_size(px(11.))
                        .text_color(theme.text_faint)
                        .child(text.to_string()),
                )
            })
            .into_any_element()
    }

    fn render_exec(
        &self,
        command: &[String],
        cwd: &Path,
        output: &str,
        exit_code: Option<i32>,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let theme = theme(cx).clone();
        let status_color = match exit_code {
            Some(0) => theme.success,
            Some(_) => theme.error,
            None => theme.accent,
        };
        let status = match exit_code {
            Some(0) => "完了".to_string(),
            Some(code) => format!("終了コード {code}"),
            None => "実行中…".to_string(),
        };

        v_flex()
            .w_full()
            .rounded(px(5.))
            .overflow_hidden()
            .bg(theme.bg_void)
            .border_1()
            .border_color(if exit_code.is_some_and(|c| c != 0) {
                theme.error
            } else {
                theme.border
            })
            .child(
                h_flex()
                    .w_full()
                    .px(px(7.))
                    .py(px(4.))
                    .gap(px(5.))
                    .bg(theme.bg_surface)
                    .child(icon(Icon::Terminal, px(11.), status_color))
                    .child(
                        div()
                            .flex_1()
                            .min_w(px(0.))
                            .font_family(MONO_FONT)
                            .text_size(px(11.))
                            .text_color(theme.text)
                            .child(format_command(command)),
                    )
                    .child(
                        div()
                            .text_size(px(9.5))
                            .text_color(status_color)
                            .child(status),
                    ),
            )
            .child(
                div()
                    .px(px(7.))
                    .py(px(2.))
                    .text_size(px(9.5))
                    .text_color(theme.text_faint)
                    .child(truncate_middle(&cwd.display().to_string(), 42)),
            )
            .when(!output.trim().is_empty(), |el| {
                el.child(
                    div()
                        .px(px(7.))
                        .py(px(5.))
                        .font_family(MONO_FONT)
                        .text_size(px(10.5))
                        .text_color(theme.text_muted)
                        .child(output.trim_end().to_string()),
                )
            })
            .into_any_element()
    }

    fn render_patch(&self, files: &[PathBuf], cx: &mut Context<Self>) -> AnyElement {
        let theme = theme(cx).clone();
        v_flex()
            .w_full()
            .p(px(7.))
            .gap(px(3.))
            .rounded(px(5.))
            .bg(theme.bg_surface)
            .border_1()
            .border_color(theme.git_modified)
            .child(
                h_flex()
                    .gap(px(5.))
                    .child(icon(Icon::Edit, px(11.), theme.git_modified))
                    .child(
                        div()
                            .text_size(px(11.))
                            .text_color(theme.git_modified)
                            .child(format!("{} 件のファイルを変更", files.len())),
                    ),
            )
            .children(files.iter().map(|path| {
                div()
                    .font_family(MONO_FONT)
                    .text_size(px(10.5))
                    .text_color(theme.text_muted)
                    .child(truncate_middle(&path.display().to_string(), 44))
            }))
            .into_any_element()
    }

    /// 承認カード。ネオンの枠と余白で会話中のどの要素より目立たせる。
    ///
    /// 見落とすと Codex 側の処理が止まったままになるため、ここだけは主張を強くする。
    fn render_approval(
        &self,
        index: usize,
        request: &CodexApprovalRequest,
        decision: Option<CodexApprovalDecision>,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let theme = theme(cx).clone();
        let pending = decision.is_none();
        let detail_lines: Vec<AnyElement> = request
            .detail
            .lines()
            .take(60)
            .map(|line| {
                let color = match request.kind {
                    CodexApprovalKind::ApplyPatch => diff_line_color(line, &theme),
                    _ => theme.text,
                };
                div()
                    .font_family(MONO_FONT)
                    .text_size(px(10.5))
                    .text_color(color)
                    .child(line.to_string())
                    .into_any_element()
            })
            .collect();

        v_flex()
            .w_full()
            .p(px(9.))
            .gap(px(6.))
            .rounded(px(7.))
            .bg(theme.bg_overlay)
            .border_2()
            .border_color(if pending {
                theme.border_glow
            } else {
                theme.border
            })
            .child(
                h_flex()
                    .gap(px(5.))
                    .child(icon(
                        Icon::Warning,
                        px(13.),
                        if pending {
                            theme.accent_secondary
                        } else {
                            theme.text_faint
                        },
                    ))
                    .child(
                        div()
                            .text_size(px(11.5))
                            .font_weight(gpui::FontWeight::SEMIBOLD)
                            .text_color(if pending {
                                theme.accent_secondary
                            } else {
                                theme.text_faint
                            })
                            .child(approval_kind_label(request.kind)),
                    ),
            )
            .child(
                div()
                    .text_size(px(12.))
                    .text_color(theme.text)
                    .child(request.summary.clone()),
            )
            .when(!detail_lines.is_empty(), |el| {
                el.child(
                    v_flex()
                        .w_full()
                        .p(px(6.))
                        .rounded(px(5.))
                        .bg(theme.bg_void)
                        .children(detail_lines),
                )
            })
            .child(match decision {
                Some(decision) => h_flex()
                    .gap(px(5.))
                    .child(icon(Icon::Check, px(11.), theme.text_muted))
                    .child(
                        div()
                            .text_size(px(11.))
                            .text_color(theme.text_muted)
                            .child(decision_label(decision)),
                    )
                    .into_any_element(),
                None => h_flex()
                    .gap(px(5.))
                    .flex_wrap()
                    .child(self.render_approval_button(
                        index,
                        CodexApprovalDecision::Approve,
                        "許可",
                        theme.success,
                        cx,
                    ))
                    .child(self.render_approval_button(
                        index,
                        CodexApprovalDecision::ApproveForSession,
                        "常に許可",
                        theme.accent,
                        cx,
                    ))
                    .child(self.render_approval_button(
                        index,
                        CodexApprovalDecision::Deny,
                        "拒否",
                        theme.warning,
                        cx,
                    ))
                    .child(self.render_approval_button(
                        index,
                        CodexApprovalDecision::Abort,
                        "中止",
                        theme.error,
                        cx,
                    ))
                    .into_any_element(),
            })
            .into_any_element()
    }

    fn render_approval_button(
        &self,
        index: usize,
        decision: CodexApprovalDecision,
        label: &'static str,
        color: Hsla,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let theme = theme(cx).clone();
        let request_id = match self.items.get(index) {
            Some(ChatItem::Approval { request, .. }) => request.request_id.clone(),
            _ => return div().into_any_element(),
        };
        h_flex()
            .id(("approval-btn", index * 8 + decision_index(decision)))
            .h(px(24.))
            .px(px(9.))
            .justify_center()
            .rounded(px(5.))
            .border_1()
            .border_color(color)
            .text_size(px(11.))
            .text_color(color)
            .cursor_pointer()
            .hover(|s| s.bg(theme.bg_surface))
            .child(label)
            .on_click(cx.listener(move |this, _e, _w, cx| {
                this.respond_approval(request_id.clone(), decision, cx)
            }))
            .into_any_element()
    }

    fn render_error(&self, message: &str, cx: &mut Context<Self>) -> AnyElement {
        let theme = theme(cx).clone();
        h_flex()
            .w_full()
            .items_start()
            .gap(px(5.))
            .p(px(7.))
            .rounded(px(5.))
            .bg(theme.bg_surface)
            .border_1()
            .border_color(theme.error)
            .child(icon(Icon::Error, px(12.), theme.error))
            .child(
                div()
                    .flex_1()
                    .min_w(px(0.))
                    .text_size(px(11.5))
                    .text_color(theme.error)
                    .child(message.to_string()),
            )
            .into_any_element()
    }

    fn render_notice(&self, message: &str, cx: &mut Context<Self>) -> AnyElement {
        let theme = theme(cx).clone();
        h_flex()
            .w_full()
            .justify_center()
            .child(
                div()
                    .text_size(px(10.))
                    .text_color(theme.text_faint)
                    .child(message.to_string()),
            )
            .into_any_element()
    }

    fn render_raw(
        &self,
        index: usize,
        method: &str,
        payload: &str,
        expanded: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let theme = theme(cx).clone();
        v_flex()
            .w_full()
            .gap(px(2.))
            .child(
                h_flex()
                    .id(("raw", index))
                    .gap(px(4.))
                    .cursor_pointer()
                    .child(icon(
                        if expanded {
                            Icon::ChevronDown
                        } else {
                            Icon::ChevronRight
                        },
                        px(9.),
                        theme.text_faint,
                    ))
                    .child(
                        div()
                            .font_family(MONO_FONT)
                            .text_size(px(9.5))
                            .text_color(theme.text_faint)
                            .child(format!("raw: {method}")),
                    )
                    .on_click(cx.listener(move |this, _e, _w, cx| {
                        if let Some(ChatItem::Raw { expanded, .. }) = this.items.get_mut(index) {
                            *expanded = !*expanded;
                            cx.notify();
                        }
                    })),
            )
            .when(expanded, |el| {
                el.child(
                    div()
                        .pl(px(13.))
                        .font_family(MONO_FONT)
                        .text_size(px(9.5))
                        .text_color(theme.text_faint)
                        .child(payload.to_string()),
                )
            })
            .into_any_element()
    }

    fn render_composer(&self, cx: &mut Context<Self>) -> AnyElement {
        let theme = theme(cx).clone();
        let focused = self.input.read(cx).is_focused();
        let can_send = !self.input.read(cx).text().trim().is_empty();
        let waiting = has_pending_approval(&self.items);

        v_flex()
            .flex_none()
            .w_full()
            .p(px(8.))
            .gap(px(6.))
            .border_t_1()
            .border_color(theme.border)
            .bg(theme.bg_elevated)
            .when(waiting, |el| {
                // 承認カードが画面外へ流れても気づけるよう、入力欄の直上でも知らせる。
                el.child(
                    h_flex()
                        .w_full()
                        .gap(px(5.))
                        .px(px(7.))
                        .py(px(4.))
                        .rounded(px(5.))
                        .bg(theme.bg_overlay)
                        .border_1()
                        .border_color(theme.accent_secondary)
                        .child(icon(Icon::Warning, px(11.), theme.accent_secondary))
                        .child(
                            div()
                                .text_size(px(11.))
                                .text_color(theme.accent_secondary)
                                .child("承認待ちです"),
                        ),
                )
            })
            .child(
                div()
                    .w_full()
                    .px(px(7.))
                    .py(px(5.))
                    .rounded(px(6.))
                    .bg(theme.bg_surface)
                    .border_1()
                    .border_color(if focused {
                        theme.border_glow
                    } else {
                        theme.border
                    })
                    .overflow_hidden()
                    .text_size(metrics::UI_FONT_SIZE)
                    .line_height(px(
                        f32::from(metrics::UI_FONT_SIZE) * metrics::LINE_HEIGHT_RATIO
                    ))
                    .text_color(theme.text)
                    .cursor(gpui::CursorStyle::IBeam)
                    // 枠の余白を押しても欄へ入れるようにする。
                    .on_mouse_down(MouseButton::Left, {
                        let input = self.input.clone();
                        move |_, window, cx| input.read(cx).focus(window)
                    })
                    .child(self.input.clone()),
            )
            .child(
                h_flex()
                    .w_full()
                    .justify_between()
                    .child(
                        div()
                            .text_size(px(9.5))
                            .text_color(theme.text_faint)
                            .child(format!(
                                "{} 送信 / {} 改行",
                                format_keystroke("enter"),
                                format_keystroke("shift-enter")
                            )),
                    )
                    .child(if self.running {
                        h_flex()
                            .id("codex-stop")
                            .h(px(24.))
                            .px(px(9.))
                            .gap(px(4.))
                            .justify_center()
                            .rounded(px(5.))
                            .border_1()
                            .border_color(theme.error)
                            .text_size(px(11.))
                            .text_color(theme.error)
                            .cursor_pointer()
                            .hover(|s| s.bg(theme.bg_overlay))
                            .child(icon(Icon::Stop, px(10.), theme.error))
                            .child("停止")
                            .on_click(cx.listener(|this, _e, _w, cx| this.interrupt(cx)))
                            .into_any_element()
                    } else {
                        h_flex()
                            .id("codex-send")
                            .h(px(24.))
                            .px(px(10.))
                            .gap(px(4.))
                            .justify_center()
                            .rounded(px(5.))
                            .text_size(px(11.))
                            .when(can_send, |el| {
                                el.bg(theme.accent_tertiary)
                                    .text_color(theme.text_inverse)
                                    .cursor_pointer()
                                    .hover(|s| s.bg(theme.accent))
                            })
                            .when(!can_send, |el| {
                                el.bg(theme.bg_overlay).text_color(theme.text_faint)
                            })
                            .child(icon(
                                Icon::Send,
                                px(10.),
                                if can_send {
                                    theme.text_inverse
                                } else {
                                    theme.text_faint
                                },
                            ))
                            .child("送信")
                            .on_click(cx.listener(|this, _e, _w, cx| this.submit(cx)))
                            .into_any_element()
                    }),
            )
            .into_any_element()
    }
}

/// diff 1 行の色。追加は緑、削除は赤、ヘッダは薄く。
fn diff_line_color(line: &str, theme: &crate::theme::Theme) -> Hsla {
    if line.starts_with("+++") || line.starts_with("---") || line.starts_with("@@") {
        theme.text_faint
    } else if line.starts_with('+') {
        theme.git_added
    } else if line.starts_with('-') {
        theme.git_deleted
    } else {
        theme.text_muted
    }
}

fn decision_index(decision: CodexApprovalDecision) -> usize {
    match decision {
        CodexApprovalDecision::Approve => 0,
        CodexApprovalDecision::ApproveForSession => 1,
        CodexApprovalDecision::Deny => 2,
        CodexApprovalDecision::Abort => 3,
    }
}

impl Render for CodexView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.bootstrap(cx);
        let theme = theme(cx).clone();
        let header = self.render_header(cx);
        let banner = self.render_auth_banner(cx);
        let messages = self.render_messages(cx);
        let composer = self.render_composer(cx);

        v_flex()
            .size_full()
            .overflow_hidden()
            .bg(theme.bg_elevated)
            // パネルの上端にバイオレットの細い線を敷き、他のサイドバーと区別する。
            .child(
                div()
                    .h(px(1.))
                    .w_full()
                    .flex_none()
                    .bg(theme.accent_tertiary),
            )
            .child(header)
            .children(banner)
            .child(messages)
            .child(composer)
    }
}

// ---------------------------------------------------------------------------
// テスト
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn approval(request_id: &str) -> CodexApprovalRequest {
        CodexApprovalRequest {
            request_id: request_id.to_string(),
            kind: CodexApprovalKind::ExecCommand,
            summary: "rm -rf /tmp/x を実行します".into(),
            detail: "rm -rf /tmp/x".into(),
        }
    }

    #[test]
    fn コードブロックを本文から切り分ける() {
        let segments = split_code_blocks("説明です\n```rust\nfn main() {}\n```\nおわり");
        assert_eq!(
            segments,
            vec![
                MarkdownSegment::Text("説明です".into()),
                MarkdownSegment::Code {
                    language: Some("rust".into()),
                    body: "fn main() {}".into(),
                },
                MarkdownSegment::Text("おわり".into()),
            ]
        );
    }

    #[test]
    fn 言語指定のないコードブロック() {
        let segments = split_code_blocks("```\nls -la\n```");
        assert_eq!(
            segments,
            vec![MarkdownSegment::Code {
                language: None,
                body: "ls -la".into(),
            }]
        );
    }

    #[test]
    fn 閉じていないフェンスは末尾までコードとして扱う() {
        // ストリーミング途中の応答。捨てると書きかけのコードが画面から消える。
        let segments = split_code_blocks("途中です\n```py\nprint(1)");
        assert_eq!(
            segments,
            vec![
                MarkdownSegment::Text("途中です".into()),
                MarkdownSegment::Code {
                    language: Some("py".into()),
                    body: "print(1)".into(),
                },
            ]
        );
    }

    #[test]
    fn コードブロックが無ければ本文だけ返す() {
        assert_eq!(
            split_code_blocks("ただの文\n2 行目"),
            vec![MarkdownSegment::Text("ただの文\n2 行目".into())]
        );
    }

    #[test]
    fn 空文字列は断片を生まない() {
        assert!(split_code_blocks("").is_empty());
        assert!(split_code_blocks("\n\n").is_empty());
    }

    #[test]
    fn コードブロック内の空行と字下げを保つ() {
        let segments = split_code_blocks("```\na\n\n    b\n```");
        assert_eq!(
            segments,
            vec![MarkdownSegment::Code {
                language: None,
                body: "a\n\n    b".into(),
            }]
        );
    }

    #[test]
    fn 増分は同じ吹き出しに追記される() {
        let mut items = Vec::new();
        push_assistant_delta(&mut items, "こん");
        push_assistant_delta(&mut items, "にちは");
        assert_eq!(items.len(), 1);
        assert_eq!(
            items[0],
            ChatItem::Assistant {
                text: "こんにちは".into(),
                sealed: false
            }
        );
    }

    #[test]
    fn 確定メッセージは増分を全文で置き換える() {
        let mut items = Vec::new();
        push_assistant_delta(&mut items, "こん");
        finish_assistant_message(&mut items, "こんにちは".into());
        assert_eq!(items.len(), 1, "吹き出しは増えない");
        assert_eq!(
            items[0],
            ChatItem::Assistant {
                text: "こんにちは".into(),
                sealed: true
            }
        );
    }

    fn raw() -> ChatItem {
        ChatItem::Raw {
            method: "item/started".into(),
            payload: "{}".into(),
            expanded: false,
        }
    }

    #[test]
    fn 増分の合間に生イベントが挟まっても同じ吹き出しに続く() {
        // バックエンドは翻訳できない通知を Raw で素通しする。末尾だけを見ていると
        // ここで吹き出しが割れる。
        let mut items = Vec::new();
        push_assistant_delta(&mut items, "こん");
        items.push(raw());
        push_assistant_delta(&mut items, "にちは");
        assert_eq!(items.len(), 2, "吹き出しは割れない");
        assert_eq!(
            items[0],
            ChatItem::Assistant {
                text: "こんにちは".into(),
                sealed: false
            }
        );
    }

    #[test]
    fn 生イベントを挟んだ確定メッセージも二重に出ない() {
        let mut items = Vec::new();
        push_assistant_delta(&mut items, "こん");
        items.push(raw());
        finish_assistant_message(&mut items, "こんにちは".into());
        assert_eq!(items.len(), 2, "確定文が別の吹き出しになっていない");
        assert_eq!(
            items[0],
            ChatItem::Assistant {
                text: "こんにちは".into(),
                sealed: true
            }
        );
    }

    #[test]
    fn 推論も生イベントを跨いで続く() {
        let mut items = Vec::new();
        push_reasoning_delta(&mut items, "考え");
        items.push(raw());
        push_reasoning_delta(&mut items, "中");
        assert_eq!(items.len(), 2);
        assert_eq!(
            items[0],
            ChatItem::Reasoning {
                text: "考え中".into(),
                sealed: false,
                collapsed: false
            }
        );
    }

    #[test]
    fn 封をされていない前ターンの吹き出しには書き足さない() {
        // 中断でターンが終わると封がされないまま残る。次のターンの応答が
        // そこへ吸い込まれると、送信した順に会話が読めなくなる。
        let mut items = vec![
            ChatItem::Assistant {
                text: "中断された応答".into(),
                sealed: false,
            },
            ChatItem::User("次の質問".into()),
        ];
        push_assistant_delta(&mut items, "新しい応答");
        assert_eq!(items.len(), 3);
        assert_eq!(
            items[0],
            ChatItem::Assistant {
                text: "中断された応答".into(),
                sealed: false
            }
        );
        assert_eq!(
            items[2],
            ChatItem::Assistant {
                text: "新しい応答".into(),
                sealed: false
            }
        );
    }

    #[test]
    fn ターン完了後の増分は新しい吹き出しになる() {
        let mut items = Vec::new();
        push_assistant_delta(&mut items, "1 回目");
        seal_turn(&mut items);
        push_assistant_delta(&mut items, "2 回目");
        assert_eq!(items.len(), 2);
        assert_eq!(
            items[1],
            ChatItem::Assistant {
                text: "2 回目".into(),
                sealed: false
            }
        );
    }

    #[test]
    fn ターン完了で推論が畳まれる() {
        let mut items = Vec::new();
        push_reasoning_delta(&mut items, "考え中");
        seal_turn(&mut items);
        assert_eq!(
            items[0],
            ChatItem::Reasoning {
                text: "考え中".into(),
                sealed: true,
                collapsed: true
            }
        );
    }

    #[test]
    fn コマンド出力は_call_id_ごとに振り分けられる() {
        let mut items = Vec::new();
        begin_exec(
            &mut items,
            "a".into(),
            vec!["ls".into()],
            PathBuf::from("/"),
        );
        begin_exec(
            &mut items,
            "b".into(),
            vec!["pwd".into()],
            PathBuf::from("/"),
        );
        append_exec_output(&mut items, "a", "file\n");
        append_exec_output(&mut items, "b", "/\n");
        finish_exec(&mut items, "a", 0);
        finish_exec(&mut items, "b", 1);

        assert_eq!(
            items[0],
            ChatItem::Exec {
                call_id: "a".into(),
                command: vec!["ls".into()],
                cwd: PathBuf::from("/"),
                output: "file\n".into(),
                exit_code: Some(0),
            }
        );
        assert_eq!(
            items[1],
            ChatItem::Exec {
                call_id: "b".into(),
                command: vec!["pwd".into()],
                cwd: PathBuf::from("/"),
                output: "/\n".into(),
                exit_code: Some(1),
            }
        );
    }

    #[test]
    fn 未知の_call_id_の出力は捨てる() {
        let mut items = vec![ChatItem::Notice("x".into())];
        append_exec_output(&mut items, "none", "出力");
        assert_eq!(items, vec![ChatItem::Notice("x".into())]);
    }

    #[test]
    fn 承認は一度だけ応答できる() {
        let mut items = vec![ChatItem::Approval {
            request: approval("r1"),
            decision: None,
        }];
        assert!(has_pending_approval(&items));
        assert!(resolve_approval(
            &mut items,
            "r1",
            CodexApprovalDecision::Approve
        ));
        assert!(!resolve_approval(
            &mut items,
            "r1",
            CodexApprovalDecision::Deny
        ));
        assert!(!has_pending_approval(&items));
        assert_eq!(
            items[0],
            ChatItem::Approval {
                request: approval("r1"),
                decision: Some(CodexApprovalDecision::Approve),
            }
        );
    }

    #[test]
    fn 存在しない承認への応答は無視される() {
        let mut items = Vec::new();
        assert!(!resolve_approval(
            &mut items,
            "none",
            CodexApprovalDecision::Approve
        ));
    }

    #[test]
    fn コマンドは空白を含む引数を引用する() {
        assert_eq!(
            format_command(&[
                "git".into(),
                "commit".into(),
                "-m".into(),
                "初回 コミット".into()
            ]),
            "git commit -m \"初回 コミット\""
        );
    }

    #[test]
    fn トークン使用量を短く表す() {
        let usage = CodexTokenUsage {
            input_tokens: 1500,
            cached_input_tokens: 0,
            output_tokens: 250,
            total_tokens: 1_750_000,
        };
        assert_eq!(format_token_usage(&usage), "↑1.5k ↓250 計1.8M");
    }

    #[test]
    fn diff_の追加行と削除行で色が変わる() {
        let theme = crate::theme::Theme::cyber_cosmic();
        assert_eq!(diff_line_color("+ added", &theme), theme.git_added);
        assert_eq!(diff_line_color("- removed", &theme), theme.git_deleted);
        assert_eq!(diff_line_color("@@ -1 +1 @@", &theme), theme.text_faint);
        assert_eq!(diff_line_color(" context", &theme), theme.text_muted);
    }
}
