//! キーバインドに割り当てるアクション。
//!
//! 名前は VS Code の対応する操作に寄せてある。移行時に既存の指の記憶が使えることを優先した。

use gpui::{App, KeyBinding, actions};

actions!(
    nebula,
    [
        /// アプリを終了する。
        Quit,
        /// コマンドパレットを開く。
        ToggleCommandPalette,
        /// ファイルのクイックオープン。
        ToggleFileFinder,
        /// サイドバーの表示切り替え。
        ToggleSidebar,
        /// 下部パネルの表示切り替え。
        TogglePanel,
        /// ターミナルパネルを開く。
        ToggleTerminal,
        /// エクスプローラーを表示する。
        ShowExplorer,
        /// 検索ビューを表示する。
        ShowSearch,
        /// Git ビューを表示する。
        ShowGit,
        /// Codex ビューを表示する。
        ShowCodex,
        /// フォルダを開く。
        OpenFolder,
        /// アクティブなタブを閉じる。
        CloseTab,
        /// 次のタブへ。
        NextTab,
        /// 前のタブへ。
        PreviousTab,
        /// 保存。
        Save,
        /// 名前を付けて保存。
        SaveAs,
        /// 新規ファイル。
        NewFile,
        /// 次のワークスペースへ切り替える。
        NextWorkspace,
        /// エディタを縦に分割する。
        SplitRight,
        /// Markdown プレビューの表示切り替え。
        TogglePreview,
    ]
);

actions!(
    editor,
    [
        Undo,
        Redo,
        Cut,
        Copy,
        Paste,
        SelectAll,
        MoveLeft,
        MoveRight,
        MoveUp,
        MoveDown,
        MoveWordLeft,
        MoveWordRight,
        MoveLineStart,
        MoveLineEnd,
        MoveDocumentStart,
        MoveDocumentEnd,
        MovePageUp,
        MovePageDown,
        SelectLeft,
        SelectRight,
        SelectUp,
        SelectDown,
        SelectWordLeft,
        SelectWordRight,
        SelectLineStart,
        SelectLineEnd,
        Backspace,
        Delete,
        DeleteWordLeft,
        Newline,
        Indent,
        Outdent,
        ToggleComment,
        DuplicateLine,
        DeleteLine,
        MoveLineUp,
        MoveLineDown,
        AddCursorAbove,
        AddCursorBelow,
        SelectNextOccurrence,
        GoToDefinition,
        FindReferences,
        ShowHover,
        TriggerCompletion,
        AcceptCompletion,
        DismissCompletion,
        NextCompletion,
        PreviousCompletion,
        Format,
        Rename,
        GoToLine,
        FindInFile,
    ]
);

/// 表示にも使う打鍵の定義。
///
/// コマンドパレットのキーバッジもツールチップの併記も、実際の登録と同じ文字列を
/// ここから読む。以前はキー文字列が actions.rs・palette.rs・ui.rs の 3 か所に
/// 別々に書かれており、片方だけ直しても誰も気づけなかった。
///
/// `secondary-` は gpui が解決する修飾キーで、macOS では ⌘、それ以外では Ctrl に
/// なる。**`cmd-` と書いてはいけない**。gpui の `platform` 修飾は Windows では
/// Windows キーを指すので、`cmd-s` は Windows で Win+S に割り当たってしまう。
pub mod keys {
    pub const QUIT: &str = "secondary-q";
    pub const TOGGLE_COMMAND_PALETTE: &str = "secondary-shift-p";
    pub const TOGGLE_FILE_FINDER: &str = "secondary-p";
    pub const TOGGLE_SIDEBAR: &str = "secondary-b";
    pub const TOGGLE_PANEL: &str = "secondary-j";
    pub const SHOW_EXPLORER: &str = "secondary-shift-e";
    pub const SHOW_SEARCH: &str = "secondary-shift-f";
    pub const SHOW_GIT: &str = "secondary-shift-g";
    pub const SHOW_CODEX: &str = "secondary-shift-a";
    pub const OPEN_FOLDER: &str = "secondary-o";
    pub const CLOSE_TAB: &str = "secondary-w";
    pub const SAVE: &str = "secondary-s";
    pub const SAVE_AS: &str = "secondary-shift-s";
    pub const NEW_FILE: &str = "secondary-n";
    pub const NEXT_WORKSPACE: &str = "secondary-shift-]";
    pub const SPLIT_RIGHT: &str = "secondary-\\";
    pub const TOGGLE_PREVIEW: &str = "secondary-shift-v";

    // 以下は全プラットフォームで Ctrl 固定。端末の開閉とタブ送りは、macOS でも
    // ⌘ ではなく Ctrl を使う慣習に合わせている。
    pub const TOGGLE_TERMINAL: &str = "ctrl-`";
    pub const NEXT_TAB: &str = "ctrl-tab";
    pub const PREVIOUS_TAB: &str = "ctrl-shift-tab";

    /// 衝突検査用の一覧。打鍵を増やしたらここにも必ず足すこと
    /// (`アプリ側のバインドはすべて keys に載っている` が個数で見張っている)。
    ///
    /// テストでしか参照しないので `#[cfg(test)]` で括る。無くすと通常ビルドで
    /// 「参照されていない」という dead_code 警告が新規に出てしまう。
    #[cfg(test)]
    pub const ALL: &[&str] = &[
        QUIT,
        TOGGLE_COMMAND_PALETTE,
        TOGGLE_FILE_FINDER,
        TOGGLE_SIDEBAR,
        TOGGLE_PANEL,
        SHOW_EXPLORER,
        SHOW_SEARCH,
        SHOW_GIT,
        SHOW_CODEX,
        OPEN_FOLDER,
        CLOSE_TAB,
        SAVE,
        SAVE_AS,
        NEW_FILE,
        NEXT_WORKSPACE,
        SPLIT_RIGHT,
        TOGGLE_PREVIEW,
        TOGGLE_TERMINAL,
        NEXT_TAB,
        PREVIOUS_TAB,
    ];
}

/// 既定のキーバインドを登録する。
pub fn init(cx: &mut App) {
    cx.bind_keys(app_bindings());
    cx.bind_keys(editor_bindings());
    cx.bind_keys(editor_navigation_bindings());
}

fn app_bindings() -> Vec<KeyBinding> {
    vec![
        KeyBinding::new(keys::QUIT, Quit, None),
        KeyBinding::new(keys::TOGGLE_COMMAND_PALETTE, ToggleCommandPalette, None),
        KeyBinding::new(keys::TOGGLE_FILE_FINDER, ToggleFileFinder, None),
        KeyBinding::new(keys::TOGGLE_SIDEBAR, ToggleSidebar, None),
        KeyBinding::new(keys::TOGGLE_PANEL, TogglePanel, None),
        KeyBinding::new(keys::TOGGLE_TERMINAL, ToggleTerminal, None),
        KeyBinding::new(keys::SHOW_EXPLORER, ShowExplorer, None),
        KeyBinding::new(keys::SHOW_SEARCH, ShowSearch, None),
        KeyBinding::new(keys::SHOW_GIT, ShowGit, None),
        KeyBinding::new(keys::SHOW_CODEX, ShowCodex, None),
        KeyBinding::new(keys::OPEN_FOLDER, OpenFolder, None),
        KeyBinding::new(keys::CLOSE_TAB, CloseTab, None),
        KeyBinding::new(keys::NEXT_TAB, NextTab, None),
        KeyBinding::new(keys::PREVIOUS_TAB, PreviousTab, None),
        KeyBinding::new(keys::SAVE, Save, None),
        KeyBinding::new(keys::SAVE_AS, SaveAs, None),
        KeyBinding::new(keys::NEW_FILE, NewFile, None),
        KeyBinding::new(keys::NEXT_WORKSPACE, NextWorkspace, None),
        KeyBinding::new(keys::SPLIT_RIGHT, SplitRight, None),
        KeyBinding::new(keys::TOGGLE_PREVIEW, TogglePreview, None),
    ]
}

/// エディタ内でのみ有効なバインドのうち、修飾キーの慣習がプラットフォームで
/// 変わらないもの。
///
/// 文脈を `Editor` に限定しないと、検索欄などの入力中に矢印キーが
/// カーソル移動へ吸われてしまう。
fn editor_bindings() -> Vec<KeyBinding> {
    let ctx = Some("Editor");
    vec![
        KeyBinding::new("secondary-z", Undo, ctx),
        KeyBinding::new("secondary-shift-z", Redo, ctx),
        KeyBinding::new("secondary-x", Cut, ctx),
        KeyBinding::new("secondary-c", Copy, ctx),
        KeyBinding::new("secondary-v", Paste, ctx),
        KeyBinding::new("secondary-a", SelectAll, ctx),
        KeyBinding::new("left", MoveLeft, ctx),
        KeyBinding::new("right", MoveRight, ctx),
        KeyBinding::new("up", MoveUp, ctx),
        KeyBinding::new("down", MoveDown, ctx),
        KeyBinding::new("home", MoveLineStart, ctx),
        KeyBinding::new("end", MoveLineEnd, ctx),
        KeyBinding::new("pageup", MovePageUp, ctx),
        KeyBinding::new("pagedown", MovePageDown, ctx),
        KeyBinding::new("shift-left", SelectLeft, ctx),
        KeyBinding::new("shift-right", SelectRight, ctx),
        KeyBinding::new("shift-up", SelectUp, ctx),
        KeyBinding::new("shift-down", SelectDown, ctx),
        KeyBinding::new("backspace", Backspace, ctx),
        KeyBinding::new("delete", Delete, ctx),
        KeyBinding::new("enter", Newline, ctx),
        KeyBinding::new("tab", Indent, ctx),
        KeyBinding::new("shift-tab", Outdent, ctx),
        KeyBinding::new("secondary-/", ToggleComment, ctx),
        KeyBinding::new("secondary-shift-d", DuplicateLine, ctx),
        KeyBinding::new("secondary-shift-k", DeleteLine, ctx),
        KeyBinding::new("alt-up", MoveLineUp, ctx),
        KeyBinding::new("alt-down", MoveLineDown, ctx),
        KeyBinding::new("secondary-alt-up", AddCursorAbove, ctx),
        KeyBinding::new("secondary-alt-down", AddCursorBelow, ctx),
        KeyBinding::new("secondary-d", SelectNextOccurrence, ctx),
        KeyBinding::new("f12", GoToDefinition, ctx),
        KeyBinding::new("secondary-shift-f12", FindReferences, ctx),
        KeyBinding::new("secondary-k secondary-i", ShowHover, ctx),
        KeyBinding::new("ctrl-space", TriggerCompletion, ctx),
        KeyBinding::new("escape", DismissCompletion, ctx),
        KeyBinding::new("alt-shift-f", Format, ctx),
        KeyBinding::new("f2", Rename, ctx),
        KeyBinding::new("ctrl-g", GoToLine, ctx),
        KeyBinding::new("secondary-f", FindInFile, ctx),
    ]
}

/// 単語・行・文書の端へ動く操作。
///
/// ここだけは `secondary-` への一括置換では正しくならない。修飾キーの割り当てが
/// プラットフォームで根本的に違うため。macOS は「⌘+← が行頭、⌥+← が単語」、
/// Windows/Linux は「Home が行頭、Ctrl+← が単語」。`cmd-left` を機械的に
/// `secondary-left` にすると、Windows で Ctrl+← が単語移動ではなく行頭移動に
/// なってしまい、その OS の利用者にとって明確に誤った割り当てになる。
#[cfg(target_os = "macos")]
fn editor_navigation_bindings() -> Vec<KeyBinding> {
    let ctx = Some("Editor");
    vec![
        KeyBinding::new("alt-left", MoveWordLeft, ctx),
        KeyBinding::new("alt-right", MoveWordRight, ctx),
        KeyBinding::new("cmd-left", MoveLineStart, ctx),
        KeyBinding::new("cmd-right", MoveLineEnd, ctx),
        KeyBinding::new("cmd-up", MoveDocumentStart, ctx),
        KeyBinding::new("cmd-down", MoveDocumentEnd, ctx),
        KeyBinding::new("alt-shift-left", SelectWordLeft, ctx),
        KeyBinding::new("alt-shift-right", SelectWordRight, ctx),
        KeyBinding::new("cmd-shift-left", SelectLineStart, ctx),
        KeyBinding::new("cmd-shift-right", SelectLineEnd, ctx),
        KeyBinding::new("alt-backspace", DeleteWordLeft, ctx),
    ]
}

#[cfg(not(target_os = "macos"))]
fn editor_navigation_bindings() -> Vec<KeyBinding> {
    let ctx = Some("Editor");
    vec![
        KeyBinding::new("ctrl-left", MoveWordLeft, ctx),
        KeyBinding::new("ctrl-right", MoveWordRight, ctx),
        KeyBinding::new("ctrl-home", MoveDocumentStart, ctx),
        KeyBinding::new("ctrl-end", MoveDocumentEnd, ctx),
        KeyBinding::new("ctrl-shift-left", SelectWordLeft, ctx),
        KeyBinding::new("ctrl-shift-right", SelectWordRight, ctx),
        KeyBinding::new("shift-home", SelectLineStart, ctx),
        KeyBinding::new("shift-end", SelectLineEnd, ctx),
        KeyBinding::new("ctrl-backspace", DeleteWordLeft, ctx),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    /// バインドの打鍵を、比較しやすい 1 本の文字列にする。
    fn keystrokes(bindings: &[KeyBinding]) -> Vec<String> {
        bindings
            .iter()
            .map(|b| {
                b.keystrokes()
                    .iter()
                    .map(|k| k.unparse())
                    .collect::<Vec<_>>()
                    .join(" ")
            })
            .collect()
    }

    fn assert_no_duplicates(mut keys: Vec<String>, what: &str) {
        let total = keys.len();
        keys.sort();
        keys.dedup();
        assert_eq!(
            keys.len(),
            total,
            "{what}: 同じ打鍵に 2 つの操作が割り当てられている"
        );
    }

    #[test]
    fn アプリ側のバインドが重複していない() {
        assert_no_duplicates(keystrokes(&app_bindings()), "アプリ全体");
    }

    /// エディタは共通ぶんと移動ぶんを別々に登録するので、合わせて見る。
    /// 片方だけを見ていると、移動ぶんが共通ぶんと同じ打鍵を奪っても気づけない。
    #[test]
    fn エディタ側のバインドが重複していない() {
        let mut bindings = editor_bindings();
        bindings.extend(editor_navigation_bindings());
        assert_no_duplicates(keystrokes(&bindings), "エディタ");
    }

    /// 表示に使う `keys` の一覧が、実際の登録と 1 対 1 で対応していること。
    ///
    /// `keys::ALL` は下の衝突検査の入力なので、ここが抜けると検査対象から
    /// こぼれた打鍵が黙って衝突する。
    #[test]
    fn アプリ側のバインドはすべて_keys_に載っている() {
        assert_eq!(
            app_bindings().len(),
            keys::ALL.len(),
            "keys::ALL と app_bindings() の数が違う。打鍵を足したら両方に足すこと"
        );
    }

    /// `secondary-` は macOS では ⌘、それ以外では Ctrl に解決される。
    ///
    /// つまり `secondary-x` と `ctrl-x` は Windows/Linux では同じ打鍵になる。
    /// そうなると片方が効かなくなるのに、macOS で開発しているあいだは
    /// `⌘X` と `⌃X` に分かれているので永久に気づけない。実際に
    /// そのプラットフォームで起きる解決結果へ直してから突き合わせる。
    #[test]
    fn windows_と_linux_でもアプリ側のキーが重複しない() {
        let resolved: Vec<String> = keys::ALL
            .iter()
            .map(|key| key.replace("secondary-", "ctrl-"))
            .collect();
        assert_no_duplicates(resolved, "アプリ全体 (Windows/Linux の解決結果)");
    }

    /// この設計は「gpui が `secondary-` を macOS では ⌘、それ以外では Ctrl へ
    /// 解決する」ことに全面的に乗っている。その解釈が変わると、Windows で
    /// `cmd-s` が Win+S に割り当たっていた頃の不具合が黙って戻る。
    /// 前提そのものをここで固定する。
    #[test]
    fn secondary_はプラットフォームごとの修飾キーへ解決される() {
        let binding = KeyBinding::new(keys::OPEN_FOLDER, OpenFolder, None);
        let modifiers = binding.keystrokes()[0].inner().modifiers;
        if cfg!(target_os = "macos") {
            assert!(modifiers.platform, "macOS では ⌘ に解決されるはず");
            assert!(!modifiers.control);
        } else {
            assert!(modifiers.control, "macOS 以外では Ctrl に解決されるはず");
            assert!(
                !modifiers.platform,
                "Windows の platform 修飾は Win キー。ここが立つと押せない打鍵になる"
            );
        }
    }

    /// `cmd-` 直書きは Windows では Windows キーに割り当たってしまう
    /// (gpui の `platform` 修飾が Win キーを指すため)。`secondary-` を使うこと。
    #[test]
    fn 打鍵に_cmd_を直書きしていない() {
        for key in keys::ALL {
            assert!(
                !key.contains("cmd-"),
                "{key}: cmd- ではなく secondary- を使うこと"
            );
        }
    }
}
