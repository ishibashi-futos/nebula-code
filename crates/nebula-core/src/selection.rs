//! カーソルと選択範囲、およびその移動。
//!
//! すべてのオフセットはバッファ先頭からの **文字 (char) 単位**。ただし左右移動だけは
//! 書記素クラスタ単位で行う。絵文字や結合文字の途中にカーソルが止まらないようにするため。

use crate::rope_ext::RopeExt;
use nebula_protocol::{Position, TextRange};
use ropey::Rope;
use unicode_segmentation::UnicodeSegmentation;

/// 1 本のカーソル。`anchor == head` なら選択なしの単なるキャレット。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Selection {
    /// 選択の固定端。
    pub anchor: usize,
    /// 選択の可動端。キャレットが描かれる位置。
    pub head: usize,
    /// 上下移動時に保持したい桁。短い行を通過しても元の桁に戻れるようにする。
    pub goal_column: Option<u32>,
}

impl Selection {
    pub const fn caret(at: usize) -> Self {
        Self {
            anchor: at,
            head: at,
            goal_column: None,
        }
    }

    pub const fn new(anchor: usize, head: usize) -> Self {
        Self {
            anchor,
            head,
            goal_column: None,
        }
    }

    pub const fn is_empty(&self) -> bool {
        self.anchor == self.head
    }

    pub fn start(&self) -> usize {
        self.anchor.min(self.head)
    }

    pub fn end(&self) -> usize {
        self.anchor.max(self.head)
    }

    pub fn range(&self) -> TextRange {
        TextRange::new(self.start(), self.end())
    }

    /// 選択を解除し、キャレットだけにする。
    pub fn collapse(&mut self) {
        self.anchor = self.head;
    }

    /// 編集によってずれたオフセットを追従させる。
    ///
    /// `range` を長さ `new_len` のテキストで置換したときの新しい位置を計算する。
    /// 置換範囲の内側にあった位置は範囲末尾へ寄せる。
    pub fn map_through_edit(&mut self, range: TextRange, new_len: usize) {
        self.anchor = map_offset(self.anchor, range, new_len);
        self.head = map_offset(self.head, range, new_len);
    }
}

pub fn map_offset(offset: usize, range: TextRange, new_len: usize) -> usize {
    if offset <= range.start {
        offset
    } else if offset >= range.end {
        offset - range.len() + new_len
    } else {
        range.start + new_len
    }
}

/// 移動の単位。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Movement {
    /// 書記素クラスタ 1 つ分。
    Grapheme,
    /// 単語境界まで。
    Word,
    /// 行頭・行末まで (行頭は空白を除いた最初の文字を優先する)。
    LineBoundary,
    /// 上下 1 行。
    Line,
    /// 上下 1 画面。呼び出し側が行数を渡す。
    Page(u32),
    /// バッファ先頭・末尾。
    Buffer,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    Backward,
    Forward,
}

/// 選択を移動する。`extend` が真なら anchor を固定したまま head だけ動かす。
pub fn move_selection(
    rope: &Rope,
    selection: &mut Selection,
    movement: Movement,
    direction: Direction,
    extend: bool,
) {
    // 選択がある状態で extend せずに左右移動した場合、選択端へ畳むのが一般的な挙動。
    if !extend
        && !selection.is_empty()
        && matches!(movement, Movement::Grapheme)
    {
        selection.head = match direction {
            Direction::Backward => selection.start(),
            Direction::Forward => selection.end(),
        };
        selection.anchor = selection.head;
        selection.goal_column = None;
        return;
    }

    let new_head = match movement {
        Movement::Grapheme => move_grapheme(rope, selection.head, direction),
        Movement::Word => move_word(rope, selection.head, direction),
        Movement::LineBoundary => move_line_boundary(rope, selection.head, direction),
        Movement::Line => {
            let goal = selection
                .goal_column
                .unwrap_or_else(|| rope.offset_to_position(selection.head).column);
            let target = move_vertical(rope, selection.head, direction, 1, goal);
            selection.goal_column = Some(goal);
            selection.head = target;
            if !extend {
                selection.anchor = target;
            }
            return;
        }
        Movement::Page(rows) => {
            let goal = selection
                .goal_column
                .unwrap_or_else(|| rope.offset_to_position(selection.head).column);
            let target = move_vertical(rope, selection.head, direction, rows, goal);
            selection.goal_column = Some(goal);
            selection.head = target;
            if !extend {
                selection.anchor = target;
            }
            return;
        }
        Movement::Buffer => match direction {
            Direction::Backward => 0,
            Direction::Forward => rope.len_chars(),
        },
    };

    selection.head = new_head;
    selection.goal_column = None;
    if !extend {
        selection.anchor = new_head;
    }
}

/// 書記素クラスタ 1 つ分の移動先を返す。
fn move_grapheme(rope: &Rope, offset: usize, direction: Direction) -> usize {
    match direction {
        Direction::Forward => {
            if offset >= rope.len_chars() {
                return offset;
            }
            // 現在位置から先を少しだけ切り出して最初のクラスタ長を測る。
            // 4 文字あれば実用上のクラスタはほぼ収まり、足りなければ 1 文字進めるだけに退化する。
            let end = (offset + 8).min(rope.len_chars());
            let chunk = rope.slice(offset..end).to_string();
            let len = chunk
                .graphemes(true)
                .next()
                .map(|g| g.chars().count())
                .unwrap_or(1);
            (offset + len.max(1)).min(rope.len_chars())
        }
        Direction::Backward => {
            if offset == 0 {
                return 0;
            }
            let start = offset.saturating_sub(8);
            let chunk = rope.slice(start..offset).to_string();
            let len = chunk
                .graphemes(true)
                .next_back()
                .map(|g| g.chars().count())
                .unwrap_or(1);
            offset.saturating_sub(len.max(1))
        }
    }
}

/// 文字の分類。単語移動の境界判定に使う。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CharClass {
    Whitespace,
    /// 識別子を構成しうる文字 (英数字・アンダースコア・非 ASCII の文字)。
    Word,
    /// 記号。
    Punctuation,
}

fn classify(c: char) -> CharClass {
    if c.is_whitespace() {
        CharClass::Whitespace
    } else if c.is_alphanumeric() || c == '_' {
        CharClass::Word
    } else {
        CharClass::Punctuation
    }
}

fn move_word(rope: &Rope, offset: usize, direction: Direction) -> usize {
    let len = rope.len_chars();
    match direction {
        Direction::Forward => {
            let mut i = offset;
            if i >= len {
                return len;
            }
            // 直後の空白を読み飛ばし、その後に続く同種の文字列を抜ける。
            while i < len && classify(rope.char(i)) == CharClass::Whitespace {
                i += 1;
            }
            if i < len {
                let class = classify(rope.char(i));
                while i < len && classify(rope.char(i)) == class {
                    i += 1;
                }
            }
            i
        }
        Direction::Backward => {
            let mut i = offset;
            if i == 0 {
                return 0;
            }
            while i > 0 && classify(rope.char(i - 1)) == CharClass::Whitespace {
                i -= 1;
            }
            if i > 0 {
                let class = classify(rope.char(i - 1));
                while i > 0 && classify(rope.char(i - 1)) == class {
                    i -= 1;
                }
            }
            i
        }
    }
}

fn move_line_boundary(rope: &Rope, offset: usize, direction: Direction) -> usize {
    let row = rope.char_to_line(offset);
    match direction {
        Direction::Backward => {
            let line_start = rope.line_to_char(row);
            // 行頭のインデントを飛ばした位置を優先し、既にそこにいるなら本当の行頭へ。
            let line = rope.line(row);
            let indent = line
                .chars()
                .take_while(|c| *c == ' ' || *c == '\t')
                .count();
            let first_non_ws = line_start + indent;
            if offset > first_non_ws {
                first_non_ws
            } else {
                line_start
            }
        }
        Direction::Forward => rope.line_end_offset(row),
    }
}

fn move_vertical(
    rope: &Rope,
    offset: usize,
    direction: Direction,
    rows: u32,
    goal_column: u32,
) -> usize {
    let current = rope.char_to_line(offset);
    let last_line = rope.len_lines().saturating_sub(1);
    let target_row = match direction {
        Direction::Backward => current.saturating_sub(rows as usize),
        Direction::Forward => (current + rows as usize).min(last_line),
    };
    rope.position_to_offset(Position::new(target_row as u32, goal_column))
}

/// 複数カーソルの正規化。
///
/// 重なった選択を 1 本に併合し、開始位置順に並べる。マルチカーソル編集では、
/// 重なりを残したまま編集すると同じ箇所を二重に書き換えてしまうため必須。
pub fn normalize(selections: &mut Vec<Selection>) {
    if selections.len() <= 1 {
        return;
    }
    selections.sort_by_key(|s| (s.start(), s.end()));
    let mut merged: Vec<Selection> = Vec::with_capacity(selections.len());
    for sel in selections.iter().copied() {
        match merged.last_mut() {
            // 端が接するだけの隣接カーソルは別物として残し、真に重なる場合だけ併合する。
            Some(last) if sel.start() < last.end() || (sel.is_empty() && last.is_empty() && sel.start() == last.start()) =>
            {
                let start = last.start().min(sel.start());
                let end = last.end().max(sel.end());
                // head の向きは後から来た方に合わせる。ユーザーの最後の操作方向を保つため。
                let reversed = sel.head < sel.anchor;
                *last = if reversed {
                    Selection::new(end, start)
                } else {
                    Selection::new(start, end)
                };
            }
            _ => merged.push(sel),
        }
    }
    *selections = merged;
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rope(s: &str) -> Rope {
        Rope::from_str(s)
    }

    #[test]
    fn 書記素単位で右に動く() {
        // 家族絵文字は ZWJ で連結された 1 クラスタ。
        let r = rope("a👨‍👩‍👧b");
        let mut sel = Selection::caret(1);
        move_selection(&r, &mut sel, Movement::Grapheme, Direction::Forward, false);
        assert_eq!(r.slice(sel.head..).to_string(), "b");
    }

    #[test]
    fn 書記素単位で左に動く() {
        let r = rope("a👨‍👩‍👧b");
        let end = r.len_chars();
        let mut sel = Selection::caret(end - 1);
        move_selection(&r, &mut sel, Movement::Grapheme, Direction::Backward, false);
        assert_eq!(sel.head, 1);
    }

    #[test]
    fn 単語単位の移動が種別境界で止まる() {
        let r = rope("let foo_bar = 42;");
        let mut sel = Selection::caret(0);
        move_selection(&r, &mut sel, Movement::Word, Direction::Forward, false);
        assert_eq!(sel.head, 3, "let の直後で止まる");
        move_selection(&r, &mut sel, Movement::Word, Direction::Forward, false);
        assert_eq!(sel.head, 11, "空白を越えて foo_bar の直後");
    }

    #[test]
    fn 行頭移動はインデント位置を優先する() {
        let r = rope("    hello\n");
        let mut sel = Selection::caret(9);
        move_selection(&r, &mut sel, Movement::LineBoundary, Direction::Backward, false);
        assert_eq!(sel.head, 4, "インデントの直後");
        move_selection(&r, &mut sel, Movement::LineBoundary, Direction::Backward, false);
        assert_eq!(sel.head, 0, "2 回目で真の行頭");
    }

    #[test]
    fn 上下移動でゴール桁を保持する() {
        let r = rope("abcdefgh\nxy\nabcdefgh\n");
        let mut sel = Selection::caret(6); // 1 行目の桁 6
        move_selection(&r, &mut sel, Movement::Line, Direction::Forward, false);
        assert_eq!(r.offset_to_position(sel.head).column, 2, "短い行では行末に収まる");
        move_selection(&r, &mut sel, Movement::Line, Direction::Forward, false);
        assert_eq!(r.offset_to_position(sel.head).column, 6, "元の桁に復帰する");
    }

    #[test]
    fn 選択中に矢印を押すと端へ畳む() {
        let r = rope("hello world");
        let mut sel = Selection::new(2, 7);
        move_selection(&r, &mut sel, Movement::Grapheme, Direction::Backward, false);
        assert_eq!(sel, Selection::caret(2));
    }

    #[test]
    fn 重なる選択を併合する() {
        let mut sels = vec![
            Selection::new(0, 5),
            Selection::new(3, 8),
            Selection::new(20, 25),
        ];
        normalize(&mut sels);
        assert_eq!(sels, vec![Selection::new(0, 8), Selection::new(20, 25)]);
    }

    #[test]
    fn 隣接するだけの空カーソルは併合しない() {
        let mut sels = vec![Selection::caret(3), Selection::caret(5)];
        normalize(&mut sels);
        assert_eq!(sels.len(), 2);
    }

    #[test]
    fn 同一位置の空カーソルは併合する() {
        let mut sels = vec![Selection::caret(3), Selection::caret(3)];
        normalize(&mut sels);
        assert_eq!(sels.len(), 1);
    }

    #[test]
    fn 編集による位置追従() {
        // "abcdef" の 1..3 ("bc") を "XYZW" に置換 → 長さ +2
        let range = TextRange::new(1, 3);
        assert_eq!(map_offset(0, range, 4), 0, "手前は不変");
        assert_eq!(map_offset(2, range, 4), 5, "内側は範囲末尾へ");
        assert_eq!(map_offset(5, range, 4), 7, "後ろは差分だけずれる");
    }
}
