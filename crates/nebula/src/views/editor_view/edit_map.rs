//! 編集適用後のカーソル位置の再計算。
//!
//! 入出力が明快な純粋関数なので、単体テストで挙動を固定しやすいようここへ切り出した。

use nebula_core::Selection;
use nebula_protocol::Edit;

/// 編集適用後のカーソル位置を求める。
///
/// 編集の範囲はすべて **適用前** の座標系。したがって各カーソルについて、
/// 元の位置と各編集の範囲を比べてずれ幅を合計する。累積した位置と比べてしまうと、
/// 2 つ目以降の編集の座標系が食い違う。
///
/// 挿入がカーソル位置ちょうどで起きた場合はカーソルを挿入テキストの右へ送る。
/// これが「文字を打つとカーソルがその右へ動く」挙動になる。
pub(super) fn map_selections_through(before: &[Selection], edits: &[Edit]) -> Vec<Selection> {
    before
        .iter()
        .map(|sel| {
            let head = map_offset_through_edits(sel.head, edits);
            let anchor = map_offset_through_edits(sel.anchor, edits);
            // 編集後は選択を解除してキャレットにする。
            Selection::caret(head.max(anchor))
        })
        .collect()
}

fn map_offset_through_edits(offset: usize, edits: &[Edit]) -> usize {
    let mut shift: isize = 0;
    let mut inside: Option<(usize, usize)> = None;
    for edit in edits {
        let new_len = edit.text.chars().count();
        if edit.range.end <= offset {
            shift += new_len as isize - edit.range.len() as isize;
        } else if edit.range.start <= offset {
            // 置換された範囲の内側にいたカーソルは、置換後テキストの末尾へ寄せる。
            inside = Some((edit.range.start, new_len));
        }
    }
    match inside {
        Some((start, new_len)) => (start as isize + shift) as usize + new_len,
        None => (offset as isize + shift).max(0) as usize,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use nebula_protocol::TextRange;

    #[test]
    fn 編集後のカーソルが挿入文字の右に来る() {
        let before = vec![Selection::caret(3)];
        let edits = vec![Edit::insert(3, "xy")];
        assert_eq!(
            map_selections_through(&before, &edits),
            vec![Selection::caret(5)]
        );
    }

    #[test]
    fn 選択を置換するとカーソルが置換後の末尾に来る() {
        let before = vec![Selection::new(2, 6)];
        let edits = vec![Edit::replace(TextRange::new(2, 6), "Z")];
        assert_eq!(
            map_selections_through(&before, &edits),
            vec![Selection::caret(3)]
        );
    }

    #[test]
    fn マーカー行だけを消す置換でカーソルが行頭に来る() {
        // Markdown のリスト打ち切り (on_newline の Terminate 分岐) は、
        // マーカー行全体を空文字に置換する。キャレットが行末 (マーカー直後) に
        // あっても行内の途中にあっても、置換後は行頭に戻ってくる必要がある
        // (でないと空になった行の外、次の行の文字の中にキャレットが迷い込む)。
        let edits = vec![Edit::replace(TextRange::new(3, 9), "")];
        assert_eq!(
            map_selections_through(&[Selection::caret(9)], &edits),
            vec![Selection::caret(3)],
            "マーカー直後 (行末) のキャレット"
        );
        assert_eq!(
            map_selections_through(&[Selection::caret(6)], &edits),
            vec![Selection::caret(3)],
            "マーカーの途中のキャレット"
        );
    }

    #[test]
    fn 複数カーソルの編集で後ろのカーソルもずれる() {
        let before = vec![Selection::caret(1), Selection::caret(5)];
        let edits = vec![Edit::insert(1, "ab"), Edit::insert(5, "cd")];
        let after = map_selections_through(&before, &edits);
        assert_eq!(after[0], Selection::caret(3), "1 番目は自分の挿入ぶん");
        assert_eq!(after[1], Selection::caret(9), "2 番目は両方の挿入ぶん");
    }
}
