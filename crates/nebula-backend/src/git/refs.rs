//! 枝一覧とコミット履歴の解析。
//!
//! どちらも git 側で出力書式を指定できるので、区切り文字を自分で決めて 1 回の
//! 呼び出しで必要な項目をまとめて取る。枝ごとに `git log` を回すような作りにしない。

use nebula_protocol::{GitBranch, GitCommitInfo};

/// `for-each-ref` の書式。タブ区切り。枝名にタブは入れられないので衝突しない。
pub const BRANCH_FORMAT: &str = "%(refname)%09%(HEAD)%09%(upstream:short)%09%(contents:subject)";

/// `log` の書式。コミット本文には改行もタブも入るため、印字されない
/// 単位区切り (0x1f) とレコード区切り (0x1e) を使う。
pub const LOG_FORMAT: &str = "%H%x1f%h%x1f%an%x1f%ae%x1f%at%x1f%s%x1f%b%x1e";

pub fn parse_branches(output: &str) -> Vec<GitBranch> {
    output.lines().filter_map(parse_branch).collect()
}

fn parse_branch(line: &str) -> Option<GitBranch> {
    let fields: Vec<&str> = line.splitn(4, '\t').collect();
    if fields.len() != 4 {
        return None;
    }
    let (is_remote, name) = match fields[0].strip_prefix("refs/heads/") {
        Some(name) => (false, name),
        None => (true, fields[0].strip_prefix("refs/remotes/")?),
    };
    // refs/remotes/<remote>/HEAD は既定枝への別名にすぎず、枝として選ばせる意味がない。
    if is_remote && name.ends_with("/HEAD") {
        return None;
    }
    Some(GitBranch {
        name: name.to_string(),
        // 現在の枝だけ "*"、それ以外は空白 1 文字。
        is_head: fields[1] == "*",
        is_remote,
        upstream: (!fields[2].is_empty()).then(|| fields[2].to_string()),
        last_commit_summary: fields[3].to_string(),
    })
}

pub fn parse_log(output: &str) -> Vec<GitCommitInfo> {
    output.split('\x1e').filter_map(parse_commit).collect()
}

fn parse_commit(record: &str) -> Option<GitCommitInfo> {
    // レコード区切りの後ろに改行が続くので、次のレコードの先頭から取り除く。
    let record = record.trim_start_matches('\n');
    if record.is_empty() {
        return None;
    }
    let fields: Vec<&str> = record.splitn(7, '\x1f').collect();
    if fields.len() != 7 {
        return None;
    }
    Some(GitCommitInfo {
        hash: fields[0].to_string(),
        short_hash: fields[1].to_string(),
        author: fields[2].to_string(),
        email: fields[3].to_string(),
        timestamp: fields[4].parse().unwrap_or(0),
        summary: fields[5].to_string(),
        // %b は本文が空でも末尾に改行を付けるので落とす。
        body: fields[6].trim_end().to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 現在の枝と上流を読む() {
        let output = "refs/heads/main\t*\torigin/main\tc2\n\
refs/heads/feature\t \t\t作業中の変更\n\
refs/remotes/origin/main\t \t\tc1\n";
        let branches = parse_branches(output);
        assert_eq!(branches.len(), 3);

        assert_eq!(branches[0].name, "main");
        assert!(branches[0].is_head);
        assert!(!branches[0].is_remote);
        assert_eq!(branches[0].upstream.as_deref(), Some("origin/main"));
        assert_eq!(branches[0].last_commit_summary, "c2");

        assert_eq!(branches[1].name, "feature");
        assert!(!branches[1].is_head);
        assert_eq!(branches[1].upstream, None);
        assert_eq!(branches[1].last_commit_summary, "作業中の変更");

        assert_eq!(branches[2].name, "origin/main");
        assert!(branches[2].is_remote);
    }

    #[test]
    fn リモートのhead別名は除かれる() {
        let output = "refs/remotes/origin/HEAD\t \t\tc1\n";
        assert!(parse_branches(output).is_empty());
    }

    #[test]
    fn 履歴の各項目を読む() {
        let output = "\
5115e0e\u{1f}5115e0e\u{1f}石橋\u{1f}i@example.com\u{1f}1786400199\u{1f}add f\u{1f}\u{1e}\n\
705e6a5\u{1f}705e6a5\u{1f}t\u{1f}t@t.t\u{1f}1786400119\u{1f}init\u{1f}詳しい説明\n二行目\n\u{1e}\n";
        let commits = parse_log(output);
        assert_eq!(commits.len(), 2);

        assert_eq!(commits[0].hash, "5115e0e");
        assert_eq!(commits[0].author, "石橋");
        assert_eq!(commits[0].email, "i@example.com");
        assert_eq!(commits[0].timestamp, 1786400199);
        assert_eq!(commits[0].summary, "add f");
        assert_eq!(commits[0].body, "");

        assert_eq!(commits[1].summary, "init");
        assert_eq!(commits[1].body, "詳しい説明\n二行目");
    }

    #[test]
    fn 空の履歴は空のまま() {
        assert!(parse_log("").is_empty());
    }
}
