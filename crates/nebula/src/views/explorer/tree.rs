//! 表示行の組み立て。
//!
//! 取得済みの階層 (`children`) と展開状態 (`expanded`) から、`uniform_list` に渡す
//! 表示行の平坦な列を作る純粋関数の塊。描画 (`ExplorerView`) から独立して
//! 検査できるよう、ここへ切り出してある。

use gpui::{Pixels, px};
use nebula_protocol::{DirEntry, FileChange, FileChangeKind};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

/// 行の高さ。`uniform_list` は先頭行を測って全行に適用するため、
/// 通常行と入力中の行で必ず同じ値を使う。
pub(super) const ROW_HEIGHT: Pixels = px(22.);
/// 1 段ぶんのインデント。
pub(super) const INDENT_STEP: f32 = 12.;

/// 平坦化された表示行 1 つ。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct TreeRow {
    /// 実在する項目のパス。仮行の場合は「作成先の親フォルダ」。
    pub(super) path: PathBuf,
    pub(super) name: String,
    pub(super) depth: usize,
    pub(super) is_dir: bool,
    pub(super) is_ignored: bool,
    /// 新規作成の入力欄だけを出す仮の行か。
    pub(super) is_draft: bool,
}

/// フォルダ直下の並び順を決める。
///
/// フォルダを先に、その中は大文字小文字を無視した名前順。バックエンドの返す順は
/// ファイルシステム依存なので、表示側で必ず正規化する。
pub(super) fn sort_entries(mut entries: Vec<DirEntry>) -> Vec<DirEntry> {
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
pub(super) fn flatten(
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
pub(super) fn insert_draft_row(rows: &mut Vec<TreeRow>, root: &Path, parent: &Path, is_dir: bool) {
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
pub(super) fn affected_dirs(changes: &[FileChange]) -> Vec<PathBuf> {
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
pub(super) fn changed_ancestor_dirs(root: &Path, paths: &[PathBuf]) -> HashSet<PathBuf> {
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
}
