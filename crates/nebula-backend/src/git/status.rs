//! `git status --porcelain=v2 --branch --untracked-files=all` の解析。
//!
//! porcelain=v2 を使うのは、v1 と違って行ごとに種別を表す先頭トークンが付き、
//! リネーム元とリネーム先が 1 行で判別できるため。文字列は git の版に関わらず安定する。

use nebula_protocol::{GitFileStatus, GitRepoStatus, GitStatusCode};
use std::path::PathBuf;

/// 出力全体を `GitRepoStatus` に写す。`in_progress` は別途 `.git` の中身で決めるので
/// ここでは触らない。
pub fn parse(output: &str) -> GitRepoStatus {
    let mut status = GitRepoStatus::default();
    for line in output.lines() {
        match line.strip_prefix("# ") {
            Some(header) => parse_header(header, &mut status),
            None => status.entries.extend(parse_entry(line)),
        }
    }
    status
}

fn parse_header(header: &str, status: &mut GitRepoStatus) {
    let Some((key, value)) = header.split_once(' ') else {
        return;
    };
    match key {
        // 分離 HEAD では枝名の代わりに "(detached)" が入る。枝ではないので持たない。
        "branch.head" => status.branch = (value != "(detached)").then(|| value.to_string()),
        "branch.upstream" => status.upstream = Some(value.to_string()),
        // "+3 -1" の形。上流が無い場合はこの行自体が出ない。
        "branch.ab" => {
            for token in value.split_whitespace() {
                if let Some(count) = token.strip_prefix('+') {
                    status.ahead = count.parse().unwrap_or(0);
                } else if let Some(count) = token.strip_prefix('-') {
                    status.behind = count.parse().unwrap_or(0);
                }
            }
        }
        _ => {}
    }
}

fn parse_entry(line: &str) -> Option<GitFileStatus> {
    let (kind, rest) = line.split_once(' ')?;
    match kind {
        // 1 <XY> <sub> <mH> <mI> <mW> <hH> <hI> <path>
        "1" => {
            let fields = split_fixed(rest, 8)?;
            let (index, worktree) = codes(fields[0])?;
            Some(GitFileStatus {
                path: PathBuf::from(fields[7]),
                original_path: None,
                index,
                worktree,
            })
        }
        // 2 <XY> <sub> <mH> <mI> <mW> <hH> <hI> <X><score> <path>\t<origPath>
        "2" => {
            let fields = split_fixed(rest, 9)?;
            let (index, worktree) = codes(fields[0])?;
            let (path, original) = fields[8].split_once('\t')?;
            Some(GitFileStatus {
                path: PathBuf::from(path),
                original_path: Some(PathBuf::from(original)),
                index,
                worktree,
            })
        }
        // u <XY> <sub> <m1> <m2> <m3> <mW> <h1> <h2> <h3> <path>
        //
        // XY は "UU" や "AA" など衝突の種類を表すが、UI では衝突かどうかだけ分かれば
        // よいので両側とも Conflicted に畳む。
        "u" => {
            let fields = split_fixed(rest, 10)?;
            Some(GitFileStatus {
                path: PathBuf::from(fields[9]),
                original_path: None,
                index: GitStatusCode::Conflicted,
                worktree: GitStatusCode::Conflicted,
            })
        }
        "?" => Some(GitFileStatus {
            path: PathBuf::from(rest),
            original_path: None,
            index: GitStatusCode::Unmodified,
            worktree: GitStatusCode::Untracked,
        }),
        "!" => Some(GitFileStatus {
            path: PathBuf::from(rest),
            original_path: None,
            index: GitStatusCode::Ignored,
            worktree: GitStatusCode::Ignored,
        }),
        _ => None,
    }
}

/// 先頭から `count - 1` 個を空白で切り、残り全部を最後の要素にする。
/// パスに空白が含まれても壊れないようにするための分割。
fn split_fixed(rest: &str, count: usize) -> Option<Vec<&str>> {
    let fields: Vec<&str> = rest.splitn(count, ' ').collect();
    (fields.len() == count).then_some(fields)
}

fn codes(xy: &str) -> Option<(GitStatusCode, GitStatusCode)> {
    let bytes = xy.as_bytes();
    Some((code(*bytes.first()?), code(*bytes.get(1)?)))
}

fn code(byte: u8) -> GitStatusCode {
    match byte {
        // T は型変更 (通常ファイル ⇄ シンボリックリンク)。GitStatusCode に対応する
        // 値が無く、UI 上も「変更された」で足りるため Modified に寄せる。
        b'M' | b'T' => GitStatusCode::Modified,
        b'A' => GitStatusCode::Added,
        b'D' => GitStatusCode::Deleted,
        b'R' => GitStatusCode::Renamed,
        b'C' => GitStatusCode::Copied,
        b'U' => GitStatusCode::Conflicted,
        // '.' は「その側に変更なし」。未知の文字もここに落とす。
        _ => GitStatusCode::Unmodified,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 枝と上流と乖離数を読む() {
        let output = "\
# branch.oid bcb585ad3af693f75202f783e8ca258bdcc72c57
# branch.head main
# branch.upstream origin/main
# branch.ab +1 -2
";
        let status = parse(output);
        assert_eq!(status.branch.as_deref(), Some("main"));
        assert_eq!(status.upstream.as_deref(), Some("origin/main"));
        assert_eq!(status.ahead, 1);
        assert_eq!(status.behind, 2);
        assert!(status.entries.is_empty());
    }

    #[test]
    fn 分離headでは枝名を持たない() {
        let status = parse("# branch.head (detached)\n");
        assert_eq!(status.branch, None);
    }

    #[test]
    fn 通常変更行を読む() {
        let output = "1 .M N... 100644 100644 100644 \
de980441c3ab03a8c07dda1ad27b8a11f39deb1e de980441c3ab03a8c07dda1ad27b8a11f39deb1e src/main.rs\n";
        let entry = &parse(output).entries[0];
        assert_eq!(entry.path, PathBuf::from("src/main.rs"));
        assert_eq!(entry.index, GitStatusCode::Unmodified);
        assert_eq!(entry.worktree, GitStatusCode::Modified);
        assert_eq!(entry.original_path, None);
    }

    #[test]
    fn 空白と日本語を含むパスを読む() {
        let output = "1 A. N... 000000 100644 100644 \
0000000000000000000000000000000000000000 975fbec8256d3e8a3797e7a3611380f27c49f4ac sub/日本語 ファイル.txt\n";
        let entry = &parse(output).entries[0];
        assert_eq!(entry.path, PathBuf::from("sub/日本語 ファイル.txt"));
        assert_eq!(entry.index, GitStatusCode::Added);
    }

    #[test]
    fn リネーム行はリネーム元を持つ() {
        let output = "2 RM N... 100644 100644 100644 \
de980441c3ab03a8c07dda1ad27b8a11f39deb1e de980441c3ab03a8c07dda1ad27b8a11f39deb1e R100 g.txt\tf.txt\n";
        let entry = &parse(output).entries[0];
        assert_eq!(entry.path, PathBuf::from("g.txt"));
        assert_eq!(entry.original_path, Some(PathBuf::from("f.txt")));
        assert_eq!(entry.index, GitStatusCode::Renamed);
        assert_eq!(entry.worktree, GitStatusCode::Modified);
    }

    #[test]
    fn 衝突行は両側とも衝突になる() {
        let output = "u UU N... 100644 100644 100644 100644 \
df967b96a579e45a18b8251732d16804b2e56a55 ba2906d0666cf726c7eaadd2cd3db615dedfdf3a \
e45c9c2666d44e0327c1f9c239a74c508336053e c.txt\n";
        let entry = &parse(output).entries[0];
        assert_eq!(entry.path, PathBuf::from("c.txt"));
        assert_eq!(entry.index, GitStatusCode::Conflicted);
        assert_eq!(entry.worktree, GitStatusCode::Conflicted);
    }

    #[test]
    fn 未追跡と無視を読む() {
        let status = parse("? untracked.txt\n! target/debug\n");
        assert_eq!(status.entries[0].worktree, GitStatusCode::Untracked);
        assert_eq!(status.entries[0].index, GitStatusCode::Unmodified);
        assert_eq!(status.entries[1].worktree, GitStatusCode::Ignored);
    }

    #[test]
    fn 壊れた行は黙って捨てる() {
        assert!(parse("1 .M short\nおかしな行\n").entries.is_empty());
    }
}
