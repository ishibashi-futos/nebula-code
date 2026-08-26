//! IME 対応: `EntityInputHandler` の実装と UTF-16 ⇔ 文字オフセットの変換。
//!
//! macOS 等は UTF-16 コードユニットで範囲を指定してくるが、内部は文字単位なので変換をここに閉じ込める。

use super::{EditorView, completion::is_word_char};
use gpui::{Bounds, Context, EntityInputHandler, Pixels, Point, UTF16Selection, Window, px};
use nebula_core::{RopeExt, Selection};
use nebula_protocol::{Edit, TextRange};
use std::ops::Range;

impl EntityInputHandler for EditorView {
    fn text_for_range(
        &mut self,
        range_utf16: Range<usize>,
        adjusted: &mut Option<Range<usize>>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<String> {
        let text = self.buffer.text();
        let range = utf16_range_to_char(&text, range_utf16);
        *adjusted = Some(char_range_to_utf16(&text, range.clone()));
        Some(self.buffer.rope().slice(range.start..range.end).to_string())
    }

    fn selected_text_range(
        &mut self,
        _ignore_disabled: bool,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<UTF16Selection> {
        let sel = *self.selections.first()?;
        let text = self.buffer.text();
        Some(UTF16Selection {
            range: char_range_to_utf16(&text, sel.start()..sel.end()),
            reversed: sel.head < sel.anchor,
        })
    }

    fn marked_text_range(
        &self,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<Range<usize>> {
        let marked = self.marked_range.clone()?;
        let text = self.buffer.text();
        Some(char_range_to_utf16(&text, marked))
    }

    fn unmark_text(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        self.marked_range = None;
        cx.notify();
    }

    fn replace_text_in_range(
        &mut self,
        range_utf16: Option<Range<usize>>,
        new_text: &str,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let text = self.buffer.text();
        let target = range_utf16
            .map(|r| utf16_range_to_char(&text, r))
            .or_else(|| self.marked_range.clone());

        let before = self.selections.clone();
        let edits = match target {
            // IME 確定や範囲指定の置換は、その範囲だけを差し替える。
            Some(range) => vec![Edit::replace(
                TextRange::new(range.start, range.end),
                new_text,
            )],
            None => before
                .iter()
                .map(|s| Edit::replace(s.range(), new_text))
                .collect(),
        };
        self.marked_range = None;
        self.hover = None;
        self.apply(edits, before, cx);
        self.scroll_to_cursor();

        // 識別子を打っている間は候補を出し直す。`.` や `:` は言語サーバー側の
        // 誘発文字として扱われることが多いので、明示的に渡す。
        let last = new_text.chars().last();
        match last {
            Some(c) if is_word_char(c) => {
                if self.completion.is_some() {
                    self.refilter_completion();
                } else if new_text.chars().count() == 1 {
                    self.request_completion(None, cx);
                }
            }
            Some(c @ ('.' | ':' | '>')) => {
                self.request_completion(Some(c.to_string()), cx);
            }
            _ => self.completion = None,
        }
    }

    fn replace_and_mark_text_in_range(
        &mut self,
        range_utf16: Option<Range<usize>>,
        new_text: &str,
        new_selected_range: Option<Range<usize>>,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let text = self.buffer.text();
        let target = range_utf16
            .map(|r| utf16_range_to_char(&text, r))
            .or_else(|| self.marked_range.clone())
            .unwrap_or_else(|| {
                let sel = self
                    .selections
                    .first()
                    .copied()
                    .unwrap_or(Selection::caret(0));
                sel.start()..sel.end()
            });

        let before = self.selections.clone();
        let edits = vec![Edit::replace(
            TextRange::new(target.start, target.end),
            new_text,
        )];
        self.apply(edits, before, cx);

        // 未確定範囲を覚えておく。描画側が下線を引く。
        let inserted_len = new_text.chars().count();
        self.marked_range = (inserted_len > 0).then(|| target.start..target.start + inserted_len);

        if let Some(selected) = new_selected_range {
            let marked_text = new_text;
            let start = target.start + utf16_to_char_offset(marked_text, selected.start);
            let end = target.start + utf16_to_char_offset(marked_text, selected.end);
            self.selections = vec![Selection::new(start, end)];
        }
        cx.notify();
    }

    fn bounds_for_range(
        &mut self,
        range_utf16: Range<usize>,
        element_bounds: Bounds<Pixels>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<Bounds<Pixels>> {
        // 変換候補ウィンドウの表示位置。範囲の先頭の座標を返す。
        let layout = self.last_layout.as_ref()?;
        let text = self.buffer.text();
        let range = utf16_range_to_char(&text, range_utf16);
        let position = self.buffer.rope().offset_to_position(range.start);
        let visible_index = (position.row as usize).checked_sub(layout.first_row)?;
        let shaped = layout.lines.get(visible_index)?;
        let byte_in_line = shaped
            .text
            .char_indices()
            .nth(position.column as usize)
            .map(|(i, _)| i)
            .unwrap_or(shaped.text.len());
        let x = layout.text_origin.x + shaped.x_for_index(byte_in_line) - self.scroll_left;
        let y = layout.text_origin.y + layout.line_height * (position.row as f32 - self.scroll_top);
        Some(Bounds::new(
            gpui::point(x, y),
            gpui::size(px(2.), layout.line_height),
        ))
        .filter(|b| element_bounds.contains(&b.origin))
    }

    fn character_index_for_point(
        &mut self,
        point: Point<Pixels>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<usize> {
        let offset = self.offset_for_position(point)?;
        let text = self.buffer.text();
        Some(char_range_to_utf16(&text, offset..offset).start)
    }
}

// -- UTF-16 との相互変換 --
//
// macOS の入力メソッドは UTF-16 コードユニットで位置を指定してくる。
// Nebula 内部は文字 (char) 単位なので、境界での取り違えを防ぐため
// 変換をこの 3 関数に閉じ込める。

fn char_range_to_utf16(text: &str, range: Range<usize>) -> Range<usize> {
    let mut utf16_index = 0usize;
    let mut start = None;
    let mut end = None;
    for (char_index, c) in text.chars().enumerate() {
        if char_index == range.start {
            start = Some(utf16_index);
        }
        if char_index == range.end {
            end = Some(utf16_index);
        }
        utf16_index += c.len_utf16();
    }
    Range {
        start: start.unwrap_or(utf16_index),
        end: end.unwrap_or(utf16_index),
    }
}

fn utf16_range_to_char(text: &str, range: Range<usize>) -> Range<usize> {
    Range {
        start: utf16_to_char_offset(text, range.start),
        end: utf16_to_char_offset(text, range.end),
    }
}

fn utf16_to_char_offset(text: &str, utf16_offset: usize) -> usize {
    let mut char_index = 0usize;
    let mut utf16_index = 0usize;
    for c in text.chars() {
        if utf16_index >= utf16_offset {
            return char_index;
        }
        utf16_index += c.len_utf16();
        char_index += 1;
    }
    char_index
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn utf16_と文字オフセットが往復する() {
        // "a" (1) + "あ" (1) + "𝄞" (サロゲートペア = 2) + "b" (1)
        let text = "aあ𝄞b";
        assert_eq!(char_range_to_utf16(text, 0..4), 0..5);
        assert_eq!(
            char_range_to_utf16(text, 2..3),
            2..4,
            "サロゲートペア 1 文字"
        );
        assert_eq!(utf16_range_to_char(text, 0..5), 0..4);
        assert_eq!(utf16_range_to_char(text, 2..4), 2..3);
    }

    #[test]
    fn utf16_オフセットが範囲外でも末尾に丸まる() {
        assert_eq!(utf16_to_char_offset("abc", 99), 3);
    }
}
