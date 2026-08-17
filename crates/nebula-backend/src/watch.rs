//! ファイル監視。
//!
//! `notify` の生イベントをそのまま GUI へ流すと、ビルド中は 1 秒に数千件が届いて
//! 描画が止まる。ここでは 3 段の絞り込みを通してから `Event::FilesChanged` を送る。
//!
//! 1. **無視パス**を捨てる (`.git/`・`target/`・`node_modules/`・`.DS_Store` と `.gitignore`)
//! 2. **リネームの対応付け**をする (From と To が別イベントで来る環境があるため)
//! 3. **デバウンス**して同一パスの重複を畳み込む
//!
//! 判定ロジックは実ファイルシステムに触れない純粋な部品 ([`classify`]・[`merge_kind`]・
//! [`IgnoreFilter`]・[`Debouncer`]) に切り出してあり、単体テストはすべてそこに当たる。

use ignore::gitignore::{Gitignore, GitignoreBuilder};
use nebula_protocol::{Event, FileChange, FileChangeKind, ProtocolError, WorkspaceId};
use notify::event::{EventKind, ModifyKind, RenameMode};
use notify::{RecommendedWatcher, RecursiveMode, Watcher};
use std::collections::{HashMap, VecDeque};
use std::path::{Component, Path, PathBuf};
use std::sync::Mutex;
use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::time::{Duration, Instant};
use tokio::sync::broadcast;

/// まとめて送るまでの待ち時間。長くすると保護は強まるが体感の追従が鈍る。
/// 150ms はエディタの外でファイルが変わったことに気づく速さとして十分で、
/// `cargo build` の書き込み嵐を 1 通に畳める下限でもある。
const DEBOUNCE_WINDOW: Duration = Duration::from_millis(150);

/// 対になる To を待つ時間。これを過ぎた From は単独の変更として解決する。
const RENAME_PAIR_WINDOW: Duration = Duration::from_millis(100);

/// 転送スレッドが期限切れを点検する間隔。イベントが来ない間も
/// ウィンドウの締めとリネームの時間切れを進める必要がある。
const TICK: Duration = Duration::from_millis(25);

/// ワークスペースを問わず常に捨てるフォルダ名。
///
/// いずれも生成物か VCS の内部で、エディタに見せる意味がないうえ
/// 変更量が他の桁違いに多い。`.gitignore` に書かれていない場合にも効かせたいので
/// ハードコードしている。
const ALWAYS_IGNORED: [&str; 4] = [".git", "target", "node_modules", ".DS_Store"];

// ---------------------------------------------------------------------------
// サービス
// ---------------------------------------------------------------------------

pub struct WatchService {
    events: broadcast::Sender<Event>,
    /// ワークスペースごとの監視。`&self` で操作されるため内部可変性を持つ。
    ///
    /// 値は `RecommendedWatcher` そのもの。drop すると notify 側のスレッドが止まり、
    /// 連動して転送スレッドの受信口も切れるので、停止処理はこの表からの削除だけで済む。
    watches: Mutex<HashMap<WorkspaceId, RecommendedWatcher>>,
}

impl WatchService {
    pub fn new(events: broadcast::Sender<Event>) -> Self {
        Self {
            events,
            watches: Mutex::new(HashMap::new()),
        }
    }

    /// 再帰的な監視を始める。既に監視中のワークスペースなら何もしない。
    pub fn watch(&self, workspace: WorkspaceId, path: &Path) -> Result<(), ProtocolError> {
        let mut watches = self.watches.lock().expect("監視表のロック");
        if watches.contains_key(&workspace) {
            return Ok(());
        }

        // FSEvents は `/private/var/...` のような実体パスでイベントを返す。
        // 無視判定をルートからの相対で行うため、ルート側も実体へ揃えておく。
        let root = path
            .canonicalize()
            .map_err(|e| ProtocolError::io(format!("{} を監視できません: {e}", path.display())))?;

        let (tx, rx) = std::sync::mpsc::channel();
        // notify のコールバックは notify 自身のスレッドから呼ばれる。ここでは
        // 送るだけにして、束ねる処理は専用スレッドへ渡す。
        let mut watcher = notify::recommended_watcher(move |result| {
            let _ = tx.send(result);
        })
        .map_err(|e| ProtocolError::io(format!("ファイル監視を開始できません: {e}")))?;

        watcher
            .watch(&root, RecursiveMode::Recursive)
            .map_err(|e| ProtocolError::io(format!("{} を監視できません: {e}", root.display())))?;

        let debouncer = Debouncer::new(IgnoreFilter::new(&root), path_exists);
        let events = self.events.clone();
        std::thread::Builder::new()
            .name(format!("nebula-watch-{workspace}"))
            .spawn(move || forward_loop(rx, debouncer, workspace, events))
            .map_err(|e| ProtocolError::io(format!("監視スレッドを起動できません: {e}")))?;

        watches.insert(workspace, watcher);
        Ok(())
    }

    pub fn unwatch(&self, workspace: WorkspaceId) {
        self.watches
            .lock()
            .expect("監視表のロック")
            .remove(&workspace);
    }

    pub fn shutdown(&self) {
        self.watches.lock().expect("監視表のロック").clear();
    }
}

/// notify からの生イベントを束ねて `Event::FilesChanged` にして流す。
///
/// `recv_timeout` で回すのは、イベントが途切れた後もウィンドウの締めと
/// リネームの時間切れを進める必要があるため。
fn forward_loop(
    rx: Receiver<notify::Result<notify::Event>>,
    mut debouncer: Debouncer,
    workspace: WorkspaceId,
    events: broadcast::Sender<Event>,
) {
    loop {
        match rx.recv_timeout(TICK) {
            Ok(Ok(event)) => debouncer.accept(&event, Instant::now()),
            // 監視エラーは一過性 (一時ファイルの消失など) のことが多く、監視自体は続けられる。
            // ただし監視数の上限超過のように監視が実質死ぬものも同じ口から来るため、
            // 黙って捨てずに必ず残す。
            Ok(Err(e)) => eprintln!("nebula-backend: ファイル監視でエラー: {e}"),
            Err(RecvTimeoutError::Timeout) => {}
            // watcher が drop された = unwatch / shutdown。溜めていた分を出して終わる。
            Err(RecvTimeoutError::Disconnected) => {
                send_changes(&events, workspace, debouncer.flush());
                return;
            }
        }
        let now = Instant::now();
        if debouncer.ready(now) {
            send_changes(&events, workspace, debouncer.take(now));
        }
    }
}

fn send_changes(
    events: &broadcast::Sender<Event>,
    workspace: WorkspaceId,
    changes: Vec<FileChange>,
) {
    if changes.is_empty() {
        return;
    }
    let _ = events.send(Event::FilesChanged { workspace, changes });
}

fn path_exists(path: &Path) -> bool {
    path.exists()
}

// ---------------------------------------------------------------------------
// 無視フィルタ
// ---------------------------------------------------------------------------

/// 監視開始時に 1 回だけ構築する無視判定。
///
/// ビルド中に `.gitignore` を読み直すと、まさに一番イベントが多い場面で
/// ディスク I/O を繰り返すことになるため内容は固定する。
struct IgnoreFilter {
    root: PathBuf,
    gitignore: Gitignore,
}

impl IgnoreFilter {
    fn new(root: &Path) -> Self {
        let mut builder = GitignoreBuilder::new(root);
        let _ = builder.add(root.join(".gitignore"));
        let _ = builder.add(root.join(".ignore"));
        Self {
            root: root.to_path_buf(),
            gitignore: builder.build().unwrap_or_else(|_| Gitignore::empty()),
        }
    }

    fn is_ignored(&self, path: &Path) -> bool {
        // 常時無視の判定はルートより下だけを見る。絶対パス全体を見ると、
        // ワークスペース自体が `.../node_modules/pkg` のような場所にあるときに
        // 配下のすべてのイベントが黙って消える。
        //
        // 剥がせないパス (= 監視対象外) はそもそも判定しない。
        // `matched_path_or_any_parents` はルート配下でないパスを渡すと panic するため、
        // ここで弾くことが gitignore 判定の前提条件にもなっている。
        let Ok(relative) = path.strip_prefix(&self.root) else {
            return false;
        };
        if has_ignored_component(relative) {
            return true;
        }
        // 削除済みのパスは種別を確かめられないので常にファイル扱いにする。
        // 親フォルダの規則は `matched_path_or_any_parents` が遡って見てくれる。
        self.gitignore
            .matched_path_or_any_parents(path, false)
            .is_ignore()
    }
}

fn has_ignored_component(path: &Path) -> bool {
    path.components().any(|component| {
        let Component::Normal(name) = component else {
            return false;
        };
        name.to_str()
            .is_some_and(|name| ALWAYS_IGNORED.contains(&name))
    })
}

// ---------------------------------------------------------------------------
// イベント種別の写像
// ---------------------------------------------------------------------------

/// notify のイベント種別を、この層で扱う粒度へ落としたもの。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RawKind {
    Created,
    Modified,
    Removed,
    /// リネーム元。対になる To を待つ。
    RenameFrom,
    /// リネーム先。待っている From があれば組にする。
    RenameTo,
    /// 方向不明のリネーム。macOS の FSEvents はこれしか返さない。
    RenameAny,
    /// From と To が 1 つのイベントに入っている (paths[0] が From、paths[1] が To)。
    RenameBoth,
}

/// `None` は「エディタに知らせる価値がないので捨てる」を意味する。
fn classify(kind: &EventKind) -> Option<RawKind> {
    match kind {
        EventKind::Create(_) => Some(RawKind::Created),
        EventKind::Remove(_) => Some(RawKind::Removed),
        EventKind::Modify(ModifyKind::Name(mode)) => Some(match mode {
            RenameMode::From => RawKind::RenameFrom,
            RenameMode::To => RawKind::RenameTo,
            RenameMode::Both => RawKind::RenameBoth,
            RenameMode::Any | RenameMode::Other => RawKind::RenameAny,
        }),
        // 権限や mtime だけの変化とアクセスは、ビルド中に大量に出るわりに
        // エディタ側で表示や再読込を変える理由にならない。
        EventKind::Modify(ModifyKind::Metadata(_)) | EventKind::Access(_) => None,
        EventKind::Modify(_) => Some(RawKind::Modified),
        // 「精度の低いモード」の backend は Any しか返さない。ここを捨てると
        // その環境で監視が丸ごと効かなくなるので変更として扱う。
        EventKind::Any | EventKind::Other => Some(RawKind::Modified),
    }
}

// ---------------------------------------------------------------------------
// デバウンス
// ---------------------------------------------------------------------------

/// 同一パスに連続して起きた変更の畳み込み。`None` は相殺されて送る必要がないことを表す。
fn merge_kind(previous: FileChangeKind, next: FileChangeKind) -> Option<FileChangeKind> {
    use FileChangeKind::*;
    match (previous, next) {
        // 作成直後の書き込みは「作成」1 件で足りる。
        (Created, Modified) => Some(Created),
        // ウィンドウ内で作られて消えたものは、外から見れば何も起きていない。
        (Created, Removed) => None,
        // 削除してから作り直すのはアトミック保存の典型。外から見れば内容が変わっただけ。
        (Removed, Created) => Some(Modified),
        (_, next) => Some(next),
    }
}

/// 挿入順を保ったまま、同一パスの重複を潰せる集積器。
///
/// 単なる `HashMap` では GUI に届く順序が実行ごとに変わり、`Vec` だけでは
/// 重複除去が O(n^2) になる。両方を持つのはそのため。
#[derive(Default)]
struct PendingChanges {
    /// 挿入順の一覧。相殺された要素は `None` になり、順序の穴として残る。
    slots: Vec<Option<FileChange>>,
    /// パス → `slots` の添字。
    index: HashMap<PathBuf, usize>,
}

impl PendingChanges {
    fn push_simple(&mut self, kind: FileChangeKind, path: &Path) {
        if let Some(&slot) = self.index.get(path) {
            let merged = match self.slots[slot].as_ref() {
                Some(existing) => merge_kind(existing.kind, kind),
                // 相殺済みのスロットは、新しい変更でそのまま作り直す。
                None => Some(kind),
            };
            self.slots[slot] = merged.map(|kind| FileChange {
                kind,
                path: path.to_path_buf(),
                to: None,
            });
            return;
        }
        self.index.insert(path.to_path_buf(), self.slots.len());
        self.slots.push(Some(FileChange {
            kind,
            path: path.to_path_buf(),
            to: None,
        }));
    }

    /// リネームは 2 つのパスにまたがるため畳み込みの対象にしない。
    /// 以降に同じパスへ来た変更が誤ってリネームへ畳み込まれないよう索引から外す。
    fn push_rename(&mut self, from: &Path, to: &Path) {
        self.index.remove(from);
        self.index.remove(to);
        self.slots.push(Some(FileChange {
            kind: FileChangeKind::Renamed,
            path: from.to_path_buf(),
            to: Some(to.to_path_buf()),
        }));
    }

    fn is_empty(&self) -> bool {
        self.slots.iter().all(Option::is_none)
    }

    fn take(&mut self) -> Vec<FileChange> {
        self.index.clear();
        std::mem::take(&mut self.slots)
            .into_iter()
            .flatten()
            .collect()
    }
}

/// 対になる To を待っているリネーム元。
struct PendingRename {
    path: PathBuf,
    at: Instant,
    /// 方向不明 (`RenameAny`) 由来か。期限切れ時の解決方法が変わる。
    direction_unknown: bool,
}

/// 生イベントを受けて、送るべき [`FileChange`] の列に畳み込む本体。
///
/// 時刻は呼び出し側から渡す。転送スレッドは `Instant::now()` を渡し、
/// テストは固定の時刻を渡すことで、実時間を待たずに境界を検証できる。
struct Debouncer {
    filter: IgnoreFilter,
    pending: PendingChanges,
    /// ビルド中は 1 つのウィンドウに複数のリネームが入るため FIFO で持つ。
    /// inotify は From と To を隣接して出すので、先頭と組にするのが正しい。
    rename_froms: VecDeque<PendingRename>,
    /// ウィンドウの起点。最初の未送信変更を受け取った時刻。
    window_started: Option<Instant>,
    /// 方向不明のリネームを Created と Removed のどちらへ倒すかの判定。
    /// テストを実ファイルシステムから切り離すため差し替え可能にしている。
    exists: fn(&Path) -> bool,
}

impl Debouncer {
    fn new(filter: IgnoreFilter, exists: fn(&Path) -> bool) -> Self {
        Self {
            filter,
            pending: PendingChanges::default(),
            rename_froms: VecDeque::new(),
            window_started: None,
            exists,
        }
    }

    fn accept(&mut self, event: &notify::Event, now: Instant) {
        let Some(raw) = classify(&event.kind) else {
            return;
        };

        match raw {
            RawKind::RenameBoth => self.accept_rename_both(&event.paths),
            RawKind::RenameFrom | RawKind::RenameTo | RawKind::RenameAny => {
                for path in &event.paths {
                    // 無視パスは組にする前に落とす。`.tmp` へ書いて本体へ rename する
                    // アトミック保存で、見える側だけを正しく拾うため。
                    if self.filter.is_ignored(path) {
                        continue;
                    }
                    self.accept_rename_single(raw, path, now);
                }
            }
            RawKind::Created | RawKind::Modified | RawKind::Removed => {
                let kind = simple_kind(raw);
                for path in &event.paths {
                    if self.filter.is_ignored(path) {
                        continue;
                    }
                    self.pending.push_simple(kind, path);
                }
            }
        }

        self.window_started = if self.pending.is_empty() {
            None
        } else {
            Some(self.window_started.unwrap_or(now))
        };
    }

    fn accept_rename_both(&mut self, paths: &[PathBuf]) {
        let (Some(from), Some(to)) = (paths.first(), paths.get(1)) else {
            return;
        };
        // 片側だけが無視対象なのは、一時ファイルを本体へ置き換える保存の形。
        // 見える側から見た出来事へ落とす。
        match (!self.filter.is_ignored(from), !self.filter.is_ignored(to)) {
            (true, true) => self.pending.push_rename(from, to),
            (true, false) => self.pending.push_simple(FileChangeKind::Removed, from),
            (false, true) => self.pending.push_simple(FileChangeKind::Created, to),
            (false, false) => {}
        }
    }

    fn accept_rename_single(&mut self, raw: RawKind, path: &Path, now: Instant) {
        match raw {
            RawKind::RenameFrom => self.rename_froms.push_back(PendingRename {
                path: path.to_path_buf(),
                at: now,
                direction_unknown: false,
            }),
            RawKind::RenameTo => match self.rename_froms.pop_front() {
                Some(from) => self.pending.push_rename(&from.path, path),
                // 対になる From が無いのは、監視範囲の外から移動してきた場合。
                None => self.pending.push_simple(FileChangeKind::Created, path),
            },
            // 方向が分からない場合は、待っている相手が「消えていて」こちらが「実在する」
            // ときだけ from→to の対とみなす。
            //
            // 単純に先頭と組にすると、無関係な 2 つの保存を取り違える。無視対象の
            // 一時ファイル側はペア付けの前に落ちるので、`a.tmp→a` と `b.tmp→b` が
            // 同じウィンドウに入ると `a` と `b` だけが残り、これらが対にされてしまう。
            // 実体の有無を見れば両方とも実在するので対にならず、それぞれ作成へ落ちる。
            RawKind::RenameAny => {
                if self.pairs_with_waiting(path) {
                    let from = self.rename_froms.pop_front().expect("直前に存在を確認済み");
                    self.pending.push_rename(&from.path, path);
                } else {
                    self.rename_froms.push_back(PendingRename {
                        path: path.to_path_buf(),
                        at: now,
                        direction_unknown: true,
                    });
                }
            }
            _ => {}
        }
    }

    /// 方向不明のリネームを、待っている先頭の相手と組にしてよいか。
    ///
    /// リネームが成立していれば元は消えて先は残る。どちらも実在するなら
    /// 別々の出来事なので組にしない。
    fn pairs_with_waiting(&self, to: &Path) -> bool {
        self.rename_froms
            .front()
            .is_some_and(|front| !(self.exists)(&front.path) && (self.exists)(to))
    }

    /// 送出すべきものが溜まっているか。
    fn ready(&self, now: Instant) -> bool {
        let window_done = self
            .window_started
            .is_some_and(|start| now.duration_since(start) >= DEBOUNCE_WINDOW);
        let rename_expired = self
            .rename_froms
            .front()
            .is_some_and(|rename| now.duration_since(rename.at) >= RENAME_PAIR_WINDOW);
        window_done || rename_expired
    }

    fn take(&mut self, now: Instant) -> Vec<FileChange> {
        self.resolve_renames(Some(now));
        self.window_started = None;
        self.pending.take()
    }

    /// 監視終了時に、期限を待たずに残りをすべて出す。
    fn flush(&mut self) -> Vec<FileChange> {
        self.resolve_renames(None);
        self.window_started = None;
        self.pending.take()
    }

    /// 対を得られなかったリネーム元を単独の変更へ落とす。
    /// `now` が `None` のときは期限に関係なくすべて解決する。
    fn resolve_renames(&mut self, now: Option<Instant>) {
        while let Some(front) = self.rename_froms.front() {
            let expired = now.is_none_or(|now| now.duration_since(front.at) >= RENAME_PAIR_WINDOW);
            if !expired {
                break;
            }
            let rename = self.rename_froms.pop_front().expect("直前に存在を確認済み");
            // 方向が分かっている From は、対が来ないなら消えたということ。
            // 方向不明の場合だけは、実体が残っているかで作成と削除を見分ける。
            let kind = if rename.direction_unknown && (self.exists)(&rename.path) {
                FileChangeKind::Created
            } else {
                FileChangeKind::Removed
            };
            self.pending.push_simple(kind, &rename.path);
        }
    }
}

fn simple_kind(raw: RawKind) -> FileChangeKind {
    match raw {
        RawKind::Created => FileChangeKind::Created,
        RawKind::Removed => FileChangeKind::Removed,
        _ => FileChangeKind::Modified,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use notify::event::{CreateKind, DataChange, MetadataKind, RemoveKind};

    /// gitignore を持たない、常時無視だけが効くフィルタ。
    fn plain_filter() -> IgnoreFilter {
        IgnoreFilter {
            root: PathBuf::from("/ws"),
            gitignore: Gitignore::empty(),
        }
    }

    fn never_exists(_: &Path) -> bool {
        false
    }

    fn always_exists(_: &Path) -> bool {
        true
    }

    /// リネームが済んだ直後の実体。移動元だけが消えている。
    fn moved_away_from_old(path: &Path) -> bool {
        path != Path::new("/ws/old.rs")
    }

    fn debouncer(exists: fn(&Path) -> bool) -> Debouncer {
        Debouncer::new(plain_filter(), exists)
    }

    fn event(kind: EventKind, paths: &[&str]) -> notify::Event {
        paths.iter().fold(notify::Event::new(kind), |event, path| {
            event.add_path(PathBuf::from(path))
        })
    }

    fn created(paths: &[&str]) -> notify::Event {
        event(EventKind::Create(CreateKind::File), paths)
    }

    fn modified(paths: &[&str]) -> notify::Event {
        event(
            EventKind::Modify(ModifyKind::Data(DataChange::Content)),
            paths,
        )
    }

    fn removed(paths: &[&str]) -> notify::Event {
        event(EventKind::Remove(RemoveKind::File), paths)
    }

    fn renamed(mode: RenameMode, paths: &[&str]) -> notify::Event {
        event(EventKind::Modify(ModifyKind::Name(mode)), paths)
    }

    // -- 種別の写像 ------------------------------------------------------

    #[test]
    fn メタデータとアクセスのイベントは捨てられる() {
        assert_eq!(
            classify(&EventKind::Modify(ModifyKind::Metadata(
                MetadataKind::WriteTime
            ))),
            None
        );
        assert_eq!(
            classify(&EventKind::Access(notify::event::AccessKind::Read)),
            None
        );
    }

    #[test]
    fn 種別不明のイベントは変更として扱う() {
        // 精度の低い backend は Any しか返さない。捨てると監視が丸ごと効かなくなる。
        assert_eq!(classify(&EventKind::Any), Some(RawKind::Modified));
        assert_eq!(classify(&EventKind::Other), Some(RawKind::Modified));
    }

    // -- 無視フィルタ ----------------------------------------------------

    #[test]
    fn 常時無視のパスは捨てられる() {
        let filter = plain_filter();
        assert!(filter.is_ignored(Path::new("/ws/.git/index")));
        assert!(filter.is_ignored(Path::new("/ws/target/debug/foo")));
        assert!(filter.is_ignored(Path::new("/ws/web/node_modules/a/b.js")));
        assert!(filter.is_ignored(Path::new("/ws/src/.DS_Store")));
        assert!(!filter.is_ignored(Path::new("/ws/src/main.rs")));
    }

    #[test]
    fn ルート自体が無視名のフォルダ配下にあっても捨てない() {
        // `node_modules/pkg` を直接開く使い方がある。絶対パス全体を見ると
        // 配下のすべてのイベントが黙って消える。
        let filter = IgnoreFilter {
            root: PathBuf::from("/home/me/node_modules/pkg"),
            gitignore: Gitignore::empty(),
        };
        assert!(!filter.is_ignored(Path::new("/home/me/node_modules/pkg/src/index.js")));
        assert!(filter.is_ignored(Path::new("/home/me/node_modules/pkg/node_modules/dep/x.js")));
    }

    #[test]
    fn gitignore_の規則が反映される() {
        let mut builder = GitignoreBuilder::new("/ws");
        builder.add_line(None, "*.log").unwrap();
        builder.add_line(None, "build/").unwrap();
        let filter = IgnoreFilter {
            root: PathBuf::from("/ws"),
            gitignore: builder.build().unwrap(),
        };
        assert!(filter.is_ignored(Path::new("/ws/out/app.log")));
        assert!(filter.is_ignored(Path::new("/ws/build/app.js")));
        assert!(!filter.is_ignored(Path::new("/ws/src/app.rs")));
    }

    #[test]
    fn 監視ルート外のパスでも_panic_しない() {
        // matched_path_or_any_parents はルート外のパスで panic する仕様。
        let mut builder = GitignoreBuilder::new("/ws");
        builder.add_line(None, "*.log").unwrap();
        let filter = IgnoreFilter {
            root: PathBuf::from("/ws"),
            gitignore: builder.build().unwrap(),
        };
        assert!(!filter.is_ignored(Path::new("/elsewhere/app.log")));
    }

    #[test]
    fn 無視パスへの変更は溜まらない() {
        let mut debouncer = debouncer(never_exists);
        let now = Instant::now();
        debouncer.accept(&modified(&["/ws/target/debug/x"]), now);
        assert!(!debouncer.ready(now + DEBOUNCE_WINDOW));
        assert!(debouncer.flush().is_empty());
    }

    // -- 重複除去と畳み込み ----------------------------------------------

    #[test]
    fn 同一パスの連続変更は_1_件に畳まれる() {
        let mut debouncer = debouncer(never_exists);
        let now = Instant::now();
        for _ in 0..1000 {
            debouncer.accept(&modified(&["/ws/src/main.rs"]), now);
        }
        let changes = debouncer.take(now + DEBOUNCE_WINDOW);
        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0].kind, FileChangeKind::Modified);
        assert_eq!(changes[0].path, PathBuf::from("/ws/src/main.rs"));
    }

    #[test]
    fn 作成直後の書き込みは作成のまま送る() {
        let mut debouncer = debouncer(never_exists);
        let now = Instant::now();
        debouncer.accept(&created(&["/ws/a.rs"]), now);
        debouncer.accept(&modified(&["/ws/a.rs"]), now);
        let changes = debouncer.take(now + DEBOUNCE_WINDOW);
        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0].kind, FileChangeKind::Created);
    }

    #[test]
    fn ウィンドウ内で作られて消えたものは相殺される() {
        let mut debouncer = debouncer(never_exists);
        let now = Instant::now();
        debouncer.accept(&created(&["/ws/tmp.rs"]), now);
        debouncer.accept(&modified(&["/ws/keep.rs"]), now);
        debouncer.accept(&removed(&["/ws/tmp.rs"]), now);
        let changes = debouncer.take(now + DEBOUNCE_WINDOW);
        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0].path, PathBuf::from("/ws/keep.rs"));
    }

    #[test]
    fn 相殺されたスロットは後続の変更で作り直される() {
        let mut debouncer = debouncer(never_exists);
        let now = Instant::now();
        debouncer.accept(&created(&["/ws/a.rs"]), now);
        debouncer.accept(&removed(&["/ws/a.rs"]), now);
        debouncer.accept(&created(&["/ws/a.rs"]), now);
        let changes = debouncer.take(now + DEBOUNCE_WINDOW);
        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0].kind, FileChangeKind::Created);
    }

    #[test]
    fn 削除してから作り直すアトミック保存は変更として送る() {
        let mut debouncer = debouncer(never_exists);
        let now = Instant::now();
        debouncer.accept(&removed(&["/ws/a.rs"]), now);
        debouncer.accept(&created(&["/ws/a.rs"]), now);
        let changes = debouncer.take(now + DEBOUNCE_WINDOW);
        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0].kind, FileChangeKind::Modified);
    }

    #[test]
    fn 送出順は最初に現れた順を保つ() {
        let mut debouncer = debouncer(never_exists);
        let now = Instant::now();
        debouncer.accept(&modified(&["/ws/c.rs"]), now);
        debouncer.accept(&modified(&["/ws/a.rs"]), now);
        debouncer.accept(&modified(&["/ws/b.rs"]), now);
        debouncer.accept(&modified(&["/ws/a.rs"]), now);
        let paths: Vec<_> = debouncer
            .take(now + DEBOUNCE_WINDOW)
            .into_iter()
            .map(|c| c.path)
            .collect();
        assert_eq!(
            paths,
            vec![
                PathBuf::from("/ws/c.rs"),
                PathBuf::from("/ws/a.rs"),
                PathBuf::from("/ws/b.rs")
            ]
        );
    }

    // -- ウィンドウ ------------------------------------------------------

    #[test]
    fn ウィンドウが閉じるまで送出しない() {
        let mut debouncer = debouncer(never_exists);
        let now = Instant::now();
        debouncer.accept(&modified(&["/ws/a.rs"]), now);
        assert!(!debouncer.ready(now));
        assert!(!debouncer.ready(now + DEBOUNCE_WINDOW - Duration::from_millis(1)));
        assert!(debouncer.ready(now + DEBOUNCE_WINDOW));
    }

    #[test]
    fn ウィンドウの起点は最初のイベント時刻に固定される() {
        // 変更が途切れずに続いても、一定間隔で必ず送出できることの確認。
        let mut debouncer = debouncer(never_exists);
        let start = Instant::now();
        for step in 0..10 {
            debouncer.accept(
                &modified(&["/ws/a.rs"]),
                start + Duration::from_millis(step * 20),
            );
        }
        assert!(debouncer.ready(start + DEBOUNCE_WINDOW));
    }

    #[test]
    fn 送出後は新しいウィンドウが始まる() {
        let mut debouncer = debouncer(never_exists);
        let start = Instant::now();
        debouncer.accept(&modified(&["/ws/a.rs"]), start);
        assert_eq!(debouncer.take(start + DEBOUNCE_WINDOW).len(), 1);
        assert!(!debouncer.ready(start + DEBOUNCE_WINDOW));

        let second = start + DEBOUNCE_WINDOW;
        debouncer.accept(&modified(&["/ws/a.rs"]), second);
        assert!(!debouncer.ready(second));
        assert!(debouncer.ready(second + DEBOUNCE_WINDOW));
    }

    // -- リネーム --------------------------------------------------------

    #[test]
    fn 別イベントで来た_from_と_to_が組になる() {
        let mut debouncer = debouncer(never_exists);
        let now = Instant::now();
        debouncer.accept(&renamed(RenameMode::From, &["/ws/old.rs"]), now);
        debouncer.accept(
            &renamed(RenameMode::To, &["/ws/new.rs"]),
            now + Duration::from_millis(1),
        );
        let changes = debouncer.take(now + DEBOUNCE_WINDOW);
        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0].kind, FileChangeKind::Renamed);
        assert_eq!(changes[0].path, PathBuf::from("/ws/old.rs"));
        assert_eq!(changes[0].to, Some(PathBuf::from("/ws/new.rs")));
    }

    #[test]
    fn 単一イベントの_both_も組になる() {
        let mut debouncer = debouncer(never_exists);
        let now = Instant::now();
        debouncer.accept(
            &renamed(RenameMode::Both, &["/ws/old.rs", "/ws/new.rs"]),
            now,
        );
        let changes = debouncer.take(now + DEBOUNCE_WINDOW);
        assert_eq!(changes[0].kind, FileChangeKind::Renamed);
        assert_eq!(changes[0].to, Some(PathBuf::from("/ws/new.rs")));
    }

    #[test]
    fn 複数のリネームが同時に進んでも先着順で組になる() {
        // ビルド中は 1 ウィンドウに複数のリネームが入る。単一スロットだと取り違える。
        let mut debouncer = debouncer(never_exists);
        let now = Instant::now();
        debouncer.accept(&renamed(RenameMode::From, &["/ws/a"]), now);
        debouncer.accept(&renamed(RenameMode::To, &["/ws/a2"]), now);
        debouncer.accept(&renamed(RenameMode::From, &["/ws/b"]), now);
        debouncer.accept(&renamed(RenameMode::To, &["/ws/b2"]), now);
        let changes = debouncer.take(now + DEBOUNCE_WINDOW);
        assert_eq!(changes.len(), 2);
        assert_eq!(changes[0].to, Some(PathBuf::from("/ws/a2")));
        assert_eq!(changes[1].to, Some(PathBuf::from("/ws/b2")));
    }

    #[test]
    fn 対にならなかった_from_は削除として送る() {
        let mut debouncer = debouncer(never_exists);
        let now = Instant::now();
        debouncer.accept(&renamed(RenameMode::From, &["/ws/gone.rs"]), now);
        assert!(!debouncer.ready(now));
        let later = now + RENAME_PAIR_WINDOW;
        assert!(debouncer.ready(later));
        let changes = debouncer.take(later);
        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0].kind, FileChangeKind::Removed);
    }

    #[test]
    fn 対にならなかった_to_は作成として送る() {
        let mut debouncer = debouncer(never_exists);
        let now = Instant::now();
        debouncer.accept(&renamed(RenameMode::To, &["/ws/arrived.rs"]), now);
        let changes = debouncer.take(now + DEBOUNCE_WINDOW);
        assert_eq!(changes[0].kind, FileChangeKind::Created);
    }

    #[test]
    fn 方向不明のリネームは実体の有無で作成と削除を分ける() {
        // FSEvents は方向を教えないため、期限切れ時点で実体が残っていれば
        // 監視範囲の外から移動してきた = 作成とみなす。
        let now = Instant::now();

        let mut arrived = debouncer(always_exists);
        arrived.accept(&renamed(RenameMode::Any, &["/ws/x.rs"]), now);
        let changes = arrived.take(now + RENAME_PAIR_WINDOW);
        assert_eq!(changes[0].kind, FileChangeKind::Created);

        let mut left = debouncer(never_exists);
        left.accept(&renamed(RenameMode::Any, &["/ws/x.rs"]), now);
        let changes = left.take(now + RENAME_PAIR_WINDOW);
        assert_eq!(changes[0].kind, FileChangeKind::Removed);
    }

    #[test]
    fn 方向不明のリネームは元が消えて先が残るときだけ組になる() {
        let mut debouncer = debouncer(moved_away_from_old);
        let now = Instant::now();
        debouncer.accept(&renamed(RenameMode::Any, &["/ws/old.rs"]), now);
        debouncer.accept(&renamed(RenameMode::Any, &["/ws/new.rs"]), now);
        let changes = debouncer.take(now + DEBOUNCE_WINDOW);
        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0].kind, FileChangeKind::Renamed);
        assert_eq!(changes[0].path, PathBuf::from("/ws/old.rs"));
        assert_eq!(changes[0].to, Some(PathBuf::from("/ws/new.rs")));
    }

    #[test]
    fn 同時に走った_2_つのアトミック保存は取り違えない() {
        // 無視対象の一時ファイル側はペア付けの前に落ちるため、方向不明のリネームは
        // 本体 2 つだけが残る。先頭と機械的に組にすると `a.rs → b.rs` という
        // 存在しないリネームを送ってしまう。両方とも実在するので組にしない。
        let mut debouncer = debouncer(always_exists);
        let now = Instant::now();
        debouncer.accept(&renamed(RenameMode::Any, &["/ws/target/a.tmp"]), now);
        debouncer.accept(&renamed(RenameMode::Any, &["/ws/a.rs"]), now);
        debouncer.accept(&renamed(RenameMode::Any, &["/ws/target/b.tmp"]), now);
        debouncer.accept(&renamed(RenameMode::Any, &["/ws/b.rs"]), now);

        let changes = debouncer.take(now + RENAME_PAIR_WINDOW);
        assert_eq!(changes.len(), 2);
        assert!(changes.iter().all(|c| c.to.is_none()));
        assert_eq!(changes[0].kind, FileChangeKind::Created);
        assert_eq!(changes[0].path, PathBuf::from("/ws/a.rs"));
        assert_eq!(changes[1].kind, FileChangeKind::Created);
        assert_eq!(changes[1].path, PathBuf::from("/ws/b.rs"));
    }

    #[test]
    fn 一時ファイルからの置き換えは作成として送る() {
        // `target/` 配下から本体へ移す形。無視側は組にする前に落ちる。
        let mut debouncer = debouncer(never_exists);
        let now = Instant::now();
        debouncer.accept(
            &renamed(RenameMode::Both, &["/ws/target/tmp", "/ws/a.rs"]),
            now,
        );
        let changes = debouncer.take(now + DEBOUNCE_WINDOW);
        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0].kind, FileChangeKind::Created);
        assert_eq!(changes[0].path, PathBuf::from("/ws/a.rs"));
    }

    #[test]
    fn 無視フォルダへの移動は削除として送る() {
        let mut debouncer = debouncer(never_exists);
        let now = Instant::now();
        debouncer.accept(
            &renamed(RenameMode::Both, &["/ws/a.rs", "/ws/target/a.rs"]),
            now,
        );
        let changes = debouncer.take(now + DEBOUNCE_WINDOW);
        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0].kind, FileChangeKind::Removed);
        assert_eq!(changes[0].path, PathBuf::from("/ws/a.rs"));
    }

    #[test]
    fn 終了時のフラッシュは期限前のリネームも解決する() {
        let mut debouncer = debouncer(never_exists);
        let now = Instant::now();
        debouncer.accept(&renamed(RenameMode::From, &["/ws/gone.rs"]), now);
        let changes = debouncer.flush();
        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0].kind, FileChangeKind::Removed);
    }

    // -- 結合テスト ------------------------------------------------------

    #[tokio::test]
    async fn 実際のファイル作成がイベントとして届く() {
        let dir = std::env::temp_dir().join(format!("nebula-watch-it-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let (tx, mut rx) = broadcast::channel(64);
        let service = WatchService::new(tx);
        service.watch(WorkspaceId(1), &dir).unwrap();

        // FSEvents はストリーム開始直後のイベントを取りこぼすことがあるので、
        // 監視が立ち上がるのを待ってから触る。
        tokio::time::sleep(Duration::from_millis(300)).await;
        std::fs::write(dir.join("hello.rs"), "fn main() {}").unwrap();

        let expected = dir.canonicalize().unwrap().join("hello.rs");
        let found = tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                let Ok(Event::FilesChanged { changes, .. }) = rx.recv().await else {
                    continue;
                };
                if changes.iter().any(|c| c.path == expected) {
                    return;
                }
            }
        })
        .await;

        service.shutdown();
        let _ = std::fs::remove_dir_all(&dir);
        found.expect("2 秒以内に hello.rs の変更が届くこと");
    }

    #[tokio::test]
    async fn 二重の監視要求は成功して何もしない() {
        let dir = std::env::temp_dir().join(format!("nebula-watch-dup-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let (tx, _rx) = broadcast::channel(16);
        let service = WatchService::new(tx);
        service.watch(WorkspaceId(1), &dir).unwrap();
        service.watch(WorkspaceId(1), &dir).unwrap();
        assert_eq!(service.watches.lock().unwrap().len(), 1);

        service.unwatch(WorkspaceId(1));
        assert!(service.watches.lock().unwrap().is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn 存在しないパスの監視は失敗する() {
        let (tx, _rx) = broadcast::channel(16);
        let service = WatchService::new(tx);
        let missing = std::env::temp_dir().join("nebula-watch-missing-xyz");
        assert!(service.watch(WorkspaceId(1), &missing).is_err());
    }
}
