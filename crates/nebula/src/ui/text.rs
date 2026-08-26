//! テキストに対する純粋関数だけを集めたモジュール。
//!
//! gpui にも `Context` にも触れない (`&str`/`usize` だけを受け取る) 関数だけを
//! ここへ寄せてある。単体テストが状態構築なしでそのまま書けるようにするため。

use std::ops::Range;
use unicode_segmentation::UnicodeSegmentation;

/// テキストを 1 行に省略表示する。
pub fn truncate_middle(text: &str, max_chars: usize) -> String {
    let chars: Vec<char> = text.chars().collect();
    if chars.len() <= max_chars || max_chars < 4 {
        return text.to_string();
    }
    let head = (max_chars - 1) / 2;
    let tail = max_chars - 1 - head;
    let mut result: String = chars[..head].iter().collect();
    result.push('…');
    result.extend(&chars[chars.len() - tail..]);
    result
}

/// キーバインドを人が読める形にする。
///
/// 表記はプラットフォームの作法に合わせる。macOS は記号を詰めて並べ
/// (`secondary-shift-p` → `⌘⇧P`)、Windows/Linux は語を `+` でつなぐ
/// (`Ctrl+Shift+P`)。macOS の記号は他の OS では通じず、逆に `Ctrl+Shift+P`
/// という書き方は macOS では見慣れない。
pub fn format_keystroke(keystroke: &str) -> String {
    format_keystroke_as(keystroke, cfg!(target_os = "macos"))
}

/// `mac` が真なら macOS の記号表記、偽なら Windows/Linux の語表記。
///
/// 実際の OS ではなく引数で切り替えるのは、どちらの表記も全プラットフォームの
/// `cargo test` で検証できるようにするため。macOS で開発していると Windows の
/// 表記が一度も実行されないまま壊れる。
fn format_keystroke_as(keystroke: &str, mac: bool) -> String {
    // `secondary-k secondary-i` のような連続打鍵は空白区切り。打鍵ごとに
    // 組み立ててから同じ区切りでつなぐ。
    keystroke
        .split_whitespace()
        .map(|stroke| format_stroke(stroke, mac))
        .collect::<Vec<_>>()
        .join(" ")
}

fn format_stroke(stroke: &str, mac: bool) -> String {
    let segments: Vec<&str> = stroke.split('-').collect();
    let mut parts: Vec<String> = Vec::with_capacity(segments.len());
    for (i, segment) in segments.iter().enumerate() {
        let is_last = i + 1 == segments.len();
        let rendered = match *segment {
            // gpui は secondary を macOS では ⌘、それ以外では Ctrl に解決する。
            // 表示もそれに合わせないと、押せない打鍵を案内することになる。
            "secondary" if mac => "⌘".to_string(),
            "secondary" => "Ctrl".to_string(),
            "cmd" | "super" if mac => "⌘".to_string(),
            "cmd" | "super" => "Win".to_string(),
            "ctrl" if mac => "⌃".to_string(),
            "ctrl" => "Ctrl".to_string(),
            "alt" | "option" if mac => "⌥".to_string(),
            "alt" | "option" => "Alt".to_string(),
            "shift" if mac => "⇧".to_string(),
            "shift" => "Shift".to_string(),
            key if is_last => format_key(key, mac),
            other => other.to_string(),
        };
        parts.push(rendered);
    }
    // macOS は記号が並ぶので区切りが要らない。語で書く側は `+` でつなぐ。
    parts.join(if mac { "" } else { "+" })
}

fn format_key(key: &str, mac: bool) -> String {
    // 矢印は記号のままがどの OS でも読みやすいので分けない。
    let arrow = match key {
        "up" => Some("↑"),
        "down" => Some("↓"),
        "left" => Some("←"),
        "right" => Some("→"),
        _ => None,
    };
    if let Some(arrow) = arrow {
        return arrow.to_string();
    }
    let named = if mac {
        match key {
            "enter" => Some("⏎"),
            "escape" => Some("⎋"),
            "backspace" => Some("⌫"),
            "delete" => Some("⌦"),
            "tab" => Some("⇥"),
            _ => None,
        }
    } else {
        match key {
            "enter" => Some("Enter"),
            "escape" => Some("Esc"),
            "backspace" => Some("Backspace"),
            "delete" => Some("Delete"),
            "tab" => Some("Tab"),
            "home" => Some("Home"),
            "end" => Some("End"),
            "pageup" => Some("PageUp"),
            "pagedown" => Some("PageDown"),
            "space" => Some("Space"),
            _ => None,
        }
    };
    named.map_or_else(|| key.to_uppercase(), str::to_string)
}

// ---------------------------------------------------------------------------
// 位置の変換 (純粋関数)
// ---------------------------------------------------------------------------

/// UTF-8 バイト位置 → UTF-16 符号単位の位置。
pub(super) fn offset_to_utf16(text: &str, offset: usize) -> usize {
    let mut utf16 = 0;
    let mut utf8 = 0;
    for ch in text.chars() {
        if utf8 >= offset {
            break;
        }
        utf8 += ch.len_utf8();
        utf16 += ch.len_utf16();
    }
    utf16
}

/// UTF-16 符号単位の位置 → UTF-8 バイト位置。
pub(super) fn offset_from_utf16(text: &str, offset: usize) -> usize {
    let mut utf8 = 0;
    let mut utf16 = 0;
    for ch in text.chars() {
        if utf16 >= offset {
            break;
        }
        utf16 += ch.len_utf16();
        utf8 += ch.len_utf8();
    }
    utf8
}

pub(super) fn range_to_utf16(text: &str, range: &Range<usize>) -> Range<usize> {
    offset_to_utf16(text, range.start)..offset_to_utf16(text, range.end)
}

pub(super) fn range_from_utf16(text: &str, range: &Range<usize>) -> Range<usize> {
    offset_from_utf16(text, range.start)..offset_from_utf16(text, range.end)
}

/// 1 つ手前の書記素境界。合成文字や絵文字の途中で切らないため char ではなく
/// 書記素で刻む。
pub(super) fn previous_grapheme(text: &str, offset: usize) -> usize {
    text.grapheme_indices(true)
        .rev()
        .find_map(|(index, _)| (index < offset).then_some(index))
        .unwrap_or(0)
}

/// 1 つ先の書記素境界。
pub(super) fn next_grapheme(text: &str, offset: usize) -> usize {
    text.grapheme_indices(true)
        .find_map(|(index, _)| (index > offset).then_some(index))
        .unwrap_or(text.len())
}

/// 単語移動のための文字の種類。
///
/// 「空白」「語」「記号」の 3 種に分けるのは、`foo.bar` の `.` で止まってほしい
/// 一方、`.....` の途中では止まってほしくないため。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CharClass {
    Space,
    Word,
    Symbol,
}

fn char_class(ch: char) -> CharClass {
    if ch.is_whitespace() {
        CharClass::Space
    } else if ch.is_alphanumeric() || ch == '_' {
        CharClass::Word
    } else {
        CharClass::Symbol
    }
}

/// 単語 1 つぶん手前の位置。空白を飛ばしてから、同じ種類が続くあいだ戻る。
pub(super) fn previous_word_boundary(text: &str, offset: usize) -> usize {
    let offset = offset.min(text.len());
    let head = &text[..offset];
    let mut chars = head.char_indices().rev().peekable();
    let mut result = offset;
    while let Some(&(index, ch)) = chars.peek() {
        if char_class(ch) != CharClass::Space {
            break;
        }
        result = index;
        chars.next();
    }
    let Some(&(_, first)) = chars.peek() else {
        return result;
    };
    let class = char_class(first);
    while let Some(&(index, ch)) = chars.peek() {
        if char_class(ch) != class {
            break;
        }
        result = index;
        chars.next();
    }
    result
}

/// 単語 1 つぶん先の位置。同じ種類が続くあいだ進み、そのあとの空白も飛ばす。
pub(super) fn next_word_boundary(text: &str, offset: usize) -> usize {
    let offset = offset.min(text.len());
    let mut chars = text[offset..].char_indices().peekable();
    let mut result = offset;
    if let Some(&(_, first)) = chars.peek()
        && char_class(first) != CharClass::Space
    {
        let class = char_class(first);
        while let Some(&(index, ch)) = chars.peek() {
            if char_class(ch) != class {
                break;
            }
            result = offset + index + ch.len_utf8();
            chars.next();
        }
    }
    while let Some(&(index, ch)) = chars.peek() {
        if char_class(ch) != CharClass::Space {
            break;
        }
        result = offset + index + ch.len_utf8();
        chars.next();
    }
    result
}

/// 位置が属する行の範囲 (改行を含まない)。
pub(super) fn line_bounds(text: &str, offset: usize) -> Range<usize> {
    let offset = offset.min(text.len());
    let start = text[..offset].rfind('\n').map(|i| i + 1).unwrap_or(0);
    let end = text[offset..]
        .find('\n')
        .map(|i| offset + i)
        .unwrap_or(text.len());
    start..end
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 中略は長さを守る() {
        let result = truncate_middle("abcdefghijklmnop", 9);
        assert_eq!(result.chars().count(), 9);
        assert!(result.contains('…'));
        assert!(result.starts_with("abcd"));
        assert!(result.ends_with("mnop"));
    }

    #[test]
    fn 短い文字列はそのまま() {
        assert_eq!(truncate_middle("short", 20), "short");
    }

    #[test]
    fn macos_のキーバインド表記は記号を詰めて並べる() {
        assert_eq!(format_keystroke_as("secondary-shift-p", true), "⌘⇧P");
        assert_eq!(format_keystroke_as("ctrl-`", true), "⌃`");
        assert_eq!(format_keystroke_as("secondary-enter", true), "⌘⏎");
        assert_eq!(format_keystroke_as("alt-shift-left", true), "⌥⇧←");
    }

    /// Windows/Linux では ⌘ も ⌥ も通じない。語を `+` でつないで書く。
    #[test]
    fn windows_のキーバインド表記は語を繋げて書く() {
        assert_eq!(
            format_keystroke_as("secondary-shift-p", false),
            "Ctrl+Shift+P"
        );
        assert_eq!(format_keystroke_as("ctrl-`", false), "Ctrl+`");
        assert_eq!(format_keystroke_as("secondary-enter", false), "Ctrl+Enter");
        assert_eq!(format_keystroke_as("alt-shift-left", false), "Alt+Shift+←");
        assert_eq!(format_keystroke_as("escape", false), "Esc");
    }

    /// `secondary` は macOS で ⌘、それ以外で Ctrl。表示がここを取り違えると、
    /// 実際には押せない打鍵を案内することになる。
    #[test]
    fn secondary_はプラットフォームごとの修飾キーとして表示される() {
        assert_eq!(format_keystroke_as("secondary-o", true), "⌘O");
        assert_eq!(format_keystroke_as("secondary-o", false), "Ctrl+O");
    }

    /// `secondary-k secondary-i` のような連続打鍵は空白区切りのまま扱う。
    #[test]
    fn 連続打鍵は空白で区切ったまま表示する() {
        assert_eq!(
            format_keystroke_as("secondary-k secondary-i", true),
            "⌘K ⌘I"
        );
        assert_eq!(
            format_keystroke_as("secondary-k secondary-i", false),
            "Ctrl+K Ctrl+I"
        );
    }

    // -----------------------------------------------------------------
    // 入力欄: UTF-16 との相互変換
    // -----------------------------------------------------------------

    #[test]
    fn ascii_では_utf8_と_utf16_の位置が一致する() {
        assert_eq!(offset_to_utf16("hello", 3), 3);
        assert_eq!(offset_from_utf16("hello", 3), 3);
    }

    #[test]
    fn 日本語は_1_文字が_3_バイト_1_符号単位() {
        let text = "あいう";
        assert_eq!(offset_to_utf16(text, 3), 1);
        assert_eq!(offset_to_utf16(text, 9), 3);
        assert_eq!(offset_from_utf16(text, 1), 3);
        assert_eq!(offset_from_utf16(text, 3), 9);
    }

    #[test]
    fn 代理対の絵文字は_4_バイト_2_符号単位() {
        let text = "日本語🎌";
        // 絵文字の手前まで。
        assert_eq!(offset_to_utf16(text, 9), 3);
        // 絵文字を含めて。
        assert_eq!(offset_to_utf16(text, 13), 5);
        assert_eq!(offset_from_utf16(text, 5), 13);
    }

    #[test]
    fn 位置の往復で元に戻る() {
        let text = "a あ b 🎌 c";
        for (index, _) in text.char_indices() {
            let utf16 = offset_to_utf16(text, index);
            assert_eq!(offset_from_utf16(text, utf16), index, "位置 {index}");
        }
    }

    // -----------------------------------------------------------------
    // 入力欄: 書記素と単語の境界
    // -----------------------------------------------------------------

    #[test]
    fn 書記素境界は多バイト文字を割らない() {
        let text = "あい";
        assert_eq!(previous_grapheme(text, 6), 3);
        assert_eq!(previous_grapheme(text, 3), 0);
        assert_eq!(previous_grapheme(text, 0), 0);
        assert_eq!(next_grapheme(text, 0), 3);
        assert_eq!(next_grapheme(text, 3), 6);
        assert_eq!(next_grapheme(text, 6), 6);
    }

    #[test]
    fn 単語移動は語の頭で止まる() {
        let text = "foo bar baz";
        assert_eq!(previous_word_boundary(text, 11), 8);
        assert_eq!(previous_word_boundary(text, 8), 4);
        assert_eq!(previous_word_boundary(text, 4), 0);
        assert_eq!(previous_word_boundary(text, 0), 0);
    }

    #[test]
    fn 単語移動は前へ進むと語の末尾と続く空白を越える() {
        let text = "foo bar baz";
        assert_eq!(next_word_boundary(text, 0), 4);
        assert_eq!(next_word_boundary(text, 4), 8);
        assert_eq!(next_word_boundary(text, 8), 11);
        assert_eq!(next_word_boundary(text, 11), 11);
    }

    #[test]
    fn 記号は語とは別の塊として扱う() {
        let text = "foo.bar";
        // 末尾から戻ると bar・記号・foo の順で刻む。
        assert_eq!(previous_word_boundary(text, 7), 4);
        assert_eq!(previous_word_boundary(text, 4), 3);
        assert_eq!(previous_word_boundary(text, 3), 0);
        assert_eq!(next_word_boundary(text, 0), 3);
        assert_eq!(next_word_boundary(text, 3), 4);
    }

    #[test]
    fn 連続した記号はひとまとまりで越える() {
        let text = "a==b";
        assert_eq!(next_word_boundary(text, 1), 3);
        assert_eq!(previous_word_boundary(text, 3), 1);
    }

    #[test]
    fn 単語移動は多バイト文字の境界で止まる() {
        let text = "あいう えお";
        assert_eq!(next_word_boundary(text, 0), 10);
        assert_eq!(previous_word_boundary(text, text.len()), 10);
    }

    // -----------------------------------------------------------------
    // 入力欄: 行の範囲
    // -----------------------------------------------------------------

    #[test]
    fn 行の範囲は前後の改行の内側() {
        let text = "ab\ncde\nf";
        assert_eq!(line_bounds(text, 0), 0..2);
        assert_eq!(line_bounds(text, 2), 0..2);
        assert_eq!(line_bounds(text, 3), 3..6);
        assert_eq!(line_bounds(text, 5), 3..6);
        assert_eq!(line_bounds(text, 7), 7..8);
    }
}
