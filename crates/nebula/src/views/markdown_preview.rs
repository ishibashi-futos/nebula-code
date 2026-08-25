//! Markdown プレビュー。
//!
//! タブの中身がアクティブなタブに従属して表示される「もう1つの見た目」であって、
//! 独立したペインではない (ペイン分割の 2 枠上限には数えない)。そのため
//! `views/editor.rs` の `Tab` ごとに 1 つ持たせ、タブが閉じられれば一緒に消える。
//!
//! Markdown → 描くべき要素の並びへの変換 (`nebula_core::markdown_preview::parse_preview`)
//! は GPUI に依存しない純粋関数として `nebula-core` 側に切り出してあり、テストも
//! そちらにある。ここでは、その結果を `div()` の並びへ描くだけに専念する。
//! `editor_element.rs` が使っている低レベルな `Element`/`shape_line` 機構は使わない
//! (あれは巨大バッファを高速に描くための仕組みで、プレビューの分量には不要)。

use crate::theme::{Theme, metrics, theme};
use crate::ui::{empty_state, h_flex, v_flex};
use crate::views::editor_view::EditorView;
use gpui::prelude::*;
use gpui::{AnyElement, Context, Entity, Render, Window, div, px};
use nebula_core::markdown_preview::{ListMarker, PreviewBlock, parse_preview};
use nebula_protocol::TokenKind;

pub struct MarkdownPreviewView {
    editor: Entity<EditorView>,
    /// 直近に変換した結果。`buffer().version()` が変わらない限り再変換しない。
    ///
    /// このビューは埋め込まれている限り、タブのフォーカス移動やカーソル点滅など
    /// 編集と無関係な再描画でも `render` が呼ばれうる。そのたびに tree-sitter で
    /// 構文解析し直すのは無駄なので、版数が同じ間はキャッシュを使い回す。
    cache: Option<(u64, Vec<PreviewBlock>)>,
}

impl MarkdownPreviewView {
    pub fn new(editor: Entity<EditorView>) -> Self {
        Self { editor, cache: None }
    }

    /// 現在のバッファ内容を変換した結果を返す。必要なときだけ再変換する。
    fn blocks<'a>(&'a mut self, cx: &mut Context<Self>) -> &'a [PreviewBlock] {
        let version = self.editor.read(cx).buffer().version();
        let stale = !matches!(&self.cache, Some((cached, _)) if *cached == version);
        if stale {
            let text = self.editor.read(cx).buffer().text();
            self.cache = Some((version, parse_preview(&text)));
        }
        &self.cache.as_ref().expect("直前に必ず設定している").1
    }
}

impl Render for MarkdownPreviewView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = theme(cx).clone();
        let blocks = self.blocks(cx);

        let body: AnyElement = if blocks.is_empty() {
            empty_state("プレビューする内容がありません", cx).into_any_element()
        } else {
            v_flex()
                .gap(px(10.))
                .children(blocks.iter().map(|block| render_block(block, &theme)))
                .into_any_element()
        };

        v_flex()
            .id("markdown-preview")
            .size_full()
            .min_h(px(0.))
            .overflow_y_scroll()
            .bg(theme.bg_elevated)
            .p(px(16.))
            .child(body)
    }
}

fn render_block(block: &PreviewBlock, theme: &Theme) -> AnyElement {
    match block {
        PreviewBlock::Heading { level, text } => render_heading(*level, text, theme),
        PreviewBlock::Paragraph { text } => div()
            .text_size(px(13.))
            .line_height(px(20.))
            .text_color(theme.text)
            .child(text.clone())
            .into_any_element(),
        PreviewBlock::ListItem { depth, marker, text } => render_list_item(*depth, marker, text, theme),
        PreviewBlock::CodeBlock { language, code } => render_code_block(language.as_deref(), code, theme),
        PreviewBlock::Quote { depth, text } => render_quote(*depth, text, theme),
        PreviewBlock::ThematicBreak => div().h(px(1.)).w_full().bg(theme.border).into_any_element(),
    }
}

/// 見出し。レベルが小さいほど大きく太く描き、1〜2 は下線も添える。
fn render_heading(level: u8, text: &str, theme: &Theme) -> AnyElement {
    let size = match level {
        1 => px(26.),
        2 => px(22.),
        3 => px(19.),
        4 => px(16.),
        5 => px(14.),
        _ => px(13.),
    };
    let weight = if level <= 2 {
        gpui::FontWeight::BOLD
    } else {
        gpui::FontWeight::SEMIBOLD
    };
    div()
        .text_size(size)
        .font_weight(weight)
        .text_color(theme.syntax_color(TokenKind::Heading))
        .when(level <= 2, |el| {
            el.pb(px(4.)).border_b_1().border_color(theme.border)
        })
        .child(text.to_string())
        .into_any_element()
}

/// 箇条書き・順序付きリスト・チェックリストの 1 項目。
/// ネストの深さぶんだけ左インデントを足す。
fn render_list_item(depth: u8, marker: &ListMarker, text: &str, theme: &Theme) -> AnyElement {
    let (label, color) = match marker {
        ListMarker::Bullet => ("•".to_string(), theme.text_faint),
        ListMarker::Ordered(n) => (format!("{n}."), theme.text_faint),
        ListMarker::TaskUnchecked => ("☐".to_string(), theme.text_faint),
        ListMarker::TaskChecked => ("☑".to_string(), theme.success),
    };
    h_flex()
        .items_start()
        .gap(px(7.))
        .pl(px(depth as f32 * 18.))
        .child(
            div()
                .flex_none()
                .min_w(px(14.))
                .text_size(px(13.))
                .text_color(color)
                .child(label),
        )
        .child(
            div()
                .flex_1()
                .min_w(px(0.))
                .text_size(px(13.))
                .line_height(px(20.))
                .text_color(theme.text)
                .child(text.to_string()),
        )
        .into_any_element()
}

/// フェンス付きコードブロック。等幅フォントで表示し、言語指定があればヘッダに出す。
fn render_code_block(language: Option<&str>, code: &str, theme: &Theme) -> AnyElement {
    v_flex()
        .rounded(px(5.))
        .overflow_hidden()
        .bg(theme.bg_surface)
        .border_1()
        .border_color(theme.border)
        .when_some(language, |el, lang| {
            el.child(
                div()
                    .px(px(8.))
                    .py(px(3.))
                    .text_size(px(10.5))
                    .text_color(theme.text_faint)
                    .border_b_1()
                    .border_color(theme.border)
                    .child(lang.to_string()),
            )
        })
        .child(
            div()
                .p(px(8.))
                .font_family(metrics::MONO_FONT_FAMILY)
                .text_size(px(12.))
                .line_height(px(18.))
                .text_color(theme.text)
                .child(code.to_string()),
        )
        .into_any_element()
}

/// 引用。左に縦線を引いて地の文と区別する。ネストしているぶんだけ字下げも足す。
fn render_quote(depth: u8, text: &str, theme: &Theme) -> AnyElement {
    div()
        .pl(px(10. + (depth.saturating_sub(1)) as f32 * 14.))
        .py(px(2.))
        .border_l_2()
        .border_color(theme.accent_tertiary)
        .text_size(px(13.))
        .line_height(px(20.))
        .text_color(theme.text_muted)
        .italic()
        .child(text.to_string())
        .into_any_element()
}
