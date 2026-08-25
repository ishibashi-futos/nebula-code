//! 画面をまたいで使う小さな部品。
//!
//! 各ビューが独自にボタンを組み立てると余白や配色がずれるので、
//! 見た目を決める要素はここに集約する。
//!
//! テキスト入力欄 ([`TextInput`]) もここに置く。gpui には編集できる欄が無いため
//! 自前で組む必要があり、以前は 6 つのビューがそれぞれ写経して持っていた。
//! 写し間違いから IME 経路のバグが実際に生まれたので、1 つにまとめてある。

use crate::assets::Icon;
use crate::theme::{Theme, theme};
use gpui::prelude::*;
use gpui::{
    AnyView, App, AvailableSpace, Bounds, ClipboardItem, Context, Div, Element, ElementId,
    ElementInputHandler, Entity, EntityInputHandler, EventEmitter, FocusHandle, Focusable, Font,
    GlobalElementId, Hsla, IntoElement, KeyBinding, LayoutId, MouseButton, MouseDownEvent,
    MouseMoveEvent, MouseUpEvent, PaintQuad, Pixels, Point, SharedString, Size, Stateful, Style,
    TextRun, UTF16Selection, UnderlineStyle, Window, WrappedLine, actions, div, fill, point, px,
    relative, size, svg,
};
use std::ops::Range;
use unicode_segmentation::UnicodeSegmentation;

/// 横並びの箱。
pub fn h_flex() -> Div {
    div().flex().flex_row().items_center()
}

/// 縦並びの箱。
pub fn v_flex() -> Div {
    div().flex().flex_col()
}

/// アイコン 1 つ。
pub fn icon(icon: Icon, size: Pixels, color: Hsla) -> impl IntoElement {
    svg()
        .path(icon.path())
        .size(size)
        .flex_none()
        .text_color(color)
}

/// アクティビティバーやツールバーで使う四角いアイコンボタン。
pub fn icon_button(id: impl Into<ElementId>, glyph: Icon, active: bool, cx: &App) -> Stateful<Div> {
    let theme = theme(cx);
    let color = if active {
        theme.accent
    } else {
        theme.text_muted
    };
    h_flex()
        .id(id)
        .justify_center()
        .size(px(28.))
        .rounded(px(6.))
        .child(icon(glyph, px(16.), color))
        .hover(|s| s.bg(theme.bg_overlay))
        .active(|s| s.bg(theme.accent_soft))
        .cursor_pointer()
}

/// アクティビティバー (最左の縦帯) の項目。選択中は左端にネオンの縦線が出る。
pub fn activity_item(
    id: impl Into<ElementId>,
    glyph: Icon,
    active: bool,
    cx: &App,
) -> Stateful<Div> {
    let theme = theme(cx);
    let color = if active {
        theme.accent
    } else {
        theme.text_faint
    };
    h_flex()
        .id(id)
        .relative()
        .justify_center()
        .w_full()
        .h(px(44.))
        .cursor_pointer()
        .when(active, |el| {
            el.child(
                div()
                    .absolute()
                    .left_0()
                    .top(px(10.))
                    .bottom(px(10.))
                    .w(px(2.))
                    .bg(theme.accent)
                    .rounded_r(px(2.)),
            )
        })
        .child(icon(glyph, px(20.), color))
        .hover(|s| s.bg(theme.bg_surface))
}

/// サイドバー各セクションの見出し。
pub fn panel_header(title: impl Into<SharedString>, cx: &App) -> Div {
    let theme = theme(cx);
    h_flex()
        .h(px(32.))
        .px(px(12.))
        .justify_between()
        .flex_none()
        .child(
            div()
                .text_size(px(10.5))
                .font_weight(gpui::FontWeight::SEMIBOLD)
                .text_color(theme.text_muted)
                .child(title.into().to_uppercase()),
        )
}

/// 一覧の 1 行。エクスプローラー・検索結果・git 変更一覧で共通して使う。
pub fn list_row(id: impl Into<ElementId>, selected: bool, cx: &App) -> Stateful<Div> {
    let theme = theme(cx);
    h_flex()
        .id(id)
        .w_full()
        .h(px(22.))
        .px(px(8.))
        .gap(px(6.))
        .text_size(px(12.5))
        .text_color(theme.text)
        .cursor_pointer()
        .when(selected, |el| el.bg(theme.accent_soft))
        .hover(|s| s.bg(theme.bg_overlay))
}

/// 押せる主要ボタン。
pub fn primary_button(
    id: impl Into<ElementId>,
    label: impl Into<SharedString>,
    enabled: bool,
    cx: &App,
) -> Stateful<Div> {
    let theme = theme(cx);
    h_flex()
        .id(id)
        .h(px(26.))
        .px(px(12.))
        .justify_center()
        .rounded(px(5.))
        .text_size(px(12.))
        .when(enabled, |el| {
            el.bg(theme.accent)
                .text_color(theme.text_inverse)
                .cursor_pointer()
                .hover(|s| s.bg(theme.accent_tertiary))
        })
        .when(!enabled, |el| {
            el.bg(theme.bg_overlay).text_color(theme.text_faint)
        })
        .child(label.into())
}

/// 控えめなボタン。
pub fn ghost_button(
    id: impl Into<ElementId>,
    label: impl Into<SharedString>,
    cx: &App,
) -> Stateful<Div> {
    let theme = theme(cx);
    h_flex()
        .id(id)
        .h(px(26.))
        .px(px(10.))
        .justify_center()
        .rounded(px(5.))
        .text_size(px(12.))
        .text_color(theme.text_muted)
        .border_1()
        .border_color(theme.border)
        .cursor_pointer()
        .hover(|s| s.bg(theme.bg_overlay).text_color(theme.text))
        .child(label.into())
}

/// アイコンのみ・略語のみのボタンに添える、1 行のツールチップ。
///
/// gpui コアには Zed の `ui::Tooltip::text` のような既製ビューが無いため、
/// 補完欄・ホバーカード・コンテキストメニュー ([`crate::views::explorer::ExplorerView`]
/// の右クリックメニューなど) と同じ角丸パネルを自前で描く小さな `Render` ビューを
/// 都度組み立てて返す。`.tooltip(simple_tooltip("…"))` として
/// [`gpui::InteractiveElement::tooltip`] にそのまま渡せる。
pub fn simple_tooltip(
    text: impl Into<SharedString>,
) -> impl Fn(&mut Window, &mut App) -> AnyView + 'static {
    let text = text.into();
    move |_window, cx| {
        cx.new(|_cx| SimpleTooltip {
            text: text.clone(),
        })
        .into()
    }
}

struct SimpleTooltip {
    text: SharedString,
}

impl Render for SimpleTooltip {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = theme(cx).clone();
        div()
            .px(px(8.))
            .py(px(4.))
            .rounded(px(6.))
            .bg(theme.bg_overlay)
            .border_1()
            .border_color(theme.border_glow)
            .shadow_lg()
            .text_size(px(11.))
            .text_color(theme.text)
            .child(self.text.clone())
    }
}

/// ツールチップの文言を集約する。
///
/// 各ビューにベタ書きすると、似た意味の文言が少しずつ違う言い回しで重複したり、
/// 見直すときに一覧できなかったりする。ここに集めておき、下のテストで
/// 「空でない」「重複が無い」「ショートカット併記の形式が揃っている」を機械的に縛る。
pub mod tooltip_text {
    // -- アプリ全体 (app.rs) --
    pub const ADD_WORKSPACE: &str = "ワークスペースを追加 (⌘O)";
    pub const SHOW_EXPLORER: &str = "エクスプローラー (⌘⇧E)";
    pub const SHOW_SEARCH: &str = "検索 (⌘⇧F)";
    pub const SHOW_GIT: &str = "ソース管理 (⌘⇧G)";
    pub const SHOW_CODEX: &str = "Codex (⌘⇧A)";
    pub const SETTINGS: &str = "設定 (未実装)";
    // ⌘J は「パネルの表示切り替え」であって「閉じる」専用ではないので、
    // このボタンにはショートカットを併記しない。閉じるだけの鍵は無い。
    pub const CLOSE_PANEL: &str = "パネルを閉じる";

    // -- エクスプローラー (views/explorer.rs) --
    pub const EXPLORER_NEW_FILE: &str = "新規ファイル";
    pub const EXPLORER_NEW_FOLDER: &str = "新規フォルダ";
    pub const EXPLORER_RELOAD: &str = "再読み込み";
    pub const EXPLORER_TOGGLE_HIDDEN: &str = "隠しファイルの表示切り替え";

    // -- 検索 (views/search.rs) --
    pub const SEARCH_REFRESH: &str = "検索をやり直す";
    pub const SEARCH_TOGGLE_REPLACE: &str = "置換欄の表示切り替え";
    pub const SEARCH_CASE_SENSITIVE: &str = "大文字・小文字を区別";
    pub const SEARCH_WHOLE_WORD: &str = "単語単位で検索";
    pub const SEARCH_REGEX: &str = "正規表現を使用";

    // -- ソース管理 (views/git.rs) --
    pub const GIT_SWITCH_BRANCH: &str = "ブランチを切り替え";
    pub const GIT_PULL: &str = "プル";
    pub const GIT_PUSH: &str = "プッシュ";
    pub const GIT_DISCARD: &str = "変更を破棄";
    pub const GIT_UNSTAGE: &str = "ステージを取り消す";
    pub const GIT_STAGE: &str = "ステージに追加";

    // -- ターミナル (views/terminal.rs) --
    pub const TERMINAL_ADD: &str = "新しいターミナル";
    pub const TERMINAL_CLOSE_TAB: &str = "ターミナルを閉じる";

    // -- エディタ (views/editor.rs) --
    pub const EDITOR_CLOSE_TAB: &str = "タブを閉じる (⌘W)";
    pub const EDITOR_TOGGLE_PREVIEW: &str = "Markdown プレビューの表示切り替え (⌘⇧V)";
    pub const EDITOR_SPLIT_RIGHT: &str = "右に分割 (⌘\\)";

    /// 一覧チェック用。文言を増やしたときはここにも必ず足すこと。
    ///
    /// テストでしか参照しないので `#[cfg(test)]` で括る。無くすと通常ビルドで
    /// 「参照されていない」という dead_code 警告が新規に出てしまう。
    #[cfg(test)]
    pub const ALL: &[&str] = &[
        ADD_WORKSPACE,
        SHOW_EXPLORER,
        SHOW_SEARCH,
        SHOW_GIT,
        SHOW_CODEX,
        SETTINGS,
        CLOSE_PANEL,
        EXPLORER_NEW_FILE,
        EXPLORER_NEW_FOLDER,
        EXPLORER_RELOAD,
        EXPLORER_TOGGLE_HIDDEN,
        SEARCH_REFRESH,
        SEARCH_TOGGLE_REPLACE,
        SEARCH_CASE_SENSITIVE,
        SEARCH_WHOLE_WORD,
        SEARCH_REGEX,
        GIT_SWITCH_BRANCH,
        GIT_PULL,
        GIT_PUSH,
        GIT_DISCARD,
        GIT_UNSTAGE,
        GIT_STAGE,
        TERMINAL_ADD,
        TERMINAL_CLOSE_TAB,
        EDITOR_CLOSE_TAB,
        EDITOR_TOGGLE_PREVIEW,
        EDITOR_SPLIT_RIGHT,
    ];

    /// [`ALL`] と同じ並びの識別子名。
    ///
    /// 文言そのものではなく識別子名を持つのは、「定数を定義しただけで実際のボタンに
    /// 配線し忘れる」という抜けをテストで捕まえるため。ビューのソースを走査して
    /// `tooltip_text::<名前>` が出てくるかを調べる (下の
    /// `すべてのツールチップ文言が実際のボタンに配線されている` を参照)。
    #[cfg(test)]
    pub const ALL_NAMES: &[&str] = &[
        "ADD_WORKSPACE",
        "SHOW_EXPLORER",
        "SHOW_SEARCH",
        "SHOW_GIT",
        "SHOW_CODEX",
        "SETTINGS",
        "CLOSE_PANEL",
        "EXPLORER_NEW_FILE",
        "EXPLORER_NEW_FOLDER",
        "EXPLORER_RELOAD",
        "EXPLORER_TOGGLE_HIDDEN",
        "SEARCH_REFRESH",
        "SEARCH_TOGGLE_REPLACE",
        "SEARCH_CASE_SENSITIVE",
        "SEARCH_WHOLE_WORD",
        "SEARCH_REGEX",
        "GIT_SWITCH_BRANCH",
        "GIT_PULL",
        "GIT_PUSH",
        "GIT_DISCARD",
        "GIT_UNSTAGE",
        "GIT_STAGE",
        "TERMINAL_ADD",
        "TERMINAL_CLOSE_TAB",
        "EDITOR_CLOSE_TAB",
        "EDITOR_TOGGLE_PREVIEW",
        "EDITOR_SPLIT_RIGHT",
    ];

    /// ツールチップを配線しているビューのソース。
    ///
    /// 配線漏れの検査に使う。gpui の要素をテストから組み立てて調べる手立てが
    /// このリポジトリには無いため、ソースを走査するという素朴な方法を採る。
    #[cfg(test)]
    pub const WIRED_SOURCES: &[(&str, &str)] = &[
        ("app.rs", include_str!("app.rs")),
        ("views/explorer.rs", include_str!("views/explorer.rs")),
        ("views/search.rs", include_str!("views/search.rs")),
        ("views/git.rs", include_str!("views/git.rs")),
        ("views/terminal.rs", include_str!("views/terminal.rs")),
        ("views/editor.rs", include_str!("views/editor.rs")),
    ];

    /// ワークスペースの切り替えボタンだけは押した先の名前を埋め込む動的な文言なので、
    /// 定数ではなく純粋関数として切り出す。
    pub fn workspace_switch(name: &str) -> String {
        format!("{name} に切り替え")
    }
}

/// 空状態の案内。どのパネルでも同じ調子で出す。
pub fn empty_state(message: impl Into<SharedString>, cx: &App) -> impl IntoElement {
    let theme = theme(cx);
    v_flex()
        .size_full()
        .items_center()
        .justify_center()
        .p(px(24.))
        .child(
            div()
                .text_size(px(12.))
                .text_color(theme.text_faint)
                .text_center()
                .child(message.into()),
        )
}

/// 星雲を思わせる背景の淡いグラデーション帯。
///
/// 単色の暗い面が広いと安っぽく見えるため、パネル上端に極薄のアクセントを敷く。
pub fn nebula_accent_line(color: Hsla) -> impl IntoElement {
    div().h(px(1.)).w_full().flex_none().bg(color)
}

/// テキストを 1 行に省略表示する。
pub fn truncate_middle(text: &str, max_chars: usize) -> String {
    let chars: Vec<char> = text.chars().collect();
    if chars.len() <= max_chars || max_chars < 4 {
        return text.to_string();
    }
    let head = (max_chars - 1) / 2;
    let tail = max_chars - 1 - head;
    let mut result: String = chars[..head].iter().collect();
    result.push('…');
    result.extend(&chars[chars.len() - tail..]);
    result
}

/// キーバインドを人が読める形にする (`cmd-shift-p` → `⌘⇧P`)。
pub fn format_keystroke(keystroke: &str) -> String {
    let mut result = String::new();
    let parts: Vec<&str> = keystroke.split('-').collect();
    for (i, part) in parts.iter().enumerate() {
        let is_last = i + 1 == parts.len();
        match *part {
            "cmd" | "super" => result.push('⌘'),
            "ctrl" => result.push('⌃'),
            "alt" | "option" => result.push('⌥'),
            "shift" => result.push('⇧'),
            key if is_last => {
                let display = match key {
                    "enter" => "⏎".to_string(),
                    "escape" => "⎋".to_string(),
                    "backspace" => "⌫".to_string(),
                    "delete" => "⌦".to_string(),
                    "tab" => "⇥".to_string(),
                    "up" => "↑".to_string(),
                    "down" => "↓".to_string(),
                    "left" => "←".to_string(),
                    "right" => "→".to_string(),
                    other => other.to_uppercase(),
                };
                result.push_str(&display);
            }
            other => result.push_str(other),
        }
    }
    result
}

/// ウィンドウがフォーカスを持つかに応じて、境界のネオンを強める。
pub fn focus_border(focused: bool, theme: &Theme) -> Hsla {
    if focused {
        theme.border_glow
    } else {
        theme.border
    }
}

// ===========================================================================
// テキスト入力欄
// ===========================================================================
//
// 構成は 3 層。
//
// 1. 純粋関数と [`InputState`] … gpui に触れない編集モデル。IME の位置計算など
//    間違えやすい部分をここへ寄せ、単体テストで固める。
// 2. [`TextInput`] … 上の状態に focus・キー操作・マウス操作を足したエンティティ。
// 3. [`TextInputElement`] … 実際に字形化して描くカスタム要素。
//
// 位置はすべて **UTF-8 バイト** で持つ。IME が寄こす UTF-16 の位置は境界で直す。

// ---------------------------------------------------------------------------
// 位置の変換 (純粋関数)
// ---------------------------------------------------------------------------

/// UTF-8 バイト位置 → UTF-16 符号単位の位置。
pub fn offset_to_utf16(text: &str, offset: usize) -> usize {
    let mut utf16 = 0;
    let mut utf8 = 0;
    for ch in text.chars() {
        if utf8 >= offset {
            break;
        }
        utf8 += ch.len_utf8();
        utf16 += ch.len_utf16();
    }
    utf16
}

/// UTF-16 符号単位の位置 → UTF-8 バイト位置。
pub fn offset_from_utf16(text: &str, offset: usize) -> usize {
    let mut utf8 = 0;
    let mut utf16 = 0;
    for ch in text.chars() {
        if utf16 >= offset {
            break;
        }
        utf16 += ch.len_utf16();
        utf8 += ch.len_utf8();
    }
    utf8
}

fn range_to_utf16(text: &str, range: &Range<usize>) -> Range<usize> {
    offset_to_utf16(text, range.start)..offset_to_utf16(text, range.end)
}

fn range_from_utf16(text: &str, range: &Range<usize>) -> Range<usize> {
    offset_from_utf16(text, range.start)..offset_from_utf16(text, range.end)
}

/// 1 つ手前の書記素境界。合成文字や絵文字の途中で切らないため char ではなく
/// 書記素で刻む。
pub fn previous_grapheme(text: &str, offset: usize) -> usize {
    text.grapheme_indices(true)
        .rev()
        .find_map(|(index, _)| (index < offset).then_some(index))
        .unwrap_or(0)
}

/// 1 つ先の書記素境界。
pub fn next_grapheme(text: &str, offset: usize) -> usize {
    text.grapheme_indices(true)
        .find_map(|(index, _)| (index > offset).then_some(index))
        .unwrap_or(text.len())
}

/// 単語移動のための文字の種類。
///
/// 「空白」「語」「記号」の 3 種に分けるのは、`foo.bar` の `.` で止まってほしい
/// 一方、`.....` の途中では止まってほしくないため。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CharClass {
    Space,
    Word,
    Symbol,
}

fn char_class(ch: char) -> CharClass {
    if ch.is_whitespace() {
        CharClass::Space
    } else if ch.is_alphanumeric() || ch == '_' {
        CharClass::Word
    } else {
        CharClass::Symbol
    }
}

/// 単語 1 つぶん手前の位置。空白を飛ばしてから、同じ種類が続くあいだ戻る。
pub fn previous_word_boundary(text: &str, offset: usize) -> usize {
    let offset = offset.min(text.len());
    let head = &text[..offset];
    let mut chars = head.char_indices().rev().peekable();
    let mut result = offset;
    while let Some(&(index, ch)) = chars.peek() {
        if char_class(ch) != CharClass::Space {
            break;
        }
        result = index;
        chars.next();
    }
    let Some(&(_, first)) = chars.peek() else {
        return result;
    };
    let class = char_class(first);
    while let Some(&(index, ch)) = chars.peek() {
        if char_class(ch) != class {
            break;
        }
        result = index;
        chars.next();
    }
    result
}

/// 単語 1 つぶん先の位置。同じ種類が続くあいだ進み、そのあとの空白も飛ばす。
pub fn next_word_boundary(text: &str, offset: usize) -> usize {
    let offset = offset.min(text.len());
    let mut chars = text[offset..].char_indices().peekable();
    let mut result = offset;
    if let Some(&(_, first)) = chars.peek()
        && char_class(first) != CharClass::Space
    {
        let class = char_class(first);
        while let Some(&(index, ch)) = chars.peek() {
            if char_class(ch) != class {
                break;
            }
            result = offset + index + ch.len_utf8();
            chars.next();
        }
    }
    while let Some(&(index, ch)) = chars.peek() {
        if char_class(ch) != CharClass::Space {
            break;
        }
        result = offset + index + ch.len_utf8();
        chars.next();
    }
    result
}

/// 位置が属する行の範囲 (改行を含まない)。
pub fn line_bounds(text: &str, offset: usize) -> Range<usize> {
    let offset = offset.min(text.len());
    let start = text[..offset].rfind('\n').map(|i| i + 1).unwrap_or(0);
    let end = text[offset..]
        .find('\n')
        .map(|i| offset + i)
        .unwrap_or(text.len());
    start..end
}

// ---------------------------------------------------------------------------
// 編集モデル (純粋)
// ---------------------------------------------------------------------------

/// 入力欄の中身と選択範囲。gpui に触れないので単体テストで検査できる。
///
/// IME の経路 ([`InputState::replace`] / [`InputState::replace_and_mark`]) は
/// 添字の足し先を 1 つ間違えるだけで範囲外パニックまで行くので、必ずここを通す。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct InputState {
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
        self.text
            .replace_range(range.clone(), new_text);
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
            KeyBinding::new("alt-backspace", InputDeleteWordLeft, ctx),
            KeyBinding::new("left", InputLeft, ctx),
            KeyBinding::new("right", InputRight, ctx),
            KeyBinding::new("shift-left", InputSelectLeft, ctx),
            KeyBinding::new("shift-right", InputSelectRight, ctx),
            KeyBinding::new("alt-left", InputWordLeft, ctx),
            KeyBinding::new("alt-right", InputWordRight, ctx),
            KeyBinding::new("alt-shift-left", InputSelectWordLeft, ctx),
            KeyBinding::new("alt-shift-right", InputSelectWordRight, ctx),
            KeyBinding::new("up", InputUp, ctx),
            KeyBinding::new("down", InputDown, ctx),
            KeyBinding::new("home", InputLineStart, ctx),
            KeyBinding::new("end", InputLineEnd, ctx),
            KeyBinding::new("cmd-left", InputLineStart, ctx),
            KeyBinding::new("cmd-right", InputLineEnd, ctx),
            KeyBinding::new("shift-home", InputSelectLineStart, ctx),
            KeyBinding::new("shift-end", InputSelectLineEnd, ctx),
            KeyBinding::new("cmd-a", InputSelectAll, ctx),
            KeyBinding::new("cmd-c", InputCopy, ctx),
            KeyBinding::new("cmd-x", InputCut, ctx),
            KeyBinding::new("cmd-v", InputPaste, ctx),
            KeyBinding::new("enter", InputEnter, ctx),
            KeyBinding::new("shift-enter", InputNewline, ctx),
            KeyBinding::new("alt-enter", InputNewline, ctx),
            KeyBinding::new("cmd-enter", InputSubmit, ctx),
            KeyBinding::new("escape", InputEscape, ctx),
            // 一覧を持つ呼び出し側 (パレット) が候補送りに使う。入力欄は
            // 何もせずそのまま親へ流す。
            KeyBinding::new("tab", InputTab, ctx),
            KeyBinding::new("shift-tab", InputBackTab, ctx),
            KeyBinding::new("ctrl-p", InputPrevItem, ctx),
            KeyBinding::new("ctrl-n", InputNextItem, ctx),
        ]);
    });
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
pub fn sanitize_insert(text: &str, multi_line: bool, policy: NewlinePolicy) -> String {
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
        self.move_to(next_word_boundary(&self.state.text, self.state.cursor()), cx);
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
        self.select_to(next_word_boundary(&self.state.text, self.state.cursor()), cx);
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

    fn on_mouse_down(&mut self, event: &MouseDownEvent, window: &mut Window, cx: &mut Context<Self>) {
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
            .child(TextInputElement {
                input: cx.entity(),
            })
    }
}

// ---------------------------------------------------------------------------
// 描画
// ---------------------------------------------------------------------------

/// 直前の描画で確定した行の字形。
struct InputLayout {
    /// 論理行ごとの (字形, 行頭のバイト位置)。
    lines: Vec<(WrappedLine, usize)>,
    line_height: Pixels,
    bounds: Bounds<Pixels>,
    /// 表示の先頭にある折り返し行の番号。
    scroll_rows: usize,
}

impl InputLayout {
    fn rows_of(line: &WrappedLine) -> usize {
        line.wrap_boundaries().len() + 1
    }

    fn scroll_y(&self) -> Pixels {
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
    fn point_for_offset(&self, offset: usize) -> Option<Point<Pixels>> {
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
    fn offset_for_point(&self, position: Point<Pixels>) -> usize {
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
    let Ok(lines) = window
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
fn marked_runs(
    text: &str,
    font: Font,
    color: Hsla,
    marked: Option<&Range<usize>>,
) -> Vec<TextRun> {
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
struct TextInputElement {
    input: Entity<TextInput>,
}

struct TextInputPrepaint {
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
        let layout_id = window.request_measured_layout(style, move |known, available, window, cx| {
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
        let visible_rows = ((f32::from(bounds.size.height) / f32::from(line_height)).floor()
            as usize)
            .max(1);
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

    #[test]
    fn 中略は長さを守る() {
        let result = truncate_middle("abcdefghijklmnop", 9);
        assert_eq!(result.chars().count(), 9);
        assert!(result.contains('…'));
        assert!(result.starts_with("abcd"));
        assert!(result.ends_with("mnop"));
    }

    #[test]
    fn 短い文字列はそのまま() {
        assert_eq!(truncate_middle("short", 20), "short");
    }

    #[test]
    fn キーバインド表記を記号に変換する() {
        assert_eq!(format_keystroke("cmd-shift-p"), "⌘⇧P");
        assert_eq!(format_keystroke("ctrl-`"), "⌃`");
        assert_eq!(format_keystroke("cmd-enter"), "⌘⏎");
    }

    // -----------------------------------------------------------------
    // ツールチップ文言
    // -----------------------------------------------------------------
    //
    // GUI の見た目 (角丸・影・色) は目視でしか確認できないので、ここでは
    // 文言そのものの一覧性 (重複や空文字が無いか) と表記ゆれ (ショートカット
    // 併記の形式) だけを機械的に縛る。

    /// `"… (⌘…)"` の形が守られているかを判定する。
    ///
    /// `⌘` を含まない文言 (「設定 (未実装)」のような注記の丸括弧) は
    /// ショートカット併記ではないので対象外にする。
    fn shortcut_suffix_is_well_formed(text: &str) -> bool {
        if !text.contains('⌘') {
            return true;
        }
        let Some(start) = text.rfind(" (⌘") else {
            return false;
        };
        // "(⌘" の直後から末尾の ")" の手前まで、丸括弧が紛れていないこと。
        let inner = &text[start + " (".len()..text.len() - 1];
        text.ends_with(')') && !inner.contains(['(', ')'])
    }

    #[test]
    fn ショートカット併記の形式を判定できる() {
        assert!(shortcut_suffix_is_well_formed("ワークスペースを追加 (⌘O)"));
        // ⌘ を含まない注記は対象外なので、丸括弧があっても崩れているとは判定しない。
        assert!(shortcut_suffix_is_well_formed("設定 (未実装)"));
        assert!(!shortcut_suffix_is_well_formed("ワークスペースを追加(⌘O)"));
        assert!(!shortcut_suffix_is_well_formed("ワークスペースを追加 (⌘O"));
    }

    /// 定数を定義しただけで実際のボタンに `.tooltip(...)` を付け忘れる、という抜けを捕まえる。
    ///
    /// gpui の要素をテストから組み立てて調べる手立てがこのリポジトリには無いので、
    /// ツールチップを配線しているビューのソースを走査して `tooltip_text::<名前>` が
    /// 現れるかを見る。素朴だが、配線を消すとこのテストが赤くなる。
    #[test]
    fn すべてのツールチップ文言が実際のボタンに配線されている() {
        for &name in tooltip_text::ALL_NAMES {
            let needle = format!("tooltip_text::{name}");
            let wired = tooltip_text::WIRED_SOURCES
                .iter()
                .any(|(_, source)| source.contains(&needle));
            assert!(wired, "{name} がどのビューのボタンにも配線されていない");
        }
    }

    /// 文言の一覧と識別子名の一覧がずれていると、上の配線チェックが素通りする。
    #[test]
    fn ツールチップの文言一覧と識別子名一覧は同じ数だけある() {
        assert_eq!(tooltip_text::ALL.len(), tooltip_text::ALL_NAMES.len());
    }

    #[test]
    fn すべてのツールチップ文言は空でない() {
        for &text in tooltip_text::ALL {
            assert!(!text.is_empty());
        }
    }

    #[test]
    fn ツールチップ文言に重複が無い() {
        use std::collections::HashSet;

        let mut seen = HashSet::new();
        for &text in tooltip_text::ALL {
            assert!(seen.insert(text), "重複した文言: {text}");
        }
    }

    #[test]
    fn ツールチップのショートカット併記は形式が揃っている() {
        for &text in tooltip_text::ALL {
            assert!(
                shortcut_suffix_is_well_formed(text),
                "ショートカット併記の形式が崩れている: {text}"
            );
        }
    }

    #[test]
    fn ワークスペース切り替えの文言に名前が含まれる() {
        assert_eq!(tooltip_text::workspace_switch("Nebula"), "Nebula に切り替え");
    }

    // -----------------------------------------------------------------
    // 入力欄: UTF-16 との相互変換
    // -----------------------------------------------------------------

    #[test]
    fn ascii_では_utf8_と_utf16_の位置が一致する() {
        assert_eq!(offset_to_utf16("hello", 3), 3);
        assert_eq!(offset_from_utf16("hello", 3), 3);
    }

    #[test]
    fn 日本語は_1_文字が_3_バイト_1_符号単位() {
        let text = "あいう";
        assert_eq!(offset_to_utf16(text, 3), 1);
        assert_eq!(offset_to_utf16(text, 9), 3);
        assert_eq!(offset_from_utf16(text, 1), 3);
        assert_eq!(offset_from_utf16(text, 3), 9);
    }

    #[test]
    fn 代理対の絵文字は_4_バイト_2_符号単位() {
        let text = "日本語🎌";
        // 絵文字の手前まで。
        assert_eq!(offset_to_utf16(text, 9), 3);
        // 絵文字を含めて。
        assert_eq!(offset_to_utf16(text, 13), 5);
        assert_eq!(offset_from_utf16(text, 5), 13);
    }

    #[test]
    fn 位置の往復で元に戻る() {
        let text = "a あ b 🎌 c";
        for (index, _) in text.char_indices() {
            let utf16 = offset_to_utf16(text, index);
            assert_eq!(offset_from_utf16(text, utf16), index, "位置 {index}");
        }
    }

    // -----------------------------------------------------------------
    // 入力欄: 書記素と単語の境界
    // -----------------------------------------------------------------

    #[test]
    fn 書記素境界は多バイト文字を割らない() {
        let text = "あい";
        assert_eq!(previous_grapheme(text, 6), 3);
        assert_eq!(previous_grapheme(text, 3), 0);
        assert_eq!(previous_grapheme(text, 0), 0);
        assert_eq!(next_grapheme(text, 0), 3);
        assert_eq!(next_grapheme(text, 3), 6);
        assert_eq!(next_grapheme(text, 6), 6);
    }

    #[test]
    fn 単語移動は語の頭で止まる() {
        let text = "foo bar baz";
        assert_eq!(previous_word_boundary(text, 11), 8);
        assert_eq!(previous_word_boundary(text, 8), 4);
        assert_eq!(previous_word_boundary(text, 4), 0);
        assert_eq!(previous_word_boundary(text, 0), 0);
    }

    #[test]
    fn 単語移動は前へ進むと語の末尾と続く空白を越える() {
        let text = "foo bar baz";
        assert_eq!(next_word_boundary(text, 0), 4);
        assert_eq!(next_word_boundary(text, 4), 8);
        assert_eq!(next_word_boundary(text, 8), 11);
        assert_eq!(next_word_boundary(text, 11), 11);
    }

    #[test]
    fn 記号は語とは別の塊として扱う() {
        let text = "foo.bar";
        // 末尾から戻ると bar・記号・foo の順で刻む。
        assert_eq!(previous_word_boundary(text, 7), 4);
        assert_eq!(previous_word_boundary(text, 4), 3);
        assert_eq!(previous_word_boundary(text, 3), 0);
        assert_eq!(next_word_boundary(text, 0), 3);
        assert_eq!(next_word_boundary(text, 3), 4);
    }

    #[test]
    fn 連続した記号はひとまとまりで越える() {
        let text = "a==b";
        assert_eq!(next_word_boundary(text, 1), 3);
        assert_eq!(previous_word_boundary(text, 3), 1);
    }

    #[test]
    fn 単語移動は多バイト文字の境界で止まる() {
        let text = "あいう えお";
        assert_eq!(next_word_boundary(text, 0), 10);
        assert_eq!(previous_word_boundary(text, text.len()), 10);
    }

    // -----------------------------------------------------------------
    // 入力欄: 行の範囲
    // -----------------------------------------------------------------

    #[test]
    fn 行の範囲は前後の改行の内側() {
        let text = "ab\ncde\nf";
        assert_eq!(line_bounds(text, 0), 0..2);
        assert_eq!(line_bounds(text, 2), 0..2);
        assert_eq!(line_bounds(text, 3), 3..6);
        assert_eq!(line_bounds(text, 5), 3..6);
        assert_eq!(line_bounds(text, 7), 7..8);
    }

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
        assert_eq!(
            sanitize_insert("a\nb", true, NewlinePolicy::Strip),
            "a\nb"
        );
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
