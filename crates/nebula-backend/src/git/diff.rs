//! 行ガター用の差分。
//!
//! 保存済みの内容は git に任せて `--unified=0` の出力を読むだけでよいが、
//! 未保存の内容はディスクに無いので git に渡せない。一時ファイルを作って
//! `--no-index` に食わせる手もあるが、書き込み先の権限や後始末で壊れやすいため、
//! HEAD 版だけ取り出して行差分を自前で取る。

use nebula_protocol::{DiffHunk, HunkKind};

// ---------------------------------------------------------------------------
// unified diff の解析
// ---------------------------------------------------------------------------


fn kind_of(old_lines: u32, new_lines: u32) -> HunkKind {
    match (old_lines, new_lines) {
        (0, _) => HunkKind::Added,
        (_, 0) => HunkKind::Removed,
        _ => HunkKind::Modified,
    }
}

// ---------------------------------------------------------------------------
// 自前の行差分
// ---------------------------------------------------------------------------

/// 編集距離の上限。Myers のトレースは O(D^2) のメモリを食うので歯止めを置く。
/// これを超えるほど食い違っている場合、行ガターに個別のハンクを出しても読めないので、
/// 差分領域全体を 1 つの変更として畳む。
const MAX_EDIT_DISTANCE: usize = 1024;

/// HEAD 版 `old` と編集中の内容 `new` の行差分を取る。
pub fn line_diff(old: &str, new: &str) -> Vec<DiffHunk> {
    let old_lines: Vec<&str> = old.lines().collect();
    let new_lines: Vec<&str> = new.lines().collect();

    // 前後の一致部分を先に落とす。編集は局所的なので、これだけで Myers に渡る
    // 行数がたいてい数行まで縮み、計算量の上限が効く場面をほぼ無くせる。
    let head = common_prefix(&old_lines, &new_lines);
    let tail = common_suffix(&old_lines[head..], &new_lines[head..]);
    let old_mid = &old_lines[head..old_lines.len() - tail];
    let new_mid = &new_lines[head..new_lines.len() - tail];

    changes(old_mid, new_mid)
        .into_iter()
        .map(|change| DiffHunk {
            kind: kind_of(change.old_len as u32, change.new_len as u32),
            new_start: (head + change.new_start) as u32,
            new_lines: change.new_len as u32,
            old_start: (head + change.old_start) as u32,
            old_lines: change.old_len as u32,
            removed_text: old_mid[change.old_start..change.old_start + change.old_len]
                .iter()
                .map(|line| line.to_string())
                .collect(),
        })
        .collect()
}

fn common_prefix(old: &[&str], new: &[&str]) -> usize {
    old.iter().zip(new).take_while(|(a, b)| a == b).count()
}

fn common_suffix(old: &[&str], new: &[&str]) -> usize {
    old.iter()
        .rev()
        .zip(new.iter().rev())
        .take_while(|(a, b)| a == b)
        .count()
}

/// 一致部分を挟んで区切られた変更のかたまり。
#[derive(Debug, PartialEq, Eq)]
struct Change {
    old_start: usize,
    old_len: usize,
    new_start: usize,
    new_len: usize,
}

fn changes(old: &[&str], new: &[&str]) -> Vec<Change> {
    match myers_trace(old, new) {
        Some(trace) => backtrack(&trace, old.len(), new.len()),
        // 上限に達した場合は領域全体を 1 つの変更として返す。
        None => vec![Change {
            old_start: 0,
            old_len: old.len(),
            new_start: 0,
            new_len: new.len(),
        }],
    }
}

/// Myers の O(ND) 差分の前半。各編集距離 d を処理する直前の到達点表を記録する。
///
/// 表は `k` が `[-(d+1), d+1]` の範囲しか使われないため、その窓だけを切り出して
/// 持つ。全幅を毎回複製すると O(D * (N+M)) になり、大きなファイルで現実的でない。
/// 戻り値の `trace[d][k + d + 1]` が距離 `d` 時点の `k` 対角線上の到達 x 座標。
fn myers_trace(old: &[&str], new: &[&str]) -> Option<Vec<Vec<isize>>> {
    let n = old.len() as isize;
    let m = new.len() as isize;
    let max = (n + m) as usize;
    // k は [-max, max] を動き、更新時に ±1 まで参照するので余白を 1 つ足す。
    let offset = max as isize + 1;
    let mut reach = vec![0isize; 2 * max + 3];
    let mut trace = Vec::new();

    for d in 0..=max.min(MAX_EDIT_DISTANCE) {
        let d = d as isize;
        trace.push(reach[(offset - d - 1) as usize..=(offset + d + 1) as usize].to_vec());
        let mut k = -d;
        while k <= d {
            // 下に進む (new 側の行を挿入) か、右に進む (old 側の行を削除) かを、
            // 到達点が遠い方を選ぶ形で決める。
            let go_down =
                k == -d || (k != d && reach[(k - 1 + offset) as usize] < reach[(k + 1 + offset) as usize]);
            let mut x = if go_down {
                reach[(k + 1 + offset) as usize]
            } else {
                reach[(k - 1 + offset) as usize] + 1
            };
            let mut y = x - k;
            while x < n && y < m && old[x as usize] == new[y as usize] {
                x += 1;
                y += 1;
            }
            reach[(k + offset) as usize] = x;
            if x >= n && y >= m {
                return Some(trace);
            }
            k += 2;
        }
    }
    None
}

/// 到達点表を終点から逆にたどり、隣接する削除・挿入をひとかたまりにまとめる。
fn backtrack(trace: &[Vec<isize>], n: usize, m: usize) -> Vec<Change> {
    let mut x = n as isize;
    let mut y = m as isize;
    let mut steps = Vec::new();

    for d in (0..trace.len()).rev() {
        let reach = &trace[d];
        let d = d as isize;
        let at = |k: isize| reach[(k + d + 1) as usize];
        let k = x - y;
        let prev_k = if k == -d || (k != d && at(k - 1) < at(k + 1)) {
            k + 1
        } else {
            k - 1
        };
        let prev_x = at(prev_k);
        let prev_y = prev_x - prev_k;
        // 斜め移動 (一致行) は編集ではないので、ここで巻き戻しておく。
        while x > prev_x && y > prev_y {
            x -= 1;
            y -= 1;
        }
        if d > 0 {
            steps.push((prev_x as usize, prev_y as usize, x > prev_x));
        }
        x = prev_x;
        y = prev_y;
    }

    let mut result: Vec<Change> = Vec::new();
    for (old_pos, new_pos, is_delete) in steps.into_iter().rev() {
        match result.last_mut() {
            // 直前の変更に隙間なく続くなら同じかたまりに足す。削除と挿入が続けば
            // 1 つの Modified になる。
            Some(last)
                if last.old_start + last.old_len == old_pos
                    && last.new_start + last.new_len == new_pos =>
            {
                if is_delete {
                    last.old_len += 1;
                } else {
                    last.new_len += 1;
                }
            }
            _ => result.push(Change {
                old_start: old_pos,
                old_len: usize::from(is_delete),
                new_start: new_pos,
                new_len: usize::from(!is_delete),
            }),
        }
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;







    // -- 自前の行差分 --

    #[test]
    fn 同一内容なら差分は無い() {
        assert!(line_diff("a\nb\nc\n", "a\nb\nc\n").is_empty());
    }

    #[test]
    fn 一行の置換を検出する() {
        let hunks = line_diff("a\nb\nc\n", "a\nB\nc\n");
        assert_eq!(hunks.len(), 1);
        assert_eq!(hunks[0].kind, HunkKind::Modified);
        assert_eq!((hunks[0].old_start, hunks[0].old_lines), (1, 1));
        assert_eq!((hunks[0].new_start, hunks[0].new_lines), (1, 1));
        assert_eq!(hunks[0].removed_text, vec!["b".to_string()]);
    }

    #[test]
    fn 末尾への追加を検出する() {
        let hunks = line_diff("a\nb\n", "a\nb\nc\nd\n");
        assert_eq!(hunks.len(), 1);
        assert_eq!(hunks[0].kind, HunkKind::Added);
        assert_eq!((hunks[0].old_start, hunks[0].old_lines), (2, 0));
        assert_eq!((hunks[0].new_start, hunks[0].new_lines), (2, 2));
        assert!(hunks[0].removed_text.is_empty());
    }

    #[test]
    fn 中間の削除を検出する() {
        let hunks = line_diff("a\nb\nc\nd\n", "a\nd\n");
        assert_eq!(hunks.len(), 1);
        assert_eq!(hunks[0].kind, HunkKind::Removed);
        assert_eq!((hunks[0].old_start, hunks[0].old_lines), (1, 2));
        assert_eq!((hunks[0].new_start, hunks[0].new_lines), (1, 0));
        assert_eq!(hunks[0].removed_text, vec!["b".to_string(), "c".to_string()]);
    }

    #[test]
    fn 離れた二箇所の変更は別のハンクになる() {
        let hunks = line_diff("a\nb\nc\nd\ne\n", "A\nb\nc\nd\nE\n");
        assert_eq!(hunks.len(), 2);
        assert_eq!((hunks[0].new_start, hunks[0].new_lines), (0, 1));
        assert_eq!((hunks[1].new_start, hunks[1].new_lines), (4, 1));
    }

    #[test]
    fn 空から全追加になる() {
        let hunks = line_diff("", "a\nb\n");
        assert_eq!(hunks.len(), 1);
        assert_eq!(hunks[0].kind, HunkKind::Added);
        assert_eq!((hunks[0].new_start, hunks[0].new_lines), (0, 2));
    }

    #[test]
    fn 全削除で空になる() {
        let hunks = line_diff("a\nb\n", "");
        assert_eq!(hunks.len(), 1);
        assert_eq!(hunks[0].kind, HunkKind::Removed);
        assert_eq!(hunks[0].removed_text, vec!["a".to_string(), "b".to_string()]);
    }

    #[test]
    fn 両方空なら差分は無い() {
        assert!(line_diff("", "").is_empty());
    }

    #[test]
    fn 挿入と削除が隣接すると一つの変更になる() {
        let hunks = line_diff("a\nb\nc\n", "a\nx\ny\nz\nc\n");
        assert_eq!(hunks.len(), 1);
        assert_eq!(hunks[0].kind, HunkKind::Modified);
        assert_eq!((hunks[0].old_start, hunks[0].old_lines), (1, 1));
        assert_eq!((hunks[0].new_start, hunks[0].new_lines), (1, 3));
    }

    #[test]
    fn 上限を超える差分は一つに畳まれる() {
        let old: String = (0..MAX_EDIT_DISTANCE + 100)
            .map(|i| format!("old{i}\n"))
            .collect();
        let new: String = (0..MAX_EDIT_DISTANCE + 100)
            .map(|i| format!("new{i}\n"))
            .collect();
        let hunks = line_diff(&old, &new);
        assert_eq!(hunks.len(), 1);
        assert_eq!(hunks[0].kind, HunkKind::Modified);
        assert_eq!(hunks[0].old_lines, (MAX_EDIT_DISTANCE + 100) as u32);
    }

    #[test]
    fn 大きなファイルの一行変更でも一つのハンクだけ出る() {
        let old: String = (0..5000).map(|i| format!("line{i}\n")).collect();
        let new = old.replacen("line2500\n", "変更\n", 1);
        let hunks = line_diff(&old, &new);
        assert_eq!(hunks.len(), 1);
        assert_eq!(hunks[0].new_start, 2500);
        assert_eq!(hunks[0].removed_text, vec!["line2500".to_string()]);
    }
}
