//! エクスプローラー (ファイルツリー)。
//!
//! ツリーは **1 階層ずつ遅延展開する**。ワークスペースを開いた時点で全階層を舐めると、
//! node_modules や target を抱えたリポジトリで冷間起動が数秒単位で伸びる。
//! バックエンドに投げるのは常に「今開いたフォルダの直下」だけ。
//!
//! 取得済みの階層は `children` に、開いている階層は `expanded` に持ち、
//! この 2 つから表示行の平坦な列 (`rows`) を組み立てる。組み立ては純粋関数
//! [`flatten`] に切り出してあり、描画から独立して検査できる。
//!
//! 行数はリポジトリ次第で数千に達するので、描画は `uniform_list` に任せて
//! 可視範囲だけを作る。

use crate::assets::Icon;
use crate::ipc_client::BackendClient;
use crate::theme::theme;
use crate::ui::{
    NewlinePolicy, TextInput, TextInputEvent, empty_state, ghost_button, h_flex, icon, icon_button,
    list_row, nebula_accent_line, panel_header, primary_button, simple_tooltip, tooltip_text,
    v_flex,
};
use crate::views::git::{GitSection, code_for, status_char, status_color};
use gpui::prelude::*;
use gpui::{
    AnyElement, App, ClipboardItem, Context, ElementId, Entity, EventEmitter, FocusHandle,
    Focusable, Hsla, MouseButton, MouseDownEvent, Pixels, Point, SharedString, Subscription,
    Window, anchored, deferred, div, px, uniform_list,
};
use nebula_protocol::{
    DirEntry, Event, FileChange, FileChangeKind, GitFileStatus, GitRepoStatus, NotificationLevel,
    Request, Response, WorkspaceInfo,
};
use std::collections::{HashMap, HashSet};
use std::ops::Range;
use std::path::{Path, PathBuf};

/// シェルへ伝える出来事。
pub enum ExplorerEvent {
    OpenFile(PathBuf),
    Notify(NotificationLevel, String),
    /// 隠しファイルの表示設定が変わった。セッションへの保存はシェル側の責務なので、
    /// このビュー自身は保存しない (`sidebar_visible` 等と同じ役割分担)。
    HiddenToggled,
}

/// 行の高さ。`uniform_list` は先頭行を測って全行に適用するため、
/// 通常行と入力中の行で必ず同じ値を使う。
const ROW_HEIGHT: Pixels = px(22.);
/// 1 段ぶんのインデント。
const INDENT_STEP: f32 = 12.;

// ---------------------------------------------------------------------------
// 純粋なツリー整形ロジック
// ---------------------------------------------------------------------------

/// 平坦化された表示行 1 つ。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TreeRow {
    /// 実在する項目のパス。仮行の場合は「作成先の親フォルダ」。
    pub path: PathBuf,
    pub name: String,
    pub depth: usize,
    pub is_dir: bool,
    pub is_ignored: bool,
    /// 新規作成の入力欄だけを出す仮の行か。
    pub is_draft: bool,
}

/// フォルダ直下の並び順を決める。
///
/// フォルダを先に、その中は大文字小文字を無視した名前順。バックエンドの返す順は
/// ファイルシステム依存なので、表示側で必ず正規化する。
fn sort_entries(mut entries: Vec<DirEntry>) -> Vec<DirEntry> {
    entries.sort_by(|a, b| {
        b.is_dir
            .cmp(&a.is_dir)
            .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
            .then_with(|| a.name.cmp(&b.name))
    });
    entries
}

/// 取得済みの階層と展開状態から、画面に出す行の並びを作る。
///
/// `show_hidden` が `false` のときは `.` で始まる項目 (`.git`・`.gitignore`・`.env` 等) を
/// 丸ごと落とす。フォルダ自身を落とせばその中身を再帰的に辿る必要も無いので、
/// 判定は `push_level` のループ先頭 1 箇所で足りる。
fn flatten(
    root: &Path,
    children: &HashMap<PathBuf, Vec<DirEntry>>,
    expanded: &HashSet<PathBuf>,
    show_hidden: bool,
) -> Vec<TreeRow> {
    let mut rows = Vec::new();
    push_level(root, 0, children, expanded, show_hidden, &mut rows);
    rows
}

fn push_level(
    dir: &Path,
    depth: usize,
    children: &HashMap<PathBuf, Vec<DirEntry>>,
    expanded: &HashSet<PathBuf>,
    show_hidden: bool,
    rows: &mut Vec<TreeRow>,
) {
    let Some(entries) = children.get(dir) else {
        return;
    };
    for entry in entries {
        if !show_hidden && entry.name.starts_with('.') {
            continue;
        }
        rows.push(TreeRow {
            path: entry.path.clone(),
            name: entry.name.clone(),
            depth,
            is_dir: entry.is_dir,
            is_ignored: entry.is_ignored,
            is_draft: false,
        });
        // 未取得のフォルダは展開済みでも子が無いので、そのまま何も足されない。
        if entry.is_dir && expanded.contains(&entry.path) {
            push_level(
                &entry.path,
                depth + 1,
                children,
                expanded,
                show_hidden,
                rows,
            );
        }
    }
}

/// 新規作成の入力欄を出す仮行を差し込む。
///
/// 差し込み先は親フォルダの直下先頭。名前が決まる前は並び順が定まらないので、
/// 一旦は先頭に置いて、作成が終わったら通常の並びに吸収させる。
fn insert_draft_row(rows: &mut Vec<TreeRow>, root: &Path, parent: &Path, is_dir: bool) {
    let (index, depth) = match rows.iter().position(|r| r.path == parent) {
        Some(i) => (i + 1, rows[i].depth + 1),
        // 親がルート自身なら先頭。行が無いフォルダ (未取得) も先頭に出す。
        None if parent == root => (0, 0),
        None => (rows.len(), 0),
    };
    rows.insert(
        index,
        TreeRow {
            path: parent.to_path_buf(),
            name: String::new(),
            depth,
            is_dir,
            is_ignored: false,
            is_draft: true,
        },
    );
}

/// ファイル変更の通知から、読み直すべきフォルダを重複なく求める。
///
/// 全体を読み直さないのは、開いている階層が多いほど無駄な往復が増えるため。
fn affected_dirs(changes: &[FileChange]) -> Vec<PathBuf> {
    let mut dirs: Vec<PathBuf> = Vec::new();
    let mut push = |path: Option<&Path>| {
        if let Some(dir) = path.and_then(|p| p.parent()) {
            let dir = dir.to_path_buf();
            if !dirs.contains(&dir) {
                dirs.push(dir);
            }
        }
    };
    for change in changes {
        push(Some(change.path.as_path()));
        if change.kind == FileChangeKind::Renamed {
            push(change.to.as_deref());
        }
    }
    dirs
}

/// 変更のあるファイルの祖先ディレクトリをすべて集める。
///
/// ロールアップ表示 (「配下に変更あり」) は未展開のフォルダでも効かせたいが、
/// `render_row` のたびに `entries` 全件を舐めては行数 × 件数のコストがかかる。
/// ここで 1 回だけ求めて `HashSet` にしておけば、行ごとの判定は O(1) の参照で済む。
///
/// `root` (ワークスペース直下) より上へは辿らない。ツリーに出ない祖先まで
/// 集めても使い道が無いうえ、`git_root` がワークスペースの祖先にある場合に
/// ファイルシステムの根まで際限なく遡らないための歯止めにもなる。
fn changed_ancestor_dirs(root: &Path, paths: &[PathBuf]) -> HashSet<PathBuf> {
    let mut set = HashSet::new();
    for path in paths {
        for ancestor in path.ancestors().skip(1) {
            if ancestor == root || !ancestor.starts_with(root) {
                break;
            }
            // 既に登録済みなら、そこから上はどのみち前回の探索で入っている。
            if !set.insert(ancestor.to_path_buf()) {
                break;
            }
        }
    }
    set
}

/// 入力された名前を検証する。問題があれば利用者に見せる文言を返す。
fn validate_name(name: &str) -> Result<&str, String> {
    let trimmed = name.trim();
    if trimmed.is_empty() {
        return Err("名前を入力してください".into());
    }
    if trimmed.contains('/') {
        return Err("名前に / は使えません".into());
    }
    if trimmed == "." || trimmed == ".." {
        return Err("その名前は使えません".into());
    }
    Ok(trimmed)
}

/// 複製先のパス。`foo.rs` → `foo copy.rs`。
///
/// 拡張子の前に付けるのは、複製しても言語判定とアイコンが変わらないようにするため。
fn duplicate_target(path: &Path) -> PathBuf {
    let parent = path.parent().unwrap_or(Path::new(""));
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_default();
    // 先頭のドットは拡張子ではなく隠しファイルの印なので、区切りとして数えない。
    let dot = name
        .char_indices()
        .skip(1)
        .filter(|(_, c)| *c == '.')
        .map(|(i, _)| i)
        .last();
    let renamed = match dot {
        Some(i) => format!("{} copy{}", &name[..i], &name[i..]),
        None => format!("{name} copy"),
    };
    parent.join(renamed)
}

// ---------------------------------------------------------------------------
// ビューの状態
// ---------------------------------------------------------------------------

/// 入力欄が何のために開いているか。
#[derive(Debug, Clone, PartialEq, Eq)]
enum EditKind {
    /// 既存項目の名前の変更。
    Rename(PathBuf),
    /// 親フォルダ直下への新規作成。
    Create { parent: PathBuf, is_dir: bool },
}

struct Editing {
    kind: EditKind,
    input: Entity<TextInput>,
    /// 保持しないと購読が即座に解除される。
    _subscription: Subscription,
}

struct ContextMenu {
    /// ウィンドウ座標。マウス位置をそのまま使う。
    position: Point<Pixels>,
    path: PathBuf,
    is_dir: bool,
}

pub struct ExplorerView {
    client: Option<BackendClient>,
    workspace: Option<WorkspaceInfo>,
    /// フォルダ → その直下の項目 (取得済みのものだけ)。
    children: HashMap<PathBuf, Vec<DirEntry>>,
    expanded: HashSet<PathBuf>,
    /// `.` で始まる項目を出すか。既定は非表示 — `.git`・`.gitignore`・`.env` のような
    /// 運用上の付随物は初見では雑音になりやすいので、必要なときだけ明示的に開く形にする。
    show_hidden: bool,
    /// 表示行。状態が変わったときだけ組み直す。
    rows: Vec<TreeRow>,
    selected: Option<PathBuf>,
    /// 読み取り要求が飛んでいるフォルダ。多重要求を防ぐ。
    loading: HashSet<PathBuf>,
    /// 直近の git 状態を絶対パスで引けるようにした索引。ファイル行のバッジ用。
    ///
    /// `GitRepoStatus` そのものは保持しない。`entries` 以外の情報 (branch 等) を
    /// このビューが使わないので、丸ごと持つと読まれないフィールドが残ってしまう。
    git_by_path: HashMap<PathBuf, GitFileStatus>,
    /// 配下に変更のあるディレクトリの絶対パス。未展開のフォルダでもロールアップの
    /// 点を出すために使う。git 状態が変わるたびに [`changed_ancestor_dirs`] で作り直す。
    changed_ancestors: HashSet<PathBuf>,
    menu: Option<ContextMenu>,
    editing: Option<Editing>,
    /// 一覧のスクロール位置。新規作成の入力欄を画面内へ送るために持つ。
    ///
    /// `uniform_list` は可視範囲しか要素を作らないので、入力欄の行が画面外にあると
    /// 描画されず、フォーカスが当たっていても打鍵が一切入らない。
    scroll: gpui::UniformListScrollHandle,
    /// 削除確認中の対象。
    pending_delete: Option<(PathBuf, bool)>,
    focus_handle: FocusHandle,
}

impl EventEmitter<ExplorerEvent> for ExplorerView {}

impl ExplorerView {
    pub fn new(cx: &mut Context<Self>) -> Self {
        Self {
            client: None,
            workspace: None,
            children: HashMap::new(),
            expanded: HashSet::new(),
            show_hidden: false,
            rows: Vec::new(),
            selected: None,
            loading: HashSet::new(),
            git_by_path: HashMap::new(),
            changed_ancestors: HashSet::new(),
            menu: None,
            editing: None,
            scroll: gpui::UniformListScrollHandle::default(),
            pending_delete: None,
            focus_handle: cx.focus_handle(),
        }
    }

    pub fn set_client(&mut self, client: BackendClient, cx: &mut Context<Self>) {
        self.client = Some(client);
        // 接続より先にワークスペースが決まっている場合があるので、ここでも読みに行く。
        if let Some(root) = self.root() {
            self.read_dir(root, cx);
        }
        cx.notify();
    }

    pub fn set_workspace(&mut self, workspace: WorkspaceInfo, cx: &mut Context<Self>) {
        // ワークスペースの切り替えでは前の木を一切引き継がない。パスが混ざると
        // 存在しない行を掴んだまま操作を投げてしまう。
        let root = workspace.root.clone();
        self.workspace = Some(workspace);
        self.children.clear();
        self.expanded.clear();
        self.loading.clear();
        self.rows.clear();
        self.selected = None;
        self.menu = None;
        self.editing = None;
        self.pending_delete = None;
        // 前のワークスペースの git 状態を引き継がない。`activate_workspace` は
        // 既に開いているワークスペースへ切り替えるとき新しい `OpenWorkspace` 要求を
        // 送らない (= バックエンドからの初回 `GitStatusChanged` が再発火しない) ので、
        // 受け身の購読だけに頼ると前のリポジトリの状態が残ってしまう。
        // `read_dir` を毎回呼び直すのと同じ理由で、ここでも自分から取りに行く。
        self.apply_git_status(GitRepoStatus::default());
        self.read_dir(root, cx);
        self.refresh_git_status(cx);
        cx.notify();
    }

    pub fn handle_event(&mut self, event: &Event, cx: &mut Context<Self>) {
        match event {
            Event::FilesChanged { workspace, changes } => {
                // 別ワークスペースの通知も届くので、自分のものだけ拾う。
                if self.workspace.as_ref().map(|w| w.id) != Some(*workspace) {
                    return;
                }
                for change in changes {
                    if change.kind == FileChangeKind::Removed {
                        self.purge_under(&change.path);
                    }
                }
                for dir in affected_dirs(changes) {
                    // 取得していない階層は展開されていないので読み直す必要がない。
                    if self.children.contains_key(&dir) {
                        self.read_dir(dir, cx);
                    }
                }
                self.rebuild_rows();
                cx.notify();
            }
            Event::GitStatusChanged { workspace, status } => {
                if self.workspace.as_ref().map(|w| w.id) != Some(*workspace) {
                    return;
                }
                self.apply_git_status(status.clone());
                cx.notify();
            }
            _ => {}
        }
    }

    fn root(&self) -> Option<PathBuf> {
        self.workspace.as_ref().map(|w| w.root.clone())
    }

    /// git リポジトリのルート。ワークスペースが管理下に無ければ `None`。
    fn git_root(&self) -> Option<&PathBuf> {
        self.workspace.as_ref()?.git_root.as_ref()
    }

    /// `GitFileStatus.path` (repo ルート相対) を絶対パスに直す。
    /// `GitView::absolute` と同じ式 (`views/git.rs`)。
    fn absolute(&self, path: &Path) -> PathBuf {
        match self.git_root() {
            Some(root) => root.join(path),
            None => path.to_path_buf(),
        }
    }

    /// 取得した git 状態を反映し、バッジ・ロールアップ用の索引を作り直す。
    ///
    /// `rows` 自体は git 状態に左右されないので組み直さない。ここで作るのは
    /// 描画のたびに `entries` を舐め直さずに済む 2 つの索引だけ:
    /// ファイルの絶対パス → 状態 (バッジ用) と、変更のある祖先ディレクトリの集合
    /// (未展開のフォルダでも効く「配下に変更あり」用)。
    fn apply_git_status(&mut self, status: GitRepoStatus) {
        let mut by_path = HashMap::with_capacity(status.entries.len());
        let mut absolute_paths = Vec::with_capacity(status.entries.len());
        for entry in status.entries {
            let absolute = self.absolute(&entry.path);
            absolute_paths.push(absolute.clone());
            by_path.insert(absolute, entry);
        }
        self.changed_ancestors = self
            .root()
            .map(|root| changed_ancestor_dirs(&root, &absolute_paths))
            .unwrap_or_default();
        self.git_by_path = by_path;
    }

    /// 隠しファイルを表示しているか。セッションへの書き出しに使う。
    pub fn show_hidden(&self) -> bool {
        self.show_hidden
    }

    /// セッション復元時に、保存されていた表示設定を反映する。
    pub fn set_show_hidden(&mut self, show_hidden: bool, cx: &mut Context<Self>) {
        if self.show_hidden == show_hidden {
            return;
        }
        self.show_hidden = show_hidden;
        self.rebuild_rows();
        cx.notify();
    }

    /// ヘッダーのボタンから呼ぶ。切り替えの通知はシェル側でセッションに書き戻すために使う。
    fn toggle_show_hidden(&mut self, cx: &mut Context<Self>) {
        self.show_hidden = !self.show_hidden;
        self.rebuild_rows();
        cx.emit(ExplorerEvent::HiddenToggled);
        cx.notify();
    }

    /// 表示行を組み直す。
    fn rebuild_rows(&mut self) {
        let Some(root) = self.root() else {
            self.rows.clear();
            return;
        };
        self.rows = flatten(&root, &self.children, &self.expanded, self.show_hidden);
        if let Some(Editing {
            kind: EditKind::Create { parent, is_dir },
            ..
        }) = self.editing.as_ref()
        {
            insert_draft_row(&mut self.rows, &root, parent, *is_dir);
        }
        self.scroll_to_editing_row();
    }

    /// 編集中の行が画面外なら、そこまでスクロールする。
    ///
    /// `uniform_list` は可視範囲の要素しか作らない。入力欄の行が範囲外だと
    /// 要素が生成されず `window.handle_input` も登録されないため、フォーカスは
    /// 当たっているのに打鍵が入らない状態になる。
    fn scroll_to_editing_row(&mut self) {
        let Some(editing) = self.editing.as_ref() else {
            return;
        };
        let target = match &editing.kind {
            EditKind::Create { .. } => self.rows.iter().position(|row| row.is_draft),
            EditKind::Rename(path) => self
                .rows
                .iter()
                .position(|row| &row.path == path && !row.is_draft),
        };
        if let Some(index) = target {
            self.scroll
                .scroll_to_item(index, gpui::ScrollStrategy::Center);
        }
    }

    /// 取得済みの階層から、指定パス配下をすべて捨てる。
    fn purge_under(&mut self, path: &Path) {
        self.children.retain(|dir, _| !dir.starts_with(path));
        self.expanded.retain(|dir| !dir.starts_with(path));
        if self
            .selected
            .as_deref()
            .is_some_and(|sel| sel.starts_with(path))
        {
            self.selected = None;
        }
    }

    // -- バックエンドとのやり取り --

    /// 1 階層だけ読む。
    fn read_dir(&mut self, path: PathBuf, cx: &mut Context<Self>) {
        let (Some(client), Some(workspace)) = (self.client.clone(), self.workspace.as_ref()) else {
            return;
        };
        if !self.loading.insert(path.clone()) {
            return;
        }
        let workspace = workspace.id;
        let requested = path.clone();
        cx.spawn(async move |this, cx| {
            let result = client
                .request(Request::ReadDir {
                    workspace,
                    path: requested.clone(),
                })
                .await;
            this.update(cx, |this, cx| {
                this.loading.remove(&requested);
                match result {
                    Ok(Response::DirEntries(entries)) => {
                        this.children.insert(requested, sort_entries(entries));
                        this.rebuild_rows();
                        cx.notify();
                    }
                    Ok(_) => {}
                    Err(e) => cx.emit(ExplorerEvent::Notify(
                        NotificationLevel::Error,
                        format!("フォルダを読み込めません: {e}"),
                    )),
                }
            })
            .ok();
        })
        .detach();
    }

    /// 副作用だけの要求を送り、終わったら親フォルダを読み直す。
    fn run_fs_request(
        &mut self,
        request: Request,
        reload: Vec<PathBuf>,
        failure: &'static str,
        cx: &mut Context<Self>,
    ) {
        let Some(client) = self.client.clone() else {
            return;
        };
        cx.spawn(async move |this, cx| {
            let result = client.request(request).await;
            this.update(cx, |this, cx| match result {
                Ok(_) => {
                    // ファイル監視は「あれば使う」機能なので、通知を待たずに自分で読み直す。
                    for dir in reload {
                        this.read_dir(dir, cx);
                    }
                }
                Err(e) => cx.emit(ExplorerEvent::Notify(
                    NotificationLevel::Error,
                    format!("{failure}: {e}"),
                )),
            })
            .ok();
        })
        .detach();
    }

    /// 取得済みの階層をすべて読み直す。
    fn reload_all(&mut self, cx: &mut Context<Self>) {
        let dirs: Vec<PathBuf> = self.children.keys().cloned().collect();
        for dir in dirs {
            self.read_dir(dir, cx);
        }
    }

    /// git 状態を取り直す。`GitView::refresh_status` と同じ理由・同じ形。
    ///
    /// `Event::GitStatusChanged` の受信だけに頼ると、`activate_workspace` が
    /// 既に開いているワークスペースへ切り替えるとき (新しい `OpenWorkspace` 要求を
    /// 送らないため初回イベントが再発火しない) 前のリポジトリの状態が残り続ける。
    fn refresh_git_status(&mut self, cx: &mut Context<Self>) {
        let (Some(client), Some(workspace)) = (
            self.client.clone(),
            self.workspace
                .as_ref()
                .filter(|w| w.git_root.is_some())
                .map(|w| w.id),
        ) else {
            return;
        };
        cx.spawn(async move |this, cx| {
            let result = client.request(Request::GitStatus { workspace }).await;
            this.update(cx, |this, cx| match result {
                Ok(Response::GitStatus(status)) => {
                    this.apply_git_status(status);
                    cx.notify();
                }
                Ok(_) => {}
                Err(e) => cx.emit(ExplorerEvent::Notify(
                    NotificationLevel::Warning,
                    format!("git の状態を取得できません: {e}"),
                )),
            })
            .ok();
        })
        .detach();
    }

    // -- 行の操作 --

    fn activate_row(&mut self, index: usize, cx: &mut Context<Self>) {
        let Some(row) = self.rows.get(index) else {
            return;
        };
        let path = row.path.clone();
        let is_dir = row.is_dir;
        self.selected = Some(path.clone());
        self.menu = None;
        if is_dir {
            self.toggle_dir(path, cx);
        } else {
            cx.emit(ExplorerEvent::OpenFile(path));
        }
        cx.notify();
    }

    fn toggle_dir(&mut self, path: PathBuf, cx: &mut Context<Self>) {
        if !self.expanded.remove(&path) {
            self.expanded.insert(path.clone());
            if !self.children.contains_key(&path) {
                self.read_dir(path, cx);
            }
        }
        self.rebuild_rows();
    }

    /// 新規作成の入力欄を出す。親は選択中の項目から決める。
    fn start_create(&mut self, is_dir: bool, window: &mut Window, cx: &mut Context<Self>) {
        let Some(root) = self.root() else {
            return;
        };
        let parent = self.create_parent(&root);
        // 折り畳んだままだと入力欄が画面に現れないので、先に開いておく。
        if parent != root && !self.expanded.contains(&parent) {
            self.expanded.insert(parent.clone());
            if !self.children.contains_key(&parent) {
                self.read_dir(parent.clone(), cx);
            }
        }
        let placeholder = if is_dir {
            "新しいフォルダ名"
        } else {
            "新しいファイル名"
        };
        self.begin_edit(
            EditKind::Create { parent, is_dir },
            String::new(),
            placeholder,
            window,
            cx,
        );
    }

    /// 新規作成の親フォルダ。選択がフォルダならその中、ファイルならその隣。
    fn create_parent(&self, root: &Path) -> PathBuf {
        let Some(selected) = self.selected.as_ref() else {
            return root.to_path_buf();
        };
        let is_dir = self
            .rows
            .iter()
            .find(|r| &r.path == selected && !r.is_draft)
            .map(|r| r.is_dir)
            .unwrap_or(false);
        if is_dir {
            selected.clone()
        } else {
            selected
                .parent()
                .map(Path::to_path_buf)
                .unwrap_or_else(|| root.to_path_buf())
        }
    }

    fn start_rename(&mut self, path: PathBuf, window: &mut Window, cx: &mut Context<Self>) {
        let initial = path
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_default();
        self.begin_edit(EditKind::Rename(path), initial, "新しい名前", window, cx);
    }

    fn begin_edit(
        &mut self,
        kind: EditKind,
        initial: String,
        placeholder: &'static str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.menu = None;
        let input = cx.new(|cx| {
            TextInput::single_line(placeholder, cx)
                .with_text(initial)
                // 名前の変更は打ち直しが既定なので、全選択した状態から始める。
                .with_all_selected()
                // ファイル名に改行は入れられない。貼り付けで紛れ込んだら取り除く。
                .with_newline_policy(NewlinePolicy::Strip)
        });
        let subscription = cx.subscribe(&input, |this, input, event, cx| match event {
            TextInputEvent::Submit => {
                let text = input.read(cx).text().to_string();
                this.commit_edit(text, cx);
            }
            TextInputEvent::Cancel => this.cancel_edit(cx),
            _ => {}
        });
        input.read(cx).focus(window);
        self.editing = Some(Editing {
            kind,
            input,
            _subscription: subscription,
        });
        self.rebuild_rows();
        cx.notify();
    }

    fn cancel_edit(&mut self, cx: &mut Context<Self>) {
        self.editing = None;
        self.rebuild_rows();
        cx.notify();
    }

    fn commit_edit(&mut self, text: String, cx: &mut Context<Self>) {
        let Some(editing) = self.editing.take() else {
            return;
        };
        self.rebuild_rows();
        cx.notify();

        let name = match validate_name(&text) {
            Ok(name) => name.to_string(),
            Err(message) => {
                cx.emit(ExplorerEvent::Notify(NotificationLevel::Warning, message));
                return;
            }
        };

        match editing.kind {
            EditKind::Create { parent, is_dir } => {
                let path = parent.join(&name);
                let request = if is_dir {
                    Request::CreateDir { path }
                } else {
                    Request::CreateFile { path }
                };
                self.run_fs_request(request, vec![parent], "作成に失敗しました", cx);
            }
            EditKind::Rename(from) => {
                if from.file_name().map(|n| n.to_string_lossy().to_string()) == Some(name.clone()) {
                    return;
                }
                let Some(parent) = from.parent().map(Path::to_path_buf) else {
                    return;
                };
                let to = parent.join(&name);
                // 旧パス配下の取得済み階層は無効になる。残すと消えた行を掴み続ける。
                self.purge_under(&from);
                self.run_fs_request(
                    Request::RenamePath { from, to },
                    vec![parent],
                    "名前を変更できません",
                    cx,
                );
            }
        }
    }

    fn duplicate(&mut self, path: PathBuf, cx: &mut Context<Self>) {
        let Some(parent) = path.parent().map(Path::to_path_buf) else {
            return;
        };
        let to = duplicate_target(&path);
        self.run_fs_request(
            Request::CopyPath { from: path, to },
            vec![parent],
            "複製できません",
            cx,
        );
    }

    fn confirm_delete(&mut self, cx: &mut Context<Self>) {
        let Some((path, is_dir)) = self.pending_delete.take() else {
            return;
        };
        let Some(parent) = path.parent().map(Path::to_path_buf) else {
            return;
        };
        self.purge_under(&path);
        self.rebuild_rows();
        self.run_fs_request(
            Request::DeletePath {
                path,
                recursive: is_dir,
            },
            vec![parent],
            "削除できません",
            cx,
        );
        cx.notify();
    }

    fn copy_path_to_clipboard(&mut self, path: &Path, cx: &mut Context<Self>) {
        cx.write_to_clipboard(ClipboardItem::new_string(path.display().to_string()));
        cx.emit(ExplorerEvent::Notify(
            NotificationLevel::Info,
            "パスをコピーしました".into(),
        ));
    }

    // -- 描画 --

    fn render_header(&self, cx: &Context<Self>) -> AnyElement {
        panel_header("エクスプローラー", cx)
            .child(
                h_flex()
                    .gap(px(2.))
                    .child(
                        icon_button("explorer-new-file", Icon::Plus, false, cx)
                            .on_click(cx.listener(|this, _e, window, cx| {
                                this.start_create(false, window, cx)
                            }))
                            .tooltip(simple_tooltip(tooltip_text::EXPLORER_NEW_FILE)),
                    )
                    .child(
                        icon_button("explorer-new-dir", Icon::Folder, false, cx)
                            .on_click(cx.listener(|this, _e, window, cx| {
                                this.start_create(true, window, cx)
                            }))
                            .tooltip(simple_tooltip(tooltip_text::EXPLORER_NEW_FOLDER)),
                    )
                    .child(
                        icon_button("explorer-reload", Icon::Refresh, false, cx)
                            .on_click(cx.listener(|this, _e, _window, cx| {
                                this.reload_all(cx);
                                cx.notify();
                            }))
                            .tooltip(simple_tooltip(tooltip_text::EXPLORER_RELOAD)),
                    )
                    .child(
                        icon_button("explorer-toggle-hidden", Icon::Eye, self.show_hidden, cx)
                            .on_click(cx.listener(|this, _e, _window, cx| {
                                this.toggle_show_hidden(cx);
                            }))
                            .tooltip(simple_tooltip(tooltip_text::EXPLORER_TOGGLE_HIDDEN)),
                    ),
            )
            .into_any_element()
    }

    fn render_row(&self, index: usize, cx: &Context<Self>) -> AnyElement {
        let theme = theme(cx);
        let Some(row) = self.rows.get(index) else {
            return div().h(ROW_HEIGHT).into_any_element();
        };
        let indent = px(8. + row.depth as f32 * INDENT_STEP);

        if row.is_draft {
            return self.render_edit_row(indent, row.is_dir, cx);
        }
        if let Some(Editing {
            kind: EditKind::Rename(target),
            ..
        }) = self.editing.as_ref()
            && target == &row.path
        {
            return self.render_edit_row(indent, row.is_dir, cx);
        }

        let selected = self.selected.as_ref() == Some(&row.path);
        let text_color = if row.is_ignored {
            theme.git_ignored
        } else if selected {
            theme.text
        } else {
            theme.text_muted
        };
        let glyph_color = if row.is_ignored {
            theme.git_ignored
        } else if row.is_dir {
            theme.accent_tertiary
        } else {
            theme.text_faint
        };
        let expanded = self.expanded.contains(&row.path);
        let path = row.path.clone();
        let is_dir = row.is_dir;
        // ファイルなら自身の状態、フォルダなら配下に変更があるかを見る。
        // `git_by_path` にはファイルしか入らないので、フォルダ行では自然に `None` になる。
        let git_code = self
            .git_by_path
            .get(&row.path)
            .map(|entry| code_for(entry, GitSection::Changed));
        let show_rollup = is_dir && self.changed_ancestors.contains(&row.path);

        list_row(("explorer-row", index), selected, cx)
            .relative()
            .pl(indent)
            .h(ROW_HEIGHT)
            .text_color(text_color)
            // 選択行の左端にネオンの縦線を引く。面で塗るより視線の邪魔にならない。
            .when(selected, |el| {
                el.child(
                    div()
                        .absolute()
                        .left_0()
                        .top(px(3.))
                        .bottom(px(3.))
                        .w(px(2.))
                        .bg(theme.accent)
                        .rounded_r(px(2.)),
                )
            })
            .child(chevron(is_dir, expanded, theme.text_faint))
            .child(icon(
                if is_dir { Icon::Folder } else { Icon::File },
                px(13.),
                glyph_color,
            ))
            .child(
                div()
                    .overflow_hidden()
                    .child(SharedString::from(row.name.clone())),
            )
            .child(div().flex_1())
            .when_some(git_code, |el, code| {
                el.child(
                    div()
                        .flex_none()
                        .text_size(px(10.5))
                        .font_weight(gpui::FontWeight::BOLD)
                        .text_color(status_color(code, &theme))
                        .child(status_char(code).to_string()),
                )
            })
            .when(show_rollup, |el| {
                el.child(
                    div()
                        .flex_none()
                        .size(px(6.))
                        .rounded_full()
                        .bg(theme.git_modified),
                )
            })
            .on_click(cx.listener(move |this, _e, _window, cx| this.activate_row(index, cx)))
            .on_mouse_down(
                MouseButton::Right,
                cx.listener(move |this, event: &MouseDownEvent, _window, cx| {
                    this.selected = Some(path.clone());
                    this.menu = Some(ContextMenu {
                        position: event.position,
                        path: path.clone(),
                        is_dir,
                    });
                    cx.notify();
                }),
            )
            .into_any_element()
    }

    /// 名前の変更・新規作成でその場に出す入力欄の行。
    fn render_edit_row(&self, indent: Pixels, is_dir: bool, cx: &Context<Self>) -> AnyElement {
        let theme = theme(cx);
        let Some(editing) = self.editing.as_ref() else {
            return div().h(ROW_HEIGHT).into_any_element();
        };
        h_flex()
            .w_full()
            .h(ROW_HEIGHT)
            .pl(indent)
            .pr(px(8.))
            .gap(px(6.))
            .bg(theme.bg_overlay)
            .child(div().w(px(12.)).flex_none())
            .child(icon(
                if is_dir { Icon::Folder } else { Icon::File },
                px(13.),
                theme.accent,
            ))
            .child(
                div()
                    .flex_1()
                    .h(px(20.))
                    .px(px(4.))
                    .overflow_hidden()
                    .rounded(px(4.))
                    .bg(theme.bg_surface)
                    .border_1()
                    .border_color(theme.border_glow)
                    .text_size(px(12.))
                    .line_height(px(18.))
                    .text_color(theme.text)
                    .cursor(gpui::CursorStyle::IBeam)
                    // 枠の余白を押しても欄へ入れるようにする。
                    .on_mouse_down(MouseButton::Left, {
                        let input = editing.input.clone();
                        move |_, window, cx| input.read(cx).focus(window)
                    })
                    .child(editing.input.clone()),
            )
            .into_any_element()
    }

    fn render_tree(&self, cx: &Context<Self>) -> AnyElement {
        if self.workspace.is_none() {
            return empty_state("フォルダが開かれていません", cx).into_any_element();
        }
        if self.rows.is_empty() {
            // ルートの取得が済むまでは「空」と断定できない。
            let loaded = self
                .root()
                .is_some_and(|root| self.children.contains_key(&root));
            let message = if loaded {
                "空のフォルダです"
            } else {
                "読み込み中…"
            };
            return empty_state(message, cx).into_any_element();
        }
        uniform_list(
            "explorer-tree",
            self.rows.len(),
            cx.processor(|this, range: Range<usize>, _window, cx| {
                range.map(|i| this.render_row(i, cx)).collect::<Vec<_>>()
            }),
        )
        .track_scroll(self.scroll.clone())
        .flex_1()
        .into_any_element()
    }

    fn render_context_menu(&self, cx: &Context<Self>) -> Option<AnyElement> {
        let menu = self.menu.as_ref()?;
        let theme = theme(cx);
        let path = menu.path.clone();
        let is_dir = menu.is_dir;

        let mut items: Vec<AnyElement> = Vec::new();
        // 新規作成の行き先は `create_parent` が選択項目から決める。
        // フォルダ上ならその中、ファイル上なら同じ階層になる。
        let target = path.clone();
        items.push(self.menu_item(
            "menu-new-file",
            Icon::Plus,
            "新規ファイル",
            cx.listener(move |this, _e, window, cx| {
                this.selected = Some(target.clone());
                this.start_create(false, window, cx);
            }),
            cx,
        ));
        let target = path.clone();
        items.push(self.menu_item(
            "menu-new-dir",
            Icon::Folder,
            "新規フォルダ",
            cx.listener(move |this, _e, window, cx| {
                this.selected = Some(target.clone());
                this.start_create(true, window, cx);
            }),
            cx,
        ));
        let target = path.clone();
        items.push(self.menu_item(
            "menu-rename",
            Icon::Edit,
            "名前の変更",
            cx.listener(move |this, _e, window, cx| {
                this.menu = None;
                this.start_rename(target.clone(), window, cx);
            }),
            cx,
        ));
        let target = path.clone();
        items.push(self.menu_item(
            "menu-duplicate",
            Icon::Copy,
            "複製",
            cx.listener(move |this, _e, _window, cx| {
                this.menu = None;
                this.duplicate(target.clone(), cx);
                cx.notify();
            }),
            cx,
        ));
        let target = path.clone();
        items.push(self.menu_item(
            "menu-delete",
            Icon::Trash,
            "削除",
            cx.listener(move |this, _e, _window, cx| {
                this.menu = None;
                this.pending_delete = Some((target.clone(), is_dir));
                cx.notify();
            }),
            cx,
        ));
        let target = path.clone();
        items.push(self.menu_item(
            "menu-copy-path",
            Icon::Files,
            "パスをコピー",
            cx.listener(move |this, _e, _window, cx| {
                this.menu = None;
                this.copy_path_to_clipboard(&target, cx);
                cx.notify();
            }),
            cx,
        ));

        // gpui にメニュー部品は無いので、絶対配置した箱を自前で組む。
        // 位置はマウスのウィンドウ座標なので、ウィンドウ基準で置ける anchored に載せる。
        Some(
            deferred(
                anchored().position(menu.position).snap_to_window().child(
                    v_flex()
                        .absolute()
                        .min_w(px(176.))
                        .py(px(4.))
                        .rounded(px(8.))
                        .bg(theme.bg_overlay)
                        .border_1()
                        .border_color(theme.border_glow)
                        .shadow_lg()
                        .occlude()
                        .on_mouse_down_out(cx.listener(|this, _e: &MouseDownEvent, _window, cx| {
                            this.menu = None;
                            cx.notify();
                        }))
                        .children(items),
                ),
            )
            .into_any_element(),
        )
    }

    fn menu_item(
        &self,
        id: impl Into<ElementId>,
        glyph: Icon,
        label: &'static str,
        on_click: impl Fn(&gpui::ClickEvent, &mut Window, &mut App) + 'static,
        cx: &Context<Self>,
    ) -> AnyElement {
        let theme = theme(cx);
        h_flex()
            .id(id)
            .h(px(26.))
            .px(px(10.))
            .gap(px(8.))
            .text_size(px(12.))
            .text_color(theme.text_muted)
            .cursor_pointer()
            .hover(|s| s.bg(theme.accent_soft).text_color(theme.text))
            .child(icon(glyph, px(13.), theme.text_faint))
            .child(label)
            .on_click(on_click)
            .into_any_element()
    }

    fn render_delete_dialog(&self, cx: &Context<Self>) -> Option<AnyElement> {
        let (path, is_dir) = self.pending_delete.as_ref()?;
        let theme = theme(cx);
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| path.display().to_string());
        let detail = if *is_dir {
            format!("フォルダ「{name}」とその中身をすべて削除します。元に戻せません。")
        } else {
            format!("ファイル「{name}」を削除します。元に戻せません。")
        };
        Some(
            div()
                .absolute()
                .inset_0()
                .flex()
                .items_center()
                .justify_center()
                .p(px(16.))
                // 背後の木を暗く沈めて、確認だけが浮いて見えるようにする。
                .bg(theme.bg_void.opacity(0.72))
                // 確認中に背後の行を触らせない。
                .occlude()
                .child(
                    v_flex()
                        .id("explorer-delete-dialog")
                        .w_full()
                        .p(px(14.))
                        .gap(px(10.))
                        .rounded(px(10.))
                        .bg(theme.bg_overlay)
                        .border_1()
                        .border_color(theme.error)
                        .shadow_lg()
                        .occlude()
                        .child(
                            div()
                                .text_size(px(12.5))
                                .text_color(theme.text)
                                .child("削除の確認"),
                        )
                        .child(
                            div()
                                .text_size(px(11.5))
                                .text_color(theme.text_muted)
                                .child(detail),
                        )
                        .child(
                            h_flex()
                                .justify_end()
                                .gap(px(8.))
                                .child(
                                    ghost_button("explorer-delete-cancel", "取り消し", cx)
                                        .on_click(cx.listener(|this, _e, _window, cx| {
                                            this.pending_delete = None;
                                            cx.notify();
                                        })),
                                )
                                .child(
                                    primary_button("explorer-delete-ok", "削除", true, cx)
                                        .bg(theme.error)
                                        .on_click(cx.listener(|this, _e, _window, cx| {
                                            this.confirm_delete(cx)
                                        })),
                                ),
                        ),
                )
                .into_any_element(),
        )
    }
}

/// フォルダの開閉印。ファイル行では幅だけ揃える。
fn chevron(is_dir: bool, expanded: bool, color: Hsla) -> AnyElement {
    if !is_dir {
        return div().w(px(12.)).flex_none().into_any_element();
    }
    div()
        .w(px(12.))
        .flex_none()
        .child(icon(
            if expanded {
                Icon::ChevronDown
            } else {
                Icon::ChevronRight
            },
            px(12.),
            color,
        ))
        .into_any_element()
}

impl Focusable for ExplorerView {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for ExplorerView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = theme(cx);
        v_flex()
            .size_full()
            .relative()
            .overflow_hidden()
            .track_focus(&self.focus_handle)
            .bg(theme.bg_elevated)
            .child(self.render_header(cx))
            // 見出しの下に一本だけ通すネオン。パネルの境目を光らせて奥行きを出す。
            .child(nebula_accent_line(theme.accent_soft))
            .child(self.render_tree(cx))
            .children(self.render_context_menu(cx))
            .children(self.render_delete_dialog(cx))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(path: &str, is_dir: bool) -> DirEntry {
        let path = PathBuf::from(path);
        DirEntry {
            name: path
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_default(),
            path,
            is_dir,
            is_symlink: false,
            size: 0,
            is_ignored: false,
        }
    }

    /// ルート直下に src/ (a.rs, b.rs) と README.md がある木。
    fn sample() -> (PathBuf, HashMap<PathBuf, Vec<DirEntry>>) {
        let root = PathBuf::from("/w");
        let mut children = HashMap::new();
        children.insert(
            root.clone(),
            sort_entries(vec![
                entry("/w/README.md", false),
                entry("/w/src", true),
                entry("/w/docs", true),
            ]),
        );
        children.insert(
            PathBuf::from("/w/src"),
            sort_entries(vec![
                entry("/w/src/b.rs", false),
                entry("/w/src/a.rs", false),
            ]),
        );
        (root, children)
    }

    /// ルート直下に隠しフォルダ (`.git`) と隠しファイル (`.gitignore`) を混ぜた木。
    /// `.git` の中身も持たせ、非表示時に再帰まで止まっていることを確かめられるようにする。
    fn sample_with_hidden() -> (PathBuf, HashMap<PathBuf, Vec<DirEntry>>) {
        let root = PathBuf::from("/w");
        let mut children = HashMap::new();
        children.insert(
            root.clone(),
            sort_entries(vec![
                entry("/w/README.md", false),
                entry("/w/.git", true),
                entry("/w/.gitignore", false),
            ]),
        );
        children.insert(
            PathBuf::from("/w/.git"),
            sort_entries(vec![entry("/w/.git/HEAD", false)]),
        );
        (root, children)
    }

    #[test]
    fn 隠しファイルを表示しない設定ではドット始まりの項目が除かれる() {
        let (root, children) = sample_with_hidden();
        let rows = flatten(&root, &children, &HashSet::new(), false);
        let listed: Vec<&str> = rows.iter().map(|r| r.name.as_str()).collect();
        assert_eq!(
            listed,
            vec!["README.md"],
            ".git と .gitignore はどちらもドット始まりなので隠す"
        );
    }

    #[test]
    fn 隠しファイルを表示する設定では通常どおり全項目が出る() {
        let (root, children) = sample_with_hidden();
        let rows = flatten(&root, &children, &HashSet::new(), true);
        let listed: Vec<&str> = rows.iter().map(|r| r.name.as_str()).collect();
        assert_eq!(listed, vec![".git", ".gitignore", "README.md"]);
    }

    #[test]
    fn 隠しフォルダを非表示にすると展開していても中身ごと消える() {
        let (root, children) = sample_with_hidden();
        // .git を展開状態にしても、フォルダ自体が非表示ならその配下を辿る理由が無い。
        let expanded: HashSet<PathBuf> = [PathBuf::from("/w/.git")].into_iter().collect();
        let rows = flatten(&root, &children, &expanded, false);
        assert_eq!(rows.len(), 1, "README.md 以外は行ごと現れない");
        assert!(rows.iter().all(|r| r.name != "HEAD"));
    }

    #[test]
    fn 並び順はフォルダが先で名前順() {
        let sorted = sort_entries(vec![
            entry("/w/Zebra.txt", false),
            entry("/w/apple", true),
            entry("/w/alpha.txt", false),
            entry("/w/Beta", true),
        ]);
        let names: Vec<&str> = sorted.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, vec!["apple", "Beta", "alpha.txt", "Zebra.txt"]);
    }

    #[test]
    fn 折り畳んだ状態ではルート直下だけが並ぶ() {
        let (root, children) = sample();
        let rows = flatten(&root, &children, &HashSet::new(), true);
        let listed: Vec<(&str, usize)> = rows.iter().map(|r| (r.name.as_str(), r.depth)).collect();
        assert_eq!(
            listed,
            vec![("docs", 0), ("src", 0), ("README.md", 0)],
            "取得済みでも展開していない階層は出さない"
        );
    }

    #[test]
    fn 展開したフォルダの子が直後に一段深く入る() {
        let (root, children) = sample();
        let expanded: HashSet<PathBuf> = [PathBuf::from("/w/src")].into_iter().collect();
        let rows = flatten(&root, &children, &expanded, true);
        let listed: Vec<(&str, usize)> = rows.iter().map(|r| (r.name.as_str(), r.depth)).collect();
        assert_eq!(
            listed,
            vec![
                ("docs", 0),
                ("src", 0),
                ("a.rs", 1),
                ("b.rs", 1),
                ("README.md", 0),
            ]
        );
    }

    #[test]
    fn 未取得のフォルダは展開しても子が増えない() {
        let (root, children) = sample();
        // docs は children に無い。
        let expanded: HashSet<PathBuf> = [PathBuf::from("/w/docs")].into_iter().collect();
        let rows = flatten(&root, &children, &expanded, true);
        assert_eq!(rows.len(), 3);
    }

    #[test]
    fn 仮行はフォルダの直後に一段深く入る() {
        let (root, children) = sample();
        let expanded: HashSet<PathBuf> = [PathBuf::from("/w/src")].into_iter().collect();
        let mut rows = flatten(&root, &children, &expanded, true);
        insert_draft_row(&mut rows, &root, Path::new("/w/src"), false);
        assert_eq!(rows[2].depth, 1);
        assert!(rows[2].is_draft);
        assert_eq!(rows[3].name, "a.rs", "既存の子は仮行の後ろへ下がる");
    }

    #[test]
    fn ルート直下の仮行は先頭に入る() {
        let (root, children) = sample();
        let mut rows = flatten(&root, &children, &HashSet::new(), true);
        insert_draft_row(&mut rows, &root, &root, true);
        assert!(rows[0].is_draft);
        assert_eq!(rows[0].depth, 0);
        assert!(rows[0].is_dir);
    }

    #[test]
    fn 変更通知から読み直すフォルダを重複なく求める() {
        let changes = vec![
            FileChange {
                kind: FileChangeKind::Created,
                path: PathBuf::from("/w/src/new.rs"),
                to: None,
            },
            FileChange {
                kind: FileChangeKind::Modified,
                path: PathBuf::from("/w/src/a.rs"),
                to: None,
            },
            FileChange {
                kind: FileChangeKind::Renamed,
                path: PathBuf::from("/w/src/b.rs"),
                to: Some(PathBuf::from("/w/docs/b.rs")),
            },
        ];
        assert_eq!(
            affected_dirs(&changes),
            vec![PathBuf::from("/w/src"), PathBuf::from("/w/docs")]
        );
    }

    #[test]
    fn ネストの深いパスは途中のフォルダすべてがロールアップ対象になる() {
        let root = PathBuf::from("/w");
        let paths = vec![PathBuf::from("/w/src/deep/nested/file.rs")];
        let dirs = changed_ancestor_dirs(&root, &paths);
        assert_eq!(
            dirs,
            [
                PathBuf::from("/w/src"),
                PathBuf::from("/w/src/deep"),
                PathBuf::from("/w/src/deep/nested"),
            ]
            .into_iter()
            .collect(),
            "ワークスペース直下 (/w) は行として存在しないので含めない"
        );
    }

    #[test]
    fn リポジトリ直下のファイルはロールアップ対象を持たない() {
        let root = PathBuf::from("/w");
        let paths = vec![PathBuf::from("/w/README.md")];
        assert!(
            changed_ancestor_dirs(&root, &paths).is_empty(),
            "唯一の親はルート自身で、ルートは行として描かないので対象は空になる"
        );
    }

    #[test]
    fn 同じフォルダの複数変更は一つのエントリにまとまる() {
        let root = PathBuf::from("/w");
        let paths = vec![PathBuf::from("/w/src/a.rs"), PathBuf::from("/w/src/b.rs")];
        let dirs = changed_ancestor_dirs(&root, &paths);
        assert_eq!(dirs, [PathBuf::from("/w/src")].into_iter().collect());
    }

    #[test]
    fn 変更が無ければロールアップ対象も無い() {
        assert!(changed_ancestor_dirs(&PathBuf::from("/w"), &[]).is_empty());
    }

    #[test]
    fn ワークスペース外の変更は対象に含めない() {
        // git_root がワークスペースの祖先にあると、他のフォルダの変更も
        // entries に混ざり得る。ツリーに出ないパスまで拾わないことを確かめる。
        let root = PathBuf::from("/w/sub");
        let paths = vec![PathBuf::from("/w/other/file.rs")];
        assert!(changed_ancestor_dirs(&root, &paths).is_empty());
    }

    #[test]
    fn 名前の検証() {
        assert_eq!(validate_name("  main.rs "), Ok("main.rs"));
        assert!(validate_name("   ").is_err());
        assert!(validate_name("a/b").is_err());
        assert!(validate_name("..").is_err());
    }

    #[test]
    fn 複製先は拡張子の前に付ける() {
        assert_eq!(
            duplicate_target(Path::new("/w/src/main.rs")),
            PathBuf::from("/w/src/main copy.rs")
        );
        assert_eq!(
            duplicate_target(Path::new("/w/src")),
            PathBuf::from("/w/src copy")
        );
        assert_eq!(
            duplicate_target(Path::new("/w/.gitignore")),
            PathBuf::from("/w/.gitignore copy"),
            "先頭のドットは拡張子ではない"
        );
        assert_eq!(
            duplicate_target(Path::new("/w/a.tar.gz")),
            PathBuf::from("/w/a.tar copy.gz")
        );
    }
}
