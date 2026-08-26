//! 変更一覧の組み立て (純粋関数)。
//!
//! `GitRepoStatus` を `uniform_list` が扱える平坦な行の列に変換するところだけを
//! 切り出してある。バックエンドとの通信や `GitView` の状態を一切持たないので、
//! ここだけは `GitView` を作らずにテストできる。

use crate::theme::Theme;
use gpui::{Hsla, Pixels, px};
use nebula_protocol::{GitBranch, GitFileStatus, GitRepoStatus, GitStatusCode};
use std::path::Path;

/// 一覧の 1 行の高さ。`uniform_list` は全行を同じ高さで扱うので、
/// 見出しもファイル行もこの値に揃える。
pub(super) const ROW_HEIGHT: Pixels = px(24.);

// ---------------------------------------------------------------------------
// 一覧の組み立て (純粋関数)
// ---------------------------------------------------------------------------

/// 変更一覧の区分。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum GitSection {
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

    pub(super) fn title(self) -> &'static str {
        match self {
            GitSection::Staged => "ステージ済み",
            GitSection::Changed => "変更",
            GitSection::Untracked => "未追跡",
        }
    }

    /// 見出しに置くまとめ操作の名前。
    pub(super) fn bulk_label(self) -> &'static str {
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
pub(super) enum GitRow {
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
pub(super) fn sections_for(entry: &GitFileStatus) -> Vec<GitSection> {
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
pub(super) fn flatten_rows(status: &GitRepoStatus, collapsed: &[GitSection]) -> Vec<GitRow> {
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
pub(super) fn split_path_display(path: &Path) -> (String, String) {
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
pub(super) fn format_sync_counts(ahead: u32, behind: u32) -> String {
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
pub(super) fn checkout_name(branch: &GitBranch) -> String {
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
// テスト
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

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
