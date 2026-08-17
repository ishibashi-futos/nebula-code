//! LSP の UTF-16 位置と Nebula の char 位置の相互変換。
//!
//! LSP の `character` は **UTF-16 コードユニット** 単位、Nebula の [`Position::column`] は
//! **char** 単位。ASCII しかない行では一致するが、日本語 (BMP 内なので UTF-16 では 1) と
//! 絵文字 (サロゲートペアなので UTF-16 では 2) が混ざると食い違う。
//!
//! ここを間違えるとホバーも補完も定義ジャンプもまとめて数文字ぶんずれる。
//! そのため変換をこのモジュールだけに閉じ込め、純粋関数としてテストで固定している。

use lsp_types as lsp;
use nebula_protocol::{Position, SpanRange};

/// 指定行の本文を返す。改行文字は含まない。範囲外の行は空文字列。
///
/// `\r\n` の `\r` を落とすのは、LSP の桁が「改行を除いた行の内容」に対する
/// オフセットとして定義されているため。
pub fn line_at(text: &str, row: u32) -> &str {
    text.split('\n')
        .nth(row as usize)
        .map(|line| line.strip_suffix('\r').unwrap_or(line))
        .unwrap_or("")
}

/// 行内の char オフセットを UTF-16 コードユニットオフセットへ変換する。
///
/// 行末を越える値は行末に丸める。LSP は行末超過の桁を許容するので、
/// こちら側でも panic せず飽和させる。
pub fn char_to_utf16(line: &str, char_column: u32) -> u32 {
    line.chars()
        .take(char_column as usize)
        .map(|c| c.len_utf16() as u32)
        .sum()
}

/// 行内の UTF-16 コードユニットオフセットを char オフセットへ変換する。
///
/// サロゲートペアの途中を指す値 (絵文字の 1 コードユニット目と 2 つ目の間) は、
/// その文字を含む位置まで進める。中途半端な位置は Nebula 側で表現できないため。
pub fn utf16_to_char(line: &str, utf16_column: u32) -> u32 {
    let mut utf16 = 0u32;
    let mut chars = 0u32;
    for c in line.chars() {
        if utf16 >= utf16_column {
            break;
        }
        utf16 += c.len_utf16() as u32;
        chars += 1;
    }
    chars
}

pub fn to_lsp_position(text: &str, position: Position) -> lsp::Position {
    lsp::Position {
        line: position.row,
        character: char_to_utf16(line_at(text, position.row), position.column),
    }
}

pub fn from_lsp_position(text: &str, position: lsp::Position) -> Position {
    Position {
        row: position.line,
        column: utf16_to_char(line_at(text, position.line), position.character),
    }
}

pub fn to_lsp_range(text: &str, range: SpanRange) -> lsp::Range {
    lsp::Range {
        start: to_lsp_position(text, range.start),
        end: to_lsp_position(text, range.end),
    }
}

pub fn from_lsp_range(text: &str, range: lsp::Range) -> SpanRange {
    SpanRange {
        start: from_lsp_position(text, range.start),
        end: from_lsp_position(text, range.end),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 「あ」は UTF-16 で 1 コードユニット、「🚀」は 2 コードユニット。
    const MIXED: &str = "let 名前 = \"🚀ロケット\";";

    #[test]
    fn ascii_のみの行は変換しても変わらない() {
        let line = "fn main() {}";
        assert_eq!(char_to_utf16(line, 3), 3);
        assert_eq!(utf16_to_char(line, 3), 3);
    }

    #[test]
    fn 日本語は_utf16_でも_1_コードユニット() {
        // "let 名前" の直後 = char で 6 文字目。日本語は BMP 内なので UTF-16 でも 6。
        assert_eq!(char_to_utf16(MIXED, 6), 6);
        assert_eq!(utf16_to_char(MIXED, 6), 6);
    }

    #[test]
    fn 絵文字はサロゲートペアで_2_コードユニット分ずれる() {
        // MIXED の char 列: l e t ␣ 名 前 ␣ = ␣ " 🚀 ロ ...
        // 「🚀」の直後は char で 11、UTF-16 では 12。
        assert_eq!(char_to_utf16(MIXED, 11), 12);
        assert_eq!(utf16_to_char(MIXED, 12), 11);
    }

    #[test]
    fn 絵文字を含む行の末尾で桁が一致する() {
        let chars = MIXED.chars().count() as u32;
        let utf16 = MIXED.encode_utf16().count() as u32;
        assert_eq!(char_to_utf16(MIXED, chars), utf16);
        assert_eq!(utf16_to_char(MIXED, utf16), chars);
    }

    #[test]
    fn サロゲートペアの途中はその文字を含む位置へ丸める() {
        // 「🚀」の 1 コードユニット目と 2 つ目の間 (UTF-16 で 11)。
        assert_eq!(utf16_to_char(MIXED, 11), 11);
    }

    #[test]
    fn 行末を越える桁は行末に丸められる() {
        let line = "ab";
        assert_eq!(char_to_utf16(line, 99), 2);
        assert_eq!(utf16_to_char(line, 99), 2);
    }

    #[test]
    fn 行の切り出しは_crlf_の_cr_を落とす() {
        let text = "one\r\ntwo\r\n";
        assert_eq!(line_at(text, 0), "one");
        assert_eq!(line_at(text, 1), "two");
        // 末尾の改行の後ろは空行として存在する。
        assert_eq!(line_at(text, 2), "");
        // 範囲外の行も空文字列 (panic しない)。
        assert_eq!(line_at(text, 99), "");
    }

    #[test]
    fn 位置の往復変換が元に戻る() {
        let text = format!("first\n{MIXED}\nlast");
        for column in 0..MIXED.chars().count() as u32 {
            let position = Position::new(1, column);
            let round = from_lsp_position(&text, to_lsp_position(&text, position));
            assert_eq!(round, position, "column {column} で往復しない");
        }
    }

    #[test]
    fn 範囲の変換は開始と終了の両方を変換する() {
        let text = format!("{MIXED}\n{MIXED}");
        let range = SpanRange::new(Position::new(0, 11), Position::new(1, 4));
        let lsp_range = to_lsp_range(&text, range);
        assert_eq!(lsp_range.start.character, 12);
        assert_eq!(lsp_range.end.character, 4);
        assert_eq!(from_lsp_range(&text, lsp_range), range);
    }
}
