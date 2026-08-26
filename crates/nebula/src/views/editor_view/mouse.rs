//! マウス操作 (クリック・ドラッグ選択・ホイールスクロール) のハンドラ。
//!
//! キー入力由来のアクションハンドラ (ルート側) と違い、生イベントを直接受け取る一群なので分けた。

use super::{EditorView, completion::is_word_char};
use gpui::{
    Context, MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent, Pixels, Point,
    ScrollWheelEvent, Window, px,
};
use nebula_core::Selection;
use nebula_core::selection::normalize;

impl EditorView {
    // -- マウス --

    pub(super) fn on_mouse_down(
        &mut self,
        event: &MouseDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if event.button != MouseButton::Left {
            return;
        }
        window.focus(&self.focus_handle);
        let Some(offset) = self.offset_for_position(event.position) else {
            return;
        };
        self.commit_undo_group();
        self.is_selecting = true;
        if event.modifiers.alt {
            // Option + クリックでカーソルを追加する。
            self.selections.push(Selection::caret(offset));
            normalize(&mut self.selections);
        } else if event.modifiers.shift {
            if let Some(last) = self.selections.last_mut() {
                last.head = offset;
            }
        } else if event.click_count >= 2 {
            self.selections = vec![self.word_at(offset)];
            self.is_selecting = false;
        } else {
            self.selections = vec![Selection::caret(offset)];
        }
        cx.notify();
    }

    pub(super) fn on_mouse_move(
        &mut self,
        event: &MouseMoveEvent,
        _w: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.is_selecting {
            return;
        }
        if let Some(offset) = self.offset_for_position(event.position)
            && let Some(last) = self.selections.last_mut()
        {
            last.head = offset;
            cx.notify();
        }
    }

    pub(super) fn on_mouse_up(
        &mut self,
        _: &MouseUpEvent,
        _w: &mut Window,
        _cx: &mut Context<Self>,
    ) {
        self.is_selecting = false;
    }

    pub(super) fn on_scroll_wheel(
        &mut self,
        event: &ScrollWheelEvent,
        _w: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(layout) = self.last_layout.as_ref() else {
            return;
        };
        let delta = event.delta.pixel_delta(layout.line_height);
        let lines = f32::from(delta.y) / f32::from(layout.line_height);
        let max = (self.buffer.len_lines() as f32 - 1.0).max(0.0);
        self.scroll_top = (self.scroll_top - lines).clamp(0.0, max);
        self.scroll_left = (self.scroll_left - delta.x).max(px(0.));
        cx.notify();
    }

    /// 画面座標を文字オフセットに変換する。
    pub(super) fn offset_for_position(&self, position: Point<Pixels>) -> Option<usize> {
        let layout = self.last_layout.as_ref()?;
        let relative_y = position.y - layout.text_origin.y;
        let row_offset = (f32::from(relative_y) / f32::from(layout.line_height)).floor();
        let row = (self.scroll_top + row_offset).max(0.0) as usize;
        let row = row.min(self.buffer.len_lines().saturating_sub(1));

        let rope = self.buffer.rope();
        let line_start = rope.line_to_char(row);
        let visible_index = row.checked_sub(layout.first_row)?;
        let Some(shaped) = layout.lines.get(visible_index) else {
            return Some(line_start);
        };
        let x = position.x - layout.text_origin.x + self.scroll_left;
        // x_for_index/index_for_x はバイト索引で動く。行内バイト → 行内文字に直す。
        let byte_in_line = shaped.index_for_x(x).unwrap_or_else(|| shaped.text.len());
        let char_in_line = shaped.text[..byte_in_line.min(shaped.text.len())]
            .chars()
            .count();
        Some(line_start + char_in_line)
    }

    fn word_at(&self, offset: usize) -> Selection {
        let rope = self.buffer.rope();
        let len = rope.len_chars();
        let mut start = offset.min(len);
        let mut end = start;
        while start > 0 && is_word_char(rope.char(start - 1)) {
            start -= 1;
        }
        while end < len && is_word_char(rope.char(end)) {
            end += 1;
        }
        Selection::new(start, end)
    }
}
