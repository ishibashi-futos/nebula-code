//! アプリケーションシェル。
//!
//! 画面全体の骨格 (アクティビティバー / サイドバー / エディタ領域 / パネル / ステータスバー)
//! を組み立て、子ビューから上がってくるイベントを配線する。
//!
//! バックエンドへの接続はウィンドウを出した **後** に非同期で行う。接続を待ってから
//! 描画すると、冷間起動の初回フレームがプロセス起動 2 つぶん遅れてしまうため。

use crate::actions;
use crate::assets::Icon;
use crate::ipc_client::BackendClient;
use crate::session::Session;
use crate::theme::{metrics, theme};
use crate::ui::{activity_item, h_flex, icon, icon_button, simple_tooltip, tooltip_text, v_flex};
use crate::views::codex::CodexView;
use crate::views::editor::{EditorArea, EditorAreaEvent};
use crate::views::explorer::{ExplorerEvent, ExplorerView};
use crate::views::git::{GitEvent, GitView};
use crate::views::palette::{CommandPalette, PaletteEvent, PaletteMode};
use crate::views::problems::ProblemsView;
use crate::views::search::{SearchEvent, SearchView};
use crate::views::terminal::TerminalView;
use gpui::prelude::*;
use gpui::{
    App, Context, Entity, FocusHandle, Focusable, Pixels, Subscription, Task, Window, div, px,
};
use nebula_protocol::{
    DetectedTools, Event, HandshakeInfo, NotificationLevel, Request, Response, WorkspaceId,
    WorkspaceInfo,
};
use std::path::PathBuf;

/// サイドバーに表示する内容。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SidebarTab {
    Explorer,
    Search,
    Git,
    Codex,
}

/// 下部パネルに表示する内容。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PanelTab {
    Terminal,
    Problems,
}

/// サイドバー幅のドラッグ操作を表す標識。
#[derive(Clone)]
struct SidebarResize;

/// 下部パネル高さのドラッグ操作を表す標識。
#[derive(Clone)]
struct PanelResize;

/// ドラッグ中に追従する見えない要素。
///
/// gpui はドラッグ開始時に「掴んでいるもの」の描画を要求するが、
/// パネルの境界を動かすだけなので何も描かない。
struct DragGhost;

impl Render for DragGhost {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        div()
    }
}

/// バックエンドとの接続状態。
#[derive(Debug, Clone)]
pub enum ConnectionState {
    Connecting,
    Ready(Box<HandshakeInfo>),
    Failed(String),
}

pub struct NebulaApp {
    pub client: Option<BackendClient>,
    pub connection: ConnectionState,
    pub tools: DetectedTools,
    pub workspaces: Vec<WorkspaceInfo>,
    pub active_workspace: Option<WorkspaceId>,

    sidebar: SidebarTab,
    sidebar_visible: bool,
    sidebar_width: Pixels,
    panel: PanelTab,
    panel_visible: bool,
    panel_height: Pixels,
    status_message: Option<(NotificationLevel, String)>,
    /// 通知を消すためのタイマー。新しい通知で差し替わり、前のものは取り消される。
    _status_timer: Option<Task<()>>,

    explorer: Entity<ExplorerView>,
    search: Entity<SearchView>,
    git: Entity<GitView>,
    codex: Entity<CodexView>,
    terminal: Entity<TerminalView>,
    problems: Entity<ProblemsView>,
    editor_area: Entity<EditorArea>,
    palette: Entity<CommandPalette>,

    focus_handle: FocusHandle,
    /// 最初の描画を終えたか。起動時間の計測に 1 度だけ使う。
    first_render_done: bool,
    /// 復元中に選択し直すべきワークスペースのルート。
    restoring_active: Option<PathBuf>,
    /// 購読を保持しておかないと即座に解除されてしまう。
    _subscriptions: Vec<Subscription>,
    /// 接続タスク。落とすと接続処理が中断されるため保持する。
    _connect_task: Option<Task<()>>,
    /// イベント受信ループ。
    _event_task: Option<Task<()>>,
}

/// `open_folder` が応答を受け取った際、一覧にどう反映しアクティブをどこにするか決める。
///
/// 戻り値は `(一覧に追加するか, アクティブにするか)`。
///
/// - `already_registered`: 同じ `WorkspaceId` が既に `workspaces` に入っているか。
///   真なら追加しない (バックエンド側で同じルートは重複排除され同じ id が返るため、
///   ここで弾かないと同じワークスペースがアクティビティバーに何度も並んでしまう)。
/// - `is_restore_target`: セッション復元中で、これが前回アクティブだったフォルダか。
///   真なら他の条件によらず必ずアクティブにする。
/// - `activate_requested`: ユーザー操作 (追加ボタン・⌘O・起動引数) 由来の要求か。
///   真なら (既に何か開いていても) 常にアクティブにする — これが直すバグの本体で、
///   従来は `has_active` が真だと無視されて何も切り替わらなかった。
/// - `has_active`: 現在どれかアクティブなワークスペースがあるか。まだ何も無ければ、
///   復元/ユーザー操作を問わずその 1 つ目を自動的にアクティブにする。
fn decide_workspace_update(
    already_registered: bool,
    is_restore_target: bool,
    activate_requested: bool,
    has_active: bool,
) -> (bool, bool) {
    let should_push = !already_registered;
    let should_activate = is_restore_target || activate_requested || !has_active;
    (should_push, should_activate)
}

impl NebulaApp {
    pub fn new(initial_folder: Option<PathBuf>, cx: &mut Context<Self>) -> Self {
        let explorer = cx.new(ExplorerView::new);
        let search = cx.new(SearchView::new);
        let git = cx.new(GitView::new);
        let codex = cx.new(CodexView::new);
        let terminal = cx.new(TerminalView::new);
        let problems = cx.new(ProblemsView::new);
        let editor_area = cx.new(EditorArea::new);
        let palette = cx.new(CommandPalette::new);

        let subscriptions = vec![
            cx.subscribe(&explorer, Self::on_explorer_event),
            cx.subscribe(&search, Self::on_search_event),
            cx.subscribe(&git, Self::on_git_event),
            cx.subscribe(&palette, Self::on_palette_event),
            cx.subscribe(&editor_area, Self::on_editor_event),
        ];

        let mut app = Self {
            client: None,
            connection: ConnectionState::Connecting,
            tools: DetectedTools::default(),
            workspaces: Vec::new(),
            active_workspace: None,
            sidebar: SidebarTab::Explorer,
            sidebar_visible: true,
            sidebar_width: metrics::SIDEBAR_DEFAULT_WIDTH,
            panel: PanelTab::Terminal,
            panel_visible: false,
            panel_height: metrics::PANEL_DEFAULT_HEIGHT,
            status_message: None,
            _status_timer: None,
            explorer,
            search,
            git,
            codex,
            terminal,
            problems,
            editor_area,
            palette,
            focus_handle: cx.focus_handle(),
            first_render_done: false,
            restoring_active: None,
            _subscriptions: subscriptions,
            _connect_task: None,
            _event_task: None,
        };
        app.start_connecting(initial_folder, cx);
        app
    }

    /// 別スレッドでバックエンドに接続し、完了したら状態を更新する。
    fn start_connecting(&mut self, initial_folder: Option<PathBuf>, cx: &mut Context<Self>) {
        let task = cx.spawn(async move |this, cx| {
            // 接続はブロッキングなので専用スレッドへ逃がす。
            let connected = cx
                .background_executor()
                .spawn(async move { BackendClient::connect_blocking() })
                .await;

            let client = match connected {
                Ok(client) => client,
                Err(e) => {
                    this.update(cx, |this, cx| {
                        this.connection = ConnectionState::Failed(e.to_string());
                        cx.notify();
                    })
                    .ok();
                    return;
                }
            };

            let handshake = match client.handshake().await {
                Ok(info) => info,
                Err(e) => {
                    this.update(cx, |this, cx| {
                        this.connection = ConnectionState::Failed(e.to_string());
                        cx.notify();
                    })
                    .ok();
                    return;
                }
            };

            this.update(cx, |this, cx| {
                this.tools = handshake.tools.clone();
                this.connection = ConnectionState::Ready(Box::new(handshake));
                this.client = Some(client.clone());
                this.distribute_client(cx);
                this.start_event_loop(client.clone(), cx);
                cx.notify();
            })
            .ok();

            // 引数でフォルダを渡されていればそれを優先し、無ければ前回の続きを開く。
            // 復元はウィンドウを出した後なので、最初のフレームには影響しない。
            match initial_folder {
                Some(folder) => {
                    this.update(cx, |this, cx| this.open_folder(folder, true, cx))
                        .ok();
                }
                None => {
                    this.update(cx, |this, cx| this.restore_session(cx)).ok();
                }
            }
        });
        self._connect_task = Some(task);
    }

    /// 接続を各子ビューに配る。
    fn distribute_client(&mut self, cx: &mut Context<Self>) {
        let Some(client) = self.client.clone() else {
            return;
        };
        self.explorer
            .update(cx, |v, cx| v.set_client(client.clone(), cx));
        self.search
            .update(cx, |v, cx| v.set_client(client.clone(), cx));
        self.git
            .update(cx, |v, cx| v.set_client(client.clone(), cx));
        self.codex
            .update(cx, |v, cx| v.set_client(client.clone(), cx));
        self.terminal
            .update(cx, |v, cx| v.set_client(client.clone(), cx));
        self.editor_area
            .update(cx, |v, cx| v.set_client(client.clone(), cx));
        self.palette.update(cx, |v, cx| v.set_client(client, cx));
    }

    /// バックエンドからのイベントを受け取り、関係する子ビューへ配る。
    fn start_event_loop(&mut self, client: BackendClient, cx: &mut Context<Self>) {
        let events = client.events();
        let task = cx.spawn(async move |this, cx| {
            while let Ok(event) = events.recv().await {
                if this
                    .update(cx, |this, cx| this.handle_backend_event(event, cx))
                    .is_err()
                {
                    break;
                }
            }
        });
        self._event_task = Some(task);
    }

    fn handle_backend_event(&mut self, event: Event, cx: &mut Context<Self>) {
        match event {
            Event::SearchMatches { .. } | Event::SearchFinished { .. } => {
                self.search.update(cx, |v, cx| v.handle_event(&event, cx));
            }
            Event::FilesChanged { .. } => {
                self.explorer.update(cx, |v, cx| v.handle_event(&event, cx));
            }
            Event::GitStatusChanged { .. } => {
                self.git.update(cx, |v, cx| v.handle_event(&event, cx));
                self.editor_area
                    .update(cx, |v, cx| v.handle_event(&event, cx));
                self.explorer.update(cx, |v, cx| v.handle_event(&event, cx));
            }
            Event::Diagnostics { .. } => {
                self.problems.update(cx, |v, cx| v.handle_event(&event, cx));
                self.editor_area
                    .update(cx, |v, cx| v.handle_event(&event, cx));
            }
            Event::TerminalUpdated(_) | Event::TerminalExited { .. } => {
                self.terminal.update(cx, |v, cx| v.handle_event(&event, cx));
            }
            Event::Codex(_) => {
                self.codex.update(cx, |v, cx| v.handle_event(&event, cx));
            }
            Event::ToolsDetected { tools } => {
                self.tools = tools;
                cx.notify();
            }
            Event::Notification { level, ref message } => {
                self.notify_status(level, message.clone(), cx);
            }
            Event::BufferFileChanged { .. }
            | Event::HighlightsInvalidated { .. }
            | Event::LspStatusChanged { .. }
            | Event::LspMessage { .. } => {
                self.editor_area
                    .update(cx, |v, cx| v.handle_event(&event, cx));
            }
        }
    }

    /// 前回開いていたフォルダを開き直す。
    fn restore_session(&mut self, cx: &mut Context<Self>) {
        let mut session = Session::load();
        session.prune_missing();
        if let Some(width) = session.sidebar_width {
            self.sidebar_width = px(width);
        }
        if let Some(height) = session.panel_height {
            self.panel_height = px(height);
        }
        self.sidebar_visible = session.sidebar_visible;
        self.explorer
            .update(cx, |v, cx| v.set_show_hidden(session.explorer_show_hidden, cx));
        self.restoring_active = session.active.clone();
        for root in session.workspaces {
            // セッション復元由来なので activate=false: 復元対象のフォルダだけを
            // アクティブにし、順に開くたびに切り替わって最後のものが選ばれる事故を避ける。
            self.open_folder(root, false, cx);
        }
        cx.notify();
    }

    /// 現在の状態をセッションとして書き出す。
    fn save_session(&self, cx: &App) {
        let session = Session {
            workspaces: self.workspaces.iter().map(|w| w.root.clone()).collect(),
            active: self
                .active_workspace
                .and_then(|id| self.workspaces.iter().find(|w| w.id == id))
                .map(|w| w.root.clone()),
            sidebar_width: Some(f32::from(self.sidebar_width)),
            panel_height: Some(f32::from(self.panel_height)),
            sidebar_visible: self.sidebar_visible,
            explorer_show_hidden: self.explorer.read(cx).show_hidden(),
        };
        session.save();
    }

    /// フォルダをワークスペースとして開く。
    ///
    /// `activate` はユーザー操作 (追加ボタン・⌘O・起動時の引数指定) 由来なら常に `true` を渡し、
    /// 開いた直後にそのワークスペースへ必ず切り替える。`restore_session` からの呼び出しだけ
    /// `false` を渡し、複数フォルダを順に開いても最後に開いたものへ勝手に切り替わらないという
    /// 既存の挙動 (`is_restore_target` によってのみアクティブが決まる) を保つ。
    pub fn open_folder(&mut self, folder: PathBuf, activate: bool, cx: &mut Context<Self>) {
        let Some(client) = self.client.clone() else {
            self.notify_status(
                NotificationLevel::Error,
                "バックエンドに接続していません".to_string(),
                cx,
            );
            return;
        };
        cx.spawn(async move |this, cx| {
            let result = client
                .request(Request::OpenWorkspace { root: folder })
                .await;
            this.update(cx, |this, cx| match result {
                Ok(Response::Workspace(info)) => {
                    let already_registered = this.workspaces.iter().any(|w| w.id == info.id);
                    // 復元中は、前回選択していたフォルダが開かれるまで選択を移さない。
                    // 順に開くたびに切り替わると、最後に開いたものが選ばれてしまう。
                    let is_restore_target = this
                        .restoring_active
                        .as_ref()
                        .is_some_and(|root| *root == info.root);
                    if is_restore_target {
                        this.restoring_active = None;
                    }
                    let (should_push, should_activate) = decide_workspace_update(
                        already_registered,
                        is_restore_target,
                        activate,
                        this.active_workspace.is_some(),
                    );
                    if should_push {
                        this.workspaces.push(info.clone());
                    }
                    if should_activate {
                        this.activate_workspace(info.id, cx);
                    }
                    this.save_session(cx);
                    cx.notify();
                }
                Ok(_) => {}
                Err(e) => {
                    this.notify_status(
                        NotificationLevel::Error,
                        format!("フォルダを開けません: {e}"),
                        cx,
                    );
                }
            })
            .ok();
        })
        .detach();
    }

    pub fn activate_workspace(&mut self, id: WorkspaceId, cx: &mut Context<Self>) {
        self.active_workspace = Some(id);
        self.save_session(cx);
        let Some(info) = self.workspaces.iter().find(|w| w.id == id).cloned() else {
            return;
        };
        self.explorer
            .update(cx, |v, cx| v.set_workspace(info.clone(), cx));
        self.search
            .update(cx, |v, cx| v.set_workspace(info.clone(), cx));
        self.git
            .update(cx, |v, cx| v.set_workspace(info.clone(), cx));
        self.codex
            .update(cx, |v, cx| v.set_workspace(info.clone(), cx));
        self.terminal
            .update(cx, |v, cx| v.set_workspace(info.clone(), cx));
        self.editor_area
            .update(cx, |v, cx| v.set_workspace(info, cx));
        cx.notify();
    }

    // -- 子ビューからのイベント --

    fn on_explorer_event(
        &mut self,
        _entity: Entity<ExplorerView>,
        event: &ExplorerEvent,
        cx: &mut Context<Self>,
    ) {
        match event {
            ExplorerEvent::OpenFile(path) => self.open_file(path.clone(), None, cx),
            ExplorerEvent::Notify(level, message) => {
                self.notify_status(*level, message.clone(), cx);
            }
            // フィールド自体はエクスプローラーが持つが、書き出し先のセッションを
            // 知っているのはシェル側だけなので、保存はここで行う。
            ExplorerEvent::HiddenToggled => self.save_session(cx),
        }
    }

    fn on_search_event(
        &mut self,
        _entity: Entity<SearchView>,
        event: &SearchEvent,
        cx: &mut Context<Self>,
    ) {
        match event {
            SearchEvent::OpenLocation { path, position } => {
                self.open_file(path.clone(), Some(*position), cx)
            }
        }
    }

    fn on_git_event(&mut self, _entity: Entity<GitView>, event: &GitEvent, cx: &mut Context<Self>) {
        match event {
            GitEvent::OpenFile(path) => self.open_file(path.clone(), None, cx),
            GitEvent::Notify(level, message) => {
                self.notify_status(*level, message.clone(), cx);
            }
        }
    }

    fn on_editor_event(
        &mut self,
        _entity: Entity<EditorArea>,
        event: &EditorAreaEvent,
        cx: &mut Context<Self>,
    ) {
        match event {
            EditorAreaEvent::Notify(level, message) => {
                self.notify_status(*level, message.clone(), cx);
            }
            EditorAreaEvent::OpenFile { path, position } => {
                self.open_file(path.clone(), *position, cx)
            }
        }
    }

    fn on_palette_event(
        &mut self,
        _entity: Entity<CommandPalette>,
        event: &PaletteEvent,
        cx: &mut Context<Self>,
    ) {
        match event {
            PaletteEvent::OpenFile(path) => self.open_file(path.clone(), None, cx),
            PaletteEvent::RunCommand(command) => self.run_command(command.clone(), cx),
            PaletteEvent::Dismissed => cx.notify(),
        }
    }

    fn open_file(
        &mut self,
        path: PathBuf,
        position: Option<nebula_protocol::Position>,
        cx: &mut Context<Self>,
    ) {
        let Some(workspace) = self.active_workspace else {
            return;
        };
        self.editor_area
            .update(cx, |area, cx| area.open_file(workspace, path, position, cx));
    }

    fn run_command(&mut self, command: String, cx: &mut Context<Self>) {
        match command.as_str() {
            "view.explorer" => self.set_sidebar(SidebarTab::Explorer, cx),
            "view.search" => self.set_sidebar(SidebarTab::Search, cx),
            "view.git" => self.set_sidebar(SidebarTab::Git, cx),
            "view.codex" => self.set_sidebar(SidebarTab::Codex, cx),
            "view.terminal" => self.set_panel(PanelTab::Terminal, cx),
            "view.problems" => self.set_panel(PanelTab::Problems, cx),
            "view.toggleSidebar" => {
                self.sidebar_visible = !self.sidebar_visible;
                cx.notify();
            }
            _ => {
                self.editor_area
                    .update(cx, |area, cx| area.run_command(&command, cx));
            }
        }
    }

    /// ステータスバーに一時的な通知を出す。
    ///
    /// 一定時間で自動的に消す。消さないと、解決済みのエラーが画面の隅に残り続けて
    /// 現在の状態を誤って伝える。エラーは長め、情報は短めにする。
    fn notify_status(&mut self, level: NotificationLevel, message: String, cx: &mut Context<Self>) {
        let seconds = match level {
            NotificationLevel::Info => 4,
            NotificationLevel::Warning => 8,
            NotificationLevel::Error => 12,
        };
        self.status_message = Some((level, message));
        self._status_timer = Some(cx.spawn(async move |this, cx| {
            cx.background_executor()
                .timer(std::time::Duration::from_secs(seconds))
                .await;
            this.update(cx, |this, cx| {
                this.status_message = None;
                cx.notify();
            })
            .ok();
        }));
        cx.notify();
    }

    fn set_sidebar(&mut self, tab: SidebarTab, cx: &mut Context<Self>) {
        if self.sidebar == tab && self.sidebar_visible {
            self.sidebar_visible = false;
        } else {
            self.sidebar = tab;
            self.sidebar_visible = true;
        }
        self.save_session(cx);
        cx.notify();
    }

    fn set_panel(&mut self, tab: PanelTab, cx: &mut Context<Self>) {
        if self.panel == tab && self.panel_visible {
            self.panel_visible = false;
        } else {
            self.panel = tab;
            self.panel_visible = true;
        }
        self.ensure_terminal_launched(cx);
        cx.notify();
    }

    /// ターミナルタブが表示された直後に呼ぶ。表示先が別タブ、またはパネル自体が
    /// 隠れているときは何もしない — 実際の起動判断 (二重起動防止や既存タブの
    /// 有無) は `TerminalView::ensure_terminal` 側が持つ。
    ///
    /// `set_panel` のほかに `on_toggle_panel` (Cmd+J) からも呼ぶ。既定のパネル
    /// タブは起動時から `Terminal` なので、`set_panel` を経由しない Cmd+J だけで
    /// 初めてパネルを開くケースがあり、そちらを取りこぼすと「＋」も出ない
    /// 空のパネルが残ってしまう。
    fn ensure_terminal_launched(&mut self, cx: &mut Context<Self>) {
        // 表示状態は必ず TerminalView へ伝える。伝えないと、接続とワークスペースが
        // 後から揃ったときに TerminalView 側が単独で起動判断をしてしまい、
        // パネルを一度も開いていない利用者の裏でシェルが常駐する。
        let visible = self.panel_visible && self.panel == PanelTab::Terminal;
        self.terminal
            .update(cx, |v, cx| v.set_panel_visible(visible, cx));
    }

    // -- アクション --

    fn on_toggle_palette(
        &mut self,
        _: &actions::ToggleCommandPalette,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.palette
            .update(cx, |p, cx| p.open(PaletteMode::Commands, window, cx));
        cx.notify();
    }

    fn on_toggle_file_finder(
        &mut self,
        _: &actions::ToggleFileFinder,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.palette
            .update(cx, |p, cx| p.open(PaletteMode::Files, window, cx));
        cx.notify();
    }

    fn on_toggle_sidebar(
        &mut self,
        _: &actions::ToggleSidebar,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.sidebar_visible = !self.sidebar_visible;
        cx.notify();
    }

    fn on_toggle_panel(
        &mut self,
        _: &actions::TogglePanel,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.panel_visible = !self.panel_visible;
        self.ensure_terminal_launched(cx);
        cx.notify();
    }

    fn on_toggle_terminal(
        &mut self,
        _: &actions::ToggleTerminal,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.set_panel(PanelTab::Terminal, cx);
    }

    fn on_show_explorer(
        &mut self,
        _: &actions::ShowExplorer,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.set_sidebar(SidebarTab::Explorer, cx);
    }

    fn on_show_search(
        &mut self,
        _: &actions::ShowSearch,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.set_sidebar(SidebarTab::Search, cx);
    }

    fn on_show_git(&mut self, _: &actions::ShowGit, _window: &mut Window, cx: &mut Context<Self>) {
        self.set_sidebar(SidebarTab::Git, cx);
    }

    fn on_show_codex(
        &mut self,
        _: &actions::ShowCodex,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.set_sidebar(SidebarTab::Codex, cx);
    }

    fn on_open_folder(
        &mut self,
        _: &actions::OpenFolder,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let paths = cx.prompt_for_paths(gpui::PathPromptOptions {
            files: false,
            directories: true,
            multiple: false,
            prompt: None,
        });
        cx.spawn(async move |this, cx| {
            if let Ok(Ok(Some(paths))) = paths.await
                && let Some(folder) = paths.into_iter().next()
            {
                this.update(cx, |this, cx| this.open_folder(folder, true, cx))
                    .ok();
            }
        })
        .detach();
    }

    fn on_next_workspace(
        &mut self,
        _: &actions::NextWorkspace,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.workspaces.len() < 2 {
            return;
        }
        let current = self
            .active_workspace
            .and_then(|id| self.workspaces.iter().position(|w| w.id == id))
            .unwrap_or(0);
        let next = self.workspaces[(current + 1) % self.workspaces.len()].id;
        self.activate_workspace(next, cx);
    }

    fn on_quit(&mut self, _: &actions::Quit, _window: &mut Window, cx: &mut Context<Self>) {
        cx.quit();
    }

    // -- 描画 --

    /// 最左のアクティビティバー。上段が Slack 風のワークスペース切り替え、
    /// 下段がビュー切り替え。
    fn render_activity_bar(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let theme = theme(cx).clone();
        let active_tab = self.sidebar;
        let sidebar_visible = self.sidebar_visible;

        let workspace_buttons: Vec<_> = self
            .workspaces
            .iter()
            .enumerate()
            .map(|(index, info)| {
                let is_active = self.active_workspace == Some(info.id);
                let initial = info
                    .name
                    .chars()
                    .next()
                    .unwrap_or('?')
                    .to_uppercase()
                    .to_string();
                let id = info.id;
                h_flex()
                    .id(("workspace", index))
                    .justify_center()
                    .size(px(34.))
                    .rounded(px(10.))
                    .text_size(px(14.))
                    .font_weight(gpui::FontWeight::BOLD)
                    .cursor_pointer()
                    .when(is_active, |el| {
                        el.bg(theme.accent)
                            .text_color(theme.text_inverse)
                            // 選択中のワークスペースはネオンで縁取り、宇宙船の計器めいた見た目にする。
                            .border_1()
                            .border_color(theme.accent)
                    })
                    .when(!is_active, |el| {
                        el.bg(theme.bg_elevated)
                            .text_color(theme.text_muted)
                            .hover(|s| s.bg(theme.bg_overlay).text_color(theme.text))
                    })
                    .child(initial)
                    .on_click(cx.listener(move |this, _, _window, cx| {
                        this.activate_workspace(id, cx);
                    }))
                    .tooltip(simple_tooltip(tooltip_text::workspace_switch(&info.name)))
            })
            .collect();

        v_flex()
            .w(metrics::ACTIVITY_BAR_WIDTH)
            .h_full()
            .flex_none()
            .bg(theme.bg_activity)
            .border_r_1()
            .border_color(theme.border)
            .child(
                // ワークスペース切り替え帯
                v_flex()
                    .items_center()
                    .gap(px(6.))
                    .py(px(10.))
                    .children(workspace_buttons)
                    .child(
                        h_flex()
                            .id("add-workspace")
                            .justify_center()
                            .size(px(34.))
                            .rounded(px(10.))
                            .cursor_pointer()
                            .border_1()
                            .border_color(theme.border)
                            .hover(|s| s.bg(theme.bg_overlay))
                            .child(icon(Icon::Plus, px(15.), theme.text_faint))
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.on_open_folder(&actions::OpenFolder, window, cx);
                            }))
                            .tooltip(simple_tooltip(tooltip_text::add_workspace())),
                    ),
            )
            .child(div().mx(px(12.)).h(px(1.)).bg(theme.border).flex_none())
            .child(
                v_flex()
                    .flex_1()
                    .pt(px(6.))
                    .child(
                        activity_item(
                            "act-explorer",
                            Icon::Files,
                            sidebar_visible && active_tab == SidebarTab::Explorer,
                            cx,
                        )
                        .on_click(
                            cx.listener(|this, _, _w, cx| {
                                this.set_sidebar(SidebarTab::Explorer, cx)
                            }),
                        )
                        .tooltip(simple_tooltip(tooltip_text::show_explorer())),
                    )
                    .child(
                        activity_item(
                            "act-search",
                            Icon::Search,
                            sidebar_visible && active_tab == SidebarTab::Search,
                            cx,
                        )
                        .on_click(
                            cx.listener(|this, _, _w, cx| this.set_sidebar(SidebarTab::Search, cx)),
                        )
                        .tooltip(simple_tooltip(tooltip_text::show_search())),
                    )
                    .child(
                        activity_item(
                            "act-git",
                            Icon::Git,
                            sidebar_visible && active_tab == SidebarTab::Git,
                            cx,
                        )
                        .on_click(
                            cx.listener(|this, _, _w, cx| this.set_sidebar(SidebarTab::Git, cx)),
                        )
                        .tooltip(simple_tooltip(tooltip_text::show_git())),
                    )
                    .child(
                        activity_item(
                            "act-codex",
                            Icon::Sparkles,
                            sidebar_visible && active_tab == SidebarTab::Codex,
                            cx,
                        )
                        .on_click(
                            cx.listener(|this, _, _w, cx| this.set_sidebar(SidebarTab::Codex, cx)),
                        )
                        .tooltip(simple_tooltip(tooltip_text::show_codex())),
                    ),
            )
            .child(v_flex().pb(px(8.)).items_center().child(
                icon_button("act-settings", Icon::Settings, false, cx)
                    .on_click(cx.listener(|this, _, _w, cx| {
                        this.notify_status(
                            NotificationLevel::Info,
                            "設定画面は今後の実装対象です".to_string(),
                            cx,
                        );
                    }))
                    .tooltip(simple_tooltip(tooltip_text::SETTINGS)),
            ))
            .into_any_element()
    }

    /// パネルの境界に置く、ドラッグでつまめる細い帯。
    ///
    /// 見た目上は 1px の境界線だが、当たり判定は 5px 取る。1px だと掴めない。
    fn render_sidebar(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let theme = theme(cx);
        v_flex()
            .w(self.sidebar_width)
            .h_full()
            .flex_none()
            .bg(theme.bg_elevated)
            .border_r_1()
            .border_color(theme.border)
            .overflow_hidden()
            .child(match self.sidebar {
                SidebarTab::Explorer => self.explorer.clone().into_any_element(),
                SidebarTab::Search => self.search.clone().into_any_element(),
                SidebarTab::Git => self.git.clone().into_any_element(),
                SidebarTab::Codex => self.codex.clone().into_any_element(),
            })
            .into_any_element()
    }

    /// サイドバー右端のドラッグ用の帯。
    fn render_sidebar_handle(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let theme = theme(cx).clone();
        div()
            .id("sidebar-resize")
            .w(px(5.))
            .h_full()
            .flex_none()
            .cursor(gpui::CursorStyle::ResizeLeftRight)
            .hover(|s| s.bg(theme.border_glow))
            .on_drag_move(cx.listener(
                |this: &mut Self, event: &gpui::DragMoveEvent<SidebarResize>, _w, cx| {
                    let x = event.event.position.x;
                    this.sidebar_width = (x - metrics::ACTIVITY_BAR_WIDTH)
                        .max(metrics::SIDEBAR_MIN_WIDTH)
                        .min(metrics::SIDEBAR_MAX_WIDTH);
                    cx.notify();
                },
            ))
            .on_drag(SidebarResize, |_, _, _, cx| cx.new(|_| DragGhost))
            // 保存はドラッグ中ではなく離したときに 1 回だけ。
            // 毎フレーム書くと 60Hz でディスクを叩くことになる。
            .on_mouse_up(
                gpui::MouseButton::Left,
                cx.listener(|this: &mut Self, _, _w, cx| this.save_session(cx)),
            )
            .into_any_element()
    }

    /// 下部パネル上端のドラッグ用の帯。
    fn render_panel_handle(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let theme = theme(cx).clone();
        div()
            .id("panel-resize")
            .h(px(5.))
            .w_full()
            .flex_none()
            .cursor(gpui::CursorStyle::ResizeUpDown)
            .hover(|s| s.bg(theme.border_glow))
            .on_drag_move(cx.listener(
                |this: &mut Self, event: &gpui::DragMoveEvent<PanelResize>, window, cx| {
                    let height = window.viewport_size().height
                        - event.event.position.y
                        - metrics::STATUS_BAR_HEIGHT;
                    this.panel_height = height.max(metrics::PANEL_MIN_HEIGHT);
                    cx.notify();
                },
            ))
            .on_drag(PanelResize, |_, _, _, cx| cx.new(|_| DragGhost))
            .on_mouse_up(
                gpui::MouseButton::Left,
                cx.listener(|this: &mut Self, _, _w, cx| this.save_session(cx)),
            )
            .into_any_element()
    }

    fn render_panel(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let theme = theme(cx).clone();
        let active = self.panel;
        v_flex()
            .h(self.panel_height)
            .w_full()
            .flex_none()
            .bg(theme.bg_elevated)
            .border_t_1()
            .border_color(theme.border)
            .child(
                h_flex()
                    .h(px(30.))
                    .px(px(10.))
                    .gap(px(4.))
                    .flex_none()
                    .border_b_1()
                    .border_color(theme.border)
                    .child(self.render_panel_tab(
                        "panel-terminal",
                        "ターミナル",
                        PanelTab::Terminal,
                        active,
                        cx,
                    ))
                    .child(self.render_panel_tab(
                        "panel-problems",
                        "問題",
                        PanelTab::Problems,
                        active,
                        cx,
                    ))
                    .child(div().flex_1())
                    .child(
                        icon_button("panel-close", Icon::Close, false, cx)
                            .on_click(cx.listener(|this, _, _w, cx| {
                                this.panel_visible = false;
                                cx.notify();
                            }))
                            .tooltip(simple_tooltip(tooltip_text::CLOSE_PANEL)),
                    ),
            )
            .child(div().flex_1().overflow_hidden().child(match active {
                PanelTab::Terminal => self.terminal.clone().into_any_element(),
                PanelTab::Problems => self.problems.clone().into_any_element(),
            }))
            .into_any_element()
    }

    fn render_panel_tab(
        &self,
        id: &'static str,
        label: &'static str,
        tab: PanelTab,
        active: PanelTab,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let theme = theme(cx).clone();
        let is_active = tab == active;
        h_flex()
            .id(id)
            .px(px(10.))
            .h(px(22.))
            .rounded(px(4.))
            .text_size(px(11.5))
            .cursor_pointer()
            .when(is_active, |el| {
                el.text_color(theme.accent).bg(theme.accent_soft)
            })
            .when(!is_active, |el| {
                el.text_color(theme.text_muted)
                    .hover(|s| s.text_color(theme.text))
            })
            .child(label)
            .on_click(cx.listener(move |this, _, _w, cx| this.set_panel(tab, cx)))
            .into_any_element()
    }

    fn render_status_bar(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let theme = theme(cx).clone();
        let (connection_text, connection_color) = match &self.connection {
            ConnectionState::Connecting => ("バックエンド接続中…".to_string(), theme.text_faint),
            ConnectionState::Ready(info) => (
                format!("バックエンド {} (pid {})", info.backend_version, info.pid),
                theme.text_faint,
            ),
            ConnectionState::Failed(e) => (format!("接続失敗: {e}"), theme.error),
        };
        // バックエンドが落ちた場合は、握手済みでも切断を優先して伝える。
        // 気づかないまま編集を続けると保存できずに作業が失われる。
        let disconnected = self
            .client
            .as_ref()
            .is_some_and(|client| client.is_disconnected());
        let (connection_text, connection_color) = if disconnected {
            (
                "バックエンドが切断しました。再起動してください".to_string(),
                theme.error,
            )
        } else {
            (connection_text, connection_color)
        };
        let workspace_name = self
            .active_workspace
            .and_then(|id| self.workspaces.iter().find(|w| w.id == id))
            .map(|w| w.name.clone())
            .unwrap_or_else(|| "フォルダ未選択".to_string());

        h_flex()
            .h(metrics::STATUS_BAR_HEIGHT)
            .w_full()
            .flex_none()
            .px(px(10.))
            .gap(px(14.))
            .bg(theme.bg_activity)
            .border_t_1()
            .border_color(theme.border)
            .text_size(px(11.))
            .child(
                h_flex()
                    .gap(px(5.))
                    .child(icon(Icon::Folder, px(12.), theme.accent))
                    .child(div().text_color(theme.text_muted).child(workspace_name)),
            )
            .child(div().text_color(connection_color).child(connection_text))
            .child(div().flex_1())
            .children(self.status_message.as_ref().map(|(level, message)| {
                let color = match level {
                    NotificationLevel::Info => theme.text_muted,
                    NotificationLevel::Warning => theme.warning,
                    NotificationLevel::Error => theme.error,
                };
                div().text_color(color).child(message.clone())
            }))
            .into_any_element()
    }
}

impl Focusable for NebulaApp {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for NebulaApp {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // 最初の描画に到達した時刻を 1 度だけ記録する。冷間起動の実測値はここ。
        if !self.first_render_done {
            self.first_render_done = true;
            if crate::trace_startup()
                && let Some(elapsed) = crate::startup_elapsed_ms()
            {
                eprintln!("nebula: main から最初の描画まで {elapsed:.1}ms");
            }
        }
        let theme = theme(cx).clone();
        let activity_bar = self.render_activity_bar(cx);
        let sidebar = self.sidebar_visible.then(|| self.render_sidebar(cx));
        let sidebar_handle = self.sidebar_visible.then(|| self.render_sidebar_handle(cx));
        let panel = self.panel_visible.then(|| self.render_panel(cx));
        let panel_handle = self.panel_visible.then(|| self.render_panel_handle(cx));
        let status_bar = self.render_status_bar(cx);

        div()
            .key_context("Workspace")
            .track_focus(&self.focus_handle)
            .size_full()
            .flex()
            .flex_col()
            .bg(theme.bg_void)
            .text_color(theme.text)
            .font_family("SF Pro Text")
            .text_size(metrics::UI_FONT_SIZE)
            .on_action(cx.listener(Self::on_quit))
            .on_action(cx.listener(Self::on_toggle_palette))
            .on_action(cx.listener(Self::on_toggle_file_finder))
            .on_action(cx.listener(Self::on_toggle_sidebar))
            .on_action(cx.listener(Self::on_toggle_panel))
            .on_action(cx.listener(Self::on_toggle_terminal))
            .on_action(cx.listener(Self::on_show_explorer))
            .on_action(cx.listener(Self::on_show_search))
            .on_action(cx.listener(Self::on_show_git))
            .on_action(cx.listener(Self::on_show_codex))
            .on_action(cx.listener(Self::on_open_folder))
            .on_action(cx.listener(Self::on_next_workspace))
            .child(
                // タイトルバー領域。macOS の信号機ボタンと重ならないよう左側を空ける。
                h_flex()
                    .h(px(30.))
                    .w_full()
                    .flex_none()
                    .pl(px(78.))
                    .pr(px(10.))
                    .bg(theme.bg_activity)
                    .border_b_1()
                    .border_color(theme.border)
                    .child(
                        div()
                            .text_size(px(11.5))
                            .text_color(theme.text_faint)
                            .child("Nebula Code"),
                    ),
            )
            .child(
                div()
                    .flex()
                    .flex_row()
                    .flex_1()
                    .overflow_hidden()
                    .child(activity_bar)
                    .children(sidebar)
                    .children(sidebar_handle)
                    .child(
                        v_flex()
                            .flex_1()
                            .overflow_hidden()
                            .child(
                                div()
                                    .flex_1()
                                    .overflow_hidden()
                                    .child(self.editor_area.clone()),
                            )
                            .children(panel_handle)
                            .children(panel),
                    ),
            )
            .child(status_bar)
            .child(self.palette.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::decide_workspace_update;

    /// 1つ目のフォルダを開いたときは、一覧に追加され、まだ何もアクティブでないので
    /// 必ずアクティブになる。
    #[test]
    fn 最初のフォルダは追加されアクティブになる() {
        let (should_push, should_activate) = decide_workspace_update(false, false, true, false);
        assert!(should_push);
        assert!(should_activate);
    }

    /// 既に1つ開いている状態でユーザーが+ボタン等から2つ目を追加すると、
    /// 一覧に追加された上でアクティブも移る。
    /// 従来はアクティブが切り替わらず「押しても何も起きない」ように見えた。これを直す。
    #[test]
    fn ユーザー操作で2つ目のフォルダを追加すると追加されアクティブが移る() {
        let (should_push, should_activate) = decide_workspace_update(false, false, true, true);
        assert!(should_push);
        assert!(should_activate);
    }

    /// 既に開いているフォルダをユーザーが再度追加した場合、
    /// 一覧には二重登録されず、そのワークスペースへアクティブだけを移す。
    #[test]
    fn 既に開いているフォルダを再度追加すると二重登録されずアクティブになる() {
        let (should_push, should_activate) = decide_workspace_update(true, false, true, true);
        assert!(!should_push);
        assert!(should_activate);
    }

    /// セッション復元で複数フォルダを順に開いても、復元対象でないものにはアクティブを奪われない
    /// (一覧には追加される)。
    #[test]
    fn セッション復元では復元対象でなければ追加されてもアクティブを奪わない() {
        // 2つ目以降 (既に1つアクティブがある) を復元中に開いた場合。
        let (should_push, should_activate) = decide_workspace_update(false, false, false, true);
        assert!(should_push);
        assert!(!should_activate);
    }

    /// セッション復元での復元対象そのものは、他のフォルダが先にアクティブになっていても
    /// 必ずアクティブに戻る。
    #[test]
    fn セッション復元の対象フォルダは必ずアクティブになる() {
        let (should_push, should_activate) = decide_workspace_update(false, true, false, true);
        assert!(should_push);
        assert!(should_activate);
    }

    /// セッション復元で1つ目 (まだ何もアクティブでない) を開いた場合は、
    /// 復元対象かどうかによらずアクティブになる (これまでの挙動を維持)。
    #[test]
    fn セッション復元の1つ目は復元対象でなくてもアクティブになる() {
        let (should_push, should_activate) = decide_workspace_update(false, false, false, false);
        assert!(should_push);
        assert!(should_activate);
    }
}
