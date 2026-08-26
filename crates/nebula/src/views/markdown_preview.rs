//! Markdown プレビュー。
//!
//! タブの中身がアクティブなタブに従属して表示される「もう1つの見た目」であって、
//! 独立したペインではない (ペイン分割の 2 枠上限には数えない)。そのため
//! `views/editor.rs` の `Tab` ごとに 1 つ持たせ、タブが閉じられれば一緒に消える。
//!
//! Markdown → 描くべき要素の並びへの変換 (tree-sitter を使う `parse_preview`) は
//! バックエンドで行う (`crates/nebula-backend/src/buffers.rs` の `markdown_preview`
//! ハンドラ)。GUI プロセスでは tree-sitter を一切動かさない
//! (構文解析は GUI では走らせない、という設計方針)。ここでは、
//! 版数のズレを防ぎながらバックエンドへ要求を送ることと、結果を `div()` の並びへ
//! 描くことに専念する。`editor_element.rs` が使っている低レベルな
//! `Element`/`shape_line` 機構は使わない (あれは巨大バッファを高速に描くための
//! 仕組みで、プレビューの分量には不要)。

use crate::ipc_client::BackendClient;
use crate::theme::{Theme, metrics, theme};
use crate::ui::{empty_state, h_flex, v_flex};
use crate::views::editor_view::EditorView;
use gpui::prelude::*;
use gpui::{AnyElement, Context, Entity, Render, Window, div, px};
use nebula_protocol::{BufferId, ListMarker, PreviewBlock, Request, Response, TokenKind};

pub struct MarkdownPreviewView {
    editor: Entity<EditorView>,
    client: BackendClient,
    buffer: BufferId,
    /// 直近に表示したブロック列。まだ 1 度も応答を受け取っていなければ空。
    ///
    /// 新しい要求を送った直後もこれは消さない。応答を待っている間、画面を
    /// 空にして点滅させるより、古い内容でも出し続けたほうが違和感が小さい。
    blocks: Vec<PreviewBlock>,
    /// `blocks` が対応するバッファ版数 (バックエンドの応答が返す版数)。
    /// まだ 1 度も応答を受け取っていなければ `None`。
    ///
    /// 応答が届くたびに [`should_adopt_preview`] でこれと比較し、より新しい
    /// 版数の応答だけを採用する。バックエンドは要求ごとに別タスクで処理するため
    /// 応答の順序は要求した順序と入れ替わりうる (詳しくは同関数のコメント)。
    displayed_version: Option<u64>,
    /// 直近に要求を送った時点の、エディタ側 (ローカル) の版数。
    ///
    /// これがエディタの現在の版数と一致している間は要求を送り直さない。
    /// このビューは埋め込まれている限り、タブのフォーカス移動やカーソル点滅など
    /// 編集と無関係な再描画でも `render` が呼ばれうるため、このガードが無いと
    /// 同じ版数へ毎フレーム要求を送ってしまう。
    requested_version: Option<u64>,
}

impl MarkdownPreviewView {
    pub fn new(editor: Entity<EditorView>, client: BackendClient, buffer: BufferId) -> Self {
        Self {
            editor,
            client,
            buffer,
            blocks: Vec::new(),
            displayed_version: None,
            requested_version: None,
        }
    }

    /// エディタが要求後に編集されていれば、バックエンドへ最新内容のプレビューを
    /// 要求する。応答は非同期に届くので、ここでは要求を送るだけで `blocks` は
    /// 変えない (差し替えは応答が届いたときだけ)。
    fn request_if_stale(&mut self, cx: &mut Context<Self>) {
        let local_version = self.editor.read(cx).buffer().version();
        if self.requested_version == Some(local_version) {
            return;
        }
        self.requested_version = Some(local_version);

        let client = self.client.clone();
        let buffer = self.buffer;
        cx.spawn(async move |this, cx| {
            let result = client.request(Request::MarkdownPreview { buffer }).await;
            this.update(cx, |this, cx| {
                if let Ok(Response::MarkdownPreview { version, blocks }) = result
                    && should_adopt_preview(this.displayed_version, version)
                {
                    this.displayed_version = Some(version);
                    this.blocks = blocks;
                    cx.notify();
                }
            })
            .ok();
        })
        .detach();
    }
}

impl Render for MarkdownPreviewView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // 表示中 (= render が呼ばれている) かつ版数が変わっていれば要求する。
        self.request_if_stale(cx);

        let theme = theme(cx).clone();
        let body: AnyElement = if self.blocks.is_empty() {
            empty_state("プレビューする内容がありません", cx).into_any_element()
        } else {
            v_flex()
                .gap(px(10.))
                .children(self.blocks.iter().map(|block| render_block(block, &theme)))
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

/// 応答の版数が、現在表示している版数より新しければ採用してよいかを判定する。
///
/// バックエンドは要求ごとに別の非同期タスクで処理する
/// (`crates/nebula-backend/src/ipc.rs` の `handle_client_message` が要求のたびに
/// `tokio::spawn` する)。そのため応答は要求した順序どおりに届くとは限らない:
/// 新しい版数を要求した直後の応答が、それより前に投げていた古い版数への応答より
/// 先に届くことが実際にありうる。この関数は、表示中の内容をその古い応答で
/// 上書きしないためのガードを純粋関数として切り出したもの。
///
/// `displayed` が `None` (まだ 1 度も応答を受け取っていない) なら常に採用する。
fn should_adopt_preview(displayed: Option<u64>, incoming: u64) -> bool {
    match displayed {
        None => true,
        Some(displayed) => incoming > displayed,
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
        PreviewBlock::ListItem {
            depth,
            marker,
            text,
        } => render_list_item(*depth, marker, text, theme),
        PreviewBlock::CodeBlock { language, code } => {
            render_code_block(language.as_deref(), code, theme)
        }
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

#[cfg(test)]
mod tests {
    use super::should_adopt_preview;

    #[test]
    fn 新しい版数の応答は採用する() {
        assert!(should_adopt_preview(Some(3), 4));
    }

    #[test]
    fn 同じ版数の応答は採用しない() {
        // 同じ版数を採用しても表示内容は変わらないはずなので、無駄な差し替えを
        // 避ける側 (採用しない) に倒しておく。
        assert!(!should_adopt_preview(Some(3), 3));
    }

    #[test]
    fn 古い版数の応答は捨てる() {
        // 新しい版数 (5) を表示した後、それより前に投げていた要求の応答 (3) が
        // 遅れて届いた状況を想定する。表示中の内容を古い方で上書きしてはいけない。
        assert!(!should_adopt_preview(Some(5), 3));
    }

    #[test]
    fn まだ何も表示していなければ最初の応答を採用する() {
        assert!(should_adopt_preview(None, 0));
    }
}
