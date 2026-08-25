//! 1 バッファぶんのエディタ。
//!
//! GUI 側はバッファの **複製** を持つ。キー入力はまずこの複製へ適用して即座に再描画し、
//! 同じ編集をバックエンドへ送る。打鍵ごとに IPC の往復を待つと、体感が
//! ネットワーク越しのエディタになるため。
//!
//! 構文ハイライトだけはバックエンドが計算する。GUI で Tree-sitter を動かさないのが
//! 「軽量 GUI 描画プロセス」という設計の要。ハイライトは 1〜2 フレーム遅れて追いつく。

use crate::actions::*;
use crate::ipc_client::BackendClient;
use crate::theme::{metrics, theme};
use crate::ui::{h_flex, v_flex};
use crate::views::editor_element::{EditorElement, EditorLayoutInfo};
use gpui::prelude::*;
use gpui::{
    Bounds, ClipboardItem, Context, EntityInputHandler, EventEmitter, FocusHandle, Focusable,
    MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent, Pixels, Point, ScrollWheelEvent,
    UTF16Selection, Window, div, px,
};
use nebula_core::selection::{Direction, Movement, move_selection, normalize};
use nebula_core::{RopeExt, Selection, TextBuffer};
use nebula_protocol::{
    BufferId, BufferSnapshot, CompletionItem, CompletionKind, Diagnostic, DiffHunk, Edit,
    HighlightSpan, HoverInfo, LanguageConfig, NotificationLevel, Position, Request, Response,
    TextRange, WorkspaceId,
};
use std::ops::Range;
use std::path::PathBuf;

/// シェルへ伝える出来事。
pub enum EditorViewEvent {
    /// 内容が変わった (タブの未保存印を更新するため)。
    Dirtied,
    /// 保存された。
    Saved,
    Notify(NotificationLevel, String),
    /// 定義ジャンプなどで別ファイルを開く。
    OpenFile {
        path: PathBuf,
        position: Option<Position>,
    },
}

/// 補完ポップアップの状態。
struct CompletionState {
    /// 言語サーバーから返った全候補。
    items: Vec<CompletionItem>,
    /// 現在の入力で絞り込んだ結果 (`items` の索引)。
    filtered: Vec<usize>,
    selected: usize,
    /// 補完対象の単語の開始位置。確定時にここから置換する。
    anchor: usize,
}

pub struct EditorView {
    client: BackendClient,
    pub workspace: WorkspaceId,
    pub buffer_id: BufferId,
    pub path: Option<PathBuf>,
    /// 描画用の複製。正本はバックエンドにある。
    buffer: TextBuffer,
    selections: Vec<Selection>,
    config: LanguageConfig,

    /// キャッシュ済みハイライトと、それが有効な行範囲・版数。
    highlights: Vec<HighlightSpan>,
    highlight_rows: Range<u32>,
    highlight_version: u64,
    /// ハイライト要求が飛んでいるか。多重要求を防ぐ。
    highlight_in_flight: bool,

    diagnostics: Vec<Diagnostic>,
    hunks: Vec<DiffHunk>,

    /// スクロール位置 (行単位、小数可)。
    pub scroll_top: f32,
    pub scroll_left: Pixels,

    /// バックエンドが認識している版数。次の編集要求の base_version になる。
    backend_version: u64,
    /// 送信待ちの編集。順序を守るため 1 件ずつ送る。
    pending_edits: Vec<Vec<Edit>>,
    /// 保存要求が待たされているか。編集をすべて送り終えてから保存する。
    pending_save: bool,
    /// 名前を付けて保存する場合の保存先。
    pending_save_path: Option<PathBuf>,
    sending: bool,

    /// 補完候補の表示状態。開いていなければ `None`。
    completion: Option<CompletionState>,
    /// ホバー情報。開いていなければ `None`。
    hover: Option<(HoverInfo, usize)>,

    /// IME の未確定範囲 (文字オフセット)。
    marked_range: Option<Range<usize>>,
    is_selecting: bool,
    /// 直前の描画で確定したレイアウト。マウス座標→文字位置の変換に使う。
    pub last_layout: Option<EditorLayoutInfo>,

    focus_handle: FocusHandle,
}

impl EventEmitter<EditorViewEvent> for EditorView {}

impl EditorView {
    pub fn new(
        client: BackendClient,
        workspace: WorkspaceId,
        snapshot: BufferSnapshot,
        cx: &mut Context<Self>,
    ) -> Self {
        let buffer = TextBuffer::new(&snapshot.text);
        let mut view = Self {
            client,
            workspace,
            buffer_id: snapshot.id,
            path: snapshot.path.clone(),
            buffer,
            selections: vec![Selection::caret(0)],
            config: LanguageConfig::default(),
            highlights: Vec::new(),
            highlight_rows: 0..0,
            highlight_version: u64::MAX,
            highlight_in_flight: false,
            diagnostics: Vec::new(),
            hunks: Vec::new(),
            scroll_top: 0.0,
            scroll_left: px(0.),
            backend_version: snapshot.version,
            pending_edits: Vec::new(),
            pending_save: false,
            pending_save_path: None,
            sending: false,
            completion: None,
            hover: None,
            marked_range: None,
            is_selecting: false,
            last_layout: None,
            focus_handle: cx.focus_handle(),
        };
        view.load_language_config(cx);
        view.refresh_git_hunks(cx);
        view
    }

    pub fn buffer(&self) -> &TextBuffer {
        &self.buffer
    }

    pub fn selections(&self) -> &[Selection] {
        &self.selections
    }

    pub fn config(&self) -> &LanguageConfig {
        &self.config
    }

    pub fn diagnostics(&self) -> &[Diagnostic] {
        &self.diagnostics
    }

    pub fn hunks(&self) -> &[DiffHunk] {
        &self.hunks
    }

    pub fn is_dirty(&self) -> bool {
        self.buffer.is_dirty()
    }

    pub fn marked_range(&self) -> Option<Range<usize>> {
        self.marked_range.clone()
    }

    /// 表示行数ぶんのハイライトを返す。範囲外は空。
    pub fn highlights_for(&self, rows: Range<u32>) -> &[HighlightSpan] {
        if self.highlight_rows.start <= rows.start && rows.end <= self.highlight_rows.end {
            &self.highlights
        } else {
            // 範囲が合わなくても、持っているぶんは描く。空白のまま出すより違和感が小さい。
            &self.highlights
        }
    }

    /// カーソル位置 (先頭カーソル)。
    pub fn primary_cursor(&self) -> usize {
        self.selections.first().map(|s| s.head).unwrap_or(0)
    }

    // -- バックエンドとのやり取り --

    fn load_language_config(&mut self, cx: &mut Context<Self>) {
        let client = self.client.clone();
        let buffer = self.buffer_id;
        cx.spawn(async move |this, cx| {
            if let Ok(Response::LanguageConfig(config)) = client
                .request(Request::BufferLanguageConfig { buffer })
                .await
            {
                this.update(cx, |this, cx| {
                    this.config = config;
                    cx.notify();
                })
                .ok();
            }
        })
        .detach();
    }

    /// 可視範囲のハイライトを要求する。
    pub fn request_highlights(&mut self, rows: Range<u32>, cx: &mut Context<Self>) {
        // 同じ版数・同じ範囲を持っているなら要求しない。
        let covered = self.highlight_version == self.buffer.version()
            && self.highlight_rows.start <= rows.start
            && rows.end <= self.highlight_rows.end;
        if covered || self.highlight_in_flight {
            return;
        }
        self.highlight_in_flight = true;
        // 前後に余裕を持たせて要求する。スクロールのたびに往復しないため。
        let margin = 80;
        let start_row = rows.start.saturating_sub(margin);
        let end_row = rows.end + margin;
        let client = self.client.clone();
        let buffer = self.buffer_id;
        cx.spawn(async move |this, cx| {
            let result = client
                .request(Request::RequestHighlights {
                    buffer,
                    start_row,
                    end_row,
                })
                .await;
            this.update(cx, |this, cx| {
                this.highlight_in_flight = false;
                if let Ok(Response::Highlights {
                    version,
                    start_row,
                    end_row,
                    spans,
                }) = result
                {
                    // 編集が進んで古くなった応答は捨てる。
                    if version == this.buffer.version() {
                        this.highlights = spans;
                        this.highlight_rows = start_row..end_row;
                        this.highlight_version = version;
                        cx.notify();
                    }
                }
            })
            .ok();
        })
        .detach();
    }

    fn refresh_git_hunks(&mut self, cx: &mut Context<Self>) {
        let (Some(path), client, workspace) =
            (self.path.clone(), self.client.clone(), self.workspace)
        else {
            return;
        };
        let contents = self.buffer.text();
        cx.spawn(async move |this, cx| {
            if let Ok(Response::GitHunks(hunks)) = client
                .request(Request::GitDiffHunks {
                    workspace,
                    path,
                    contents: Some(contents),
                })
                .await
            {
                this.update(cx, |this, cx| {
                    this.hunks = hunks;
                    cx.notify();
                })
                .ok();
            }
        })
        .detach();
    }

    pub fn set_diagnostics(&mut self, diagnostics: Vec<Diagnostic>, cx: &mut Context<Self>) {
        self.diagnostics = diagnostics;
        cx.notify();
    }

    /// 外部でファイルが変わったときの差し替え。
    pub fn reload(&mut self, text: &str, version: u64, cx: &mut Context<Self>) {
        self.buffer.reset(text);
        self.backend_version = version;
        self.highlight_version = u64::MAX;
        self.clamp_selections();
        cx.notify();
    }

    /// 編集をバックエンドへ送る。**必ず 1 件ずつ順に送る。**
    ///
    /// 並行して送ると、バックエンド側が要求ごとに別タスクで処理するため
    /// 適用順が入れ替わり、版数が食い違う。
    fn queue_edits(&mut self, edits: Vec<Edit>, cx: &mut Context<Self>) {
        self.pending_edits.push(edits);
        self.flush_edits(cx);
    }

    /// 待ち行列の先頭を 1 つ送る。編集がすべて片付いたら保存を送る。
    ///
    /// 保存を独立した要求として投げてはいけない。バックエンドは要求ごとに別タスクで
    /// 処理するため、まだ送っていない編集を追い越してディスクに古い内容が書かれる。
    fn flush_edits(&mut self, cx: &mut Context<Self>) {
        if self.sending {
            return;
        }
        if !self.pending_edits.is_empty() {
            self.send_next_edit(cx);
        } else if self.pending_save {
            self.pending_save = false;
            self.send_save(cx);
        }
    }

    fn send_next_edit(&mut self, cx: &mut Context<Self>) {
        self.sending = true;
        let edits = self.pending_edits.remove(0);
        let client = self.client.clone();
        let buffer = self.buffer_id;
        let base_version = self.backend_version;
        cx.spawn(async move |this, cx| {
            let result = client
                .request(Request::ApplyEdits {
                    buffer,
                    base_version,
                    edits,
                })
                .await;
            this.update(cx, |this, cx| {
                this.sending = false;
                match result {
                    Ok(Response::BufferVersion { version }) => {
                        this.backend_version = version;
                    }
                    Ok(_) => {}
                    Err(e) => {
                        // 版数がずれた場合、これ以上送っても直らない。取り直しを促す。
                        this.pending_edits.clear();
                        this.pending_save = false;
                        cx.emit(EditorViewEvent::Notify(
                            NotificationLevel::Error,
                            format!("編集を反映できませんでした: {e}"),
                        ));
                    }
                }
                this.flush_edits(cx);
            })
            .ok();
        })
        .detach();
    }

    fn send_save(&mut self, cx: &mut Context<Self>) {
        self.sending = true;
        let client = self.client.clone();
        let buffer = self.buffer_id;
        // 保存要求を出した時点の版数。応答が返るまでに編集が進んでいたら、
        // ディスクの内容は既に古いので未保存のままにする。
        let version_at_save = self.buffer.version();
        let path = self.pending_save_path.take();
        cx.spawn(async move |this, cx| {
            let result = client.request(Request::SaveBuffer { buffer, path }).await;
            this.update(cx, |this, cx| {
                this.sending = false;
                match result {
                    Ok(Response::Saved { path, version }) => {
                        this.backend_version = version;
                        this.path = Some(path);
                        if this.buffer.version() == version_at_save {
                            this.buffer.mark_saved();
                        }
                        this.refresh_git_hunks(cx);
                        cx.emit(EditorViewEvent::Saved);
                        cx.notify();
                    }
                    Ok(_) => {}
                    Err(e) => cx.emit(EditorViewEvent::Notify(
                        NotificationLevel::Error,
                        format!("保存に失敗しました: {e}"),
                    )),
                }
                this.flush_edits(cx);
            })
            .ok();
        })
        .detach();
    }

    // -- 編集の基本操作 --

    /// 選択範囲を置き換える。すべての編集はここを通る。
    fn replace_selections(&mut self, text: &str, cx: &mut Context<Self>) {
        let before = self.selections.clone();
        let edits: Vec<Edit> = self
            .selections
            .iter()
            .map(|s| Edit::replace(s.range(), text))
            .collect();
        self.apply(edits, before, cx);
    }

    /// 編集を適用し、カーソルを追従させ、バックエンドへ送る。
    fn apply(&mut self, edits: Vec<Edit>, before: Vec<Selection>, cx: &mut Context<Self>) {
        if edits.is_empty() {
            return;
        }
        // 適用後のカーソル位置を先に計算する。編集を通すと元の座標が使えなくなるため。
        let after = map_selections_through(&before, &edits);
        if self.buffer.edit(&edits, &before, &after).is_err() {
            return;
        }
        self.selections = after;
        normalize(&mut self.selections);
        self.queue_edits(edits, cx);
        cx.emit(EditorViewEvent::Dirtied);
        cx.notify();
    }

    /// 入力の区切り。ここまでを 1 回の Undo 単位にする。
    fn commit_undo_group(&mut self) {
        self.buffer.commit();
    }

    fn clamp_selections(&mut self) {
        let len = self.buffer.len_chars();
        for sel in &mut self.selections {
            sel.anchor = sel.anchor.min(len);
            sel.head = sel.head.min(len);
        }
        normalize(&mut self.selections);
    }

    /// カーソルが画面に入るようスクロールする。
    fn scroll_to_cursor(&mut self) {
        let Some(layout) = self.last_layout.as_ref() else {
            return;
        };
        let row = self
            .buffer
            .rope()
            .offset_to_position(self.primary_cursor())
            .row as f32;
        let visible = layout.visible_rows.max(1.0);
        // 上下 2 行ぶんの余白を残す。行が画面端に貼り付くと文脈が読めない。
        let margin = 2.0_f32.min(visible / 4.0);
        if row < self.scroll_top + margin {
            self.scroll_top = (row - margin).max(0.0);
        } else if row > self.scroll_top + visible - 1.0 - margin {
            self.scroll_top = (row - visible + 1.0 + margin).max(0.0);
        }
    }

    // -- 移動系アクション --

    fn move_all(&mut self, movement: Movement, direction: Direction, extend: bool) {
        let rope = self.buffer.rope();
        for sel in &mut self.selections {
            move_selection(rope, sel, movement, direction, extend);
        }
        normalize(&mut self.selections);
    }

    fn movement_action(
        &mut self,
        movement: Movement,
        direction: Direction,
        extend: bool,
        cx: &mut Context<Self>,
    ) {
        self.commit_undo_group();
        self.move_all(movement, direction, extend);
        self.scroll_to_cursor();
        cx.notify();
    }

    fn page_rows(&self) -> u32 {
        self.last_layout
            .as_ref()
            .map(|l| (l.visible_rows as u32).saturating_sub(2).max(1))
            .unwrap_or(20)
    }

    // -- アクションハンドラ --

    fn on_move_left(&mut self, _: &MoveLeft, _w: &mut Window, cx: &mut Context<Self>) {
        self.movement_action(Movement::Grapheme, Direction::Backward, false, cx);
    }
    fn on_move_right(&mut self, _: &MoveRight, _w: &mut Window, cx: &mut Context<Self>) {
        self.movement_action(Movement::Grapheme, Direction::Forward, false, cx);
    }
    fn on_move_up(&mut self, _: &MoveUp, _w: &mut Window, cx: &mut Context<Self>) {
        // 補完が開いている間は上下キーで候補を選ぶ。カーソルは動かさない。
        if let Some(state) = self.completion.as_mut() {
            state.selected = state.selected.saturating_sub(1);
            cx.notify();
            return;
        }
        self.hover = None;
        self.movement_action(Movement::Line, Direction::Backward, false, cx);
    }
    fn on_move_down(&mut self, _: &MoveDown, _w: &mut Window, cx: &mut Context<Self>) {
        if let Some(state) = self.completion.as_mut() {
            let last = state.filtered.len().saturating_sub(1).min(11);
            state.selected = (state.selected + 1).min(last);
            cx.notify();
            return;
        }
        self.hover = None;
        self.movement_action(Movement::Line, Direction::Forward, false, cx);
    }
    fn on_move_word_left(&mut self, _: &MoveWordLeft, _w: &mut Window, cx: &mut Context<Self>) {
        self.movement_action(Movement::Word, Direction::Backward, false, cx);
    }
    fn on_move_word_right(&mut self, _: &MoveWordRight, _w: &mut Window, cx: &mut Context<Self>) {
        self.movement_action(Movement::Word, Direction::Forward, false, cx);
    }
    fn on_move_line_start(&mut self, _: &MoveLineStart, _w: &mut Window, cx: &mut Context<Self>) {
        self.movement_action(Movement::LineBoundary, Direction::Backward, false, cx);
    }
    fn on_move_line_end(&mut self, _: &MoveLineEnd, _w: &mut Window, cx: &mut Context<Self>) {
        self.movement_action(Movement::LineBoundary, Direction::Forward, false, cx);
    }
    fn on_move_doc_start(
        &mut self,
        _: &MoveDocumentStart,
        _w: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.movement_action(Movement::Buffer, Direction::Backward, false, cx);
    }
    fn on_move_doc_end(&mut self, _: &MoveDocumentEnd, _w: &mut Window, cx: &mut Context<Self>) {
        self.movement_action(Movement::Buffer, Direction::Forward, false, cx);
    }
    fn on_page_up(&mut self, _: &MovePageUp, _w: &mut Window, cx: &mut Context<Self>) {
        let rows = self.page_rows();
        self.movement_action(Movement::Page(rows), Direction::Backward, false, cx);
    }
    fn on_page_down(&mut self, _: &MovePageDown, _w: &mut Window, cx: &mut Context<Self>) {
        let rows = self.page_rows();
        self.movement_action(Movement::Page(rows), Direction::Forward, false, cx);
    }
    fn on_select_left(&mut self, _: &SelectLeft, _w: &mut Window, cx: &mut Context<Self>) {
        self.movement_action(Movement::Grapheme, Direction::Backward, true, cx);
    }
    fn on_select_right(&mut self, _: &SelectRight, _w: &mut Window, cx: &mut Context<Self>) {
        self.movement_action(Movement::Grapheme, Direction::Forward, true, cx);
    }
    fn on_select_up(&mut self, _: &SelectUp, _w: &mut Window, cx: &mut Context<Self>) {
        self.movement_action(Movement::Line, Direction::Backward, true, cx);
    }
    fn on_select_down(&mut self, _: &SelectDown, _w: &mut Window, cx: &mut Context<Self>) {
        self.movement_action(Movement::Line, Direction::Forward, true, cx);
    }
    fn on_select_word_left(&mut self, _: &SelectWordLeft, _w: &mut Window, cx: &mut Context<Self>) {
        self.movement_action(Movement::Word, Direction::Backward, true, cx);
    }
    fn on_select_word_right(
        &mut self,
        _: &SelectWordRight,
        _w: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.movement_action(Movement::Word, Direction::Forward, true, cx);
    }
    fn on_select_line_start(
        &mut self,
        _: &SelectLineStart,
        _w: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.movement_action(Movement::LineBoundary, Direction::Backward, true, cx);
    }
    fn on_select_line_end(&mut self, _: &SelectLineEnd, _w: &mut Window, cx: &mut Context<Self>) {
        self.movement_action(Movement::LineBoundary, Direction::Forward, true, cx);
    }

    fn on_select_all(&mut self, _: &SelectAll, _w: &mut Window, cx: &mut Context<Self>) {
        self.commit_undo_group();
        self.selections = vec![Selection::new(0, self.buffer.len_chars())];
        cx.notify();
    }

    fn on_backspace(&mut self, _: &Backspace, _w: &mut Window, cx: &mut Context<Self>) {
        self.hover = None;
        let before = self.selections.clone();
        let rope = self.buffer.rope();
        let mut edits = Vec::new();
        for sel in &before {
            if sel.is_empty() {
                if sel.head == 0 {
                    continue;
                }
                let mut probe = *sel;
                move_selection(
                    rope,
                    &mut probe,
                    Movement::Grapheme,
                    Direction::Backward,
                    false,
                );
                edits.push(Edit::delete(TextRange::new(probe.head, sel.head)));
            } else {
                edits.push(Edit::delete(sel.range()));
            }
        }
        self.apply(edits, before, cx);
    }

    fn on_delete(&mut self, _: &Delete, _w: &mut Window, cx: &mut Context<Self>) {
        let before = self.selections.clone();
        let rope = self.buffer.rope();
        let len = rope.len_chars();
        let mut edits = Vec::new();
        for sel in &before {
            if sel.is_empty() {
                if sel.head >= len {
                    continue;
                }
                let mut probe = *sel;
                move_selection(
                    rope,
                    &mut probe,
                    Movement::Grapheme,
                    Direction::Forward,
                    false,
                );
                edits.push(Edit::delete(TextRange::new(sel.head, probe.head)));
            } else {
                edits.push(Edit::delete(sel.range()));
            }
        }
        self.apply(edits, before, cx);
    }

    fn on_delete_word_left(&mut self, _: &DeleteWordLeft, _w: &mut Window, cx: &mut Context<Self>) {
        let before = self.selections.clone();
        let rope = self.buffer.rope();
        let mut edits = Vec::new();
        for sel in &before {
            let mut probe = *sel;
            if sel.is_empty() {
                move_selection(rope, &mut probe, Movement::Word, Direction::Backward, false);
                if probe.head != sel.head {
                    edits.push(Edit::delete(TextRange::new(probe.head, sel.head)));
                }
            } else {
                edits.push(Edit::delete(sel.range()));
            }
        }
        self.apply(edits, before, cx);
    }

    fn on_newline(&mut self, _: &Newline, _w: &mut Window, cx: &mut Context<Self>) {
        if self.accept_completion(cx) {
            return;
        }
        // 改行時は前の行のインデントを引き継ぐ。引き継がないと毎行手で揃えることになる。
        let before = self.selections.clone();
        let rope = self.buffer.rope();
        let mut edits = Vec::new();
        for sel in &before {
            let row = rope.char_to_line(sel.start());
            let line = rope.line_text(row).to_string();
            let indent: String = line
                .chars()
                .take_while(|c| *c == ' ' || *c == '\t')
                .collect();
            // 開き括弧の直後ならもう 1 段深くする。
            let opens_block = line.trim_end().ends_with(['{', '(', '[', ':']);
            let extra = if opens_block {
                if self.config.use_tabs {
                    "\t".to_string()
                } else {
                    " ".repeat(self.config.indent_width as usize)
                }
            } else {
                String::new()
            };
            edits.push(Edit::replace(sel.range(), format!("\n{indent}{extra}")));
        }
        self.commit_undo_group();
        self.apply(edits, before, cx);
        self.commit_undo_group();
        self.scroll_to_cursor();
    }

    fn on_indent(&mut self, _: &Indent, _w: &mut Window, cx: &mut Context<Self>) {
        if self.accept_completion(cx) {
            return;
        }
        let unit = if self.config.use_tabs {
            "\t".to_string()
        } else {
            " ".repeat(self.config.indent_width as usize)
        };
        let before = self.selections.clone();
        // 複数行にまたがる選択があるなら行頭にインデントを足す。無ければただの挿入。
        if before.iter().any(|s| self.spans_multiple_lines(s)) {
            let mut edits = Vec::new();
            for row in self.selected_rows() {
                let offset = self.buffer.rope().line_to_char(row);
                edits.push(Edit::insert(offset, unit.clone()));
            }
            self.apply(edits, before, cx);
        } else {
            self.replace_selections(&unit, cx);
        }
    }

    fn on_outdent(&mut self, _: &Outdent, _w: &mut Window, cx: &mut Context<Self>) {
        let width = self.config.indent_width as usize;
        let before = self.selections.clone();
        let rope = self.buffer.rope();
        let mut edits = Vec::new();
        for row in self.selected_rows() {
            let start = rope.line_to_char(row);
            let line = rope.line_text(row).to_string();
            let removable = if line.starts_with('\t') {
                1
            } else {
                line.chars().take(width).take_while(|c| *c == ' ').count()
            };
            if removable > 0 {
                edits.push(Edit::delete(TextRange::new(start, start + removable)));
            }
        }
        self.apply(edits, before, cx);
    }

    fn on_toggle_comment(&mut self, _: &ToggleComment, _w: &mut Window, cx: &mut Context<Self>) {
        let Some(prefix) = self.config.line_comment.clone() else {
            return;
        };
        let before = self.selections.clone();
        let rope = self.buffer.rope();
        let rows: Vec<usize> = self.selected_rows().collect();
        // 対象行がすべてコメント済みなら外す。1 行でも素のままなら全部つける。
        let all_commented = rows.iter().all(|row| {
            let line = rope.line_text(*row).to_string();
            line.trim().is_empty() || line.trim_start().starts_with(&prefix)
        });
        let mut edits = Vec::new();
        for row in rows {
            let line_start = rope.line_to_char(row);
            let line = rope.line_text(row).to_string();
            if line.trim().is_empty() {
                continue;
            }
            let indent = line.chars().take_while(|c| c.is_whitespace()).count();
            if all_commented {
                let after_indent = &line[line
                    .char_indices()
                    .nth(indent)
                    .map(|(i, _)| i)
                    .unwrap_or(line.len())..];
                if let Some(rest) = after_indent.strip_prefix(&prefix) {
                    // 接頭辞のあとの空白 1 つも一緒に外す。付けるときに入れているため。
                    let extra = usize::from(rest.starts_with(' '));
                    let from = line_start + indent;
                    edits.push(Edit::delete(TextRange::new(
                        from,
                        from + prefix.chars().count() + extra,
                    )));
                }
            } else {
                edits.push(Edit::insert(line_start + indent, format!("{prefix} ")));
            }
        }
        self.apply(edits, before, cx);
    }

    fn on_duplicate_line(&mut self, _: &DuplicateLine, _w: &mut Window, cx: &mut Context<Self>) {
        let before = self.selections.clone();
        let rope = self.buffer.rope();
        let mut edits = Vec::new();
        for row in self.selected_rows() {
            let start = rope.line_to_char(row);
            let text = rope.line_text(row).to_string();
            edits.push(Edit::insert(start, format!("{text}\n")));
        }
        self.apply(edits, before, cx);
    }

    fn on_delete_line(&mut self, _: &DeleteLine, _w: &mut Window, cx: &mut Context<Self>) {
        let before = self.selections.clone();
        let rope = self.buffer.rope();
        let rows: Vec<usize> = self.selected_rows().collect();
        let mut edits = Vec::new();
        for row in rows {
            let start = rope.line_to_char(row);
            let end = if row + 1 < rope.len_lines() {
                rope.line_to_char(row + 1)
            } else {
                rope.len_chars()
            };
            edits.push(Edit::delete(TextRange::new(start, end)));
        }
        self.apply(edits, before, cx);
    }

    fn on_undo(&mut self, _: &Undo, _w: &mut Window, cx: &mut Context<Self>) {
        self.commit_undo_group();
        let backend_len = self.buffer.len_chars();
        let Some((selections, _)) = self.buffer.undo() else {
            return;
        };
        self.selections = selections;
        self.clamp_selections();
        self.resync_backend(backend_len, cx);
        self.scroll_to_cursor();
        cx.emit(EditorViewEvent::Dirtied);
        cx.notify();
    }

    fn on_redo(&mut self, _: &Redo, _w: &mut Window, cx: &mut Context<Self>) {
        let backend_len = self.buffer.len_chars();
        let Some((selections, _)) = self.buffer.redo() else {
            return;
        };
        self.selections = selections;
        self.clamp_selections();
        self.resync_backend(backend_len, cx);
        self.scroll_to_cursor();
        cx.emit(EditorViewEvent::Dirtied);
        cx.notify();
    }

    /// Undo/Redo 後、バックエンドの正本を全文で合わせ直す。
    ///
    /// 逆編集を送る方が転送量は小さいが、GUI 側の履歴とバックエンド側の版数を
    /// 両方追いかける必要が出て壊れやすい。全文置換なら 1 回で確実に一致する。
    ///
    /// `backend_len` には **Undo/Redo を適用する直前の** 文字数を渡す。
    /// その時点では GUI とバックエンドの内容が一致しているので、それが正本の長さになる。
    fn resync_backend(&mut self, backend_len: usize, cx: &mut Context<Self>) {
        let text = self.buffer.text();
        self.queue_edits(
            vec![Edit::replace(TextRange::new(0, backend_len), text)],
            cx,
        );
    }

    fn on_cut(&mut self, _: &Cut, _w: &mut Window, cx: &mut Context<Self>) {
        let text = self.selected_text();
        if text.is_empty() {
            return;
        }
        cx.write_to_clipboard(ClipboardItem::new_string(text));
        self.replace_selections("", cx);
    }

    fn on_copy(&mut self, _: &Copy, _w: &mut Window, cx: &mut Context<Self>) {
        let text = self.selected_text();
        if !text.is_empty() {
            cx.write_to_clipboard(ClipboardItem::new_string(text));
        }
    }

    fn on_paste(&mut self, _: &Paste, _w: &mut Window, cx: &mut Context<Self>) {
        let Some(text) = cx.read_from_clipboard().and_then(|item| item.text()) else {
            return;
        };
        self.commit_undo_group();
        self.replace_selections(&text, cx);
        self.commit_undo_group();
        self.scroll_to_cursor();
    }

    fn on_add_cursor_above(&mut self, _: &AddCursorAbove, _w: &mut Window, cx: &mut Context<Self>) {
        self.add_cursor_vertically(Direction::Backward, cx);
    }

    fn on_add_cursor_below(&mut self, _: &AddCursorBelow, _w: &mut Window, cx: &mut Context<Self>) {
        self.add_cursor_vertically(Direction::Forward, cx);
    }

    fn add_cursor_vertically(&mut self, direction: Direction, cx: &mut Context<Self>) {
        let rope = self.buffer.rope();
        // 進行方向の端にあるカーソルを基準に 1 本足す。
        let anchor_sel = match direction {
            Direction::Backward => self.selections.iter().min_by_key(|s| s.head),
            Direction::Forward => self.selections.iter().max_by_key(|s| s.head),
        };
        let Some(mut probe) = anchor_sel.copied() else {
            return;
        };
        let row_before = rope.char_to_line(probe.head);
        move_selection(rope, &mut probe, Movement::Line, direction, false);
        if rope.char_to_line(probe.head) == row_before {
            return; // 端に達している
        }
        self.selections.push(Selection::caret(probe.head));
        normalize(&mut self.selections);
        cx.notify();
    }

    fn on_select_next_occurrence(
        &mut self,
        _: &SelectNextOccurrence,
        _w: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(last) = self.selections.last().copied() else {
            return;
        };
        // 選択が無いときはまず単語を選ぶ。あるときは同じ文字列の次の出現を足す。
        if last.is_empty() {
            let rope = self.buffer.rope();
            let mut start = last.head;
            let mut end = last.head;
            while start > 0 && is_word_char(rope.char(start - 1)) {
                start -= 1;
            }
            while end < rope.len_chars() && is_word_char(rope.char(end)) {
                end += 1;
            }
            if start < end {
                self.selections = vec![Selection::new(start, end)];
                cx.notify();
            }
            return;
        }
        let needle = self.text_in(last.range());
        let haystack = self.buffer.text();
        let from_byte = char_to_byte(&haystack, last.end());
        let found = haystack[from_byte..]
            .find(&needle)
            .map(|i| from_byte + i)
            .or_else(|| haystack.find(&needle));
        if let Some(byte) = found {
            let start = byte_to_char(&haystack, byte);
            let end = start + needle.chars().count();
            if !self.selections.iter().any(|s| s.start() == start) {
                self.selections.push(Selection::new(start, end));
                normalize(&mut self.selections);
                self.scroll_to_cursor();
                cx.notify();
            }
        }
    }

    fn on_go_to_definition(&mut self, _: &GoToDefinition, _w: &mut Window, cx: &mut Context<Self>) {
        let position = self.buffer.rope().offset_to_position(self.primary_cursor());
        let client = self.client.clone();
        let buffer = self.buffer_id;
        cx.spawn(async move |this, cx| {
            let result = client
                .request(Request::LspDefinition { buffer, position })
                .await;
            this.update(cx, |_this, cx| match result {
                Ok(Response::Locations(locations)) if !locations.is_empty() => {
                    let target = &locations[0];
                    cx.emit(EditorViewEvent::OpenFile {
                        path: target.path.clone(),
                        position: Some(target.range.start),
                    });
                }
                Ok(_) => cx.emit(EditorViewEvent::Notify(
                    NotificationLevel::Info,
                    "定義が見つかりません".into(),
                )),
                Err(e) => cx.emit(EditorViewEvent::Notify(
                    NotificationLevel::Warning,
                    format!("定義ジャンプに失敗しました: {e}"),
                )),
            })
            .ok();
        })
        .detach();
    }

    fn on_format(&mut self, _: &Format, _w: &mut Window, cx: &mut Context<Self>) {
        let client = self.client.clone();
        let buffer = self.buffer_id;
        cx.spawn(async move |this, cx| {
            let result = client.request(Request::LspFormat { buffer }).await;
            this.update(cx, |this, cx| match result {
                Ok(Response::TextEdits(edits)) if !edits.is_empty() => {
                    let rope = this.buffer.rope();
                    let converted: Vec<Edit> = edits
                        .iter()
                        .map(|e| {
                            Edit::replace(
                                TextRange::new(
                                    rope.position_to_offset(e.range.start),
                                    rope.position_to_offset(e.range.end),
                                ),
                                e.new_text.clone(),
                            )
                        })
                        .collect();
                    let before = this.selections.clone();
                    this.commit_undo_group();
                    this.apply(converted, before, cx);
                    this.commit_undo_group();
                }
                Ok(_) => {}
                Err(e) => cx.emit(EditorViewEvent::Notify(
                    NotificationLevel::Warning,
                    format!("整形に失敗しました: {e}"),
                )),
            })
            .ok();
        })
        .detach();
    }

    /// 保存する。送信待ちの編集がすべて届いてから書き込まれる。
    pub fn save(&mut self, cx: &mut Context<Self>) {
        self.commit_undo_group();
        self.pending_save = true;
        self.flush_edits(cx);
    }

    /// 名前を付けて保存する。
    pub fn save_as(&mut self, path: PathBuf, cx: &mut Context<Self>) {
        self.commit_undo_group();
        self.pending_save_path = Some(path);
        self.pending_save = true;
        self.flush_edits(cx);
    }

    /// 指定位置へカーソルを移動して表示する。
    pub fn reveal_position(&mut self, position: Position, cx: &mut Context<Self>) {
        let offset = self.buffer.rope().position_to_offset(position);
        self.selections = vec![Selection::caret(offset)];
        // 目的行が画面中央に来るようにする。端に出されると前後の文脈が読めない。
        let visible = self
            .last_layout
            .as_ref()
            .map(|l| l.visible_rows)
            .unwrap_or(30.0);
        self.scroll_top = (position.row as f32 - visible / 2.0).max(0.0);
        cx.notify();
    }

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
    fn request_completion(&mut self, trigger: Option<String>, cx: &mut Context<Self>) {
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
    fn refilter_completion(&mut self) {
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
    fn accept_completion(&mut self, cx: &mut Context<Self>) -> bool {
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

    fn on_trigger_completion(
        &mut self,
        _: &TriggerCompletion,
        _w: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.request_completion(None, cx);
    }

    fn on_dismiss_completion(
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

    fn on_show_hover(&mut self, _: &ShowHover, _w: &mut Window, cx: &mut Context<Self>) {
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

    fn render_completion_popup(&self, cx: &mut Context<Self>) -> Option<gpui::AnyElement> {
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

    fn render_hover_card(&self, cx: &mut Context<Self>) -> Option<gpui::AnyElement> {
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

    // -- マウス --

    fn on_mouse_down(
        &mut self,
        event: &MouseDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if event.button != MouseButton::Left {
            return;
        }
        window.focus(&self.focus_handle);
        let Some(offset) = self.offset_for_position(event.position) else {
            return;
        };
        self.commit_undo_group();
        self.is_selecting = true;
        if event.modifiers.alt {
            // Option + クリックでカーソルを追加する。
            self.selections.push(Selection::caret(offset));
            normalize(&mut self.selections);
        } else if event.modifiers.shift {
            if let Some(last) = self.selections.last_mut() {
                last.head = offset;
            }
        } else if event.click_count >= 2 {
            self.selections = vec![self.word_at(offset)];
            self.is_selecting = false;
        } else {
            self.selections = vec![Selection::caret(offset)];
        }
        cx.notify();
    }

    fn on_mouse_move(&mut self, event: &MouseMoveEvent, _w: &mut Window, cx: &mut Context<Self>) {
        if !self.is_selecting {
            return;
        }
        if let Some(offset) = self.offset_for_position(event.position)
            && let Some(last) = self.selections.last_mut()
        {
            last.head = offset;
            cx.notify();
        }
    }

    fn on_mouse_up(&mut self, _: &MouseUpEvent, _w: &mut Window, _cx: &mut Context<Self>) {
        self.is_selecting = false;
    }

    fn on_scroll_wheel(
        &mut self,
        event: &ScrollWheelEvent,
        _w: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(layout) = self.last_layout.as_ref() else {
            return;
        };
        let delta = event.delta.pixel_delta(layout.line_height);
        let lines = f32::from(delta.y) / f32::from(layout.line_height);
        let max = (self.buffer.len_lines() as f32 - 1.0).max(0.0);
        self.scroll_top = (self.scroll_top - lines).clamp(0.0, max);
        self.scroll_left = (self.scroll_left - delta.x).max(px(0.));
        cx.notify();
    }

    /// 画面座標を文字オフセットに変換する。
    pub fn offset_for_position(&self, position: Point<Pixels>) -> Option<usize> {
        let layout = self.last_layout.as_ref()?;
        let relative_y = position.y - layout.text_origin.y;
        let row_offset = (f32::from(relative_y) / f32::from(layout.line_height)).floor();
        let row = (self.scroll_top + row_offset).max(0.0) as usize;
        let row = row.min(self.buffer.len_lines().saturating_sub(1));

        let rope = self.buffer.rope();
        let line_start = rope.line_to_char(row);
        let visible_index = row.checked_sub(layout.first_row)?;
        let Some(shaped) = layout.lines.get(visible_index) else {
            return Some(line_start);
        };
        let x = position.x - layout.text_origin.x + self.scroll_left;
        // x_for_index/index_for_x はバイト索引で動く。行内バイト → 行内文字に直す。
        let byte_in_line = shaped.index_for_x(x).unwrap_or_else(|| shaped.text.len());
        let char_in_line = shaped.text[..byte_in_line.min(shaped.text.len())]
            .chars()
            .count();
        Some(line_start + char_in_line)
    }

    fn word_at(&self, offset: usize) -> Selection {
        let rope = self.buffer.rope();
        let len = rope.len_chars();
        let mut start = offset.min(len);
        let mut end = start;
        while start > 0 && is_word_char(rope.char(start - 1)) {
            start -= 1;
        }
        while end < len && is_word_char(rope.char(end)) {
            end += 1;
        }
        Selection::new(start, end)
    }

    // -- 補助 --

    fn selected_text(&self) -> String {
        self.selections
            .iter()
            .filter(|s| !s.is_empty())
            .map(|s| self.text_in(s.range()))
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn text_in(&self, range: TextRange) -> String {
        let len = self.buffer.len_chars();
        self.buffer
            .rope()
            .slice(range.start.min(len)..range.end.min(len))
            .to_string()
    }

    fn spans_multiple_lines(&self, sel: &Selection) -> bool {
        let rope = self.buffer.rope();
        rope.char_to_line(sel.start()) != rope.char_to_line(sel.end())
    }

    /// 選択に含まれる行番号 (降順)。
    ///
    /// 降順で返すのは、行頭への挿入・削除を後ろの行から適用すれば
    /// 前の行のオフセットが動かないため。
    fn selected_rows(&self) -> impl Iterator<Item = usize> + use<> {
        let rope = self.buffer.rope();
        let mut rows: Vec<usize> = Vec::new();
        for sel in &self.selections {
            let from = rope.char_to_line(sel.start());
            let to = rope.char_to_line(sel.end());
            for row in from..=to {
                if !rows.contains(&row) {
                    rows.push(row);
                }
            }
        }
        rows.sort_unstable();
        rows.reverse();
        rows.into_iter()
    }
}

fn is_word_char(c: char) -> bool {
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

fn char_to_byte(text: &str, char_offset: usize) -> usize {
    text.char_indices()
        .nth(char_offset)
        .map(|(i, _)| i)
        .unwrap_or(text.len())
}

fn byte_to_char(text: &str, byte_offset: usize) -> usize {
    text[..byte_offset.min(text.len())].chars().count()
}

/// 編集適用後のカーソル位置を求める。
///
/// 編集の範囲はすべて **適用前** の座標系。したがって各カーソルについて、
/// 元の位置と各編集の範囲を比べてずれ幅を合計する。累積した位置と比べてしまうと、
/// 2 つ目以降の編集の座標系が食い違う。
///
/// 挿入がカーソル位置ちょうどで起きた場合はカーソルを挿入テキストの右へ送る。
/// これが「文字を打つとカーソルがその右へ動く」挙動になる。
pub fn map_selections_through(before: &[Selection], edits: &[Edit]) -> Vec<Selection> {
    before
        .iter()
        .map(|sel| {
            let head = map_offset_through_edits(sel.head, edits);
            let anchor = map_offset_through_edits(sel.anchor, edits);
            // 編集後は選択を解除してキャレットにする。
            Selection::caret(head.max(anchor))
        })
        .collect()
}

fn map_offset_through_edits(offset: usize, edits: &[Edit]) -> usize {
    let mut shift: isize = 0;
    let mut inside: Option<(usize, usize)> = None;
    for edit in edits {
        let new_len = edit.text.chars().count();
        if edit.range.end <= offset {
            shift += new_len as isize - edit.range.len() as isize;
        } else if edit.range.start <= offset {
            // 置換された範囲の内側にいたカーソルは、置換後テキストの末尾へ寄せる。
            inside = Some((edit.range.start, new_len));
        }
    }
    match inside {
        Some((start, new_len)) => (start as isize + shift) as usize + new_len,
        None => (offset as isize + shift).max(0) as usize,
    }
}

impl Focusable for EditorView {
    fn focus_handle(&self, _cx: &gpui::App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl EntityInputHandler for EditorView {
    fn text_for_range(
        &mut self,
        range_utf16: Range<usize>,
        adjusted: &mut Option<Range<usize>>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<String> {
        let text = self.buffer.text();
        let range = utf16_range_to_char(&text, range_utf16);
        *adjusted = Some(char_range_to_utf16(&text, range.clone()));
        Some(self.buffer.rope().slice(range.start..range.end).to_string())
    }

    fn selected_text_range(
        &mut self,
        _ignore_disabled: bool,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<UTF16Selection> {
        let sel = *self.selections.first()?;
        let text = self.buffer.text();
        Some(UTF16Selection {
            range: char_range_to_utf16(&text, sel.start()..sel.end()),
            reversed: sel.head < sel.anchor,
        })
    }

    fn marked_text_range(
        &self,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<Range<usize>> {
        let marked = self.marked_range.clone()?;
        let text = self.buffer.text();
        Some(char_range_to_utf16(&text, marked))
    }

    fn unmark_text(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        self.marked_range = None;
        cx.notify();
    }

    fn replace_text_in_range(
        &mut self,
        range_utf16: Option<Range<usize>>,
        new_text: &str,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let text = self.buffer.text();
        let target = range_utf16
            .map(|r| utf16_range_to_char(&text, r))
            .or_else(|| self.marked_range.clone());

        let before = self.selections.clone();
        let edits = match target {
            // IME 確定や範囲指定の置換は、その範囲だけを差し替える。
            Some(range) => vec![Edit::replace(
                TextRange::new(range.start, range.end),
                new_text,
            )],
            None => before
                .iter()
                .map(|s| Edit::replace(s.range(), new_text))
                .collect(),
        };
        self.marked_range = None;
        self.hover = None;
        self.apply(edits, before, cx);
        self.scroll_to_cursor();

        // 識別子を打っている間は候補を出し直す。`.` や `:` は言語サーバー側の
        // 誘発文字として扱われることが多いので、明示的に渡す。
        let last = new_text.chars().last();
        match last {
            Some(c) if is_word_char(c) => {
                if self.completion.is_some() {
                    self.refilter_completion();
                } else if new_text.chars().count() == 1 {
                    self.request_completion(None, cx);
                }
            }
            Some(c @ ('.' | ':' | '>')) => {
                self.request_completion(Some(c.to_string()), cx);
            }
            _ => self.completion = None,
        }
    }

    fn replace_and_mark_text_in_range(
        &mut self,
        range_utf16: Option<Range<usize>>,
        new_text: &str,
        new_selected_range: Option<Range<usize>>,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let text = self.buffer.text();
        let target = range_utf16
            .map(|r| utf16_range_to_char(&text, r))
            .or_else(|| self.marked_range.clone())
            .unwrap_or_else(|| {
                let sel = self
                    .selections
                    .first()
                    .copied()
                    .unwrap_or(Selection::caret(0));
                sel.start()..sel.end()
            });

        let before = self.selections.clone();
        let edits = vec![Edit::replace(
            TextRange::new(target.start, target.end),
            new_text,
        )];
        self.apply(edits, before, cx);

        // 未確定範囲を覚えておく。描画側が下線を引く。
        let inserted_len = new_text.chars().count();
        self.marked_range = (inserted_len > 0).then(|| target.start..target.start + inserted_len);

        if let Some(selected) = new_selected_range {
            let marked_text = new_text;
            let start = target.start + utf16_to_char_offset(marked_text, selected.start);
            let end = target.start + utf16_to_char_offset(marked_text, selected.end);
            self.selections = vec![Selection::new(start, end)];
        }
        cx.notify();
    }

    fn bounds_for_range(
        &mut self,
        range_utf16: Range<usize>,
        element_bounds: Bounds<Pixels>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<Bounds<Pixels>> {
        // 変換候補ウィンドウの表示位置。範囲の先頭の座標を返す。
        let layout = self.last_layout.as_ref()?;
        let text = self.buffer.text();
        let range = utf16_range_to_char(&text, range_utf16);
        let position = self.buffer.rope().offset_to_position(range.start);
        let visible_index = (position.row as usize).checked_sub(layout.first_row)?;
        let shaped = layout.lines.get(visible_index)?;
        let byte_in_line = shaped
            .text
            .char_indices()
            .nth(position.column as usize)
            .map(|(i, _)| i)
            .unwrap_or(shaped.text.len());
        let x = layout.text_origin.x + shaped.x_for_index(byte_in_line) - self.scroll_left;
        let y = layout.text_origin.y + layout.line_height * (position.row as f32 - self.scroll_top);
        Some(Bounds::new(
            gpui::point(x, y),
            gpui::size(px(2.), layout.line_height),
        ))
        .filter(|b| element_bounds.contains(&b.origin))
    }

    fn character_index_for_point(
        &mut self,
        point: Point<Pixels>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<usize> {
        let offset = self.offset_for_position(point)?;
        let text = self.buffer.text();
        Some(char_range_to_utf16(&text, offset..offset).start)
    }
}

// -- UTF-16 との相互変換 --
//
// macOS の入力メソッドは UTF-16 コードユニットで位置を指定してくる。
// Nebula 内部は文字 (char) 単位なので、境界での取り違えを防ぐため
// 変換をこの 3 関数に閉じ込める。

fn char_range_to_utf16(text: &str, range: Range<usize>) -> Range<usize> {
    let mut char_index = 0usize;
    let mut utf16_index = 0usize;
    let mut start = None;
    let mut end = None;
    for c in text.chars() {
        if char_index == range.start {
            start = Some(utf16_index);
        }
        if char_index == range.end {
            end = Some(utf16_index);
        }
        char_index += 1;
        utf16_index += c.len_utf16();
    }
    Range {
        start: start.unwrap_or(utf16_index),
        end: end.unwrap_or(utf16_index),
    }
}

fn utf16_range_to_char(text: &str, range: Range<usize>) -> Range<usize> {
    Range {
        start: utf16_to_char_offset(text, range.start),
        end: utf16_to_char_offset(text, range.end),
    }
}

fn utf16_to_char_offset(text: &str, utf16_offset: usize) -> usize {
    let mut char_index = 0usize;
    let mut utf16_index = 0usize;
    for c in text.chars() {
        if utf16_index >= utf16_offset {
            return char_index;
        }
        utf16_index += c.len_utf16();
        char_index += 1;
    }
    char_index
}

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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn utf16_と文字オフセットが往復する() {
        // "a" (1) + "あ" (1) + "𝄞" (サロゲートペア = 2) + "b" (1)
        let text = "aあ𝄞b";
        assert_eq!(char_range_to_utf16(text, 0..4), 0..5);
        assert_eq!(
            char_range_to_utf16(text, 2..3),
            2..4,
            "サロゲートペア 1 文字"
        );
        assert_eq!(utf16_range_to_char(text, 0..5), 0..4);
        assert_eq!(utf16_range_to_char(text, 2..4), 2..3);
    }

    #[test]
    fn utf16_オフセットが範囲外でも末尾に丸まる() {
        assert_eq!(utf16_to_char_offset("abc", 99), 3);
    }

    #[test]
    fn 編集後のカーソルが挿入文字の右に来る() {
        let before = vec![Selection::caret(3)];
        let edits = vec![Edit::insert(3, "xy")];
        assert_eq!(
            map_selections_through(&before, &edits),
            vec![Selection::caret(5)]
        );
    }

    #[test]
    fn 選択を置換するとカーソルが置換後の末尾に来る() {
        let before = vec![Selection::new(2, 6)];
        let edits = vec![Edit::replace(TextRange::new(2, 6), "Z")];
        assert_eq!(
            map_selections_through(&before, &edits),
            vec![Selection::caret(3)]
        );
    }

    #[test]
    fn 複数カーソルの編集で後ろのカーソルもずれる() {
        let before = vec![Selection::caret(1), Selection::caret(5)];
        let edits = vec![Edit::insert(1, "ab"), Edit::insert(5, "cd")];
        let after = map_selections_through(&before, &edits);
        assert_eq!(after[0], Selection::caret(3), "1 番目は自分の挿入ぶん");
        assert_eq!(after[1], Selection::caret(9), "2 番目は両方の挿入ぶん");
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
}
