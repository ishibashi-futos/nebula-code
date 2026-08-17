//! 統合ターミナル。
//!
//! PTY と ANSI 解釈はバックエンドが担う。GUI が持つのは可視グリッド
//! (`Vec<Vec<TerminalCell>>`) だけで、バックエンドは **変化した行だけ** を
//! [`Event::TerminalUpdated`] で送ってくる。
//!
//! 描画は `div` を並べずカスタム [`Element`] で行う。80x24 のグリッドを
//! `div` でセルごとに組むと 1920 要素になり、レイアウトだけで 1 フレームを
//! 使い切ってしまう。ここでは 1 行を 1 回の `shape_line` に畳み、
//! 背景と枠だけを矩形として描く。

use crate::assets::Icon;
use crate::ipc_client::BackendClient;
use crate::theme::{Theme, metrics, theme};
use crate::ui::{focus_border, h_flex, icon, primary_button, truncate_middle, v_flex};
use gpui::prelude::*;
use gpui::{
    AnyElement, App, BorderStyle, Bounds, Context, ElementId, Entity, FocusHandle, Focusable, Font,
    FontStyle, FontWeight, GlobalElementId, Hsla, KeyDownEvent, Keystroke, LayoutId, MouseButton,
    MouseDownEvent, PaintQuad, Pixels, Point, ScrollWheelEvent, ShapedLine, SharedString,
    StrikethroughStyle, Style, TextRun, UnderlineStyle, Window, div, fill, outline, point, px,
    relative, size,
};
use nebula_protocol::{
    Event, Request, Response, TermColor, TerminalCell, TerminalId, TerminalSpec, WorkspaceInfo,
    cell_flags,
};
use std::collections::HashMap;

// ---------------------------------------------------------------------------
// 状態
// ---------------------------------------------------------------------------

/// 1 つの PTY セッションぶんの表示状態。
pub struct TerminalState {
    /// 端末が設定したタイトル (OSC 0/2)。未設定なら空。
    title: String,
    /// 可視領域のセル。行数・桁数は端末側の都合で増減しうるので固定長にしない。
    grid: Vec<Vec<TerminalCell>>,
    cursor_row: u16,
    cursor_col: u16,
    cursor_visible: bool,
    scrollback_len: usize,
    /// プロセスが終了したか。終了後もグリッドは残して読めるようにする。
    exited: bool,
    exit_code: Option<i32>,
    /// 最後にバックエンドへ伝えた (行数, 桁数)。
    ///
    /// 応答ではなく **送信時点** を覚える。応答待ちのあいだも「未送信」と見なすと
    /// 毎フレーム同じリサイズを送り続けてしまうため。
    sent_size: (u16, u16),
}

impl TerminalState {
    fn new(rows: u16, cols: u16) -> Self {
        Self {
            title: String::new(),
            grid: vec![vec![TerminalCell::default(); cols as usize]; rows as usize],
            cursor_row: 0,
            cursor_col: 0,
            cursor_visible: true,
            scrollback_len: 0,
            exited: false,
            exit_code: None,
            sent_size: (rows, cols),
        }
    }
}

/// 直前の描画で確定した 1 文字の寸法とグリッドの大きさ。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct GridMetrics {
    cell_width: Pixels,
    line_height: Pixels,
    rows: u16,
    cols: u16,
}

pub struct TerminalView {
    client: Option<BackendClient>,
    workspace: Option<WorkspaceInfo>,
    terminals: HashMap<TerminalId, TerminalState>,
    /// タブの並び。`HashMap` は順序を持たないので別に保つ。
    order: Vec<TerminalId>,
    active: Option<TerminalId>,
    grid_metrics: Option<GridMetrics>,
    /// 作成要求が飛んでいるか。連打で端末が増えるのを防ぐ。
    creating: bool,
    /// 送信待ちの入力バイト列。
    ///
    /// バックエンドは要求ごとに別タスクで処理するため、並行して投げると
    /// PTY への書き込み順が入れ替わる。打鍵順が命なので 1 件ずつ送る。
    pending_input: Vec<(TerminalId, Vec<u8>)>,
    sending_input: bool,
    focus_handle: FocusHandle,
}

impl TerminalView {
    pub fn new(cx: &mut Context<Self>) -> Self {
        Self {
            client: None,
            workspace: None,
            terminals: HashMap::new(),
            order: Vec::new(),
            active: None,
            grid_metrics: None,
            creating: false,
            pending_input: Vec::new(),
            sending_input: false,
            focus_handle: cx.focus_handle(),
        }
    }

    pub fn set_client(&mut self, client: BackendClient, cx: &mut Context<Self>) {
        self.client = Some(client);
        cx.notify();
    }

    pub fn set_workspace(&mut self, workspace: WorkspaceInfo, cx: &mut Context<Self>) {
        self.workspace = Some(workspace);
        cx.notify();
    }

    pub fn handle_event(&mut self, event: &Event, cx: &mut Context<Self>) {
        match event {
            Event::TerminalUpdated(update) => {
                let Some(state) = self.terminals.get_mut(&update.id) else {
                    return;
                };
                apply_dirty_lines(&mut state.grid, &update.dirty_lines);
                state.cursor_row = update.cursor_row;
                state.cursor_col = update.cursor_col;
                state.cursor_visible = update.cursor_visible;
                state.scrollback_len = update.scrollback_len;
                if let Some(title) = update.title.as_ref() {
                    state.title = title.clone();
                }
            }
            Event::TerminalExited {
                terminal,
                exit_code,
            } => {
                if let Some(state) = self.terminals.get_mut(terminal) {
                    state.exited = true;
                    state.exit_code = *exit_code;
                    state.cursor_visible = false;
                }
            }
            _ => return,
        }
        cx.notify();
    }

    fn active_state(&self) -> Option<&TerminalState> {
        self.active.and_then(|id| self.terminals.get(&id))
    }

    // -- バックエンドへの要求 --

    /// 新しいターミナルを起動する。
    fn create_terminal(&mut self, cx: &mut Context<Self>) {
        let (Some(client), Some(workspace)) = (self.client.clone(), self.workspace.clone()) else {
            return;
        };
        if self.creating {
            return;
        }
        self.creating = true;

        // 初回はまだ実寸が分からないので、慣例的な 80x24 で起こす。
        // 実寸が決まった時点で `sync_size` がリサイズを送る。
        let (rows, cols) = self
            .grid_metrics
            .map(|m| (m.rows, m.cols))
            .unwrap_or((24, 80));
        let spec = TerminalSpec {
            workspace: workspace.id,
            shell: None,
            args: Vec::new(),
            cwd: Some(workspace.root.clone()),
            env: Vec::new(),
            rows,
            cols,
        };

        cx.spawn(async move |this, cx| {
            let result = client.request(Request::TerminalCreate { spec }).await;
            this.update(cx, |this, cx| {
                this.creating = false;
                if let Ok(Response::Terminal { terminal }) = result {
                    this.terminals
                        .insert(terminal, TerminalState::new(rows, cols));
                    this.order.push(terminal);
                    this.active = Some(terminal);
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
        cx.notify();
    }

    fn close_terminal(&mut self, id: TerminalId, cx: &mut Context<Self>) {
        if self.active == Some(id) {
            self.active = next_active_after_close(&self.order, id);
        }
        self.terminals.remove(&id);
        self.order.retain(|other| *other != id);
        if let Some(client) = self.client.clone() {
            cx.spawn(async move |_this, _cx| {
                client
                    .request(Request::TerminalClose { terminal: id })
                    .await
                    .ok();
            })
            .detach();
        }
        cx.notify();
    }

    /// 入力を待ち行列に積み、送信中でなければ送り始める。
    fn queue_input(&mut self, bytes: Vec<u8>, cx: &mut Context<Self>) {
        let Some(terminal) = self.active else {
            return;
        };
        self.pending_input.push((terminal, bytes));
        self.flush_input(cx);
    }

    fn flush_input(&mut self, cx: &mut Context<Self>) {
        if self.sending_input || self.pending_input.is_empty() {
            return;
        }
        let Some(client) = self.client.clone() else {
            self.pending_input.clear();
            return;
        };
        self.sending_input = true;
        let (terminal, bytes) = self.pending_input.remove(0);
        cx.spawn(async move |this, cx| {
            client
                .request(Request::TerminalInput { terminal, bytes })
                .await
                .ok();
            this.update(cx, |this, cx| {
                this.sending_input = false;
                this.flush_input(cx);
            })
            .ok();
        })
        .detach();
    }

    /// 描画側で確定した寸法を受け取り、変わっていればリサイズを送る。
    fn sync_size(&mut self, grid_metrics: GridMetrics, cx: &mut Context<Self>) {
        self.grid_metrics = Some(grid_metrics);
        let (Some(id), Some(client)) = (self.active, self.client.clone()) else {
            return;
        };
        let Some(state) = self.terminals.get_mut(&id) else {
            return;
        };
        let next = (grid_metrics.rows, grid_metrics.cols);
        if state.sent_size == next || state.exited {
            return;
        }
        state.sent_size = next;
        cx.spawn(async move |_this, _cx| {
            client
                .request(Request::TerminalResize {
                    terminal: id,
                    rows: next.0,
                    cols: next.1,
                })
                .await
                .ok();
        })
        .detach();
    }

    // -- 入力 --

    fn on_key_down(&mut self, event: &KeyDownEvent, _window: &mut Window, cx: &mut Context<Self>) {
        if self.active.is_none() {
            return;
        }
        let Some(bytes) = keystroke_to_bytes(&event.keystroke) else {
            return;
        };
        self.queue_input(bytes, cx);
        // 端末が処理したキーはエディタのアクションへ流さない。
        cx.stop_propagation();
    }

    fn on_mouse_down(&mut self, _: &MouseDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        window.focus(&self.focus_handle);
        cx.notify();
    }

    fn on_scroll_wheel(
        &mut self,
        event: &ScrollWheelEvent,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let (Some(id), Some(grid_metrics), Some(client)) =
            (self.active, self.grid_metrics, self.client.clone())
        else {
            return;
        };
        let delta = event.delta.pixel_delta(grid_metrics.line_height);
        // 正の delta.y は「指を下へ」= 過去方向。プロトコルの符号と一致する。
        let lines = (f32::from(delta.y) / f32::from(grid_metrics.line_height)).round() as i32;
        if lines == 0 {
            return;
        }
        cx.spawn(async move |_this, _cx| {
            client
                .request(Request::TerminalScroll {
                    terminal: id,
                    delta_lines: lines,
                })
                .await
                .ok();
        })
        .detach();
    }

    // -- 描画 --

    fn render_tab_bar(&self, cx: &mut Context<Self>) -> AnyElement {
        let theme = theme(cx).clone();
        let tabs: Vec<AnyElement> = self
            .order
            .iter()
            .enumerate()
            .map(|(index, id)| self.render_tab(index, *id, cx))
            .collect();

        h_flex()
            .h(px(26.))
            .w_full()
            .flex_none()
            .px(px(6.))
            .gap(px(4.))
            .bg(theme.bg_elevated)
            .border_b_1()
            .border_color(theme.border)
            .children(tabs)
            .child(
                h_flex()
                    .id("terminal-add")
                    .justify_center()
                    .size(px(20.))
                    .rounded(px(4.))
                    .cursor_pointer()
                    .hover(|s| s.bg(theme.accent_soft))
                    .child(icon(Icon::Plus, px(12.), theme.text_muted))
                    .on_click(cx.listener(|this, _, window, cx| {
                        window.focus(&this.focus_handle);
                        this.create_terminal(cx);
                    })),
            )
            .child(div().flex_1())
            .children(self.active_state().and_then(|state| {
                (state.scrollback_len > 0).then(|| {
                    div()
                        .text_size(px(10.5))
                        .text_color(theme.text_faint)
                        .child(format!("スクロールバック {} 行", state.scrollback_len))
                })
            }))
            .into_any_element()
    }

    fn render_tab(&self, index: usize, id: TerminalId, cx: &mut Context<Self>) -> AnyElement {
        let theme = theme(cx).clone();
        let is_active = self.active == Some(id);
        let state = self.terminals.get(&id);
        let exited = state.map(|s| s.exited).unwrap_or(false);
        let label = tab_label(state.map(|s| s.title.as_str()).unwrap_or(""), index, exited);
        let glyph_color = if exited {
            theme.text_faint
        } else if is_active {
            theme.accent
        } else {
            theme.text_muted
        };

        h_flex()
            .id(("terminal-tab", index))
            .h(px(20.))
            .px(px(7.))
            .gap(px(5.))
            .rounded(px(4.))
            .text_size(px(11.))
            .cursor_pointer()
            .when(is_active, |el| {
                // 選択中のタブだけネオンで縁取る。計器盤の「今つながっている回線」に見せる。
                el.bg(theme.accent_soft)
                    .text_color(theme.accent)
                    .border_1()
                    .border_color(theme.border_glow)
            })
            .when(!is_active, |el| {
                el.text_color(theme.text_muted)
                    .hover(|s| s.bg(theme.bg_overlay).text_color(theme.text))
            })
            .child(icon(Icon::Terminal, px(11.), glyph_color))
            .child(label)
            .child(
                h_flex()
                    .id(("terminal-tab-close", index))
                    .justify_center()
                    .size(px(14.))
                    .rounded(px(3.))
                    .hover(|s| s.bg(theme.bg_overlay))
                    .child(icon(Icon::Close, px(9.), theme.text_faint))
                    .on_click(cx.listener(move |this, _, _window, cx| {
                        // 親タブの切り替えまで発火させない。
                        cx.stop_propagation();
                        this.close_terminal(id, cx);
                    })),
            )
            .on_click(cx.listener(move |this, _, window, cx| {
                this.active = Some(id);
                window.focus(&this.focus_handle);
                cx.notify();
            }))
            .into_any_element()
    }

    /// 終了したプロセスの案内帯。
    fn render_exit_banner(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let state = self.active_state()?;
        if !state.exited {
            return None;
        }
        let theme = theme(cx).clone();
        let message = match state.exit_code {
            Some(0) => "プロセスは正常に終了しました".to_string(),
            Some(code) => format!("プロセスは終了コード {code} で終了しました"),
            None => "プロセスはシグナルで終了しました".to_string(),
        };
        Some(
            h_flex()
                .w_full()
                .flex_none()
                .h(px(22.))
                .px(px(10.))
                .gap(px(6.))
                .bg(theme.bg_overlay)
                .border_t_1()
                .border_color(theme.border)
                .text_size(px(11.))
                .text_color(theme.text_muted)
                .child(icon(Icon::Stop, px(11.), theme.accent_secondary))
                .child(message)
                .into_any_element(),
        )
    }

    fn render_launcher(&self, cx: &mut Context<Self>) -> AnyElement {
        let theme = theme(cx).clone();
        let ready = self.client.is_some() && self.workspace.is_some();
        let hint = if ready {
            "シェルを起動して、この宙域にコマンドを送ります"
        } else {
            "フォルダを開くとターミナルを起動できます"
        };
        v_flex()
            .size_full()
            .items_center()
            .justify_center()
            .gap(px(10.))
            .child(icon(Icon::Terminal, px(26.), theme.accent))
            .child(
                div()
                    .text_size(px(11.5))
                    .text_color(theme.text_faint)
                    .child(hint),
            )
            .child(
                primary_button("terminal-launch", "ターミナルを起動", ready, cx).on_click(
                    cx.listener(|this, _, window, cx| {
                        window.focus(&this.focus_handle);
                        this.create_terminal(cx);
                    }),
                ),
            )
            .into_any_element()
    }
}

impl Focusable for TerminalView {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for TerminalView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = theme(cx).clone();
        let focused = self.focus_handle.is_focused(window);
        let has_terminal = self.active.is_some();
        let tab_bar = (!self.order.is_empty()).then(|| self.render_tab_bar(cx));
        let banner = self.render_exit_banner(cx);

        let body = if has_terminal {
            div()
                .flex_1()
                .overflow_hidden()
                .bg(theme.bg_surface)
                // 等幅前提。桁の位置を 1 文字幅の整数倍で決めている。
                .font_family("SF Mono")
                .text_size(metrics::EDITOR_FONT_SIZE)
                .text_color(theme.text)
                .child(TerminalElement {
                    view: cx.entity().clone(),
                })
                .into_any_element()
        } else {
            self.render_launcher(cx)
        };

        v_flex()
            .key_context("Terminal")
            .track_focus(&self.focus_handle)
            .size_full()
            .overflow_hidden()
            .bg(theme.bg_surface)
            // フォーカス時だけ上端がネオンに灯る。どの区画が入力を受けるかを一目で示す。
            .border_t_1()
            .border_color(focus_border(focused, &theme))
            .on_key_down(cx.listener(Self::on_key_down))
            .on_mouse_down(MouseButton::Left, cx.listener(Self::on_mouse_down))
            .on_scroll_wheel(cx.listener(Self::on_scroll_wheel))
            .children(tab_bar)
            .child(body)
            .children(banner)
    }
}

// ---------------------------------------------------------------------------
// グリッドの描画要素
// ---------------------------------------------------------------------------

struct TerminalElement {
    view: Entity<TerminalView>,
}

struct TerminalPrepaint {
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

                let inverse_col = inverse_cell.and_then(|(r, c)| (r == row).then_some(c));
                let (text, runs) = build_row(cells, &theme, &base_font, inverse_col);
                if text.is_empty() {
                    continue;
                }
                let shaped = window.text_system().shape_line(
                    SharedString::from(text),
                    font_size,
                    &runs,
                    None,
                );
                lines.push((shaped, point(bounds.origin.x, y)));
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

/// 変化した行をグリッドへ反映する。
///
/// 端末が拡がった直後は、まだ GUI 側に存在しない行番号が届きうる。足りなければ伸ばす。
fn apply_dirty_lines(grid: &mut Vec<Vec<TerminalCell>>, dirty: &[(u16, Vec<TerminalCell>)]) {
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

/// 1 行ぶんの表示文字列とテキストランを組み立てる。
///
/// `TextRun::len` は **バイト数**。同じ色・同じ装飾が続くセルは 1 ランに畳む。
/// `inverse_col` にはカーソルが乗っている桁を渡す。その桁だけ反転色で描く。
fn build_row(
    cells: &[TerminalCell],
    theme: &Theme,
    base_font: &Font,
    inverse_col: Option<usize>,
) -> (String, Vec<TextRun>) {
    let end = visible_len(cells);
    let mut text = String::new();
    let mut runs: Vec<TextRun> = Vec::new();
    let mut current: Option<(RunStyle, usize)> = None;

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
        text.push(ch);
        let byte_len = ch.len_utf8();
        current = match current {
            Some((run_style, len)) if run_style == style => Some((run_style, len + byte_len)),
            Some((run_style, len)) => {
                runs.push(make_run(&run_style, len, base_font));
                Some((style, byte_len))
            }
            None => Some((style, byte_len)),
        };
    }
    if let Some((style, len)) = current {
        runs.push(make_run(&style, len, base_font));
    }
    (text, runs)
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

/// タブに出す名前。
fn tab_label(title: &str, index: usize, exited: bool) -> String {
    let trimmed = title.trim();
    let base = if trimmed.is_empty() {
        format!("シェル {}", index + 1)
    } else {
        truncate_middle(trimmed, 24)
    };
    if exited {
        format!("{base} ・終了")
    } else {
        base
    }
}

/// タブを閉じたあとに選ぶべきタブ。
///
/// 右隣を優先し、無ければ左隣。VS Code と同じ挙動にしておく。
fn next_active_after_close(order: &[TerminalId], closed: TerminalId) -> Option<TerminalId> {
    let index = order.iter().position(|id| *id == closed)?;
    order
        .get(index + 1)
        .or_else(|| index.checked_sub(1).and_then(|prev| order.get(prev)))
        .copied()
}

/// キー入力を PTY へ流すバイト列に変換する。
///
/// 返り値が `None` のキーは端末に送らない (アプリのショートカットへ譲る)。
fn keystroke_to_bytes(keystroke: &Keystroke) -> Option<Vec<u8>> {
    let modifiers = keystroke.modifiers;
    // cmd と fn はアプリ側の割り当て。端末には渡さない。
    if modifiers.platform || modifiers.function {
        return None;
    }
    let key = keystroke.key.as_str();

    let special: Option<&[u8]> = match key {
        "enter" => Some(b"\r"),
        "tab" if modifiers.shift => Some(b"\x1b[Z"),
        "tab" => Some(b"\t"),
        "backspace" => Some(b"\x7f"),
        "escape" => Some(b"\x1b"),
        "up" => Some(b"\x1b[A"),
        "down" => Some(b"\x1b[B"),
        "right" => Some(b"\x1b[C"),
        "left" => Some(b"\x1b[D"),
        "home" => Some(b"\x1b[H"),
        "end" => Some(b"\x1b[F"),
        "pageup" => Some(b"\x1b[5~"),
        "pagedown" => Some(b"\x1b[6~"),
        "delete" => Some(b"\x1b[3~"),
        _ => None,
    };
    if let Some(bytes) = special {
        return Some(bytes.to_vec());
    }

    if modifiers.control {
        return control_bytes(key);
    }

    // alt は meta として ESC 前置で送る。合成済みの文字ではなく素のキーを使う。
    if modifiers.alt {
        let mut bytes = vec![0x1b];
        bytes.extend_from_slice(printable_text(keystroke)?.as_bytes());
        return Some(bytes);
    }

    Some(printable_text(keystroke)?.into_bytes())
}

/// 通常文字として送れる文字列。送れないキー (f1 など) は `None`。
fn printable_text(keystroke: &Keystroke) -> Option<String> {
    if keystroke.key == "space" {
        return Some(" ".to_string());
    }
    // key_char は option-s → "ß" のような合成結果を含む。あればそちらを優先する。
    if let Some(text) = keystroke.key_char.as_deref()
        && !text.is_empty()
        && !text.chars().any(|c| c.is_control())
    {
        return Some(text.to_string());
    }
    let mut chars = keystroke.key.chars();
    match (chars.next(), chars.next()) {
        (Some(c), None) if !c.is_control() => Some(c.to_string()),
        _ => None,
    }
}

/// Ctrl 併用時の制御文字。
fn control_bytes(key: &str) -> Option<Vec<u8>> {
    if key == "space" {
        return Some(vec![0]);
    }
    let mut chars = key.chars();
    let (Some(c), None) = (chars.next(), chars.next()) else {
        return None;
    };
    let byte = match c {
        'a'..='z' => c as u8 - b'a' + 1,
        'A'..='Z' => c as u8 - b'A' + 1,
        '@' => 0,
        '[' => 0x1b,
        '\\' => 0x1c,
        ']' => 0x1d,
        '^' => 0x1e,
        '_' => 0x1f,
        '?' => 0x7f,
        _ => return None,
    };
    Some(vec![byte])
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::Modifiers;

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

    fn key(name: &str, modifiers: Modifiers) -> Keystroke {
        Keystroke {
            modifiers,
            key: name.to_string(),
            key_char: (name.chars().count() == 1).then(|| name.to_string()),
        }
    }

    fn ctrl() -> Modifiers {
        Modifiers {
            control: true,
            ..Modifiers::default()
        }
    }

    // -- キー変換 --

    #[test]
    fn 通常文字は_utf8_バイトになる() {
        assert_eq!(
            keystroke_to_bytes(&key("a", Modifiers::default())),
            Some(b"a".to_vec())
        );
        let mut ime = key("a", Modifiers::default());
        ime.key_char = Some("あ".to_string());
        assert_eq!(keystroke_to_bytes(&ime), Some("あ".as_bytes().to_vec()));
    }

    #[test]
    fn 特殊キーがエスケープシーケンスになる() {
        let table = [
            ("enter", b"\r".to_vec()),
            ("tab", b"\t".to_vec()),
            ("backspace", b"\x7f".to_vec()),
            ("escape", b"\x1b".to_vec()),
            ("up", b"\x1b[A".to_vec()),
            ("down", b"\x1b[B".to_vec()),
            ("right", b"\x1b[C".to_vec()),
            ("left", b"\x1b[D".to_vec()),
            ("home", b"\x1b[H".to_vec()),
            ("end", b"\x1b[F".to_vec()),
            ("pageup", b"\x1b[5~".to_vec()),
            ("pagedown", b"\x1b[6~".to_vec()),
            ("delete", b"\x1b[3~".to_vec()),
        ];
        for (name, expected) in table {
            assert_eq!(
                keystroke_to_bytes(&key(name, Modifiers::default())),
                Some(expected),
                "{name} の変換"
            );
        }
    }

    #[test]
    fn ctrl_併用で制御文字になる() {
        assert_eq!(keystroke_to_bytes(&key("a", ctrl())), Some(vec![0x01]));
        assert_eq!(keystroke_to_bytes(&key("c", ctrl())), Some(vec![0x03]));
        assert_eq!(keystroke_to_bytes(&key("z", ctrl())), Some(vec![0x1a]));
        assert_eq!(keystroke_to_bytes(&key("space", ctrl())), Some(vec![0x00]));
    }

    #[test]
    fn cmd_併用は端末に送らない() {
        let cmd = Modifiers {
            platform: true,
            ..Modifiers::default()
        };
        assert_eq!(keystroke_to_bytes(&key("c", cmd)), None);
        assert_eq!(keystroke_to_bytes(&key("f1", Modifiers::default())), None);
    }

    #[test]
    fn shift_tab_は逆タブ_alt_は_esc_前置() {
        let shift = Modifiers {
            shift: true,
            ..Modifiers::default()
        };
        assert_eq!(
            keystroke_to_bytes(&key("tab", shift)),
            Some(b"\x1b[Z".to_vec())
        );
        let alt = Modifiers {
            alt: true,
            ..Modifiers::default()
        };
        assert_eq!(keystroke_to_bytes(&key("b", alt)), Some(b"\x1bb".to_vec()));
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

    #[test]
    fn 同色のセルが_1_ランに畳まれる() {
        let theme = Theme::cyber_cosmic();
        let font = gpui::font("SF Mono");
        let cells = vec![
            colored('a', TermColor::Indexed(2)),
            colored('b', TermColor::Indexed(2)),
            colored('c', TermColor::Indexed(5)),
        ];
        let (text, runs) = build_row(&cells, &theme, &font, None);
        assert_eq!(text, "abc");
        assert_eq!(runs.len(), 2, "色が変わるところでだけランが切れる");
        assert_eq!(runs[0].len, 2);
        assert_eq!(runs[1].len, 1);
    }

    #[test]
    fn ラン長の合計はバイト数に一致する() {
        let theme = Theme::cyber_cosmic();
        let font = gpui::font("SF Mono");
        let cells = vec![cell('あ'), cell('a'), colored('い', TermColor::Indexed(4))];
        let (text, runs) = build_row(&cells, &theme, &font, None);
        let total: usize = runs.iter().map(|r| r.len).sum();
        assert_eq!(total, text.len());
        assert_eq!(text.len(), 7, "3+1+3 バイト");
    }

    #[test]
    fn 全角の後続セルは読み飛ばす() {
        let theme = Theme::cyber_cosmic();
        let font = gpui::font("SF Mono");
        let trailer = TerminalCell {
            ch: ' ',
            flags: cell_flags::WIDE_TRAILER,
            ..TerminalCell::default()
        };
        let cells = vec![cell('あ'), trailer, cell('x')];
        let (text, _) = build_row(&cells, &theme, &font, None);
        assert_eq!(text, "あx");
    }

    #[test]
    fn カーソル下の文字は反転色で描く() {
        let theme = Theme::cyber_cosmic();
        let font = gpui::font("SF Mono");
        let cells = vec![cell('a'), cell('b'), cell('c')];
        let (_, runs) = build_row(&cells, &theme, &font, Some(1));
        assert_eq!(runs.len(), 3);
        assert_eq!(runs[1].color, theme.text_inverse);
        assert_ne!(runs[0].color, theme.text_inverse);
    }

    #[test]
    fn 装飾フラグがランに反映される() {
        let theme = Theme::cyber_cosmic();
        let font = gpui::font("SF Mono");
        let decorated = TerminalCell {
            ch: 'x',
            flags: cell_flags::BOLD | cell_flags::UNDERLINE | cell_flags::STRIKETHROUGH,
            ..TerminalCell::default()
        };
        let (_, runs) = build_row(&[decorated], &theme, &font, None);
        assert_eq!(runs[0].font.weight, FontWeight::BOLD);
        assert!(runs[0].underline.is_some());
        assert!(runs[0].strikethrough.is_some());
    }

    // -- タブ --

    #[test]
    fn タブ名はタイトルが空なら連番になる() {
        assert_eq!(tab_label("", 0, false), "シェル 1");
        assert_eq!(tab_label("  ", 2, false), "シェル 3");
        assert_eq!(tab_label("zsh", 0, false), "zsh");
        assert_eq!(tab_label("zsh", 0, true), "zsh ・終了");
    }

    #[test]
    fn 閉じたあとは右隣を選ぶ() {
        let ids = [TerminalId(1), TerminalId(2), TerminalId(3)];
        assert_eq!(
            next_active_after_close(&ids, TerminalId(2)),
            Some(TerminalId(3))
        );
        assert_eq!(
            next_active_after_close(&ids, TerminalId(3)),
            Some(TerminalId(2)),
            "末尾なら左隣"
        );
        assert_eq!(
            next_active_after_close(&[TerminalId(1)], TerminalId(1)),
            None
        );
        assert_eq!(next_active_after_close(&ids, TerminalId(9)), None);
    }
}
