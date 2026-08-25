//! 統合ターミナル。
//!
//! PTY と ANSI 解釈はバックエンドが担う。GUI が持つのは可視グリッド
//! (`Vec<Vec<TerminalCell>>`) だけで、バックエンドは **変化した行だけ** を
//! [`Event::TerminalUpdated`] で送ってくる。
//!
//! 描画は `div` を並べずカスタム [`Element`] で行う。80x24 のグリッドを
//! `div` でセルごとに組むと 1920 要素になり、レイアウトだけで 1 フレームを
//! 使い切ってしまう。ここでは 1 行を「同じ見た目が続く区間」へ畳んで
//! 区間ごとに `shape_line` し、背景と枠だけを矩形として描く。
//!
//! 区間には **開始桁** を持たせ、`cell_width * 桁` に置く。字送りに桁位置を
//! 任せると、全角やフォールバック書体が混ざった時点で文字とカーソル矩形がずれる。

use crate::assets::Icon;
use crate::ipc_client::BackendClient;
use crate::theme::{Theme, metrics, theme};
use crate::ui::{focus_border, h_flex, icon, truncate_middle, v_flex};
use gpui::prelude::*;
use gpui::{
    AnyElement, App, BorderStyle, Bounds, ClipboardItem, Context, ElementId, Entity, FocusHandle,
    Focusable, Font, FontStyle, FontWeight, GlobalElementId, Hsla, KeyDownEvent, Keystroke,
    LayoutId, MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent, PaintQuad, Pixels, Point,
    ScrollWheelEvent, ShapedLine, SharedString, StrikethroughStyle, Style, TextRun, UnderlineStyle,
    Window, anchored, deferred, div, fill, outline, point, px, relative, size,
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
    /// 端末領域の左上 (ウィンドウ座標)。マウス位置をセルへ変換するのに要る。
    origin: Point<Pixels>,
}

/// マウス選択の範囲 (グリッド座標)。
///
/// `anchor` はドラッグを始めた側で固定、`head` はドラッグ中に動く側
/// (呼び名は `nebula_core::Selection` に合わせてある)。上下どちらに向かって
/// ドラッグしても選べるよう、使う側は `normalized` で読み順に直してから使う。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct GridSelection {
    anchor_row: usize,
    anchor_col: usize,
    head_row: usize,
    head_col: usize,
}

impl GridSelection {
    fn caret(row: usize, col: usize) -> Self {
        Self {
            anchor_row: row,
            anchor_col: col,
            head_row: row,
            head_col: col,
        }
    }

    /// ドラッグしていない (1 セルも選んでいない) か。
    fn is_empty(&self) -> bool {
        self.anchor_row == self.head_row && self.anchor_col == self.head_col
    }

    /// 読み順 (行が小さい方を先) に正規化した (開始行, 開始桁, 終了行, 終了桁)。
    /// 終了桁は最終行の中で選ばれた最後の桁 (これを含む)。
    fn normalized(&self) -> (usize, usize, usize, usize) {
        let anchor = (self.anchor_row, self.anchor_col);
        let head = (self.head_row, self.head_col);
        let (start, end) = if anchor <= head {
            (anchor, head)
        } else {
            (head, anchor)
        };
        (start.0, start.1, end.0, end.1)
    }
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
    /// マウスドラッグで選んだ範囲。ドラッグを終えてもコピーできるよう、
    /// マウスを離した後も保持する。
    selection: Option<GridSelection>,
    /// 左ボタンを押してから離すまでの間だけ true。
    selecting: bool,
    /// 右クリックメニューを開いている位置 (ウィンドウ座標)。
    context_menu: Option<Point<Pixels>>,
    /// 下部パネルでターミナルが表示されているか。自動起動の門番に使う。
    panel_visible: bool,
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
            selection: None,
            selecting: false,
            context_menu: None,
            panel_visible: false,
        }
    }

    /// 下部パネルでターミナルが実際に表示されているかを伝える。
    ///
    /// 自動起動をこの状態で門番するために要る。接続とワークスペースが揃った
    /// だけで起動してしまうと、ターミナルを一度も開かない利用者の裏でシェルが
    /// 常駐することになる (issue は「ターミナルを開いたとき」に起動せよと言っている)。
    pub fn set_panel_visible(&mut self, visible: bool, cx: &mut Context<Self>) {
        self.panel_visible = visible;
        if visible {
            self.ensure_terminal(cx);
        }
    }

    pub fn set_client(&mut self, client: BackendClient, cx: &mut Context<Self>) {
        self.client = Some(client);
        self.ensure_terminal(cx);
        cx.notify();
    }

    pub fn set_workspace(&mut self, workspace: WorkspaceInfo, cx: &mut Context<Self>) {
        self.workspace = Some(workspace);
        self.ensure_terminal(cx);
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

    /// 端末パネルを表示したときに呼ぶ。1 つも端末が無ければ既定シェルを起こす。
    ///
    /// 判定そのものは `should_auto_launch_terminal` に切り出してある
    /// (境界をテストで固定するため)。ここでは自身の状態をその引数へ渡すだけ。
    pub fn ensure_terminal(&mut self, cx: &mut Context<Self>) {
        if should_auto_launch_terminal(
            self.creating,
            !self.order.is_empty(),
            self.client.is_some(),
            self.workspace.is_some(),
            self.panel_visible,
        ) {
            self.create_terminal(cx);
        }
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
        // macOS の作法である Cmd+C / Cmd+V はキー変換の通常経路 (keystroke_to_bytes)
        // より先に処理する。Ctrl+C は SIGINT なので platform 修飾を見ることで
        // 混同しない (下の is_copy_shortcut / is_paste_shortcut を参照)。
        if is_copy_shortcut(&event.keystroke) {
            self.copy_selection(cx);
            cx.stop_propagation();
            return;
        }
        if is_paste_shortcut(&event.keystroke) {
            self.paste_from_clipboard(cx);
            cx.stop_propagation();
            return;
        }
        let Some(bytes) = keystroke_to_bytes(&event.keystroke) else {
            return;
        };
        self.queue_input(bytes, cx);
        // 端末が処理したキーはエディタのアクションへ流さない。
        cx.stop_propagation();
    }

    fn on_mouse_down(&mut self, event: &MouseDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        window.focus(&self.focus_handle);
        // ドラッグ選択の開始点を記録する。実寸がまだ無ければ (初回接続前など) 何もしない。
        if self.active.is_some()
            && let Some(metrics) = self.grid_metrics
        {
            let (row, col) = position_to_cell(
                f32::from(event.position.x),
                f32::from(event.position.y),
                f32::from(metrics.origin.x),
                f32::from(metrics.origin.y),
                f32::from(metrics.cell_width),
                f32::from(metrics.line_height),
                metrics.rows,
                metrics.cols,
            );
            self.selection = Some(GridSelection::caret(row, col));
            self.selecting = true;
        }
        cx.notify();
    }

    fn on_mouse_move(&mut self, event: &MouseMoveEvent, _window: &mut Window, cx: &mut Context<Self>) {
        if !self.selecting {
            return;
        }
        // パネルの外でボタンを離すと (bubble ハンドラである) on_mouse_up が発火しない。
        // ここでボタンの状態を見て打ち切らないと、外で離した後に領域内へ戻すだけで
        // ボタンを押していないのに選択が伸び続けてしまう。
        if event.pressed_button != Some(MouseButton::Left) {
            self.selecting = false;
            return;
        }
        let Some(metrics) = self.grid_metrics else {
            return;
        };
        let (row, col) = position_to_cell(
            f32::from(event.position.x),
            f32::from(event.position.y),
            f32::from(metrics.origin.x),
            f32::from(metrics.origin.y),
            f32::from(metrics.cell_width),
            f32::from(metrics.line_height),
            metrics.rows,
            metrics.cols,
        );
        let Some(selection) = self.selection.as_mut() else {
            return;
        };
        // セルが変わらないあいだは再描画しない。ドラッグ中は大量に飛んでくるので。
        if selection.head_row == row && selection.head_col == col {
            return;
        }
        selection.head_row = row;
        selection.head_col = col;
        cx.notify();
    }

    fn on_mouse_up(&mut self, _event: &MouseUpEvent, _window: &mut Window, cx: &mut Context<Self>) {
        self.selecting = false;
        // ドラッグせずクリックしただけなら選択は残さない (1 セルだけのコピーは意味がない)。
        if self.selection.is_some_and(|s| s.is_empty()) {
            self.selection = None;
        }
        cx.notify();
    }

    fn on_context_menu(&mut self, event: &MouseDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        window.focus(&self.focus_handle);
        if self.active.is_none() {
            return;
        }
        self.context_menu = Some(event.position);
        cx.notify();
    }

    /// 選択範囲をクリップボードへコピーする。選択が無ければ何もしない。
    fn copy_selection(&mut self, cx: &mut Context<Self>) {
        let Some(selection) = self.selection else {
            return;
        };
        let Some(state) = self.active_state() else {
            return;
        };
        let text = extract_selected_text(&state.grid, &selection);
        if !text.is_empty() {
            cx.write_to_clipboard(ClipboardItem::new_string(text));
        }
    }

    /// クリップボードの文字列を、キー入力と全く同じ経路 (queue_input) で PTY へ流す。
    fn paste_from_clipboard(&mut self, cx: &mut Context<Self>) {
        let Some(text) = cx.read_from_clipboard().and_then(|item| item.text()) else {
            return;
        };
        self.queue_input(text.into_bytes(), cx);
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
        // 可視グリッドの内容がスクロールでずれるので、選択は座標ごと無効にする。
        // 保持したままだとハイライトもコピー結果も別の行を指すことになる。
        if self.selection.take().is_some() {
            cx.notify();
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

    /// 端末がまだ無い間の案内。ボタンは出さない — 端末は `ensure_terminal` が
    /// パネルを開いた時点で自動的に起こす。ここに来るのは、ワークスペースが
    /// まだ無いか、起動要求がまだ返ってきていない一瞬か、直前のタブを
    /// 閉じた直後 (タブバーの「＋」で作り直せる) のいずれか。
    fn render_empty_state(&self, cx: &mut Context<Self>) -> AnyElement {
        let theme = theme(cx).clone();
        let ready = self.client.is_some() && self.workspace.is_some();
        let hint = if !ready {
            "フォルダを開くとターミナルを起動できます"
        } else if self.creating {
            "シェルを起動しています…"
        } else {
            "上の＋からターミナルを起動できます"
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
            .into_any_element()
    }

    // -- 右クリックメニュー --

    fn render_context_menu(&self, cx: &Context<Self>) -> Option<AnyElement> {
        let position = self.context_menu?;
        let theme = theme(cx);
        let can_copy = self.selection.is_some();

        let items = vec![
            self.menu_item(
                "terminal-menu-copy",
                Icon::Copy,
                "コピー",
                can_copy,
                cx.listener(|this, _e, _window, cx| {
                    this.copy_selection(cx);
                    this.context_menu = None;
                    cx.notify();
                }),
                cx,
            ),
            self.menu_item(
                "terminal-menu-paste",
                Icon::Files,
                "貼り付け",
                true,
                cx.listener(|this, _e, _window, cx| {
                    this.paste_from_clipboard(cx);
                    this.context_menu = None;
                    cx.notify();
                }),
                cx,
            ),
        ];

        // gpui にメニュー部品は無いので、絶対配置した箱を自前で組む (explorer.rs と同じ要領)。
        // 位置はマウスのウィンドウ座標なので、ウィンドウ基準で置ける anchored に載せる。
        Some(
            deferred(
                anchored().position(position).snap_to_window().child(
                    v_flex()
                        .absolute()
                        .min_w(px(160.))
                        .py(px(4.))
                        .rounded(px(8.))
                        .bg(theme.bg_overlay)
                        .border_1()
                        .border_color(theme.border_glow)
                        .shadow_lg()
                        // メニュー上のクリックが背後の端末領域 (ドラッグ選択の on_mouse_down) へ
                        // 突き抜けないようにする。無いと「コピー」を押した瞬間に選択が
                        // メニュー位置の 1 セルへ上書きされ、コピーが空になる。
                        .occlude()
                        .on_mouse_down_out(cx.listener(|this, _e: &MouseDownEvent, _window, cx| {
                            this.context_menu = None;
                            cx.notify();
                        }))
                        .children(items),
                ),
            )
            .into_any_element(),
        )
    }

    fn menu_item(
        &self,
        id: impl Into<ElementId>,
        glyph: Icon,
        label: &'static str,
        enabled: bool,
        on_click: impl Fn(&gpui::ClickEvent, &mut Window, &mut App) + 'static,
        cx: &Context<Self>,
    ) -> AnyElement {
        let theme = theme(cx);
        let row = h_flex()
            .id(id)
            .h(px(26.))
            .px(px(10.))
            .gap(px(8.))
            .text_size(px(12.))
            .text_color(if enabled {
                theme.text_muted
            } else {
                theme.text_faint
            })
            .child(icon(glyph, px(13.), theme.text_faint))
            .child(label);
        if enabled {
            row.cursor_pointer()
                .hover(|s| s.bg(theme.accent_soft).text_color(theme.text))
                .on_click(on_click)
                .into_any_element()
        } else {
            row.into_any_element()
        }
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
        let ready = self.client.is_some() && self.workspace.is_some();
        // タブが 1 つも無くても、起こせる状態なら「＋」だけのタブバーを出す。
        // ボタンを廃したぶん、最初の 1 枚を手で作り直す手段はこれだけになるため。
        let tab_bar = (!self.order.is_empty() || ready).then(|| self.render_tab_bar(cx));
        let banner = self.render_exit_banner(cx);

        let body = if has_terminal {
            div()
                .flex_1()
                .overflow_hidden()
                .bg(theme.bg_surface)
                // 等幅前提。桁の位置を 1 文字幅の整数倍で決めている。
                .font_family(metrics::MONO_FONT_FAMILY)
                .text_size(metrics::EDITOR_FONT_SIZE)
                .text_color(theme.text)
                .child(TerminalElement {
                    view: cx.entity().clone(),
                })
                .into_any_element()
        } else {
            self.render_empty_state(cx)
        };

        v_flex()
            .key_context("Terminal")
            .track_focus(&self.focus_handle)
            .relative()
            .size_full()
            .overflow_hidden()
            .bg(theme.bg_surface)
            // フォーカス時だけ上端がネオンに灯る。どの区画が入力を受けるかを一目で示す。
            .border_t_1()
            .border_color(focus_border(focused, &theme))
            .on_key_down(cx.listener(Self::on_key_down))
            .on_mouse_down(MouseButton::Left, cx.listener(Self::on_mouse_down))
            .on_mouse_move(cx.listener(Self::on_mouse_move))
            .on_mouse_up(MouseButton::Left, cx.listener(Self::on_mouse_up))
            .on_mouse_down(MouseButton::Right, cx.listener(Self::on_context_menu))
            .on_scroll_wheel(cx.listener(Self::on_scroll_wheel))
            .children(tab_bar)
            .child(body)
            .children(banner)
            .children(self.render_context_menu(cx))
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
fn position_to_cell(
    x: f32,
    y: f32,
    origin_x: f32,
    origin_y: f32,
    cell_width: f32,
    line_height: f32,
    rows: u16,
    cols: u16,
) -> (usize, usize) {
    if cell_width <= 0.0 || line_height <= 0.0 || rows == 0 || cols == 0 {
        return (0, 0);
    }
    let col = ((x - origin_x) / cell_width).floor().max(0.0) as usize;
    let row = ((y - origin_y) / line_height).floor().max(0.0) as usize;
    (row.min(rows as usize - 1), col.min(cols as usize - 1))
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
    let from = if row == start_row { start_col.min(row_len) } else { 0 };
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
fn extract_selected_text(grid: &[Vec<TerminalCell>], selection: &GridSelection) -> String {
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

/// 端末パネルを開いたときに、既定シェルを自動で起こしてよいか。
///
/// `has_tabs` には `order` が空でないかを渡す — 終了 (exited) したタブが
/// 残っている間は自動再起動しない。さもないと落ちるコマンドを打つたびに
/// 際限なく起動し直してしまう。作り直したければタブバーの「＋」から手で行う。
fn should_auto_launch_terminal(
    creating: bool,
    has_tabs: bool,
    has_client: bool,
    has_workspace: bool,
    panel_visible: bool,
) -> bool {
    !creating && !has_tabs && has_client && has_workspace && panel_visible
}

/// macOS の作法で Cmd+C (コピー) か。
///
/// Ctrl+C は SIGINT でありコピーではないので、platform 修飾 (Cmd) が
/// 立っているときだけコピーと判定する。混同すると Ctrl+C でプロセスを
/// 止められなくなる。
fn is_copy_shortcut(keystroke: &Keystroke) -> bool {
    keystroke.modifiers.platform && keystroke.key == "c"
}

/// macOS の作法で Cmd+V (貼り付け) か。
fn is_paste_shortcut(keystroke: &Keystroke) -> bool {
    keystroke.modifiers.platform && keystroke.key == "v"
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

    #[test]
    fn cmd_c_はコピーと判定され_ctrl_c_は判定されない() {
        let cmd = Modifiers {
            platform: true,
            ..Modifiers::default()
        };
        assert!(is_copy_shortcut(&key("c", cmd)));
        // Ctrl+C は SIGINT。誤ってコピー扱いすると端末でプロセスを止められなくなる。
        assert!(!is_copy_shortcut(&key("c", ctrl())));
    }

    #[test]
    fn cmd_v_はペーストと判定され_修飾無しの_v_は判定されない() {
        let cmd = Modifiers {
            platform: true,
            ..Modifiers::default()
        };
        assert!(is_paste_shortcut(&key("v", cmd)));
        assert!(!is_paste_shortcut(&key("v", Modifiers::default())));
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

    #[test]
    fn 左上の角ちょうどは_0_行_0_列になる() {
        assert_eq!(
            position_to_cell(100.0, 50.0, 100.0, 50.0, 8.0, 16.0, 24, 80),
            (0, 0)
        );
    }

    #[test]
    fn 右下の最終セルちょうどは最終行最終列になる() {
        // 80 列 24 行なら最終セルは列 79・行 23。その左上ぴったりを指す。
        let x = 100.0 + 8.0 * 79.0;
        let y = 50.0 + 16.0 * 23.0;
        assert_eq!(
            position_to_cell(x, y, 100.0, 50.0, 8.0, 16.0, 24, 80),
            (23, 79)
        );
    }

    #[test]
    fn 領域より左上の負値は_0_行_0_列に丸められる() {
        // 原点 (100, 50) より左上の座標を渡す。相対位置が負になるケース。
        assert_eq!(
            position_to_cell(0.0, 0.0, 100.0, 50.0, 8.0, 16.0, 24, 80),
            (0, 0)
        );
    }

    #[test]
    fn 領域より右下の大きすぎる値は最終行最終列に丸められる() {
        let x = 100.0 + 8.0 * 1000.0;
        let y = 50.0 + 16.0 * 1000.0;
        assert_eq!(
            position_to_cell(x, y, 100.0, 50.0, 8.0, 16.0, 24, 80),
            (23, 79)
        );
    }

    #[test]
    fn 全角文字の後続セルの範囲内をクリックしてもそのセルの列になる() {
        // 全角文字は 2 列を占めるが、字送り幅そのものは列ごとに一定
        // (ファイル冒頭のコメント参照)。後続セル (列 1) の範囲内なら列 1 が返ればよい。
        let x = 100.0 + 8.0 * 1.0 + 4.0;
        assert_eq!(
            position_to_cell(x, 50.0, 100.0, 50.0, 8.0, 16.0, 24, 80),
            (0, 1)
        );
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

    // -- 自動起動 --

    #[test]
    fn 起動処理中は自動起動しない() {
        assert!(!should_auto_launch_terminal(true, false, true, true, true));
    }

    #[test]
    fn 既存タブがあれば自動起動しない() {
        // exited のまま残っているタブも「既存タブ」に含める。でないと落ちる
        // コマンドを打つたびに際限なく再起動してしまう。
        assert!(!should_auto_launch_terminal(false, true, true, true, true));
    }

    #[test]
    fn バックエンドへ未接続なら自動起動しない() {
        assert!(!should_auto_launch_terminal(false, false, false, true, true));
    }

    #[test]
    fn 作業フォルダがまだ無ければ自動起動しない() {
        assert!(!should_auto_launch_terminal(false, false, true, false, true));
    }

    #[test]
    fn 起動処理中でなく既存タブも無く準備が整っていれば自動起動する() {
        assert!(should_auto_launch_terminal(false, false, true, true, true));
    }

    /// 「ターミナルを開いたとき」に起動するのが issue の要求。接続とワークスペースが
    /// 揃っただけで起動すると、パネルを一度も開かない利用者の裏でシェルが常駐する。
    #[test]
    fn パネルが表示されていなければ自動起動しない() {
        assert!(!should_auto_launch_terminal(false, false, true, true, false));
    }
}
