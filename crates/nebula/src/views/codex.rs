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
//!
//! ファイルはこの設計と対応して 3 つに分かれている。
//! [`chat`] が要点 3 の状態遷移、[`format`] が表示用の文字列整形、[`render`] が
//! 実際の描画。ここ (ルート) には [`CodexView`] 本体・イベント配線・バックエンドへの
//! 要求・ヘッダのドロップダウン UI だけを残す。

mod chat;
mod format;
mod render;

use crate::assets::Icon;
use crate::ipc_client::BackendClient;
use crate::theme::theme;
use crate::ui::{
    TextInput, TextInputEvent, h_flex, icon, list_row, panel_header, truncate_middle, v_flex,
};
use chat::ChatItem;
use format::{approval_policy_label, format_token_usage, sandbox_policy_label};
use gpui::prelude::*;
use gpui::{AnyElement, Context, ElementId, Entity, ScrollHandle, Subscription, Window, div, px};
use nebula_protocol::{
    CodexApprovalDecision, CodexApprovalPolicy, CodexConversationId, CodexEvent,
    CodexSandboxPolicy, CodexSessionSpec, CodexTokenUsage, Event, Request, Response, WorkspaceInfo,
};

/// 入力欄が伸びる上限 (折り返し後の行数)。これを超えたぶんはカーソル追従でスクロールする。
const MAX_INPUT_ROWS: usize = 8;

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
        if let Some(conversation) = chat::conversation_of(event)
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
                chat::push_assistant_delta(&mut self.items, delta)
            }
            CodexEvent::AgentMessage { text, .. } => {
                chat::finish_assistant_message(&mut self.items, text.clone())
            }
            CodexEvent::ReasoningDelta { delta, .. } => {
                chat::push_reasoning_delta(&mut self.items, delta)
            }
            CodexEvent::ExecBegin {
                call_id,
                command,
                cwd,
                ..
            } => chat::begin_exec(
                &mut self.items,
                call_id.clone(),
                command.clone(),
                cwd.clone(),
            ),
            CodexEvent::ExecOutput { call_id, chunk, .. } => {
                chat::append_exec_output(&mut self.items, call_id, chunk)
            }
            CodexEvent::ExecEnd {
                call_id, exit_code, ..
            } => chat::finish_exec(&mut self.items, call_id, *exit_code),
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
                chat::seal_turn(&mut self.items);
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
        if !chat::resolve_approval(&mut self.items, &request_id, decision) {
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

    // -- 描画 (ヘッダとドロップダウン。会話本文の描画は render.rs) --

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
