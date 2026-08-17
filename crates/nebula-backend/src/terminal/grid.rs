//! セルグリッドと ANSI エスケープシーケンスの解釈。
//!
//! PTY から完全に切り離してあるのは、端末エミュレーションが最も壊れやすく、
//! かつ「バイト列を入れてグリッドの状態を見る」形で丸ごと単体テストできる部分だから。
//! ここにプロセス起動やスレッドを混ぜると、その性質が失われる。

use nebula_protocol::{TermColor, TerminalCell, TerminalId, TerminalUpdate, cell_flags};
use std::borrow::Cow;
use std::collections::VecDeque;
use unicode_width::UnicodeWidthChar;
use vte::{Params, Perform};

/// スクロールバックの保持行数。
///
/// 1 行あたり数百バイトになるため、無制限にすると長時間動かしたバックエンドの
/// 常駐メモリが読めなくなる。
const SCROLLBACK_LIMIT: usize = 5000;

/// タブ幅。可変にする実装 (CSI ... W) は使う側がほぼ居ないので固定にしている。
const TAB_WIDTH: usize = 8;

/// 現在の文字装飾。SGR で更新し、書き込むセルへ複写する。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Pen {
    fg: TermColor,
    bg: TermColor,
    flags: u8,
}

impl Default for Pen {
    fn default() -> Self {
        Self {
            fg: TermColor::Default,
            bg: TermColor::Default,
            flags: 0,
        }
    }
}

impl Pen {
    fn cell(&self, ch: char, extra_flags: u8) -> TerminalCell {
        TerminalCell {
            ch,
            fg: self.fg,
            bg: self.bg,
            flags: self.flags | extra_flags,
        }
    }
}

/// 表示領域の 1 行。
struct Line {
    cells: Vec<TerminalCell>,
    /// 前回 GUI へ送ってから変化したか。差分送信の単位。
    dirty: bool,
}

impl Line {
    fn blank(cols: usize) -> Self {
        Self {
            cells: vec![TerminalCell::default(); cols],
            dirty: true,
        }
    }
}

/// 代替画面へ切り替える前の主画面。復帰時にそのまま戻す。
///
/// スクロール領域まで持つのは、代替画面のアプリ (vim など) が DECSTBM で設定した
/// 領域を残したまま抜けると、戻ったシェルのスクロールが画面の一部でしか起きなくなるため。
struct MainScreen {
    lines: Vec<Line>,
    cursor: (usize, usize),
    scroll_region: (usize, usize),
}

/// パーサとグリッドの組。
///
/// `vte::Parser` は `Perform` を可変借用するため、[`TerminalGrid`] 自身には持たせられない。
/// 分割された 2 つを 1 か所で束ねて、呼び出し側がパーサの寿命を気にせずに済むようにする。
pub struct TerminalEmulator {
    parser: vte::Parser,
    grid: TerminalGrid,
}

impl TerminalEmulator {
    pub fn new(rows: u16, cols: u16) -> Self {
        Self {
            parser: vte::Parser::new(),
            grid: TerminalGrid::new(rows as usize, cols as usize),
        }
    }

    /// PTY から読んだ生バイト列を食わせる。途中で切れたシーケンスはパーサが持ち越す。
    pub fn advance(&mut self, bytes: &[u8]) {
        self.parser.advance(&mut self.grid, bytes);
    }

    pub fn grid_mut(&mut self) -> &mut TerminalGrid {
        &mut self.grid
    }
}

pub struct TerminalGrid {
    rows: usize,
    cols: usize,
    lines: Vec<Line>,
    scrollback: VecDeque<Vec<TerminalCell>>,
    cursor_row: usize,
    /// 桁位置。行末まで書いた直後は `cols` を取り得る (遅延折り返し)。
    cursor_col: usize,
    saved_cursor: (usize, usize),
    cursor_visible: bool,
    pen: Pen,
    /// DECSTBM のスクロール領域 (両端を含む 0 始まりの行番号)。
    scroll_top: usize,
    scroll_bottom: usize,
    /// スクロールバックを何行遡って表示しているか。0 が最下部。
    view_offset: usize,
    /// 代替画面表示中に退避してある主画面。`Some` の間が代替画面。
    main_screen: Option<MainScreen>,
    /// DECCKM。カーソルキーを `ESC O A` 形式で送るか。
    application_cursor_keys: bool,
    /// DEC 特殊図形集合 (`ESC ( 0`) を選択中か。
    dec_graphics: bool,
    /// 端末クエリ (DSR / DA) への応答。呼び出し側が取り出して PTY へ書き戻す。
    ///
    /// グリッドから PTY を直接触らないのは、このモジュールを「バイト列を入れて
    /// 状態を見る」だけで試せる形に保つため。
    responses: Vec<u8>,
    title: Option<String>,
    title_dirty: bool,
    bell: bool,
    /// 行単位の追跡では足りず全行を送り直す必要がある状態か。
    redraw_all: bool,
    last_cursor: (u16, u16, bool),
}

impl TerminalGrid {
    pub fn new(rows: usize, cols: usize) -> Self {
        let rows = rows.max(1);
        let cols = cols.max(1);
        Self {
            rows,
            cols,
            lines: (0..rows).map(|_| Line::blank(cols)).collect(),
            scrollback: VecDeque::new(),
            cursor_row: 0,
            cursor_col: 0,
            saved_cursor: (0, 0),
            cursor_visible: true,
            pen: Pen::default(),
            scroll_top: 0,
            scroll_bottom: rows - 1,
            view_offset: 0,
            main_screen: None,
            application_cursor_keys: false,
            dec_graphics: false,
            responses: Vec::new(),
            title: None,
            title_dirty: false,
            bell: false,
            // 生成直後は GUI が何も持っていないので、初回は全画面を送る。
            redraw_all: true,
            last_cursor: (0, 0, true),
        }
    }

    // -----------------------------------------------------------------------
    // 差分の取り出し
    // -----------------------------------------------------------------------

    /// 前回から変化した内容を取り出す。変化が無ければ `None`。
    ///
    /// 取り出した印はここで消す。消さないと 60Hz のタイマーが同じ内容を送り続ける。
    pub fn take_update(&mut self, id: TerminalId) -> Option<TerminalUpdate> {
        let cursor = self.reported_cursor();
        let cursor_changed = cursor != self.last_cursor;
        let title_changed = self.title_dirty;
        let dirty_lines = self.take_dirty_lines();

        if dirty_lines.is_empty() && !cursor_changed && !title_changed && !self.bell {
            return None;
        }

        self.last_cursor = cursor;
        self.title_dirty = false;
        let bell = std::mem::take(&mut self.bell);

        Some(TerminalUpdate {
            id,
            dirty_lines,
            cursor_row: cursor.0,
            cursor_col: cursor.1,
            cursor_visible: cursor.2,
            // 変わっていないタイトルを毎回積むと差分送信の意味が薄れる。
            title: if title_changed {
                self.title.clone()
            } else {
                None
            },
            scrollback_len: self.scrollback.len(),
            bell,
        })
    }

    fn take_dirty_lines(&mut self) -> Vec<(u16, Vec<TerminalCell>)> {
        let changed = self.redraw_all || self.lines.iter().any(|l| l.dirty);
        if !changed {
            return Vec::new();
        }
        // スクロールバックを遡って見ている間は、可視行と画面行の対応がずれるうえ、
        // 新しい出力が積まれるたびに表示全体が動く。行単位の追跡を諦めて全行送る。
        let send_all = self.redraw_all || self.view_offset > 0;

        let mut out = Vec::with_capacity(if send_all { self.rows } else { 4 });
        for row in 0..self.rows {
            if send_all || self.lines[row].dirty {
                out.push((row as u16, self.visible_row_cells(row)));
            }
        }
        for line in &mut self.lines {
            line.dirty = false;
        }
        self.redraw_all = false;
        out
    }

    /// 表示領域の `row` 行目に見えているセル列。スクロールバック表示中はそちらから取る。
    fn visible_row_cells(&self, row: usize) -> Vec<TerminalCell> {
        let source = if self.view_offset == 0 {
            &self.lines[row].cells
        } else {
            let index = self.scrollback.len() - self.view_offset + row;
            match self.scrollback.get(index) {
                Some(cells) => cells,
                None => &self.lines[index - self.scrollback.len()].cells,
            }
        };
        let mut cells = source.clone();
        // リサイズ前に積まれたスクロールバックは桁数が違う。GUI は常に cols 個を期待する。
        cells.resize(self.cols, TerminalCell::default());
        cells
    }

    fn reported_cursor(&self) -> (u16, u16, bool) {
        let row = self.cursor_row + self.view_offset;
        // 遡って見ている間はカーソルが表示領域の下に外れる。
        let visible = self.cursor_visible && row < self.rows;
        (
            row.min(self.rows - 1) as u16,
            self.cursor_col.min(self.cols - 1) as u16,
            visible,
        )
    }

    // -----------------------------------------------------------------------
    // 外部からの操作
    // -----------------------------------------------------------------------

    /// 表示サイズを変える。内容は可能な範囲で保持する。
    pub fn resize(&mut self, rows: u16, cols: u16) {
        let rows = (rows as usize).max(1);
        let cols = (cols as usize).max(1);

        // 行数が減ってカーソルが画面外になる分だけ上へ寄せる。単純に下を切り落とすと
        // プロンプト行が消えてしまう。
        if self.cursor_row >= rows {
            let shift = self.cursor_row + 1 - rows;
            for _ in 0..shift {
                let line = self.lines.remove(0);
                self.push_scrollback(line.cells);
            }
            self.cursor_row -= shift;
        }
        fit_lines(&mut self.lines, rows, cols);
        // 退避中の主画面も一緒に合わせる。寸法が取り残されたまま代替画面から戻ると、
        // 行や桁が足りないグリッドを rows / cols で参照して破綻する。
        if let Some(main) = &mut self.main_screen {
            fit_lines(&mut main.lines, rows, cols);
            main.cursor = (main.cursor.0.min(rows - 1), main.cursor.1.min(cols - 1));
            main.scroll_region = (0, rows - 1);
        }

        self.rows = rows;
        self.cols = cols;
        self.scroll_top = 0;
        self.scroll_bottom = rows - 1;
        self.cursor_col = self.cursor_col.min(cols - 1);
        self.view_offset = self.view_offset.min(self.scrollback.len());
        self.redraw_all = true;
    }

    /// スクロールバックの表示位置を動かす。正で過去 (上) 方向。
    pub fn scroll_view(&mut self, delta_lines: i32) {
        // 代替画面の内容は履歴に積まないので、遡ると主画面の履歴と代替画面の行が
        // 混ざって見える。全画面アプリの表示中は位置を動かさない。
        if self.main_screen.is_some() {
            return;
        }
        let max = self.scrollback.len() as i64;
        let next = (self.view_offset as i64 + delta_lines as i64).clamp(0, max);
        if next as usize != self.view_offset {
            self.view_offset = next as usize;
            self.redraw_all = true;
        }
    }

    /// 表示位置を最下部へ戻す。キー入力時に呼ぶ。
    pub fn scroll_to_bottom(&mut self) {
        self.scroll_view(i32::MIN / 2);
    }

    /// 端末クエリへの応答として PTY へ書き戻すべきバイト列を取り出す。
    ///
    /// 取り出した分は消す。残したまま次の読み取りで再送すると、プロンプトが
    /// 応答を二重に受け取って表示が崩れる。
    pub fn take_responses(&mut self) -> Vec<u8> {
        std::mem::take(&mut self.responses)
    }

    /// GUI から届いたキー入力を、現在の端末モードに合わせて書き換える。
    ///
    /// DECCKM (`CSI ?1 h/l`) を GUI へ伝える経路が `TerminalUpdate` に無いため、
    /// モードを知っているバックエンド側で解決する。GUI は常に `ESC [ A` 形式を送り、
    /// PTY へ書く直前にここで `ESC O A` 形式へ写す。GUI をモード非依存に保てるうえ、
    /// 変換が必要な箇所が入力経路の 1 か所に閉じる。
    pub fn encode_input<'a>(&self, bytes: &'a [u8]) -> Cow<'a, [u8]> {
        // 対象は 3 バイトのカーソルキーだけ。`ESC [ 5 ~` (PageUp) や alt 前置の
        // 入力まで巻き込まないよう、丸ごと一致する場合のみ書き換える。
        let is_cursor_key = matches!(bytes, [0x1b, b'[', b'A'..=b'D' | b'F' | b'H']);
        if !self.application_cursor_keys || !is_cursor_key {
            return Cow::Borrowed(bytes);
        }
        Cow::Owned(vec![0x1b, b'O', bytes[2]])
    }

    // -----------------------------------------------------------------------
    // グリッド操作の基本部品
    // -----------------------------------------------------------------------

    fn push_scrollback(&mut self, cells: Vec<TerminalCell>) {
        // 代替画面は「使い捨ての作業面」なので履歴に残さない。ここで止めておけば
        // 改行・スクロール・リサイズのどの経路から来ても汚れない。
        if self.main_screen.is_some() {
            return;
        }
        self.scrollback.push_back(cells);
        let trimmed = self.scrollback.len() > SCROLLBACK_LIMIT;
        if trimmed {
            self.scrollback.pop_front();
        }
        if self.view_offset > 0 {
            // 遡って見ている間は同じ内容を見せ続ける。上限に達して先頭が捨てられた
            // 場合は位置がずれるので、いずれにせよ全行を送り直す。
            if !trimmed {
                self.view_offset += 1;
            }
            self.redraw_all = true;
        }
    }

    /// `top`..=`bottom` を `n` 行上へ送る。`to_scrollback` なら押し出された行を履歴に積む。
    fn scroll_region_up(&mut self, top: usize, bottom: usize, n: usize, to_scrollback: bool) {
        if top > bottom {
            return;
        }
        let n = n.min(bottom - top + 1);
        for _ in 0..n {
            let line = self.lines.remove(top);
            if to_scrollback {
                self.push_scrollback(line.cells);
            }
            self.lines.insert(bottom, Line::blank(self.cols));
        }
        self.mark_dirty(top, bottom);
    }

    fn scroll_region_down(&mut self, top: usize, bottom: usize, n: usize) {
        if top > bottom {
            return;
        }
        let n = n.min(bottom - top + 1);
        for _ in 0..n {
            self.lines.remove(bottom);
            self.lines.insert(top, Line::blank(self.cols));
        }
        self.mark_dirty(top, bottom);
    }

    fn mark_dirty(&mut self, from: usize, to: usize) {
        for row in from..=to.min(self.rows - 1) {
            self.lines[row].dirty = true;
        }
    }

    fn line_feed(&mut self) {
        if self.cursor_row == self.scroll_bottom {
            // 画面全体のスクロールでだけ履歴に積む。部分領域のスクロールで積むと
            // 履歴に無関係な行が混ざる。
            let to_scrollback = self.scroll_top == 0;
            self.scroll_region_up(self.scroll_top, self.scroll_bottom, 1, to_scrollback);
        } else if self.cursor_row + 1 < self.rows {
            self.cursor_row += 1;
        }
    }

    fn reverse_index(&mut self) {
        if self.cursor_row == self.scroll_top {
            self.scroll_region_down(self.scroll_top, self.scroll_bottom, 1);
        } else {
            self.cursor_row = self.cursor_row.saturating_sub(1);
        }
    }

    /// 消去後のセル。BCE (背景色つき消去) として現在の背景色だけを引き継ぐ。
    ///
    /// 装飾フラグまで引き継ぐと、下線や反転を設定したまま消した領域に線が残る。
    fn erased_cell(&self) -> TerminalCell {
        TerminalCell {
            bg: self.pen.bg,
            ..TerminalCell::default()
        }
    }

    fn erase_cells(&mut self, row: usize, from: usize, to: usize) {
        let from = from.min(self.cols);
        let to = to.min(self.cols);
        let blank = self.erased_cell();
        for col in from..to {
            self.lines[row].cells[col] = blank;
        }
        self.lines[row].dirty = true;
    }

    fn print_char(&mut self, c: char) {
        // 罫線集合の選択中は写像した文字を書く。写像は表示のためだけのものなので、
        // グリッドには写像後の文字を持たせて描画側を単純に保つ。
        let c = if self.dec_graphics {
            dec_graphic(c).unwrap_or(c)
        } else {
            c
        };
        let width = UnicodeWidthChar::width(c).unwrap_or(0);
        // 幅 0 の結合文字は直前のセルに合成せず捨てる。合成を持ち込むとセル = 1 文字の
        // 前提が崩れ、グリッド全体が複雑になる割に得るものが少ない。
        if width == 0 || width > self.cols {
            return;
        }
        if self.cursor_col + width > self.cols {
            self.line_feed();
            self.cursor_col = 0;
        }
        let (row, col) = (self.cursor_row, self.cursor_col);
        self.lines[row].cells[col] = self.pen.cell(c, 0);
        if width == 2 {
            self.lines[row].cells[col + 1] = self.pen.cell(' ', cell_flags::WIDE_TRAILER);
        }
        self.lines[row].dirty = true;
        self.cursor_col += width;
    }

    // -----------------------------------------------------------------------
    // CSI の実処理
    // -----------------------------------------------------------------------

    fn move_cursor(&mut self, row: usize, col: usize) {
        self.cursor_row = row.min(self.rows - 1);
        self.cursor_col = col.min(self.cols - 1);
    }

    /// ED: 画面消去。
    fn erase_in_display(&mut self, mode: u16) {
        match mode {
            0 => {
                self.erase_cells(self.cursor_row, self.cursor_col, self.cols);
                for row in self.cursor_row + 1..self.rows {
                    self.erase_cells(row, 0, self.cols);
                }
            }
            1 => {
                for row in 0..self.cursor_row {
                    self.erase_cells(row, 0, self.cols);
                }
                self.erase_cells(self.cursor_row, 0, self.cursor_col + 1);
            }
            2 => {
                for row in 0..self.rows {
                    self.erase_cells(row, 0, self.cols);
                }
            }
            3 => {
                self.scrollback.clear();
                self.view_offset = 0;
                self.redraw_all = true;
            }
            _ => {}
        }
    }

    /// EL: 行消去。
    fn erase_in_line(&mut self, mode: u16) {
        let row = self.cursor_row;
        match mode {
            0 => self.erase_cells(row, self.cursor_col, self.cols),
            1 => self.erase_cells(row, 0, self.cursor_col + 1),
            2 => self.erase_cells(row, 0, self.cols),
            _ => {}
        }
    }

    /// ICH: カーソル位置に空白を挿入し、右へ押し出す。
    fn insert_chars(&mut self, n: usize) {
        let (row, col) = (self.cursor_row, self.cursor_col.min(self.cols - 1));
        let cells = &mut self.lines[row].cells;
        for _ in 0..n.min(self.cols - col) {
            cells.insert(col, TerminalCell::default());
            cells.pop();
        }
        self.lines[row].dirty = true;
    }

    /// DCH: カーソル位置から削除し、左へ詰める。
    fn delete_chars(&mut self, n: usize) {
        let (row, col) = (self.cursor_row, self.cursor_col.min(self.cols - 1));
        let cells = &mut self.lines[row].cells;
        for _ in 0..n.min(self.cols - col) {
            cells.remove(col);
            cells.push(TerminalCell::default());
        }
        self.lines[row].dirty = true;
    }

    fn set_scroll_region(&mut self, top: usize, bottom: usize) {
        let top = top.min(self.rows - 1);
        let bottom = bottom.min(self.rows - 1);
        if top < bottom {
            self.scroll_top = top;
            self.scroll_bottom = bottom;
        } else {
            self.scroll_top = 0;
            self.scroll_bottom = self.rows - 1;
        }
        // DECSTBM は原点へ戻す規定。
        self.move_cursor(0, 0);
    }

    fn apply_sgr(&mut self, params: &Params) {
        let groups: Vec<&[u16]> = params.iter().collect();
        let mut i = 0;
        while i < groups.len() {
            let group = groups[i];
            let Some(&code) = group.first() else {
                i += 1;
                continue;
            };
            // コロン区切り (`38:2:r:g:b`) は 1 グループに全要素が入る。
            if group.len() > 1 {
                if let Some(color) = color_from_subparams(group) {
                    self.set_color(code, color);
                }
                i += 1;
                continue;
            }
            match code {
                38 | 48 => {
                    // セミコロン区切り。後続のパラメータを必要数だけ食う。
                    let mode = groups.get(i + 1).and_then(|g| g.first()).copied();
                    let (color, consumed) = match mode {
                        Some(5) => (
                            groups
                                .get(i + 2)
                                .and_then(|g| g.first())
                                .map(|&n| TermColor::Indexed(n as u8)),
                            3,
                        ),
                        Some(2) => {
                            let value = |offset: usize| {
                                groups.get(i + offset).and_then(|g| g.first()).copied()
                            };
                            match (value(2), value(3), value(4)) {
                                (Some(r), Some(g), Some(b)) => {
                                    (Some(TermColor::Rgb(r as u8, g as u8, b as u8)), 5)
                                }
                                _ => (None, 5),
                            }
                        }
                        _ => (None, 1),
                    };
                    if let Some(color) = color {
                        self.set_color(code, color);
                    }
                    i += consumed;
                }
                _ => {
                    self.apply_sgr_simple(code);
                    i += 1;
                }
            }
        }
    }

    fn set_color(&mut self, code: u16, color: TermColor) {
        if code == 38 {
            self.pen.fg = color;
        } else {
            self.pen.bg = color;
        }
    }

    fn apply_sgr_simple(&mut self, code: u16) {
        use cell_flags::*;
        match code {
            0 => self.pen = Pen::default(),
            1 => self.pen.flags |= BOLD,
            2 => self.pen.flags |= DIM,
            3 => self.pen.flags |= ITALIC,
            4 => self.pen.flags |= UNDERLINE,
            7 => self.pen.flags |= INVERSE,
            9 => self.pen.flags |= STRIKETHROUGH,
            22 => self.pen.flags &= !(BOLD | DIM),
            23 => self.pen.flags &= !ITALIC,
            24 => self.pen.flags &= !UNDERLINE,
            27 => self.pen.flags &= !INVERSE,
            29 => self.pen.flags &= !STRIKETHROUGH,
            30..=37 => self.pen.fg = TermColor::Indexed((code - 30) as u8),
            39 => self.pen.fg = TermColor::Default,
            40..=47 => self.pen.bg = TermColor::Indexed((code - 40) as u8),
            49 => self.pen.bg = TermColor::Default,
            90..=97 => self.pen.fg = TermColor::Indexed((code - 90 + 8) as u8),
            100..=107 => self.pen.bg = TermColor::Indexed((code - 100 + 8) as u8),
            _ => {}
        }
    }

    fn set_private_modes(&mut self, params: &Params, enabled: bool) {
        for group in params.iter() {
            let Some(&mode) = group.first() else {
                continue;
            };
            match mode {
                // DECCKM。
                1 => self.application_cursor_keys = enabled,
                // DECTCEM。
                25 => self.cursor_visible = enabled,
                // 47 / 1047 は代替画面の切り替えだけ。1049 はカーソル退避も伴う。
                47 | 1047 => self.set_alt_screen(enabled, false),
                1049 => self.set_alt_screen(enabled, true),
                // マウス報告や括弧付き貼り付けなどの私的モードは未対応。
                _ => {}
            }
        }
    }

    /// 代替画面へ切り替える / 主画面へ戻る。
    ///
    /// `save_cursor` は DECSC / DECRC 相当のカーソル退避を伴うか (`?1049` だけが真)。
    /// 主画面のカーソル位置自体はどのモードでも退避・復元する。そうしないと、
    /// 全画面アプリが最後に置いた位置にプロンプトが描かれてしまう。
    fn set_alt_screen(&mut self, enter: bool, save_cursor: bool) {
        // 同じ画面への二重切り替えは無視する。受け入れると、代替画面の内容で
        // 主画面の退避を上書きしてしまう。
        if enter == self.main_screen.is_some() {
            return;
        }
        if enter {
            if save_cursor {
                self.saved_cursor = (self.cursor_row, self.cursor_col);
            }
            self.main_screen = Some(MainScreen {
                lines: std::mem::replace(
                    &mut self.lines,
                    (0..self.rows).map(|_| Line::blank(self.cols)).collect(),
                ),
                cursor: (self.cursor_row, self.cursor_col),
                scroll_region: (self.scroll_top, self.scroll_bottom),
            });
            self.scroll_top = 0;
            self.scroll_bottom = self.rows - 1;
            // 履歴を遡ったまま切り替わると、代替画面の行と履歴が混ざって見える。
            self.view_offset = 0;
        } else {
            let Some(main) = self.main_screen.take() else {
                return;
            };
            self.lines = main.lines;
            let (row, col) = main.cursor;
            self.move_cursor(row, col);
            (self.scroll_top, self.scroll_bottom) = main.scroll_region;
            if save_cursor {
                let (row, col) = self.saved_cursor;
                self.move_cursor(row, col);
            }
        }
        self.redraw_all = true;
    }

    /// DSR: 端末の状態問い合わせ。
    fn device_status_report(&mut self, mode: u16) {
        match mode {
            // 5 は「異常なし」。応答が無いと待ち続ける呼び出し側が居る。
            5 => self.responses.extend_from_slice(b"\x1b[0n"),
            6 => {
                // CPR は 1 始まり。行末まで書いた直後の cursor_col は cols を取り得るので、
                // 画面内へ丸めてから返す。
                let row = self.cursor_row.min(self.rows - 1) + 1;
                let col = self.cursor_col.min(self.cols - 1) + 1;
                self.responses
                    .extend_from_slice(format!("\x1b[{row};{col}R").as_bytes());
            }
            _ => {}
        }
    }
}

// ---------------------------------------------------------------------------
// vte からの呼び出し
// ---------------------------------------------------------------------------

impl Perform for TerminalGrid {
    fn print(&mut self, c: char) {
        self.print_char(c);
    }

    fn execute(&mut self, byte: u8) {
        match byte {
            0x07 => self.bell = true,
            0x08 => self.cursor_col = self.cursor_col.saturating_sub(1),
            0x09 => {
                let next = (self.cursor_col / TAB_WIDTH + 1) * TAB_WIDTH;
                self.cursor_col = next.min(self.cols - 1);
            }
            // LF・VT・FF。VT と FF も端末では LF と同じ扱いにするのが慣例。
            0x0A..=0x0C => self.line_feed(),
            0x0D => self.cursor_col = 0,
            _ => {}
        }
    }

    fn csi_dispatch(&mut self, params: &Params, intermediates: &[u8], _ignore: bool, action: char) {
        let private = intermediates.first().copied();
        match (private, action) {
            (Some(b'?'), 'h') => self.set_private_modes(params, true),
            (Some(b'?'), 'l') => self.set_private_modes(params, false),
            // 中間バイト付きのその他 (`CSI > ... c` の問い合わせなど) は解釈しない。
            (Some(_), _) => {}

            (None, 'A') => self.cursor_row = self.cursor_row.saturating_sub(param(params, 0, 1)),
            (None, 'B') => {
                let row = self.cursor_row + param(params, 0, 1);
                self.cursor_row = row.min(self.rows - 1);
            }
            (None, 'C') => {
                let col = self.cursor_col + param(params, 0, 1);
                self.cursor_col = col.min(self.cols - 1);
            }
            (None, 'D') => self.cursor_col = self.cursor_col.saturating_sub(param(params, 0, 1)),
            (None, 'G') | (None, '`') => {
                let col = param(params, 0, 1) - 1;
                self.move_cursor(self.cursor_row, col);
            }
            (None, 'd') => {
                let row = param(params, 0, 1) - 1;
                self.move_cursor(row, self.cursor_col);
            }
            (None, 'H') | (None, 'f') => {
                let row = param(params, 0, 1) - 1;
                let col = param(params, 1, 1) - 1;
                self.move_cursor(row, col);
            }
            (None, 'J') => self.erase_in_display(param_raw(params, 0)),
            (None, 'K') => self.erase_in_line(param_raw(params, 0)),
            (None, 'L') => {
                let (top, bottom) = (self.cursor_row, self.scroll_bottom);
                self.scroll_region_down(top, bottom, param(params, 0, 1));
            }
            (None, 'M') => {
                let (top, bottom) = (self.cursor_row, self.scroll_bottom);
                self.scroll_region_up(top, bottom, param(params, 0, 1), false);
            }
            (None, '@') => self.insert_chars(param(params, 0, 1)),
            (None, 'P') => self.delete_chars(param(params, 0, 1)),
            (None, 'X') => {
                let from = self.cursor_col;
                let to = from + param(params, 0, 1);
                self.erase_cells(self.cursor_row, from, to);
            }
            (None, 'S') => {
                let (top, bottom) = (self.scroll_top, self.scroll_bottom);
                let to_scrollback = top == 0;
                self.scroll_region_up(top, bottom, param(params, 0, 1), to_scrollback);
            }
            (None, 'T') => {
                let (top, bottom) = (self.scroll_top, self.scroll_bottom);
                self.scroll_region_down(top, bottom, param(params, 0, 1));
            }
            (None, 'r') => {
                let top = param(params, 0, 1) - 1;
                let bottom = param(params, 1, self.rows as u16) - 1;
                self.set_scroll_region(top, bottom);
            }
            (None, 's') => self.saved_cursor = (self.cursor_row, self.cursor_col),
            (None, 'u') => {
                let (row, col) = self.saved_cursor;
                self.move_cursor(row, col);
            }
            (None, 'm') => self.apply_sgr(params),
            (None, 'n') => self.device_status_report(param_raw(params, 0)),
            // DA: 装置属性。VT102 相当を名乗る。パラメータ付き (`CSI > c` など) は別物。
            (None, 'c') if param_raw(params, 0) == 0 => {
                self.responses.extend_from_slice(b"\x1b[?6c")
            }
            // SM / RM (`CSI h` / `CSI l`) の公的モードは挿入モードなど未対応のものばかり。
            _ => {}
        }
    }

    fn esc_dispatch(&mut self, intermediates: &[u8], _ignore: bool, byte: u8) {
        // 中間バイト付き (`ESC # 8` の DECALN など) は別物なので最終バイトだけで判定しない。
        // `ESC # 8` を DECRC と取り違えるのを防ぐ。
        if let Some(&intermediate) = intermediates.first() {
            // G0 への文字集合指定。SO / SI による G1 への切り替えは、
            // xterm-256color の smacs / rmacs が `ESC ( 0` / `ESC ( B` を使うため要らない。
            if intermediate == b'(' {
                self.dec_graphics = byte == b'0';
            }
            return;
        }
        match byte {
            b'7' => self.saved_cursor = (self.cursor_row, self.cursor_col),
            b'8' => {
                let (row, col) = self.saved_cursor;
                self.move_cursor(row, col);
            }
            b'M' => self.reverse_index(),
            b'D' => self.line_feed(),
            b'E' => {
                self.line_feed();
                self.cursor_col = 0;
            }
            _ => {}
        }
    }

    fn osc_dispatch(&mut self, params: &[&[u8]], _bell_terminated: bool) {
        // OSC 0 はアイコン名とタイトルの同時設定、OSC 2 はタイトルのみ。
        // Nebula はアイコン名を持たないのでどちらもタイトルとして扱う。
        let Some(kind) = params.first() else {
            return;
        };
        if *kind != b"0" && *kind != b"2" {
            return;
        }
        if params.len() < 2 {
            return;
        }
        // vte は OSC の中身を `;` で分割して渡してくる。タイトルに `;` を含む
        // (シェルが実行中のコマンド行をそのまま入れる場合など) と複数要素に割れるので、
        // 区切りを戻しながら連結する。先頭要素だけを見ると `make; test` が `make` になる。
        let title = params[1..]
            .iter()
            .map(|part| String::from_utf8_lossy(part))
            .collect::<Vec<_>>()
            .join(";");
        if self.title.as_deref() != Some(title.as_str()) {
            self.title = Some(title);
            self.title_dirty = true;
        }
    }
}

/// 行の並びを `rows` 行 × `cols` 桁に合わせる。足りない分は空行・空セルで埋める。
fn fit_lines(lines: &mut Vec<Line>, rows: usize, cols: usize) {
    lines.truncate(rows);
    while lines.len() < rows {
        lines.push(Line::blank(cols));
    }
    for line in lines {
        line.cells.resize(cols, TerminalCell::default());
    }
}

/// DEC 特殊図形集合 (`ESC ( 0`) の写像。範囲外の文字はそのまま使う。
fn dec_graphic(c: char) -> Option<char> {
    Some(match c {
        '_' => ' ',
        '`' => '◆',
        'a' => '▒',
        'b' => '␉',
        'c' => '␌',
        'd' => '␍',
        'e' => '␊',
        'f' => '°',
        'g' => '±',
        'h' => '␤',
        'i' => '␋',
        'j' => '┘',
        'k' => '┐',
        'l' => '┌',
        'm' => '└',
        'n' => '┼',
        'o' => '⎺',
        'p' => '⎻',
        'q' => '─',
        'r' => '⎼',
        's' => '⎽',
        't' => '├',
        'u' => '┤',
        'v' => '┴',
        'w' => '┬',
        'x' => '│',
        'y' => '≤',
        'z' => '≥',
        '{' => 'π',
        '|' => '≠',
        '}' => '£',
        '~' => '·',
        _ => return None,
    })
}

/// `index` 番目のパラメータ。省略と 0 はどちらも既定値扱い (CSI の一般規則)。
fn param(params: &Params, index: usize, default: u16) -> usize {
    match param_raw(params, index) {
        0 => default as usize,
        value => value as usize,
    }
}

/// `index` 番目のパラメータをそのまま返す。0 に意味がある ED / EL 用。
fn param_raw(params: &Params, index: usize) -> u16 {
    params
        .iter()
        .nth(index)
        .and_then(|group| group.first())
        .copied()
        .unwrap_or(0)
}

/// コロン区切りの拡張色 (`38:5:n` / `38:2:r:g:b` / `38:2:色空間:r:g:b`) を読む。
fn color_from_subparams(group: &[u16]) -> Option<TermColor> {
    match group.get(1)? {
        5 => group.get(2).map(|&n| TermColor::Indexed(n as u8)),
        2 => {
            // 6 要素形式は 3 番目に色空間 ID が挟まる。
            let rgb = if group.len() >= 6 {
                &group[3..6]
            } else {
                group.get(2..5)?
            };
            Some(TermColor::Rgb(rgb[0] as u8, rgb[1] as u8, rgb[2] as u8))
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn emulator(rows: u16, cols: u16) -> TerminalEmulator {
        TerminalEmulator::new(rows, cols)
    }

    /// 検査を読みやすくするため、行の文字だけを取り出す。
    fn line_text(grid: &TerminalGrid, row: usize) -> String {
        grid.lines[row]
            .cells
            .iter()
            .filter(|c| c.flags & cell_flags::WIDE_TRAILER == 0)
            .map(|c| c.ch)
            .collect::<String>()
            .trim_end()
            .to_string()
    }

    #[test]
    fn 文字を書くとカーソルが進む() {
        let mut emu = emulator(3, 10);
        emu.advance(b"abc");
        let grid = emu.grid_mut();
        assert_eq!(line_text(grid, 0), "abc");
        assert_eq!((grid.cursor_row, grid.cursor_col), (0, 3));
    }

    #[test]
    fn sgr_で前景色が付き_リセットで戻る() {
        let mut emu = emulator(3, 10);
        emu.advance(b"\x1b[31mRED\x1b[0mX");
        let grid = emu.grid_mut();
        for col in 0..3 {
            assert_eq!(grid.lines[0].cells[col].fg, TermColor::Indexed(1));
        }
        assert_eq!(grid.lines[0].cells[3].fg, TermColor::Default);
    }

    #[test]
    fn 明るい色と背景色を解釈する() {
        let mut emu = emulator(2, 10);
        emu.advance(b"\x1b[92;104mA");
        let cell = emu.grid_mut().lines[0].cells[0];
        assert_eq!(cell.fg, TermColor::Indexed(10));
        assert_eq!(cell.bg, TermColor::Indexed(12));
    }

    #[test]
    fn 二五六色指定を解釈する() {
        let mut emu = emulator(2, 10);
        emu.advance(b"\x1b[38;5;208;48;5;17mA");
        let cell = emu.grid_mut().lines[0].cells[0];
        assert_eq!(cell.fg, TermColor::Indexed(208));
        assert_eq!(cell.bg, TermColor::Indexed(17));
    }

    #[test]
    fn rgb指定がセミコロンでもコロンでも同じになる() {
        let mut semicolon = emulator(2, 10);
        semicolon.advance(b"\x1b[38;2;10;20;30mA");
        let mut colon = emulator(2, 10);
        colon.advance(b"\x1b[38:2:10:20:30mA");
        let expected = TermColor::Rgb(10, 20, 30);
        assert_eq!(semicolon.grid_mut().lines[0].cells[0].fg, expected);
        assert_eq!(colon.grid_mut().lines[0].cells[0].fg, expected);
    }

    #[test]
    fn rgb指定は色空間idを挟む形式も読める() {
        let mut emu = emulator(2, 10);
        emu.advance(b"\x1b[48:2::1:2:3mA");
        assert_eq!(emu.grid_mut().lines[0].cells[0].bg, TermColor::Rgb(1, 2, 3));
    }

    #[test]
    fn 装飾フラグの設定と解除ができる() {
        let mut emu = emulator(2, 10);
        emu.advance(b"\x1b[1;4;7mA\x1b[24mB");
        let grid = emu.grid_mut();
        let a = grid.lines[0].cells[0];
        assert_eq!(
            a.flags,
            cell_flags::BOLD | cell_flags::UNDERLINE | cell_flags::INVERSE
        );
        let b = grid.lines[0].cells[1];
        assert_eq!(b.flags, cell_flags::BOLD | cell_flags::INVERSE);
    }

    #[test]
    fn 全角文字は二セル使い後続に印が付く() {
        let mut emu = emulator(2, 10);
        emu.advance("あa".as_bytes());
        let grid = emu.grid_mut();
        assert_eq!(grid.lines[0].cells[0].ch, 'あ');
        assert_eq!(grid.lines[0].cells[0].flags & cell_flags::WIDE_TRAILER, 0);
        assert_ne!(grid.lines[0].cells[1].flags & cell_flags::WIDE_TRAILER, 0);
        assert_eq!(grid.lines[0].cells[2].ch, 'a');
        assert_eq!(grid.cursor_col, 3);
    }

    #[test]
    fn 全角文字は行末に入らず次行へ折り返す() {
        let mut emu = emulator(3, 3);
        emu.advance("ああ".as_bytes());
        let grid = emu.grid_mut();
        assert_eq!(grid.lines[0].cells[0].ch, 'あ');
        assert_eq!(grid.lines[1].cells[0].ch, 'あ');
    }

    #[test]
    fn 行末を超えると折り返す() {
        let mut emu = emulator(3, 3);
        emu.advance(b"abcd");
        let grid = emu.grid_mut();
        assert_eq!(line_text(grid, 0), "abc");
        assert_eq!(line_text(grid, 1), "d");
    }

    #[test]
    fn 制御文字を処理する() {
        let mut emu = emulator(3, 10);
        emu.advance(b"abc\rX\n\tY\x08Z");
        let grid = emu.grid_mut();
        assert_eq!(line_text(grid, 0), "Xbc");
        // タブで 8 桁目へ跳び、Y を書いてから後退した位置を Z で上書きする。
        assert_eq!(line_text(grid, 1), "        Z");
        assert!(!grid.bell);
        emu.advance(b"\x07");
        assert!(emu.grid_mut().bell);
    }

    #[test]
    fn 改行でスクロールし履歴に積まれる() {
        let mut emu = emulator(2, 10);
        emu.advance(b"one\r\ntwo\r\nthree");
        let grid = emu.grid_mut();
        assert_eq!(grid.scrollback.len(), 1);
        assert_eq!(line_text(grid, 0), "two");
        assert_eq!(line_text(grid, 1), "three");
        let first: String = grid.scrollback[0].iter().map(|c| c.ch).collect();
        assert_eq!(first.trim_end(), "one");
    }

    #[test]
    fn 画面消去で全セルが空白になる() {
        let mut emu = emulator(2, 5);
        emu.advance(b"ab\r\ncd\x1b[2J");
        let grid = emu.grid_mut();
        assert!(
            grid.lines
                .iter()
                .all(|l| l.cells.iter().all(|c| c.ch == ' '))
        );
    }

    #[test]
    fn 行消去はモードごとに範囲が変わる() {
        let mut emu = emulator(2, 6);
        emu.advance(b"abcdef\x1b[1;3H\x1b[0K");
        assert_eq!(line_text(emu.grid_mut(), 0), "ab");

        let mut emu = emulator(2, 6);
        emu.advance(b"abcdef\x1b[1;3H\x1b[1K");
        assert_eq!(line_text(emu.grid_mut(), 0), "   def");
    }

    #[test]
    fn カーソル移動シーケンスを解釈する() {
        let cursor = |emu: &mut TerminalEmulator| {
            let grid = emu.grid_mut();
            (grid.cursor_row, grid.cursor_col)
        };
        let mut emu = emulator(5, 10);
        emu.advance(b"\x1b[3;5H");
        assert_eq!(cursor(&mut emu), (2, 4));
        emu.advance(b"\x1b[2A\x1b[3C");
        assert_eq!(cursor(&mut emu), (0, 7));
        emu.advance(b"\x1b[B\x1b[2D");
        assert_eq!(cursor(&mut emu), (1, 5));
        // 画面外へは出ない。
        emu.advance(b"\x1b[99;99H");
        assert_eq!(cursor(&mut emu), (4, 9));
    }

    #[test]
    fn decstbm_の領域内だけがスクロールする() {
        let mut emu = emulator(4, 5);
        emu.advance(b"a\r\nb\r\nc\r\nd");
        // 2〜3 行目を領域にして、その最終行で改行する。
        emu.advance(b"\x1b[2;3r\x1b[3;1H\n");
        let grid = emu.grid_mut();
        assert_eq!(line_text(grid, 0), "a");
        assert_eq!(line_text(grid, 1), "c");
        assert_eq!(line_text(grid, 2), "");
        assert_eq!(line_text(grid, 3), "d");
        // 部分領域のスクロールは履歴に積まない。
        assert_eq!(grid.scrollback.len(), 0);
    }

    #[test]
    fn dectcem_でカーソル表示が切り替わる() {
        let mut emu = emulator(2, 5);
        emu.advance(b"\x1b[?25l");
        assert!(!emu.grid_mut().cursor_visible);
        emu.advance(b"\x1b[?25h");
        assert!(emu.grid_mut().cursor_visible);
    }

    #[test]
    fn 代替画面から戻ると主画面が復元される() {
        let mut emu = emulator(3, 6);
        emu.advance(b"main\r\n2nd");
        emu.advance(b"\x1b[?1049h");
        // 入った直後の代替画面は空。
        let grid = emu.grid_mut();
        assert_eq!(line_text(grid, 0), "");
        emu.advance(b"\x1b[1;1Halt");
        assert_eq!(line_text(emu.grid_mut(), 0), "alt");

        emu.advance(b"\x1b[?1049l");
        let grid = emu.grid_mut();
        assert_eq!(line_text(grid, 0), "main");
        assert_eq!(line_text(grid, 1), "2nd");
        assert_eq!((grid.cursor_row, grid.cursor_col), (1, 3));
    }

    #[test]
    fn 代替画面の改行は履歴に積まれない() {
        let mut emu = emulator(2, 6);
        emu.advance(b"one\r\ntwo\r\nthree");
        let before = emu.grid_mut().scrollback.len();
        assert_eq!(before, 1);

        emu.advance(b"\x1b[?1049h");
        emu.advance(b"a\r\nb\r\nc\r\nd");
        assert_eq!(emu.grid_mut().scrollback.len(), before, "履歴が汚れない");

        emu.advance(b"\x1b[?1049l");
        let grid = emu.grid_mut();
        assert_eq!(grid.scrollback.len(), before);
        assert_eq!(line_text(grid, 1), "three");
    }

    #[test]
    fn 代替画面はカーソル退避の有無がモードで変わる() {
        // 1049 は DECSC 相当を伴うので、代替画面で `ESC 8` しても入る前の位置へ戻る。
        let mut emu = emulator(4, 8);
        emu.advance(b"\x1b[3;5H\x1b[?1049h\x1b[1;1H\x1b8");
        let grid = emu.grid_mut();
        assert_eq!((grid.cursor_row, grid.cursor_col), (2, 4));

        // 47 は退避しないので、事前に退避した位置がそのまま残る。
        let mut emu = emulator(4, 8);
        emu.advance(b"\x1b[2;2H\x1b7\x1b[3;5H\x1b[?47h\x1b[1;1H\x1b8");
        let grid = emu.grid_mut();
        assert_eq!((grid.cursor_row, grid.cursor_col), (1, 1));
    }

    #[test]
    fn 代替画面表示中のリサイズでも主画面へ戻れる() {
        let mut emu = emulator(4, 8);
        emu.advance(b"main");
        emu.advance(b"\x1b[?1049h");
        emu.grid_mut().resize(2, 4);
        emu.advance(b"\x1b[?1049l");
        let grid = emu.grid_mut();
        assert_eq!(grid.lines.len(), 2);
        assert_eq!(grid.lines[0].cells.len(), 4);
        assert_eq!(line_text(grid, 0), "main");
    }

    #[test]
    fn decckm_でカーソルキーの送出形式が変わる() {
        let mut emu = emulator(2, 5);
        let grid = emu.grid_mut();
        assert_eq!(grid.encode_input(b"\x1b[A").as_ref(), b"\x1b[A");

        emu.advance(b"\x1b[?1h");
        let grid = emu.grid_mut();
        assert!(grid.application_cursor_keys);
        assert_eq!(grid.encode_input(b"\x1b[A").as_ref(), b"\x1bOA");
        assert_eq!(grid.encode_input(b"\x1b[D").as_ref(), b"\x1bOD");
        assert_eq!(grid.encode_input(b"\x1b[H").as_ref(), b"\x1bOH");
        // カーソルキー以外は書き換えない。
        assert_eq!(grid.encode_input(b"\x1b[5~").as_ref(), b"\x1b[5~");
        assert_eq!(grid.encode_input(b"a").as_ref(), b"a");

        emu.advance(b"\x1b[?1l");
        assert_eq!(emu.grid_mut().encode_input(b"\x1b[A").as_ref(), b"\x1b[A");
    }

    #[test]
    fn カーソル位置の問い合わせに応答する() {
        let mut emu = emulator(5, 10);
        emu.advance(b"\x1b[3;5H\x1b[6n");
        assert_eq!(emu.grid_mut().take_responses(), b"\x1b[3;5R".to_vec());
        // 取り出した応答は消える。
        assert!(emu.grid_mut().take_responses().is_empty());
    }

    #[test]
    fn 装置属性の問い合わせに応答する() {
        let mut emu = emulator(2, 5);
        emu.advance(b"\x1b[c");
        assert_eq!(emu.grid_mut().take_responses(), b"\x1b[?6c".to_vec());
        // 状態問い合わせは「異常なし」を返す。
        emu.advance(b"\x1b[5n");
        assert_eq!(emu.grid_mut().take_responses(), b"\x1b[0n".to_vec());
    }

    #[test]
    fn 罫線集合の間だけ罫線文字に写す() {
        let mut emu = emulator(2, 10);
        emu.advance(b"\x1b(0qxlj\x1b(Bq");
        let grid = emu.grid_mut();
        assert_eq!(line_text(grid, 0), "─│┌┘q");
    }

    #[test]
    fn 消去したセルに現在の背景色が付く() {
        let mut emu = emulator(2, 4);
        emu.advance(b"\x1b[41m\x1b[2J");
        let grid = emu.grid_mut();
        assert!(
            grid.lines
                .iter()
                .flat_map(|l| &l.cells)
                .all(|c| c.bg == TermColor::Indexed(1) && c.ch == ' ')
        );

        // 装飾は引き継がない。下線付きのまま消すと消去部分に線が残る。
        let mut emu = emulator(2, 4);
        emu.advance(b"\x1b[4;42mab\x1b[2K");
        let cell = emu.grid_mut().lines[0].cells[0];
        assert_eq!(cell.bg, TermColor::Indexed(2));
        assert_eq!(cell.flags, 0);
    }

    #[test]
    fn csi_s_と_csi_u_でカーソルを退避復元する() {
        let mut emu = emulator(4, 8);
        emu.advance(b"\x1b[2;3H\x1bs\x1b[4;7H\x1bu");
        let grid = emu.grid_mut();
        assert_eq!((grid.cursor_row, grid.cursor_col), (3, 6), "ESC s は無効");

        let mut emu = emulator(4, 8);
        emu.advance(b"\x1b[2;3H\x1b[s\x1b[4;7H\x1b[u");
        let grid = emu.grid_mut();
        assert_eq!((grid.cursor_row, grid.cursor_col), (1, 2));
    }

    #[test]
    fn osc_でタイトルを設定する() {
        let mut emu = emulator(2, 5);
        emu.advance(b"\x1b]0;nebula\x07");
        assert_eq!(emu.grid_mut().title.as_deref(), Some("nebula"));
        emu.advance(b"\x1b]2;code\x1b\\");
        assert_eq!(emu.grid_mut().title.as_deref(), Some("code"));
    }

    #[test]
    fn 中間バイト付きのエスケープは解釈しない() {
        let mut emu = emulator(3, 5);
        emu.advance(b"\x1b[3;4H\x1b7\x1b[1;1H");
        // `ESC # 8` (DECALN) を DECRC と取り違えるとカーソルが (2,3) へ戻ってしまう。
        emu.advance(b"\x1b#8");
        let grid = emu.grid_mut();
        assert_eq!((grid.cursor_row, grid.cursor_col), (0, 0));
        // 中間バイト無しの `ESC 8` は従来どおり復元する。
        emu.advance(b"\x1b8");
        let grid = emu.grid_mut();
        assert_eq!((grid.cursor_row, grid.cursor_col), (2, 3));
    }

    #[test]
    fn タイトルにセミコロンが含まれても切れない() {
        let mut emu = emulator(2, 5);
        emu.advance(b"\x1b]2;make; test\x07");
        assert_eq!(emu.grid_mut().title.as_deref(), Some("make; test"));
    }

    #[test]
    fn 文字の挿入と削除ができる() {
        let mut emu = emulator(2, 6);
        emu.advance(b"abcdef\x1b[1;2H\x1b[2@");
        assert_eq!(line_text(emu.grid_mut(), 0), "a  bcd");

        let mut emu = emulator(2, 6);
        emu.advance(b"abcdef\x1b[1;2H\x1b[2P");
        assert_eq!(line_text(emu.grid_mut(), 0), "adef");
    }

    #[test]
    fn 分割して届いたシーケンスでも解釈できる() {
        let mut emu = emulator(2, 10);
        emu.advance(b"\x1b[3");
        emu.advance(b"1mR");
        assert_eq!(emu.grid_mut().lines[0].cells[0].fg, TermColor::Indexed(1));
    }

    #[test]
    fn 分割して届いた全角文字でも解釈できる() {
        let bytes = "あ".as_bytes().to_vec();
        let mut emu = emulator(2, 10);
        emu.advance(&bytes[..1]);
        emu.advance(&bytes[1..]);
        assert_eq!(emu.grid_mut().lines[0].cells[0].ch, 'あ');
    }

    #[test]
    fn 差分は変化した行だけを返す() {
        let id = TerminalId(1);
        let mut emu = emulator(3, 5);
        // 生成直後は全行を送る。
        let first = emu.grid_mut().take_update(id).expect("初回は全画面が届く");
        assert_eq!(first.dirty_lines.len(), 3);
        // 変化が無ければ何も送らない。
        assert!(emu.grid_mut().take_update(id).is_none());

        emu.advance(b"\r\nhi");
        let update = emu.grid_mut().take_update(id).expect("2 行目が変わる");
        assert_eq!(update.dirty_lines.len(), 1);
        assert_eq!(update.dirty_lines[0].0, 1);
        assert_eq!(update.cursor_row, 1);
        assert_eq!(update.cursor_col, 2);
    }

    #[test]
    fn ベルとタイトルは一度だけ通知される() {
        let id = TerminalId(1);
        let mut emu = emulator(2, 5);
        let _ = emu.grid_mut().take_update(id);

        emu.advance(b"\x07\x1b]2;t\x07");
        let update = emu
            .grid_mut()
            .take_update(id)
            .expect("ベルとタイトルが届く");
        assert!(update.bell);
        assert_eq!(update.title.as_deref(), Some("t"));
        assert!(emu.grid_mut().take_update(id).is_none());
    }

    #[test]
    fn スクロール表示で履歴が見える() {
        let id = TerminalId(1);
        let mut emu = emulator(2, 6);
        emu.advance(b"one\r\ntwo\r\nthree");
        let _ = emu.grid_mut().take_update(id);

        emu.grid_mut().scroll_view(1);
        let update = emu.grid_mut().take_update(id).expect("表示位置が動く");
        // 遡っている間は全行を送り直す。
        assert_eq!(update.dirty_lines.len(), 2);
        let top: String = update.dirty_lines[0].1.iter().map(|c| c.ch).collect();
        assert_eq!(top.trim_end(), "one");
        assert_eq!(update.scrollback_len, 1);

        emu.grid_mut().scroll_to_bottom();
        let update = emu.grid_mut().take_update(id).expect("最下部へ戻る");
        let top: String = update.dirty_lines[0].1.iter().map(|c| c.ch).collect();
        assert_eq!(top.trim_end(), "two");
    }

    #[test]
    fn スクロールバックは上限で古い行から捨てられる() {
        let mut emu = emulator(1, 4);
        for _ in 0..SCROLLBACK_LIMIT + 10 {
            emu.advance(b"x\r\n");
        }
        assert_eq!(emu.grid_mut().scrollback.len(), SCROLLBACK_LIMIT);
    }

    #[test]
    fn リサイズで内容と桁数が保たれる() {
        let mut emu = emulator(4, 10);
        emu.advance(b"hello\r\nworld");
        emu.grid_mut().resize(4, 4);
        let grid = emu.grid_mut();
        assert_eq!(grid.lines[0].cells.len(), 4);
        assert_eq!(line_text(grid, 0), "hell");
        assert_eq!(line_text(grid, 1), "worl");
    }

    #[test]
    fn 行数を縮めるとカーソル行が残る() {
        let mut emu = emulator(4, 6);
        emu.advance(b"a\r\nb\r\nc\r\nd");
        emu.grid_mut().resize(2, 6);
        let grid = emu.grid_mut();
        assert_eq!(grid.cursor_row, 1);
        assert_eq!(line_text(grid, 0), "c");
        assert_eq!(line_text(grid, 1), "d");
        assert_eq!(grid.scrollback.len(), 2);
    }
}
