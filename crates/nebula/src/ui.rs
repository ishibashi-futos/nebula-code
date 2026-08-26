//! 画面をまたいで使う小さな部品。
//!
//! 各ビューが独自にボタンを組み立てると余白や配色がずれるので、
//! 見た目を決める要素はここに集約する。
//!
//! テキスト入力欄 ([`input::TextInput`]) は [`input`] サブモジュールに置く。
//! gpui には編集できる欄が無いため自前で組む必要があり、以前は 6 つのビューが
//! それぞれ写経して持っていた。写し間違いから IME 経路のバグが実際に生まれた
//! ので、1 つにまとめてある。純粋なテキスト処理は [`text`] へ、字形化して描く
//! 部分は `input::element` へ、それぞれ責務ごとに分けてある。

mod input;
mod text;

pub use input::*;
pub use text::{format_keystroke, truncate_middle};

use crate::assets::Icon;
use crate::theme::{Theme, theme};
use gpui::prelude::*;
use gpui::{
    AnyView, App, Context, Div, ElementId, Hsla, IntoElement, Pixels, SharedString, Stateful,
    Window, div, px, svg,
};

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
    move |_window, cx| cx.new(|_cx| SimpleTooltip { text: text.clone() }).into()
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
    use crate::actions::keys;

    /// ショートカットを併記した文言を作る。
    ///
    /// 打鍵は `actions::keys` から取り、表記は `format_keystroke` に任せる。
    /// 以前はここに `⌘O` と直書きしていたので、(1) actions.rs 側で打鍵を変えても
    /// 文言が古いまま残り、(2) Windows では存在しない打鍵を案内していた。
    fn with_shortcut(label: &str, key: &str) -> String {
        format!("{label} ({})", super::format_keystroke(key))
    }

    // -- アプリ全体 (app.rs) --
    pub fn add_workspace() -> String {
        with_shortcut("ワークスペースを追加", keys::OPEN_FOLDER)
    }
    pub fn show_explorer() -> String {
        with_shortcut("エクスプローラー", keys::SHOW_EXPLORER)
    }
    pub fn show_search() -> String {
        with_shortcut("検索", keys::SHOW_SEARCH)
    }
    pub fn show_git() -> String {
        with_shortcut("ソース管理", keys::SHOW_GIT)
    }
    pub fn show_codex() -> String {
        with_shortcut("Codex", keys::SHOW_CODEX)
    }
    pub const SETTINGS: &str = "設定 (未実装)";
    // パネルの表示切り替えの打鍵は「閉じる」専用ではないので、このボタンには
    // 併記しない。閉じるだけの鍵は無い。
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
    pub fn editor_close_tab() -> String {
        with_shortcut("タブを閉じる", keys::CLOSE_TAB)
    }
    pub fn editor_toggle_preview() -> String {
        with_shortcut("Markdown プレビューの表示切り替え", keys::TOGGLE_PREVIEW)
    }
    pub fn editor_split_right() -> String {
        with_shortcut("右に分割", keys::SPLIT_RIGHT)
    }

    /// 一覧チェック用。文言を増やしたときはここにも必ず足すこと。
    ///
    /// ショートカットを併記するものは表記がプラットフォームで変わるため定数に
    /// できず、関数として持っている。一覧はその両方を平らに並べる。
    ///
    /// テストでしか参照しないので `#[cfg(test)]` で括る。無くすと通常ビルドで
    /// 「参照されていない」という dead_code 警告が新規に出てしまう。
    #[cfg(test)]
    pub fn all() -> Vec<String> {
        let mut all: Vec<String> = vec![
            add_workspace(),
            show_explorer(),
            show_search(),
            show_git(),
            show_codex(),
            editor_close_tab(),
            editor_toggle_preview(),
            editor_split_right(),
        ];
        all.extend(
            [
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
            ]
            .iter()
            .map(|s| s.to_string()),
        );
        all
    }

    /// [`ALL`] と同じ並びの識別子名。
    ///
    /// 文言そのものではなく識別子名を持つのは、「定数を定義しただけで実際のボタンに
    /// 配線し忘れる」という抜けをテストで捕まえるため。ビューのソースを走査して
    /// `tooltip_text::<名前>` が出てくるかを調べる (下の
    /// `すべてのツールチップ文言が実際のボタンに配線されている` を参照)。
    #[cfg(test)]
    pub const ALL_NAMES: &[&str] = &[
        "add_workspace",
        "show_explorer",
        "show_search",
        "show_git",
        "show_codex",
        "editor_close_tab",
        "editor_toggle_preview",
        "editor_split_right",
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

/// ウィンドウがフォーカスを持つかに応じて、境界のネオンを強める。
pub fn focus_border(focused: bool, theme: &Theme) -> Hsla {
    if focused {
        theme.border_glow
    } else {
        theme.border
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // -----------------------------------------------------------------
    // ツールチップ文言
    // -----------------------------------------------------------------
    //
    // GUI の見た目 (角丸・影・色) は目視でしか確認できないので、ここでは
    // 文言そのものの一覧性 (重複や空文字が無いか) と表記ゆれ (ショートカット
    // 併記の形式) だけを機械的に縛る。

    /// ショートカット併記は `format_keystroke` の結果をそのまま使う。
    ///
    /// 以前は文言に `⌘O` と直書きされていて、打鍵を変えても文言が古いまま
    /// 残った。今は `actions::keys` から組み立てるので、ここでは
    /// 「組み立て方が変わっていないこと」だけを見れば足りる。
    #[test]
    fn ショートカット併記は打鍵の表記から作られる() {
        use crate::actions::keys;
        assert_eq!(
            tooltip_text::add_workspace(),
            format!(
                "ワークスペースを追加 ({})",
                format_keystroke(keys::OPEN_FOLDER)
            )
        );
        assert_eq!(
            tooltip_text::editor_split_right(),
            format!("右に分割 ({})", format_keystroke(keys::SPLIT_RIGHT))
        );
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
        assert_eq!(tooltip_text::all().len(), tooltip_text::ALL_NAMES.len());
    }

    #[test]
    fn すべてのツールチップ文言は空でない() {
        for text in tooltip_text::all() {
            assert!(!text.is_empty());
        }
    }

    #[test]
    fn ツールチップ文言に重複が無い() {
        use std::collections::HashSet;

        let mut seen = HashSet::new();
        for text in tooltip_text::all() {
            assert!(seen.insert(text.clone()), "重複した文言: {text}");
        }
    }

    #[test]
    fn ワークスペース切り替えの文言に名前が含まれる() {
        assert_eq!(
            tooltip_text::workspace_switch("Nebula"),
            "Nebula に切り替え"
        );
    }
}
