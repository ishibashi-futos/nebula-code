//! `Rope` に対する Nebula 固有の補助操作。
//!
//! ropey の `line()` は行末の改行を含むため、行末位置や行の表示長を求めるたびに
//! 改行の有無を判定する定型処理が必要になる。ここに一度だけ書いて共有する。

use nebula_protocol::{LineEnding, Position};
use ropey::{Rope, RopeSlice};

pub trait RopeExt {
    /// 文字オフセットを行桁に変換する。
    fn offset_to_position(&self, offset: usize) -> Position;
    /// 行桁を文字オフセットに変換する。範囲外の行桁は最も近い有効位置に丸める。
    fn position_to_offset(&self, position: Position) -> usize;
    /// 行の内容の長さ (改行を含まない文字数)。
    fn line_len(&self, row: usize) -> usize;
    /// 行末 (改行の直前) の文字オフセット。
    fn line_end_offset(&self, row: usize) -> usize;
    /// 改行を含まない行の内容。
    fn line_text(&self, row: usize) -> RopeSlice<'_>;
    /// 文書全体の改行コードを推定する。最初に現れた改行で決める。
    fn detect_line_ending(&self) -> LineEnding;
}

impl RopeExt for Rope {
    fn offset_to_position(&self, offset: usize) -> Position {
        let offset = offset.min(self.len_chars());
        let row = self.char_to_line(offset);
        let column = offset - self.line_to_char(row);
        Position::new(row as u32, column as u32)
    }

    fn position_to_offset(&self, position: Position) -> usize {
        let last_row = self.len_lines().saturating_sub(1);
        let row = (position.row as usize).min(last_row);
        let line_start = self.line_to_char(row);
        let column = (position.column as usize).min(self.line_len(row));
        line_start + column
    }

    fn line_len(&self, row: usize) -> usize {
        if row >= self.len_lines() {
            return 0;
        }
        let line = self.line(row);
        let total = line.len_chars();
        // 行末の改行 (LF または CRLF) を除いた長さを返す。
        if total > 0 && line.char(total - 1) == '\n' {
            if total > 1 && line.char(total - 2) == '\r' {
                total - 2
            } else {
                total - 1
            }
        } else {
            total
        }
    }

    fn line_end_offset(&self, row: usize) -> usize {
        if row >= self.len_lines() {
            return self.len_chars();
        }
        self.line_to_char(row) + self.line_len(row)
    }

    fn line_text(&self, row: usize) -> RopeSlice<'_> {
        if row >= self.len_lines() {
            return self.slice(self.len_chars()..self.len_chars());
        }
        let start = self.line_to_char(row);
        self.slice(start..start + self.line_len(row))
    }

    fn detect_line_ending(&self) -> LineEnding {
        // 先頭 64KB だけ見れば十分。巨大ファイル全体の走査は起動時間に効く。
        let probe_end = self.len_chars().min(65_536);
        let probe = self.slice(..probe_end);
        for (i, c) in probe.chars().enumerate() {
            if c == '\n' {
                let prev_is_cr = i > 0 && probe.char(i - 1) == '\r';
                return if prev_is_cr {
                    LineEnding::Crlf
                } else {
                    LineEnding::Lf
                };
            }
        }
        LineEnding::Lf
    }
}

/// 括弧の対応表。
const BRACKET_PAIRS: &[(char, char)] = &[('(', ')'), ('[', ']'), ('{', '}')];

/// `offset` の位置にある括弧に対応する括弧の位置を返す。
///
/// 構文木ではなく文字の対応だけで数える。木を使うと入力途中の不完全な状態で
/// 対応が取れなくなり、打っている最中に印が消えたり付いたりして落ち着かない。
pub fn matching_bracket(rope: &Rope, offset: usize) -> Option<usize> {
    let len = rope.len_chars();
    if offset >= len {
        return None;
    }
    let ch = rope.char(offset);
    let (open, close, forward) = match BRACKET_PAIRS.iter().find(|(o, _)| *o == ch) {
        Some((o, c)) => (*o, *c, true),
        None => {
            let (o, c) = BRACKET_PAIRS.iter().find(|(_, c)| *c == ch)?;
            (*o, *c, false)
        }
    };

    let mut depth = 0i32;
    let mut i = offset;
    loop {
        let c = rope.char(i);
        if c == open {
            depth += if forward { 1 } else { -1 };
        } else if c == close {
            depth += if forward { -1 } else { 1 };
        }
        if depth == 0 {
            return Some(i);
        }
        if forward {
            i += 1;
            if i >= len {
                return None;
            }
        } else {
            if i == 0 {
                return None;
            }
            i -= 1;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 行桁とオフセットが往復する() {
        let r = Rope::from_str("abc\nde\n\nfghi");
        for offset in 0..=r.len_chars() {
            let pos = r.offset_to_position(offset);
            assert_eq!(r.position_to_offset(pos), offset, "offset {offset}");
        }
    }

    #[test]
    fn 行末位置は改行を含まない() {
        let r = Rope::from_str("abc\nde\n");
        assert_eq!(r.line_end_offset(0), 3);
        assert_eq!(r.line_end_offset(1), 6);
        assert_eq!(r.line_len(0), 3);
    }

    #[test]
    fn crlf_の行長は二文字分を除く() {
        let r = Rope::from_str("abc\r\ndef");
        assert_eq!(r.line_len(0), 3);
        assert_eq!(r.line_end_offset(0), 3);
        assert_eq!(r.detect_line_ending(), LineEnding::Crlf);
    }

    #[test]
    fn 改行のない文書は_lf_扱い() {
        let r = Rope::from_str("abc");
        assert_eq!(r.detect_line_ending(), LineEnding::Lf);
        assert_eq!(r.line_len(0), 3);
    }

    #[test]
    fn 対応する括弧を前後どちらからも辿れる() {
        let r = Rope::from_str("fn f() { g((1 + 2)); }");
        let open = r.to_string().find('{').unwrap();
        let close = r.to_string().rfind('}').unwrap();
        assert_eq!(matching_bracket(&r, open), Some(close));
        assert_eq!(matching_bracket(&r, close), Some(open));
    }

    #[test]
    fn 入れ子の括弧を正しく数える() {
        let r = Rope::from_str("((a))");
        assert_eq!(matching_bracket(&r, 0), Some(4));
        assert_eq!(matching_bracket(&r, 1), Some(3));
    }

    #[test]
    fn 対応が無ければ_none() {
        let r = Rope::from_str("(((");
        assert_eq!(matching_bracket(&r, 0), None);
        let r = Rope::from_str("abc");
        assert_eq!(matching_bracket(&r, 1), None, "括弧でない位置");
        assert_eq!(matching_bracket(&r, 99), None, "範囲外");
    }

    #[test]
    fn マルチバイト文字を挟んでも数えられる() {
        let r = Rope::from_str("(あいう)");
        assert_eq!(matching_bracket(&r, 0), Some(4));
    }

    #[test]
    fn 範囲外の行桁は丸められる() {
        let r = Rope::from_str("abc\nde");
        assert_eq!(r.position_to_offset(Position::new(99, 99)), 6);
        assert_eq!(r.position_to_offset(Position::new(0, 99)), 3);
    }
}
