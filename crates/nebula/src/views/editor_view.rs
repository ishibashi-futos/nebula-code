//! 1 バッファぶんのエディタ。
//!
//! GUI 側はバッファの **複製** を持つ。キー入力はまずこの複製へ適用して即座に再描画し、
//! 同じ編集をバックエンドへ送る。打鍵ごとに IPC の往復を待つと、体感が
//! ネットワーク越しのエディタになるため。
//!
//! 構文ハイライトだけはバックエンドが計算する。GUI で Tree-sitter を動かさないのが
//! 「軽量 GUI 描画プロセス」という設計の要。ハイライトは 1〜2 フレーム遅れて追いつく。

mod completion;
mod edit_map;
mod ime;
mod mouse;
mod render;

use crate::actions::*;
use crate::ipc_client::BackendClient;
use crate::views::editor_element::EditorLayoutInfo;
use completion::is_word_char;
use edit_map::map_selections_through;
use gpui::{ClipboardItem, Context, EventEmitter, FocusHandle, Focusable, Pixels, Window, px};
use nebula_core::markdown::{ListContinuation, list_continuation, marker_end_column};
use nebula_core::selection::{Direction, Movement, move_selection, normalize};
use nebula_core::{RopeExt, Selection, TextBuffer};
use nebula_protocol::{
    BufferId, BufferSnapshot, CompletionItem, Diagnostic, DiffHunk, Edit, HighlightSpan, HoverInfo,
    LanguageConfig, NotificationLevel, Position, Request, Response, TextRange, WorkspaceId,
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
        // Markdown は箇条書き・チェックリスト・引用を改行時に継続する、文章向けの
        // 別ルールを使う。他の言語のブロック開始インデント判定 (opens_block) を
        // 混ぜると、地の文で行末が ':' のときに誤発火したり、リスト継続と二重に
        // インデントが入ったりするので、Markdown のときは opens_block を使わない。
        let is_markdown = self.config.language.as_deref() == Some("markdown");
        let rope = self.buffer.rope();
        let mut edits = Vec::new();
        for sel in &before {
            let row = rope.char_to_line(sel.start());
            let line = rope.line_text(row).to_string();
            let indent: String = line
                .chars()
                .take_while(|c| *c == ' ' || *c == '\t')
                .collect();

            if is_markdown {
                match list_continuation(&line) {
                    Some(ListContinuation::Continue(prefix)) => {
                        // キャレットがマーカーより手前 (行頭やマーカーの内部) にある
                        // ときに継続すると、`- abc` の行頭で改行しただけで
                        // `\n- - abc` のようにマーカーが二重になる。マーカーを
                        // 打ち終わった後ろにいるときだけ継続する。
                        let column = sel.start() - rope.line_to_char(row);
                        let after_marker =
                            marker_end_column(&line).is_some_and(|end| column >= end);
                        if after_marker {
                            edits.push(Edit::replace(sel.range(), format!("\n{prefix}")));
                        } else {
                            edits.push(Edit::replace(sel.range(), format!("\n{indent}")));
                        }
                    }
                    Some(ListContinuation::Terminate) if sel.is_empty() => {
                        // マーカーだけで中身が空の行だったのでリストから抜ける。
                        // 改行はせず、現在行のマーカーを消して空行にする。
                        let line_range =
                            TextRange::new(rope.line_to_char(row), rope.line_end_offset(row));
                        edits.push(Edit::replace(line_range, String::new()));
                    }
                    Some(ListContinuation::Terminate) => {
                        // 選択範囲がある Enter は行全体を消す特殊処理と噛み合わない
                        // (選択が行をまたいでいると選択後半の文字が消えずに残ってしまう)。
                        // キャレット単体のときだけ打ち切りにして、選択があるときは
                        // 普通に選択を改行で置き換える。
                        edits.push(Edit::replace(sel.range(), format!("\n{indent}")));
                    }
                    None => {
                        edits.push(Edit::replace(sel.range(), format!("\n{indent}")));
                    }
                }
                continue;
            }

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

fn char_to_byte(text: &str, char_offset: usize) -> usize {
    text.char_indices()
        .nth(char_offset)
        .map(|(i, _)| i)
        .unwrap_or(text.len())
}

fn byte_to_char(text: &str, byte_offset: usize) -> usize {
    text[..byte_offset.min(text.len())].chars().count()
}

impl Focusable for EditorView {
    fn focus_handle(&self, _cx: &gpui::App) -> FocusHandle {
        self.focus_handle.clone()
    }
}
