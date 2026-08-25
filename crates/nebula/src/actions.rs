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

/// 既定のキーバインドを登録する。
pub fn init(cx: &mut App) {
    cx.bind_keys(app_bindings());
    cx.bind_keys(editor_bindings());
}

fn app_bindings() -> Vec<KeyBinding> {
    vec![
        KeyBinding::new("cmd-q", Quit, None),
        KeyBinding::new("cmd-shift-p", ToggleCommandPalette, None),
        KeyBinding::new("cmd-p", ToggleFileFinder, None),
        KeyBinding::new("cmd-b", ToggleSidebar, None),
        KeyBinding::new("cmd-j", TogglePanel, None),
        KeyBinding::new("ctrl-`", ToggleTerminal, None),
        KeyBinding::new("cmd-shift-e", ShowExplorer, None),
        KeyBinding::new("cmd-shift-f", ShowSearch, None),
        KeyBinding::new("cmd-shift-g", ShowGit, None),
        KeyBinding::new("cmd-shift-a", ShowCodex, None),
        KeyBinding::new("cmd-o", OpenFolder, None),
        KeyBinding::new("cmd-w", CloseTab, None),
        KeyBinding::new("ctrl-tab", NextTab, None),
        KeyBinding::new("ctrl-shift-tab", PreviousTab, None),
        KeyBinding::new("cmd-s", Save, None),
        KeyBinding::new("cmd-shift-s", SaveAs, None),
        KeyBinding::new("cmd-n", NewFile, None),
        KeyBinding::new("cmd-shift-]", NextWorkspace, None),
        KeyBinding::new("cmd-\\", SplitRight, None),
        KeyBinding::new("cmd-shift-v", TogglePreview, None),
    ]
}

/// エディタ内でのみ有効なバインド。
///
/// 文脈を `Editor` に限定しないと、検索欄などの入力中に矢印キーが
/// カーソル移動へ吸われてしまう。
fn editor_bindings() -> Vec<KeyBinding> {
    let ctx = Some("Editor");
    vec![
        KeyBinding::new("cmd-z", Undo, ctx),
        KeyBinding::new("cmd-shift-z", Redo, ctx),
        KeyBinding::new("cmd-x", Cut, ctx),
        KeyBinding::new("cmd-c", Copy, ctx),
        KeyBinding::new("cmd-v", Paste, ctx),
        KeyBinding::new("cmd-a", SelectAll, ctx),
        KeyBinding::new("left", MoveLeft, ctx),
        KeyBinding::new("right", MoveRight, ctx),
        KeyBinding::new("up", MoveUp, ctx),
        KeyBinding::new("down", MoveDown, ctx),
        KeyBinding::new("alt-left", MoveWordLeft, ctx),
        KeyBinding::new("alt-right", MoveWordRight, ctx),
        KeyBinding::new("cmd-left", MoveLineStart, ctx),
        KeyBinding::new("cmd-right", MoveLineEnd, ctx),
        KeyBinding::new("home", MoveLineStart, ctx),
        KeyBinding::new("end", MoveLineEnd, ctx),
        KeyBinding::new("cmd-up", MoveDocumentStart, ctx),
        KeyBinding::new("cmd-down", MoveDocumentEnd, ctx),
        KeyBinding::new("pageup", MovePageUp, ctx),
        KeyBinding::new("pagedown", MovePageDown, ctx),
        KeyBinding::new("shift-left", SelectLeft, ctx),
        KeyBinding::new("shift-right", SelectRight, ctx),
        KeyBinding::new("shift-up", SelectUp, ctx),
        KeyBinding::new("shift-down", SelectDown, ctx),
        KeyBinding::new("alt-shift-left", SelectWordLeft, ctx),
        KeyBinding::new("alt-shift-right", SelectWordRight, ctx),
        KeyBinding::new("cmd-shift-left", SelectLineStart, ctx),
        KeyBinding::new("cmd-shift-right", SelectLineEnd, ctx),
        KeyBinding::new("backspace", Backspace, ctx),
        KeyBinding::new("delete", Delete, ctx),
        KeyBinding::new("alt-backspace", DeleteWordLeft, ctx),
        KeyBinding::new("enter", Newline, ctx),
        KeyBinding::new("tab", Indent, ctx),
        KeyBinding::new("shift-tab", Outdent, ctx),
        KeyBinding::new("cmd-/", ToggleComment, ctx),
        KeyBinding::new("cmd-shift-d", DuplicateLine, ctx),
        KeyBinding::new("cmd-shift-k", DeleteLine, ctx),
        KeyBinding::new("alt-up", MoveLineUp, ctx),
        KeyBinding::new("alt-down", MoveLineDown, ctx),
        KeyBinding::new("cmd-alt-up", AddCursorAbove, ctx),
        KeyBinding::new("cmd-alt-down", AddCursorBelow, ctx),
        KeyBinding::new("cmd-d", SelectNextOccurrence, ctx),
        KeyBinding::new("f12", GoToDefinition, ctx),
        KeyBinding::new("cmd-shift-f12", FindReferences, ctx),
        KeyBinding::new("cmd-k cmd-i", ShowHover, ctx),
        KeyBinding::new("ctrl-space", TriggerCompletion, ctx),
        KeyBinding::new("escape", DismissCompletion, ctx),
        KeyBinding::new("alt-shift-f", Format, ctx),
        KeyBinding::new("f2", Rename, ctx),
        KeyBinding::new("ctrl-g", GoToLine, ctx),
        KeyBinding::new("cmd-f", FindInFile, ctx),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn アプリ側のバインドが重複していない() {
        let bindings = app_bindings();
        let mut keys: Vec<String> = bindings
            .iter()
            .map(|b| {
                b.keystrokes()
                    .iter()
                    .map(|k| k.unparse())
                    .collect::<Vec<_>>()
                    .join(" ")
            })
            .collect();
        let total = keys.len();
        keys.sort();
        keys.dedup();
        assert_eq!(
            keys.len(),
            total,
            "同じキーに 2 つの操作が割り当てられている"
        );
    }

    #[test]
    fn エディタ側のバインドが重複していない() {
        let bindings = editor_bindings();
        let mut keys: Vec<String> = bindings
            .iter()
            .map(|b| {
                b.keystrokes()
                    .iter()
                    .map(|k| k.unparse())
                    .collect::<Vec<_>>()
                    .join(" ")
            })
            .collect();
        let total = keys.len();
        keys.sort();
        keys.dedup();
        assert_eq!(
            keys.len(),
            total,
            "同じキーに 2 つの操作が割り当てられている"
        );
    }
}
