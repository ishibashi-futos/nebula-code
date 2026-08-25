//! エディタ本文の描画。
//!
//! `div` の組み合わせでは行数ぶんの要素が生まれてしまうので、可視行だけを自前で
//! シェイプして描く低レベル要素として実装する。1 万行のファイルでも描画コストは
//! 画面に映る数十行ぶんにしか比例しない。

use crate::theme::{metrics, theme};
use crate::views::editor_view::EditorView;
use gpui::prelude::*;
use gpui::{
    App, Bounds, ElementId, ElementInputHandler, Entity, Focusable, FontWeight, GlobalElementId,
    Hsla, LayoutId, PaintQuad, Pixels, Point, ShapedLine, SharedString, Style, TextRun,
    UnderlineStyle, Window, fill, point, px, relative, size,
};
use nebula_core::RopeExt;
use nebula_protocol::{DiagnosticSeverity, HighlightSpan, HunkKind, TokenKind};
use std::ops::Range;

/// 直前の描画で確定した幾何情報。マウス座標→文字位置の変換に使う。
pub struct EditorLayoutInfo {
    /// 要素の左上。ポップアップを要素相対で置くために使う。
    pub origin: Point<Pixels>,
    /// 可視行のシェイプ結果。索引 0 が `first_row` 行目。
    pub lines: Vec<ShapedLine>,
    pub first_row: usize,
    pub line_height: Pixels,
    /// 本文の左上 (ガターの右側)。
    pub text_origin: Point<Pixels>,
    pub visible_rows: f32,
    pub gutter_width: Pixels,
}

pub struct EditorElement {
    pub view: Entity<EditorView>,
}

pub struct EditorPrepaint {
    lines: Vec<ShapedLine>,
    line_numbers: Vec<(ShapedLine, Pixels, bool)>,
    background_quads: Vec<PaintQuad>,
    cursor_quads: Vec<PaintQuad>,
    gutter_marks: Vec<PaintQuad>,
    indent_guides: Vec<PaintQuad>,
    first_row: usize,
    line_height: Pixels,
    text_origin: Point<Pixels>,
    gutter_width: Pixels,
    visible_rows: f32,
    scroll_left: Pixels,
}

impl IntoElement for EditorElement {
    type Element = Self;

    fn into_element(self) -> Self::Element {
        self
    }
}

impl Element for EditorElement {
    type RequestLayoutState = ();
    type PrepaintState = EditorPrepaint;

    fn id(&self) -> Option<ElementId> {
        None
    }

    fn source_location(&self) -> Option<&'static core::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&gpui::InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, Self::RequestLayoutState) {
        let mut style = Style::default();
        style.size.width = relative(1.).into();
        style.size.height = relative(1.).into();
        (window.request_layout(style, [], cx), ())
    }

    fn prepaint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&gpui::InspectorElementId>,
        bounds: Bounds<Pixels>,
        _layout: &mut Self::RequestLayoutState,
        window: &mut Window,
        cx: &mut App,
    ) -> Self::PrepaintState {
        let theme = theme(cx).clone();
        let text_style = window.text_style();
        let font_size = metrics::EDITOR_FONT_SIZE;
        let line_height = px(f32::from(font_size) * metrics::LINE_HEIGHT_RATIO);

        let view = self.view.read(cx);
        let rope = view.buffer().rope();
        let total_lines = rope.len_lines();
        let scroll_top = view.scroll_top;
        let scroll_left = view.scroll_left;
        let visible_rows = f32::from(bounds.size.height) / f32::from(line_height);

        let first_row = scroll_top.floor().max(0.0) as usize;
        let last_row = (first_row + visible_rows.ceil() as usize + 1).min(total_lines);

        // ガター幅は総行数の桁数で決める。スクロールしても幅が動かないようにするため。
        let digits = total_lines.max(1).to_string().len();
        let digit_width = window
            .text_system()
            .shape_line(
                SharedString::from("0".repeat(digits)),
                font_size,
                &[TextRun {
                    len: digits,
                    font: text_style.font(),
                    color: theme.line_number,
                    background_color: None,
                    underline: None,
                    strikethrough: None,
                }],
                None,
            )
            .width;
        let gutter_width = (digit_width + px(28.)).max(metrics::GUTTER_MIN_WIDTH);
        let text_origin = point(bounds.origin.x + gutter_width, bounds.origin.y);

        let cursor = view.primary_cursor();
        let cursor_row = rope.offset_to_position(cursor).row as usize;
        // 対応する括弧。カーソルの直前にある括弧も見るのは、閉じ括弧を打った直後に
        // 印が出ないと「対応が取れているか」を確認できないため。
        let brackets: Vec<usize> = [Some(cursor), cursor.checked_sub(1)]
            .into_iter()
            .flatten()
            .filter_map(|at| nebula_core::matching_bracket(rope, at).map(|other| [at, other]))
            .next()
            .map(|pair| pair.to_vec())
            .unwrap_or_default();
        let highlights = view.highlights_for(first_row as u32..last_row as u32);
        let marked = view.marked_range();

        let mut lines = Vec::with_capacity(last_row.saturating_sub(first_row));
        let mut line_numbers = Vec::with_capacity(lines.capacity());
        let mut background_quads = Vec::new();
        let mut cursor_quads = Vec::new();
        let mut gutter_marks = Vec::new();
        let mut indent_guides = Vec::new();

        for row in first_row..last_row {
            let y = bounds.origin.y + line_height * (row as f32 - scroll_top);
            let line_start = rope.line_to_char(row);
            let line_text = rope.line_text(row).to_string();

            // 現在行の帯
            if row == cursor_row {
                background_quads.push(fill(
                    Bounds::new(
                        point(bounds.origin.x, y),
                        size(bounds.size.width, line_height),
                    ),
                    theme.current_line,
                ));
            }

            // git ガター
            if let Some(kind) = hunk_kind_at(view, row as u32) {
                let color = match kind {
                    HunkKind::Added => theme.git_added,
                    HunkKind::Modified => theme.git_modified,
                    HunkKind::Removed => theme.git_deleted,
                };
                let height = if kind == HunkKind::Removed {
                    px(2.)
                } else {
                    line_height
                };
                gutter_marks.push(fill(
                    Bounds::new(
                        point(bounds.origin.x + gutter_width - px(6.), y),
                        size(px(2.), height),
                    ),
                    color,
                ));
            }

            // インデントガイド
            let indent_columns = leading_indent_columns(&line_text, view.config().indent_width);
            for level in 1..indent_columns {
                let x = text_origin.x
                    + digit_advance(window, font_size, &text_style, &theme)
                        * (level * view.config().indent_width) as f32
                    - scroll_left;
                if x >= text_origin.x {
                    indent_guides.push(fill(
                        Bounds::new(point(x, y), size(px(1.), line_height)),
                        theme.indent_guide,
                    ));
                }
            }

            // 本文のシェイプ
            let runs = build_runs(
                &line_text,
                line_start,
                highlights,
                &text_style,
                &theme,
                view.diagnostics(),
                row as u32,
                marked.as_ref(),
            );
            let shaped = window.text_system().shape_line(
                SharedString::from(line_text.clone()),
                font_size,
                &runs,
                None,
            );

            // 対応する括弧の下線
            let line_end = line_start + line_text.chars().count();
            for at in &brackets {
                if *at < line_start || *at >= line_end {
                    continue;
                }
                let column = at - line_start;
                let x0 = shaped.x_for_index(char_to_byte(&line_text, column));
                let x1 = shaped.x_for_index(char_to_byte(&line_text, column + 1));
                gutter_marks.push(fill(
                    Bounds::new(
                        point(text_origin.x + x0 - scroll_left, y + line_height - px(2.)),
                        size((x1 - x0).max(px(4.)), px(2.)),
                    ),
                    theme.bracket_match,
                ));
            }

            // 選択範囲
            for sel in view.selections() {
                if sel.is_empty() || sel.end() <= line_start || sel.start() > line_end {
                    continue;
                }
                let from = sel.start().max(line_start) - line_start;
                let to = (sel.end().min(line_end)) - line_start;
                let x0 = shaped.x_for_index(char_to_byte(&line_text, from));
                let x1 = shaped.x_for_index(char_to_byte(&line_text, to));
                // 行末を跨ぐ選択は改行ぶんの幅を足して「行全体が選ばれている」ことを示す。
                let extra = if sel.end() > line_end { px(6.) } else { px(0.) };
                background_quads.push(fill(
                    Bounds::new(
                        point(text_origin.x + x0 - scroll_left, y),
                        size(x1 - x0 + extra, line_height),
                    ),
                    theme.selection,
                ));
            }

            // カーソル
            for sel in view.selections() {
                if sel.head < line_start || sel.head > line_end {
                    continue;
                }
                let column = sel.head - line_start;
                let x = shaped.x_for_index(char_to_byte(&line_text, column));
                cursor_quads.push(fill(
                    Bounds::new(
                        point(text_origin.x + x - scroll_left, y),
                        size(px(2.), line_height),
                    ),
                    theme.cursor,
                ));
            }

            // 行番号
            let is_current = row == cursor_row;
            let number = SharedString::from((row + 1).to_string());
            let number_color = if is_current {
                theme.line_number_active
            } else {
                theme.line_number
            };
            let number_run = TextRun {
                len: number.len(),
                font: text_style.font(),
                color: number_color,
                background_color: None,
                underline: None,
                strikethrough: None,
            };
            let shaped_number =
                window
                    .text_system()
                    .shape_line(number, font_size, &[number_run], None);
            line_numbers.push((shaped_number, y, is_current));

            lines.push(shaped);
        }

        // 可視範囲のハイライトを要求しておく。次のフレームで色がつく。
        let request_range = first_row as u32..last_row as u32;
        self.view.update(cx, |view, cx| {
            view.request_highlights(request_range, cx);
        });

        EditorPrepaint {
            lines,
            line_numbers,
            background_quads,
            cursor_quads,
            gutter_marks,
            indent_guides,
            first_row,
            line_height,
            text_origin,
            gutter_width,
            visible_rows,
            scroll_left,
        }
    }

    fn paint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&gpui::InspectorElementId>,
        bounds: Bounds<Pixels>,
        _layout: &mut Self::RequestLayoutState,
        prepaint: &mut Self::PrepaintState,
        window: &mut Window,
        cx: &mut App,
    ) {
        let theme = theme(cx).clone();
        let focus_handle = self.view.read(cx).focus_handle(cx);
        window.handle_input(
            &focus_handle,
            ElementInputHandler::new(bounds, self.view.clone()),
            cx,
        );
        let focused = focus_handle.is_focused(window);

        window.paint_quad(fill(bounds, theme.editor_bg));
        for quad in prepaint.background_quads.drain(..) {
            window.paint_quad(quad);
        }
        for quad in prepaint.indent_guides.drain(..) {
            window.paint_quad(quad);
        }
        for quad in prepaint.gutter_marks.drain(..) {
            window.paint_quad(quad);
        }

        // 行番号は本文より先に描く。横スクロールしても隠れないよう、
        // 本文はガターの右側にしか出ない前提で座標を組んでいる。
        for (shaped, y, _is_current) in prepaint.line_numbers.drain(..) {
            let x = bounds.origin.x + prepaint.gutter_width - px(16.) - shaped.width;
            shaped
                .paint(point(x, y), prepaint.line_height, window, cx)
                .ok();
        }

        for (index, line) in prepaint.lines.iter().enumerate() {
            let y = bounds.origin.y
                + prepaint.line_height
                    * ((prepaint.first_row + index) as f32 - self.view.read(cx).scroll_top);
            line.paint(
                point(prepaint.text_origin.x - prepaint.scroll_left, y),
                prepaint.line_height,
                window,
                cx,
            )
            .ok();
        }

        if focused {
            for quad in prepaint.cursor_quads.drain(..) {
                window.paint_quad(quad);
            }
        }

        // レイアウトを記録する。マウス操作とスクロール量の計算に使う。
        let layout = EditorLayoutInfo {
            origin: bounds.origin,
            lines: std::mem::take(&mut prepaint.lines),
            first_row: prepaint.first_row,
            line_height: prepaint.line_height,
            text_origin: prepaint.text_origin,
            visible_rows: prepaint.visible_rows,
            gutter_width: prepaint.gutter_width,
        };
        self.view.update(cx, |view, _cx| {
            view.last_layout = Some(layout);
        });
    }
}

/// 行に対応する diff ハンクの種別。
fn hunk_kind_at(view: &EditorView, row: u32) -> Option<HunkKind> {
    view.hunks()
        .iter()
        .find(|h| {
            if h.kind == HunkKind::Removed {
                // 削除は「その位置に何かが消えた」ことを示すので 1 行として扱う。
                h.new_start == row
            } else {
                row >= h.new_start && row < h.new_start + h.new_lines
            }
        })
        .map(|h| h.kind)
}

/// 行頭のインデントが何段ぶんか。
fn leading_indent_columns(line: &str, indent_width: u32) -> u32 {
    let mut columns = 0u32;
    for c in line.chars() {
        match c {
            ' ' => columns += 1,
            '\t' => columns += indent_width,
            _ => break,
        }
    }
    columns / indent_width.max(1)
}

/// 等幅フォントの 1 文字幅。インデントガイドの位置決めに使う。
fn digit_advance(
    window: &mut Window,
    font_size: Pixels,
    text_style: &gpui::TextStyle,
    theme: &crate::theme::Theme,
) -> Pixels {
    window
        .text_system()
        .shape_line(
            SharedString::from("0"),
            font_size,
            &[TextRun {
                len: 1,
                font: text_style.font(),
                color: theme.text,
                background_color: None,
                underline: None,
                strikethrough: None,
            }],
            None,
        )
        .width
}

fn char_to_byte(text: &str, char_offset: usize) -> usize {
    text.char_indices()
        .nth(char_offset)
        .map(|(i, _)| i)
        .unwrap_or(text.len())
}

/// 1 行ぶんのテキストランを組み立てる。
///
/// `TextRun::len` は **バイト数**。文字オフセットで持っているスパンを
/// バイト境界に直しながら、隙間を既定色で埋める。
#[allow(clippy::too_many_arguments)]
fn build_runs(
    line_text: &str,
    line_start: usize,
    highlights: &[HighlightSpan],
    text_style: &gpui::TextStyle,
    theme: &crate::theme::Theme,
    diagnostics: &[nebula_protocol::Diagnostic],
    row: u32,
    marked: Option<&Range<usize>>,
) -> Vec<TextRun> {
    let line_chars = line_text.chars().count();
    let line_end = line_start + line_chars;
    if line_chars == 0 {
        return Vec::new();
    }

    // 行内の各文字に色を割り当てる。スパンは重ならない前提 (バックエンドが保証する)。
    let mut colors: Vec<Hsla> = vec![text_style.color; line_chars];
    // 見出しだけ太字にする。色だけだと本文と紛れやすく、Markdown の見出しは
    // 太字であるべきという一般的な見た目の期待にも沿う。
    let mut bold: Vec<bool> = vec![false; line_chars];
    for span in highlights {
        if span.end <= line_start || span.start >= line_end {
            continue;
        }
        let from = span.start.max(line_start) - line_start;
        let to = span.end.min(line_end) - line_start;
        let color = theme.syntax_color(span.token);
        for slot in &mut colors[from..to] {
            *slot = color;
        }
        if span.token == TokenKind::Heading {
            for slot in &mut bold[from..to] {
                *slot = true;
            }
        }
    }

    // 診断の波線。行に重なる範囲だけ。
    let mut underlines: Vec<Option<UnderlineStyle>> = vec![None; line_chars];
    for diagnostic in diagnostics {
        if diagnostic.range.start.row > row || diagnostic.range.end.row < row {
            continue;
        }
        let from = if diagnostic.range.start.row == row {
            diagnostic.range.start.column as usize
        } else {
            0
        };
        let to = if diagnostic.range.end.row == row {
            (diagnostic.range.end.column as usize).min(line_chars)
        } else {
            line_chars
        };
        let style = UnderlineStyle {
            color: Some(severity_color(theme, diagnostic.severity)),
            thickness: px(1.),
            wavy: true,
        };
        for slot in &mut underlines[from.min(line_chars)..to.max(from).min(line_chars)] {
            *slot = Some(style.clone());
        }
    }

    // IME の未確定範囲には直線の下線を引く。波線と区別できるようにする。
    if let Some(marked) = marked
        && marked.end > line_start
        && marked.start < line_end
    {
        let from = marked.start.max(line_start) - line_start;
        let to = marked.end.min(line_end) - line_start;
        let style = UnderlineStyle {
            color: Some(theme.accent),
            thickness: px(1.),
            wavy: false,
        };
        for slot in &mut underlines[from..to] {
            *slot = Some(style.clone());
        }
    }

    // 同じ色・同じ下線・同じ太さが続く区間を 1 ラン に畳む。ランが多いとシェイプが遅くなる。
    let mut runs: Vec<TextRun> = Vec::new();
    let mut current: Option<(Hsla, Option<UnderlineStyle>, bool, usize)> = None;
    for (index, c) in line_text.chars().enumerate() {
        let color = colors[index];
        let underline = underlines[index].clone();
        let is_bold = bold[index];
        let byte_len = c.len_utf8();
        match &mut current {
            Some((run_color, run_underline, run_bold, len))
                if *run_color == color
                    && underline_eq(run_underline, &underline)
                    && *run_bold == is_bold =>
            {
                *len += byte_len;
            }
            Some((run_color, run_underline, run_bold, len)) => {
                runs.push(TextRun {
                    len: *len,
                    font: run_font(text_style, *run_bold),
                    color: *run_color,
                    background_color: None,
                    underline: run_underline.clone(),
                    strikethrough: None,
                });
                current = Some((color, underline, is_bold, byte_len));
            }
            None => current = Some((color, underline, is_bold, byte_len)),
        }
    }
    if let Some((color, underline, is_bold, len)) = current {
        runs.push(TextRun {
            len,
            font: run_font(text_style, is_bold),
            color,
            background_color: None,
            underline,
            strikethrough: None,
        });
    }
    runs
}

/// 見出しなど強調したいトークンだけ太字にしたフォントを返す。
///
/// 前例: `views/terminal.rs` の `make_run` と同じイディオム。
fn run_font(text_style: &gpui::TextStyle, bold: bool) -> gpui::Font {
    let mut font = text_style.font();
    if bold {
        font.weight = FontWeight::BOLD;
    }
    font
}

fn underline_eq(a: &Option<UnderlineStyle>, b: &Option<UnderlineStyle>) -> bool {
    match (a, b) {
        (None, None) => true,
        (Some(a), Some(b)) => a.color == b.color && a.thickness == b.thickness && a.wavy == b.wavy,
        _ => false,
    }
}

fn severity_color(theme: &crate::theme::Theme, severity: DiagnosticSeverity) -> Hsla {
    theme.diagnostic_color(severity)
}

/// テーマから未使用警告を出さないための参照。
#[allow(dead_code)]
fn _token_kind_marker(_: TokenKind) {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn インデント段数を数える() {
        assert_eq!(leading_indent_columns("        x", 4), 2);
        assert_eq!(leading_indent_columns("    x", 4), 1);
        assert_eq!(leading_indent_columns("x", 4), 0);
        assert_eq!(leading_indent_columns("\t\tx", 4), 2);
    }

    #[test]
    fn 文字オフセットをバイトに変換する() {
        assert_eq!(char_to_byte("あいう", 0), 0);
        assert_eq!(char_to_byte("あいう", 1), 3);
        assert_eq!(char_to_byte("あいう", 3), 9, "末尾は文字列長");
        assert_eq!(char_to_byte("あいう", 99), 9, "範囲外も末尾に丸める");
    }

    #[test]
    fn 見出しトークンだけ太字のランになる() {
        // "# " はマーカー部分 (太字にしない)、"見出し" が Heading スパン (太字にする)
        // という状況を模す。TokenKind::Heading のスパンだけがランを分割し、
        // そのランのフォントだけが太くなることを確認する。
        let line = "# 見出し";
        let heading_start = "# ".chars().count();
        let highlights = vec![HighlightSpan {
            start: heading_start,
            end: line.chars().count(),
            token: TokenKind::Heading,
        }];
        let text_style = gpui::TextStyle::default();
        let theme = crate::theme::Theme::cyber_cosmic();
        let runs = build_runs(line, 0, &highlights, &text_style, &theme, &[], 0, None);

        assert_eq!(runs.len(), 2, "マーカーと見出し本文で2ランに分かれる");
        assert_ne!(
            runs[0].font.weight,
            FontWeight::BOLD,
            "マーカー部分は太字にしない"
        );
        assert_eq!(
            runs[1].font.weight,
            FontWeight::BOLD,
            "見出し本文は太字にする"
        );
    }
}
