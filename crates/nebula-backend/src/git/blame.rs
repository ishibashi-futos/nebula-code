//! `git blame --line-porcelain` の解析。
//!
//! `--porcelain` ではなく `--line-porcelain` を使うのは、後者だと全行に著者情報が
//! 付いてくるため、コミットごとの情報をこちら側で表引きせずに済むから。
//! 行数ぶん冗長になるが、解析が状態を持たなくなる利点の方が大きい。

use nebula_protocol::BlameLine;

pub fn parse(output: &str) -> Vec<BlameLine> {
    let mut lines = Vec::new();
    let mut current: Option<BlameLine> = None;
    for raw in output.lines() {
        // 行の内容はタブ始まりで、1 エントリの終わりを兼ねる。
        if raw.starts_with('\t') {
            lines.extend(current.take());
            continue;
        }
        match current.as_mut() {
            Some(entry) => absorb(entry, raw),
            None => current = parse_header(raw),
        }
    }
    lines
}

/// `<sha> <元の行> <最終の行> [<続く行数>]` を読む。
fn parse_header(line: &str) -> Option<BlameLine> {
    let mut fields = line.split(' ');
    let commit = fields.next()?;
    if commit.len() != 40 || !commit.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    let _original_line = fields.next()?;
    let final_line: u32 = fields.next()?.parse().ok()?;
    Some(BlameLine {
        commit: commit.to_string(),
        author: String::new(),
        timestamp: 0,
        summary: String::new(),
        // git は 1 始まりで返すが、プロトコル側は 0 始まり。
        line: final_line.saturating_sub(1),
        // 未コミットの行はオール 0 の擬似 SHA になる。
        is_uncommitted: commit.bytes().all(|b| b == b'0'),
    })
}

fn absorb(entry: &mut BlameLine, line: &str) {
    let Some((key, value)) = line.split_once(' ') else {
        return;
    };
    match key {
        "author" => entry.author = value.to_string(),
        "author-time" => entry.timestamp = value.parse().unwrap_or(0),
        "summary" => entry.summary = value.to_string(),
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = "\
5115e0eb1245af53ee9fa6ed0f8446993386315a 1 1 1
author t
author-mail <t@t.t>
author-time 1786400199
author-tz +0900
committer t
committer-mail <t@t.t>
committer-time 1786400199
committer-tz +0900
summary add f
filename f.txt
\ta
0000000000000000000000000000000000000000 2 2 1
author Not Committed Yet
author-mail <not.committed.yet>
author-time 1786400221
author-tz +0900
committer Not Committed Yet
committer-mail <not.committed.yet>
committer-time 1786400221
committer-tz +0900
summary Version of g.txt from g.txt
previous 5115e0eb1245af53ee9fa6ed0f8446993386315a f.txt
filename g.txt
\tB
";

    #[test]
    fn 各行の著者と時刻を読む() {
        let lines = parse(SAMPLE);
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0].commit, "5115e0eb1245af53ee9fa6ed0f8446993386315a");
        assert_eq!(lines[0].author, "t");
        assert_eq!(lines[0].timestamp, 1786400199);
        assert_eq!(lines[0].summary, "add f");
        assert_eq!(lines[0].line, 0);
        assert!(!lines[0].is_uncommitted);
    }

    #[test]
    fn オールゼロのshaは未コミット扱いになる() {
        let lines = parse(SAMPLE);
        assert!(lines[1].is_uncommitted);
        assert_eq!(lines[1].line, 1);
        assert_eq!(lines[1].author, "Not Committed Yet");
    }

    #[test]
    fn 内容にタブが含まれても行がずれない() {
        let output = "\
5115e0eb1245af53ee9fa6ed0f8446993386315a 3 3 1
author t
author-time 1
summary s
\t\tインデント付きの行
";
        let lines = parse(output);
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0].line, 2);
    }

    #[test]
    fn 空の出力は空のまま() {
        assert!(parse("").is_empty());
    }
}
