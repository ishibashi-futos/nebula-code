//! グリッドの描画。
//!
//! 描画は `div` を並べずカスタム [`Element`] で行う。80x24 のグリッドを
//! `div` でセルごとに組むと 1920 要素になり、レイアウトだけで 1 フレームを
//! 使い切ってしまう。ここでは 1 行を「同じ見た目が続く区間」へ畳んで
//! 区間ごとに `shape_line` し、背景と枠だけを矩形として描く。
//!
//! 区間には **開始桁** を持たせ、`cell_width * 桁` に置く。字送りに桁位置を
//! 任せると、全角やフォールバック書体が混ざった時点で文字とカーソル矩形がずれる。
//! セル座標からピクセルへの変換 (マウス→セル、行の組み立て、色の解決) も
//! 描画に閉じた計算なので、まとめてここに置く。

use super::{GridMetrics, GridSelection, TerminalView};
use crate::theme::{Theme, metrics, theme};
use gpui::prelude::*;
use gpui::{
    App, BorderStyle, Bounds, ElementId, Entity, Font, FontStyle, FontWeight, GlobalElementId,
    Hsla, LayoutId, PaintQuad, Pixels, Point, ShapedLine, SharedString, StrikethroughStyle, Style,
    TextRun, UnderlineStyle, Window, fill, outline, point, px, relative, size,
};
use nebula_protocol::{TermColor, TerminalCell, cell_flags};

// ---------------------------------------------------------------------------
// グリッドの描画要素
// ---------------------------------------------------------------------------

pub(super) struct TerminalElement {
    pub(super) view: Entity<TerminalView>,
}

pub(super) struct TerminalPrepaint {
    /// 既定でない背景色のセル。文字より先に塗る。
    backgrounds: Vec<PaintQuad>,
    /// シェイプ済みの各行と、その左上座標。
    lines: Vec<(ShapedLine, Point<Pixels>)>,
    line_height: Pixels,
    cursor: Option<PaintQuad>,
}

impl IntoElement for TerminalElement {
    type Element = Self;

    fn into_element(self) -> Self::Element {
        self
    }
}

impl Element for TerminalElement {
    type RequestLayoutState = ();
    type PrepaintState = TerminalPrepaint;

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
        let font_size = metrics::EDITOR_FONT_SIZE;
        let line_height = px(f32::from(font_size) * metrics::LINE_HEIGHT_RATIO);
        let base_font = window.text_style().font();
        let cell_width = measure_cell_width(window, font_size, &base_font, theme.text);

        let (rows, cols) = grid_dimensions(
            f32::from(bounds.size.width),
            f32::from(bounds.size.height),
            f32::from(cell_width),
            f32::from(line_height),
        );

        let focus_handle = self.view.read(cx).focus_handle.clone();
        let focused = focus_handle.is_focused(window);

        let mut backgrounds = Vec::new();
        let mut lines = Vec::new();
        let mut cursor = None;

        // ドラッグしていない (1 セルも選んでいない) 選択はハイライトしない。
        // マウスダウン直後は anchor == head の状態で 1 フレーム挟まるため。
        let selection = self.view.read(cx).selection.filter(|s| !s.is_empty());

        if let Some(state) = self.view.read(cx).active_state() {
            let cursor_row = state.cursor_row as usize;
            let cursor_col = state.cursor_col as usize;
            // 塗りつぶしカーソルの下の文字は反転色で描く。二重描画を避けるため、
            // 文字色の差し替えとして扱い、上から glyph を重ねない。
            let inverse_cell =
                (focused && state.cursor_visible).then_some((cursor_row, cursor_col));

            for (row, cells) in state.grid.iter().enumerate().take(rows as usize) {
                let y = bounds.origin.y + line_height * row as f32;

                for (start, len, color) in background_spans(cells, &theme) {
                    backgrounds.push(fill(
                        Bounds::new(
                            point(bounds.origin.x + cell_width * start as f32, y),
                            size(cell_width * len as f32, line_height),
                        ),
                        color,
                    ));
                }

                // 選択のハイライト。背景色の上、文字の下に重ねる (下の paint 参照)。
                if let Some((from, to)) =
                    selection.and_then(|s| selection_columns_in_row(&s, row, cells.len()))
                {
                    backgrounds.push(fill(
                        Bounds::new(
                            point(bounds.origin.x + cell_width * from as f32, y),
                            size(cell_width * (to - from) as f32, line_height),
                        ),
                        theme.selection,
                    ));
                }

                let inverse_col = inverse_cell.and_then(|(r, c)| (r == row).then_some(c));
                for RowSegment {
                    start_col,
                    text,
                    run,
                } in build_row(cells, &theme, &base_font, inverse_col)
                {
                    let shaped = window.text_system().shape_line(
                        SharedString::from(text),
                        font_size,
                        &[run],
                        None,
                    );
                    lines.push((
                        shaped,
                        point(bounds.origin.x + cell_width * start_col as f32, y),
                    ));
                }
            }

            if state.cursor_visible {
                let cursor_bounds = Bounds::new(
                    point(
                        bounds.origin.x + cell_width * cursor_col as f32,
                        bounds.origin.y + line_height * cursor_row as f32,
                    ),
                    size(cell_width, line_height),
                );
                cursor = Some(if focused {
                    fill(cursor_bounds, theme.cursor)
                } else {
                    outline(cursor_bounds, theme.cursor, BorderStyle::Solid)
                });
            }
        }

        // 実寸が決まったので、必要ならバックエンドへリサイズを伝える。
        self.view.update(cx, |view, cx| {
            view.sync_size(
                GridMetrics {
                    cell_width,
                    line_height,
                    rows,
                    cols,
                    origin: bounds.origin,
                },
                cx,
            );
        });

        TerminalPrepaint {
            backgrounds,
            lines,
            line_height,
            cursor,
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
        window.paint_quad(fill(bounds, theme.bg_surface));
        for quad in prepaint.backgrounds.drain(..) {
            window.paint_quad(quad);
        }
        // 塗りつぶしカーソルは文字の下。文字側が反転色で描かれるので重ならない。
        let outlined = prepaint
            .cursor
            .as_ref()
            .map(|quad| quad.background.is_transparent())
            .unwrap_or(false);
        if !outlined && let Some(quad) = prepaint.cursor.clone() {
            window.paint_quad(quad);
        }
        for (line, origin) in prepaint.lines.iter() {
            line.paint(*origin, prepaint.line_height, window, cx).ok();
        }
        if outlined && let Some(quad) = prepaint.cursor.take() {
            window.paint_quad(quad);
        }
    }
}

/// 等幅フォントの 1 文字ぶんの送り幅。
fn measure_cell_width(window: &mut Window, font_size: Pixels, font: &Font, color: Hsla) -> Pixels {
    let width = window
        .text_system()
        .shape_line(
            SharedString::from("M"),
            font_size,
            &[TextRun {
                len: 1,
                font: font.clone(),
                color,
                background_color: None,
                underline: None,
                strikethrough: None,
            }],
            None,
        )
        .width;
    // 0 幅だと桁数計算がゼロ除算になる。フォント未解決時の保険。
    if f32::from(width) > 0.0 {
        width
    } else {
        px(8.)
    }
}

// ---------------------------------------------------------------------------
// 純粋関数 (描画から切り離してテストする)
// ---------------------------------------------------------------------------

/// 要素の実寸から端末の行数・桁数を求める。
fn grid_dimensions(width: f32, height: f32, cell_width: f32, line_height: f32) -> (u16, u16) {
    if cell_width <= 0.0 || line_height <= 0.0 {
        return (24, 80);
    }
    let cols = (width / cell_width).floor().clamp(1.0, u16::MAX as f32) as u16;
    let rows = (height / line_height).floor().clamp(1.0, u16::MAX as f32) as u16;
    (rows, cols)
}

/// マウス座標 (ウィンドウ基準) をグリッドのセル位置 (行, 桁) に変換する。
///
/// セルの寸法は等幅フォント前提で一定 — 全角文字が来ても字送りが変わるだけで
/// グリッド上の桁幅そのものは変わらない (ファイル冒頭のコメント参照)。
/// 領域の外に出ても呼び出し側が扱いやすいよう、常に有効な行・桁へ丸める
/// (負値は 0 へ、右端・下端を超える値は最終桁・最終行へ)。
pub(super) fn position_to_cell(x: f32, y: f32, metrics: &GridMetrics) -> (usize, usize) {
    let cell_width = f32::from(metrics.cell_width);
    let line_height = f32::from(metrics.line_height);
    if cell_width <= 0.0 || line_height <= 0.0 || metrics.rows == 0 || metrics.cols == 0 {
        return (0, 0);
    }
    let col = ((x - f32::from(metrics.origin.x)) / cell_width)
        .floor()
        .max(0.0) as usize;
    let row = ((y - f32::from(metrics.origin.y)) / line_height)
        .floor()
        .max(0.0) as usize;
    (
        row.min(metrics.rows as usize - 1),
        col.min(metrics.cols as usize - 1),
    )
}

/// 変化した行をグリッドへ反映する。
///
/// 端末が拡がった直後は、まだ GUI 側に存在しない行番号が届きうる。足りなければ伸ばす。
pub(super) fn apply_dirty_lines(
    grid: &mut Vec<Vec<TerminalCell>>,
    dirty: &[(u16, Vec<TerminalCell>)],
) {
    for (row, cells) in dirty {
        let row = *row as usize;
        if grid.len() <= row {
            grid.resize(row + 1, Vec::new());
        }
        grid[row] = cells.clone();
    }
}

/// INVERSE と DIM を適用したあとの (前景色, 背景色)。
///
/// 反転は **色を解決したあと** に行う。`TermColor::Default` のまま入れ替えると
/// 既定色どうしの交換になり、反転が効かない。
fn resolved_colors(cell: &TerminalCell, theme: &Theme) -> (Hsla, Hsla) {
    let mut fg = theme.term_color(cell.fg, false);
    let mut bg = theme.term_color(cell.bg, true);
    if cell.flags & cell_flags::INVERSE != 0 {
        std::mem::swap(&mut fg, &mut bg);
    }
    if cell.flags & cell_flags::DIM != 0 {
        fg.a *= 0.6;
    }
    (fg, bg)
}

/// 既定でない背景色が続く区間を (開始桁, 桁数, 色) で返す。
///
/// セルごとに矩形を積むと 1 行で桁数ぶんの draw call になるので、隣り合う同色を畳む。
fn background_spans(cells: &[TerminalCell], theme: &Theme) -> Vec<(usize, usize, Hsla)> {
    let default_bg = theme.term_color(TermColor::Default, true);
    let mut spans: Vec<(usize, usize, Hsla)> = Vec::new();
    let mut current: Option<(usize, usize, Hsla)> = None;
    for (col, cell) in cells.iter().enumerate() {
        let (_, bg) = resolved_colors(cell, theme);
        if bg == default_bg {
            if let Some(span) = current.take() {
                spans.push(span);
            }
            continue;
        }
        current = match current {
            Some((start, len, color)) if color == bg => Some((start, len + 1, color)),
            Some(span) => {
                spans.push(span);
                Some((col, 1, bg))
            }
            None => Some((col, 1, bg)),
        };
    }
    if let Some(span) = current {
        spans.push(span);
    }
    spans
}

/// 行末まで続く「素の空白」を除いた描画対象セル数。
///
/// 端末のグリッドは常に桁数ぶん埋まっているため、切り詰めないと 1 行につき
/// 桁数ぶんの空白をシェイプすることになる。
fn visible_len(cells: &[TerminalCell]) -> usize {
    cells
        .iter()
        .rposition(|cell| !(cell.ch == ' ' && cell.flags == 0))
        .map(|last| last + 1)
        .unwrap_or(0)
}

/// 正規化した選択のうち、指定した行に属する桁範囲 [開始, 終了) を返す。
/// その行が選択に含まれなければ `None`。
///
/// 最初の行は選択開始の桁から行末まで、最後の行は行頭から選択終了の桁まで、
/// 間の行は全桁を選ぶ — 複数行にまたがる選択の一般的な挙動に合わせている。
/// 選択のハイライト描画とコピー用テキストの抽出の両方から使う共通ロジック。
fn selection_columns_in_row(
    selection: &GridSelection,
    row: usize,
    row_len: usize,
) -> Option<(usize, usize)> {
    let (start_row, start_col, end_row, end_col) = selection.normalized();
    if row < start_row || row > end_row || row_len == 0 {
        return None;
    }
    let from = if row == start_row {
        start_col.min(row_len)
    } else {
        0
    };
    let to = if row == end_row {
        (end_col + 1).min(row_len)
    } else {
        row_len
    };
    (from < to).then_some((from, to))
}

/// 選択範囲のセルからコピー用のテキストを取り出す。
///
/// 各行とも、端末が桁数ぶん埋めている行末の空白セルは含めない
/// (`visible_len` と同じ判定)。行の途中や行頭の空白はそのまま残す —
/// 削るのはあくまで「実際には打たれていない行末の埋め草」だけ。
/// 全角文字の後続セルは中身が無い (常に空白) ので読み飛ばす。
/// 複数行にまたがる選択は行の間を改行でつなぐ。
pub(super) fn extract_selected_text(
    grid: &[Vec<TerminalCell>],
    selection: &GridSelection,
) -> String {
    let (start_row, _, end_row, _) = selection.normalized();
    (start_row..=end_row)
        .map(|row| {
            let Some(cells) = grid.get(row) else {
                return String::new();
            };
            let Some((from, to)) = selection_columns_in_row(selection, row, cells.len()) else {
                return String::new();
            };
            let content_end = visible_len(cells).min(to);
            if from >= content_end {
                return String::new();
            }
            cells[from..content_end]
                .iter()
                .filter(|c| c.flags & cell_flags::WIDE_TRAILER == 0)
                .map(|c| c.ch)
                .collect()
        })
        .collect::<Vec<String>>()
        .join("\n")
}

/// 1 行を桁位置つきの描画区間へ分割したうちの 1 つ。
///
/// 開始桁を持つので、描画側は `cell_width * start_col` に置くだけでよい。
/// 見た目が変わる位置と全角セルで区間を切るため、区間の中身は常に 1 ラン。
struct RowSegment {
    /// この区間が始まる桁。
    start_col: usize,
    text: String,
    run: TextRun,
}

/// 1 行ぶんの描画区間を組み立てる。
///
/// `TextRun::len` は **バイト数**。同じ色・同じ装飾が続くセルは 1 区間に畳み、
/// 見た目が変わる位置と全角セルの前後で切る。全角セルを単独の区間にするのは、
/// 代替書体で描かれたときの送り幅が 2 桁ぶんと一致する保証が無いため。
/// `inverse_col` にはカーソルが乗っている桁を渡す。その桁だけ反転色で描く。
fn build_row(
    cells: &[TerminalCell],
    theme: &Theme,
    base_font: &Font,
    inverse_col: Option<usize>,
) -> Vec<RowSegment> {
    let end = visible_len(cells);
    let mut segments: Vec<RowSegment> = Vec::new();
    let mut current: Option<SegmentBuilder> = None;

    for (col, cell) in cells.iter().enumerate().take(end) {
        // 全角の後続セルには文字が無い。読み飛ばさないと桁がずれる。
        if cell.flags & cell_flags::WIDE_TRAILER != 0 {
            continue;
        }
        let (fg, _) = resolved_colors(cell, theme);
        let style = RunStyle {
            color: if inverse_col == Some(col) {
                theme.text_inverse
            } else {
                fg
            },
            bold: cell.flags & cell_flags::BOLD != 0,
            italic: cell.flags & cell_flags::ITALIC != 0,
            underline: cell.flags & cell_flags::UNDERLINE != 0,
            strikethrough: cell.flags & cell_flags::STRIKETHROUGH != 0,
        };
        // 制御文字がそのまま届いた場合に備えて空白へ落とす。
        let ch = if cell.ch.is_control() { ' ' } else { cell.ch };
        let wide = cells
            .get(col + 1)
            .is_some_and(|next| next.flags & cell_flags::WIDE_TRAILER != 0);

        // 見た目が同じあいだは畳む。空白が続くだけの区間も同じ扱い。
        // 全角セルは前後で切るので、畳んでいるあいだ桁は必ず 1 つずつ進む。
        if !wide
            && let Some(builder) = current.as_mut()
            && builder.style == style
        {
            builder.push(ch);
            continue;
        }

        if let Some(builder) = current.take() {
            segments.push(builder.build(base_font));
        }
        let mut builder = SegmentBuilder::new(col, style);
        builder.push(ch);
        if wide {
            segments.push(builder.build(base_font));
        } else {
            current = Some(builder);
        }
    }
    if let Some(builder) = current {
        segments.push(builder.build(base_font));
    }
    segments
}

/// 組み立て中の区間。
struct SegmentBuilder {
    start_col: usize,
    style: RunStyle,
    text: String,
}

impl SegmentBuilder {
    fn new(start_col: usize, style: RunStyle) -> Self {
        Self {
            start_col,
            style,
            text: String::new(),
        }
    }

    fn push(&mut self, ch: char) {
        self.text.push(ch);
    }

    fn build(self, base_font: &Font) -> RowSegment {
        let run = make_run(&self.style, self.text.len(), base_font);
        RowSegment {
            start_col: self.start_col,
            text: self.text,
            run,
        }
    }
}

/// 1 ランぶんの見た目。同値なら畳めるかどうかの判定に使う。
#[derive(Clone, PartialEq)]
struct RunStyle {
    color: Hsla,
    bold: bool,
    italic: bool,
    underline: bool,
    strikethrough: bool,
}

fn make_run(style: &RunStyle, len: usize, base_font: &Font) -> TextRun {
    let mut font = base_font.clone();
    if style.bold {
        font.weight = FontWeight::BOLD;
    }
    if style.italic {
        font.style = FontStyle::Italic;
    }
    TextRun {
        len,
        font,
        color: style.color,
        background_color: None,
        underline: style.underline.then(|| UnderlineStyle {
            color: Some(style.color),
            thickness: px(1.),
            wavy: false,
        }),
        strikethrough: style.strikethrough.then(|| StrikethroughStyle {
            thickness: px(1.),
            color: Some(style.color),
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cell(ch: char) -> TerminalCell {
        TerminalCell {
            ch,
            ..TerminalCell::default()
        }
    }

    fn colored(ch: char, fg: TermColor) -> TerminalCell {
        TerminalCell {
            ch,
            fg,
            ..TerminalCell::default()
        }
    }

    // -- グリッド --

    #[test]
    fn 差分行がグリッドへ反映される() {
        let mut grid = vec![vec![cell('a')], vec![cell('b')]];
        apply_dirty_lines(&mut grid, &[(1, vec![cell('z')])]);
        assert_eq!(grid[1][0].ch, 'z');
        assert_eq!(grid[0][0].ch, 'a', "触れていない行は残る");
    }

    #[test]
    fn 未知の行番号が来たらグリッドを伸ばす() {
        let mut grid = vec![vec![cell('a')]];
        apply_dirty_lines(&mut grid, &[(3, vec![cell('x')])]);
        assert_eq!(grid.len(), 4);
        assert_eq!(grid[3][0].ch, 'x');
        assert!(grid[2].is_empty(), "隙間は空行で埋まる");
    }

    #[test]
    fn 実寸から桁数と行数を求める() {
        assert_eq!(grid_dimensions(800.0, 480.0, 8.0, 20.0), (24, 100));
        assert_eq!(
            grid_dimensions(7.0, 5.0, 8.0, 20.0),
            (1, 1),
            "1 桁未満でも 0 にはしない"
        );
        assert_eq!(
            grid_dimensions(800.0, 480.0, 0.0, 20.0),
            (24, 80),
            "寸法が取れないときは既定値"
        );
    }

    // -- 選択 (マウス座標 -> セル、セル -> コピー用テキスト) --

    /// `position_to_cell` のテストで共通して使う寸法 (原点 (100, 50)、1 セル 8x16、24 行 80 列)。
    fn テスト用メトリクス() -> GridMetrics {
        GridMetrics {
            cell_width: px(8.0),
            line_height: px(16.0),
            rows: 24,
            cols: 80,
            origin: point(px(100.0), px(50.0)),
        }
    }

    #[test]
    fn 左上の角ちょうどは_0_行_0_列になる() {
        assert_eq!(position_to_cell(100.0, 50.0, &テスト用メトリクス()), (0, 0));
    }

    #[test]
    fn 右下の最終セルちょうどは最終行最終列になる() {
        // 80 列 24 行なら最終セルは列 79・行 23。その左上ぴったりを指す。
        let x = 100.0 + 8.0 * 79.0;
        let y = 50.0 + 16.0 * 23.0;
        assert_eq!(position_to_cell(x, y, &テスト用メトリクス()), (23, 79));
    }

    #[test]
    fn 領域より左上の負値は_0_行_0_列に丸められる() {
        // 原点 (100, 50) より左上の座標を渡す。相対位置が負になるケース。
        assert_eq!(position_to_cell(0.0, 0.0, &テスト用メトリクス()), (0, 0));
    }

    #[test]
    fn 領域より右下の大きすぎる値は最終行最終列に丸められる() {
        let x = 100.0 + 8.0 * 1000.0;
        let y = 50.0 + 16.0 * 1000.0;
        assert_eq!(position_to_cell(x, y, &テスト用メトリクス()), (23, 79));
    }

    #[test]
    fn 全角文字の後続セルの範囲内をクリックしてもそのセルの列になる() {
        // 全角文字は 2 列を占めるが、字送り幅そのものは列ごとに一定
        // (ファイル冒頭のコメント参照)。後続セル (列 1) の範囲内なら列 1 が返ればよい。
        let x = 100.0 + 8.0 * 1.0 + 4.0;
        assert_eq!(position_to_cell(x, 50.0, &テスト用メトリクス()), (0, 1));
    }

    /// テスト用のグリッド。"hello world" の 1 行だけ。
    fn hello_world_row() -> Vec<Vec<TerminalCell>> {
        vec!["hello world".chars().map(cell).collect()]
    }

    #[test]
    fn 一行内の部分選択を取り出す() {
        let grid = hello_world_row();
        let selection = GridSelection {
            anchor_row: 0,
            anchor_col: 2,
            head_row: 0,
            head_col: 6,
        };
        assert_eq!(extract_selected_text(&grid, &selection), "llo w");
    }

    #[test]
    fn 右から左へドラッグしても同じ範囲になる() {
        // head が anchor より前に来ても (右から左へのドラッグ)、
        // 選ばれる文字は座標の前後を入れ替えたときと同じでなければならない。
        let grid = hello_world_row();
        let selection = GridSelection {
            anchor_row: 0,
            anchor_col: 6,
            head_row: 0,
            head_col: 2,
        };
        assert_eq!(extract_selected_text(&grid, &selection), "llo w");
    }

    #[test]
    fn 複数行の選択は改行でつなぐ() {
        let grid = vec![
            vec![cell('f'), cell('o'), cell('o')],
            vec![cell('b'), cell('a'), cell('r')],
        ];
        // 1 行目は開始桁から行末まで、2 行目は行頭から終了桁まで。
        let selection = GridSelection {
            anchor_row: 0,
            anchor_col: 1,
            head_row: 1,
            head_col: 1,
        };
        assert_eq!(extract_selected_text(&grid, &selection), "oo\nba");
    }

    #[test]
    fn 行末の埋め草の空白は取り除かれる() {
        let grid = vec![vec![cell('h'), cell('i'), cell(' '), cell(' '), cell(' ')]];
        // 行末まで選んでも、実際には打たれていない埋め草の空白は含めない。
        let selection = GridSelection {
            anchor_row: 0,
            anchor_col: 0,
            head_row: 0,
            head_col: 4,
        };
        assert_eq!(extract_selected_text(&grid, &selection), "hi");
    }

    #[test]
    fn 全角文字を含む行は後続セルを除いて連結する() {
        let grid = vec![vec![cell('あ'), wide_trailer(), cell('b')]];
        let selection = GridSelection {
            anchor_row: 0,
            anchor_col: 0,
            head_row: 0,
            head_col: 2,
        };
        assert_eq!(extract_selected_text(&grid, &selection), "あb");
    }

    #[test]
    fn 何も打たれていない行の選択は空文字列になる() {
        let grid = vec![vec![cell(' '), cell(' '), cell(' ')]];
        let selection = GridSelection {
            anchor_row: 0,
            anchor_col: 0,
            head_row: 0,
            head_col: 2,
        };
        assert_eq!(extract_selected_text(&grid, &selection), "");
    }

    // -- 色 --

    #[test]
    fn inverse_は解決後の色を入れ替える() {
        let theme = Theme::cyber_cosmic();
        let plain = TerminalCell::default();
        let inverse = TerminalCell {
            flags: cell_flags::INVERSE,
            ..TerminalCell::default()
        };
        let (fg, bg) = resolved_colors(&plain, &theme);
        let (inv_fg, inv_bg) = resolved_colors(&inverse, &theme);
        assert_eq!((fg, bg), (inv_bg, inv_fg), "前景と背景が入れ替わる");
        assert_ne!(fg, inv_fg, "既定色どうしでも反転が効く");
    }

    #[test]
    fn dim_は前景を薄くする() {
        let theme = Theme::cyber_cosmic();
        let dim = TerminalCell {
            flags: cell_flags::DIM,
            ..TerminalCell::default()
        };
        let (fg, _) = resolved_colors(&dim, &theme);
        let (plain_fg, _) = resolved_colors(&TerminalCell::default(), &theme);
        assert!(fg.a < plain_fg.a);
    }

    #[test]
    fn 既定背景のセルは塗らない() {
        let theme = Theme::cyber_cosmic();
        let cells = vec![cell('a'), cell('b')];
        assert!(background_spans(&cells, &theme).is_empty());
    }

    #[test]
    fn 同色の背景は_1_つの区間に畳まれる() {
        let theme = Theme::cyber_cosmic();
        let red = TerminalCell {
            ch: ' ',
            bg: TermColor::Indexed(1),
            ..TerminalCell::default()
        };
        let cells = vec![cell('a'), red, red, cell('b'), red];
        let spans = background_spans(&cells, &theme);
        assert_eq!(spans.len(), 2);
        assert_eq!((spans[0].0, spans[0].1), (1, 2));
        assert_eq!((spans[1].0, spans[1].1), (4, 1));
    }

    // -- 行の組み立て --

    #[test]
    fn 行末の空白はシェイプしない() {
        let cells = vec![cell('h'), cell('i'), cell(' '), cell(' ')];
        assert_eq!(visible_len(&cells), 2);
        assert_eq!(visible_len(&[cell(' '), cell(' ')]), 0);
    }

    fn wide_trailer() -> TerminalCell {
        TerminalCell {
            ch: ' ',
            flags: cell_flags::WIDE_TRAILER,
            ..TerminalCell::default()
        }
    }

    #[test]
    fn 同色のセルが_1_区間に畳まれる() {
        let theme = Theme::cyber_cosmic();
        let font = gpui::font(metrics::MONO_FONT_FAMILY);
        let cells = vec![
            colored('a', TermColor::Indexed(2)),
            colored('b', TermColor::Indexed(2)),
            colored('c', TermColor::Indexed(5)),
        ];
        let segments = build_row(&cells, &theme, &font, None);
        assert_eq!(segments.len(), 2, "色が変わるところでだけ区間が切れる");
        assert_eq!(
            (segments[0].start_col, segments[0].text.as_str()),
            (0, "ab")
        );
        assert_eq!(
            (segments[1].start_col, segments[1].text.as_str()),
            (2, "c"),
            "2 つ目の区間は色が変わった桁から始まる"
        );
    }

    #[test]
    fn 連続する空白は区間を増やさない() {
        let theme = Theme::cyber_cosmic();
        let font = gpui::font(metrics::MONO_FONT_FAMILY);
        let cells = vec![cell('a'), cell(' '), cell(' '), cell('b')];
        let segments = build_row(&cells, &theme, &font, None);
        assert_eq!(segments.len(), 1);
        assert_eq!(
            (segments[0].start_col, segments[0].text.as_str()),
            (0, "a  b")
        );
    }

    #[test]
    fn 区間のラン長は文字列のバイト数に一致する() {
        let theme = Theme::cyber_cosmic();
        let font = gpui::font(metrics::MONO_FONT_FAMILY);
        let cells = vec![
            cell('あ'),
            wide_trailer(),
            cell('a'),
            colored('い', TermColor::Indexed(4)),
            wide_trailer(),
        ];
        let segments = build_row(&cells, &theme, &font, None);
        for segment in &segments {
            assert_eq!(
                segment.run.len,
                segment.text.len(),
                "{} の区間",
                segment.start_col
            );
        }
        let total: usize = segments.iter().map(|s| s.text.len()).sum();
        assert_eq!(total, 7, "3+1+3 バイト");
    }

    #[test]
    fn 全角セルは単独の区間になり次の区間は_2_桁進む() {
        let theme = Theme::cyber_cosmic();
        let font = gpui::font(metrics::MONO_FONT_FAMILY);
        let cells = vec![cell('a'), cell('あ'), wide_trailer(), cell('x'), cell('y')];
        let segments = build_row(&cells, &theme, &font, None);
        assert_eq!(segments.len(), 3);
        assert_eq!((segments[0].start_col, segments[0].text.as_str()), (0, "a"));
        assert_eq!(
            (segments[1].start_col, segments[1].text.as_str()),
            (1, "あ"),
            "全角セルは前後から切り離す"
        );
        assert_eq!(
            (segments[2].start_col, segments[2].text.as_str()),
            (3, "xy"),
            "WIDE_TRAILER を読み飛ばしても桁は 2 つ進む"
        );
    }

    #[test]
    fn カーソル下の文字は反転色で描く() {
        let theme = Theme::cyber_cosmic();
        let font = gpui::font(metrics::MONO_FONT_FAMILY);
        let cells = vec![cell('a'), cell('b'), cell('c')];
        let segments = build_row(&cells, &theme, &font, Some(1));
        assert_eq!(segments.len(), 3, "カーソル桁だけ色が変わるので区間が 3 つ");
        assert_eq!((segments[1].start_col, segments[1].text.as_str()), (1, "b"));
        assert_eq!(segments[1].run.color, theme.text_inverse);
        assert_ne!(segments[0].run.color, theme.text_inverse);
        assert_ne!(segments[2].run.color, theme.text_inverse);
    }

    #[test]
    fn 装飾フラグがランに反映される() {
        let theme = Theme::cyber_cosmic();
        let font = gpui::font(metrics::MONO_FONT_FAMILY);
        let decorated = TerminalCell {
            ch: 'x',
            flags: cell_flags::BOLD | cell_flags::UNDERLINE | cell_flags::STRIKETHROUGH,
            ..TerminalCell::default()
        };
        let segments = build_row(&[decorated], &theme, &font, None);
        assert_eq!(segments[0].run.font.weight, FontWeight::BOLD);
        assert!(segments[0].run.underline.is_some());
        assert!(segments[0].run.strikethrough.is_some());
    }
}
