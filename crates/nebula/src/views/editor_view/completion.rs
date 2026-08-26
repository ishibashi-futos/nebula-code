//! 補完ポップアップとホバー情報の要求・絞り込み・確定・描画。
//!
//! `CompletionState` はフィールドとして `EditorView` (ルート) に残し、操作する手続きだけを寄せた。

use super::{CompletionState, EditorView, EditorViewEvent};
use crate::actions::{DismissCompletion, ShowHover, TriggerCompletion};
use crate::theme::theme;
use crate::ui::{h_flex, v_flex};
use gpui::prelude::*;
use gpui::{Context, Pixels, Window, div, px};
use nebula_core::RopeExt;
use nebula_protocol::{
    CompletionItem, CompletionKind, Edit, NotificationLevel, Request, Response, TextRange,
};

impl EditorView {
    // -- 補完とホバー --

    /// 補完対象の単語の開始位置。カーソル直前の識別子文字をさかのぼる。
    fn completion_anchor(&self, offset: usize) -> usize {
        let rope = self.buffer.rope();
        let mut start = offset.min(rope.len_chars());
        while start > 0 && is_word_char(rope.char(start - 1)) {
            start -= 1;
        }
        start
    }

    /// 補完を要求する。
    pub(super) fn request_completion(&mut self, trigger: Option<String>, cx: &mut Context<Self>) {
        if self.path.is_none() {
            return;
        }
        let cursor = self.primary_cursor();
        let anchor = self.completion_anchor(cursor);
        let position = self.buffer.rope().offset_to_position(cursor);
        let client = self.client.clone();
        let buffer = self.buffer_id;
        cx.spawn(async move |this, cx| {
            let result = client
                .request(Request::LspCompletion {
                    buffer,
                    position,
                    trigger,
                })
                .await;
            this.update(cx, |this, cx| {
                let Ok(Response::Completions(items)) = result else {
                    // 言語サーバーが無い・失敗した場合は黙って閉じる。
                    // 入力のたびに警告を出すとうるさい。
                    this.completion = None;
                    return;
                };
                if items.is_empty() {
                    this.completion = None;
                } else {
                    this.completion = Some(CompletionState {
                        items,
                        filtered: Vec::new(),
                        selected: 0,
                        anchor,
                    });
                    this.refilter_completion();
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// 現在入力されている接頭辞で候補を絞り込む。
    pub(super) fn refilter_completion(&mut self) {
        let cursor = self.primary_cursor();
        let Some(state) = self.completion.as_mut() else {
            return;
        };
        if cursor < state.anchor {
            // カーソルが補完開始位置より手前へ動いた = 補完対象から外れた。
            self.completion = None;
            return;
        }
        let prefix: String = self
            .buffer
            .rope()
            .slice(state.anchor..cursor)
            .to_string()
            .to_lowercase();
        state.filtered = state
            .items
            .iter()
            .enumerate()
            .filter(|(_, item)| {
                prefix.is_empty()
                    || fuzzy_contains(
                        &item
                            .filter_text
                            .clone()
                            .unwrap_or_else(|| item.label.clone()),
                        &prefix,
                    )
            })
            .map(|(index, _)| index)
            .collect();
        // 言語サーバーの並び順 (sort_text) を尊重しつつ、接頭辞が完全一致するものを前に出す。
        let items = &state.items;
        state.filtered.sort_by_key(|index| {
            let item = &items[*index];
            let starts = !item.label.to_lowercase().starts_with(&prefix);
            (
                starts,
                item.sort_text.clone().unwrap_or_else(|| item.label.clone()),
            )
        });
        state.selected = 0;
        if state.filtered.is_empty() {
            self.completion = None;
        }
    }

    /// 選択中の候補を確定する。補完が開いていなければ `false`。
    pub(super) fn accept_completion(&mut self, cx: &mut Context<Self>) -> bool {
        let Some(state) = self.completion.take() else {
            return false;
        };
        let Some(index) = state.filtered.get(state.selected).copied() else {
            return true;
        };
        let item = &state.items[index];
        let cursor = self.primary_cursor();
        // スニペット記法はそのまま挿入せず、プレースホルダを取り除く。
        // 本格的なスニペット展開は未対応だが、`$0` などが残るよりはよい。
        let text = if item.is_snippet {
            strip_snippet_placeholders(&item.insert_text)
        } else {
            item.insert_text.clone()
        };
        let before = self.selections.clone();
        let edits = vec![Edit::replace(TextRange::new(state.anchor, cursor), text)];
        self.commit_undo_group();
        self.apply(edits, before, cx);
        self.commit_undo_group();
        true
    }

    pub(super) fn on_trigger_completion(
        &mut self,
        _: &TriggerCompletion,
        _w: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.request_completion(None, cx);
    }

    pub(super) fn on_dismiss_completion(
        &mut self,
        _: &DismissCompletion,
        _w: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.completion.is_some() || self.hover.is_some() {
            self.completion = None;
            self.hover = None;
            cx.notify();
        }
    }

    pub(super) fn on_show_hover(&mut self, _: &ShowHover, _w: &mut Window, cx: &mut Context<Self>) {
        let cursor = self.primary_cursor();
        let position = self.buffer.rope().offset_to_position(cursor);
        let client = self.client.clone();
        let buffer = self.buffer_id;
        cx.spawn(async move |this, cx| {
            let result = client.request(Request::LspHover { buffer, position }).await;
            this.update(cx, |this, cx| {
                match result {
                    Ok(Response::Hover(Some(info))) => this.hover = Some((info, cursor)),
                    Ok(_) => {
                        this.hover = None;
                        cx.emit(EditorViewEvent::Notify(
                            NotificationLevel::Info,
                            "この位置に情報はありません".into(),
                        ));
                    }
                    Err(e) => cx.emit(EditorViewEvent::Notify(
                        NotificationLevel::Warning,
                        format!("情報を取得できません: {e}"),
                    )),
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// 文字オフセットの画面座標 (この要素の左上からの相対) を求める。
    fn popup_position(&self, offset: usize) -> Option<gpui::Point<Pixels>> {
        let layout = self.last_layout.as_ref()?;
        let position = self.buffer.rope().offset_to_position(offset);
        let visible_index = (position.row as usize).checked_sub(layout.first_row)?;
        let shaped = layout.lines.get(visible_index)?;
        let byte_in_line = shaped
            .text
            .char_indices()
            .nth(position.column as usize)
            .map(|(i, _)| i)
            .unwrap_or(shaped.text.len());
        let x = layout.text_origin.x + shaped.x_for_index(byte_in_line)
            - self.scroll_left
            - layout.origin.x;
        // 行の下に出す。行に被せると入力中の文字が隠れる。
        let y = layout.text_origin.y
            + layout.line_height * (position.row as f32 - self.scroll_top + 1.0)
            - layout.origin.y;
        Some(gpui::point(x.max(px(0.)), y))
    }

    pub(super) fn render_completion_popup(
        &self,
        cx: &mut Context<Self>,
    ) -> Option<gpui::AnyElement> {
        let state = self.completion.as_ref()?;
        let origin = self.popup_position(state.anchor)?;
        let theme = theme(cx).clone();
        // 一度に出すのは 12 件まで。それ以上は絞り込ませた方が速い。
        let visible: Vec<(usize, &CompletionItem)> = state
            .filtered
            .iter()
            .take(12)
            .map(|index| (*index, &state.items[*index]))
            .collect();
        let selected = state.selected;
        let detail = state
            .filtered
            .get(selected)
            .map(|index| &state.items[*index])
            .and_then(|item| item.documentation.clone().or_else(|| item.detail.clone()));

        let rows: Vec<_> = visible
            .iter()
            .enumerate()
            .map(|(row, (_, item))| {
                let is_selected = row == selected;
                let label = item.label.clone();
                let kind = completion_kind_label(item.kind);
                let detail = item.detail.clone().unwrap_or_default();
                h_flex()
                    .id(("completion", row))
                    .w_full()
                    .h(px(22.))
                    .px(px(8.))
                    .gap(px(8.))
                    .cursor_pointer()
                    .when(is_selected, |el| el.bg(theme.accent_soft))
                    .hover(|s| s.bg(theme.bg_overlay))
                    .child(
                        div()
                            .w(px(20.))
                            .flex_none()
                            .text_size(px(10.))
                            .text_color(theme.accent_tertiary)
                            .child(kind),
                    )
                    .child(
                        div()
                            .flex_none()
                            .text_size(px(12.))
                            .text_color(if is_selected {
                                theme.text
                            } else {
                                theme.text_muted
                            })
                            .child(label),
                    )
                    .child(
                        div()
                            .flex_1()
                            .overflow_hidden()
                            .text_size(px(11.))
                            .text_color(theme.text_faint)
                            .child(crate::ui::truncate_middle(&detail, 40)),
                    )
                    .on_click(cx.listener(move |this, _, _w, cx| {
                        if let Some(state) = this.completion.as_mut() {
                            state.selected = row;
                        }
                        this.accept_completion(cx);
                        cx.notify();
                    }))
            })
            .collect();

        Some(
            v_flex()
                .absolute()
                .left(origin.x)
                .top(origin.y)
                .w(px(420.))
                .max_h(px(300.))
                .bg(theme.bg_overlay)
                .border_1()
                .border_color(theme.border_glow)
                .rounded(px(6.))
                .overflow_hidden()
                .children(rows)
                .children(detail.map(|text| {
                    div()
                        .border_t_1()
                        .border_color(theme.border)
                        .p(px(8.))
                        .text_size(px(11.))
                        .text_color(theme.text_muted)
                        .child(text.chars().take(400).collect::<String>())
                }))
                .into_any_element(),
        )
    }

    pub(super) fn render_hover_card(&self, cx: &mut Context<Self>) -> Option<gpui::AnyElement> {
        let (info, offset) = self.hover.as_ref()?;
        let origin = self.popup_position(*offset)?;
        let theme = theme(cx).clone();
        Some(
            v_flex()
                .absolute()
                .left(origin.x)
                .top(origin.y)
                .max_w(px(560.))
                .max_h(px(320.))
                .p(px(10.))
                .bg(theme.bg_overlay)
                .border_1()
                .border_color(theme.border_glow)
                .rounded(px(6.))
                .overflow_hidden()
                .text_size(px(12.))
                .text_color(theme.text)
                .child(info.contents.clone())
                .into_any_element(),
        )
    }
}

pub(super) fn is_word_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

/// 接頭辞が候補に部分列として含まれるか。
///
/// 完全な前方一致だけにすると `hm` で `HashMap` が出せない。逆に何でも通すと
/// 候補が絞れないので、順序を保った部分列一致に留める。
fn fuzzy_contains(candidate: &str, lowercase_query: &str) -> bool {
    let mut chars = candidate.chars().flat_map(char::to_lowercase);
    lowercase_query
        .chars()
        .all(|needle| chars.any(|c| c == needle))
}

/// スニペット記法からプレースホルダを取り除く。
///
/// `${1:name}` → `name`、`$0` → 空。本格的な展開 (タブ移動) は未対応だが、
/// 記号がそのまま挿入されるよりは実用的。
fn strip_snippet_placeholders(text: &str) -> String {
    let mut result = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '$' {
            result.push(c);
            continue;
        }
        match chars.peek() {
            Some('{') => {
                chars.next();
                // `${1:name}` の `name` 部分だけを残す。
                let mut body = String::new();
                let mut depth = 1;
                for c in chars.by_ref() {
                    match c {
                        '{' => depth += 1,
                        '}' => {
                            depth -= 1;
                            if depth == 0 {
                                break;
                            }
                        }
                        _ => {}
                    }
                    if depth > 0 {
                        body.push(c);
                    }
                }
                if let Some((_, label)) = body.split_once(':') {
                    result.push_str(label);
                }
            }
            Some(c) if c.is_ascii_digit() => {
                while chars.peek().is_some_and(|c| c.is_ascii_digit()) {
                    chars.next();
                }
            }
            _ => result.push('$'),
        }
    }
    result
}

/// 候補の種別を 1〜2 文字で表す。アイコンを増やすより一覧が読みやすい。
fn completion_kind_label(kind: CompletionKind) -> &'static str {
    use CompletionKind::*;
    match kind {
        Method | Function => "fn",
        Constructor => "new",
        Field | Property => "var",
        Variable => "let",
        Class | Struct => "St",
        Interface => "tr",
        Module => "mod",
        Enum => "en",
        EnumMember => "em",
        Keyword => "kw",
        Snippet => "sn",
        File => "fi",
        Folder => "di",
        Constant => "cn",
        TypeParameter => "T",
        Operator => "op",
        _ => "·",
    }
}

#[cfg(test)]
mod completion_tests {
    use super::*;

    #[test]
    fn 部分列で候補を絞り込める() {
        assert!(fuzzy_contains("HashMap", "hm"));
        assert!(fuzzy_contains("HashMap", "hashm"));
        assert!(fuzzy_contains("into_iter", "iter"));
        assert!(!fuzzy_contains("HashMap", "mh"), "順序が違えば一致しない");
        assert!(!fuzzy_contains("Vec", "vecx"));
    }

    #[test]
    fn 空の接頭辞は常に一致する() {
        assert!(fuzzy_contains("anything", ""));
    }

    #[test]
    fn スニペットのプレースホルダを展開する() {
        assert_eq!(
            strip_snippet_placeholders("println!(\"${1:msg}\")$0"),
            "println!(\"msg\")"
        );
        assert_eq!(
            strip_snippet_placeholders("fn ${1:name}() {}"),
            "fn name() {}"
        );
        assert_eq!(strip_snippet_placeholders("plain"), "plain");
    }

    #[test]
    fn プレースホルダにラベルが無ければ消える() {
        assert_eq!(strip_snippet_placeholders("a${1}b"), "ab");
        assert_eq!(strip_snippet_placeholders("a$1b"), "ab");
    }

    #[test]
    fn ドル記号単体は残る() {
        assert_eq!(strip_snippet_placeholders("price: $"), "price: $");
    }

    #[test]
    fn 候補種別に短い表記が割り当てられる() {
        assert_eq!(completion_kind_label(CompletionKind::Function), "fn");
        assert_eq!(completion_kind_label(CompletionKind::Struct), "St");
        assert_eq!(completion_kind_label(CompletionKind::Color), "·");
    }

    #[test]
    fn 単語構成文字の判定() {
        assert!(is_word_char('a'));
        assert!(is_word_char('_'));
        assert!(is_word_char('あ'));
        assert!(!is_word_char('-'));
        assert!(!is_word_char(' '));
    }
}
