//! ファイル名のあいまい (fuzzy) 一致スコアリング。
//!
//! クイックオープンは 1 打鍵ごとに数万件を採点するため、動的計画法ではなく
//! 前方走査 + 後方詰めの 2 パス (O(n)) で一致位置を決める。後方詰めを入れるのは、
//! 前方走査だけだと `src/search.rs` に対する `sr` が `s`(0), `r`(1) ではなく
//! `s`(0), `r`(2) のような緩い一致に落ち着き、連続一致ボーナスを取り逃すため。

/// 採点結果。
pub struct Hit {
    pub score: i32,
    /// 一致した文字位置 (char 単位)。UI の強調表示にそのまま渡せる。
    pub positions: Vec<u32>,
}

/// 1 文字一致の基礎点。
const BASE: i32 = 16;
/// 直前の文字と隣接している場合の加点。
const CONSECUTIVE: i32 = 16;
/// 単語境界 (区切り文字の直後、または camelCase の切れ目) での加点。
const BOUNDARY: i32 = 12;
/// ファイル名部分での一致の加点。ディレクトリ名より優先させる。
const IN_FILENAME: i32 = 18;
/// パス先頭またはファイル名先頭での一致の加点。
const AT_START: i32 = 10;
/// 大文字小文字まで一致した場合の加点。
const EXACT_CASE: i32 = 4;
/// 読み飛ばした 1 文字あたりの減点。
const GAP: i32 = -2;
/// 1 つの隙間で引く減点の下限。長いパスの途中一致が過度に沈まないようにする。
const GAP_FLOOR: i32 = -20;
/// パス長による減点の分母。同点なら短いパスを上に出すための緩い傾斜。
const LENGTH_DIVISOR: i32 = 8;

/// 相対パスのうちファイル名が始まる文字位置を返す。
pub fn name_start_of(relative: &str) -> usize {
    match relative.rfind(['/', '\\']) {
        Some(byte) => relative[..=byte].chars().count(),
        None => 0,
    }
}

/// `text` に対する `query` のあいまい一致を採点する。部分列でなければ `None`。
///
/// 比較は ASCII 範囲のみ大文字小文字を無視する。Unicode の完全なケースフォールディングは
/// 文字数が変わる組 (`İ` など) があり、char 単位の一致位置と辻褄が合わなくなるため使わない。
pub fn score(text: &str, query: &str, name_start: usize) -> Option<Hit> {
    let text: Vec<char> = text.chars().collect();
    let query: Vec<char> = query.chars().collect();
    if query.is_empty() {
        return None;
    }
    let end = match_forward(&text, &query)?;
    let positions = match_backward(&text, &query, end);
    Some(Hit {
        score: rate(&text, &query, &positions, name_start),
        positions: positions.iter().map(|&p| p as u32).collect(),
    })
}

/// 前方から最短で部分列を成立させ、最後の一致位置を返す。
fn match_forward(text: &[char], query: &[char]) -> Option<usize> {
    let mut qi = 0;
    for (i, c) in text.iter().enumerate() {
        if same(*c, query[qi]) {
            qi += 1;
            if qi == query.len() {
                return Some(i);
            }
        }
    }
    None
}

/// `end` から左へ詰め直し、可能な限り右寄せ (= 密) な一致位置を得る。
///
/// 前方走査が成功している範囲内でしか動かないので、必ず全文字ぶん埋まる。
fn match_backward(text: &[char], query: &[char], end: usize) -> Vec<usize> {
    let mut positions = vec![0usize; query.len()];
    let mut qi = query.len();
    let mut i = end + 1;
    while qi > 0 {
        i -= 1;
        if same(text[i], query[qi - 1]) {
            qi -= 1;
            positions[qi] = i;
        }
    }
    positions
}

fn rate(text: &[char], query: &[char], positions: &[usize], name_start: usize) -> i32 {
    let mut score = 0;
    let mut previous: Option<usize> = None;
    for (qi, &pos) in positions.iter().enumerate() {
        score += BASE;
        if text[pos] == query[qi] {
            score += EXACT_CASE;
        }
        if pos >= name_start {
            score += IN_FILENAME;
        }
        if pos == 0 || pos == name_start {
            score += AT_START;
        } else if is_boundary(text, pos) {
            score += BOUNDARY;
        }
        let gap = match previous {
            Some(p) if p + 1 == pos => {
                score += CONSECUTIVE;
                0
            }
            Some(p) => pos - p - 1,
            // 先頭からの距離も隙間として扱う。前方一致を優遇するため。
            None => pos,
        };
        score += (GAP * gap as i32).max(GAP_FLOOR);
        previous = Some(pos);
    }
    score - text.len() as i32 / LENGTH_DIVISOR
}

/// 区切り文字の直後、または camelCase の切れ目か。
fn is_boundary(text: &[char], pos: usize) -> bool {
    let Some(&prev) = pos.checked_sub(1).and_then(|i| text.get(i)) else {
        return true;
    };
    if matches!(prev, '/' | '\\' | '_' | '-' | '.' | ' ' | ':') {
        return true;
    }
    !prev.is_uppercase() && text[pos].is_uppercase()
}

fn same(a: char, b: char) -> bool {
    a == b || a.eq_ignore_ascii_case(&b)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// テストの見通しのため、相対パスから name_start を自動で求めて採点する。
    fn rank(relative: &str, query: &str) -> Option<Hit> {
        score(relative, query, name_start_of(relative))
    }

    #[test]
    fn 部分列でなければ一致しない() {
        assert!(rank("src/main.rs", "xyz").is_none());
        // 順序が違えば部分列にならない。
        assert!(rank("src/main.rs", "niam").is_none());
    }

    #[test]
    fn ファイル名部分の一致が優先される() {
        let in_name = rank("a/foo.rs", "foo").unwrap();
        let in_dir = rank("foo/a.rs", "foo").unwrap();
        assert!(
            in_name.score > in_dir.score,
            "a/foo.rs={} foo/a.rs={}",
            in_name.score,
            in_dir.score
        );
    }

    #[test]
    fn 連続一致が飛び石より高い() {
        let tight = rank("abc.txt", "abc").unwrap();
        let loose = rank("a_b_c.txt", "abc").unwrap();
        assert!(tight.score > loose.score);
    }

    #[test]
    fn 短いパスが同点時に上に来る() {
        let short = rank("src/main.rs", "main").unwrap();
        let long = rank("crates/nebula/src/main.rs", "main").unwrap();
        assert!(short.score > long.score);
    }

    #[test]
    fn 大文字小文字を無視して一致する() {
        let hit = rank("src/MainView.rs", "mainview").unwrap();
        assert_eq!(hit.positions, vec![4, 5, 6, 7, 8, 9, 10, 11]);
    }

    #[test]
    fn 一致位置は後方に詰められる() {
        // 前方走査だけなら s(0), r(1) を拾うが、後方詰めで search 側の連続一致を選ぶ。
        let hit = rank("src/search.rs", "sear").unwrap();
        assert_eq!(hit.positions, vec![4, 5, 6, 7]);
    }

    #[test]
    fn マルチバイトでも文字位置を返す() {
        let hit = score("日本語/foo.rs", "foo", name_start_of("日本語/foo.rs")).unwrap();
        assert_eq!(hit.positions, vec![4, 5, 6]);
    }

    #[test]
    fn ファイル名開始位置は文字単位で求まる() {
        assert_eq!(name_start_of("日本語/a.rs"), 4);
        assert_eq!(name_start_of("a.rs"), 0);
    }
}
