//! `Render` の実装。
//!
//! キー割り当て (`on_action`) の列挙とマウスイベントの配線だけの、ロジックを持たない層。
//! アクションハンドラ本体 (ルート側) と分けることで、「どのキーが何に繋がっているか」を
//! 一望できるようにした。

use super::EditorView;
use crate::theme::metrics;
use crate::views::editor_element::EditorElement;
use gpui::prelude::*;
use gpui::{Context, MouseButton, Window, div};

impl Render for EditorView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // ポップアップは cx を借りるので、要素を組み立てる前に作っておく。
        let completion = self.render_completion_popup(cx);
        let hover = self.render_hover_card(cx);
        div()
            .key_context("Editor")
            .track_focus(&self.focus_handle)
            .size_full()
            .relative()
            .overflow_hidden()
            // Workspace ルートの "SF Pro Text" (プロポーショナル体) をそのまま継承すると
            // コードが可変幅で描かれてしまうため、ここで等幅書体を明示する。
            .font_family(metrics::MONO_FONT_FAMILY)
            .on_action(cx.listener(Self::on_move_left))
            .on_action(cx.listener(Self::on_move_right))
            .on_action(cx.listener(Self::on_move_up))
            .on_action(cx.listener(Self::on_move_down))
            .on_action(cx.listener(Self::on_move_word_left))
            .on_action(cx.listener(Self::on_move_word_right))
            .on_action(cx.listener(Self::on_move_line_start))
            .on_action(cx.listener(Self::on_move_line_end))
            .on_action(cx.listener(Self::on_move_doc_start))
            .on_action(cx.listener(Self::on_move_doc_end))
            .on_action(cx.listener(Self::on_page_up))
            .on_action(cx.listener(Self::on_page_down))
            .on_action(cx.listener(Self::on_select_left))
            .on_action(cx.listener(Self::on_select_right))
            .on_action(cx.listener(Self::on_select_up))
            .on_action(cx.listener(Self::on_select_down))
            .on_action(cx.listener(Self::on_select_word_left))
            .on_action(cx.listener(Self::on_select_word_right))
            .on_action(cx.listener(Self::on_select_line_start))
            .on_action(cx.listener(Self::on_select_line_end))
            .on_action(cx.listener(Self::on_select_all))
            .on_action(cx.listener(Self::on_backspace))
            .on_action(cx.listener(Self::on_delete))
            .on_action(cx.listener(Self::on_delete_word_left))
            .on_action(cx.listener(Self::on_newline))
            .on_action(cx.listener(Self::on_indent))
            .on_action(cx.listener(Self::on_outdent))
            .on_action(cx.listener(Self::on_toggle_comment))
            .on_action(cx.listener(Self::on_duplicate_line))
            .on_action(cx.listener(Self::on_delete_line))
            .on_action(cx.listener(Self::on_undo))
            .on_action(cx.listener(Self::on_redo))
            .on_action(cx.listener(Self::on_cut))
            .on_action(cx.listener(Self::on_copy))
            .on_action(cx.listener(Self::on_paste))
            .on_action(cx.listener(Self::on_add_cursor_above))
            .on_action(cx.listener(Self::on_add_cursor_below))
            .on_action(cx.listener(Self::on_select_next_occurrence))
            .on_action(cx.listener(Self::on_go_to_definition))
            .on_action(cx.listener(Self::on_format))
            .on_action(cx.listener(Self::on_trigger_completion))
            .on_action(cx.listener(Self::on_dismiss_completion))
            .on_action(cx.listener(Self::on_show_hover))
            .on_mouse_down(MouseButton::Left, cx.listener(Self::on_mouse_down))
            .on_mouse_move(cx.listener(Self::on_mouse_move))
            .on_mouse_up(MouseButton::Left, cx.listener(Self::on_mouse_up))
            .on_scroll_wheel(cx.listener(Self::on_scroll_wheel))
            .child(EditorElement {
                view: cx.entity().clone(),
            })
            .children(completion)
            .children(hover)
    }
}
