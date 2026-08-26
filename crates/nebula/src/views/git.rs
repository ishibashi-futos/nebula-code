//! Git パネル (ソース管理)。
//!
//! バックエンドが `git` CLI を叩き、結果は `GitRepoStatus` として届く。
//! このビューが持つのはその写しと、まだ送っていない入力 (コミットメッセージ・
//! 新しいブランチ名) だけ。ステージやコミットのような副作用のある操作は、
//! バックエンドが完了後に `Event::GitStatusChanged` を配ってくれるので、
//! **応答を見て自分で一覧を作り直すことはしない**。二重に更新経路を持つと、
//! 外部で `git` を実行されたときだけ表示がずれる、という追いにくいバグになる。

use crate::ui::format_keystroke;
use crate::assets::Icon;
use crate::ipc_client::BackendClient;
use crate::theme::{Theme, theme};
use crate::ui::{
    TextInput, TextInputEvent, empty_state, h_flex, icon, icon_button, list_row,
    nebula_accent_line, panel_header, primary_button, simple_tooltip, tooltip_text,
    truncate_middle, v_flex,
};
use gpui::prelude::*;
use gpui::{
    AnyElement, Context, ElementId, Entity, EventEmitter, Hsla, MouseButton, Pixels, SharedString,
    Stateful, Subscription, Window, div, px, uniform_list,
};
use nebula_protocol::{
    Event, GitBranch, GitFileStatus, GitRepoStatus, GitStatusCode, NotificationLevel, Request,
    Response, WorkspaceId, WorkspaceInfo,
};
use std::ops::Range;
use std::path::{Path, PathBuf};

pub enum GitEvent {
    OpenFile(PathBuf),
    Notify(NotificationLevel, String),
}

/// 一覧の 1 行の高さ。`uniform_list` は全行を同じ高さで扱うので、
/// 見出しもファイル行もこの値に揃える。
const ROW_HEIGHT: Pixels = px(24.);

// ---------------------------------------------------------------------------
// 一覧の組み立て (純粋関数)
// ---------------------------------------------------------------------------

/// 変更一覧の区分。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GitSection {
    Staged,
    Changed,
    Untracked,
}

impl GitSection {
    /// 表示順。上から「ステージ済み」「変更」「未追跡」。
    const ORDER: [GitSection; 3] = [
        GitSection::Staged,
        GitSection::Changed,
        GitSection::Untracked,
    ];

    fn title(self) -> &'static str {
        match self {
            GitSection::Staged => "ステージ済み",
            GitSection::Changed => "変更",
            GitSection::Untracked => "未追跡",
        }
    }

    /// 見出しに置くまとめ操作の名前。
    fn bulk_label(self) -> &'static str {
        match self {
            GitSection::Staged => "すべてアンステージ",
            _ => "すべてステージ",
        }
    }
}

/// 平坦化した一覧の 1 行。
///
/// 見出しとファイル行を 1 本の列にまとめるのは、`uniform_list` が
/// 「同じ高さの要素が n 個並ぶ」という形しか扱えないため。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GitRow {
    Header {
        section: GitSection,
        count: usize,
        collapsed: bool,
    },
    File {
        section: GitSection,
        entry: GitFileStatus,
    },
}

/// ファイルがどの区分に出るかを決める。
///
/// 索引と作業ツリーの双方に変更があるファイル (部分ステージ) は
/// **両方の区分に出す**。git 自身がそう扱っており、片方に寄せると
/// 「ステージしたはずの変更が消えた」ように見える。
fn sections_for(entry: &GitFileStatus) -> Vec<GitSection> {
    // 衝突中のファイルは索引側にも印が付くが、解決前にステージ操作をさせたくないので
    // 「変更」にだけ出す。
    if entry.index == GitStatusCode::Conflicted || entry.worktree == GitStatusCode::Conflicted {
        return vec![GitSection::Changed];
    }
    if entry.index == GitStatusCode::Untracked || entry.worktree == GitStatusCode::Untracked {
        return vec![GitSection::Untracked];
    }
    let mut sections = Vec::new();
    if is_changed(entry.index) {
        sections.push(GitSection::Staged);
    }
    if is_changed(entry.worktree) {
        sections.push(GitSection::Changed);
    }
    sections
}

fn is_changed(code: GitStatusCode) -> bool {
    !matches!(code, GitStatusCode::Unmodified | GitStatusCode::Ignored)
}

/// その区分の行に出す状態コード。
///
/// エクスプローラーのファイルバッジも同じ優先順位 (作業ツリーに変化があればそれ、
/// 無ければ索引) で 1 つの代表コードが要るため、`GitSection::Changed` を渡す形で
/// ここを再利用する (コピペしない)。
pub(crate) fn code_for(entry: &GitFileStatus, section: GitSection) -> GitStatusCode {
    match section {
        GitSection::Staged => entry.index,
        GitSection::Untracked => GitStatusCode::Untracked,
        GitSection::Changed => {
            if is_changed(entry.worktree) {
                entry.worktree
            } else {
                entry.index
            }
        }
    }
}

/// 状態を 1 文字で表す。git の porcelain 表記に合わせる。
pub(crate) fn status_char(code: GitStatusCode) -> char {
    match code {
        GitStatusCode::Modified => 'M',
        GitStatusCode::Added => 'A',
        GitStatusCode::Deleted => 'D',
        GitStatusCode::Renamed => 'R',
        GitStatusCode::Copied => 'C',
        GitStatusCode::Untracked => '?',
        GitStatusCode::Conflicted => 'U',
        GitStatusCode::Ignored => '!',
        GitStatusCode::Unmodified => '·',
    }
}

pub(crate) fn status_color(code: GitStatusCode, theme: &Theme) -> Hsla {
    match code {
        GitStatusCode::Added | GitStatusCode::Copied | GitStatusCode::Untracked => theme.git_added,
        GitStatusCode::Modified | GitStatusCode::Renamed => theme.git_modified,
        GitStatusCode::Deleted => theme.git_deleted,
        GitStatusCode::Conflicted => theme.git_conflict,
        GitStatusCode::Ignored | GitStatusCode::Unmodified => theme.git_ignored,
    }
}

/// 状態を区分ごとにまとめ、`uniform_list` に渡せる 1 本の列にする。
///
/// 折りたたまれた区分は見出しだけを残す。件数は折りたたみに関わらず実数を出す。
pub fn flatten_rows(status: &GitRepoStatus, collapsed: &[GitSection]) -> Vec<GitRow> {
    let mut rows = Vec::new();
    for section in GitSection::ORDER {
        let entries: Vec<&GitFileStatus> = status
            .entries
            .iter()
            .filter(|entry| sections_for(entry).contains(&section))
            .collect();
        if entries.is_empty() {
            continue;
        }
        let is_collapsed = collapsed.contains(&section);
        rows.push(GitRow::Header {
            section,
            count: entries.len(),
            collapsed: is_collapsed,
        });
        if is_collapsed {
            continue;
        }
        rows.extend(entries.into_iter().map(|entry| GitRow::File {
            section,
            entry: entry.clone(),
        }));
    }
    rows
}

/// 「ファイル名」と「親ディレクトリ」に分ける。
fn split_path_display(path: &Path) -> (String, String) {
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| path.to_string_lossy().to_string());
    let parent = path
        .parent()
        .map(|p| p.to_string_lossy().to_string())
        .unwrap_or_default();
    (name, parent)
}

/// 上流との差を「↑2 ↓1」の形にする。差が無ければ空文字。
fn format_sync_counts(ahead: u32, behind: u32) -> String {
    let mut parts = Vec::new();
    if ahead > 0 {
        parts.push(format!("↑{ahead}"));
    }
    if behind > 0 {
        parts.push(format!("↓{behind}"));
    }
    parts.join(" ")
}

/// チェックアウトに渡す名前。
///
/// 遠隔ブランチは `origin/foo` の形で届く。そのまま `git switch` に渡すと
/// 分離 HEAD になるため接頭辞を外す。同名の遠隔ブランチが 1 つだけなら
/// git が追跡ブランチを自動で作る。
fn checkout_name(branch: &GitBranch) -> String {
    if branch.is_remote {
        branch
            .name
            .split_once('/')
            .map(|(_, rest)| rest.to_string())
            .unwrap_or_else(|| branch.name.clone())
    } else {
        branch.name.clone()
    }
}

// ---------------------------------------------------------------------------
// Git パネル本体
// ---------------------------------------------------------------------------

/// 実行中の遠隔操作。実行中は同じ操作を重ねて出さない。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Busy {
    Push,
    Pull,
    Commit,
    Checkout,
}

impl Busy {
    fn label(self) -> &'static str {
        match self {
            Busy::Push => "プッシュ中…",
            Busy::Pull => "プル中…",
            Busy::Commit => "コミット中…",
            Busy::Checkout => "切り替え中…",
        }
    }
}

pub struct GitView {
    client: Option<BackendClient>,
    workspace: Option<WorkspaceInfo>,
    status: GitRepoStatus,
    /// `status` から作った表示用の平坦な一覧。描画のたびに組み直さないよう保持する。
    rows: Vec<GitRow>,
    collapsed: Vec<GitSection>,
    branches: Vec<GitBranch>,
    branch_menu_open: bool,
    creating_branch: bool,
    amend: bool,
    /// 破棄の確認待ち (絶対パス, 表示名)。
    discard: Option<(PathBuf, String)>,
    busy: Option<Busy>,
    hovered: Option<usize>,
    commit_input: Entity<TextInput>,
    branch_input: Entity<TextInput>,
    /// 保持しないと購読が即座に解除される。
    _subscriptions: Vec<Subscription>,
}

impl EventEmitter<GitEvent> for GitView {}

impl GitView {
    pub fn new(cx: &mut Context<Self>) -> Self {
        // コミットメッセージは複数行。Enter は改行、確定は ⌘Enter。
        let commit_input = cx.new(|cx| {
            TextInput::multi_line("コミットメッセージ", 3, None, cx).submitting_on_enter(false)
        });
        let branch_input = cx.new(|cx| TextInput::single_line("新しいブランチ名", cx));
        let subscriptions = vec![
            cx.subscribe(&commit_input, Self::on_commit_input),
            cx.subscribe(&branch_input, Self::on_branch_input),
        ];
        Self {
            client: None,
            workspace: None,
            status: GitRepoStatus::default(),
            rows: Vec::new(),
            collapsed: Vec::new(),
            branches: Vec::new(),
            branch_menu_open: false,
            creating_branch: false,
            amend: false,
            discard: None,
            busy: None,
            hovered: None,
            commit_input,
            branch_input,
            _subscriptions: subscriptions,
        }
    }

    pub fn set_client(&mut self, client: BackendClient, cx: &mut Context<Self>) {
        self.client = Some(client);
        self.refresh_status(cx);
        cx.notify();
    }

    pub fn set_workspace(&mut self, workspace: WorkspaceInfo, cx: &mut Context<Self>) {
        self.workspace = Some(workspace);
        // 別のフォルダに切り替わったら、前のリポジトリの状態は捨てる。
        self.status = GitRepoStatus::default();
        self.rows.clear();
        self.branches.clear();
        self.branch_menu_open = false;
        // 確認待ちは前のリポジトリの絶対パスを握っている。持ち越すと、そのまま
        // 押されたときに新しいワークスペース ID と古いパスの組で破棄要求が飛ぶ。
        self.discard = None;
        self.refresh_status(cx);
        cx.notify();
    }

    pub fn handle_event(&mut self, event: &Event, cx: &mut Context<Self>) {
        if let Event::GitStatusChanged { workspace, status } = event
            && Some(*workspace) == self.workspace.as_ref().map(|w| w.id)
        {
            self.status = status.clone();
            self.rebuild_rows();
        }
        cx.notify();
    }

    // -- バックエンドとのやり取り --

    /// git 管理下のワークスペース ID。管理外なら `None`。
    fn workspace_id(&self) -> Option<WorkspaceId> {
        let workspace = self.workspace.as_ref()?;
        workspace.git_root.as_ref()?;
        Some(workspace.id)
    }

    fn repo_root(&self) -> Option<&PathBuf> {
        self.workspace.as_ref()?.git_root.as_ref()
    }

    /// 一覧の相対パスを絶対パスに直す。エディタで開くにも git に渡すにも絶対パスを使う。
    fn absolute(&self, path: &Path) -> PathBuf {
        match self.repo_root() {
            Some(root) => root.join(path),
            None => path.to_path_buf(),
        }
    }

    fn rebuild_rows(&mut self) {
        self.rows = flatten_rows(&self.status, &self.collapsed);
        self.hovered = None;
    }

    fn refresh_status(&mut self, cx: &mut Context<Self>) {
        let (Some(client), Some(workspace)) = (self.client.clone(), self.workspace_id()) else {
            return;
        };
        cx.spawn(async move |this, cx| {
            let result = client.request(Request::GitStatus { workspace }).await;
            this.update(cx, |this, cx| {
                match result {
                    Ok(Response::GitStatus(status)) => {
                        this.status = status;
                        this.rebuild_rows();
                    }
                    Ok(_) => {}
                    Err(e) => cx.emit(GitEvent::Notify(
                        NotificationLevel::Warning,
                        format!("git の状態を取得できません: {e}"),
                    )),
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// 副作用だけの要求を投げる。結果の一覧更新は `GitStatusChanged` に任せる。
    ///
    /// `owner` はこの要求が立てた実行中表示。完了時に **自分が立てたものだけ** を下ろす。
    /// 無条件に下ろすと、プッシュ中にステージした 1 件が先に返ってきただけで
    /// 「プッシュ中…」が消え、ボタンが復活して二重にプッシュできてしまう。
    /// 実行中表示を持たない要求 (ステージ等) は `None` を渡す。
    fn send(
        &mut self,
        request: Request,
        owner: Option<Busy>,
        failure: &'static str,
        cx: &mut Context<Self>,
    ) {
        let Some(client) = self.client.clone() else {
            return;
        };
        cx.spawn(async move |this, cx| {
            let result = client.request(request).await;
            this.update(cx, |this, cx| {
                if owner.is_some() && this.busy == owner {
                    this.busy = None;
                }
                if let Err(e) = result {
                    cx.emit(GitEvent::Notify(
                        NotificationLevel::Error,
                        format!("{failure}: {e}"),
                    ));
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn stage(&mut self, paths: Vec<PathBuf>, cx: &mut Context<Self>) {
        let Some(workspace) = self.workspace_id() else {
            return;
        };
        self.send(
            Request::GitStage { workspace, paths },
            None,
            "ステージに失敗しました",
            cx,
        );
    }

    fn unstage(&mut self, paths: Vec<PathBuf>, cx: &mut Context<Self>) {
        let Some(workspace) = self.workspace_id() else {
            return;
        };
        self.send(
            Request::GitUnstage { workspace, paths },
            None,
            "アンステージに失敗しました",
            cx,
        );
    }

    /// 区分の全ファイルをまとめて操作する。
    fn bulk(&mut self, section: GitSection, cx: &mut Context<Self>) {
        let paths: Vec<PathBuf> = self
            .status
            .entries
            .iter()
            .filter(|entry| sections_for(entry).contains(&section))
            .map(|entry| self.absolute(&entry.path))
            .collect();
        if paths.is_empty() {
            return;
        }
        match section {
            GitSection::Staged => self.unstage(paths, cx),
            _ => self.stage(paths, cx),
        }
    }

    fn confirm_discard(&mut self, cx: &mut Context<Self>) {
        let Some((path, _)) = self.discard.take() else {
            return;
        };
        let Some(workspace) = self.workspace_id() else {
            return;
        };
        self.send(
            Request::GitDiscardChanges {
                workspace,
                paths: vec![path],
            },
            None,
            "変更の破棄に失敗しました",
            cx,
        );
    }

    fn commit(&mut self, cx: &mut Context<Self>) {
        // ボタンだけでなく ⌘⏎ もここを通る。入口で弾かないと、無効表示のまま
        // 打鍵で失敗する要求が飛んだり、連打で二重にコミットされたりする。
        if self.busy.is_some() || (self.staged_count() == 0 && !self.amend) {
            return;
        }
        let message = self.commit_input.read(cx).text().to_string();
        if message.trim().is_empty() {
            cx.emit(GitEvent::Notify(
                NotificationLevel::Warning,
                "コミットメッセージを入力してください".into(),
            ));
            return;
        }
        let (Some(client), Some(workspace)) = (self.client.clone(), self.workspace_id()) else {
            return;
        };
        let amend = self.amend;
        self.busy = Some(Busy::Commit);
        cx.notify();
        cx.spawn(async move |this, cx| {
            let result = client
                .request(Request::GitCommit {
                    workspace,
                    message,
                    amend,
                })
                .await;
            this.update(cx, |this, cx| {
                this.busy = None;
                match result {
                    Ok(_) => {
                        this.commit_input.update(cx, |input, cx| input.clear(cx));
                        this.amend = false;
                        cx.emit(GitEvent::Notify(
                            NotificationLevel::Info,
                            "コミットしました".into(),
                        ));
                    }
                    Err(e) => cx.emit(GitEvent::Notify(
                        NotificationLevel::Error,
                        format!("コミットに失敗しました: {e}"),
                    )),
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn checkout(&mut self, branch: String, create: bool, cx: &mut Context<Self>) {
        let Some(workspace) = self.workspace_id() else {
            return;
        };
        // 一覧を続けて 2 回押されたときに 2 本目を弾く。切り替えは作業ツリーを
        // 書き換えるので、重ねて走らせると git 側でどちらかが必ず失敗する。
        if self.busy.is_some() {
            return;
        }
        self.branch_menu_open = false;
        self.creating_branch = false;
        self.busy = Some(Busy::Checkout);
        self.branch_input.update(cx, |input, cx| input.clear(cx));
        // 入力欄側の notify では GitView は描き直されない。ここで通知しないと
        // 応答が返るまでメニューが開いたままに見える。
        cx.notify();
        self.send(
            Request::GitCheckout {
                workspace,
                branch,
                create,
            },
            Some(Busy::Checkout),
            "ブランチを切り替えられません",
            cx,
        );
    }

    fn sync(&mut self, push: bool, cx: &mut Context<Self>) {
        let Some(workspace) = self.workspace_id() else {
            return;
        };
        if self.busy.is_some() {
            return;
        }
        let owner = if push { Busy::Push } else { Busy::Pull };
        self.busy = Some(owner);
        cx.notify();
        let (request, failure) = if push {
            (Request::GitPush { workspace }, "プッシュに失敗しました")
        } else {
            (Request::GitPull { workspace }, "プルに失敗しました")
        };
        self.send(request, Some(owner), failure, cx);
    }

    fn toggle_branch_menu(&mut self, cx: &mut Context<Self>) {
        self.branch_menu_open = !self.branch_menu_open;
        self.creating_branch = false;
        if self.branch_menu_open {
            self.load_branches(cx);
            // ブランチを選ぶ前に変更一覧も取り直す。端末で git を叩かれていた場合、
            // 古い一覧のまま切り替えると「消えたはずの変更」が残って見える。
            self.refresh_status(cx);
        }
        cx.notify();
    }

    fn load_branches(&mut self, cx: &mut Context<Self>) {
        let (Some(client), Some(workspace)) = (self.client.clone(), self.workspace_id()) else {
            return;
        };
        cx.spawn(async move |this, cx| {
            let result = client.request(Request::GitListBranches { workspace }).await;
            this.update(cx, |this, cx| {
                match result {
                    Ok(Response::GitBranches(branches)) => this.branches = branches,
                    Ok(_) => {}
                    Err(e) => cx.emit(GitEvent::Notify(
                        NotificationLevel::Warning,
                        format!("ブランチ一覧を取得できません: {e}"),
                    )),
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn toggle_section(&mut self, section: GitSection, cx: &mut Context<Self>) {
        match self.collapsed.iter().position(|s| *s == section) {
            Some(index) => {
                self.collapsed.remove(index);
            }
            None => self.collapsed.push(section),
        }
        self.rebuild_rows();
        cx.notify();
    }

    /// 行のホバー状態を反映する。
    ///
    /// 隣の行へ移るとき「新しい行の進入」が「古い行の離脱」より先に届くことがある。
    /// 離脱を無条件に反映すると、直後に消されて操作ボタンが一瞬で消える。
    /// 自分がまだ対象のときだけ消す。
    fn hover_row(&mut self, index: usize, hovered: bool, cx: &mut Context<Self>) {
        if hovered {
            self.set_hovered(Some(index), cx);
        } else if self.hovered == Some(index) {
            self.set_hovered(None, cx);
        }
    }

    fn set_hovered(&mut self, index: Option<usize>, cx: &mut Context<Self>) {
        // 変化したときだけ通知する。毎回通知すると再描画とホバー判定が往復し続ける。
        if self.hovered != index {
            self.hovered = index;
            cx.notify();
        }
    }

    fn staged_count(&self) -> usize {
        self.status
            .entries
            .iter()
            .filter(|entry| sections_for(entry).contains(&GitSection::Staged))
            .count()
    }

    // -- 入力欄からの通知 --

    fn on_commit_input(
        &mut self,
        _entity: Entity<TextInput>,
        event: &TextInputEvent,
        cx: &mut Context<Self>,
    ) {
        match event {
            TextInputEvent::Submit => self.commit(cx),
            // 枠のネオンはこのビューが描くので、出入りのたびに描き直す。
            TextInputEvent::FocusChanged => cx.notify(),
            TextInputEvent::Changed => {}
            TextInputEvent::Cancel => {
                self.commit_input.update(cx, |input, cx| input.clear(cx));
            }
        }
    }

    fn on_branch_input(
        &mut self,
        _entity: Entity<TextInput>,
        event: &TextInputEvent,
        cx: &mut Context<Self>,
    ) {
        match event {
            TextInputEvent::Submit => {
                let name = self.branch_input.read(cx).text().trim().to_string();
                if name.is_empty() {
                    return;
                }
                self.checkout(name, true, cx);
            }
            TextInputEvent::Cancel => {
                self.creating_branch = false;
                self.branch_input.update(cx, |input, cx| input.clear(cx));
                cx.notify();
            }
            // 枠のネオンはこのビューが描くので、出入りのたびに描き直す。
            TextInputEvent::FocusChanged => cx.notify(),
            TextInputEvent::Changed => {}
        }
    }
}

// ---------------------------------------------------------------------------
// 描画
// ---------------------------------------------------------------------------

/// 一覧の行に置く小さなアイコンボタン。共通の `icon_button` は 28px あり、
/// 24px の行に収まらないのでここだけ小型のものを使う。
fn row_icon_button(
    id: impl Into<ElementId>,
    glyph: Icon,
    color: Hsla,
    theme: &Theme,
) -> Stateful<gpui::Div> {
    h_flex()
        .id(id)
        .justify_center()
        .size(px(18.))
        .rounded(px(4.))
        .child(icon(glyph, px(11.), color))
        .hover(|s| s.bg(theme.bg_surface))
        .cursor_pointer()
}

/// 見出しに置く文字だけの小さなボタン。
fn mini_button(
    id: impl Into<ElementId>,
    label: impl Into<SharedString>,
    theme: &Theme,
) -> Stateful<gpui::Div> {
    h_flex()
        .id(id)
        .h(px(16.))
        .px(px(5.))
        .rounded(px(3.))
        .text_size(px(10.))
        .text_color(theme.accent)
        .bg(theme.accent_soft)
        .cursor_pointer()
        .hover(|s| s.text_color(theme.text))
        .child(label.into())
}

impl GitView {
    fn render_header(&self, cx: &mut Context<Self>) -> AnyElement {
        let theme = theme(cx).clone();
        let branch = self
            .status
            .branch
            .clone()
            .unwrap_or_else(|| "(切り離された HEAD)".to_string());
        let counts = format_sync_counts(self.status.ahead, self.status.behind);
        let busy = self.busy;

        h_flex()
            .h(px(36.))
            .px(px(8.))
            .gap(px(6.))
            .flex_none()
            .justify_between()
            .child(
                h_flex()
                    .id("git-branch")
                    .h(px(24.))
                    .px(px(6.))
                    .gap(px(5.))
                    .rounded(px(6.))
                    .border_1()
                    .border_color(if self.branch_menu_open {
                        theme.border_glow
                    } else {
                        theme.border
                    })
                    .cursor_pointer()
                    .hover(|s| s.bg(theme.bg_overlay))
                    .child(icon(Icon::Branch, px(12.), theme.accent))
                    .child(
                        div()
                            .text_size(px(11.5))
                            .text_color(theme.text)
                            .child(truncate_middle(&branch, 20)),
                    )
                    .when(!counts.is_empty(), |el| {
                        el.child(
                            div()
                                .text_size(px(10.))
                                .text_color(theme.accent_secondary)
                                .child(counts.clone()),
                        )
                    })
                    .child(icon(Icon::ChevronDown, px(10.), theme.text_faint))
                    .on_click(cx.listener(|this, _, _w, cx| this.toggle_branch_menu(cx)))
                    .tooltip(simple_tooltip(tooltip_text::GIT_SWITCH_BRANCH)),
            )
            .child(match busy {
                Some(busy) => h_flex()
                    .h(px(20.))
                    .px(px(8.))
                    .rounded(px(10.))
                    .bg(theme.accent_soft)
                    .text_size(px(10.5))
                    .text_color(theme.accent)
                    .child(busy.label())
                    .into_any_element(),
                // 行の破棄ボタンが Undo を使うので、プルには双方向矢印の Refresh を割り当てる。
                // 同じ形の記号を別の意味で 2 か所に出すと押し間違えるため。
                None => h_flex()
                    .gap(px(2.))
                    .child(
                        icon_button("git-pull", Icon::Refresh, false, cx)
                            .on_click(cx.listener(|this, _, _w, cx| this.sync(false, cx)))
                            .tooltip(simple_tooltip(tooltip_text::GIT_PULL)),
                    )
                    .child(
                        icon_button("git-push", Icon::Send, false, cx)
                            .on_click(cx.listener(|this, _, _w, cx| this.sync(true, cx)))
                            .tooltip(simple_tooltip(tooltip_text::GIT_PUSH)),
                    )
                    .into_any_element(),
            })
            .into_any_element()
    }

    fn render_commit_box(&self, cx: &mut Context<Self>) -> AnyElement {
        let theme = theme(cx).clone();
        let staged = self.staged_count();
        // amend は既存コミットの書き換えなので、ステージ済みが 0 でも意味がある。
        let enabled = self.busy.is_none() && (staged > 0 || self.amend);
        let label = if self.amend {
            "修正してコミット"
        } else {
            "コミット"
        };

        v_flex()
            .px(px(8.))
            .pb(px(8.))
            .gap(px(6.))
            .flex_none()
            .child(
                div()
                    .p(px(6.))
                    .rounded(px(6.))
                    .bg(theme.bg_surface)
                    .border_1()
                    .border_color(theme.border)
                    .overflow_hidden()
                    .text_size(px(12.))
                    .line_height(px(17.))
                    .text_color(theme.text)
                    .cursor(gpui::CursorStyle::IBeam)
                    // 枠の余白を押しても欄へ入れるようにする。
                    .on_mouse_down(MouseButton::Left, {
                        let input = self.commit_input.clone();
                        move |_, window, cx| input.read(cx).focus(window)
                    })
                    .child(self.commit_input.clone()),
            )
            .child(
                h_flex()
                    .justify_between()
                    .gap(px(6.))
                    .child(
                        h_flex()
                            .id("git-amend")
                            .gap(px(5.))
                            .h(px(20.))
                            .px(px(5.))
                            .rounded(px(4.))
                            .cursor_pointer()
                            .hover(|s| s.bg(theme.bg_overlay))
                            .child(
                                h_flex()
                                    .justify_center()
                                    .size(px(13.))
                                    .rounded(px(3.))
                                    .border_1()
                                    .border_color(if self.amend {
                                        theme.accent
                                    } else {
                                        theme.border_strong
                                    })
                                    .when(self.amend, |el| {
                                        el.bg(theme.accent_soft).child(icon(
                                            Icon::Check,
                                            px(9.),
                                            theme.accent,
                                        ))
                                    }),
                            )
                            .child(
                                div()
                                    .text_size(px(11.))
                                    .text_color(if self.amend {
                                        theme.accent
                                    } else {
                                        theme.text_muted
                                    })
                                    .child("変更を修正 (amend)"),
                            )
                            .on_click(cx.listener(|this, _, _w, cx| {
                                this.amend = !this.amend;
                                cx.notify();
                            })),
                    )
                    .child(
                        primary_button("git-commit", label, enabled, cx)
                            .on_click(cx.listener(|this, _, _w, cx| this.commit(cx))),
                    ),
            )
            .into_any_element()
    }

    fn render_list(&self, cx: &mut Context<Self>) -> AnyElement {
        if self.rows.is_empty() {
            return div()
                .flex_1()
                .child(empty_state("変更はありません", cx))
                .into_any_element();
        }
        let count = self.rows.len();
        div()
            .flex_1()
            .overflow_hidden()
            .child(
                uniform_list(
                    "git-rows",
                    count,
                    cx.processor(|this, range: Range<usize>, _window, cx| {
                        range.map(|index| this.render_row(index, cx)).collect()
                    }),
                )
                .h_full(),
            )
            .into_any_element()
    }

    fn render_row(&self, index: usize, cx: &mut Context<Self>) -> AnyElement {
        match self.rows.get(index) {
            Some(GitRow::Header {
                section,
                count,
                collapsed,
            }) => self.render_section_header(index, *section, *count, *collapsed, cx),
            Some(GitRow::File { section, entry }) => {
                self.render_file_row(index, *section, entry.clone(), cx)
            }
            None => div().h(ROW_HEIGHT).into_any_element(),
        }
    }

    fn render_section_header(
        &self,
        index: usize,
        section: GitSection,
        count: usize,
        collapsed: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let theme = theme(cx).clone();
        let hovered = self.hovered == Some(index);
        h_flex()
            .id(("git-row", index))
            .w_full()
            .h(ROW_HEIGHT)
            .px(px(8.))
            .gap(px(5.))
            .cursor_pointer()
            .hover(|s| s.bg(theme.bg_overlay))
            .on_hover(cx.listener(move |this, hovered: &bool, _w, cx| {
                this.hover_row(index, *hovered, cx);
            }))
            .on_click(cx.listener(move |this, _, _w, cx| this.toggle_section(section, cx)))
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
                    .font_weight(gpui::FontWeight::SEMIBOLD)
                    .text_color(theme.text_muted)
                    .child(section.title()),
            )
            .child(
                div()
                    .text_size(px(10.))
                    .text_color(theme.text_faint)
                    .child(count.to_string()),
            )
            .child(div().flex_1())
            .when(hovered, |el| {
                el.child(
                    mini_button(("git-bulk", index), section.bulk_label(), &theme).on_click(
                        cx.listener(move |this, _, _w, cx| {
                            cx.stop_propagation();
                            this.bulk(section, cx);
                        }),
                    ),
                )
            })
            .into_any_element()
    }

    fn render_file_row(
        &self,
        index: usize,
        section: GitSection,
        entry: GitFileStatus,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let theme = theme(cx).clone();
        let hovered = self.hovered == Some(index);
        let code = code_for(&entry, section);
        let (name, parent) = split_path_display(&entry.path);
        let absolute = self.absolute(&entry.path);
        let display_name = truncate_middle(&name, 26);
        let display_parent = truncate_middle(&parent, 22);

        list_row(("git-row", index), false, cx)
            .h(ROW_HEIGHT)
            .gap(px(5.))
            .overflow_hidden()
            .on_hover(cx.listener(move |this, hovered: &bool, _w, cx| {
                this.hover_row(index, *hovered, cx);
            }))
            .on_click({
                let absolute = absolute.clone();
                cx.listener(move |_this, _, _w, cx| {
                    cx.emit(GitEvent::OpenFile(absolute.clone()));
                })
            })
            .child(
                div()
                    .w(px(11.))
                    .flex_none()
                    .text_size(px(11.))
                    .font_weight(gpui::FontWeight::BOLD)
                    .text_color(status_color(code, &theme))
                    .child(status_char(code).to_string()),
            )
            .child(
                div()
                    .flex_none()
                    .text_size(px(12.))
                    .text_color(if code == GitStatusCode::Deleted {
                        theme.text_muted
                    } else {
                        theme.text
                    })
                    .child(display_name),
            )
            .when(!display_parent.is_empty(), |el| {
                el.child(
                    div()
                        .text_size(px(10.5))
                        .text_color(theme.text_faint)
                        .overflow_hidden()
                        .child(display_parent),
                )
            })
            .child(div().flex_1())
            .when(hovered, |el| {
                el.child(self.render_row_actions(index, section, absolute, name, cx))
            })
            .into_any_element()
    }

    /// 行のホバーで出す操作ボタン。親のクリック (ファイルを開く) に流さないよう
    /// いずれも伝播を止める。
    fn render_row_actions(
        &self,
        index: usize,
        section: GitSection,
        absolute: PathBuf,
        name: String,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let theme = theme(cx).clone();
        let mut row = h_flex().gap(px(1.)).flex_none();

        if section != GitSection::Staged {
            let target = absolute.clone();
            row = row.child(
                row_icon_button(
                    ("git-discard", index),
                    Icon::Undo,
                    theme.git_deleted,
                    &theme,
                )
                .on_click(cx.listener(move |this, _, _w, cx| {
                    cx.stop_propagation();
                    this.discard = Some((target.clone(), name.clone()));
                    cx.notify();
                }))
                .tooltip(simple_tooltip(tooltip_text::GIT_DISCARD)),
            );
        }
        row = match section {
            GitSection::Staged => {
                let target = absolute.clone();
                row.child(
                    row_icon_button(
                        ("git-unstage", index),
                        Icon::Close,
                        theme.text_muted,
                        &theme,
                    )
                    .on_click(cx.listener(move |this, _, _w, cx| {
                        cx.stop_propagation();
                        this.unstage(vec![target.clone()], cx);
                    }))
                    .tooltip(simple_tooltip(tooltip_text::GIT_UNSTAGE)),
                )
            }
            _ => {
                let target = absolute.clone();
                row.child(
                    row_icon_button(("git-stage", index), Icon::Plus, theme.accent, &theme)
                        .on_click(cx.listener(move |this, _, _w, cx| {
                            cx.stop_propagation();
                            this.stage(vec![target.clone()], cx);
                        }))
                        .tooltip(simple_tooltip(tooltip_text::GIT_STAGE)),
                )
            }
        };
        row.into_any_element()
    }

    fn render_branch_menu(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        if !self.branch_menu_open {
            return None;
        }
        let theme = theme(cx).clone();
        let branches = self.branches.clone();
        let count = branches.len();
        // 一覧の高さは行数に合わせるが、上限を設けてパネルを覆い尽くさないようにする。
        let list_height = px((count.clamp(1, 9) as f32) * 24.);

        Some(
            div()
                .absolute()
                .top_0()
                .left_0()
                .size_full()
                // 背後を覆う板。ここを押すと閉じる。
                .child(
                    div()
                        .id("git-branch-backdrop")
                        .absolute()
                        .top_0()
                        .left_0()
                        .size_full()
                        .on_click(cx.listener(|this, _, _w, cx| {
                            this.branch_menu_open = false;
                            cx.notify();
                        })),
                )
                .child(
                    v_flex()
                        .absolute()
                        // パネル見出し (32) + アクセント線 (1) + ヘッダ内のブランチ表示の下端。
                        .top(px(66.))
                        .left(px(8.))
                        .right(px(8.))
                        .rounded(px(8.))
                        .bg(theme.bg_overlay)
                        .border_1()
                        .border_color(theme.border_glow)
                        .overflow_hidden()
                        .child(nebula_accent_line(theme.accent))
                        .child(
                            h_flex()
                                .id("git-new-branch")
                                .h(px(26.))
                                .px(px(8.))
                                .gap(px(5.))
                                .cursor_pointer()
                                .hover(|s| s.bg(theme.bg_surface))
                                .child(icon(Icon::Plus, px(11.), theme.accent))
                                .child(
                                    div()
                                        .text_size(px(11.5))
                                        .text_color(theme.accent)
                                        .child("新しいブランチ"),
                                )
                                .on_click(cx.listener(|this, _, window, cx| {
                                    this.creating_branch = true;
                                    this.branch_input.read(cx).focus(window);
                                    cx.notify();
                                })),
                        )
                        .when(self.creating_branch, |el| {
                            el.child(
                                v_flex()
                                    .px(px(8.))
                                    .pb(px(6.))
                                    .gap(px(3.))
                                    .child(
                                        div()
                                            .px(px(5.))
                                            .py(px(3.))
                                            .rounded(px(4.))
                                            .bg(theme.bg_surface)
                                            .border_1()
                                            .border_color(theme.border_glow)
                                            .overflow_hidden()
                                            .text_size(px(12.))
                                            .line_height(px(17.))
                                            .text_color(theme.text)
                                            .cursor(gpui::CursorStyle::IBeam)
                                            // 枠の余白を押しても欄へ入れるようにする。
                                            .on_mouse_down(MouseButton::Left, {
                                                let input = self.branch_input.clone();
                                                move |_, window, cx| input.read(cx).focus(window)
                                            })
                                            .child(self.branch_input.clone()),
                                    )
                                    .child(
                                        div()
                                            .text_size(px(10.))
                                            .text_color(theme.text_faint)
                                            .child(format!(
                                                "{} で作成して切り替え / {} で取消",
                                                format_keystroke("enter"),
                                                format_keystroke("escape")
                                            )),
                                    ),
                            )
                        })
                        .child(div().h(px(1.)).w_full().bg(theme.border))
                        .child(if count == 0 {
                            div()
                                .h(px(26.))
                                .px(px(8.))
                                .text_size(px(11.))
                                .text_color(theme.text_faint)
                                .child("ブランチを読み込んでいます…")
                                .into_any_element()
                        } else {
                            div()
                                .h(list_height)
                                .child(
                                    uniform_list(
                                        "git-branches",
                                        count,
                                        cx.processor(
                                            move |this, range: Range<usize>, _window, cx| {
                                                range
                                                    .map(|index| this.render_branch_row(index, cx))
                                                    .collect()
                                            },
                                        ),
                                    )
                                    .h_full(),
                                )
                                .into_any_element()
                        }),
                )
                .into_any_element(),
        )
    }

    fn render_branch_row(&self, index: usize, cx: &mut Context<Self>) -> AnyElement {
        let theme = theme(cx).clone();
        let Some(branch) = self.branches.get(index) else {
            return div().h(px(24.)).into_any_element();
        };
        let name = branch.name.clone();
        let summary = branch.last_commit_summary.clone();
        let is_head = branch.is_head;
        let is_remote = branch.is_remote;
        let target = checkout_name(branch);

        h_flex()
            .id(("git-branch-row", index))
            .w_full()
            .h(px(24.))
            .px(px(8.))
            .gap(px(5.))
            .cursor_pointer()
            .overflow_hidden()
            .hover(|s| s.bg(theme.bg_surface))
            .when(is_head, |el| el.bg(theme.accent_soft))
            .child(div().w(px(11.)).flex_none().when(is_head, |el| {
                el.child(icon(Icon::Check, px(10.), theme.accent))
            }))
            .child(
                div()
                    .flex_none()
                    .text_size(px(11.5))
                    .text_color(if is_remote {
                        theme.text_muted
                    } else {
                        theme.text
                    })
                    .child(truncate_middle(&name, 24)),
            )
            .child(
                div()
                    .flex_1()
                    .overflow_hidden()
                    .text_size(px(10.))
                    .text_color(theme.text_faint)
                    .child(truncate_middle(&summary, 24)),
            )
            .on_click(cx.listener(move |this, _, _w, cx| {
                if is_head {
                    this.branch_menu_open = false;
                    cx.notify();
                    return;
                }
                this.checkout(target.clone(), false, cx);
            }))
            .into_any_element()
    }

    fn render_discard_confirm(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let (_, name) = self.discard.as_ref()?;
        let theme = theme(cx).clone();
        let name = name.clone();
        Some(
            v_flex()
                .id("git-discard-overlay")
                .absolute()
                .top_0()
                .left_0()
                .size_full()
                .items_center()
                .justify_center()
                .p(px(12.))
                .bg(theme.bg_void.opacity(0.75))
                // 暗くした余白を押しても下のファイル行に届かせない。確認中に
                // 別のファイルが開くと、どれを破棄しようとしていたか分からなくなる。
                .on_click(|_, _w, cx| cx.stop_propagation())
                .child(
                    v_flex()
                        .w_full()
                        .gap(px(8.))
                        .p(px(12.))
                        .rounded(px(8.))
                        .bg(theme.bg_overlay)
                        .border_1()
                        .border_color(theme.git_deleted)
                        .child(
                            div()
                                .text_size(px(12.))
                                .text_color(theme.text)
                                .child(format!(
                                    "{} の変更を破棄しますか?",
                                    truncate_middle(&name, 24)
                                )),
                        )
                        .child(
                            div()
                                .text_size(px(10.5))
                                .text_color(theme.text_faint)
                                .child("この操作は取り消せません。"),
                        )
                        .child(
                            h_flex()
                                .gap(px(6.))
                                .justify_end()
                                .child(
                                    h_flex()
                                        .id("git-discard-cancel")
                                        .h(px(24.))
                                        .px(px(10.))
                                        .justify_center()
                                        .rounded(px(5.))
                                        .text_size(px(11.5))
                                        .text_color(theme.text_muted)
                                        .border_1()
                                        .border_color(theme.border)
                                        .cursor_pointer()
                                        .hover(|s| s.bg(theme.bg_surface))
                                        .child("やめる")
                                        .on_click(cx.listener(|this, _, _w, cx| {
                                            this.discard = None;
                                            cx.notify();
                                        })),
                                )
                                .child(
                                    h_flex()
                                        .id("git-discard-ok")
                                        .h(px(24.))
                                        .px(px(10.))
                                        .justify_center()
                                        .rounded(px(5.))
                                        .text_size(px(11.5))
                                        .text_color(theme.text_inverse)
                                        .bg(theme.git_deleted)
                                        .cursor_pointer()
                                        .hover(|s| s.bg(theme.error))
                                        .child("破棄する")
                                        .on_click(
                                            cx.listener(|this, _, _w, cx| this.confirm_discard(cx)),
                                        ),
                                ),
                        ),
                )
                .into_any_element(),
        )
    }
}

impl Render for GitView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = theme(cx).clone();

        if self.repo_root().is_none() {
            return v_flex()
                .size_full()
                .child(panel_header("ソース管理", cx))
                .child(empty_state(
                    "このフォルダは git リポジトリではありません。\n\
                     git init を実行するか、管理下のフォルダを開いてください。",
                    cx,
                ))
                .into_any_element();
        }

        let progress = self.status.in_progress.clone();

        v_flex()
            .size_full()
            .relative()
            .overflow_hidden()
            .child(panel_header("ソース管理", cx))
            .child(nebula_accent_line(theme.accent_soft))
            .child(self.render_header(cx))
            .when_some(progress, |el, progress| {
                // リベース中・マージ中は操作の意味が変わるので、常に目に入る位置に出す。
                el.child(
                    h_flex()
                        .h(px(20.))
                        .px(px(8.))
                        .flex_none()
                        .bg(theme.accent_soft)
                        .text_size(px(10.5))
                        .text_color(theme.accent_secondary)
                        .child(format!("{progress} が進行中")),
                )
            })
            .child(self.render_commit_box(cx))
            .child(self.render_list(cx))
            .children(self.render_branch_menu(cx))
            .children(self.render_discard_confirm(cx))
            .into_any_element()
    }
}

// ---------------------------------------------------------------------------
// テスト
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(path: &str, index: GitStatusCode, worktree: GitStatusCode) -> GitFileStatus {
        GitFileStatus {
            path: PathBuf::from(path),
            original_path: None,
            index,
            worktree,
        }
    }

    fn status(entries: Vec<GitFileStatus>) -> GitRepoStatus {
        GitRepoStatus {
            entries,
            ..Default::default()
        }
    }

    #[test]
    fn 三つの区分が順に並ぶ() {
        let status = status(vec![
            entry("a.rs", GitStatusCode::Added, GitStatusCode::Unmodified),
            entry("b.rs", GitStatusCode::Unmodified, GitStatusCode::Modified),
            entry("c.rs", GitStatusCode::Untracked, GitStatusCode::Untracked),
        ]);
        let rows = flatten_rows(&status, &[]);
        assert_eq!(rows.len(), 6, "見出し 3 + ファイル 3");
        let sections: Vec<GitSection> = rows
            .iter()
            .filter_map(|row| match row {
                GitRow::Header { section, .. } => Some(*section),
                _ => None,
            })
            .collect();
        assert_eq!(
            sections,
            vec![
                GitSection::Staged,
                GitSection::Changed,
                GitSection::Untracked
            ]
        );
    }

    #[test]
    fn 変更が無い区分は見出しごと出さない() {
        let status = status(vec![entry(
            "a.rs",
            GitStatusCode::Unmodified,
            GitStatusCode::Modified,
        )]);
        let rows = flatten_rows(&status, &[]);
        assert_eq!(rows.len(), 2);
        assert!(matches!(
            rows[0],
            GitRow::Header {
                section: GitSection::Changed,
                count: 1,
                ..
            }
        ));
    }

    #[test]
    fn 部分ステージのファイルは両方の区分に出る() {
        let status = status(vec![entry(
            "a.rs",
            GitStatusCode::Modified,
            GitStatusCode::Modified,
        )]);
        let rows = flatten_rows(&status, &[]);
        let files: Vec<GitSection> = rows
            .iter()
            .filter_map(|row| match row {
                GitRow::File { section, .. } => Some(*section),
                _ => None,
            })
            .collect();
        assert_eq!(files, vec![GitSection::Staged, GitSection::Changed]);
    }

    #[test]
    fn 衝突中のファイルは変更にだけ出る() {
        let status = status(vec![entry(
            "a.rs",
            GitStatusCode::Conflicted,
            GitStatusCode::Conflicted,
        )]);
        let rows = flatten_rows(&status, &[]);
        assert_eq!(rows.len(), 2);
        assert!(matches!(
            rows[1],
            GitRow::File {
                section: GitSection::Changed,
                ..
            }
        ));
        assert_eq!(
            code_for(&status.entries[0], GitSection::Changed),
            GitStatusCode::Conflicted
        );
    }

    #[test]
    fn 折りたたむと見出しだけが残る() {
        let status = status(vec![
            entry("a.rs", GitStatusCode::Added, GitStatusCode::Unmodified),
            entry("b.rs", GitStatusCode::Unmodified, GitStatusCode::Modified),
        ]);
        let rows = flatten_rows(&status, &[GitSection::Staged]);
        assert_eq!(rows.len(), 3, "畳んだ側は見出しのみ");
        assert!(matches!(
            rows[0],
            GitRow::Header {
                section: GitSection::Staged,
                count: 1,
                collapsed: true
            }
        ));
    }

    #[test]
    fn 変更が無ければ行も無い() {
        assert!(flatten_rows(&GitRepoStatus::default(), &[]).is_empty());
    }

    #[test]
    fn 状態コードが一文字に写る() {
        assert_eq!(status_char(GitStatusCode::Modified), 'M');
        assert_eq!(status_char(GitStatusCode::Added), 'A');
        assert_eq!(status_char(GitStatusCode::Deleted), 'D');
        assert_eq!(status_char(GitStatusCode::Renamed), 'R');
        assert_eq!(status_char(GitStatusCode::Untracked), '?');
        assert_eq!(status_char(GitStatusCode::Conflicted), 'U');
    }

    #[test]
    fn ステージ済みの行は索引側の状態を出す() {
        let e = entry("a.rs", GitStatusCode::Added, GitStatusCode::Modified);
        assert_eq!(code_for(&e, GitSection::Staged), GitStatusCode::Added);
        assert_eq!(code_for(&e, GitSection::Changed), GitStatusCode::Modified);
    }

    #[test]
    fn パスをファイル名と親に分ける() {
        let (name, parent) = split_path_display(Path::new("src/views/git.rs"));
        assert_eq!(name, "git.rs");
        assert_eq!(parent, "src/views");
        let (name, parent) = split_path_display(Path::new("README.md"));
        assert_eq!(name, "README.md");
        assert_eq!(parent, "");
    }

    #[test]
    fn 上流との差を矢印で表す() {
        assert_eq!(format_sync_counts(0, 0), "");
        assert_eq!(format_sync_counts(2, 0), "↑2");
        assert_eq!(format_sync_counts(0, 3), "↓3");
        assert_eq!(format_sync_counts(2, 3), "↑2 ↓3");
    }

    #[test]
    fn 遠隔ブランチは接頭辞を外して切り替える() {
        let remote = GitBranch {
            name: "origin/feature/x".into(),
            is_head: false,
            is_remote: true,
            upstream: None,
            last_commit_summary: String::new(),
        };
        assert_eq!(checkout_name(&remote), "feature/x");
        let local = GitBranch {
            is_remote: false,
            name: "main".into(),
            ..remote
        };
        assert_eq!(checkout_name(&local), "main");
    }
}
