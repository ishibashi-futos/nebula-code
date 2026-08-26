//! 統合ターミナル。
//!
//! PTY と ANSI 解釈はバックエンドが担う。GUI が持つのは可視グリッド
//! (`Vec<Vec<TerminalCell>>`) だけで、バックエンドは **変化した行だけ** を
//! [`Event::TerminalUpdated`] で送ってくる。
//!
//! グリッドの描画 (カスタム `Element`) は `element` サブモジュールへ、
//! 打鍵をバイト列へ変換する部分は `keys` サブモジュールへそれぞれ切り出して
//! ある。ここに残すのは状態の保持とバックエンドとのやり取り、タブバーまわりの描画。

mod element;
mod keys;

use crate::assets::Icon;
use crate::ipc_client::BackendClient;
use crate::theme::{metrics, theme};
use crate::ui::{
    focus_border, h_flex, icon, simple_tooltip, tooltip_text, truncate_middle, v_flex,
};
use element::{TerminalElement, apply_dirty_lines, extract_selected_text, position_to_cell};
use gpui::prelude::*;
use gpui::{
    AnyElement, App, ClipboardItem, Context, ElementId, FocusHandle, Focusable, KeyDownEvent,
    MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent, Pixels, Point, ScrollWheelEvent,
    Window, anchored, deferred, div, px,
};
use keys::{is_copy_shortcut, is_paste_shortcut, keystroke_to_bytes};
use nebula_protocol::{
    Event, Request, Response, TerminalCell, TerminalId, TerminalSpec, WorkspaceInfo,
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
        // コピー・貼り付けはキー変換の通常経路 (keystroke_to_bytes) より先に
        // 処理する。打鍵の選び方はプラットフォームで違う
        // (下の is_clipboard_shortcut を参照)。
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

    fn on_mouse_down(
        &mut self,
        event: &MouseDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        window.focus(&self.focus_handle);
        // ドラッグ選択の開始点を記録する。実寸がまだ無ければ (初回接続前など) 何もしない。
        if self.active.is_some()
            && let Some(metrics) = self.grid_metrics
        {
            let (row, col) = position_to_cell(
                f32::from(event.position.x),
                f32::from(event.position.y),
                &metrics,
            );
            self.selection = Some(GridSelection::caret(row, col));
            self.selecting = true;
        }
        cx.notify();
    }

    fn on_mouse_move(
        &mut self,
        event: &MouseMoveEvent,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
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
            &metrics,
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

    fn on_context_menu(
        &mut self,
        event: &MouseDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
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
                    }))
                    .tooltip(simple_tooltip(tooltip_text::TERMINAL_ADD)),
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
                    }))
                    .tooltip(simple_tooltip(tooltip_text::TERMINAL_CLOSE_TAB)),
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

#[cfg(test)]
mod tests {
    use super::*;

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
        assert!(!should_auto_launch_terminal(
            false, false, false, true, true
        ));
    }

    #[test]
    fn 作業フォルダがまだ無ければ自動起動しない() {
        assert!(!should_auto_launch_terminal(
            false, false, true, false, true
        ));
    }

    #[test]
    fn 起動処理中でなく既存タブも無く準備が整っていれば自動起動する() {
        assert!(should_auto_launch_terminal(false, false, true, true, true));
    }

    /// 「ターミナルを開いたとき」に起動するのが issue の要求。接続とワークスペースが
    /// 揃っただけで起動すると、パネルを一度も開かない利用者の裏でシェルが常駐する。
    #[test]
    fn パネルが表示されていなければ自動起動しない() {
        assert!(!should_auto_launch_terminal(
            false, false, true, true, false
        ));
    }
}
