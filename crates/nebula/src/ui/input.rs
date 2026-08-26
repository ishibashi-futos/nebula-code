//! テキスト入力欄 [`TextInput`] のエンティティと、その編集モデル。
//!
//! 構成は 3 層。
//!
//! 1. [`super::text`] の純粋関数と [`InputState`] … gpui に触れない編集モデル。
//!    IME の位置計算など間違えやすい部分をここへ寄せ、単体テストで固める。
//! 2. ここ ([`TextInput`]) … 上の状態に focus・キー操作・マウス操作を足した
//!    エンティティ。
//! 3. [`element`] の [`element::TextInputElement`] … 実際に字形化して描く
//!    カスタム要素。
//!
//! 位置はすべて **UTF-8 バイト** で持つ。IME が寄こす UTF-16 の位置は境界で直す。
//!
//! `element` を子モジュールにしてあるのは、[`TextInput`] と
//! [`element::InputLayout`] が互いの内部状態を読み合うため。兄弟モジュールに
//! すると両方向に `pub(in ...)` が要るが、親子なら片方向 (子が親へ公開する側)
//! だけで済む。

mod element;

use element::{InputLayout, TextInputElement};
use gpui::prelude::*;
use gpui::{
    App, Bounds, ClipboardItem, Context, EntityInputHandler, EventEmitter, FocusHandle, Focusable,
    IntoElement, KeyBinding, MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent, Pixels,
    Point, SharedString, UTF16Selection, Window, actions, div, point, px, size,
};
use std::ops::Range;

use super::text::{
    line_bounds, next_grapheme, next_word_boundary, offset_to_utf16, previous_grapheme,
    previous_word_boundary, range_from_utf16, range_to_utf16,
};

// ---------------------------------------------------------------------------
// 編集モデル (純粋)
// ---------------------------------------------------------------------------

/// 入力欄の中身と選択範囲。gpui に触れないので単体テストで検査できる。
///
/// IME の経路 ([`InputState::replace`] / [`InputState::replace_and_mark`]) は
/// 添字の足し先を 1 つ間違えるだけで範囲外パニックまで行くので、必ずここを通す。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(super) struct InputState {
    /// 本文。
    pub text: String,
    /// 選択範囲 (バイト)。空ならキャレットだけ。
    pub selected_range: Range<usize>,
    /// 選択を左向きに伸ばしているか。キャレットは範囲の先頭側にある。
    pub selection_reversed: bool,
    /// IME の未確定範囲 (バイト)。
    pub marked_range: Option<Range<usize>>,
}

impl InputState {
    /// キャレット位置。選択の向きによって範囲のどちら側かが変わる。
    pub fn cursor(&self) -> usize {
        if self.selection_reversed {
            self.selected_range.start
        } else {
            self.selected_range.end
        }
    }

    pub fn selected_text(&self) -> &str {
        &self.text[self.selected_range.clone()]
    }

    pub fn move_to(&mut self, offset: usize) {
        let offset = offset.min(self.text.len());
        self.selected_range = offset..offset;
        self.selection_reversed = false;
    }

    /// 選択をこの位置まで伸ばす。行き過ぎたら向きを反転させる。
    pub fn select_to(&mut self, offset: usize) {
        let offset = offset.min(self.text.len());
        if self.selection_reversed {
            self.selected_range.start = offset;
        } else {
            self.selected_range.end = offset;
        }
        if self.selected_range.end < self.selected_range.start {
            self.selection_reversed = !self.selection_reversed;
            self.selected_range = self.selected_range.end..self.selected_range.start;
        }
    }

    pub fn select_all(&mut self) {
        self.selected_range = 0..self.text.len();
        self.selection_reversed = false;
    }

    /// 本文を丸ごと差し替え、キャレットを末尾へ置く。
    pub fn set_text(&mut self, text: String) {
        self.text = text;
        let end = self.text.len();
        self.selected_range = end..end;
        self.selection_reversed = false;
        self.marked_range = None;
    }

    /// 置き換える範囲を決める。IME が指定しなければ未確定範囲、それも無ければ選択範囲。
    fn target_range(&self, range_utf16: Option<Range<usize>>) -> Range<usize> {
        range_utf16
            .as_ref()
            .map(|range| range_from_utf16(&self.text, range))
            .or_else(|| self.marked_range.clone())
            .unwrap_or_else(|| self.selected_range.clone())
    }

    /// 確定した文字列を差し込む。
    pub fn replace(&mut self, range_utf16: Option<Range<usize>>, new_text: &str) {
        let range = self.target_range(range_utf16);
        self.text.replace_range(range.clone(), new_text);
        let caret = range.start + new_text.len();
        self.selected_range = caret..caret;
        // 逆向きの選択を消したあとに向きが残っていると、次の Shift+矢印が
        // 反対側の端を伸ばしてしまう。
        self.selection_reversed = false;
        self.marked_range = None;
    }

    /// 変換中の文字列を差し込み、未確定範囲を付け直す。
    pub fn replace_and_mark(
        &mut self,
        range_utf16: Option<Range<usize>>,
        new_text: &str,
        new_selected_range_utf16: Option<Range<usize>>,
    ) {
        let range = self.target_range(range_utf16);
        self.text.replace_range(range.clone(), new_text);
        self.marked_range =
            (!new_text.is_empty()).then(|| range.start..range.start + new_text.len());
        // 変換中の選択位置は「置き換えた部分の先頭」からの相対位置なので、両端とも
        // range.start を足す。range.end を足すと変換が伸びるほど選択が本文の外へ
        // はみ出し、その状態でコピーするとバイト添字で落ちる。
        self.selected_range = new_selected_range_utf16
            .as_ref()
            .map(|range_utf16| range_from_utf16(&self.text, range_utf16))
            .map(|relative| relative.start + range.start..relative.end + range.start)
            .unwrap_or_else(|| {
                let caret = range.start + new_text.len();
                caret..caret
            });
        self.selection_reversed = false;
    }
}

// ---------------------------------------------------------------------------
// キーバインド
// ---------------------------------------------------------------------------

actions!(
    text_input,
    [
        InputBackspace,
        InputDelete,
        InputDeleteWordLeft,
        InputLeft,
        InputRight,
        InputSelectLeft,
        InputSelectRight,
        InputWordLeft,
        InputWordRight,
        InputSelectWordLeft,
        InputSelectWordRight,
        InputUp,
        InputDown,
        InputLineStart,
        InputLineEnd,
        InputSelectLineStart,
        InputSelectLineEnd,
        InputSelectAll,
        InputCopy,
        InputCut,
        InputPaste,
        InputEnter,
        InputNewline,
        InputSubmit,
        InputEscape,
        InputTab,
        InputBackTab,
        InputPrevItem,
        InputNextItem,
    ]
);

/// 入力欄のキー文脈。この名前の下でだけバインドが効く。
const INPUT_CONTEXT: &str = "text_input";

/// キーバインドを 1 度だけ登録する。
///
/// [`TextInput`] の構築子から呼ぶので、呼び出し側は何もしなくてよい。ビューが
/// 作り直されるたびに積むと同じ打鍵に同じ操作が何重にも割り当たるため [`Once`]
/// で守る。
///
/// [`Once`]: std::sync::Once
fn install_text_input_keymap(cx: &mut App) {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        let ctx = Some(INPUT_CONTEXT);
        cx.bind_keys([
            KeyBinding::new("backspace", InputBackspace, ctx),
            KeyBinding::new("delete", InputDelete, ctx),
            KeyBinding::new("left", InputLeft, ctx),
            KeyBinding::new("right", InputRight, ctx),
            KeyBinding::new("shift-left", InputSelectLeft, ctx),
            KeyBinding::new("shift-right", InputSelectRight, ctx),
            KeyBinding::new("up", InputUp, ctx),
            KeyBinding::new("down", InputDown, ctx),
            KeyBinding::new("home", InputLineStart, ctx),
            KeyBinding::new("end", InputLineEnd, ctx),
            KeyBinding::new("shift-home", InputSelectLineStart, ctx),
            KeyBinding::new("shift-end", InputSelectLineEnd, ctx),
            KeyBinding::new("secondary-a", InputSelectAll, ctx),
            KeyBinding::new("secondary-c", InputCopy, ctx),
            KeyBinding::new("secondary-x", InputCut, ctx),
            KeyBinding::new("secondary-v", InputPaste, ctx),
            KeyBinding::new("enter", InputEnter, ctx),
            KeyBinding::new("shift-enter", InputNewline, ctx),
            KeyBinding::new("alt-enter", InputNewline, ctx),
            KeyBinding::new("secondary-enter", InputSubmit, ctx),
            KeyBinding::new("escape", InputEscape, ctx),
            // 一覧を持つ呼び出し側 (パレット) が候補送りに使う。入力欄は
            // 何もせずそのまま親へ流す。
            KeyBinding::new("tab", InputTab, ctx),
            KeyBinding::new("shift-tab", InputBackTab, ctx),
        ]);
        cx.bind_keys(text_input_navigation_keymap(ctx));
    });
}

/// 単語・行の端へ動く操作。
///
/// エディタ側 (`actions::editor_navigation_bindings`) と同じ理由でここだけ
/// プラットフォームで分ける。macOS は「⌘+← が行頭、⌥+← が単語」、
/// Windows/Linux は「Home が行頭、Ctrl+← が単語」。
#[cfg(target_os = "macos")]
fn text_input_navigation_keymap(ctx: Option<&'static str>) -> Vec<KeyBinding> {
    vec![
        KeyBinding::new("alt-backspace", InputDeleteWordLeft, ctx),
        KeyBinding::new("alt-left", InputWordLeft, ctx),
        KeyBinding::new("alt-right", InputWordRight, ctx),
        KeyBinding::new("alt-shift-left", InputSelectWordLeft, ctx),
        KeyBinding::new("alt-shift-right", InputSelectWordRight, ctx),
        KeyBinding::new("cmd-left", InputLineStart, ctx),
        KeyBinding::new("cmd-right", InputLineEnd, ctx),
        // Emacs 風の候補送り。macOS ではアプリ側の打鍵が ⌘ を使うので、
        // Ctrl+P / Ctrl+N を入力欄が取っても何も奪わない。
        KeyBinding::new("ctrl-p", InputPrevItem, ctx),
        KeyBinding::new("ctrl-n", InputNextItem, ctx),
    ]
}

/// Windows/Linux では Emacs 風の候補送り (Ctrl+P / Ctrl+N) を入れない。
///
/// アプリ側の打鍵も Ctrl を使うため、入力欄がここを取ると Ctrl+P
/// (クイックオープン) と Ctrl+N (新規ファイル) が入力欄にフォーカスがある
/// あいだ効かなくなる。入力欄は常にフォーカスを持っている場面が多いので、
/// 実質「効かない」に等しい。候補送りは ↑↓ で足りるので、そちらへ譲る。
#[cfg(not(target_os = "macos"))]
fn text_input_navigation_keymap(ctx: Option<&'static str>) -> Vec<KeyBinding> {
    vec![
        KeyBinding::new("ctrl-backspace", InputDeleteWordLeft, ctx),
        KeyBinding::new("ctrl-left", InputWordLeft, ctx),
        KeyBinding::new("ctrl-right", InputWordRight, ctx),
        KeyBinding::new("ctrl-shift-left", InputSelectWordLeft, ctx),
        KeyBinding::new("ctrl-shift-right", InputSelectWordRight, ctx),
    ]
}

// ---------------------------------------------------------------------------
// 設定と出来事
// ---------------------------------------------------------------------------

/// 呼び出し側が奪えるキー。
///
/// [`TextInput::reserving`] で宣言すると、入力欄はそのキーを処理せず、対応する
/// アクション ([`InputEnter`] など) をそのまま親へ流す。親は `on_action` で
/// 受ければよく、`Window` も手に入る。
///
/// なお `tab` / `shift-tab` / `ctrl-p` / `ctrl-n` は入力欄が最初から何もしない
/// ので、宣言しなくても親に届く。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InputKey {
    Enter,
    Escape,
    Up,
    Down,
}

/// 差し込む文字列から、その欄が持てない改行を取り除く。
///
/// 貼り付けは複数行の文字列が来る唯一の経路なので、ここを通さないと 1 行欄の
/// 本文に改行が紛れ込み、字形化した行と本文のバイト位置がずれる。
pub(super) fn sanitize_insert(text: &str, multi_line: bool, policy: NewlinePolicy) -> String {
    if multi_line {
        return text.to_string();
    }
    match policy {
        NewlinePolicy::Space => text.replace(['\n', '\r'], " "),
        NewlinePolicy::Strip => text.replace(['\n', '\r'], ""),
    }
}

/// 1 行入力欄に改行が貼り付けられたときの扱い。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NewlinePolicy {
    /// 空白に潰す。文章を貼ったときに単語が繋がらない。
    Space,
    /// 取り除く。ファイル名など空白も避けたい欄向け。
    Strip,
}

/// 入力欄から呼び出し側への通知。
pub enum TextInputEvent {
    /// 本文が変わった。IME の変換中も届く。
    Changed,
    /// 確定 (Enter、複数行なら ⌘Enter)。
    Submit,
    /// 取り消し (Escape)。
    Cancel,
    /// フォーカスの出入り。枠の色を親が描く場合に使う。
    FocusChanged,
}

/// 欄ごとの振る舞い。
#[derive(Debug, Clone)]
struct InputConfig {
    multi_line: bool,
    /// Enter を確定にするか。`false` かつ複数行なら Enter は改行になる。
    enter_submits: bool,
    newline_policy: NewlinePolicy,
    /// 空でも確保する行数。
    min_rows: usize,
    /// これを超えたらスクロールする。`None` なら伸び続ける。
    max_rows: Option<usize>,
    reserved: Vec<InputKey>,
}

// ---------------------------------------------------------------------------
// エンティティ
// ---------------------------------------------------------------------------

/// 共通のテキスト入力欄。
///
/// 枠・背景・余白は持たない。呼び出し側が好きな箱に入れて使う。
pub struct TextInput {
    focus_handle: FocusHandle,
    state: InputState,
    placeholder: SharedString,
    config: InputConfig,
    /// 直前の描画で確定した幾何。マウス座標や IME の位置問い合わせに使う。
    layout: Option<InputLayout>,
    /// 直前の描画時点でフォーカスがあったか。
    ///
    /// 枠のネオンは親が描くので、親が読めるところに持っておく必要がある。
    focused: bool,
    is_selecting: bool,
}

impl EventEmitter<TextInputEvent> for TextInput {}

impl TextInput {
    /// 1 行の欄。Enter は [`TextInputEvent::Submit`]。
    pub fn single_line(placeholder: impl Into<SharedString>, cx: &mut Context<Self>) -> Self {
        install_text_input_keymap(cx);
        Self {
            focus_handle: cx.focus_handle(),
            state: InputState::default(),
            placeholder: placeholder.into(),
            config: InputConfig {
                multi_line: false,
                enter_submits: true,
                newline_policy: NewlinePolicy::Space,
                min_rows: 1,
                max_rows: Some(1),
                reserved: Vec::new(),
            },
            layout: None,
            focused: false,
            is_selecting: false,
        }
    }

    /// 複数行の欄。`max_rows` を超えるとキャレットを追ってスクロールする。
    pub fn multi_line(
        placeholder: impl Into<SharedString>,
        min_rows: usize,
        max_rows: Option<usize>,
        cx: &mut Context<Self>,
    ) -> Self {
        let mut input = Self::single_line(placeholder, cx);
        input.config.multi_line = true;
        input.config.min_rows = min_rows.max(1);
        input.config.max_rows = max_rows;
        input
    }

    // -- 組み立て --

    /// 初期値を入れる。キャレットは末尾。
    pub fn with_text(mut self, text: impl Into<String>) -> Self {
        self.state.set_text(text.into());
        self
    }

    /// 初期値を全選択した状態で始める。名前の変更のように「打ち直しが既定」の欄向け。
    pub fn with_all_selected(mut self) -> Self {
        self.state.select_all();
        self
    }

    /// Enter を確定にするか。複数行で `false` にすると Enter が改行になる。
    pub fn submitting_on_enter(mut self, yes: bool) -> Self {
        self.config.enter_submits = yes;
        self
    }

    /// 1 行欄に改行が来たときの扱い。
    pub fn with_newline_policy(mut self, policy: NewlinePolicy) -> Self {
        self.config.newline_policy = policy;
        self
    }

    /// 呼び出し側が自分で処理するキーを宣言する。
    pub fn reserving(mut self, keys: &[InputKey]) -> Self {
        self.config.reserved = keys.to_vec();
        self
    }

    // -- 呼び出し側から --

    pub fn text(&self) -> &str {
        &self.state.text
    }

    /// 直前の描画時点でフォーカスがあったか。枠の色を決めるのに使う。
    pub fn is_focused(&self) -> bool {
        self.focused
    }

    pub fn set_text(&mut self, text: impl Into<String>, cx: &mut Context<Self>) {
        self.state.set_text(text.into());
        cx.emit(TextInputEvent::Changed);
        cx.notify();
    }

    pub fn clear(&mut self, cx: &mut Context<Self>) {
        self.set_text(String::new(), cx);
    }

    pub fn select_all(&mut self, cx: &mut Context<Self>) {
        self.state.select_all();
        cx.notify();
    }

    pub fn focus(&self, window: &mut Window) {
        window.focus(&self.focus_handle);
    }

    // -- 内部 --

    fn reserved(&self, key: InputKey) -> bool {
        self.config.reserved.contains(&key)
    }

    fn move_to(&mut self, offset: usize, cx: &mut Context<Self>) {
        self.state.move_to(offset);
        cx.notify();
    }

    fn select_to(&mut self, offset: usize, cx: &mut Context<Self>) {
        self.state.select_to(offset);
        cx.notify();
    }

    /// 1 行欄では改行を持てないので、差し込む前に潰す。
    fn sanitize(&self, text: &str) -> String {
        sanitize_insert(text, self.config.multi_line, self.config.newline_policy)
    }

    fn insert(&mut self, text: &str, window: &mut Window, cx: &mut Context<Self>) {
        self.replace_text_in_range(None, text, window, cx);
    }

    // -- アクション --

    fn on_backspace(&mut self, _: &InputBackspace, window: &mut Window, cx: &mut Context<Self>) {
        if self.state.selected_range.is_empty() {
            self.select_to(previous_grapheme(&self.state.text, self.state.cursor()), cx);
        }
        self.insert("", window, cx);
    }

    fn on_delete(&mut self, _: &InputDelete, window: &mut Window, cx: &mut Context<Self>) {
        if self.state.selected_range.is_empty() {
            self.select_to(next_grapheme(&self.state.text, self.state.cursor()), cx);
        }
        self.insert("", window, cx);
    }

    fn on_delete_word_left(
        &mut self,
        _: &InputDeleteWordLeft,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.state.selected_range.is_empty() {
            self.select_to(
                previous_word_boundary(&self.state.text, self.state.cursor()),
                cx,
            );
        }
        self.insert("", window, cx);
    }

    fn on_left(&mut self, _: &InputLeft, _: &mut Window, cx: &mut Context<Self>) {
        if self.state.selected_range.is_empty() {
            self.move_to(previous_grapheme(&self.state.text, self.state.cursor()), cx);
        } else {
            self.move_to(self.state.selected_range.start, cx);
        }
    }

    fn on_right(&mut self, _: &InputRight, _: &mut Window, cx: &mut Context<Self>) {
        if self.state.selected_range.is_empty() {
            self.move_to(next_grapheme(&self.state.text, self.state.cursor()), cx);
        } else {
            self.move_to(self.state.selected_range.end, cx);
        }
    }

    fn on_select_left(&mut self, _: &InputSelectLeft, _: &mut Window, cx: &mut Context<Self>) {
        self.select_to(previous_grapheme(&self.state.text, self.state.cursor()), cx);
    }

    fn on_select_right(&mut self, _: &InputSelectRight, _: &mut Window, cx: &mut Context<Self>) {
        self.select_to(next_grapheme(&self.state.text, self.state.cursor()), cx);
    }

    fn on_word_left(&mut self, _: &InputWordLeft, _: &mut Window, cx: &mut Context<Self>) {
        self.move_to(
            previous_word_boundary(&self.state.text, self.state.cursor()),
            cx,
        );
    }

    fn on_word_right(&mut self, _: &InputWordRight, _: &mut Window, cx: &mut Context<Self>) {
        self.move_to(
            next_word_boundary(&self.state.text, self.state.cursor()),
            cx,
        );
    }

    fn on_select_word_left(
        &mut self,
        _: &InputSelectWordLeft,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.select_to(
            previous_word_boundary(&self.state.text, self.state.cursor()),
            cx,
        );
    }

    fn on_select_word_right(
        &mut self,
        _: &InputSelectWordRight,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.select_to(
            next_word_boundary(&self.state.text, self.state.cursor()),
            cx,
        );
    }

    fn on_line_start(&mut self, _: &InputLineStart, _: &mut Window, cx: &mut Context<Self>) {
        let start = line_bounds(&self.state.text, self.state.cursor()).start;
        self.move_to(start, cx);
    }

    fn on_line_end(&mut self, _: &InputLineEnd, _: &mut Window, cx: &mut Context<Self>) {
        let end = line_bounds(&self.state.text, self.state.cursor()).end;
        self.move_to(end, cx);
    }

    fn on_select_line_start(
        &mut self,
        _: &InputSelectLineStart,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let start = line_bounds(&self.state.text, self.state.cursor()).start;
        self.select_to(start, cx);
    }

    fn on_select_line_end(
        &mut self,
        _: &InputSelectLineEnd,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let end = line_bounds(&self.state.text, self.state.cursor()).end;
        self.select_to(end, cx);
    }

    fn on_select_all(&mut self, _: &InputSelectAll, _: &mut Window, cx: &mut Context<Self>) {
        self.select_all(cx);
    }

    fn on_up(&mut self, _: &InputUp, _: &mut Window, cx: &mut Context<Self>) {
        if self.reserved(InputKey::Up) || !self.config.multi_line {
            cx.propagate();
            return;
        }
        self.move_vertically(true, cx);
    }

    fn on_down(&mut self, _: &InputDown, _: &mut Window, cx: &mut Context<Self>) {
        if self.reserved(InputKey::Down) || !self.config.multi_line {
            cx.propagate();
            return;
        }
        self.move_vertically(false, cx);
    }

    /// 上下移動。折り返しを跨ぐので、行番号ではなく描画座標で 1 行ぶん動かす。
    fn move_vertically(&mut self, up: bool, cx: &mut Context<Self>) {
        let Some(layout) = self.layout.as_ref() else {
            return;
        };
        let Some(current) = layout.point_for_offset(self.state.cursor()) else {
            return;
        };
        let delta = if up {
            -layout.line_height
        } else {
            layout.line_height
        };
        let offset = layout.offset_for_point(point(current.x, current.y + delta));
        self.move_to(offset, cx);
    }

    fn on_copy(&mut self, _: &InputCopy, _: &mut Window, cx: &mut Context<Self>) {
        if self.state.selected_range.is_empty() {
            return;
        }
        cx.write_to_clipboard(ClipboardItem::new_string(
            self.state.selected_text().to_string(),
        ));
    }

    fn on_cut(&mut self, _: &InputCut, window: &mut Window, cx: &mut Context<Self>) {
        if self.state.selected_range.is_empty() {
            return;
        }
        cx.write_to_clipboard(ClipboardItem::new_string(
            self.state.selected_text().to_string(),
        ));
        self.insert("", window, cx);
    }

    fn on_paste(&mut self, _: &InputPaste, window: &mut Window, cx: &mut Context<Self>) {
        let Some(text) = cx.read_from_clipboard().and_then(|item| item.text()) else {
            return;
        };
        self.insert(&text, window, cx);
    }

    fn on_enter(&mut self, _: &InputEnter, window: &mut Window, cx: &mut Context<Self>) {
        if self.reserved(InputKey::Enter) {
            cx.propagate();
            return;
        }
        if self.config.multi_line && !self.config.enter_submits {
            self.insert("\n", window, cx);
            return;
        }
        self.submit(cx);
    }

    fn on_newline(&mut self, _: &InputNewline, window: &mut Window, cx: &mut Context<Self>) {
        if !self.config.multi_line {
            cx.propagate();
            return;
        }
        self.insert("\n", window, cx);
    }

    fn on_submit(&mut self, _: &InputSubmit, _: &mut Window, cx: &mut Context<Self>) {
        self.submit(cx);
    }

    fn submit(&mut self, cx: &mut Context<Self>) {
        // 変換中の確定は IME が握っている。未確定文字が残っていれば送らない。
        if self.state.marked_range.is_some() {
            return;
        }
        cx.emit(TextInputEvent::Submit);
    }

    fn on_escape(&mut self, _: &InputEscape, _: &mut Window, cx: &mut Context<Self>) {
        if self.reserved(InputKey::Escape) {
            cx.propagate();
            return;
        }
        cx.emit(TextInputEvent::Cancel);
    }

    // -- マウス --

    fn on_mouse_down(
        &mut self,
        event: &MouseDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        window.focus(&self.focus_handle);
        self.is_selecting = true;
        let offset = self.offset_for_window_point(event.position);
        if event.modifiers.shift {
            self.select_to(offset, cx);
        } else {
            self.move_to(offset, cx);
        }
    }

    fn on_mouse_move(&mut self, event: &MouseMoveEvent, _: &mut Window, cx: &mut Context<Self>) {
        if !self.is_selecting {
            return;
        }
        let offset = self.offset_for_window_point(event.position);
        self.select_to(offset, cx);
    }

    fn on_mouse_up(&mut self, _: &MouseUpEvent, _: &mut Window, _: &mut Context<Self>) {
        self.is_selecting = false;
    }

    /// ウィンドウ座標からバイト位置を引く。
    ///
    /// ドラッグは欄の外へ出ても続くので、`Bounds::localize` (範囲外で `None`)
    /// ではなく引き算と丸めで求める。
    fn offset_for_window_point(&self, position: Point<Pixels>) -> usize {
        let Some(layout) = self.layout.as_ref() else {
            return self.state.text.len();
        };
        let local = text_space_point(layout.bounds, layout.scroll_y(), position);
        layout.offset_for_point(local).min(self.state.text.len())
    }
}

/// ウィンドウ座標を「本文の左上を原点とする座標」へ直す。
///
/// `Bounds::localize` も同じ値 (origin からの相対) を返す。その結果から更に
/// origin を引くと常に欄の左端を指してしまうので、引き算は 1 回だけにする。
fn text_space_point(
    bounds: Bounds<Pixels>,
    scroll_y: Pixels,
    position: Point<Pixels>,
) -> Point<Pixels> {
    point(
        position.x - bounds.left(),
        position.y - bounds.top() + scroll_y,
    )
}

impl Focusable for TextInput {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl EntityInputHandler for TextInput {
    fn text_for_range(
        &mut self,
        range_utf16: Range<usize>,
        actual_range: &mut Option<Range<usize>>,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<String> {
        let range = range_from_utf16(&self.state.text, &range_utf16);
        actual_range.replace(range_to_utf16(&self.state.text, &range));
        Some(self.state.text[range].to_string())
    }

    fn selected_text_range(
        &mut self,
        _ignore_disabled_input: bool,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<UTF16Selection> {
        Some(UTF16Selection {
            range: range_to_utf16(&self.state.text, &self.state.selected_range),
            reversed: self.state.selection_reversed,
        })
    }

    fn marked_text_range(&self, _: &mut Window, _: &mut Context<Self>) -> Option<Range<usize>> {
        self.state
            .marked_range
            .as_ref()
            .map(|range| range_to_utf16(&self.state.text, range))
    }

    fn unmark_text(&mut self, _: &mut Window, _: &mut Context<Self>) {
        self.state.marked_range = None;
    }

    fn replace_text_in_range(
        &mut self,
        range_utf16: Option<Range<usize>>,
        new_text: &str,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let new_text = self.sanitize(new_text);
        self.state.replace(range_utf16, &new_text);
        cx.emit(TextInputEvent::Changed);
        cx.notify();
    }

    fn replace_and_mark_text_in_range(
        &mut self,
        range_utf16: Option<Range<usize>>,
        new_text: &str,
        new_selected_range_utf16: Option<Range<usize>>,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.state
            .replace_and_mark(range_utf16, new_text, new_selected_range_utf16);
        // 日本語などの変換中もここだけを通る。ここで知らせないと絞り込みが追従しない。
        cx.emit(TextInputEvent::Changed);
        cx.notify();
    }

    fn bounds_for_range(
        &mut self,
        range_utf16: Range<usize>,
        element_bounds: Bounds<Pixels>,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<Bounds<Pixels>> {
        // 変換候補の窓を出す位置。未確定範囲の先頭に付ける。
        let layout = self.layout.as_ref()?;
        let range = range_from_utf16(&self.state.text, &range_utf16);
        let position = layout.point_for_offset(range.start)?;
        Some(Bounds::new(
            point(
                element_bounds.left() + position.x,
                element_bounds.top() + position.y - layout.scroll_y(),
            ),
            size(px(2.), layout.line_height),
        ))
    }

    fn character_index_for_point(
        &mut self,
        position: Point<Pixels>,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<usize> {
        let layout = self.layout.as_ref()?;
        // localize は既に origin からの相対位置を返す。ここで更に origin を引くと
        // 常に欄の左端を問い合わせることになり、変換候補の位置がずれる。
        let local = layout.bounds.localize(&position)?;
        let offset = layout
            .offset_for_point(point(local.x, local.y + layout.scroll_y()))
            .min(self.state.text.len());
        Some(offset_to_utf16(&self.state.text, offset))
    }
}

impl Render for TextInput {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .key_context(INPUT_CONTEXT)
            .track_focus(&self.focus_handle)
            .size_full()
            .cursor(gpui::CursorStyle::IBeam)
            .on_action(cx.listener(Self::on_backspace))
            .on_action(cx.listener(Self::on_delete))
            .on_action(cx.listener(Self::on_delete_word_left))
            .on_action(cx.listener(Self::on_left))
            .on_action(cx.listener(Self::on_right))
            .on_action(cx.listener(Self::on_select_left))
            .on_action(cx.listener(Self::on_select_right))
            .on_action(cx.listener(Self::on_word_left))
            .on_action(cx.listener(Self::on_word_right))
            .on_action(cx.listener(Self::on_select_word_left))
            .on_action(cx.listener(Self::on_select_word_right))
            .on_action(cx.listener(Self::on_up))
            .on_action(cx.listener(Self::on_down))
            .on_action(cx.listener(Self::on_line_start))
            .on_action(cx.listener(Self::on_line_end))
            .on_action(cx.listener(Self::on_select_line_start))
            .on_action(cx.listener(Self::on_select_line_end))
            .on_action(cx.listener(Self::on_select_all))
            .on_action(cx.listener(Self::on_copy))
            .on_action(cx.listener(Self::on_cut))
            .on_action(cx.listener(Self::on_paste))
            .on_action(cx.listener(Self::on_enter))
            .on_action(cx.listener(Self::on_newline))
            .on_action(cx.listener(Self::on_submit))
            .on_action(cx.listener(Self::on_escape))
            .on_mouse_down(MouseButton::Left, cx.listener(Self::on_mouse_down))
            .on_mouse_move(cx.listener(Self::on_mouse_move))
            .on_mouse_up(MouseButton::Left, cx.listener(Self::on_mouse_up))
            .on_mouse_up_out(MouseButton::Left, cx.listener(Self::on_mouse_up))
            .child(TextInputElement { input: cx.entity() })
    }
}

impl TextInput {
    /// 描画する文字列。空なら案内文を出す。
    fn display_text(&self) -> SharedString {
        if self.state.text.is_empty() {
            self.placeholder.clone()
        } else {
            SharedString::from(self.state.text.clone())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // -----------------------------------------------------------------
    // 入力欄: 編集モデル
    // -----------------------------------------------------------------

    fn 状態(text: &str, range: Range<usize>) -> InputState {
        InputState {
            text: text.to_string(),
            selected_range: range,
            selection_reversed: false,
            marked_range: None,
        }
    }

    #[test]
    fn キャレットは選択の向きで端が変わる() {
        let mut state = 状態("abcdef", 2..4);
        assert_eq!(state.cursor(), 4);
        state.selection_reversed = true;
        assert_eq!(state.cursor(), 2);
    }

    #[test]
    fn 選択を行き過ぎると向きが反転する() {
        let mut state = 状態("abcdef", 3..3);
        state.select_to(1);
        assert_eq!(state.selected_range, 1..3);
        assert!(state.selection_reversed);
        state.select_to(5);
        assert_eq!(state.selected_range, 3..5);
        assert!(!state.selection_reversed);
    }

    #[test]
    fn 選択範囲を入力で置き換える() {
        let mut state = 状態("abcdef", 1..4);
        state.replace(None, "X");
        assert_eq!(state.text, "aXef");
        assert_eq!(state.selected_range, 2..2);
    }

    /// 回帰: 逆向き選択のあとに入力すると Shift+矢印が反対の端を伸ばしていた。
    #[test]
    fn 入力すると選択の向きが必ず戻る() {
        let mut state = 状態("abcdef", 1..4);
        state.selection_reversed = true;
        state.replace(None, "X");
        assert!(!state.selection_reversed, "確定入力で向きが残っている");

        let mut state = 状態("abcdef", 1..4);
        state.selection_reversed = true;
        state.replace_and_mark(None, "あ", Some(0..1));
        assert!(!state.selection_reversed, "変換中の入力で向きが残っている");
    }

    /// 回帰: 変換中の選択範囲の終端に `range.end` を足していた。変換が伸びるほど
    /// 選択が本文の外へ出て、その状態でコピーすると添字範囲外で落ちた。
    #[test]
    fn 変換中の選択範囲は本文の内側に収まる() {
        let mut state = 状態("先頭", 6..6);
        // 「にほんご」を変換中。IME は差し込んだ文字列の先頭からの相対位置を寄こす。
        state.replace_and_mark(None, "にほんご", Some(0..4));
        assert_eq!(state.text, "先頭にほんご");
        assert_eq!(state.marked_range, Some(6..18));
        assert_eq!(state.selected_range, 6..18);
        assert!(
            state.selected_range.end <= state.text.len(),
            "選択が本文の外へ出ている"
        );
        // ここで落ちなければ回帰していない。
        assert_eq!(state.selected_text(), "にほんご");
    }

    #[test]
    fn 変換が伸びても選択は本文の内側に収まる() {
        let mut state = 状態("", 0..0);
        for length in 1..=8 {
            let text: String = "あ".repeat(length);
            state.replace_and_mark(None, &text, Some(0..length));
            assert!(
                state.selected_range.end <= state.text.len(),
                "{length} 文字目で選択が本文の外へ出た"
            );
            assert_eq!(state.selected_text(), text);
        }
    }

    #[test]
    fn 変換の確定で未確定範囲が消える() {
        let mut state = 状態("", 0..0);
        state.replace_and_mark(None, "にほん", Some(0..3));
        assert!(state.marked_range.is_some());
        state.replace(None, "日本");
        assert_eq!(state.text, "日本");
        assert_eq!(state.marked_range, None);
        assert_eq!(state.selected_range, 6..6);
    }

    #[test]
    fn 変換を空文字で取り消すと未確定範囲が消える() {
        let mut state = 状態("", 0..0);
        state.replace_and_mark(None, "にほん", Some(0..3));
        state.replace_and_mark(None, "", None);
        assert_eq!(state.text, "");
        assert_eq!(state.marked_range, None);
    }

    #[test]
    fn 全選択と本文の入れ替え() {
        let mut state = 状態("abc", 0..0);
        state.select_all();
        assert_eq!(state.selected_range, 0..3);
        state.set_text("あいう".into());
        assert_eq!(state.selected_range, 9..9);
        assert!(state.marked_range.is_none());
    }

    /// 回帰: IME が「置き換える範囲」を明示してきた経路。ここを取り違えると
    /// 変換候補を選び直すたびに本文が壊れる。
    #[test]
    fn 変換中に_ime_が指定した範囲だけを置き換える() {
        let mut state = 状態("あab", 9..9);
        // UTF-16 で 1..3 は「ab」の部分。
        state.replace(Some(1..3), "い");
        assert_eq!(state.text, "あい");
        assert_eq!(state.selected_range, 6..6);
    }

    #[test]
    fn 変換中の未確定範囲は次の変換で置き換わる() {
        let mut state = 状態("", 0..0);
        state.replace_and_mark(None, "にほ", Some(0..2));
        assert_eq!(state.marked_range, Some(0..6));
        // 範囲を渡さなければ未確定範囲が置き換え先になる。
        state.replace_and_mark(None, "にほん", Some(0..3));
        assert_eq!(state.text, "にほん");
        assert_eq!(state.marked_range, Some(0..9));
        assert_eq!(state.selected_text(), "にほん");
    }

    #[test]
    fn 選択の伸ばし先は本文の末尾で止まる() {
        let mut state = 状態("abc", 1..1);
        state.select_to(99);
        assert_eq!(state.selected_range, 1..3);
        assert_eq!(state.selected_text(), "bc");
    }

    #[test]
    fn キャレットの移動先は本文の末尾で止まる() {
        let mut state = 状態("あい", 0..0);
        state.move_to(99);
        assert_eq!(state.cursor(), 6);
    }

    #[test]
    fn 逆向きの選択を消したあとのキャレットは差し込んだ末尾() {
        let mut state = 状態("abcdef", 1..4);
        state.selection_reversed = true;
        state.replace(None, "XY");
        assert_eq!(state.text, "aXYef");
        assert_eq!(state.cursor(), 3);
    }

    // -----------------------------------------------------------------
    // 入力欄: 貼り付けた改行の扱い
    // -----------------------------------------------------------------

    #[test]
    fn 一行欄は貼り付けた改行を空白に潰す() {
        // 文章を貼ったときに単語が繋がらないよう空白を残す。
        assert_eq!(
            sanitize_insert("あ\nい\r\nう", false, NewlinePolicy::Space),
            "あ い  う"
        );
    }

    #[test]
    fn ファイル名の欄は貼り付けた改行を取り除く() {
        assert_eq!(
            sanitize_insert("a\nb\r\nc", false, NewlinePolicy::Strip),
            "abc"
        );
    }

    #[test]
    fn 複数行欄は貼り付けた改行をそのまま通す() {
        assert_eq!(sanitize_insert("a\nb", true, NewlinePolicy::Strip), "a\nb");
    }

    #[test]
    fn 潰した改行を差し込んでも本文の位置がずれない() {
        // 1 行欄の本文に改行が残ると、字形化した行と本文のバイト位置がずれる。
        let mut state = 状態("", 0..0);
        let pasted = sanitize_insert("あ\nい", false, NewlinePolicy::Space);
        state.replace(None, &pasted);
        assert!(!state.text.contains('\n'));
        assert_eq!(state.cursor(), state.text.len());
    }

    // -----------------------------------------------------------------
    // 入力欄: 座標の変換
    // -----------------------------------------------------------------

    /// 回帰: `Bounds::localize` の結果から更に origin を引いていた。
    #[test]
    fn localize_は既に原点相対の位置を返す() {
        let bounds = Bounds::new(point(px(100.), px(10.)), size(px(200.), px(20.)));
        let local = bounds
            .localize(&point(px(150.), px(15.)))
            .expect("範囲の内側");
        assert_eq!(local.x, px(50.));
        // 旧実装の `point.x - local.x` は origin.x そのものになり、
        // どこを押しても常に欄の左端が返っていた。
        assert_eq!(px(150.) - local.x, bounds.left());
    }

    #[test]
    fn 本文座標への変換は原点を一度だけ引く() {
        let bounds = Bounds::new(point(px(100.), px(10.)), size(px(200.), px(60.)));
        let local = text_space_point(bounds, px(0.), point(px(150.), px(30.)));
        assert_eq!(local.x, px(50.));
        assert_eq!(local.y, px(20.));
    }

    #[test]
    fn 本文座標はスクロールぶんを足し戻す() {
        let bounds = Bounds::new(point(px(100.), px(10.)), size(px(200.), px(60.)));
        let local = text_space_point(bounds, px(40.), point(px(150.), px(30.)));
        assert_eq!(local.y, px(60.));
    }

    #[test]
    fn 本文座標は枠の外でも求まる() {
        // ドラッグは欄の外へ出ても続くので、範囲外でも値が要る。
        let bounds = Bounds::new(point(px(100.), px(10.)), size(px(200.), px(60.)));
        let local = text_space_point(bounds, px(0.), point(px(400.), px(200.)));
        assert_eq!(local.x, px(300.));
        assert_eq!(local.y, px(190.));
    }
}
