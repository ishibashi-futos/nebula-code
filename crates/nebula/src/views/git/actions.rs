//! バックエンドへ副作用を投げる操作群 (ステージ・コミット・ブランチ切り替え等)。
//!
//! `GitView` の状態を直接書き換える側であり、`rows` モジュールの純粋関数と違って
//! `git` CLI を呼ぶ IPC 通信を伴う。描画コードから分けることで、
//! 「何が状態を変えるか」を一望できるようにしてある。

use super::rows::{GitSection, flatten_rows, sections_for};
use super::{Busy, GitEvent, GitView};
use crate::ipc_client::BackendClient;
use crate::ui::{TextInput, TextInputEvent};
use gpui::prelude::*;
use gpui::{Context, Entity};
use nebula_protocol::{
    Event, GitRepoStatus, NotificationLevel, Request, Response, WorkspaceId, WorkspaceInfo,
};
use std::path::{Path, PathBuf};

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

    pub(super) fn repo_root(&self) -> Option<&PathBuf> {
        self.workspace.as_ref()?.git_root.as_ref()
    }

    /// 一覧の相対パスを絶対パスに直す。エディタで開くにも git に渡すにも絶対パスを使う。
    pub(super) fn absolute(&self, path: &Path) -> PathBuf {
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

    pub(super) fn stage(&mut self, paths: Vec<PathBuf>, cx: &mut Context<Self>) {
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

    pub(super) fn unstage(&mut self, paths: Vec<PathBuf>, cx: &mut Context<Self>) {
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
    pub(super) fn bulk(&mut self, section: GitSection, cx: &mut Context<Self>) {
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

    pub(super) fn confirm_discard(&mut self, cx: &mut Context<Self>) {
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

    pub(super) fn commit(&mut self, cx: &mut Context<Self>) {
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

    pub(super) fn checkout(&mut self, branch: String, create: bool, cx: &mut Context<Self>) {
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

    pub(super) fn sync(&mut self, push: bool, cx: &mut Context<Self>) {
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

    pub(super) fn toggle_branch_menu(&mut self, cx: &mut Context<Self>) {
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

    pub(super) fn toggle_section(&mut self, section: GitSection, cx: &mut Context<Self>) {
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
    pub(super) fn hover_row(&mut self, index: usize, hovered: bool, cx: &mut Context<Self>) {
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

    pub(super) fn staged_count(&self) -> usize {
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
