//! [`TextInput`] の本文を実際に字形化して描くカスタム要素。
//!
//! [`super::TextInput`] が持つ [`InputState`](super::InputState) を、
//! マウス位置やキャレット・選択範囲の描画座標へ落とし込む層。純粋な計算
//! (行の折り返し・座標変換) と gpui の [`Element`] 実装が絡み合うため、
//! 状態を持つ [`TextInput`] 本体とは別ファイルに分けてある。

use super::{TextInput, TextInputEvent};
use crate::theme::theme;
use gpui::{
    App, AvailableSpace, Bounds, Element, ElementId, ElementInputHandler, Entity, Font,
    GlobalElementId, Hsla, IntoElement, LayoutId, PaintQuad, Pixels, Point, SharedString, Size,
    Style, TextRun, UnderlineStyle, Window, WrappedLine, fill, point, px, relative, size,
};
use std::ops::Range;

// ---------------------------------------------------------------------------
// 描画
// ---------------------------------------------------------------------------

/// 直前の描画で確定した行の字形。
pub(super) struct InputLayout {
    /// 論理行ごとの (字形, 行頭のバイト位置)。
    lines: Vec<(WrappedLine, usize)>,
    pub(super) line_height: Pixels,
    pub(super) bounds: Bounds<Pixels>,
    /// 表示の先頭にある折り返し行の番号。
    scroll_rows: usize,
}

impl InputLayout {
    fn rows_of(line: &WrappedLine) -> usize {
        line.wrap_boundaries().len() + 1
    }

    pub(super) fn scroll_y(&self) -> Pixels {
        self.line_height * self.scroll_rows as f32
    }

    fn total_rows(&self) -> usize {
        self.lines
            .iter()
            .map(|(line, _)| Self::rows_of(line))
            .sum::<usize>()
            .max(1)
    }

    /// バイト位置 → テキスト座標系の点 (本文先頭の左上が原点)。
    pub(super) fn point_for_offset(&self, offset: usize) -> Option<Point<Pixels>> {
        let mut y = px(0.);
        for (line, start) in &self.lines {
            let end = start + line.text.len();
            if offset <= end {
                let local = line.position_for_index(offset - start, self.line_height)?;
                return Some(point(local.x, y + local.y));
            }
            y += self.line_height * Self::rows_of(line) as f32;
        }
        None
    }

    /// テキスト座標系の点 → バイト位置。
    pub(super) fn offset_for_point(&self, position: Point<Pixels>) -> usize {
        let mut y = px(0.);
        let mut last_end = 0;
        for (line, start) in &self.lines {
            let height = self.line_height * Self::rows_of(line) as f32;
            last_end = start + line.text.len();
            if position.y < y + height {
                let local = point(position.x, (position.y - y).max(px(0.)));
                let index = line
                    .closest_index_for_position(local, self.line_height)
                    .unwrap_or_else(|index| index);
                return start + index;
            }
            y += height;
        }
        last_end
    }
}

/// 本文を字形化する。改行で論理行に割れる。
fn shape_input(
    window: &mut Window,
    text: &SharedString,
    runs: &[TextRun],
    font_size: Pixels,
    wrap_width: Option<Pixels>,
) -> Vec<(WrappedLine, usize)> {
    let Ok(lines) =
        window
            .text_system()
            .shape_text(text.clone(), font_size, runs, wrap_width, None)
    else {
        return Vec::new();
    };
    let mut start = 0;
    lines
        .into_iter()
        .map(|line| {
            let entry = (line, start);
            // 改行 1 バイトぶんを足して次の行頭へ進む。
            start += entry.0.text.len() + 1;
            entry
        })
        .collect()
}

/// 未確定 (IME 変換中) の部分にだけ下線を引く描画指定。
fn marked_runs(text: &str, font: Font, color: Hsla, marked: Option<&Range<usize>>) -> Vec<TextRun> {
    let base = TextRun {
        len: text.len(),
        font,
        color,
        background_color: None,
        underline: None,
        strikethrough: None,
    };
    let Some(marked) = marked.filter(|range| range.end <= text.len() && range.start < range.end)
    else {
        return vec![base];
    };
    vec![
        TextRun {
            len: marked.start,
            ..base.clone()
        },
        TextRun {
            len: marked.end - marked.start,
            underline: Some(UnderlineStyle {
                color: Some(color),
                thickness: px(1.),
                wavy: false,
            }),
            ..base.clone()
        },
        TextRun {
            len: text.len() - marked.end,
            ..base
        },
    ]
    .into_iter()
    .filter(|run| run.len > 0)
    .collect()
}

/// 範囲を覆う帯。折り返しと改行を跨ぐので 1 行ずつ作る。
fn range_quads(
    layout: &InputLayout,
    range: Range<usize>,
    bounds: Bounds<Pixels>,
    color: Hsla,
) -> Vec<PaintQuad> {
    let mut quads = Vec::new();
    let (Some(start), Some(end)) = (
        layout.point_for_offset(range.start),
        layout.point_for_offset(range.end),
    ) else {
        return quads;
    };
    let scroll_y = layout.scroll_y();
    let mut y = start.y;
    while y <= end.y {
        let left = if y == start.y { start.x } else { px(0.) };
        let right = if y == end.y { end.x } else { bounds.size.width };
        if right > left {
            quads.push(fill(
                Bounds::new(
                    point(bounds.left() + left, bounds.top() + y - scroll_y),
                    size(right - left, layout.line_height),
                ),
                color,
            ));
        }
        y += layout.line_height;
    }
    quads
}

/// 入力欄の本文を描くカスタム要素。
///
/// `div` に文字列を入れるだけではキャレットも選択も描けないので自前で組む。
pub(super) struct TextInputElement {
    pub(super) input: Entity<TextInput>,
}

pub(super) struct TextInputPrepaint {
    lines: Vec<(WrappedLine, usize)>,
    line_height: Pixels,
    scroll_rows: usize,
    cursor: Option<PaintQuad>,
    selections: Vec<PaintQuad>,
}

impl IntoElement for TextInputElement {
    type Element = Self;

    fn into_element(self) -> Self::Element {
        self
    }
}

impl Element for TextInputElement {
    type RequestLayoutState = ();
    type PrepaintState = TextInputPrepaint;

    fn id(&self) -> Option<ElementId> {
        None
    }

    fn source_location(&self) -> Option<&'static core::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&gpui::InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, Self::RequestLayoutState) {
        let mut style = Style::default();
        style.size.width = relative(1.).into();

        let input = self.input.read(cx);
        if !input.config.multi_line {
            style.size.height = window.line_height().into();
            return (window.request_layout(style, [], cx), ());
        }

        let line_height = window.line_height();
        let font = window.text_style().font();
        let font_size = window.text_style().font_size.to_pixels(window.rem_size());
        let min_rows = input.config.min_rows;
        let max_rows = input.config.max_rows;
        let entity = self.input.clone();
        // 折り返しの数は幅が決まらないと分からないので、測定つきで頼む。
        let layout_id =
            window.request_measured_layout(style, move |known, available, window, cx| {
                let wrap_width = known.width.or(match available.width {
                    AvailableSpace::Definite(width) => Some(width),
                    _ => None,
                });
                let text = entity.read(cx).display_text();
                let runs = marked_runs(&text, font.clone(), gpui::black(), None);
                let lines = shape_input(window, &text, &runs, font_size, wrap_width);
                let rows: usize = lines
                    .iter()
                    .map(|(line, _)| InputLayout::rows_of(line))
                    .sum();
                let rows = rows.max(min_rows).min(max_rows.unwrap_or(usize::MAX));
                Size {
                    width: wrap_width.unwrap_or(px(0.)),
                    height: line_height * rows.max(1) as f32,
                }
            });
        (layout_id, ())
    }

    fn prepaint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&gpui::InspectorElementId>,
        bounds: Bounds<Pixels>,
        _: &mut Self::RequestLayoutState,
        window: &mut Window,
        cx: &mut App,
    ) -> Self::PrepaintState {
        let (cursor_color, selection_color, placeholder_color) = {
            let theme = theme(cx);
            (theme.cursor, theme.selection, theme.text_faint)
        };
        let style = window.text_style();
        let font = style.font();
        let font_size = style.font_size.to_pixels(window.rem_size());
        let line_height = window.line_height();

        let input = self.input.read(cx);
        let multi_line = input.config.multi_line;
        let text = input.display_text();
        let is_placeholder = input.state.text.is_empty();
        let selected_range = input.state.selected_range.clone();
        let cursor_offset = input.state.cursor();
        // 案内文の上に下線を引いても意味が無いので、本文があるときだけ渡す。
        let marked = (!is_placeholder)
            .then(|| input.state.marked_range.clone())
            .flatten();

        let color = if is_placeholder {
            placeholder_color
        } else {
            style.color
        };
        let runs = marked_runs(&text, font, color, marked.as_ref());
        let wrap_width = multi_line.then_some(bounds.size.width);
        let lines = shape_input(window, &text, &runs, font_size, wrap_width);

        let mut layout = InputLayout {
            lines,
            line_height,
            bounds,
            scroll_rows: 0,
        };

        // キャレットが枠に入るまで表示開始行をずらす。入力が伸びても打っている行が見える。
        let visible_rows =
            ((f32::from(bounds.size.height) / f32::from(line_height)).floor() as usize).max(1);
        let cursor_point = layout.point_for_offset(if is_placeholder { 0 } else { cursor_offset });
        let cursor_row = cursor_point
            .map(|p| (f32::from(p.y) / f32::from(line_height)).round() as usize)
            .unwrap_or(0);
        let max_scroll = layout.total_rows().saturating_sub(visible_rows);
        layout.scroll_rows = cursor_row.saturating_sub(visible_rows - 1).min(max_scroll);
        let scroll_y = layout.scroll_y();

        let cursor = cursor_point.map(|position| {
            fill(
                Bounds::new(
                    point(
                        bounds.left() + position.x,
                        bounds.top() + position.y - scroll_y,
                    ),
                    size(px(1.5), line_height),
                ),
                cursor_color,
            )
        });

        let selections = if is_placeholder || selected_range.is_empty() {
            Vec::new()
        } else {
            range_quads(&layout, selected_range, bounds, selection_color)
        };

        TextInputPrepaint {
            lines: layout.lines,
            line_height,
            scroll_rows: layout.scroll_rows,
            cursor,
            selections,
        }
    }

    fn paint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&gpui::InspectorElementId>,
        bounds: Bounds<Pixels>,
        _: &mut Self::RequestLayoutState,
        prepaint: &mut Self::PrepaintState,
        window: &mut Window,
        cx: &mut App,
    ) {
        let focus_handle = self.input.read(cx).focus_handle.clone();
        window.handle_input(
            &focus_handle,
            ElementInputHandler::new(bounds, self.input.clone()),
            cx,
        );
        let focused = focus_handle.is_focused(window);

        // スクロールで枠の外に出た行と帯が親へ漏れないよう自前で切り抜く。
        let scroll_y = prepaint.line_height * prepaint.scroll_rows as f32;
        window.with_content_mask(Some(gpui::ContentMask { bounds }), |window| {
            for quad in prepaint.selections.drain(..) {
                window.paint_quad(quad);
            }
            let mut y = bounds.top() - scroll_y;
            for (line, _) in &prepaint.lines {
                line.paint(
                    point(bounds.left(), y),
                    prepaint.line_height,
                    gpui::TextAlign::Left,
                    Some(bounds),
                    window,
                    cx,
                )
                .ok();
                y += prepaint.line_height * InputLayout::rows_of(line) as f32;
            }
            if focused && let Some(cursor) = prepaint.cursor.take() {
                window.paint_quad(cursor);
            }
        });

        let lines = std::mem::take(&mut prepaint.lines);
        let line_height = prepaint.line_height;
        let scroll_rows = prepaint.scroll_rows;
        self.input.update(cx, |input, cx| {
            if input.focused != focused {
                input.focused = focused;
                // 枠のネオンは親が描くので、変化を親へ伝える。
                cx.emit(TextInputEvent::FocusChanged);
                cx.notify();
            }
            input.layout = Some(InputLayout {
                lines,
                line_height,
                bounds,
                scroll_rows,
            });
        });
    }
}
